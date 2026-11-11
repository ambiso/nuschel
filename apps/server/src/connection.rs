// SPDX-License-Identifier: EUPL-1.2
//! One TCP connection: admission (bans, autoban, per-address and pending caps),
//! TLS within the handshake timeout, then two phases:
//!
//! - before authentication only `Version`, `Authenticate` and `Ping` are
//!   accepted, with small size limits; anything else closes the connection;
//! - after it, every frame is size-checked by type from its header before the
//!   body is buffered, then handed to [`State::handle`](crate::state::State).
//!
//! A writer task drains the connection's byte-bounded queue ([`Link`]).
use crate::{
    auth,
    config::{Security, Settings},
    extension::Negotiation,
    routing::Datagrams,
    server::Server,
    state::{Link, Peer},
};
use bytes::{Bytes, BytesMut};
use nuschel_protocol::{ControlFrame, control as proto, decode_control_limited, kind, relay};
use prost::Message as _;
use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, mpsc},
    time::{Instant, timeout, timeout_at},
};

/// IPv4-mapped IPv6 addresses compare as IPv4, so TCP and UDP agree.
pub fn canonical(address: SocketAddr) -> SocketAddr {
    SocketAddr::new(address.ip().to_canonical(), address.port())
}

/// Body limits before authentication; other types close the connection.
fn pre_authentication_limit(kind: u16) -> Option<usize> {
    match kind {
        kind::VERSION => Some(4 * 1024),
        kind::AUTHENTICATE => Some(16 * 1024),
        kind::PING => Some(256),
        _ => None,
    }
}

/// Body limits after authentication, by type, before the body is buffered.
/// Element counts are checked again after decoding.
pub fn body_limit(settings: &Settings, kind: u16) -> Option<usize> {
    let config = &settings.config;
    let unlimited = nuschel_protocol::MAX_CONTROL_BYTES;
    let image = match config.image_message_length.max(config.text_message_length) {
        0 => unlimited,
        length => (length as usize).saturating_mul(4),
    };
    Some(
        match kind {
            // UTF-16 limits: a code unit is at most three UTF-8 bytes; four is safe.
            kind::TEXT_MESSAGE | kind::CHANNEL_STATE => image.saturating_add(16 * 1024),
            kind::USER_STATE => image.saturating_add(64 * 1024),
            kind::ACL => 512 * 1024,
            kind::BAN_LIST => 4 * 1024 * 1024,
            kind::USER_LIST => 512 * 1024,
            kind::VOICE_TARGET => 8 * 1024,
            kind::PLUGIN_DATA_TRANSMISSION => 2 * 1024,
            kind::UDP_TUNNEL => relay::MAX_PLAIN + nuschel_protocol::crypt::HEADER,
            kind::QUERY_USERS => 64 * 1024,
            kind::REQUEST_BLOB | kind::PING | kind::CRYPT_SETUP | kind::PERMISSION_QUERY => {
                4 * 1024
            }
            // The extension's own caps (docs/extension.md §2), whether or not
            // this connection negotiated it.
            extension if kind::NUSCHEL_EXTENSION.contains(&extension) => {
                nuschel_protocol::nuschel::client_limit(extension)
            }
            _ => 16 * 1024,
        }
        .min(unlimited),
    )
}

/// Read one frame, applying `limit` from the header before buffering the body.
/// `Ok(None)` on a clean end of stream.
async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin),
    buffer: &mut BytesMut,
    limit: impl Fn(u16) -> Option<usize>,
) -> io::Result<Option<ControlFrame>> {
    loop {
        match decode_control_limited(buffer, &limit) {
            Ok(Some(frame)) => return Ok(Some(frame)),
            Ok(None) => {}
            Err(error) => return Err(io::Error::other(error)),
        }
        buffer.reserve(16 * 1024);
        if reader.read_buf(buffer).await? == 0 {
            return Ok(None);
        }
    }
}

/// Accept connections; transient errors (descriptor exhaustion) back off
/// instead of spinning.
pub async fn accept(server: Arc<Server>, listener: TcpListener) {
    let mut backoff = Duration::from_millis(10);
    loop {
        match listener.accept().await {
            Ok((stream, address)) => {
                backoff = Duration::from_millis(10);
                let address = canonical(address);
                if !server.admit(address.ip()) {
                    continue;
                }
                let Ok(permit) = server.pending.clone().try_acquire_owned() else {
                    server.release(address.ip());
                    continue;
                };
                server.counters.connections.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(serve(server.clone(), stream, address, permit));
            }
            Err(error) => {
                tracing::warn!(%error, "accept failed");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(1));
            }
        }
    }
}

/// Releases the connection's per-address slot however the task ends.
struct Slot(Arc<Server>, std::net::IpAddr);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.release(self.1);
    }
}

async fn write_loop(
    mut writer: impl AsyncWrite + Unpin,
    mut frames: mpsc::UnboundedReceiver<Bytes>,
    queued: Arc<AtomicUsize>,
) {
    while let Some(frame) = frames.recv().await {
        let mut written = frame.len();
        if writer.write_all(&frame).await.is_err() {
            return;
        }
        while written < 256 * 1024 {
            let Ok(frame) = frames.try_recv() else { break };
            written += frame.len();
            if writer.write_all(&frame).await.is_err() {
                return;
            }
        }
        queued.fetch_sub(written, Ordering::Relaxed);
        if writer.flush().await.is_err() {
            return;
        }
    }
    let _ = writer.shutdown().await;
}

async fn serve(
    server: Arc<Server>,
    stream: TcpStream,
    address: SocketAddr,
    permit: OwnedSemaphorePermit,
) {
    let _slot = Slot(server.clone(), address.ip());
    let config = &server.settings.config;
    let started = Instant::now();
    let handshake = Duration::from_secs(config.handshake_timeout);
    let _ = stream.set_nodelay(true);
    let Ok(Ok(tls)) = timeout_at(started + handshake, server.acceptor.accept(stream)).await else {
        return;
    };
    let (_, connection) = tls.get_ref();
    let certificates: Vec<Vec<u8>> = connection
        .peer_certificates()
        .map(|chain| chain.iter().take(8).map(|der| der.to_vec()).collect())
        .unwrap_or_default();
    let negotiation = Negotiation::from_alpn(connection.alpn_protocol());
    let certificate_hash = certificates.first().map(|der| crate::tls::sha1_hex(der));
    if certificate_hash
        .as_deref()
        .is_some_and(|hash| server.banned_certificate(hash))
    {
        tracing::info!(address = %server.settings.addresses.socket(address), "refused a banned certificate");
        return;
    }
    let (mut reader, writer) = tokio::io::split(tls);
    let (link, frames, queued) = Link::new(config.max_queued_bytes);
    let signal = link.signal.clone();
    let writer = tokio::spawn(write_loop(writer, frames, queued));
    let stock = negotiation == Negotiation::Stock;
    if stock && config.security == Security::E2ee {
        // Stock clients show the reason; their Authenticate is never read.
        link.message(
            kind::REJECT,
            &proto::Reject {
                r#type: Some(proto::reject::RejectType::None as i32),
                reason: Some("This server requires the Nuschel client".into()),
            },
        );
        drop(link);
        let _ = timeout(Duration::from_secs(5), writer).await;
        return;
    }
    link.send(if stock {
        server.version_frame.clone()
    } else {
        server.nuschel_version_frame.clone()
    });
    let mut peer = Peer {
        address,
        certificates,
        certificate_hash,
        strong: false,
        negotiation,
        info: Default::default(),
        link,
    };
    let mut buffer = BytesMut::with_capacity(4096);
    // Authentication must complete within one and a half handshake timeouts.
    let deadline = started + handshake + handshake / 2;
    let session = loop {
        let frame = tokio::select! {
            frame = timeout_at(deadline, read_frame(&mut reader, &mut buffer, pre_authentication_limit)) => frame,
            () = signal.close.notified() => break None,
        };
        let Ok(Ok(Some(frame))) = frame else {
            break None;
        };
        match frame.kind {
            kind::VERSION => {
                if let Ok(message) = proto::Version::decode(frame.payload.clone()) {
                    peer.info.apply(&message);
                }
                if let Err(error) = peer
                    .negotiation
                    .version(&frame.payload, server.capabilities)
                {
                    tracing::info!(address = %server.settings.addresses.socket(address), %error, "closed a nuschel/1 connection");
                    break None;
                }
            }
            kind::AUTHENTICATE => {
                // ALPN `nuschel/1` without a hello fails closed, before the
                // credentials are even decoded.
                if peer.negotiation.freeze().is_err() {
                    tracing::info!(address = %server.settings.addresses.socket(address), "closed a nuschel/1 connection without a hello");
                    break None;
                }
                let Ok(message) = proto::Authenticate::decode(frame.payload) else {
                    break None;
                };
                match auth::authenticate(&server, &peer, message).await {
                    Ok(session) => {
                        server.counters.logins.fetch_add(1, Ordering::Relaxed);
                        break Some(session);
                    }
                    Err(rejection) => {
                        server.counters.rejected.fetch_add(1, Ordering::Relaxed);
                        tracing::info!(address = %server.settings.addresses.socket(address), reason = %rejection.reason, "rejected");
                        peer.link.message(
                            kind::REJECT,
                            &proto::Reject {
                                r#type: Some(rejection.kind as i32),
                                reason: Some(rejection.reason),
                            },
                        );
                        break None;
                    }
                }
            }
            // Pings before authentication are ignored, as in Murmur.
            _ => {}
        }
    };
    drop(peer);
    if let Some(session) = session {
        drop(permit);
        server.forgive(address.ip());
        authenticated(&server, session, &mut reader, &mut buffer, &signal).await;
        server.lock().disconnect(session, true);
    }
    // Let the writer flush what is queued (a Reject, a kick reason), unless the
    // client stopped reading.
    if signal.overflowed.load(Ordering::Relaxed) {
        tracing::info!(address = %server.settings.addresses.socket(address), "disconnected a client that stopped reading");
        writer.abort();
    } else if timeout(Duration::from_secs(5), writer).await.is_err() {
        tracing::debug!(address = %server.settings.addresses.socket(address), "gave up flushing to a closing connection");
    }
}

async fn authenticated(
    server: &Arc<Server>,
    session: u32,
    reader: &mut (impl AsyncRead + Unpin),
    buffer: &mut BytesMut,
    signal: &crate::state::LinkSignal,
) {
    let idle = Duration::from_secs(server.settings.config.timeout);
    let mut datagrams = Datagrams::default();
    loop {
        let frame = tokio::select! {
            frame = timeout(idle, read_frame(reader, buffer, |kind| body_limit(&server.settings, kind))) => frame,
            () = signal.close.notified() => return,
        };
        let frame = match frame {
            Ok(Ok(Some(frame))) => frame,
            Ok(Ok(None)) => return,
            Ok(Err(error)) => {
                tracing::debug!(session, %error, "connection error");
                return;
            }
            Err(_) => {
                tracing::info!(session, "timeout");
                return;
            }
        };
        {
            let mut state = server.lock();
            if !state.users.contains_key(&session) {
                return;
            }
            state.handle(session, frame, &mut datagrams);
        }
        send_datagrams(server, &mut datagrams);
    }
}

/// Send what routing produced, after the state lock is released.
pub fn send_datagrams(server: &Server, datagrams: &mut Datagrams) {
    for (socket, address, bytes) in datagrams.iter() {
        if let Some(socket) = server.udp.get(socket) {
            // Voice is dropped rather than queued when the socket is full.
            let _ = socket.try_send_to(bytes, address);
        }
    }
    datagrams.clear();
}
