use tokio_util::sync::CancellationToken;
use tracing::info;

/// Cancels the token at ctrl-c, and at SIGTERM on Unix.
pub fn on_signal() -> CancellationToken {
    let token = CancellationToken::new();
    let cancel = token.clone();

    tokio::spawn(async move {
        wait().await;
        info!("stopping");
        cancel.cancel();
    });

    token
}

#[cfg(unix)]
async fn wait() {
    use tokio::signal::unix::{SignalKind, signal};

    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(not(unix))]
async fn wait() {
    let _ = tokio::signal::ctrl_c().await;
}
