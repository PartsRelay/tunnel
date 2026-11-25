use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::{TcpStream, lookup_host};
use tokio::time::{Instant, timeout, timeout_at};

use crate::allow::{Host, Target};

/// Why a target could not be connected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unreachable {
    #[error("cannot resolve the name: {0}")]
    Resolve(String),
    #[error("the connection was refused")]
    Refused,
    #[error("no answer in time")]
    Timeout,
    #[error("cannot connect: {0}")]
    Failed(String),
}

/// Tries each address for an equal share of `limit`.
async fn first<T, F, Dial>(
    addresses: &[SocketAddr],
    limit: Duration,
    dial: F,
) -> Result<T, Unreachable>
where
    F: Fn(SocketAddr) -> Dial,
    Dial: Future<Output = io::Result<T>>,
{
    let mut failure = Unreachable::Resolve("the name has no address".into());
    // A silent address leaves time for the others
    let share = limit / u32::try_from(addresses.len().max(1)).unwrap_or(u32::MAX);

    for address in addresses {
        failure = match timeout(share, dial(*address)).await {
            Ok(Ok(connected)) => return Ok(connected),
            Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => {
                Unreachable::Refused
            }
            Ok(Err(error)) => Unreachable::Failed(error.to_string()),
            Err(_) => Unreachable::Timeout,
        };
    }

    Err(failure)
}

/// Connects to `target` within `limit`, whichever address answers.
pub async fn connect(target: &Target, limit: Duration) -> Result<TcpStream, Unreachable> {
    let deadline = Instant::now() + limit;
    let addresses: Vec<SocketAddr> = match &target.host {
        Host::Ip(ip) => vec![SocketAddr::new(*ip, target.port)],
        Host::Name(name) => timeout_at(deadline, lookup_host((name.as_str(), target.port)))
            .await
            .map_err(|_| Unreachable::Timeout)?
            .map_err(|error| Unreachable::Resolve(error.to_string()))?
            .collect(),
    };
    let left = deadline.saturating_duration_since(Instant::now());

    first(&addresses, left, TcpStream::connect).await
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use super::*;

    fn address(last: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, last], 80))
    }

    #[tokio::test(start_paused = true)]
    async fn reaches_the_second_address_when_the_first_is_silent() {
        let started = Instant::now();
        let dial = |address: SocketAddr| async move {
            if address == self::address(1) {
                pending::<()>().await;
            }
            Ok(address)
        };

        let reached = first(&[address(1), address(2)], Duration::from_secs(10), dial).await;

        assert_eq!(reached, Ok(address(2)));
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn tells_why_the_last_address_failed() {
        let refused = |_| async { Err::<(), _>(io::Error::from(io::ErrorKind::ConnectionRefused)) };
        let silent = |_| pending::<io::Result<()>>();
        let none = |_| async { Ok(()) };

        assert_eq!(
            first(&[address(1)], Duration::from_secs(10), refused).await,
            Err(Unreachable::Refused)
        );
        assert_eq!(
            first(&[address(1)], Duration::from_secs(10), silent).await,
            Err(Unreachable::Timeout)
        );
        assert!(matches!(
            first(&[], Duration::from_secs(10), none).await,
            Err(Unreachable::Resolve(_))
        ));
    }
}
