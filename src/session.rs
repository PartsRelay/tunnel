use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf, copy};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until, timeout, timeout_at};
use tokio_tungstenite::WebSocketStream;
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use yamux::{Config as YamuxConfig, Connection, ConnectionError, DEFAULT_CREDIT, Mode, Stream};

use crate::allow::Target;
use crate::config::Config;
use crate::dial::BoxIo;
use crate::proto::{ErrorCode, OpenRequest, OpenResponse, read_frame, write_frame};
use crate::reach::{self, Unreachable};
use crate::wsio::{Keepalive, WsIo};

/// Time to say goodbye once nothing is left to send.
const GOODBYE: Duration = Duration::from_secs(1);

/// Why a session with the server is over.
#[derive(Debug)]
pub enum SessionEnd {
    /// This side stopped it, after draining its streams.
    Stopped,
    /// The server closed it, maybe with a close code.
    Closed(Option<u16>),
    Failed(io::Error),
}

/// Streams yamux lets in: room to refuse the ones beyond.
pub fn stream_capacity(max_streams: usize) -> usize {
    max_streams * 2
}

fn yamux(cfg: &Config) -> YamuxConfig {
    let streams = stream_capacity(cfg.max_streams);
    let mut config = YamuxConfig::default();

    config.set_max_num_streams(streams);
    // No window grows past the default: memory stays bounded
    config.set_max_connection_receive_window(Some(streams * DEFAULT_CREDIT as usize));
    config
}

fn code_of(unreachable: &Unreachable) -> ErrorCode {
    match unreachable {
        Unreachable::Resolve(_) => ErrorCode::ResolveFailed,
        Unreachable::Refused => ErrorCode::ConnectRefused,
        Unreachable::Timeout => ErrorCode::ConnectTimeout,
        Unreachable::Failed(_) => ErrorCode::ConnectFailed,
    }
}

async fn refuse(stream: &mut Compat<Stream>, limit: Duration, code: ErrorCode, message: String) {
    debug!(?code, %message, "stream refused");
    // A server that stopped reading must not keep the task
    let _ = timeout(limit, async {
        write_frame(stream, &OpenResponse::refused(code, message)).await?;
        stream.shutdown().await
    })
    .await;
}

/// One yamux stream, read and written by two halves.
#[derive(Clone)]
struct Halves(Arc<Mutex<Compat<Stream>>>);

impl Halves {
    fn stream(&self) -> MutexGuard<'_, Compat<Stream>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reset by the opener, or closed in both directions.
    fn is_closed(&self) -> bool {
        self.stream().get_ref().is_closed()
    }
}

impl AsyncRead for Halves {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.stream()).poll_read(cx, buf)
    }
}

impl AsyncWrite for Halves {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.stream()).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.stream()).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.stream()).poll_shutdown(cx)
    }
}

/// Copies both ways until both ends finished or one failed.
async fn pump(
    stream: Compat<Stream>,
    tcp: &mut TcpStream,
    abandoned_after: Duration,
) -> io::Result<(u64, u64)> {
    let halves = Halves(Arc::new(Mutex::new(stream)));
    let (mut from_server, mut to_server) = (halves.clone(), halves.clone());
    let (mut from_target, mut to_target) = tcp.split();
    let (asked, answered) = (AtomicBool::new(false), AtomicBool::new(false));

    let up = async {
        let sent = copy(&mut from_server, &mut to_target).await?;

        // A reset reads like an end: hang up
        if halves.is_closed() && !answered.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "the server reset the stream",
            ));
        }
        asked.store(true, Ordering::Relaxed);
        to_target.shutdown().await?;

        Ok(sent)
    };
    let down = async {
        let mut buffer = vec![0; 16 * 1024];
        let mut received = 0;

        loop {
            let read = from_target.read(&mut buffer).await?;
            if read == 0 {
                break;
            }

            let mut rest = &buffer[..read];
            while !rest.is_empty() {
                match timeout(abandoned_after, to_server.write(rest)).await {
                    Ok(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(written) => rest = &rest[written?..],
                    // Slow is fine while the opener still writes
                    Err(_) if asked.load(Ordering::Relaxed) => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "the server stopped reading the stream",
                        ));
                    }
                    Err(_) => {}
                }
            }
            received += read as u64;
        }
        answered.store(true, Ordering::Relaxed);
        to_server.shutdown().await?;

        Ok(received)
    };

    tokio::try_join!(up, down)
}

async fn relay(stream: Stream, cfg: Arc<Config>, slot: Result<OwnedSemaphorePermit, ErrorCode>) {
    let mut stream = stream.compat();
    let patience = cfg.tuning.header_timeout;
    let _slot = match slot {
        Ok(slot) => slot,
        Err(code) => {
            let reason = match code {
                ErrorCode::ShuttingDown => "the tunnel is restarting",
                _ => "too many streams are open",
            };
            return refuse(&mut stream, patience, code, reason.into()).await;
        }
    };
    let request = match timeout(patience, read_frame::<_, OpenRequest>(&mut stream)).await {
        Ok(Ok(request)) => request,
        Ok(Err(error)) => {
            return refuse(
                &mut stream,
                patience,
                ErrorCode::BadRequest,
                error.to_string(),
            )
            .await;
        }
        Err(_) => {
            let late = "no header in time".into();
            return refuse(&mut stream, patience, ErrorCode::BadRequest, late).await;
        }
    };
    let target: Target = match request.target.parse() {
        Ok(target) => target,
        Err(error) => {
            return refuse(
                &mut stream,
                patience,
                ErrorCode::BadRequest,
                error.to_string(),
            )
            .await;
        }
    };

    if !cfg.allow.permits(&target) {
        warn!(%target, "target not in the allow list");
        let reason = format!("{target} is not allowed");
        return refuse(&mut stream, patience, ErrorCode::NotAllowed, reason).await;
    }

    let mut tcp = match reach::connect(&target, cfg.tuning.connect_timeout).await {
        Ok(tcp) => tcp,
        Err(unreachable) => {
            warn!(%target, %unreachable, "target unreachable");
            let reason = format!("{target}: {unreachable}");
            return refuse(&mut stream, patience, code_of(&unreachable), reason).await;
        }
    };
    let _ = tcp.set_nodelay(true);

    if write_frame(&mut stream, &OpenResponse::accepted())
        .await
        .is_err()
    {
        return;
    }

    debug!(%target, "stream open");
    match pump(stream, &mut tcp, cfg.tuning.abandoned_after).await {
        Ok((sent, received)) => debug!(%target, sent, received, "stream closed"),
        Err(error) => debug!(%target, %error, "stream failed"),
    }
}

fn failure(error: ConnectionError) -> io::Error {
    match error {
        ConnectionError::Io(error) => error,
        other => io::Error::other(other.to_string()),
    }
}

/// Serves the streams the server opens, until either side ends.
pub async fn serve(
    ws: WebSocketStream<BoxIo>,
    cfg: Arc<Config>,
    stop: CancellationToken,
) -> SessionEnd {
    let keepalive = Keepalive {
        every: cfg.tuning.keepalive,
        silence: cfg.tuning.silence,
    };
    let (io, link) = WsIo::new(ws, keepalive);
    let mut connection = Connection::new(io.compat(), yamux(&cfg), Mode::Server);
    let slots = Arc::new(Semaphore::new(cfg.max_streams));
    let mut streams = JoinSet::new();
    let mut drain_until: Option<Instant> = None;

    // None: this side stops. Some: the other did
    let ended = loop {
        let draining = drain_until.is_some();
        let deadline = drain_until.unwrap_or_else(Instant::now);

        tokio::select! {
            next = poll_fn(|cx| connection.poll_next_inbound(cx)) => match next {
                Some(Ok(stream)) => {
                    let slot = match draining {
                        true => Err(ErrorCode::ShuttingDown),
                        false => slots
                            .clone()
                            .try_acquire_owned()
                            .map_err(|_| ErrorCode::TooManyStreams),
                    };
                    streams.spawn(relay(stream, cfg.clone(), slot));
                }
                Some(Err(error)) => break Some(Err(error)),
                None => break Some(Ok(())),
            },
            Some(_) = streams.join_next(), if !streams.is_empty() => {}
            _ = stop.cancelled(), if !draining => {
                info!(open = streams.len(), "draining the open streams");
                drain_until = Some(Instant::now() + cfg.tuning.drain);
            }
            _ = sleep_until(deadline), if draining => break None,
        }

        if drain_until.is_some() && streams.is_empty() {
            break None;
        }
    };

    streams.shutdown().await;

    match ended {
        None => {
            // What is queued, a GoAway, then a WebSocket Close
            let limit = drain_until
                .unwrap_or_else(Instant::now)
                .max(Instant::now() + GOODBYE);
            let _ = timeout_at(limit, poll_fn(|cx| connection.poll_close(cx))).await;
            // Unread bytes at the close would reset the connection
            link.closed(GOODBYE).await;

            SessionEnd::Stopped
        }
        Some(result) => {
            drop(connection);
            // A close code may follow the GoAway
            let code = match &result {
                Ok(()) => link.closed(GOODBYE).await,
                Err(_) => link.close_code(),
            };

            match (code, result) {
                (None, Err(error)) => SessionEnd::Failed(failure(error)),
                (code, _) => SessionEnd::Closed(code),
            }
        }
    }
}
