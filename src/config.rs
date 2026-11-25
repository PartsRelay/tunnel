use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;
use tokio_tungstenite::tungstenite::http::Uri;

use crate::allow::{AllowList, Target};
use crate::proxy::ProxyUrl;

pub const DEFAULT_SERVER: &str = "wss://connectors.partsrelay.com/v1/tunnel";
pub const DEFAULT_MAX_STREAMS: usize = 64;
const MAX_STREAMS_LIMIT: usize = 1024;

/// A value that never reaches a log.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Where the tunnel connects: `wss://`, or `ws://` on loopback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerUrl {
    pub uri: Uri,
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

impl ServerUrl {
    pub fn authority(&self) -> String {
        match self.host.contains(':') {
            true => format!("[{}]:{}", self.host, self.port),
            false => format!("{}:{}", self.host, self.port),
        }
    }
}

fn loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

impl FromStr for ServerUrl {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let uri: Uri = text.parse().map_err(|_| "it is not a URL".to_string())?;
        let tls = match uri.scheme_str() {
            Some("wss") => true,
            Some("ws") => false,
            _ => return Err("it must start with wss://".into()),
        };
        let host = uri.host().unwrap_or_default();
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();

        if host.is_empty() {
            return Err("it has no host".into());
        }
        if !tls && !loopback(&host) {
            return Err("ws:// is only for this computer, use wss://".into());
        }

        let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });

        Ok(Self {
            uri,
            host,
            port,
            tls,
        })
    }
}

/// Timings, shortened only by the tests.
#[derive(Debug, Clone)]
pub struct Tuning {
    pub header_timeout: Duration,
    pub connect_timeout: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub auth_backoff_min: Duration,
    pub auth_backoff_max: Duration,
    pub replaced_wait: Duration,
    pub healthy_after: Duration,
    pub keepalive: Duration,
    pub silence: Duration,
    pub abandoned_after: Duration,
    pub drain: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            header_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(10),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            auth_backoff_min: Duration::from_secs(60),
            auth_backoff_max: Duration::from_secs(15 * 60),
            replaced_wait: Duration::from_secs(5 * 60),
            healthy_after: Duration::from_secs(60),
            keepalive: Duration::from_secs(20),
            silence: Duration::from_secs(60),
            abandoned_after: Duration::from_secs(60),
            drain: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerUrl,
    pub token: Secret,
    pub allow: AllowList,
    pub auto_update: bool,
    pub proxy: Option<ProxyUrl>,
    pub ca_file: Option<PathBuf>,
    pub max_streams: usize,
    pub tuning: Tuning,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the configuration file is not valid: {0}")]
    Parse(String),
    #[error("set the token, in the file or in PARTSRELAY_TUNNEL_TOKEN")]
    Token,
    #[error("{field} is not valid: {reason}")]
    Invalid { field: &'static str, reason: String },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    server: Option<String>,
    token: Option<String>,
    allow: Option<Vec<String>>,
    auto_update: Option<bool>,
    proxy: Option<String>,
    ca_file: Option<PathBuf>,
    max_streams: Option<usize>,
}

const SETTINGS: [&str; 7] = [
    "server",
    "token",
    "allow",
    "auto_update",
    "proxy",
    "ca_file",
    "max_streams",
];

/// Where the file is wrong, never what the line holds.
fn unreadable(text: &str, error: &toml::de::Error) -> ConfigError {
    let Some(start) = error.span().map(|span| span.start.min(text.len())) else {
        return ConfigError::Parse("it is not valid TOML".into());
    };
    let before = &text.as_bytes()[..start];
    let number = before.iter().filter(|byte| **byte == b'\n').count() + 1;
    let line = text.lines().nth(number - 1).unwrap_or_default();
    // A secret may be anywhere on the line
    let setting = line
        .split_once('=')
        .map(|(key, _)| key.trim())
        .and_then(|key| SETTINGS.iter().find(|setting| **setting == key));

    ConfigError::Parse(match setting {
        Some(setting) => format!("line {number}: the value of {setting} is not valid"),
        None if line.contains('=') => format!(
            "line {number}: unknown setting, use one of {}",
            SETTINGS.join(", ")
        ),
        None => format!("line {number} is not valid TOML"),
    })
}

fn invalid(field: &'static str, reason: impl fmt::Display) -> ConfigError {
    ConfigError::Invalid {
        field,
        reason: reason.to_string(),
    }
}

/// The file every install reads when `--config` is not given.
pub fn default_path() -> PathBuf {
    if cfg!(windows) {
        let data = std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
        PathBuf::from(data).join("PartsRelay").join("tunnel.toml")
    } else {
        PathBuf::from("/etc/partsrelay/tunnel.toml")
    }
}

impl Config {
    /// Reads the file, then lets the environment override it.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let read = |path: &Path| {
            std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                path: path.to_path_buf(),
                source,
            })
        };
        let file = match path {
            Some(path) => Some(read(path)?),
            None => {
                let path = default_path();
                if path.exists() {
                    Some(read(&path)?)
                } else {
                    None
                }
            }
        };

        Self::from_sources(file.as_deref(), |name| std::env::var(name).ok())
    }

    pub fn from_sources(
        file: Option<&str>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let file: FileConfig = match file {
            Some(text) => toml::from_str(text).map_err(|error| unreadable(text, &error))?,
            None => FileConfig::default(),
        };
        let env = |name: &str| {
            env(&format!("PARTSRELAY_TUNNEL_{name}")).filter(|value| !value.trim().is_empty())
        };

        let server = env("SERVER")
            .or(file.server)
            .unwrap_or_else(|| DEFAULT_SERVER.into());
        let server = server
            .trim()
            .parse()
            .map_err(|reason: String| invalid("server", reason))?;

        let token = env("TOKEN")
            .or(file.token)
            .unwrap_or_default()
            .trim()
            .to_string();
        if token.is_empty() {
            return Err(ConfigError::Token);
        }
        if !token.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(invalid(
                "token",
                "it has spaces or characters outside ASCII",
            ));
        }

        let allow = match env("ALLOW") {
            Some(list) => list
                .split(',')
                .map(|target| target.trim().to_string())
                .collect(),
            None => file.allow.unwrap_or_default(),
        };
        let allow = allow
            .iter()
            .filter(|target| !target.is_empty())
            .map(|target| {
                target
                    .parse::<Target>()
                    .map_err(|error| invalid("allow", format!("{target}: {error}")))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let auto_update = match env("AUTO_UPDATE") {
            Some(value) => match value.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => true,
                "false" | "0" | "no" | "off" => false,
                _ => return Err(invalid("auto_update", "use true or false")),
            },
            None => file.auto_update.unwrap_or(true),
        };

        let proxy = env("PROXY")
            .or(file.proxy)
            .map(|proxy| {
                proxy
                    .trim()
                    .parse::<ProxyUrl>()
                    .map_err(|error| invalid("proxy", error))
            })
            .transpose()?;

        let max_streams = match env("MAX_STREAMS") {
            Some(value) => value
                .trim()
                .parse()
                .map_err(|_| invalid("max_streams", "use a number"))?,
            None => file.max_streams.unwrap_or(DEFAULT_MAX_STREAMS),
        };
        if !(1..=MAX_STREAMS_LIMIT).contains(&max_streams) {
            return Err(invalid(
                "max_streams",
                format!("use 1 to {MAX_STREAMS_LIMIT}"),
            ));
        }

        Ok(Self {
            server,
            token: Secret::new(token),
            allow: AllowList::new(allow),
            auto_update,
            proxy,
            ca_file: env("CA_FILE").map(PathBuf::from).or(file.ca_file),
            max_streams,
            tuning: Tuning::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"
        server = "wss://connectors.example.com/v1/tunnel"
        token = "prt_file"
        allow = ["erp.local:8080", "192.168.1.10:1433"]
        auto_update = false
        proxy = "http://proxy.local:3128"
        max_streams = 8
    "#;

    fn none(_: &str) -> Option<String> {
        None
    }

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn reads_every_setting_of_the_file() {
        let config = Config::from_sources(Some(FILE), none).unwrap();

        assert_eq!(config.server.host, "connectors.example.com");
        assert_eq!((config.server.port, config.server.tls), (443, true));
        assert_eq!(config.token.expose(), "prt_file");
        assert!(config.allow.permits(&"erp.local:8080".parse().unwrap()));
        assert!(config.allow.permits(&"192.168.1.10:1433".parse().unwrap()));
        assert!(!config.auto_update);
        assert_eq!(config.proxy.unwrap().port, 3128);
        assert_eq!(config.max_streams, 8);
    }

    #[test]
    fn runs_from_the_environment_alone_with_the_defaults() {
        let vars = [
            ("PARTSRELAY_TUNNEL_TOKEN", "prt_env"),
            ("PARTSRELAY_TUNNEL_ALLOW", "a:1, b:2"),
        ];
        let config = Config::from_sources(None, env(&vars)).unwrap();

        assert_eq!(config.server.uri.to_string(), DEFAULT_SERVER);
        assert_eq!(config.token.expose(), "prt_env");
        assert_eq!(config.allow.targets().len(), 2);
        assert!(config.auto_update);
        assert_eq!(config.max_streams, DEFAULT_MAX_STREAMS);
        assert!(config.proxy.is_none());
    }

    #[test]
    fn lets_the_environment_override_the_file() {
        let vars = [
            ("PARTSRELAY_TUNNEL_TOKEN", "prt_env"),
            ("PARTSRELAY_TUNNEL_SERVER", "ws://127.0.0.1:9000/v1/tunnel"),
            ("PARTSRELAY_TUNNEL_AUTO_UPDATE", "true"),
            ("PARTSRELAY_TUNNEL_PROXY", " "),
        ];
        let config = Config::from_sources(Some(FILE), env(&vars)).unwrap();

        assert_eq!(config.token.expose(), "prt_env");
        assert_eq!(
            (config.server.host.as_str(), config.server.port),
            ("127.0.0.1", 9000)
        );
        assert!(!config.server.tls);
        assert!(config.auto_update);
        assert_eq!(config.proxy.unwrap().host, "proxy.local");
    }

    #[test]
    fn refuses_what_it_does_not_understand() {
        let broken = |file: &str| {
            Config::from_sources(Some(file), none)
                .unwrap_err()
                .to_string()
        };

        assert!(matches!(
            Config::from_sources(None, none),
            Err(ConfigError::Token)
        ));
        assert!(broken("token = 'a'\ntokn = 'b'").contains("line 2: unknown setting"));
        assert!(broken("token = 'a b'").starts_with("token is not valid"));
        assert!(
            broken("token = 'a'\nallow = ['erp.local']").contains("erp.local: expected host:port")
        );
        assert!(broken("token = 'a'\nallow = ['*:80']").starts_with("allow is not valid"));
        assert!(broken("token = 'a'\nmax_streams = 0").starts_with("max_streams is not valid"));
        assert!(broken("token = 'a'\nproxy = 'socks5://p:1'").starts_with("proxy is not valid"));
    }

    #[test]
    fn never_repeats_a_line_it_cannot_read() {
        let broken = |file: &str| {
            Config::from_sources(Some(file), none)
                .unwrap_err()
                .to_string()
        };
        let files = [
            "server = 'wss://a.example'\ntoken = \"prt_SECRET_live_123",
            "token = prt_SECRET_live_123",
            "tokn = \"prt_SECRET_live_123\"",
            "token = 'a'\nproxy = \"http://mario:prt_SECRET@proxy.local:3128",
            "token = 'a'\nmax_streams = \"prt_SECRET\"",
            "token = 'a'\nauto_update = prt_SECRET",
            "prt_SECRET_live_123",
            "prt_SECRET== 'x'",
        ];

        for file in files {
            let message = broken(file);

            assert!(!message.contains("SECRET"), "{message}");
            assert!(message.contains("line "), "{message}");
        }
        assert!(broken(files[1]).ends_with("line 1: the value of token is not valid"));
        assert!(broken(files[3]).ends_with("line 2: the value of proxy is not valid"));
    }

    #[test]
    fn takes_plain_websockets_only_on_this_computer() {
        let server = |url: &str| url.parse::<ServerUrl>();

        assert!(server("ws://localhost:8080/v1/tunnel").is_ok());
        assert!(
            server("ws://[::1]:8080/v1/tunnel").is_ok_and(|url| url.authority() == "[::1]:8080")
        );
        assert!(server("ws://connectors.example.com/v1/tunnel").is_err());
        assert!(server("ws://192.168.1.4/v1/tunnel").is_err());
        assert!(server("https://connectors.example.com").is_err());
        assert!(server("wss://Connectors.Example.com").is_ok_and(|url| url.port == 443));
    }

    #[test]
    fn never_prints_the_token() {
        let config = Config::from_sources(Some(FILE), none).unwrap();

        assert!(!format!("{config:?}").contains("prt_file"));
        assert!(format!("{config:?}").contains("[redacted]"));
    }
}
