use std::future::{Future, poll_fn};
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use futures_util::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Instant, Sleep, sleep, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{Error, Message};

/// Largest binary message this side sends.
pub const MAX_CHUNK: usize = 64 * 1024;

/// When to ping, and how long silence may last.
#[derive(Debug, Clone, Copy)]
pub struct Keepalive {
    pub every: Duration,
    pub silence: Duration,
}

impl Default for Keepalive {
    fn default() -> Self {
        Self {
            every: Duration::from_secs(20),
            silence: Duration::from_secs(60),
        }
    }
}

struct Inner<S> {
    ws: WebSocketStream<S>,
    incoming: Bytes,
    outgoing: BytesMut,
    close_code: Option<u16>,
    ended: bool,
    keepalive: Keepalive,
    timer: Pin<Box<Sleep>>,
    heard: Instant,
    ping_due: bool,
}

/// A WebSocket seen as one stream of bytes.
pub struct WsIo<S> {
    inner: Arc<Mutex<Inner<S>>>,
}

/// Reads how the peer closed, even after the stream.
pub struct Link<S> {
    inner: Arc<Mutex<Inner<S>>>,
}

fn failed(error: Error) -> io::Error {
    match error {
        Error::Io(error) => error,
        Error::ConnectionClosed | Error::AlreadyClosed => io::ErrorKind::BrokenPipe.into(),
        other => io::Error::other(other),
    }
}

fn locked<S>(inner: &Mutex<Inner<S>>) -> MutexGuard<'_, Inner<S>> {
    inner.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<S> WsIo<S> {
    pub fn new(ws: WebSocketStream<S>, keepalive: Keepalive) -> (Self, Link<S>) {
        let inner = Arc::new(Mutex::new(Inner {
            ws,
            incoming: Bytes::new(),
            outgoing: BytesMut::new(),
            close_code: None,
            ended: false,
            keepalive,
            timer: Box::pin(sleep(keepalive.every)),
            heard: Instant::now(),
            ping_due: false,
        }));

        (
            Self {
                inner: inner.clone(),
            },
            Link { inner },
        )
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Inner<S> {
    /// Pings when due, fails after too long a silence.
    fn poll_alive(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        while self.timer.as_mut().poll(cx).is_ready() {
            if self.heard.elapsed() >= self.keepalive.silence {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the server went silent",
                ));
            }
            self.ping_due = true;
            let next = Instant::now() + self.keepalive.every;
            self.timer.as_mut().reset(next);
        }

        if self.ping_due
            && let Poll::Ready(ready) = Pin::new(&mut self.ws).poll_ready(cx)
        {
            ready.map_err(failed)?;
            Pin::new(&mut self.ws)
                .start_send(Message::Ping(Bytes::new()))
                .map_err(failed)?;
            self.ping_due = false;
            if let Poll::Ready(Err(error)) = Pin::new(&mut self.ws).poll_flush(cx) {
                return Err(failed(error));
            }
        }

        Ok(())
    }

    /// The next binary payload, none at the end.
    fn poll_message(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Option<Bytes>>> {
        loop {
            if self.ended {
                return Poll::Ready(Ok(None));
            }

            let Poll::Ready(message) = Pin::new(&mut self.ws).poll_next(cx) else {
                self.poll_alive(cx)?;
                return Poll::Pending;
            };
            self.heard = Instant::now();

            match message {
                Some(Ok(Message::Binary(bytes))) => return Poll::Ready(Ok(Some(bytes))),
                // tungstenite answers the pings itself
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Text(_))) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "text message on a binary tunnel",
                    )));
                }
                Some(Ok(Message::Close(frame))) => {
                    self.close_code = Some(frame.map_or(1005, |frame| u16::from(frame.code)));
                    self.ended = true;
                }
                Some(Err(Error::ConnectionClosed | Error::AlreadyClosed)) | None => {
                    self.ended = true;
                }
                Some(Err(error)) => return Poll::Ready(Err(failed(error))),
            }
        }
    }

    /// Hands the buffered bytes to the WebSocket as one message.
    fn poll_send(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.outgoing.is_empty() {
            return Poll::Ready(Ok(()));
        }
        ready!(Pin::new(&mut self.ws).poll_ready(cx)).map_err(failed)?;

        let message = Message::Binary(self.outgoing.split().freeze());
        Pin::new(&mut self.ws).start_send(message).map_err(failed)?;

        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for WsIo<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut inner = locked(&self.inner);

        while inner.incoming.is_empty() {
            match ready!(inner.poll_message(cx))? {
                Some(bytes) => inner.incoming = bytes,
                None => return Poll::Ready(Ok(())),
            }
        }

        let length = inner.incoming.len().min(buf.remaining());
        buf.put_slice(&inner.incoming[..length]);
        inner.incoming.advance(length);

        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WsIo<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut inner = locked(&self.inner);

        if inner.outgoing.len() >= MAX_CHUNK {
            ready!(inner.poll_send(cx))?;
        }

        let length = buf.len().min(MAX_CHUNK - inner.outgoing.len());
        inner.outgoing.extend_from_slice(&buf[..length]);

        Poll::Ready(Ok(length))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = locked(&self.inner);

        // Also here: a blocked writer may never read
        inner.poll_alive(cx)?;
        ready!(inner.poll_send(cx))?;
        Pin::new(&mut inner.ws).poll_flush(cx).map_err(failed)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = locked(&self.inner);

        ready!(inner.poll_send(cx))?;
        match ready!(Pin::new(&mut inner.ws).poll_close(cx)) {
            Ok(()) | Err(Error::ConnectionClosed | Error::AlreadyClosed) => Poll::Ready(Ok(())),
            Err(error) => Poll::Ready(Err(failed(error))),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Link<S> {
    /// The close code of the peer, if it closed.
    pub fn close_code(&self) -> Option<u16> {
        locked(&self.inner).close_code
    }

    /// Reads on until the peer closes, for `wait` at most.
    pub async fn closed(&self, wait: Duration) -> Option<u16> {
        let reading = poll_fn(|cx| {
            let mut inner = locked(&self.inner);

            loop {
                match ready!(inner.poll_message(cx)) {
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => return Poll::Ready(()),
                }
            }
        });
        let _ = timeout(wait, reading).await;

        self.close_code()
    }
}

#[cfg(test)]
mod tests {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
    use tokio_tungstenite::tungstenite::protocol::Role;
    use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    use super::*;

    type Peer = WebSocketStream<DuplexStream>;

    async fn pair_with(keepalive: Keepalive) -> (WsIo<DuplexStream>, Link<DuplexStream>, Peer) {
        let (near, far) = duplex(256 * 1024);
        let near = WebSocketStream::from_raw_socket(near, Role::Client, None).await;
        let far = WebSocketStream::from_raw_socket(far, Role::Server, None).await;
        let (io, link) = WsIo::new(near, keepalive);

        (io, link, far)
    }

    async fn pair() -> (WsIo<DuplexStream>, Peer) {
        let (io, _, peer) = pair_with(Keepalive::default()).await;

        (io, peer)
    }

    #[tokio::test]
    async fn reads_the_binary_messages_as_one_stream() {
        let (mut io, mut peer) = pair().await;

        peer.send(Message::Binary(Bytes::from_static(b"hel")))
            .await
            .unwrap();
        peer.send(Message::Ping(Bytes::from_static(b"?")))
            .await
            .unwrap();
        peer.send(Message::Binary(Bytes::from_static(b"lo wor")))
            .await
            .unwrap();
        peer.send(Message::Binary(Bytes::from_static(b"ld")))
            .await
            .unwrap();

        let mut read = [0; 11];
        io.read_exact(&mut read).await.unwrap();

        assert_eq!(&read, b"hello world");
    }

    #[tokio::test]
    async fn writes_binary_messages_no_longer_than_the_chunk() {
        let (mut io, mut peer) = pair().await;
        let sent = vec![7; MAX_CHUNK + 10];

        io.write_all(&sent).await.unwrap();
        io.flush().await.unwrap();

        let mut received = Vec::new();
        while received.len() < sent.len() {
            match peer.next().await.unwrap().unwrap() {
                Message::Binary(bytes) => {
                    assert!(bytes.len() <= MAX_CHUNK);
                    received.extend_from_slice(&bytes);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(received, sent);
    }

    #[tokio::test]
    async fn sends_the_small_writes_before_a_flush_as_one_message() {
        let (mut io, mut peer) = pair().await;

        io.write_all(b"header").await.unwrap();
        io.write_all(b" and body").await.unwrap();
        io.flush().await.unwrap();

        match peer.next().await.unwrap().unwrap() {
            Message::Binary(bytes) => assert_eq!(&bytes[..], b"header and body"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn ends_at_a_close_and_keeps_its_code() {
        let (mut io, link, mut peer) = pair_with(Keepalive::default()).await;
        let frame = CloseFrame {
            code: CloseCode::from(4001),
            reason: "replaced".into(),
        };

        peer.send(Message::Close(Some(frame))).await.unwrap();

        assert_eq!(io.read(&mut [0; 8]).await.unwrap(), 0);
        assert_eq!(link.close_code(), Some(4001));
    }

    #[tokio::test]
    async fn finds_the_close_code_after_the_stream_is_gone() {
        let (io, link, mut peer) = pair_with(Keepalive::default()).await;
        let frame = CloseFrame {
            code: CloseCode::from(4001),
            reason: "replaced".into(),
        };

        drop(io);
        peer.send(Message::Binary(Bytes::from_static(b"left over")))
            .await
            .unwrap();
        peer.send(Message::Close(Some(frame))).await.unwrap();

        assert_eq!(link.closed(Duration::from_secs(5)).await, Some(4001));
    }

    #[tokio::test]
    async fn refuses_a_text_message() {
        let (mut io, mut peer) = pair().await;

        peer.send(Message::text("hello")).await.unwrap();

        let error = io.read(&mut [0; 8]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test(start_paused = true)]
    async fn pings_a_quiet_peer_and_gives_up_on_a_silent_one() {
        let keepalive = Keepalive {
            every: Duration::from_secs(20),
            silence: Duration::from_secs(60),
        };
        let (mut io, _link, mut peer) = pair_with(keepalive).await;
        let reading = tokio::spawn(async move { io.read(&mut [0; 8]).await });

        // The peer is there: its pong counts
        match timeout(Duration::from_secs(25), peer.next()).await {
            Ok(Some(Ok(Message::Ping(_)))) => {}
            other => panic!("no ping: {other:?}"),
        }
        peer.flush().await.unwrap();

        // Then it stops reading and answering
        let error = timeout(Duration::from_secs(120), reading)
            .await
            .expect("an end in time")
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
