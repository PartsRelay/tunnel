mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{Connectors, config, echo, read_all, until};
use partsrelay_tunnel::config::Config;
use partsrelay_tunnel::{Exit, run};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

fn start(config: Config) -> (CancellationToken, JoinHandle<anyhow::Result<Exit>>) {
    let stop = CancellationToken::new();

    (stop.clone(), tokio::spawn(run(Arc::new(config), stop)))
}

/// A target that sends `total` bytes, counting its open connections.
async fn blast(total: usize, after_the_request: bool) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let open = Arc::new(AtomicUsize::new(0));
    let counted = open.clone();

    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let counted = counted.clone();

            counted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                if after_the_request {
                    let mut request = [0; 1024];
                    while matches!(tcp.read(&mut request).await, Ok(read) if read > 0) {}
                }
                let chunk = vec![0x5a; 16 * 1024];
                let mut sent = 0;
                while sent < total && tcp.write_all(&chunk).await.is_ok() {
                    sent += chunk.len();
                }
                let _ = tcp.shutdown().await;
                drop(tcp);
                counted.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });

    (address, open)
}

/// A target that accepts and never reads, soon full.
#[cfg(target_os = "linux")]
async fn deaf() -> SocketAddr {
    use socket2::{Domain, Protocol, Socket, Type};

    // Small segments and buffer: loopback would take megabytes
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
    socket.set_tcp_mss(536).unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    socket.set_nonblocking(true).unwrap();
    socket
        .bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
        .unwrap();
    socket.listen(16).unwrap();
    let listener = TcpListener::from_std(socket.into()).unwrap();
    let address = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((tcp, _)) = listener.accept().await {
            held.push(tcp);
        }
    });

    address
}

#[tokio::test]
async fn sends_a_long_answer_after_the_opener_finished_writing() {
    let total = 2 * 1024 * 1024;
    let (target, _) = blast(total, true).await;
    let mut connectors = Connectors::start().await;
    let (stop, _tunnel) = start(config(&connectors.url(), &[target.to_string()]));
    let mut session = connectors.tunnel().await;

    let (mut stream, response) = session.open(&target.to_string()).await;
    assert!(response.ok);
    // The request is over, the answer exceeds one window
    stream.shutdown().await.unwrap();

    let mut received = 0;
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match timeout(Duration::from_secs(5), stream.read(&mut buffer)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(read)) => received += read,
            Ok(Err(error)) => panic!("failed after {received} bytes: {error}"),
            Err(_) => panic!("stalled after {received} of {total} bytes"),
        }
    }
    assert_eq!(received, total);

    stop.cancel();
}

#[tokio::test]
async fn frees_the_place_of_a_stream_the_opener_gave_up() {
    let (target, open) = blast(64 * 1024 * 1024, false).await;
    let (erp, _) = echo().await;
    let mut connectors = Connectors::start().await;
    let mut limited = config(&connectors.url(), &[target.to_string(), erp.to_string()]);
    limited.max_streams = 2;
    limited.tuning.abandoned_after = Duration::from_millis(300);
    let (stop, _tunnel) = start(limited);
    let mut session = connectors.tunnel().await;

    for _ in 0..2 {
        let (mut stream, response) = session.open(&target.to_string()).await;
        assert!(response.ok);
        // Done with it while the target still sends
        stream.shutdown().await.unwrap();
        drop(stream);
    }

    until("the targets to be hung up", || {
        open.load(Ordering::SeqCst) == 0
    })
    .await;
    let mut third = session.open(&erp.to_string()).await.1;
    for _ in 0..100 {
        if third.ok {
            break;
        }
        sleep(Duration::from_millis(20)).await;
        third = session.open(&erp.to_string()).await.1;
    }
    assert!(third.ok, "{third:?}");

    stop.cancel();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn keeps_serving_while_one_target_reads_nothing() {
    let stuck = deaf().await;
    let (erp, _) = echo().await;
    let mut connectors = Connectors::start().await;
    let (stop, _tunnel) = start(config(
        &connectors.url(),
        &[stuck.to_string(), erp.to_string()],
    ));
    let mut session = connectors.tunnel().await;

    let (mut before, response) = session.open(&erp.to_string()).await;
    assert!(response.ok);
    before.write_all(b"ping").await.unwrap();
    assert_eq!(read_all(&mut before, 4).await, b"ping");

    // An upload until target and window take no more
    let (mut upload, response) = session.open(&stuck.to_string()).await;
    assert!(response.ok);
    let chunk = vec![1; 4096];
    let mut written = 0;
    while written < 16 * 1024 * 1024 {
        match timeout(Duration::from_secs(1), upload.write_all(&chunk)).await {
            Ok(Ok(())) => written += chunk.len(),
            _ => break,
        }
    }
    assert!(written < 16 * 1024 * 1024, "the target took everything");

    // The stream of before and a new one still answer
    before.write_all(b"pong").await.unwrap();
    assert_eq!(read_all(&mut before, 4).await, b"pong");
    let (mut fresh, response) = session.open(&erp.to_string()).await;
    assert!(response.ok);
    fresh.write_all(b"new").await.unwrap();
    assert_eq!(read_all(&mut fresh, 3).await, b"new");
    assert_eq!(connectors.attempts(), 1);

    stop.cancel();
}

#[tokio::test]
async fn answers_a_request_sent_while_the_target_is_slow_to_connect() {
    let (erp, _) = echo().await;
    let mut connectors = Connectors::start().await;
    let (stop, _tunnel) = start(config(&connectors.url(), &[erp.to_string()]));
    let mut session = connectors.tunnel().await;

    // Payload right behind the request, before the answer
    let mut stream = session.control.open_stream().await.unwrap();
    let request = partsrelay_tunnel::proto::OpenRequest {
        target: erp.to_string(),
    };
    partsrelay_tunnel::proto::write_frame(&mut stream, &request)
        .await
        .unwrap();
    stream.write_all(b"early").await.unwrap();
    let response: partsrelay_tunnel::proto::OpenResponse =
        partsrelay_tunnel::proto::read_frame(&mut stream)
            .await
            .unwrap();

    assert!(response.ok);
    assert_eq!(read_all(&mut stream, 5).await, b"early");

    stop.cancel();
}
