use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::task::{Context, Poll, ready};

use bytes::{Buf, Bytes};
use futures_util::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{Error, Message};

/// Largest binary message this side sends.
pub const MAX_CHUNK: usize = 64 * 1024;

/// A WebSocket seen as one stream of bytes.
pub struct WsIo<S> {
    ws: WebSocketStream<S>,
    pending: Bytes,
    close_code: Arc<AtomicU16>,
    ended: bool,
}

fn failed(error: Error) -> io::Error {
    match error {
        Error::Io(error) => error,
        Error::ConnectionClosed | Error::AlreadyClosed => io::ErrorKind::BrokenPipe.into(),
        other => io::Error::other(other),
    }
}

impl<S> WsIo<S> {
    /// Also gives the peer's close code, 0 until it closes.
    pub fn new(ws: WebSocketStream<S>) -> (Self, Arc<AtomicU16>) {
        let close_code = Arc::new(AtomicU16::new(0));
        let io = Self {
            ws,
            pending: Bytes::new(),
            close_code: close_code.clone(),
            ended: false,
        };

        (io, close_code)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for WsIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if !self.pending.is_empty() {
                let length = self.pending.len().min(buf.remaining());
                buf.put_slice(&self.pending[..length]);
                self.pending.advance(length);
                return Poll::Ready(Ok(()));
            }
            if self.ended {
                return Poll::Ready(Ok(()));
            }

            match ready!(Pin::new(&mut self.ws).poll_next(cx)) {
                Some(Ok(Message::Binary(bytes))) => self.pending = bytes,
                // tungstenite answers the pings itself
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Text(_))) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "text message on a binary tunnel",
                    )));
                }
                Some(Ok(Message::Close(frame))) => {
                    let code = frame.map_or(1005, |frame| u16::from(frame.code));
                    self.close_code.store(code, Ordering::Relaxed);
                    self.ended = true;
                }
                Some(Err(Error::ConnectionClosed | Error::AlreadyClosed)) | None => {
                    self.ended = true;
                }
                Some(Err(error)) => return Poll::Ready(Err(failed(error))),
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WsIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        ready!(Pin::new(&mut self.ws).poll_ready(cx)).map_err(failed)?;

        let length = buf.len().min(MAX_CHUNK);
        let message = Message::Binary(Bytes::copy_from_slice(&buf[..length]));
        Pin::new(&mut self.ws).start_send(message).map_err(failed)?;

        Poll::Ready(Ok(length))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.ws).poll_flush(cx).map_err(failed)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match ready!(Pin::new(&mut self.ws).poll_close(cx)) {
            Ok(()) | Err(Error::ConnectionClosed | Error::AlreadyClosed) => Poll::Ready(Ok(())),
            Err(error) => Poll::Ready(Err(failed(error))),
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
    use tokio_tungstenite::tungstenite::protocol::Role;
    use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    use super::*;

    async fn pair() -> (
        WsIo<tokio::io::DuplexStream>,
        WebSocketStream<tokio::io::DuplexStream>,
    ) {
        let (near, far) = duplex(256 * 1024);
        let near = WebSocketStream::from_raw_socket(near, Role::Client, None).await;
        let far = WebSocketStream::from_raw_socket(far, Role::Server, None).await;

        (WsIo::new(near).0, far)
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
    async fn ends_at_a_close_and_keeps_its_code() {
        let (near, far) = duplex(1024);
        let near = WebSocketStream::from_raw_socket(near, Role::Client, None).await;
        let mut peer = WebSocketStream::from_raw_socket(far, Role::Server, None).await;
        let (mut io, code) = WsIo::new(near);
        let frame = CloseFrame {
            code: CloseCode::from(4001),
            reason: "replaced".into(),
        };

        peer.send(Message::Close(Some(frame))).await.unwrap();

        assert_eq!(io.read(&mut [0; 8]).await.unwrap(), 0);
        assert_eq!(code.load(Ordering::Relaxed), 4001);
    }

    #[tokio::test]
    async fn refuses_a_text_message() {
        let (mut io, mut peer) = pair().await;

        peer.send(Message::text("hello")).await.unwrap();

        let error = io.read(&mut [0; 8]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
