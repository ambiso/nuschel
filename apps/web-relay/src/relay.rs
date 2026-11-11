// SPDX-License-Identifier: EUPL-1.2
//! Accepting WebSockets and forwarding each tunnel (docs/web.md, "The tunnel
//! relay"). The relay never decrypts, parses or logs what it forwards: it
//! checks only the first bytes in each direction for the shape of a TLS
//! handshake, so it cannot be used to talk to anything but TLS servers.

use crate::{
    config::{Config, Listen},
    limits::{Admission, Bucket, ClientKey, Refusal},
    policy::Policy,
};
use futures_util::{SinkExt, StreamExt};
use nuschl_protocol::tunnel::{self, CloseReason, Frame};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    time::timeout,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response},
        http::{HeaderValue, StatusCode},
        protocol::WebSocketConfig,
    },
};

/// The WebSocket upgrade and the `Open` frame must arrive within this.
const HANDSHAKE: Duration = Duration::from_secs(10);
const RESOLVE: Duration = Duration::from_secs(5);
const CONNECT: Duration = Duration::from_secs(5);
/// Each of the two pings waits this long for its answer.
const PING: Duration = Duration::from_millis(750);
/// The server's first bytes (its ServerHello) must arrive within this.
const SERVER_HELLO: Duration = Duration::from_secs(15);
/// Addresses of one name tried, at most.
const ADDRESSES: usize = 4;
/// Sockets still in their WebSocket handshake, beyond the tunnel limit.
const HANDSHAKE_HEADROOM: usize = 256;

pub struct Relay {
    config: Config,
    policy: Policy,
    admission: Admission,
    sockets: Arc<AtomicUsize>,
}

struct Socket(Arc<AtomicUsize>);
impl Drop for Socket {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Relay {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            policy: Policy::new(&config),
            admission: Admission::new(&config),
            sockets: Arc::new(AtomicUsize::new(0)),
            config,
        })
    }

    /// Listen where the configuration says, until the task is dropped.
    pub async fn run(self: Arc<Self>) -> std::io::Result<()> {
        match self.config.listen.clone() {
            Listen::Tcp(address) => self.serve_tcp(TcpListener::bind(address).await?).await,
            Listen::Unix(path) => {
                #[cfg(not(unix))]
                {
                    let _ = path;
                    Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "Unix relay sockets are not supported on this platform; use a TCP listen address",
                    ))
                }
                #[cfg(unix)]
                {
                    // A socket left by an earlier run.
                    let _ = std::fs::remove_file(&path);
                    let listener = UnixListener::bind(&path)?;
                    {
                        use std::os::unix::fs::PermissionsExt;
                        // The directory decides who may connect (the module gives it
                        // to the web server's group).
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))?;
                    }
                    self.serve_unix(listener).await
                }
            }
        }
    }

    pub async fn serve_tcp(self: Arc<Self>, listener: TcpListener) -> std::io::Result<()> {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!("accept failed: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Some(socket) = self.socket() else {
                continue;
            };
            let _ = stream.set_nodelay(true);
            let peer = peer.ip();
            let trusted = self
                .config
                .trusted_proxies
                .iter()
                .any(|network| network.contains(peer));
            let relay = self.clone();
            tokio::spawn(async move {
                relay.handle(stream, Some(peer), trusted).await;
                drop(socket);
            });
        }
    }

    #[cfg(unix)]
    pub async fn serve_unix(self: Arc<Self>, listener: UnixListener) -> std::io::Result<()> {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!("accept failed: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Some(socket) = self.socket() else {
                continue;
            };
            let relay = self.clone();
            tokio::spawn(async move {
                relay.handle(stream, None, true).await;
                drop(socket);
            });
        }
    }

    /// Open tunnels.
    pub fn tunnels(&self) -> usize {
        self.admission.open()
    }

    fn socket(&self) -> Option<Socket> {
        let limit = self.config.max_tunnels + HANDSHAKE_HEADROOM;
        let previous = self.sockets.fetch_add(1, Ordering::Relaxed);
        let socket = Socket(self.sockets.clone());
        (previous < limit).then_some(socket)
    }

    // tungstenite's handshake callback returns its HTTP error response by value.
    #[allow(clippy::result_large_err)]
    async fn handle<S>(&self, stream: S, peer: Option<IpAddr>, trusted: bool)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut forwarded = None;
        let origins = &self.config.allowed_origins;
        let callback = |request: &Request, mut response: Response| {
            let headers = request.headers();
            let offered = headers
                .get_all("Sec-WebSocket-Protocol")
                .iter()
                .filter_map(|value| value.to_str().ok())
                .flat_map(|value| value.split(','))
                .any(|protocol| protocol.trim() == tunnel::SUBPROTOCOL);
            if !offered {
                return Err(refuse(StatusCode::BAD_REQUEST, "nuschl-tunnel.1 required"));
            }
            if !origins.is_empty() {
                let origin = headers.get("Origin").and_then(|value| value.to_str().ok());
                if !origin.is_some_and(|origin| origins.iter().any(|allowed| allowed == origin)) {
                    return Err(refuse(StatusCode::FORBIDDEN, "origin not allowed"));
                }
            }
            if trusted {
                forwarded = headers
                    .get("X-Real-IP")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<IpAddr>().ok());
            }
            response.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                HeaderValue::from_static(tunnel::SUBPROTOCOL),
            );
            Ok(response)
        };
        let config = WebSocketConfig::default()
            .max_message_size(Some(tunnel::MAX_FRAME))
            .max_frame_size(Some(tunnel::MAX_FRAME))
            .max_write_buffer_size(4 * tunnel::MAX_FRAME);
        let ws = match timeout(
            HANDSHAKE,
            tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(config)),
        )
        .await
        {
            Ok(Ok(ws)) => ws,
            Ok(Err(error)) => {
                tracing::debug!("WebSocket handshake failed: {error}");
                return;
            }
            Err(_) => return,
        };
        let client = forwarded
            .or(peer)
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        self.tunnel(ws, client).await;
    }

    async fn tunnel<S>(&self, mut ws: WebSocketStream<S>, client: IpAddr)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let _ticket = match self.admission.admit(ClientKey::of(client), Instant::now()) {
            Ok(ticket) => ticket,
            Err(refusal) => {
                let reason = match refusal {
                    Refusal::RateLimited => CloseReason::RateLimited,
                    Refusal::Busy => CloseReason::Busy,
                };
                tracing::info!("tunnel refused: {reason:?}");
                close(&mut ws, reason).await;
                return;
            }
        };
        let (host, port) = match timeout(HANDSHAKE, ws.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => match tunnel::decode(&bytes) {
                Ok(Frame::Open { host, port }) => (host.to_ascii_lowercase(), port),
                _ => return close(&mut ws, CloseReason::Protocol).await,
            },
            Ok(_) => return close(&mut ws, CloseReason::Protocol).await,
            Err(_) => return close(&mut ws, CloseReason::Timeout).await,
        };
        let (stream, udp) = match self.reach(&host, port).await {
            Ok(reached) => reached,
            Err(reason) => {
                tracing::info!("tunnel to {host}:{port} refused: {reason:?}");
                return close(&mut ws, reason).await;
            }
        };
        let opened = tunnel::encode(&Frame::Opened { udp: udp.is_some() }).expect("small");
        if ws.send(Message::binary(opened)).await.is_err() {
            return;
        }
        tracing::debug!(
            "tunnel to {host}:{port} open (udp {})",
            if udp.is_some() { "yes" } else { "no" }
        );
        let (up, down) = self.forward(ws, stream, udp).await;
        tracing::debug!("tunnel to {host}:{port} closed after {up} bytes up, {down} down");
    }

    /// Resolve, check and connect: the TCP connection, and a UDP socket if
    /// the server answered a Mumble ping.
    async fn reach(
        &self,
        host: &str,
        port: u16,
    ) -> Result<(TcpStream, Option<UdpSocket>), CloseReason> {
        if !self.config.port_allowed(port) {
            return Err(CloseReason::PortRefused);
        }
        let addresses: Vec<SocketAddr> = match host.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => timeout(RESOLVE, tokio::net::lookup_host((host, port)))
                .await
                .map_err(|_| CloseReason::Unresolved)?
                .map_err(|_| CloseReason::Unresolved)?
                .take(16)
                .collect(),
        };
        if addresses.is_empty() {
            return Err(CloseReason::Unresolved);
        }
        // Connect only to checked addresses (never resolving again, so a name
        // cannot change its answer in between).
        let allowed: Vec<SocketAddr> = addresses
            .into_iter()
            .filter(|address| self.policy.allows(address.ip()))
            .take(ADDRESSES)
            .collect();
        if allowed.is_empty() {
            return Err(CloseReason::AddressRefused);
        }
        let mut last = CloseReason::Unreachable;
        for address in allowed {
            let (udp, tcp) = tokio::join!(ping(address), connect(address));
            match tcp {
                Some(tcp) if udp.is_some() || !self.config.require_ping => return Ok((tcp, udp)),
                Some(_) => last = CloseReason::NotMumble,
                None => {}
            }
        }
        Err(last)
    }

    /// Forward until either side ends; the bytes forwarded up and down.
    async fn forward<S>(
        &self,
        ws: WebSocketStream<S>,
        stream: TcpStream,
        udp: Option<UdpSocket>,
    ) -> (u64, u64)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sink, mut source) = ws.split();
        let (mut tcp_read, mut tcp_write) = stream.into_split();
        let udp = udp.as_ref();
        let validated = AtomicBool::new(false);
        let start = Instant::now();
        let activity = AtomicU64::new(0);
        let up_bytes = AtomicU64::new(0);
        let down_bytes = AtomicU64::new(0);
        let touch = || activity.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
        let (close_tx, mut close_rx) = tokio::sync::oneshot::channel::<CloseReason>();
        let rate = self.config.bytes_per_second;
        let burst = self.config.burst_bytes;

        // Client → server.
        let upstream = async {
            let mut bucket = Bucket::new(rate, burst);
            let mut hello: Option<Vec<u8>> = Some(Vec::new());
            let reason = loop {
                let bytes = match source.next().await {
                    Some(Ok(Message::Binary(bytes))) => bytes,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                        break CloseReason::Ended;
                    }
                    Some(Ok(_)) => break CloseReason::Protocol,
                };
                match tunnel::decode(&bytes) {
                    Ok(Frame::Stream(data)) => {
                        bucket.spend(data.len()).await;
                        touch();
                        up_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                        let data = match hello.as_mut() {
                            None => data,
                            Some(first) => {
                                first.extend_from_slice(data);
                                match tunnel::starts_like_client_hello(first) {
                                    None => continue,
                                    Some(false) => break CloseReason::Protocol,
                                    Some(true) => {}
                                }
                                let first = hello.take().unwrap_or_default();
                                if tcp_write.write_all(&first).await.is_err() {
                                    break CloseReason::Ended;
                                }
                                continue;
                            }
                        };
                        if tcp_write.write_all(data).await.is_err() {
                            break CloseReason::Ended;
                        }
                    }
                    Ok(Frame::Datagram(data)) => {
                        // Only once the server proved to speak TLS.
                        if let Some(udp) = udp
                            && validated.load(Ordering::Relaxed)
                        {
                            bucket.spend(data.len()).await;
                            touch();
                            up_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                            let _ = udp.send(data).await;
                        }
                    }
                    _ => break CloseReason::Protocol,
                }
            };
            // The server sees the end; a dropped write half would do the same.
            let _ = timeout(Duration::from_secs(1), tcp_write.shutdown()).await;
            reason
        };

        // Server → client.
        let downstream = async {
            let mut bucket = Bucket::new(rate, burst);
            let mut buffer = vec![0_u8; 16 * 1024];
            let mut datagram = [0_u8; tunnel::MAX_DATAGRAM + 1];
            let mut first: Option<Vec<u8>> = Some(Vec::new());
            let hello_deadline = tokio::time::Instant::now() + SERVER_HELLO;
            let reason = loop {
                let checked = first.is_none();
                tokio::select! {
                    reason = &mut close_rx => break reason.unwrap_or(CloseReason::Ended),
                    _ = tokio::time::sleep_until(hello_deadline), if !checked => {
                        break CloseReason::Timeout;
                    }
                    read = tcp_read.read(&mut buffer) => {
                        let size = match read {
                            Ok(0) | Err(_) => break CloseReason::Ended,
                            Ok(size) => size,
                        };
                        bucket.spend(size).await;
                        touch();
                        down_bytes.fetch_add(size as u64, Ordering::Relaxed);
                        let data = match first.as_mut() {
                            None => &buffer[..size],
                            Some(start) => {
                                start.extend_from_slice(&buffer[..size]);
                                match tunnel::starts_like_server_hello(start) {
                                    None => continue,
                                    Some(false) => break CloseReason::NotMumble,
                                    Some(true) => {}
                                }
                                validated.store(true, Ordering::Relaxed);
                                let start = first.take().unwrap_or_default();
                                if send_stream(&mut sink, &start).await.is_err() {
                                    return;
                                }
                                continue;
                            }
                        };
                        if send_stream(&mut sink, data).await.is_err() {
                            return;
                        }
                    }
                    received = receive(udp, &mut datagram), if checked => {
                        let Ok(size) = received else { continue };
                        if !(1..=tunnel::MAX_DATAGRAM).contains(&size) {
                            continue;
                        }
                        bucket.spend(size).await;
                        touch();
                        down_bytes.fetch_add(size as u64, Ordering::Relaxed);
                        let frame = tunnel::encode(&Frame::Datagram(&datagram[..size])).expect("bounded");
                        if sink.send(Message::binary(frame)).await.is_err() {
                            return;
                        }
                    }
                }
            };
            let frame = tunnel::encode(&Frame::Closed(reason)).expect("small");
            let _ = timeout(Duration::from_secs(2), async {
                let _ = sink.send(Message::binary(frame)).await;
                let _ = sink.close().await;
            })
            .await;
        };

        let idle = Duration::from_secs(self.config.idle_timeout_seconds);
        let watchdog = async {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let last = Duration::from_millis(activity.load(Ordering::Relaxed));
                if start.elapsed().saturating_sub(last) > idle {
                    return;
                }
            }
        };
        tokio::pin!(upstream, downstream, watchdog);
        let reason = tokio::select! {
            reason = &mut upstream => Some(reason),
            () = &mut downstream => None,
            () = &mut watchdog => Some(CloseReason::Timeout),
        };
        if let Some(reason) = reason {
            let _ = close_tx.send(reason);
            let _ = timeout(Duration::from_secs(3), &mut downstream).await;
        }
        (
            up_bytes.load(Ordering::Relaxed),
            down_bytes.load(Ordering::Relaxed),
        )
    }
}

fn refuse(status: StatusCode, text: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(text.to_owned()));
    *response.status_mut() = status;
    response
}

async fn close<S>(ws: &mut WebSocketStream<S>, reason: CloseReason)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let frame = tunnel::encode(&Frame::Closed(reason)).expect("small");
    let _ = timeout(Duration::from_secs(2), async {
        let _ = ws.send(Message::binary(frame)).await;
        let _ = ws.close(None).await;
    })
    .await;
}

async fn send_stream<S>(
    sink: &mut futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    data: &[u8],
) -> Result<(), ()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    for chunk in data.chunks(tunnel::MAX_STREAM) {
        let frame = tunnel::encode(&Frame::Stream(chunk)).ok_or(())?;
        sink.send(Message::binary(frame)).await.map_err(|_| ())?;
    }
    Ok(())
}

async fn receive(udp: Option<&UdpSocket>, buffer: &mut [u8]) -> std::io::Result<usize> {
    match udp {
        Some(udp) => udp.recv(buffer).await,
        None => std::future::pending().await,
    }
}

async fn connect(address: SocketAddr) -> Option<TcpStream> {
    let stream = timeout(CONNECT, TcpStream::connect(address))
        .await
        .ok()?
        .ok()?;
    let _ = stream.set_nodelay(true);
    Some(stream)
}

/// A Mumble server answers the unencrypted 12-byte ping with 24 bytes echoing
/// our value; the socket then carries the tunnel's datagrams.
async fn ping(address: SocketAddr) -> Option<UdpSocket> {
    let local: SocketAddr = if address.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let socket = UdpSocket::bind(local).await.ok()?;
    socket.connect(address).await.ok()?;
    let ident: u64 = rand::random();
    let mut reply = [0_u8; 64];
    for _ in 0..2 {
        socket.send(&tunnel::mumble_ping(ident)).await.ok()?;
        let deadline = tokio::time::Instant::now() + PING;
        while let Ok(Ok(size)) = tokio::time::timeout_at(deadline, socket.recv(&mut reply)).await {
            if tunnel::is_ping_reply(&reply[..size], ident) {
                return Some(socket);
            }
        }
    }
    None
}
