use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::{TcpStream, lookup_host};
use tokio::time::timeout;

use crate::allow::{Host, Target};
use crate::config::Config;
use crate::dial::dial;
use crate::proxy;

const PROBE: Duration = Duration::from_secs(5);

async fn reachable(target: &Target) -> Result<(), String> {
    let probe = async {
        let address: SocketAddr = match &target.host {
            Host::Ip(ip) => SocketAddr::new(*ip, target.port),
            Host::Name(name) => lookup_host((name.as_str(), target.port))
                .await
                .map_err(|error| error.to_string())?
                .next()
                .ok_or("no address")?,
        };

        TcpStream::connect(address)
            .await
            .map(drop)
            .map_err(|error| error.to_string())
    };

    timeout(PROBE, probe)
        .await
        .unwrap_or_else(|_| Err("no answer in time".into()))
}

/// Prints what works. False if anything fails.
pub async fn run(cfg: &Config) -> bool {
    let mut healthy = true;
    let env = |name: &str| std::env::var(name).ok();

    println!("server      {}", cfg.server.uri);
    match proxy::resolve(cfg.proxy.as_ref(), env, &cfg.server.host) {
        Some(proxy) => println!("proxy       {proxy}"),
        None => println!("proxy       none"),
    }
    println!("auto update {}", if cfg.auto_update { "on" } else { "off" });

    if cfg.allow.is_empty() {
        println!("allow       empty: the tunnel will refuse every target");
        healthy = false;
    }
    for target in cfg.allow.targets() {
        match reachable(target).await {
            Ok(()) => println!("allow       {target}: reachable"),
            Err(error) => {
                println!("allow       {target}: NOT reachable ({error})");
                healthy = false;
            }
        }
    }

    match dial(cfg).await {
        Ok(_) => println!("connection  the server accepted the token"),
        Err(error) => {
            println!("connection  FAILED: {error}");
            healthy = false;
        }
    }

    healthy
}
