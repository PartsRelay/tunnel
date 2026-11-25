// The handshake callback type is the library's
#![allow(dead_code, clippy::result_large_err)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use partsrelay_tunnel::SUBPROTOCOL;
use partsrelay_tunnel::config::Config;
use partsrelay_tunnel::dial::BoxIo;
use partsrelay_tunnel::proto::{OpenRequest, OpenResponse, read_frame, write_frame};
use partsrelay_tunnel::wsio::WsIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderMap, HeaderValue, StatusCode};
use tokio_yamux::{Config as YamuxConfig, Control, Session, StreamHandle};

pub const TOKEN: &str = "prt_test_token_4f9a";
pub const WAIT: Duration = Duration::from_secs(10);

/// One tunnel connected to the fake connectors.
pub struct Tunnel {
    pub control: Control,
    pub headers: HeaderMap,
    driver: JoinHandle<()>,
}

impl Tunnel {
    /// Opens a stream and sends its header.
    pub async fn open(&mut self, target: &str) -> (StreamHandle, OpenResponse) {
        let mut stream = self.control.open_stream().await.expect("the stream opens");
        let request = OpenRequest {
            target: target.into(),
        };

        write_frame(&mut stream, &request)
            .await
            .expect("the header is sent");
        let response = timeout(WAIT, read_frame(&mut stream))
            .await
            .expect("an answer in time");

        (stream, response.expect("a valid answer"))
    }

    /// Opens a stream and sends raw bytes as its header.
    pub async fn open_raw(&mut self, bytes: &[u8]) -> OpenResponse {
        let mut stream = self.control.open_stream().await.expect("the stream opens");

        stream.write_all(bytes).await.expect("the bytes are sent");
        stream.flush().await.expect("the bytes are flushed");

        timeout(WAIT, read_frame(&mut stream))
            .await
            .expect("an answer in time")
            .expect("an answer")
    }

    /// Cuts the connection, as a server that crashes.
    pub fn cut(self) {
        self.driver.abort();
    }
}

/// The server side of the protocol, in this process.
pub struct Connectors {
    pub address: SocketAddr,
    pub attempts: Arc<AtomicUsize>,
    status: Arc<AtomicU16>,
    tunnels: mpsc::UnboundedReceiver<Tunnel>,
    scheme: &'static str,
    host: &'static str,
}

impl Connectors {
    pub async fn start() -> Self {
        Self::listen(None).await
    }

    pub async fn start_tls(acceptor: TlsAcceptor) -> Self {
        Self::listen(Some(acceptor)).await
    }

    async fn listen(tls: Option<TlsAcceptor>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let status = Arc::new(AtomicU16::new(0));
        let (sender, tunnels) = mpsc::unbounded_channel();
        let (counted, answer) = (attempts.clone(), status.clone());
        let secure = tls.is_some();

        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                let (sender, answer, tls) = (sender.clone(), answer.clone(), tls.clone());

                tokio::spawn(async move {
                    let io: BoxIo = match tls {
                        Some(acceptor) => match acceptor.accept(tcp).await {
                            Ok(tls) => Box::new(tls),
                            Err(_) => return,
                        },
                        None => Box::new(tcp),
                    };
                    let seen = Arc::new(Mutex::new(HeaderMap::new()));
                    let headers = seen.clone();
                    let callback = move |request: &Request, mut response: Response| {
                        *headers.lock().unwrap() = request.headers().clone();

                        match answer.load(Ordering::SeqCst) {
                            0 => {
                                response.headers_mut().insert(
                                    "sec-websocket-protocol",
                                    HeaderValue::from_static(SUBPROTOCOL),
                                );
                                Ok(response)
                            }
                            status => {
                                let mut refused = ErrorResponse::new(None);
                                *refused.status_mut() = StatusCode::from_u16(status).unwrap();
                                Err(refused)
                            }
                        }
                    };
                    let Ok(ws) = accept_hdr_async(io, callback).await else {
                        return;
                    };
                    let (io, _) = WsIo::new(ws);
                    let mut session = Session::new_client(io, YamuxConfig::default());
                    let control = session.control();
                    let driver =
                        tokio::spawn(
                            async move { while let Some(Ok(_)) = session.next().await {} },
                        );
                    let headers = seen.lock().unwrap().clone();

                    let _ = sender.send(Tunnel {
                        control,
                        headers,
                        driver,
                    });
                });
            }
        });

        Self {
            address,
            attempts,
            status,
            tunnels,
            scheme: if secure { "wss" } else { "ws" },
            host: if secure { "localhost" } else { "127.0.0.1" },
        }
    }

    pub fn url(&self) -> String {
        format!(
            "{}://{}:{}/v1/tunnel",
            self.scheme,
            self.host,
            self.address.port()
        )
    }

    /// Answers every new handshake with this HTTP status.
    pub fn refuse_with(&self, status: u16) {
        self.status.store(status, Ordering::SeqCst);
    }

    /// The next tunnel that connects.
    pub async fn tunnel(&mut self) -> Tunnel {
        timeout(WAIT, self.tunnels.recv())
            .await
            .expect("a tunnel in time")
            .expect("a tunnel")
    }

    /// Waits for a tunnel without a limit of its own.
    pub async fn tunnels_now(&mut self) -> Option<Tunnel> {
        self.tunnels.recv().await
    }

    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

/// A management system that sends back what it receives.
pub async fn echo() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counted = accepted.clone();

    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let (mut reader, mut writer) = tcp.split();
                let _ = tokio::io::copy(&mut reader, &mut writer).await;
                let _ = writer.shutdown().await;
            });
        }
    });

    (address, accepted)
}

/// A port nothing listens on: tcpmux.
pub fn closed_port() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 1))
}

/// Waits until `done` is true, failing after the usual limit.
pub async fn until(what: &str, done: impl Fn() -> bool) {
    let waiting = async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };

    timeout(WAIT, waiting)
        .await
        .unwrap_or_else(|_| panic!("{what} did not happen in time"));
}

/// A configuration for `server`, with short waits.
pub fn config(server: &str, allow: &[String]) -> Config {
    let allow = allow.join(",");
    let mut config = Config::from_sources(None, |name| match name {
        "PARTSRELAY_TUNNEL_SERVER" => Some(server.to_string()),
        "PARTSRELAY_TUNNEL_TOKEN" => Some(TOKEN.to_string()),
        "PARTSRELAY_TUNNEL_ALLOW" => Some(allow.clone()),
        "PARTSRELAY_TUNNEL_AUTO_UPDATE" => Some("false".to_string()),
        _ => None,
    })
    .expect("a valid configuration");

    config.tuning.backoff_min = Duration::from_millis(20);
    config.tuning.backoff_max = Duration::from_millis(100);
    config.tuning.auth_backoff_min = Duration::from_secs(30);
    config.tuning.header_timeout = Duration::from_millis(300);
    config.tuning.connect_timeout = Duration::from_secs(8);
    config.tuning.drain = Duration::from_secs(3);
    config
}

/// Reads from `stream` until `length` bytes arrived.
pub async fn read_all(stream: &mut StreamHandle, length: usize) -> Vec<u8> {
    let mut received = vec![0; length];

    timeout(WAIT, stream.read_exact(&mut received))
        .await
        .expect("bytes in time")
        .expect("bytes");
    received
}

/// An HTTP proxy that only knows CONNECT, recording each request.
pub async fn connect_proxy() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();

    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    match client.read_u8().await {
                        Ok(byte) => head.push(byte),
                        Err(_) => return,
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let authority = head
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                seen.lock().unwrap().push(head);

                let Ok(mut upstream) = TcpStream::connect(&authority).await else {
                    return;
                };
                let _ = client
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await;
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });

    (address, requests)
}
