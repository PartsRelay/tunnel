use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use data_encoding::BASE64;
use percent_encoding::percent_decode_str;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_RESPONSE_HEAD: usize = 8192;

/// An `http://` proxy that tunnels with CONNECT.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyUrl {
    pub host: String,
    pub port: u16,
    // user and password, already percent decoded
    credentials: Option<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProxyUrlError {
    #[error("only http:// proxies are supported")]
    Scheme,
    #[error("the proxy needs a host, and a port between 1 and 65535")]
    Authority,
}

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("the proxy asks for credentials (407)")]
    Credentials,
    #[error("the proxy refused the connection with status {0}")]
    Status(u16),
    #[error("the proxy answered something that is not HTTP")]
    Malformed,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl FromStr for ProxyUrl {
    type Err = ProxyUrlError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let rest = match text.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
            Some(_) => return Err(ProxyUrlError::Scheme),
            None => text,
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let (credentials, address) = match authority.rsplit_once('@') {
            Some((user, address)) => {
                let (name, password) = user.split_once(':').unwrap_or((user, ""));
                let decode = |part: &str| percent_decode_str(part).decode_utf8_lossy().into_owned();

                (Some((decode(name), decode(password))), address)
            }
            None => (None, authority),
        };
        let (host, port) = match address.strip_prefix('[') {
            Some(v6) => {
                let (host, port) = v6.split_once(']').ok_or(ProxyUrlError::Authority)?;
                (host, port.strip_prefix(':'))
            }
            None => match address.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (address, None),
            },
        };
        let port = match port {
            Some(port) => port.parse().ok().filter(|port| *port > 0),
            None => Some(80),
        };

        match port {
            Some(port) if !host.is_empty() => Ok(Self {
                host: host.to_ascii_lowercase(),
                port,
                credentials,
            }),
            _ => Err(ProxyUrlError::Authority),
        }
    }
}

impl fmt::Display for ProxyUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.host.contains(':') {
            true => write!(f, "http://[{}]:{}", self.host, self.port),
            false => write!(f, "http://{}:{}", self.host, self.port),
        }
    }
}

// Never prints the credentials
impl fmt::Debug for ProxyUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl ProxyUrl {
    pub fn username(&self) -> Option<&str> {
        self.credentials.as_ref().map(|(name, _)| name.as_str())
    }

    pub fn password(&self) -> Option<&str> {
        self.credentials
            .as_ref()
            .map(|(_, password)| password.as_str())
    }

    fn authorization(&self) -> Option<String> {
        let (name, password) = self.credentials.as_ref()?;

        Some(format!(
            "Basic {}",
            BASE64.encode(format!("{name}:{password}").as_bytes())
        ))
    }
}

fn loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn bypassed(no_proxy: &str, host: &str) -> bool {
    no_proxy
        .split(',')
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
        .any(|rule| {
            let rule = rule
                .trim_start_matches("*.")
                .trim_start_matches('.')
                .to_ascii_lowercase();
            let host = host.to_ascii_lowercase();

            rule == "*" || host == rule || host.ends_with(&format!(".{rule}"))
        })
}

/// The proxy for `host`: the configured one, else the environment's.
pub fn resolve(
    explicit: Option<&ProxyUrl>,
    env: impl Fn(&str) -> Option<String>,
    host: &str,
) -> Option<ProxyUrl> {
    if let Some(proxy) = explicit {
        return Some(proxy.clone());
    }

    let read = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| env(name).filter(|value| !value.trim().is_empty()))
    };
    let no_proxy = read(&["NO_PROXY", "no_proxy"]).unwrap_or_default();

    if loopback(host) || bypassed(&no_proxy, host) {
        return None;
    }

    read(&["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"])?
        .trim()
        .parse()
        .ok()
}

/// Asks the proxy for a tunnel to `authority`, over `io`.
pub async fn connect<S>(io: &mut S, authority: &str, proxy: &ProxyUrl) -> Result<(), ProxyError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");

    if let Some(authorization) = proxy.authorization() {
        request.push_str(&format!("Proxy-Authorization: {authorization}\r\n"));
    }
    request.push_str("\r\n");
    io.write_all(request.as_bytes()).await?;
    io.flush().await?;

    // Byte by byte: what follows the head is not ours
    let mut head = Vec::with_capacity(256);
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_RESPONSE_HEAD {
            return Err(ProxyError::Malformed);
        }
        head.push(io.read_u8().await?);
    }

    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers);

    match response.parse(&head) {
        Ok(httparse::Status::Complete(_)) => match response.code {
            Some(200..=299) => Ok(()),
            Some(407) => Err(ProxyError::Credentials),
            Some(status) => Err(ProxyError::Status(status)),
            None => Err(ProxyError::Malformed),
        },
        _ => Err(ProxyError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::duplex;

    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    async fn answered(response: &'static [u8], proxy: &str) -> (Result<(), ProxyError>, String) {
        let (mut client, mut server) = duplex(16 * 1024);
        let proxy: ProxyUrl = proxy.parse().unwrap();
        let asked = tokio::spawn(async move {
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(server.read_u8().await.unwrap());
            }
            // Two writes, as a slow proxy would answer
            let (first, second) = response.split_at(response.len() / 2);
            server.write_all(first).await.unwrap();
            server.flush().await.unwrap();
            server.write_all(second).await.unwrap();
            (String::from_utf8(request).unwrap(), server)
        });
        let result = connect(&mut client, "connectors.example.com:443", &proxy).await;

        (result, asked.await.unwrap().0)
    }

    #[test]
    fn reads_a_proxy_url_with_its_credentials_decoded() {
        let proxy: ProxyUrl = "http://mario:p%40ss%3Aword@Proxy.Local:3128/"
            .parse()
            .unwrap();

        assert_eq!((proxy.host.as_str(), proxy.port), ("proxy.local", 3128));
        assert_eq!(
            (proxy.username(), proxy.password()),
            (Some("mario"), Some("p@ss:word"))
        );
        assert_eq!(format!("{proxy:?}"), "http://proxy.local:3128");
        assert_eq!("proxy.local".parse::<ProxyUrl>().unwrap().port, 80);
        assert_eq!("http://[::1]:8080".parse::<ProxyUrl>().unwrap().host, "::1");
        assert_eq!(
            "https://proxy.local".parse::<ProxyUrl>(),
            Err(ProxyUrlError::Scheme)
        );
        assert_eq!(
            "socks5://proxy.local:1080".parse::<ProxyUrl>(),
            Err(ProxyUrlError::Scheme)
        );
        assert_eq!(
            "http://:3128".parse::<ProxyUrl>(),
            Err(ProxyUrlError::Authority)
        );
        assert_eq!(
            "http://proxy.local:0".parse::<ProxyUrl>(),
            Err(ProxyUrlError::Authority)
        );
    }

    #[test]
    fn prefers_the_configured_proxy_then_the_environment() {
        let configured: ProxyUrl = "http://configured:8080".parse().unwrap();
        let vars = [
            ("HTTPS_PROXY", "http://from-env:3128"),
            ("NO_PROXY", "localhost,.internal"),
        ];

        let proxy = |explicit, host| resolve(explicit, env(&vars), host).map(|proxy| proxy.host);

        assert_eq!(
            proxy(Some(&configured), "example.com").as_deref(),
            Some("configured")
        );
        assert_eq!(
            proxy(Some(&configured), "erp.internal").as_deref(),
            Some("configured")
        );
        assert_eq!(proxy(None, "example.com").as_deref(), Some("from-env"));
        assert_eq!(proxy(None, "erp.internal"), None);
        assert_eq!(proxy(None, "internal"), None);
        assert_eq!(proxy(None, "127.0.0.1"), None);
        assert_eq!(
            resolve(None, env(&[("all_proxy", "http://all:1")]), "a.com")
                .unwrap()
                .host,
            "all"
        );
        assert_eq!(
            resolve(None, env(&[("HTTPS_PROXY", "socks5://x:1")]), "a.com"),
            None
        );
        assert_eq!(resolve(None, env(&[]), "example.com"), None);
    }

    #[tokio::test]
    async fn opens_a_tunnel_and_sends_the_credentials() {
        let (result, request) = answered(
            b"HTTP/1.1 200 Connection established\r\n\r\n",
            "http://mario:secret@p:1",
        )
        .await;

        assert!(result.is_ok());
        assert!(request.starts_with("CONNECT connectors.example.com:443 HTTP/1.1\r\n"));
        assert!(request.contains("Host: connectors.example.com:443\r\n"));
        assert!(request.contains("Proxy-Authorization: Basic bWFyaW86c2VjcmV0\r\n"));
    }

    #[tokio::test]
    async fn tells_why_the_proxy_refused() {
        let denied = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n";
        let login = b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n";

        let (result, request) = answered(login, "http://p:1").await;
        assert!(matches!(result, Err(ProxyError::Credentials)));
        assert!(!request.contains("Proxy-Authorization"));
        assert!(matches!(
            answered(denied, "http://p:1").await.0,
            Err(ProxyError::Status(403))
        ));
        assert!(matches!(
            answered(b"SSH-2.0-OpenSSH\r\n\r\n", "p:1").await.0,
            Err(ProxyError::Malformed)
        ));
    }

    #[tokio::test]
    async fn gives_up_on_an_endless_response_head() {
        let (mut client, mut server) = duplex(64 * 1024);
        let proxy: ProxyUrl = "http://p:1".parse().unwrap();

        tokio::spawn(async move {
            let _ = server.write_all(&vec![b'a'; MAX_RESPONSE_HEAD + 16]).await;
            server
        });

        assert!(matches!(
            connect(&mut client, "a:1", &proxy).await,
            Err(ProxyError::Malformed)
        ));
    }
}
