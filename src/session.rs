use std::future::poll_fn;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::io::{AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpStream, lookup_host};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_util::sync::CancellationToken;
use tokio_yamux::{Config as YamuxConfig, Session, StreamHandle};
use tracing::{debug, info, warn};

use crate::allow::{Host, Target};
use crate::config::Config;
use crate::dial::BoxIo;
use crate::proto::{ErrorCode, OpenRequest, OpenResponse, read_frame, write_frame};
use crate::wsio::WsIo;

/// Why a session with the server is over.
#[derive(Debug)]
pub enum SessionEnd {
    /// This side stopped it, after draining its streams.
    Stopped,
    /// The server closed it, maybe with a close code.
    Closed(Option<u16>),
    Failed(io::Error),
}

fn yamux(cfg: &Config) -> YamuxConfig {
    YamuxConfig {
        accept_backlog: cfg.max_streams,
        max_stream_count: cfg.max_streams,
        keepalive_interval: cfg.tuning.keepalive,
        ..YamuxConfig::default()
    }
}

async fn connect(target: &Target, limit: Duration) -> Result<TcpStream, (ErrorCode, String)> {
    let attempt = async {
        let addresses: Vec<SocketAddr> = match &target.host {
            Host::Ip(ip) => vec![SocketAddr::new(*ip, target.port)],
            Host::Name(name) => lookup_host((name.as_str(), target.port))
                .await
                .map_err(|error| (ErrorCode::ResolveFailed, error.to_string()))?
                .collect(),
        };
        let mut failure = (ErrorCode::ResolveFailed, format!("{target} has no address"));

        for address in addresses {
            match TcpStream::connect(address).await {
                Ok(tcp) => return Ok(tcp),
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    failure = (ErrorCode::ConnectRefused, error.to_string());
                }
                Err(error) => failure = (ErrorCode::ConnectFailed, error.to_string()),
            }
        }

        Err(failure)
    };

    match timeout(limit, attempt).await {
        Ok(result) => result,
        Err(_) => Err((
            ErrorCode::ConnectTimeout,
            format!("{target} did not answer in time"),
        )),
    }
}

async fn refuse(stream: &mut StreamHandle, code: ErrorCode, message: String) {
    debug!(?code, %message, "stream refused");
    let _ = write_frame(stream, &OpenResponse::refused(code, message)).await;
    let _ = stream.shutdown().await;
}

async fn relay(mut stream: StreamHandle, cfg: Arc<Config>, draining: bool) {
    let header = timeout(
        cfg.tuning.header_timeout,
        read_frame::<_, OpenRequest>(&mut stream),
    );
    let request = match header.await {
        Ok(Ok(request)) => request,
        Ok(Err(error)) => {
            return refuse(&mut stream, ErrorCode::BadRequest, error.to_string()).await;
        }
        Err(_) => {
            return refuse(
                &mut stream,
                ErrorCode::BadRequest,
                "no header in time".into(),
            )
            .await;
        }
    };

    if draining {
        return refuse(
            &mut stream,
            ErrorCode::ShuttingDown,
            "the tunnel is restarting".into(),
        )
        .await;
    }

    let target: Target = match request.target.parse() {
        Ok(target) => target,
        Err(error) => return refuse(&mut stream, ErrorCode::BadRequest, error.to_string()).await,
    };

    if !cfg.allow.permits(&target) {
        warn!(%target, "target not in the allow list");
        return refuse(
            &mut stream,
            ErrorCode::NotAllowed,
            format!("{target} is not allowed"),
        )
        .await;
    }

    let mut tcp = match connect(&target, cfg.tuning.connect_timeout).await {
        Ok(tcp) => tcp,
        Err((code, message)) => {
            warn!(%target, ?code, "target unreachable");
            return refuse(&mut stream, code, message).await;
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
    match copy_bidirectional(&mut stream, &mut tcp).await {
        Ok((sent, received)) => debug!(%target, sent, received, "stream closed"),
        Err(error) => debug!(%target, %error, "stream failed"),
    }
}

/// Serves the streams the server opens, until either side ends.
pub async fn serve(
    ws: WebSocketStream<BoxIo>,
    cfg: Arc<Config>,
    stop: CancellationToken,
) -> SessionEnd {
    let (io, close_code) = WsIo::new(ws);
    let mut session = Session::new_server(io, yamux(&cfg));
    let mut streams = JoinSet::new();
    let mut drain_until: Option<Instant> = None;
    let closed = || match close_code.load(Ordering::Relaxed) {
        0 => None,
        code => Some(code),
    };

    loop {
        let draining = drain_until.is_some();
        let deadline = drain_until.unwrap_or_else(Instant::now);

        tokio::select! {
            next = session.next() => match next {
                Some(Ok(stream)) => {
                    streams.spawn(relay(stream, cfg.clone(), draining));
                }
                Some(Err(error)) => return match closed() {
                    Some(code) => SessionEnd::Closed(Some(code)),
                    None => SessionEnd::Failed(error),
                },
                None => return SessionEnd::Closed(closed()),
            },
            Some(_) = streams.join_next(), if !streams.is_empty() => {}
            _ = stop.cancelled(), if !draining => {
                info!(open = streams.len(), "draining the open streams");
                drain_until = Some(Instant::now() + cfg.tuning.drain);
            }
            _ = sleep_until(deadline), if draining => break,
        }

        if drain_until.is_some() && streams.is_empty() {
            break;
        }
    }

    // Tell the server to stop opening streams, then flush
    let _ = poll_fn(|cx| Poll::Ready(session.shutdown(cx))).await;
    let _ = timeout(Duration::from_secs(1), session.next()).await;

    SessionEnd::Stopped
}
