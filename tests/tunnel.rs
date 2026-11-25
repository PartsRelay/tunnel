mod common;

use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Connectors, TOKEN, WAIT, closed_port, config, connect_proxy, echo, read_all, until};
use partsrelay_tunnel::config::Config;
use partsrelay_tunnel::proto::{ErrorCode, MAX_HEADER_LEN};
use partsrelay_tunnel::{Exit, SUBPROTOCOL, run};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

fn start(config: Config) -> (CancellationToken, JoinHandle<anyhow::Result<Exit>>) {
    let stop = CancellationToken::new();

    (stop.clone(), tokio::spawn(run(Arc::new(config), stop)))
}

#[tokio::test]
async fn relays_bytes_both_ways_to_an_allowed_target() {
    let (erp, accepted) = echo().await;
    let mut connectors = Connectors::start().await;
    let (stop, _tunnel) = start(config(&connectors.url(), &[erp.to_string()]));
    let mut session = connectors.tunnel().await;

    let (stream, response) = session.open(&erp.to_string()).await;
    assert!(response.ok, "{response:?}");

    // More than the flow control window
    let sent: Vec<u8> = (0..1024 * 1024).map(|index| (index % 251) as u8).collect();
    let (mut reader, mut writer) = tokio::io::split(stream);
    let outgoing = sent.clone();
    let writing = tokio::spawn(async move {
        writer.write_all(&outgoing).await.unwrap();
        writer.flush().await.unwrap();
        writer
    });
    let mut received = vec![0; sent.len()];
    timeout(
        WAIT,
        tokio::io::AsyncReadExt::read_exact(&mut reader, &mut received),
    )
    .await
    .expect("the echo in time")
    .unwrap();
    writing.await.unwrap();

    assert!(received == sent, "the bytes came back changed");
    assert_eq!(accepted.load(Ordering::SeqCst), 1);

    stop.cancel();
}

#[tokio::test]
async fn sends_the_token_its_version_and_the_protocol() {
    let mut connectors = Connectors::start().await;
    let (stop, _tunnel) = start(config(&connectors.url(), &[]));
    let session = connectors.tunnel().await;
    let header = |name: &str| {
        session
            .headers
            .get(name)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    };

    assert_eq!(header("authorization"), format!("Bearer {TOKEN}"));
    assert!(header("user-agent").starts_with("partsrelay-tunnel/"));
    assert_eq!(header("sec-websocket-protocol"), SUBPROTOCOL);

    stop.cancel();
}

#[tokio::test]
async fn refuses_a_target_outside_the_allow_list() {
    let (erp, accepted) = echo().await;
    let (other, reached) = echo().await;
    let mut connectors = Connectors::start().await;
    let (stop, _tunnel) = start(config(&connectors.url(), &[erp.to_string()]));
    let mut session = connectors.tunnel().await;

    let (_, by_address) = session.open(&other.to_string()).await;
    let (_, by_name) = session.open(&format!("localhost:{}", erp.port())).await;

    assert_eq!(
        (by_address.ok, by_address.error),
        (false, Some(ErrorCode::NotAllowed))
    );
    assert_eq!(
        (by_name.ok, by_name.error),
        (false, Some(ErrorCode::NotAllowed))
    );
    assert_eq!(
        accepted.load(Ordering::SeqCst) + reached.load(Ordering::SeqCst),
        0
    );

    stop.cancel();
}

#[tokio::test]
async fn refuses_a_bad_header_and_tells_when_the_target_is_down() {
    let down = closed_port();
    let mut connectors = Connectors::start().await;
    let (stop, _tunnel) = start(config(&connectors.url(), &[down.to_string()]));
    let mut session = connectors.tunnel().await;
    let oversized = ((MAX_HEADER_LEN + 1) as u16).to_be_bytes();

    let malformed = session.open_raw(&[0, 2, b'{', b'}']).await;
    let too_long = session.open_raw(&oversized).await;
    let late = session.open_raw(&[0]).await;
    let (_, no_port) = session.open("erp.local").await;
    let (_, refused) = session.open(&down.to_string()).await;

    assert_eq!(malformed.error, Some(ErrorCode::BadRequest));
    assert_eq!(too_long.error, Some(ErrorCode::BadRequest));
    assert_eq!(late.error, Some(ErrorCode::BadRequest));
    assert_eq!(no_port.error, Some(ErrorCode::BadRequest));
    assert_eq!(refused.error, Some(ErrorCode::ConnectRefused));

    stop.cancel();
}

#[tokio::test]
async fn closes_its_side_when_the_target_closes() {
    let (erp, _) = echo().await;
    let mut connectors = Connectors::start().await;
    let (stop, _tunnel) = start(config(&connectors.url(), &[erp.to_string()]));
    let mut session = connectors.tunnel().await;
    let (mut stream, response) = session.open(&erp.to_string()).await;

    assert!(response.ok);
    stream.write_all(b"last words").await.unwrap();
    stream.shutdown().await.unwrap();

    assert_eq!(read_all(&mut stream, 10).await, b"last words");
    let end = timeout(
        WAIT,
        tokio::io::AsyncReadExt::read(&mut stream, &mut [0; 8]),
    )
    .await;
    assert_eq!(end.expect("the end in time").unwrap(), 0);

    stop.cancel();
}

#[tokio::test]
async fn connects_again_after_the_server_drops_it() {
    let (erp, _) = echo().await;
    let mut connectors = Connectors::start().await;
    let (stop, _tunnel) = start(config(&connectors.url(), &[erp.to_string()]));

    connectors.tunnel().await.cut();
    let mut again = connectors.tunnel().await;
    let (mut stream, response) = again.open(&erp.to_string()).await;

    assert!(response.ok);
    stream.write_all(b"still here").await.unwrap();
    assert_eq!(read_all(&mut stream, 10).await, b"still here");
    assert!(connectors.attempts() >= 2);

    stop.cancel();
}

#[tokio::test]
async fn waits_long_before_trying_a_refused_token_again() {
    let connectors = Connectors::start().await;
    connectors.refuse_with(401);
    let (stop, tunnel) = start(config(&connectors.url(), &[]));

    sleep(Duration::from_millis(600)).await;
    assert_eq!(connectors.attempts(), 1);

    stop.cancel();
    assert_eq!(
        timeout(WAIT, tunnel).await.unwrap().unwrap().unwrap(),
        Exit::Shutdown
    );
}

#[tokio::test]
async fn keeps_trying_a_server_that_is_not_ready() {
    let mut connectors = Connectors::start().await;
    connectors.refuse_with(503);
    let (stop, _tunnel) = start(config(&connectors.url(), &[]));

    until("a third attempt", || connectors.attempts() >= 3).await;

    connectors.refuse_with(0);
    connectors.tunnel().await;

    stop.cancel();
}

#[tokio::test]
async fn limits_the_streams_open_at_once() {
    let (erp, _) = echo().await;
    let mut connectors = Connectors::start().await;
    let mut limited = config(&connectors.url(), &[erp.to_string()]);
    limited.max_streams = 2;
    let (stop, _tunnel) = start(limited);
    let mut session = connectors.tunnel().await;

    let (_first, one) = session.open(&erp.to_string()).await;
    let (_second, two) = session.open(&erp.to_string()).await;
    assert!(one.ok && two.ok);

    let mut third = session.control.open_stream().await.unwrap();
    let request = partsrelay_tunnel::proto::OpenRequest {
        target: erp.to_string(),
    };
    let _ = partsrelay_tunnel::proto::write_frame(&mut third, &request).await;
    let answer = timeout(
        Duration::from_secs(2),
        partsrelay_tunnel::proto::read_frame::<_, partsrelay_tunnel::proto::OpenResponse>(
            &mut third,
        ),
    )
    .await;

    assert!(
        !matches!(answer, Ok(Ok(response)) if response.ok),
        "a third stream was served"
    );

    stop.cancel();
}

#[tokio::test]
async fn drains_the_open_streams_then_stops() {
    let (erp, _) = echo().await;
    let mut connectors = Connectors::start().await;
    let (stop, tunnel) = start(config(&connectors.url(), &[erp.to_string()]));
    let mut session = connectors.tunnel().await;
    let (mut open, response) = session.open(&erp.to_string()).await;
    assert!(response.ok);

    stop.cancel();
    // A stream opened before the drain is served, then dropped
    let mut late = session.open(&erp.to_string()).await.1;
    for _ in 0..100 {
        if !late.ok {
            break;
        }
        sleep(Duration::from_millis(50)).await;
        late = session.open(&erp.to_string()).await.1;
    }
    assert_eq!(late.error, Some(ErrorCode::ShuttingDown));

    // The stream opened before still works, then ends the drain
    open.write_all(b"finish").await.unwrap();
    assert_eq!(read_all(&mut open, 6).await, b"finish");
    assert!(!tunnel.is_finished());
    open.shutdown().await.unwrap();
    drop(open);

    assert_eq!(
        timeout(WAIT, tunnel).await.unwrap().unwrap().unwrap(),
        Exit::Shutdown
    );
}

#[tokio::test]
async fn connects_through_a_proxy_with_its_credentials() {
    let (erp, _) = echo().await;
    let (proxy, requests) = connect_proxy().await;
    let mut connectors = Connectors::start().await;
    let mut proxied = config(&connectors.url(), &[erp.to_string()]);
    proxied.proxy = Some(format!("http://mario:s3cret@{proxy}").parse().unwrap());
    let (stop, _tunnel) = start(proxied);
    let mut session = connectors.tunnel().await;

    let (mut stream, response) = session.open(&erp.to_string()).await;
    assert!(response.ok);
    stream.write_all(b"through").await.unwrap();
    assert_eq!(read_all(&mut stream, 7).await, b"through");

    let request = requests.lock().unwrap().first().cloned().unwrap();
    assert!(request.starts_with(&format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1",
        connectors.address.port()
    )));
    assert!(request.contains("Proxy-Authorization: Basic bWFyaW86czNjcmV0\r\n"));

    stop.cancel();
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn never_logs_the_token() {
    let logs = Captured::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _capturing = tracing::subscriber::set_default(subscriber);

    let (erp, _) = echo().await;
    let (other, _) = echo().await;
    let mut connectors = Connectors::start().await;
    let (stop, tunnel) = start(config(&connectors.url(), &[erp.to_string()]));
    let mut session = connectors.tunnel().await;
    let (_stream, accepted) = session.open(&erp.to_string()).await;
    let (_, refused) = session.open(&other.to_string()).await;
    assert!(accepted.ok && !refused.ok);

    connectors.refuse_with(401);
    session.cut();
    sleep(Duration::from_millis(400)).await;
    stop.cancel();
    let _ = timeout(WAIT, tunnel).await;

    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("connected"), "nothing was captured: {logs}");
    assert!(logs.contains("not in the allow list"));
    assert!(logs.contains("check the token"));
    assert!(!logs.contains(TOKEN), "the token reached the logs");
    assert!(!logs.contains("Bearer"));
}

// The verifiers of macOS and Windows refuse a test certificate
#[cfg(target_os = "linux")]
mod tls {
    use std::io::Write;
    use std::sync::Arc;
    use std::time::Duration;

    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::io::AsyncWriteExt;
    use tokio::time::timeout;
    use tokio_rustls::TlsAcceptor;

    use super::common::{Connectors, config, echo, read_all, until};
    use super::start;

    #[tokio::test]
    async fn connects_over_tls_trusting_the_extra_certificate() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(issued.signing_key.serialize_der()));
        let server = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![issued.cert.der().clone()], key)
            .unwrap();
        let mut ca_file = tempfile::NamedTempFile::new().unwrap();
        ca_file.write_all(issued.cert.pem().as_bytes()).unwrap();

        let (erp, _) = echo().await;
        let mut connectors = Connectors::start_tls(TlsAcceptor::from(Arc::new(server))).await;
        let mut trusting = config(&connectors.url(), &[erp.to_string()]);
        trusting.ca_file = Some(ca_file.path().to_path_buf());
        let (stop, _tunnel) = start(trusting);
        let mut session = connectors.tunnel().await;

        let (mut stream, response) = session.open(&erp.to_string()).await;
        assert!(response.ok);
        stream.write_all(b"secure").await.unwrap();
        assert_eq!(read_all(&mut stream, 6).await, b"secure");
        stop.cancel();

        // Without the extra certificate the server is not trusted
        let mut strangers = Connectors::start_tls(TlsAcceptor::from(Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![issued.cert.der().clone()],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                        issued.signing_key.serialize_der(),
                    )),
                )
                .unwrap(),
        )))
        .await;
        let (stop, _tunnel) = start(config(&strangers.url(), &[]));
        until("a second attempt", || strangers.attempts() >= 2).await;
        assert!(
            timeout(Duration::from_millis(50), strangers.tunnels_now())
                .await
                .is_err()
        );
        stop.cancel();
    }
}
