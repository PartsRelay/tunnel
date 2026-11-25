use std::time::Duration;

use tokio::time::timeout;

use crate::config::Config;
use crate::dial::{probe, proxy_of};
use crate::reach;

const PROBE: Duration = Duration::from_secs(5);

/// Prints what works. False if anything fails.
pub async fn run(cfg: &Config) -> bool {
    let mut healthy = true;

    println!("server      {}", cfg.server.uri);
    match proxy_of(cfg) {
        Some(proxy) => println!("proxy       {proxy}"),
        None => println!("proxy       none"),
    }
    println!("auto update {}", if cfg.auto_update { "on" } else { "off" });

    if cfg.allow.is_empty() {
        println!("allow       empty: the tunnel will refuse every target");
        healthy = false;
    }
    for target in cfg.allow.targets() {
        match reach::connect(target, PROBE).await {
            Ok(_) => println!("allow       {target}: reachable"),
            Err(error) => {
                println!("allow       {target}: NOT reachable ({error})");
                healthy = false;
            }
        }
    }

    // A probe: the running tunnel keeps its session
    match probe(cfg).await {
        Ok(mut ws) => {
            let _ = timeout(PROBE, ws.close(None)).await;
            println!("connection  the server accepted the token");
        }
        Err(error) => {
            println!("connection  FAILED: {error}");
            healthy = false;
        }
    }

    healthy
}
