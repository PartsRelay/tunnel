// The handshake callback type is the library's
#![allow(clippy::result_large_err)]

mod common;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use common::config;
use futures_util::{SinkExt, StreamExt};
use partsrelay_tunnel::config::Config;
use partsrelay_tunnel::dial::{PROBE_HEADER, probe};
use partsrelay_tunnel::{Exit, SUBPROTOCOL, run};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderMap, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, accept_hdr_async};
use tokio_util::sync::CancellationToken;

// yamux, written by hand: what any server may send
const DATA: u8 = 0;
const WINDOW: u8 = 1;
const GO_AWAY: u8 = 3;
const SYN: u16 = 1;
const FIN: u16 = 4;
const HEADER: usize = 12;

type Ws = WebSocketStream<TcpStream>;

fn start(config: Config) -> (CancellationToken, JoinHandle<anyhow::Result<Exit>>) {
    let stop = CancellationToken::new();

    (stop.clone(), tokio::spawn(run(Arc::new(config), stop)))
}

fn frame(kind: u8, flags: u16, stream: u32, length: u32, body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0, kind];

    bytes.extend_from_slice(&flags.to_be_bytes());
    bytes.extend_from_slice(&stream.to_be_bytes());
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

fn open(stream: u32, target: &str) -> Vec<u8> {
    let json = format!(r#"{{"target":"{target}"}}"#);
    let mut body = (json.len() as u16).to_be_bytes().to_vec();

    body.extend_from_slice(json.as_bytes());
    frame(DATA, SYN, stream, body.len() as u32, &body)
}

/// A server that hands over each WebSocket as it is.
struct Server {
    url: String,
    sockets: mpsc::UnboundedReceiver<(Ws, HeaderMap)>,
    attempts: Arc<AtomicUsize>,
}

impl Server {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = attempts.clone();
        let (sender, sockets) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                let sender = sender.clone();

                tokio::spawn(async move {
                    let seen = Arc::new(Mutex::new(HeaderMap::new()));
                    let headers = seen.clone();
                    let callback = move |request: &Request, mut response: Response| {
                        *headers.lock().unwrap() = request.headers().clone();
                        response.headers_mut().insert(
                            "sec-websocket-protocol",
                            HeaderValue::from_static(SUBPROTOCOL),
                        );
                        Ok(response)
                    };

                    if let Ok(ws) = accept_hdr_async(tcp, callback).await {
                        let headers = seen.lock().unwrap().clone();
                        let _ = sender.send((ws, headers));
                    }
                });
            }
        });

        Self {
            url: format!("ws://{address}/v1/tunnel"),
            sockets,
            attempts,
        }
    }

    async fn socket(&mut self) -> (Ws, HeaderMap) {
        timeout(Duration::from_secs(5), self.sockets.recv())
            .await
            .expect("a tunnel in time")
            .expect("a tunnel")
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

/// A target that sends `total` patterned bytes, then waits.
async fn blast(total: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let bytes: Vec<u8> = (0..total).map(|index| (index % 251) as u8).collect();
                let _ = tcp.write_all(&bytes).await;
                let _ = tcp.shutdown().await;
                let mut rest = [0; 1024];
                while matches!(tcp.read(&mut rest).await, Ok(read) if read > 0) {}
            });
        }
    });

    address
}

#[derive(Default)]
struct Seen {
    bytes: usize,
    fin: bool,
    changed: bool,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivers_everything_before_it_says_goodbye() {
    const STREAMS: u32 = 40;
    const TOTAL: usize = 250_000;
    // The answer frame of the tunnel comes first
    const ACCEPTED: usize = 2 + r#"{"ok":true}"#.len();
    let target = blast(TOTAL).await;
    let mut server = Server::start().await;
    let mut draining = config(&server.url, &[target.to_string()]);
    draining.tuning.drain = Duration::from_secs(10);
    let (stop, tunnel) = start(draining);
    let (mut ws, _) = server.socket().await;

    for index in 0..STREAMS {
        let stream = index * 2 + 1;
        let mut bytes = open(stream, &target.to_string());

        bytes.extend_from_slice(&frame(WINDOW, FIN, stream, 0, &[]));
        ws.send(Message::Binary(Bytes::from(bytes))).await.unwrap();
    }

    // A slow uplink: nothing is read, then the stop
    sleep(Duration::from_millis(1500)).await;
    stop.cancel();
    sleep(Duration::from_millis(300)).await;

    let mut wire = BytesMut::new();
    let mut go_away = false;
    let mut closed = false;
    let mut streams: BTreeMap<u32, Seen> = BTreeMap::new();

    while let Ok(Some(Ok(message))) = timeout(Duration::from_secs(5), ws.next()).await {
        match message {
            Message::Binary(bytes) => wire.extend_from_slice(&bytes),
            Message::Close(_) => closed = true,
            _ => {}
        }
        while wire.len() >= HEADER {
            let kind = wire[1];
            let flags = u16::from_be_bytes([wire[2], wire[3]]);
            let stream = u32::from_be_bytes([wire[4], wire[5], wire[6], wire[7]]);
            let length = u32::from_be_bytes([wire[8], wire[9], wire[10], wire[11]]) as usize;
            let body = if kind == DATA { length } else { 0 };

            if wire.len() < HEADER + body {
                break;
            }
            wire.advance(HEADER);
            let data = wire.split_to(body);

            if kind == GO_AWAY {
                go_away = true;
            }
            if matches!(kind, DATA | WINDOW) && stream != 0 {
                let seen = streams.entry(stream).or_default();

                for byte in data.iter() {
                    let expected = seen.bytes.checked_sub(ACCEPTED).map(|at| (at % 251) as u8);
                    seen.changed |= expected.is_some_and(|expected| expected != *byte);
                    seen.bytes += 1;
                }
                seen.fin |= flags & FIN != 0;
            }
        }
    }

    let whole = streams
        .values()
        .filter(|seen| seen.fin && seen.bytes == TOTAL + ACCEPTED && !seen.changed)
        .count();

    assert_eq!(whole, STREAMS as usize, "streams lost their last bytes");
    assert!(go_away, "no yamux GoAway");
    assert!(closed, "no WebSocket Close");
    assert_eq!(
        timeout(Duration::from_secs(5), tunnel)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Exit::Shutdown
    );
}

#[cfg(target_os = "linux")]
fn resident_mib() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: usize = statm.split_whitespace().nth(1).unwrap().parse().unwrap();

    pages * 4096 / (1024 * 1024)
}

// Reads the memory of the process: Linux only
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffers_little_for_a_server_that_grants_a_huge_window() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let chunk = vec![7; 64 * 1024];
                while tcp.write_all(&chunk).await.is_ok() {}
            });
        }
    });
    let mut server = Server::start().await;
    let (stop, _tunnel) = start(config(&server.url, &[target.to_string()]));
    let (mut ws, _) = server.socket().await;

    ws.send(Message::Binary(Bytes::from(open(1, &target.to_string()))))
        .await
        .unwrap();
    sleep(Duration::from_millis(500)).await;
    let before = resident_mib();

    // Then the window grows a lot and nothing is read
    let grant = frame(WINDOW, 0, 1, 0x7000_0000, &[]);
    ws.send(Message::Binary(Bytes::from(grant))).await.unwrap();
    sleep(Duration::from_secs(2)).await;
    let after = resident_mib();

    stop.cancel();
    assert!(after < before + 64, "from {before} to {after} MiB");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_the_close_code_that_follows_a_go_away() {
    for go_away_first in [false, true] {
        let mut server = Server::start().await;
        let (stop, _tunnel) = start(config(&server.url, &[]));
        let (mut ws, _) = server.socket().await;
        let replaced = CloseFrame {
            code: CloseCode::from(4001),
            reason: "replaced".into(),
        };

        if go_away_first {
            let bye = frame(GO_AWAY, 0, 0, 0, &[]);
            ws.send(Message::Binary(Bytes::from(bye))).await.unwrap();
        }
        let _ = ws.send(Message::Close(Some(replaced))).await;

        // Replaced: it stays away, the usual backoff would be back
        sleep(Duration::from_millis(1500)).await;
        assert_eq!(server.attempts(), 1, "go away first: {go_away_first}");
        stop.cancel();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_comes_back_sooner_than_its_backoff() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = attempts.clone();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    match tcp.read_u8().await {
                        Ok(byte) => head.push(byte),
                        Err(_) => return,
                    }
                }
                let _ = tcp
                    .write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await;
            });
        }
    });
    let mut patient = config(&format!("ws://{address}/v1/tunnel"), &[]);
    patient.tuning.backoff_min = Duration::from_millis(400);
    patient.tuning.backoff_max = Duration::from_millis(400);
    let (stop, _tunnel) = start(patient);

    // Each wait is 200 to 400 ms
    sleep(Duration::from_millis(1500)).await;
    let dials = attempts.load(Ordering::SeqCst);

    stop.cancel();
    assert!((2..=9).contains(&dials), "{dials} dials");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probes_without_asking_for_the_session() {
    let mut server = Server::start().await;
    let cfg = config(&server.url, &[]);

    let mut probing = probe(&cfg).await.unwrap();
    let (mut ws, headers) = server.socket().await;
    probing.close(None).await.unwrap();

    assert_eq!(headers.get(PROBE_HEADER).unwrap(), "1");
    assert!(matches!(
        timeout(Duration::from_secs(5), ws.next()).await,
        Ok(Some(Ok(Message::Close(_))))
    ));

    // The tunnel itself never sends the header
    let (stop, _tunnel) = start(cfg);
    let (_ws, headers) = server.socket().await;
    assert!(headers.get(PROBE_HEADER).is_none());
    stop.cancel();
}
