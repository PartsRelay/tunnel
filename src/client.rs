use std::sync::Arc;
use std::time::Duration;

use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::dial::{DialError, dial};
use crate::session::{SessionEnd, serve};

/// Close code: another tunnel took over with this token.
pub const CLOSE_REPLACED: u16 = 4001;
/// Close code of a token the server no longer accepts.
pub const CLOSE_POLICY: u16 = 1008;

/// Doubles from `min` to `max`, waiting in the upper half.
#[derive(Debug, Clone)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    pub fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max: max.max(min),
            next: min,
        }
    }

    pub fn delay(&mut self) -> Duration {
        let ceiling = self.next;
        let half = ceiling / 2;
        let jitter = fastrand::u64(0..=u64::try_from(half.as_millis()).unwrap_or(u64::MAX));

        self.next = (ceiling * 2).min(self.max);

        half + Duration::from_millis(jitter)
    }

    pub fn reset(&mut self) {
        self.next = self.min;
    }
}

/// Keeps one session open with the server until `stop`.
pub async fn run(cfg: Arc<Config>, stop: CancellationToken) {
    let tuning = &cfg.tuning;
    let mut backoff = Backoff::new(tuning.backoff_min, tuning.backoff_max);
    let mut refused = Backoff::new(tuning.auth_backoff_min, tuning.auth_backoff_max);

    while !stop.is_cancelled() {
        let dialed = tokio::select! {
            dialed = dial(&cfg) => dialed,
            _ = stop.cancelled() => break,
        };
        let wait = match dialed {
            Ok(ws) => {
                info!(server = %cfg.server.uri, "connected");
                let started = Instant::now();
                let end = serve(ws, cfg.clone(), stop.clone()).await;

                if started.elapsed() >= tuning.healthy_after {
                    backoff.reset();
                    refused.reset();
                }

                match end {
                    SessionEnd::Stopped => break,
                    SessionEnd::Closed(Some(CLOSE_REPLACED)) => {
                        warn!("another tunnel took over with this token");
                        tuning.replaced_wait
                    }
                    SessionEnd::Closed(Some(CLOSE_POLICY)) => {
                        error!("the server no longer accepts the token");
                        refused.delay()
                    }
                    SessionEnd::Closed(code) => {
                        info!(?code, "the server closed the connection");
                        backoff.delay()
                    }
                    SessionEnd::Failed(error) => {
                        warn!(%error, "connection lost");
                        backoff.delay()
                    }
                }
            }
            Err(error @ DialError::Auth(_)) => {
                error!(%error, "check the token of this tunnel");
                refused.delay()
            }
            Err(DialError::Rejected {
                status,
                retry_after,
            }) => {
                warn!(status, "the server is not accepting tunnels now");
                retry_after.unwrap_or_else(|| backoff.delay())
            }
            Err(error) => {
                warn!(%error, "cannot connect");
                backoff.delay()
            }
        };

        tokio::select! {
            _ = sleep(wait) => {}
            _ = stop.cancelled() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_longer_each_time_up_to_the_maximum() {
        let (min, max) = (Duration::from_secs(1), Duration::from_secs(60));
        let mut backoff = Backoff::new(min, max);

        for ceiling in [1, 2, 4, 8, 16, 32, 60, 60] {
            let delay = backoff.delay();
            let ceiling = Duration::from_secs(ceiling);

            assert!(
                delay >= ceiling / 2 && delay <= ceiling,
                "{delay:?} for {ceiling:?}"
            );
        }
    }

    #[test]
    fn starts_again_from_the_minimum_after_a_reset() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(60));

        for _ in 0..5 {
            backoff.delay();
        }
        backoff.reset();

        assert!(backoff.delay() <= Duration::from_secs(1));
    }
}
