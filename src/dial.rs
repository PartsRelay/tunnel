use std::io;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, ServerName, pem::PemObject};
use rustls::{ClientConfig, RootCertStore};
use rustls_platform_verifier::Verifier;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{
    AUTHORIZATION, RETRY_AFTER, SEC_WEBSOCKET_PROTOCOL, USER_AGENT,
};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{WebSocketStream, client_async_with_config};

use crate::config::Config;
use crate::proxy::{self, ProxyError};
use crate::{NAME, SUBPROTOCOL, TARGET, VERSION};

/// Largest WebSocket message and frame accepted.
pub const MAX_MESSAGE: usize = 1024 * 1024;
/// Header of a handshake that must not replace a session.
pub const PROBE_HEADER: &str = "partsrelay-tunnel-probe";
const MAX_RETRY_AFTER: Duration = Duration::from_secs(15 * 60);

pub trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

pub type BoxIo = Box<dyn Io>;

#[derive(Debug, thiserror::Error)]
pub enum DialError {
    #[error("the server refused the token ({0})")]
    Auth(u16),
    #[error("the server answered {status}")]
    Rejected {
        status: u16,
        retry_after: Option<Duration>,
    },
    #[error("the TLS connection failed: {0}")]
    Tls(String),
    #[error(transparent)]
    Proxy(#[from] ProxyError),
    #[error("the connection timed out")]
    Timeout,
    #[error("the WebSocket handshake failed: {0}")]
    Handshake(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub fn user_agent() -> String {
    format!("{NAME}/{VERSION} ({TARGET})")
}

/// TLS that trusts what the operating system trusts.
pub fn tls_config(cfg: &Config) -> Result<ClientConfig, DialError> {
    let tls = |error: &dyn std::fmt::Display| DialError::Tls(error.to_string());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = match &cfg.ca_file {
        Some(path) => {
            let roots = CertificateDer::pem_file_iter(path)
                .and_then(|certificates| certificates.collect::<Result<Vec<_>, _>>())
                .map_err(|error| tls(&format!("{}: {error}", path.display())))?;
            // Fails early on a file without certificates
            let mut store = RootCertStore::empty();
            let (added, _) = store.add_parsable_certificates(roots.iter().cloned());
            if added == 0 {
                return Err(tls(&format!("{}: no certificate found", path.display())));
            }
            Verifier::new_with_extra_roots(roots, provider.clone())
        }
        None => Verifier::new(provider.clone()),
    }
    .map_err(|error| tls(&error))?;

    Ok(ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| tls(&error))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth())
}

/// The proxy to reach the server through, if any.
pub fn proxy_of(cfg: &Config) -> Option<proxy::ProxyUrl> {
    // Plain ws:// is this computer: no proxy sees the token
    if !cfg.server.tls {
        return None;
    }

    proxy::resolve(
        cfg.proxy.as_ref(),
        |name| std::env::var(name).ok(),
        &cfg.server.host,
    )
}

async fn transport(cfg: &Config) -> Result<BoxIo, DialError> {
    let server = &cfg.server;
    let proxy = proxy_of(cfg);
    let tcp = match &proxy {
        Some(proxy) => {
            let mut tcp = TcpStream::connect((proxy.host.as_str(), proxy.port)).await?;
            proxy::connect(&mut tcp, &server.authority(), proxy).await?;
            tcp
        }
        None => TcpStream::connect((server.host.as_str(), server.port)).await?,
    };
    tcp.set_nodelay(true)?;

    if !server.tls {
        return Ok(Box::new(tcp));
    }

    let name = ServerName::try_from(server.host.clone())
        .map_err(|error| DialError::Tls(error.to_string()))?;
    let tls = TlsConnector::from(Arc::new(tls_config(cfg)?))
        .connect(name, tcp)
        .await
        .map_err(|error| DialError::Tls(error.to_string()))?;

    Ok(Box::new(tls))
}

fn rejected(status: StatusCode, retry_after: Option<&HeaderValue>) -> DialError {
    let seconds = || -> Option<u64> { retry_after?.to_str().ok()?.trim().parse().ok() };

    match status.as_u16() {
        status @ (401 | 403) => DialError::Auth(status),
        // Only "not now" says when to come back
        status @ (429 | 503) => DialError::Rejected {
            status,
            retry_after: seconds().map(|seconds| Duration::from_secs(seconds).min(MAX_RETRY_AFTER)),
        },
        status => DialError::Rejected {
            status,
            retry_after: None,
        },
    }
}

/// Connects out to the server, through the proxy if any.
pub async fn dial(cfg: &Config) -> Result<WebSocketStream<BoxIo>, DialError> {
    open(cfg, false).await
}

/// Tests the token without replacing the live session.
pub async fn probe(cfg: &Config) -> Result<WebSocketStream<BoxIo>, DialError> {
    open(cfg, true).await
}

async fn open(cfg: &Config, probe: bool) -> Result<WebSocketStream<BoxIo>, DialError> {
    let handshake = |error: &dyn std::fmt::Display| DialError::Handshake(error.to_string());
    let mut request = cfg
        .server
        .uri
        .clone()
        .into_client_request()
        .map_err(|error| handshake(&error))?;
    let mut bearer = HeaderValue::from_str(&format!("Bearer {}", cfg.token.expose()))
        .map_err(|error| handshake(&error))?;
    let agent = HeaderValue::from_str(&user_agent()).map_err(|error| handshake(&error))?;

    bearer.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, bearer);
    request.headers_mut().insert(USER_AGENT, agent);
    request.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static(SUBPROTOCOL),
    );
    if probe {
        request
            .headers_mut()
            .insert(PROBE_HEADER, HeaderValue::from_static("1"));
    }

    let limits = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let connect = async {
        let io = transport(cfg).await?;

        match client_async_with_config(request, io, Some(limits)).await {
            Ok((ws, response)) => {
                let protocol = response.headers().get(SEC_WEBSOCKET_PROTOCOL);
                if protocol.is_none_or(|protocol| protocol != SUBPROTOCOL) {
                    return Err(handshake(&"the server does not speak partsrelay-tunnel.v1"));
                }
                Ok(ws)
            }
            Err(WsError::Http(response)) => Err(rejected(
                response.status(),
                response.headers().get(RETRY_AFTER),
            )),
            Err(WsError::Io(error)) => Err(DialError::Io(error)),
            Err(error) => Err(handshake(&error)),
        }
    };

    timeout(cfg.tuning.connect_timeout, connect)
        .await
        .map_err(|_| DialError::Timeout)?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_of(status: u16, retry_after: Option<&str>) -> Option<Duration> {
        let header = retry_after.map(|value| HeaderValue::from_str(value).unwrap());

        match rejected(StatusCode::from_u16(status).unwrap(), header.as_ref()) {
            DialError::Rejected { retry_after, .. } => retry_after,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn reads_retry_after_only_when_the_server_says_not_now() {
        assert_eq!(wait_of(503, Some("30")), Some(Duration::from_secs(30)));
        assert_eq!(wait_of(429, Some(" 7 ")), Some(Duration::from_secs(7)));
        assert_eq!(wait_of(503, Some("86400")), Some(MAX_RETRY_AFTER));
        assert_eq!(wait_of(503, Some("soon")), None);
        assert_eq!(wait_of(503, None), None);
        assert_eq!(wait_of(500, Some("30")), None);
        assert_eq!(wait_of(404, Some("0")), None);
    }

    #[test]
    fn tells_a_refused_token_from_the_other_refusals() {
        let refused = rejected(StatusCode::UNAUTHORIZED, None);
        let forbidden = rejected(StatusCode::FORBIDDEN, None);

        assert!(matches!(refused, DialError::Auth(401)));
        assert!(matches!(forbidden, DialError::Auth(403)));
    }
}
