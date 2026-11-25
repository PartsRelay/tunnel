mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Connectors, TOKEN, WAIT, config, echo};
use partsrelay_tunnel::run;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

// Alone in its binary: the capture is per thread
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
    let stop = CancellationToken::new();
    let tunnel = tokio::spawn(run(
        Arc::new(config(&connectors.url(), &[erp.to_string()])),
        stop.clone(),
    ));
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
