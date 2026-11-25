use std::time::Duration;

use anyhow::{Context, Result, bail};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::{Agent, Proxy, ProxyProtocol};

use crate::dial::user_agent;
use crate::proxy::ProxyUrl;

/// An HTTP client that trusts what the operating system trusts.
pub fn agent(proxy: Option<&ProxyUrl>) -> Result<Agent> {
    let proxy = proxy
        .map(|proxy| {
            let mut builder = Proxy::builder(ProxyProtocol::Http)
                .host(&proxy.host)
                .port(proxy.port);
            if let Some(username) = proxy.username() {
                builder = builder
                    .username(username)
                    .password(proxy.password().unwrap_or_default());
            }
            builder
                .build()
                .context("the proxy is not usable for the updates")
        })
        .transpose()?;
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();

    Ok(Agent::config_builder()
        .tls_config(tls)
        .proxy(proxy)
        .user_agent(user_agent())
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(300)))
        .build()
        .new_agent())
}

/// Downloads `url`, refusing more than `limit` bytes.
pub fn get(agent: &Agent, url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut response = agent
        .get(url)
        .call()
        .with_context(|| format!("cannot download {url}"))?;
    let body = response
        .body_mut()
        .with_config()
        .limit(limit + 1)
        .read_to_vec()
        .with_context(|| format!("cannot read {url}"))?;

    if body.len() as u64 > limit {
        bail!("{url} is larger than {limit} bytes");
    }

    Ok(body)
}
