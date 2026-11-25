use std::sync::Arc;

use tokio_util::sync::CancellationToken;

pub mod allow;
pub mod check;
pub mod cli;
pub mod client;
pub mod config;
pub mod dial;
pub mod logging;
pub mod proto;
pub mod proxy;
pub mod reach;
pub mod session;
pub mod shutdown;
pub mod update;
pub mod wsio;

use config::Config;

pub const NAME: &str = "partsrelay-tunnel";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const TARGET: &str = env!("TARGET");
pub const SUBPROTOCOL: &str = "partsrelay-tunnel.v1";

/// How the process ends after `run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Shutdown,
    Restart,
}

/// Keeps the tunnel connected until a shutdown or an update.
pub async fn run(cfg: Arc<Config>, shutdown: CancellationToken) -> anyhow::Result<Exit> {
    let stop = shutdown.child_token();
    let tunnel = tokio::spawn(client::run(cfg.clone(), stop.clone()));
    let updated = if cfg.auto_update {
        match update::Updater::embedded(&cfg) {
            Ok(updater) if !update::install::writable(&updater.exe) => {
                let exe = updater.exe.display().to_string();
                tracing::warn!(exe, "automatic updates are off: its directory is read only");
                shutdown.cancelled().await;
                false
            }
            Ok(updater) => update::watch(updater, shutdown.clone()).await,
            Err(error) => {
                tracing::warn!(%error, "automatic updates are off");
                shutdown.cancelled().await;
                false
            }
        }
    } else {
        shutdown.cancelled().await;
        false
    };

    // Drain the open streams before leaving
    stop.cancel();
    tunnel.await?;

    // Asked to stop: the new version starts next time
    Ok(if updated && !shutdown.is_cancelled() {
        Exit::Restart
    } else {
        Exit::Shutdown
    })
}
