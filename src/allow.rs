use std::collections::HashSet;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Host {
    Name(String),
    Ip(IpAddr),
}

/// A `host:port` the tunnel may be asked to reach.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub host: Host,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetError {
    #[error("expected host:port")]
    Shape,
    #[error("the port must be between 1 and 65535")]
    Port,
    #[error("the host is not a name or an IP address")]
    Host,
}

fn name(host: &str) -> Result<Host, TargetError> {
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    let label = |label: &str| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    };

    if host.is_empty() || host.len() > 253 || !host.split('.').all(label) {
        return Err(TargetError::Host);
    }

    Ok(Host::Name(host))
}

impl FromStr for Target {
    type Err = TargetError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (host, port) = text.rsplit_once(':').ok_or(TargetError::Shape)?;
        let digits = !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit());
        let port = match port.parse::<u16>() {
            Ok(port) if digits && port > 0 => port,
            _ => return Err(TargetError::Port),
        };
        let host = match host
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            Some(v6) => Host::Ip(IpAddr::V6(v6.parse().map_err(|_| TargetError::Host)?)),
            None => match host.parse::<std::net::Ipv4Addr>() {
                Ok(v4) => Host::Ip(IpAddr::V4(v4)),
                Err(_) => name(host)?,
            },
        };

        Ok(Target { host, port })
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Host::Name(name) => write!(f, "{name}:{}", self.port),
            Host::Ip(IpAddr::V4(ip)) => write!(f, "{ip}:{}", self.port),
            Host::Ip(IpAddr::V6(ip)) => write!(f, "[{ip}]:{}", self.port),
        }
    }
}

/// Exact targets only: no wildcards, no name for its address.
#[derive(Debug, Clone, Default)]
pub struct AllowList(HashSet<Target>);

impl AllowList {
    pub fn new(targets: impl IntoIterator<Item = Target>) -> Self {
        Self(targets.into_iter().collect())
    }

    pub fn permits(&self, target: &Target) -> bool {
        self.0.contains(target)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn targets(&self) -> Vec<&Target> {
        let mut targets: Vec<_> = self.0.iter().collect();
        targets.sort_by_key(|target| target.to_string());
        targets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(targets: &[&str]) -> AllowList {
        AllowList::new(targets.iter().map(|target| target.parse().unwrap()))
    }

    #[test]
    fn matches_a_name_whatever_its_case_and_trailing_dot() {
        let allow = list(&["ERP.Local.:8080"]);

        assert!(allow.permits(&"erp.local:8080".parse().unwrap()));
        assert!(allow.permits(&"Erp.LOCAL.:8080".parse().unwrap()));
        assert!(!allow.permits(&"erp.local:8081".parse().unwrap()));
        assert!(!allow.permits(&"other.local:8080".parse().unwrap()));
    }

    #[test]
    fn compares_addresses_parsed_and_never_through_a_name() {
        let allow = list(&["192.168.1.10:1433", "[fe80::1]:5432", "localhost:80"]);

        assert!(allow.permits(&"192.168.1.10:1433".parse().unwrap()));
        assert!(allow.permits(&"[FE80:0:0:0:0:0:0:1]:5432".parse().unwrap()));
        assert!(!allow.permits(&"127.0.0.1:80".parse().unwrap()));
        assert_eq!(
            "[fe80::1]:5432".parse::<Target>().unwrap().to_string(),
            "[fe80::1]:5432"
        );
    }

    #[test]
    fn refuses_a_target_without_an_exact_host_and_port() {
        for text in [
            "erp.local",
            "erp.local:",
            "erp.local:0",
            "erp.local:70000",
            ":80",
        ] {
            assert!(text.parse::<Target>().is_err(), "{text}");
        }
        for text in [
            "*:80",
            "*.local:80",
            "erp local:80",
            "fe80::1:80",
            "erp.local:+80",
            "a..b:1",
        ] {
            assert!(text.parse::<Target>().is_err(), "{text}");
        }
    }
}
