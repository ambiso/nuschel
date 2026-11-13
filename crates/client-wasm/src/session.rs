// SPDX-License-Identifier: EUPL-1.2
//! A Mumble connection through the web relay's tunnel, without I/O
//! (docs/web.md, "The web client's network path"): WebSocket messages in,
//! WebSocket messages and events out. TLS runs here (rustls), so the relay
//! sees only ciphertext, and the server's certificate is checked here: against
//! the saved pin, or the web's certificate authorities, as the apps do.
//!
//! The browser glue (`browser.rs`) moves bytes; tests drive this directly.
//! Legacy text, presence and Opus over TLS UDPTunnel are supported. Modern
//! server archives and MLS are deliberately not negotiated by this adapter.

use bytes::BytesMut;
use nuschl_client::input::ServerEvent;
use nuschl_core::{
    MAX_CONTENT_BYTES,
    model::{Channel, TextTarget, User},
};
use nuschl_protocol::{
    control as proto, decode_control, encode_control, kind,
    tunnel::{self, CloseReason, Frame},
};
use prost::Message as _;
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{Read, Write},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

/// Where to log in.
#[derive(Clone, Debug)]
pub struct Login {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub tokens: Vec<String>,
    /// The server certificate's SHA-256 (hex); without one, a certificate
    /// authority must vouch for the server.
    pub pin: Option<String>,
}

/// What happened, for the platform to report.
#[derive(Clone, Debug)]
pub enum Event {
    /// The relay reached the server (`udp`: datagrams can go over UDP).
    Opened { udp: bool },
    /// What the SDK models.
    Server(ServerEvent),
    /// Bounded legacy welcome markup; rendered only in the content worker.
    Welcome(String),
    /// The certificate is neither pinned nor vouched for: the user decides.
    Certificate { sha256: String, changed: bool },
    /// Bounded original text. The platform persists it before SDK admission;
    /// credential-free rendering follows. The relay never sees plaintext.
    Text {
        target: TextTarget,
        /// Original wire channel lifetime, captured before later queued states.
        channel_instance: Option<u64>,
        peer: Option<User>,
        author: String,
        body: String,
        html: bool,
        outgoing: bool,
        /// Last complete TLS tunnel-record boundary for a local send.
        transport: Option<u64>,
    },
    /// Bounded encoded Opus; decoding belongs to the separate audio worker.
    Voice(nuschl_protocol::voice::VoicePacket),
    Sent {
        target: TextTarget,
        body: String,
        transport: u64,
    },
    /// The connection is over.
    Closed { reason: String },
}

#[cfg(any(test, all(target_arch = "wasm32", target_os = "unknown")))]
impl Event {
    pub(crate) fn transport_admitted(&self, boundary: u64) -> bool {
        match self {
            Self::Sent { transport, .. } => *transport <= boundary,
            Self::Text {
                outgoing: true,
                transport,
                ..
            } => transport.is_some_and(|transport| transport <= boundary),
            _ => true,
        }
    }
}

/// Mumble's server drops a client that is silent for 30 seconds.
const PING_MS: f64 = 10_000.0;
/// Plaintext waiting to be framed; a server sending more is cut off.
const MAX_BUFFER: usize = 16 * 1024 * 1024;
pub(crate) const MAX_OUTGOING_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTGOING_FRAMES: usize = 512;

/// One bounded browser transport batch. Returns the last actually enqueued
/// record even on a later partial failure, so independent peer events and
/// earlier complete local frames retain their own admission semantics.
#[cfg(any(test, all(target_arch = "wasm32", target_os = "unknown")))]
pub(crate) fn enqueue_outgoing(
    before: u64,
    frames: &[Vec<u8>],
    buffered: usize,
    mut send: impl FnMut(&[u8]) -> Result<(), ()>,
) -> (u64, Option<&'static str>) {
    let total = frames
        .iter()
        .try_fold(buffered, |total, frame| total.checked_add(frame.len()));
    if frames.len() > MAX_OUTGOING_FRAMES
        || total.is_none_or(|total| total > MAX_OUTGOING_BYTES)
        || before.checked_add(frames.len() as u64).is_none()
    {
        return (
            before,
            Some(
                "The browser transport is full. The connection closed; keep your draft and reconnect.",
            ),
        );
    }
    let mut admitted = before;
    for frame in frames {
        if send(frame).is_err() {
            return (
                admitted,
                Some(
                    "The browser transport could not enqueue this message. The connection closed; keep your draft and reconnect.",
                ),
            );
        }
        admitted += 1; // entire batch's checked bound above excludes wrap
    }
    (admitted, None)
}

/// At most one tunnel-sized allocation, independent of TLS advertised output.
struct TlsChunk {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for TlsChunk {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let count = bytes.len().min(self.limit.saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const MAX_EVENTS: usize = 512;
// Stock Murmur shares a burst-5, sustained-1/s bucket across self-state,
// Version and text. Rejected controls are silently discarded, so retry the
// latest intent without ever treating a timeout as an acknowledgement.
const SELF_RETRY_MS: f64 = 1100.0;
const SELF_RETRIES: u8 = 5;
struct SelfRetry {
    after: f64,
    remaining: u8,
}

fn next_channel_instance() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn next_user_instance() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug)]
struct Verifier {
    pin: Option<String>,
    seen: Mutex<Option<(String, bool)>>,
    webpki: Arc<WebPkiServerVerifier>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = hex(&Sha256::digest(end_entity.as_ref()));
        let result = match &self.pin {
            Some(pin) if pin.eq_ignore_ascii_case(&actual) => Ok(ServerCertVerified::assertion()),
            Some(_) => Err(rustls::Error::General(
                "the pinned server certificate has changed".into(),
            )),
            None => self
                .webpki
                .verify_server_cert(end_entity, intermediates, name, ocsp, now),
        };
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some((actual, result.is_err()));
        result
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for the relay's `Opened`.
    Opening,
    /// TLS and login.
    LoggingIn,
    LoggedIn,
    Closed,
}

pub struct Session {
    login: Login,
    phase: Phase,
    tls: rustls::ClientConnection,
    verifier: Arc<Verifier>,
    plaintext: BytesMut,
    outgoing: Vec<Vec<u8>>,
    outgoing_bytes: usize,
    transport_queued: u64,
    transport_taken: u64,
    events: VecDeque<Event>,
    channels: BTreeMap<u32, Channel>,
    users: BTreeMap<u32, User>,
    own: Option<u32>,
    welcome_source: Option<String>,
    oversized_text_reported: bool,
    last_ping: f64,
    certificate_reported: bool,
    limits: nuschl_core::model::ServerLimits,
    voice_version: u64,
    opus_allowed: bool,
    desired_muted: bool,
    desired_deafened: bool,
    self_change_pending: bool,
    now_ms: f64,
    self_retry: Option<SelfRetry>,
}

impl Session {
    fn welcome(&mut self, source: String) {
        if self.phase != Phase::LoggedIn {
            return;
        }
        let source = if source.len() > MAX_CONTENT_BYTES {
            "[Server welcome exceeds the display limit]".into()
        } else {
            source
        };
        if self.welcome_source.as_deref() == Some(source.as_str()) {
            return;
        }
        self.welcome_source = Some(source.clone());
        self.events.push_back(Event::Welcome(source));
    }

    /// A session whose first message (the tunnel's `Open`) is waiting in
    /// [`Session::take_outgoing`].
    pub fn new(login: Login) -> Result<Self, String> {
        Self::with_roots(login, rustls::RootCertStore::from_iter(webpki_roots()))
    }

    // Private fixture injection only. No root-store override is exported to JS.
    fn with_roots(login: Login, roots: rustls::RootCertStore) -> Result<Self, String> {
        let open = tunnel::encode(&Frame::Open {
            host: &login.host,
            port: login.port,
        })
        .ok_or("the server's address is not valid")?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .map_err(|error| error.to_string())?;
        let verifier = Arc::new(Verifier {
            pin: login.pin.clone(),
            seen: Mutex::new(None),
            webpki,
            provider: provider.clone(),
        });
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|error| error.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(verifier.clone())
            .with_no_client_auth();
        // No resumption (docs/extension.md §1, rule 4).
        config.resumption = rustls::client::Resumption::disabled();
        let name = ServerName::try_from(login.host.clone())
            .map_err(|_| "the server's address is not valid")?;
        let tls = rustls::ClientConnection::new(Arc::new(config), name)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            login,
            phase: Phase::Opening,
            tls,
            verifier,
            plaintext: BytesMut::new(),
            outgoing_bytes: open.len(),
            transport_queued: 1,
            transport_taken: 0,
            outgoing: vec![open],
            events: VecDeque::new(),
            channels: BTreeMap::new(),
            users: BTreeMap::new(),
            own: None,
            welcome_source: None,
            oversized_text_reported: false,
            last_ping: 0.0,
            certificate_reported: false,
            limits: nuschl_core::model::ServerLimits::default(),
            voice_version: 0,
            opus_allowed: true,
            desired_muted: true,
            desired_deafened: true,
            self_change_pending: false,
            now_ms: 0.0,
            self_retry: None,
        })
    }

    pub fn is_closed(&self) -> bool {
        self.phase == Phase::Closed
    }

    /// WebSocket messages to send, in order.
    pub fn take_outgoing(&mut self) -> Vec<Vec<u8>> {
        self.outgoing_bytes = 0;
        let outgoing = std::mem::take(&mut self.outgoing);
        if !outgoing.is_empty() {
            self.transport_taken = self.transport_queued;
        }
        outgoing
    }

    /// Boundary preceding this exact drained batch. The legacy native helper
    /// above remains unchanged; browser admission records each actual WS write.
    pub fn take_outgoing_batch(&mut self) -> (u64, Vec<Vec<u8>>) {
        let before = self.transport_taken;
        (before, self.take_outgoing())
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        self.events.drain(..).collect()
    }

    /// One WebSocket message from the relay.
    pub fn receive(&mut self, message: &[u8]) {
        if self.phase == Phase::Closed {
            return;
        }
        match tunnel::decode(message) {
            Ok(Frame::Opened { udp }) if self.phase == Phase::Opening => {
                self.phase = Phase::LoggingIn;
                self.events.push_back(Event::Opened { udp });
                // rustls has the ClientHello ready.
                self.flush_tls();
            }
            Ok(Frame::Stream(bytes)) if self.phase != Phase::Opening => self.tls_in(bytes),
            // Voice deliberately uses TLS UDPTunnel, never relay-visible UDP.
            Ok(Frame::Datagram(_)) => {}
            Ok(Frame::Closed(reason)) => self.close(match reason {
                CloseReason::Ended => "The server closed the connection.".into(),
                other => other.text().into(),
            }),
            _ => self.close("The relay broke the tunnel protocol.".into()),
        }
    }

    /// The relay closed the WebSocket.
    pub fn disconnected(&mut self) {
        self.close("The connection to the relay ended.".into());
    }

    /// Keep the session alive; `now_ms` is any monotonic clock.
    pub fn tick(&mut self, now_ms: f64) {
        if !now_ms.is_finite() || now_ms < self.now_ms {
            return;
        }
        self.now_ms = now_ms;
        if self.phase == Phase::LoggedIn
            && self
                .self_retry
                .as_ref()
                .is_some_and(|retry| now_ms >= retry.after)
        {
            let retry = self.self_retry.as_mut().expect("checked");
            if retry.remaining == 0 {
                self.self_retry = None;
                self.events.push_back(Event::Server(ServerEvent::Notice("The server has not confirmed the microphone settings. The microphone stays muted; toggle mute to retry.".into())));
            } else {
                retry.remaining -= 1;
                retry.after = now_ms + SELF_RETRY_MS;
                self.send_self();
            }
        }
        if self.phase == Phase::LoggedIn && now_ms - self.last_ping >= PING_MS {
            self.last_ping = now_ms;
            self.send(kind::PING, &proto::Ping::default());
        }
    }

    /// Move to a channel.
    pub fn join(&mut self, channel: u32) {
        let Some(session) = self.own_session() else {
            return;
        };
        self.send(
            kind::USER_STATE,
            &proto::UserState {
                session: Some(session),
                channel_id: Some(channel),
                ..Default::default()
            },
        );
    }

    /// Send local policy in full; sparse/delayed echoes cannot undo it.
    pub fn set_self(&mut self, muted: bool, deafened: bool) {
        self.desired_muted = muted || deafened;
        self.desired_deafened = deafened;
        if self.own_session().is_some() {
            self.self_change_pending = true;
            self.self_retry = Some(SelfRetry {
                after: self.now_ms + SELF_RETRY_MS,
                remaining: SELF_RETRIES,
            });
            self.send_self();
        }
    }

    fn send_self(&mut self) {
        if let Some(session) = self.own_session() {
            self.send(
                kind::USER_STATE,
                &proto::UserState {
                    session: Some(session),
                    self_mute: Some(self.desired_muted),
                    self_deaf: Some(self.desired_deafened),
                    ..Default::default()
                },
            );
        }
    }

    /// Server acknowledgement AND local intent gate the transport itself.
    pub fn voice_policy(&self) -> (bool, bool) {
        let Some(own) = self.own_session().and_then(|id| self.users.get(&id)) else {
            return (true, true);
        };
        let deaf = self.desired_deafened || own.self_deafened || own.deafened;
        (
            self.desired_muted
                || self.self_change_pending
                || deaf
                || own.self_muted
                || own.muted
                || own.suppressed
                || !self.opus_allowed,
            deaf,
        )
    }

    /// Effective mono target for the default request on this authenticated
    /// connection, including the conservative server-accounting reserve.
    pub fn voice_bitrate(&self) -> u32 {
        nuschl_core::audio::voice_bitrate(self.limits.bandwidth())
    }

    /// What `requested` becomes against this connection's budget, and why.
    pub fn voice_target(&self, requested: u32) -> nuschl_core::bitrate::VoiceBitrate {
        nuschl_core::bitrate::VoiceBitrate::resolve(requested, self.limits.bandwidth())
    }

    pub fn send_voice(&mut self, sequence: u64, opus: Vec<u8>, end: bool) -> Result<(), String> {
        // The route admits against the requested target; this is the
        // server's own cap (the highest request it allows), never exceeded.
        let cap = self
            .voice_target(nuschl_core::bitrate::MAX_VOICE_BITRATE)
            .effective;
        if self.voice_policy().0 || !nuschl_core::bitrate::voice_frame_fits(cap, opus.len()) {
            return Err(
                "Voice exceeds its bandwidth budget, is muted or not acknowledged by the server."
                    .into(),
            );
        }
        let packet = nuschl_protocol::voice::VoicePacket {
            sender: 0,
            sequence,
            opus,
            end,
            volume: 1.0,
        };
        let bytes = nuschl_protocol::voice::encode_voice(self.voice_version, 0, &packet)
            .map_err(|_| "Invalid or oversized voice packet.")?;
        let frame = nuschl_protocol::encode_tunnel(&bytes);
        self.send_frame(&frame)?;
        Ok(())
    }

    /// Legacy text only. Modern/encrypted sends have no fallback here.
    pub fn send_text(
        &mut self,
        target: TextTarget,
        body: String,
        expected_peer: Option<u64>,
    ) -> Result<(), String> {
        if self.phase != Phase::LoggedIn || body.trim().is_empty() || body.len() > MAX_CONTENT_BYTES
        {
            return Err("Connect and enter a message of at most 256 KiB.".into());
        }
        let mut wire = proto::TextMessage::default();
        match &target {
            TextTarget::Channel(id) if self.channels.contains_key(id) => wire.channel_id.push(*id),
            TextTarget::Direct(id)
                if self
                    .users
                    .get(id)
                    .is_some_and(|user| expected_peer == Some(user.instance)) =>
            {
                wire.session.push(*id)
            }
            _ => {
                return Err(
                    "This recipient is unavailable or requires an unsupported protocol.".into(),
                );
            }
        }
        wire.message = nuschl_legacy_html::legacy_body(&body, self.limits.allow_html);
        self.limits
            .check_text_wire(&wire.message)
            .map_err(|reason| reason.to_string())?;
        self.send_control(kind::TEXT_MESSAGE, &wire)?;
        if self.is_closed() {
            return Err("TLS failed while sending.".into());
        }
        let peer = match target {
            TextTarget::Direct(id) => self.users.get(&id).cloned(),
            _ => None,
        };
        let channel_instance = match &target {
            TextTarget::Channel(id) => self.channels.get(id).map(|channel| channel.instance),
            _ => None,
        };
        self.events.push_back(Event::Text {
            target: target.clone(),
            channel_instance,
            peer,
            author: self.login.username.clone(),
            body: body.clone(),
            html: false,
            outgoing: true,
            transport: Some(self.transport_queued),
        });
        self.events.push_back(Event::Sent {
            target,
            body,
            transport: self.transport_queued,
        });
        Ok(())
    }

    /// The SDK admits only fixed worker-encoded PNG/JPEG plus its own captured
    /// caption. Every authority/limit check occurs again at actual enqueue.
    pub fn send_inline_image(
        &mut self,
        target: TextTarget,
        image: &str,
        caption: &str,
        expected_peer: Option<u64>,
        expected_channel: Option<u64>,
        expected_own: u64,
    ) -> Result<(), String> {
        crate::content_broker::validate_image_html(image)?;
        if self.phase != Phase::LoggedIn
            || !self.limits.allow_html
            || caption.len() > MAX_CONTENT_BYTES
            || self
                .own
                .and_then(|id| self.users.get(&id))
                .is_none_or(|user| user.instance != expected_own)
        {
            return Err("This image belongs to an unavailable conversation.".into());
        }
        let mut wire = proto::TextMessage::default();
        match &target {
            TextTarget::Channel(id) if self.channels.get(id).is_some_and(|channel| Some(channel.instance) == expected_channel) => wire.channel_id.push(*id),
            TextTarget::Direct(id) if self.users.get(id).is_some_and(|user| Some(user.instance) == expected_peer) => wire.session.push(*id),
            _ => return Err("This image recipient changed. Keep the attachment and select its destination again.".into()),
        }
        wire.message = crate::image::compose(image, caption, &self.limits)?;
        self.send_control(kind::TEXT_MESSAGE, &wire)?;
        if self.is_closed() {
            return Err("TLS failed while sending the image.".into());
        }
        let peer = match target {
            TextTarget::Direct(id) => self.users.get(&id).cloned(),
            _ => None,
        };
        self.events.push_back(Event::Text {
            target: target.clone(),
            channel_instance: expected_channel,
            peer,
            author: self.login.username.clone(),
            body: wire.message,
            html: true,
            outgoing: true,
            transport: Some(self.transport_queued),
        });
        self.events.push_back(Event::Sent {
            target,
            body: caption.into(),
            transport: self.transport_queued,
        });
        Ok(())
    }

    pub fn request_permissions(&mut self, channel: u32) {
        if self.phase == Phase::LoggedIn {
            self.send(
                kind::PERMISSION_QUERY,
                &proto::PermissionQuery {
                    channel_id: Some(channel),
                    ..Default::default()
                },
            );
        }
    }

    /// Available only after successful TLS verification, including CA-backed
    /// connections without a preconfigured pin. Matches ServerBookmark scoping.
    pub fn history_identity(&self) -> Option<String> {
        if self.tls.is_handshaking() {
            return None;
        }
        self.verifier
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|(_, rejected)| !rejected)
            .map(|(pin, _)| {
                if self.login.pin.is_some() {
                    nuschl_core::bookmark::fingerprint(
                        format!("{}:{}:{pin}", self.login.host, self.login.port).as_bytes(),
                    )
                } else {
                    nuschl_core::bookmark::webpki_history_identity(
                        &self.login.host,
                        self.login.port,
                    )
                }
            })
    }

    /// Our session number once logged in.
    pub fn own_session(&self) -> Option<u32> {
        self.own.filter(|_| self.phase == Phase::LoggedIn)
    }

    fn close(&mut self, reason: String) {
        if self.phase != Phase::Closed {
            self.phase = Phase::Closed;
            self.events.push_back(Event::Closed { reason });
        }
    }

    fn tls_in(&mut self, mut bytes: &[u8]) {
        let mut ended = false;
        while !bytes.is_empty() {
            match self.tls.read_tls(&mut bytes) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => return self.close("TLS failed.".into()),
            }
            if let Err(error) = self.tls.process_new_packets() {
                self.report_certificate();
                let reason = format!("TLS failed: {error}");
                self.flush_tls();
                return self.close(reason);
            }
            // rustls applies receive-plaintext backpressure independently of
            // the relay's chunk bound. Drain each processed batch before the
            // next read_tls, but frame it only after this TLS input succeeds.
            match self.drain_tls_plaintext() {
                Ok(eof) => ended = eof,
                Err(reason) => return self.close(reason.into()),
            }
            if ended {
                break;
            }
        }
        if self.phase == Phase::LoggingIn
            && !self.tls.is_handshaking()
            && !self.certificate_reported
        {
            self.report_certificate();
            self.log_in();
        }
        if !ended {
            match self.drain_tls_plaintext() {
                Ok(eof) => ended = eof,
                Err(reason) => return self.close(reason.into()),
            }
        }
        self.frames();
        // close_notify can follow valid application bytes in the same input.
        // Preserve those complete frames before reporting the clean EOF.
        if ended {
            self.close("The server closed the connection.".into());
        }
        self.flush_tls();
    }

    fn drain_tls_plaintext(&mut self) -> Result<bool, &'static str> {
        let mut chunk = [0_u8; 16 * 1024];
        loop {
            match self.tls.reader().read(&mut chunk) {
                Ok(0) => return Ok(true),
                Ok(size) => {
                    if self.plaintext.len() + size > MAX_BUFFER {
                        return Err("The server sent too much at once.");
                    }
                    self.plaintext.extend_from_slice(&chunk[..size]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
                Err(_) => return Err("TLS failed."),
            }
        }
    }

    /// The certificate seen: pinned and matching, vouched for, or for the user.
    fn report_certificate(&mut self) {
        if self.certificate_reported {
            return;
        }
        let seen = self
            .verifier
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some((sha256, rejected)) = seen else {
            return;
        };
        // A pinned/CA-valid certificate is not evidence of TLS key possession
        // until CertificateVerify/Finished have succeeded. Failed handshakes may
        // report a rejected certificate prompt, never an accepted peer identity.
        if !rejected && self.tls.is_handshaking() {
            return;
        }
        self.certificate_reported = true;
        if rejected {
            self.events.push_back(Event::Certificate {
                sha256,
                changed: self.login.pin.is_some(),
            });
        } else {
            self.events
                .push_back(Event::Server(ServerEvent::PeerCertificate(sha256)));
        }
    }

    fn log_in(&mut self) {
        let version = proto::Version {
            version_v1: Some(0x0001_0500),
            version_v2: Some(nuschl_protocol::voice::PROTOBUF_VERSION),
            release: Some(format!("Nuschl web {}", env!("CARGO_PKG_VERSION"))),
            os: Some("web".into()),
            ..Default::default()
        };
        self.send(kind::VERSION, &version);
        let authenticate = proto::Authenticate {
            username: Some(self.login.username.clone()),
            password: self.login.password.clone(),
            tokens: self.login.tokens.clone(),
            opus: Some(true),
            ..Default::default()
        };
        self.send(kind::AUTHENTICATE, &authenticate);
        self.send(
            kind::USER_STATE,
            &proto::UserState {
                self_mute: Some(self.desired_muted),
                self_deaf: Some(self.desired_deafened),
                ..Default::default()
            },
        );
    }

    fn send(&mut self, kind: u16, message: &impl prost::Message) {
        if let Err(reason) = self.send_control(kind, message) {
            self.fail_transport(reason);
        }
    }

    fn send_control(&mut self, kind: u16, message: &impl prost::Message) -> Result<(), String> {
        let frame = encode_control(kind, message)
            .ok_or("The outgoing control message exceeds its protocol limit.")?;
        self.send_frame(&frame)
    }

    /// Serialize one complete control/voice frame synchronously. rustls keeps
    /// its default 64 KiB buffer; drain records between partial plaintext writes.
    /// No other frame can interleave with a partially consumed plaintext frame.
    fn send_frame(&mut self, frame: &[u8]) -> Result<(), String> {
        if self.is_closed() {
            return Err("This connection is closed.".into());
        }
        if let Err(reason) = self.flush_tls_checked() {
            self.fail_transport(reason.clone());
            return Err(reason);
        }
        // Reserve conservative TLS/tunnel overhead before consuming ANY byte.
        // A temporary full transport refuses this send without closing or
        // emitting a partial frame. Other complete pending frames stay intact.
        let records = frame.len().div_ceil(16 * 1024);
        let reserve = frame
            .len()
            .saturating_add(records.saturating_mul(128))
            .saturating_add(4096);
        if reserve > MAX_OUTGOING_BYTES.saturating_sub(self.outgoing_bytes)
            || self
                .outgoing
                .len()
                .saturating_add(records)
                .saturating_add(4)
                > MAX_OUTGOING_FRAMES
        {
            return Err("The outgoing transport is busy. Keep the message and retry after pending traffic is sent.".into());
        }
        let mut remaining = frame;
        while !remaining.is_empty() {
            let count = match self.tls.writer().write(remaining) {
                Ok(count) => count,
                Err(_) => {
                    self.fail_transport("TLS failed while writing an outgoing frame.".into());
                    return Err("TLS failed while writing an outgoing frame.".into());
                }
            };
            if count == 0 {
                // Zero progress after a drain cannot be repaired by spinning or
                // acknowledging a partial application frame as a successful send.
                self.fail_transport("TLS made no progress while writing an outgoing frame.".into());
                return Err("TLS made no progress while writing an outgoing frame.".into());
            }
            remaining = &remaining[count..];
            if let Err(reason) = self.flush_tls_checked() {
                self.fail_transport(reason.clone());
                return Err(reason);
            }
        }
        Ok(())
    }

    fn fail_transport(&mut self, reason: String) {
        // Never release a truncated application frame to the WebSocket pump.
        self.outgoing.clear();
        self.outgoing_bytes = 0;
        self.close(reason);
    }

    fn flush_tls(&mut self) {
        if let Err(reason) = self.flush_tls_checked() {
            self.fail_transport(reason);
        }
    }

    fn flush_tls_checked(&mut self) -> Result<(), String> {
        while self.tls.wants_write() {
            if self.outgoing_bytes >= MAX_OUTGOING_BYTES
                || self.outgoing.len() >= MAX_OUTGOING_FRAMES
            {
                return Err("The outgoing TLS transport exceeds its bounded queue.".into());
            }
            let mut bytes = TlsChunk {
                bytes: Vec::new(),
                limit: tunnel::MAX_STREAM.min(MAX_OUTGOING_BYTES - self.outgoing_bytes - 1),
            };
            let written = self
                .tls
                .write_tls(&mut bytes)
                .map_err(|_| "TLS failed while draining an outgoing frame.")?;
            if written == 0 || bytes.bytes.is_empty() {
                return Err("TLS made no progress while draining an outgoing frame.".into());
            }
            let frame = tunnel::encode(&Frame::Stream(&bytes.bytes))
                .ok_or("Invalid outgoing TLS stream chunk.")?;
            self.transport_queued = self
                .transport_queued
                .checked_add(1)
                .ok_or("Outgoing transport sequence exhausted.")?;
            self.outgoing_bytes += frame.len();
            self.outgoing.push(frame);
        }
        Ok(())
    }

    fn frames(&mut self) {
        loop {
            if self.is_closed() {
                return;
            }
            match decode_control(&mut self.plaintext) {
                Ok(Some(frame)) => {
                    self.control(frame.kind, frame.payload);
                    if self.events.len() > MAX_EVENTS {
                        return self.close("The server sent too much at once.".into());
                    }
                }
                Ok(None) => return,
                Err(_) => return self.close("The server sent a frame that is too large.".into()),
            }
        }
    }

    fn control(&mut self, frame_kind: u16, payload: bytes::Bytes) {
        match frame_kind {
            kind::VERSION => {
                if let Ok(version) = proto::Version::decode(payload) {
                    self.voice_version = version.version_v2.unwrap_or_else(|| {
                        let v = version.version_v1.unwrap_or(0);
                        (((v >> 16) as u64) << 48)
                            | ((((v >> 8) & 255) as u64) << 32)
                            | (((v & 255) as u64) << 16)
                    });
                }
            }
            kind::CODEC_VERSION => {
                if let Ok(codec) = proto::CodecVersion::decode(payload) {
                    self.opus_allowed = codec.opus.unwrap_or(false);
                    self.events
                        .push_back(Event::Server(ServerEvent::OpusAvailable(self.opus_allowed)));
                }
            }
            kind::UDP_TUNNEL => {
                if self.phase != Phase::LoggedIn || self.voice_policy().1 || !self.opus_allowed {
                    return;
                }
                if let Ok(packet) = nuschl_protocol::voice::decode_voice(&payload)
                    && self.users.contains_key(&packet.sender)
                    && self.events.len() < MAX_EVENTS
                {
                    self.events.push_back(Event::Voice(packet));
                }
            }
            kind::SERVER_SYNC => {
                let Ok(sync) = proto::ServerSync::decode(payload) else {
                    return;
                };
                let Some(session) = sync.session else {
                    return self.close("The server omitted our session number.".into());
                };
                self.limits.max_bandwidth = Some(
                    sync.max_bandwidth
                        .unwrap_or(nuschl_core::audio::DEFAULT_SERVER_BANDWIDTH),
                );
                self.phase = Phase::LoggedIn;
                self.own = Some(session);
                self.events
                    .push_back(Event::Server(ServerEvent::Connected { session }));
                if let Some(source) = sync.welcome_text {
                    self.welcome(source);
                }
                self.events
                    .push_back(Event::Server(ServerEvent::Limits(self.limits.clone())));
            }
            kind::REJECT => {
                let reject = proto::Reject::decode(payload).unwrap_or_default();
                let reason = reject
                    .reason
                    .filter(|reason| !reason.is_empty())
                    .unwrap_or_else(|| "The server refused the login.".into());
                self.close(bounded(reason));
            }
            kind::CHANNEL_STATE => {
                let Ok(state) = proto::ChannelState::decode(payload) else {
                    return;
                };
                let Some(id) = state.channel_id else {
                    return;
                };
                if self.channels.len() >= 10_000 && !self.channels.contains_key(&id) {
                    return;
                }
                let channel = self.channels.entry(id).or_insert_with(|| Channel {
                    id,
                    instance: next_channel_instance(),
                    can_enter: true,
                    ..Channel::default()
                });
                if let Some(parent) = state.parent {
                    channel.parent = Some(parent);
                }
                if let Some(name) = state.name {
                    channel.name = bounded(name);
                }
                if let Some(position) = state.position {
                    channel.position = position;
                }
                let channel = channel.clone();
                self.events
                    .push_back(Event::Server(ServerEvent::Channel(channel)));
            }
            kind::CHANNEL_REMOVE => {
                if let Ok(remove) = proto::ChannelRemove::decode(payload) {
                    self.channels.remove(&remove.channel_id);
                    self.events
                        .push_back(Event::Server(ServerEvent::ChannelRemoved(
                            remove.channel_id,
                        )));
                }
            }
            kind::SERVER_CONFIG => {
                if let Ok(config) = proto::ServerConfig::decode(payload) {
                    if let Some(source) = config.welcome_text {
                        self.welcome(source);
                    }
                    let supplied = self.limits.apply(nuschl_core::model::LimitsUpdate {
                        allow_html: config.allow_html,
                        message_length: config.message_length,
                        image_message_length: config.image_message_length,
                        max_users: config.max_users,
                        recording_allowed: config.recording_allowed,
                        max_bandwidth: config.max_bandwidth,
                    });
                    if supplied {
                        self.events
                            .push_back(Event::Server(ServerEvent::Limits(self.limits.clone())));
                    }
                }
            }

            kind::PERMISSION_QUERY => {
                if let Ok(query) = proto::PermissionQuery::decode(payload)
                    && let (Some(channel), Some(permissions)) =
                        (query.channel_id, query.permissions)
                {
                    self.events
                        .push_back(Event::Server(ServerEvent::Permissions {
                            channel,
                            permissions,
                            flush: query.flush.unwrap_or(false),
                        }));
                }
            }
            kind::PERMISSION_DENIED => {
                self.events
                    .push_back(Event::Server(ServerEvent::PermissionDenied(
                        "The server refused this action.".into(),
                    )));
            }
            kind::TEXT_MESSAGE => {
                let Ok(message) = proto::TextMessage::decode(payload) else {
                    return;
                };
                if self.phase != Phase::LoggedIn {
                    return;
                }
                if message.message.len() > MAX_CONTENT_BYTES {
                    if !self.oversized_text_reported {
                        self.oversized_text_reported = true;
                        self.events.push_back(Event::Server(ServerEvent::Notice(
                            "Some incoming messages exceeded the local display limit and were not shown.".into(),
                        )));
                    }
                    return;
                }
                let peer = message.actor.and_then(|id| self.users.get(&id)).cloned();
                let author = peer
                    .as_ref()
                    .map(|user| user.name.clone())
                    .unwrap_or_else(|| "Server".into());
                let mut targets = message
                    .channel_id
                    .iter()
                    .filter(|id| self.channels.contains_key(id))
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>();
                if let Some(own) = self.own.and_then(|id| self.users.get(&id)) {
                    for root in message.tree_id {
                        let mut current = Some(own.channel);
                        let mut seen = std::collections::BTreeSet::new();
                        while let Some(id) = current {
                            if !seen.insert(id) {
                                break;
                            }
                            if id == root {
                                targets.insert(own.channel);
                                break;
                            }
                            current = self.channels.get(&id).and_then(|channel| channel.parent);
                        }
                    }
                }
                let targets = if targets.is_empty() {
                    vec![
                        peer.as_ref()
                            .map(|peer| TextTarget::Direct(peer.session))
                            .unwrap_or(TextTarget::Channel(0)),
                    ]
                } else {
                    targets.into_iter().map(TextTarget::Channel).collect()
                };
                for target in targets {
                    if self.events.len() >= MAX_EVENTS {
                        return self.close("The server sent too much text at once.".into());
                    }
                    let channel_instance = match &target {
                        TextTarget::Channel(id) => {
                            self.channels.get(id).map(|channel| channel.instance)
                        }
                        _ => None,
                    };
                    self.events.push_back(Event::Text {
                        target,
                        channel_instance,
                        peer: peer.clone(),
                        author: author.clone(),
                        body: message.message.clone(),
                        html: self.limits.allow_html,
                        outgoing: false,
                        transport: None,
                    });
                }
            }
            kind::USER_STATE => {
                let Ok(state) = proto::UserState::decode(payload) else {
                    return;
                };
                let Some(session) = state.session else {
                    return;
                };
                if self.users.len() >= 10_000 && !self.users.contains_key(&session) {
                    return;
                }
                let observed_self = state.self_mute.is_some() || state.self_deaf.is_some();
                let user = self.users.entry(session).or_insert_with(|| User {
                    session,
                    instance: next_user_instance(),
                    ..User::default()
                });
                if let Some(name) = state.name {
                    user.name = bounded(name);
                }
                if let Some(channel) = state.channel_id {
                    user.channel = channel;
                }
                if let Some(user_id) = state.user_id {
                    user.user_id = Some(user_id);
                }
                if let Some(hash) = state.hash {
                    if user
                        .certificate_hash
                        .as_deref()
                        .filter(|old| !old.is_empty())
                        .is_some_and(|old| !old.eq_ignore_ascii_case(&hash))
                    {
                        user.instance = next_user_instance();
                    }
                    user.certificate_hash = Some(bounded(hash));
                }
                if let Some(muted) = state.mute {
                    user.muted = muted;
                }
                if let Some(deafened) = state.deaf {
                    user.deafened = deafened;
                }
                if let Some(suppressed) = state.suppress {
                    user.suppressed = suppressed;
                }
                if let Some(muted) = state.self_mute {
                    user.self_muted = muted;
                }
                if let Some(deafened) = state.self_deaf {
                    user.self_deafened = deafened;
                }
                user.self_muted |= user.self_deafened;
                if self.own == Some(session)
                    && observed_self
                    && (user.self_muted, user.self_deafened)
                        == (self.desired_muted, self.desired_deafened)
                {
                    self.self_change_pending = false;
                    self.self_retry = None;
                }
                let user = user.clone();
                self.events
                    .push_back(Event::Server(ServerEvent::User(user)));
            }
            kind::USER_REMOVE => {
                if let Ok(remove) = proto::UserRemove::decode(payload) {
                    self.users.remove(&remove.session);
                    self.events
                        .push_back(Event::Server(ServerEvent::UserRemoved(remove.session)));
                }
            }
            _ => {}
        }
    }
}

/// Server text shown as plain text: one bounded line.
fn bounded(text: String) -> String {
    let line = text.replace(['\n', '\r'], " ");
    line.chars().take(512).collect()
}

fn webpki_roots() -> impl Iterator<Item = rustls::pki_types::TrustAnchor<'static>> {
    webpki_roots::TLS_SERVER_ROOTS.iter().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_pressure_is_refused_before_any_send_and_partial_failure_keeps_exact_receipts() {
        let frames = vec![vec![0; 20]; 6];
        let mut calls = 0;
        let result = enqueue_outgoing(10, &frames, MAX_OUTGOING_BYTES, |_| {
            calls += 1;
            Ok(())
        });
        assert!(result.1.is_some());
        assert_eq!(result.0, 10);
        assert_eq!(calls, 0);
        assert!(
            enqueue_outgoing(u64::MAX, &frames, 0, |_| panic!(
                "overflow must be refused before send"
            ))
            .1
            .is_some()
        );
        let result = enqueue_outgoing(10, &frames, 0, |_| {
            calls += 1;
            if calls == 4 { Err(()) } else { Ok(()) }
        });
        assert_eq!(result.0, 13);
        assert!(result.1.is_some());
        assert_eq!(calls, 4);
        assert!(
            Event::Sent {
                target: TextTarget::Channel(0),
                body: "first complete frame".into(),
                transport: 12
            }
            .transport_admitted(result.0)
        );
        assert!(
            !Event::Sent {
                target: TextTarget::Channel(0),
                body: "second incomplete frame".into(),
                transport: 16
            }
            .transport_admitted(result.0)
        );
        let peer = Event::Text {
            target: TextTarget::Channel(0),
            channel_instance: Some(7),
            peer: None,
            author: "peer".into(),
            body: "independently received".into(),
            html: false,
            outgoing: false,
            transport: None,
        };
        assert!(peer.transport_admitted(result.0));
        let own = Event::Text {
            target: TextTarget::Channel(0),
            channel_instance: Some(7),
            peer: None,
            author: "own".into(),
            body: "not fully enqueued".into(),
            html: false,
            outgoing: true,
            transport: Some(16),
        };
        assert!(!own.transport_admitted(result.0));
    }

    #[test]
    fn tls_chunks_bound_allocations_and_zero_progress_does_not_spin_or_ack() {
        let mut chunk = TlsChunk {
            bytes: Vec::new(),
            limit: 3,
        };
        assert_eq!(chunk.write(b"abcdef").unwrap(), 3);
        assert_eq!(chunk.write(b"more").unwrap(), 0);
        assert_eq!(chunk.bytes, b"abc");
        let mut session = session();
        // Before handshake, rustls can buffer only its default plaintext limit.
        // Draining cannot encrypt it yet: fail closed instead of spinning.
        assert!(session.send_frame(&vec![0; 200_000]).is_err());
        assert!(session.is_closed());
        let (before, outgoing) = session.take_outgoing_batch();
        assert_eq!(
            before, 0,
            "discarded unsent TLS records are never an admitted/drained boundary"
        );
        assert!(outgoing.is_empty());
        assert!(
            !Event::Sent {
                target: TextTarget::Channel(0),
                body: "discarded".into(),
                transport: 1
            }
            .transport_admitted(before)
        );
        assert!(session.take_events().iter().all(|event| !matches!(
            event,
            Event::Sent { .. } | Event::Text { outgoing: true, .. }
        )));
    }

    #[test]
    fn sync_publishes_limits_but_only_codec_version_publishes_negotiated_support() {
        for bandwidth in [None, Some(0), Some(24_000)] {
            let mut session = voice_session();
            session.take_events();
            session.control(
                kind::SERVER_SYNC,
                proto::ServerSync {
                    session: Some(1),
                    max_bandwidth: bandwidth,
                    ..Default::default()
                }
                .encode_to_vec()
                .into(),
            );
            let events = session.take_events();
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Event::Server(ServerEvent::OpusAvailable(_))))
            );
            let limits = events
                .iter()
                .find_map(|event| match event {
                    Event::Server(ServerEvent::Limits(limits)) => Some(limits),
                    _ => None,
                })
                .expect("Sync without ServerConfig must publish current limits");
            assert_eq!(limits.max_bandwidth, Some(bandwidth.unwrap_or(72_000)));
            assert_eq!(limits.message_length, 5000);
            for opus in [Some(false), Some(true), None] {
                session.control(
                    kind::CODEC_VERSION,
                    proto::CodecVersion {
                        opus,
                        ..Default::default()
                    }
                    .encode_to_vec()
                    .into(),
                );
                assert!(session.take_events().iter().any(|event| matches!(event,
                    Event::Server(ServerEvent::OpusAvailable(value)) if *value == opus.unwrap_or(false))));
            }
        }
    }

    #[test]
    fn sparse_config_keeps_previous_limits_and_fresh_connections_reset_them() {
        let mut s = voice_session();
        s.limits.max_bandwidth = Some(48_000);
        for (config, expected) in [
            (
                proto::ServerConfig {
                    allow_html: Some(false),
                    message_length: Some(128),
                    image_message_length: Some(256),
                    max_users: Some(12),
                    recording_allowed: Some(false),
                    ..Default::default()
                },
                (false, 128, 256, Some(12), false, 48_000),
            ),
            (
                proto::ServerConfig {
                    max_bandwidth: Some(24_000),
                    ..Default::default()
                },
                (false, 128, 256, Some(12), false, 24_000),
            ),
            (
                proto::ServerConfig {
                    message_length: Some(0),
                    ..Default::default()
                },
                (false, 0, 256, Some(12), false, 24_000),
            ),
            (
                proto::ServerConfig {
                    image_message_length: Some(0),
                    max_users: Some(0),
                    ..Default::default()
                },
                (false, 0, 0, Some(0), false, 24_000),
            ),
            (
                proto::ServerConfig {
                    recording_allowed: Some(true),
                    ..Default::default()
                },
                (false, 0, 0, Some(0), true, 24_000),
            ),
            (
                proto::ServerConfig {
                    allow_html: Some(true),
                    message_length: Some(u32::MAX),
                    ..Default::default()
                },
                (true, u32::MAX as usize, 0, Some(0), true, 24_000),
            ),
        ] {
            s.control(kind::SERVER_CONFIG, config.encode_to_vec().into());
            let events = s.take_events();
            let [Event::Server(ServerEvent::Limits(value))] = events.as_slice() else {
                panic!("limits missing")
            };
            assert_eq!(
                (
                    value.allow_html,
                    value.message_length,
                    value.image_message_length,
                    value.max_users,
                    value.recording_allowed,
                    value.max_bandwidth.unwrap()
                ),
                expected
            );
        }
        let mut fresh = voice_session();
        fresh.control(
            kind::SERVER_CONFIG,
            proto::ServerConfig {
                max_users: Some(1),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        let events = fresh.take_events();
        let [Event::Server(ServerEvent::Limits(value))] = events.as_slice() else {
            panic!("limits missing")
        };
        assert_eq!(
            (
                value.allow_html,
                value.message_length,
                value.image_message_length,
                value.max_users,
                value.recording_allowed,
                value.max_bandwidth
            ),
            (true, 5000, 131_072, Some(1), true, Some(72_000))
        );
    }

    #[test]
    fn send_caps_count_formatted_utf16_and_reject_before_any_output_or_ack() {
        for (html, body, cap, allowed) in [
            (true, "<&>\n", 16, false),
            (true, "<&>\n", 18, true),
            (false, "<&>\n", 4, true),
            (false, "é", 1, true),
            (false, "é", 2, true),
            (true, "漢字", 2, true),
            (true, "🙂", 1, false),
            (true, "🙂", 2, true),
        ] {
            let mut s = voice_session();
            s.channels.insert(
                0,
                Channel {
                    id: 0,
                    ..Default::default()
                },
            );
            s.take_outgoing();
            s.control(
                kind::SERVER_CONFIG,
                proto::ServerConfig {
                    allow_html: Some(html),
                    message_length: Some(cap),
                    ..Default::default()
                }
                .encode_to_vec()
                .into(),
            );
            s.take_events();
            let result = s.send_text(TextTarget::Channel(0), body.into(), None);
            assert_eq!(result.is_ok(), allowed, "{html:?} {body:?} {cap}");
            if !allowed {
                assert!(s.take_outgoing().is_empty());
                assert!(s.take_events().is_empty());
            }
        }
        let mut s = voice_session();
        s.channels.insert(
            0,
            Channel {
                id: 0,
                ..Default::default()
            },
        );
        s.take_outgoing();
        assert!(
            s.send_text(TextTarget::Channel(0), "x".repeat(5001), None)
                .is_err()
        );
        s.control(
            kind::SERVER_CONFIG,
            proto::ServerConfig {
                allow_html: Some(false),
                message_length: Some(0),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        s.take_events();
        assert!(
            s.send_text(TextTarget::Channel(0), "x".repeat(5001), None)
                .is_ok()
        );
        assert!(
            s.send_text(
                TextTarget::Channel(0),
                "x".repeat(MAX_CONTENT_BYTES + 1),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn unlimited_text_still_has_html_total_and_local_wire_byte_limits() {
        for (image_limit, body, accepted, reason) in [
            (
                128,
                "x".repeat(129),
                false,
                "128-character total HTML limit",
            ),
            (128, "x".repeat(128), true, ""),
            (0, "x".repeat(5001), true, ""),
            (0, "&".repeat(60_000), false, "256 KiB client limit"),
        ] {
            let mut session = voice_session();
            session.channels.insert(
                0,
                Channel {
                    id: 0,
                    ..Default::default()
                },
            );
            session.take_outgoing();
            session.control(
                kind::SERVER_CONFIG,
                proto::ServerConfig {
                    allow_html: Some(true),
                    message_length: Some(0),
                    image_message_length: Some(image_limit),
                    ..Default::default()
                }
                .encode_to_vec()
                .into(),
            );
            session.take_events();
            let result = session.send_text(TextTarget::Channel(0), body, None);
            assert_eq!(result.is_ok(), accepted);
            if !accepted {
                assert!(result.unwrap_err().contains(reason));
                assert!(session.take_outgoing().is_empty());
                assert!(session.take_events().is_empty());
            }
        }
    }

    #[test]
    fn live_welcome_changes_and_empty_revocation_never_reset_limits_or_audio() {
        let mut s = session();
        let send = |s: &mut Session, source: Option<&str>| {
            s.control(
                kind::SERVER_CONFIG,
                proto::ServerConfig {
                    welcome_text: source.map(str::to_owned),
                    ..Default::default()
                }
                .encode_to_vec()
                .into(),
            )
        };
        send(&mut s, Some("before"));
        assert!(s.take_events().is_empty());
        assert!(s.welcome_source.is_none());
        s.control(
            kind::SERVER_SYNC,
            proto::ServerSync {
                session: Some(3),
                welcome_text: Some("initial".into()),
                max_bandwidth: Some(48_000),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        s.take_events();
        s.take_outgoing();
        let policy = (
            s.voice_policy(),
            s.voice_bitrate(),
            s.limits.bandwidth(),
            s.limits.allow_html,
            s.desired_muted,
            s.desired_deafened,
        );
        for (source, expected) in [
            (None, None),
            (Some("initial"), None),
            (
                Some("<b>changed.test</b>\nsecond"),
                Some("<b>changed.test</b>\nsecond"),
            ),
            (Some(""), Some("")),
            (Some(""), None),
            (None, None),
            (Some("initial"), Some("initial")),
        ] {
            send(&mut s, source);
            let events = s.take_events();
            match expected {
                Some(expected) => assert!(
                    matches!(events.as_slice(), [Event::Welcome(value)] if value == expected)
                ),
                None => assert!(events.is_empty()),
            }
            assert_eq!(
                (
                    s.voice_policy(),
                    s.voice_bitrate(),
                    s.limits.bandwidth(),
                    s.limits.allow_html,
                    s.desired_muted,
                    s.desired_deafened
                ),
                policy
            );
            assert!(s.take_outgoing().is_empty());
        }
        s.close("test disconnect".into());
        s.take_events();
        send(&mut s, Some("late closed"));
        assert!(s.take_events().is_empty());
        let mut replacement = voice_session();
        send(&mut replacement, Some("initial"));
        assert!(
            matches!(replacement.take_events().as_slice(),[Event::Welcome(value)] if value=="initial")
        );
    }

    #[test]
    fn oversized_live_welcome_uses_the_same_plain_fallback() {
        let mut s = voice_session();
        s.control(
            kind::SERVER_CONFIG,
            proto::ServerConfig {
                welcome_text: Some("界".repeat(MAX_CONTENT_BYTES)),
                allow_html: Some(false),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert!(
            matches!(&s.take_events()[0],Event::Welcome(value) if value=="[Server welcome exceeds the display limit]")
        );
        assert!(!s.limits.allow_html);
    }

    #[test]
    fn welcome_is_ordered_after_connected_and_preserves_markup_and_newlines() {
        let mut s = session();
        let source =
            "<b>something.de</b>\n&lt;em&gt;literal&lt;/em&gt;\n```rust\nfn main() {}\n```";
        s.control(
            kind::SERVER_SYNC,
            proto::ServerSync {
                session: Some(3),
                welcome_text: Some(source.into()),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        let events = s.take_events();
        assert!(matches!(
            events[0],
            Event::Server(ServerEvent::Connected { session: 3 })
        ));
        assert!(matches!(&events[1], Event::Welcome(value) if value == source));
    }

    #[test]
    fn oversized_welcome_is_plain_fallback_and_missing_session_cannot_publish_it() {
        let mut s = session();
        s.control(
            kind::SERVER_SYNC,
            proto::ServerSync {
                session: Some(3),
                welcome_text: Some("界".repeat(MAX_CONTENT_BYTES)),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        let events = s.take_events();
        let Event::Welcome(source) = &events[1] else {
            panic!("welcome missing")
        };
        assert_eq!(source, "[Server welcome exceeds the display limit]");
        let mut s = session();
        s.control(
            kind::SERVER_SYNC,
            proto::ServerSync {
                welcome_text: Some("must not appear".into()),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert!(
            !s.take_events()
                .iter()
                .any(|e| matches!(e, Event::Welcome(_)))
        );
    }

    fn session() -> Session {
        Session::new(Login {
            host: "localhost".into(),
            port: 64738,
            username: "test".into(),
            password: None,
            tokens: vec![],
            pin: None,
        })
        .unwrap()
    }

    fn voice_session() -> Session {
        let mut s = session();
        s.phase = Phase::LoggedIn;
        s.own = Some(3);
        s.users.insert(
            3,
            User {
                session: 3,
                channel: 1,
                ..Default::default()
            },
        );
        s
    }

    #[test]
    fn unacknowledged_self_state_retries_are_bounded_and_fail_closed() {
        let mut s = voice_session();
        s.set_self(false, false);
        for tick in 1..=7 {
            s.tick(tick as f64 * SELF_RETRY_MS);
            assert!(s.voice_policy().0);
        }
        let notices = s
            .take_events()
            .iter()
            .filter(|event| matches!(event, Event::Server(ServerEvent::Notice(_))))
            .count();
        assert_eq!(notices, 1);
        assert!(s.self_retry.is_none());
        s.tick(100_000.0);
        assert!(
            !s.take_events()
                .iter()
                .any(|event| matches!(event, Event::Server(ServerEvent::Notice(_))))
        );
        assert!(s.voice_policy().0);
    }

    #[test]
    fn retries_keep_latest_intent_and_only_matching_self_ack_stops_them() {
        let mut s = voice_session();
        s.set_self(false, false);
        s.set_self(true, true);
        s.set_self(false, false);
        s.control(
            kind::USER_STATE,
            proto::UserState {
                session: Some(3),
                self_mute: Some(true),
                self_deaf: Some(true),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        s.tick(SELF_RETRY_MS);
        assert!(s.self_retry.is_some());
        assert!(s.voice_policy().0);
        assert_eq!((s.desired_muted, s.desired_deafened), (false, false));
        s.control(
            kind::USER_STATE,
            proto::UserState {
                session: Some(3),
                self_deaf: Some(false),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert!(s.self_retry.is_some());
        s.control(
            kind::USER_STATE,
            proto::UserState {
                session: Some(3),
                self_mute: Some(false),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert!(s.self_retry.is_none());
        assert_eq!(s.voice_policy(), (false, false));
        s.tick(100_000.0);
        assert!(
            !s.take_events()
                .iter()
                .any(|event| matches!(event, Event::Server(ServerEvent::Notice(_))))
        );
    }

    #[test]
    fn authenticated_server_budget_tracks_initial_and_live_limits_before_voice_leaves_tls() {
        let mut s = voice_session();
        s.control(
            kind::SERVER_SYNC,
            proto::ServerSync {
                session: Some(3),
                max_bandwidth: Some(200_000),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert_eq!(s.voice_bitrate(), 128_000);
        s.set_self(false, false);
        s.control(
            kind::USER_STATE,
            proto::UserState {
                session: Some(3),
                self_mute: Some(false),
                self_deaf: Some(false),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert!(s.send_voice(0, vec![0; 320], false).is_ok());
        s.control(
            kind::SERVER_CONFIG,
            proto::ServerConfig {
                max_bandwidth: Some(48_000),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert_eq!(s.voice_bitrate(), 24_000);
        s.take_outgoing();
        assert!(s.send_voice(2, vec![0; 320], false).is_err());
        assert!(s.take_outgoing().is_empty());
        assert!(s.send_voice(2, vec![0; 60], false).is_ok());
        s.control(
            kind::SERVER_CONFIG,
            proto::ServerConfig {
                allow_html: Some(false),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert_eq!(
            s.voice_bitrate(),
            24_000,
            "sparse config preserves the budget"
        );
        s.control(
            kind::SERVER_CONFIG,
            proto::ServerConfig {
                max_bandwidth: Some(29_999),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert_eq!(s.voice_bitrate(), 0);
        assert!(s.send_voice(4, vec![0; 3], false).is_err());
        s.control(
            kind::SERVER_CONFIG,
            proto::ServerConfig {
                max_bandwidth: Some(200_000),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert_eq!(s.voice_bitrate(), 128_000);
        let replacement = voice_session();
        assert_eq!(
            replacement.voice_bitrate(),
            48_000,
            "reconnect does not inherit another server budget"
        );
    }

    #[test]
    fn local_mute_and_deafen_survive_sparse_and_delayed_server_echoes() {
        let mut s = voice_session();
        assert_eq!(s.voice_policy(), (true, true));
        assert!(s.send_voice(0, vec![1], false).is_err());
        s.set_self(false, false);
        // Missing/default false fields are not an acknowledgement of this command.
        assert_eq!(s.voice_policy(), (true, false));
        s.control(
            kind::USER_STATE,
            proto::UserState {
                session: Some(3),
                channel_id: Some(1),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert_eq!(s.voice_policy(), (true, false));
        s.control(
            kind::USER_STATE,
            proto::UserState {
                session: Some(3),
                self_mute: Some(false),
                self_deaf: Some(false),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert_eq!(s.voice_policy(), (false, false));
        s.set_self(false, true);
        s.control(
            kind::USER_STATE,
            proto::UserState {
                session: Some(3),
                self_mute: Some(false),
                self_deaf: Some(false),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert_eq!(s.voice_policy(), (true, true));
        assert!(s.send_voice(2, vec![1], false).is_err());
        s.set_self(true, false);
        s.control(
            kind::USER_STATE,
            proto::UserState {
                session: Some(3),
                channel_id: Some(2),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert_eq!(s.voice_policy(), (true, false));
        s.set_self(false, false);
        s.control(
            kind::USER_STATE,
            proto::UserState {
                session: Some(3),
                suppress: Some(true),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert!(s.voice_policy().0);
    }

    #[test]
    fn tunneled_voice_is_bounded_known_opus_and_blocked_while_deafened() {
        let mut s = voice_session();
        let mut packet = vec![0];
        nuschl_protocol::udp::Audio {
            sender_session: 3,
            frame_number: 9007199254740993,
            opus_data: vec![1, 2, 3],
            ..Default::default()
        }
        .encode(&mut packet)
        .unwrap();
        s.control(kind::UDP_TUNNEL, packet.clone().into());
        assert!(s.take_events().is_empty());
        s.set_self(true, false);
        s.control(kind::UDP_TUNNEL, packet.clone().into());
        assert!(
            matches!(s.take_events().as_slice(),[Event::Voice(p)] if p.sequence == 9007199254740993 && p.opus == [1,2,3])
        );
        s.control(kind::UDP_TUNNEL, vec![0; 1025].into());
        assert!(s.take_events().is_empty());
        s.users.clear();
        s.control(kind::UDP_TUNNEL, packet.into());
        assert!(s.take_events().is_empty());
        s.users.insert(
            3,
            User {
                session: 3,
                ..Default::default()
            },
        );
        s.set_self(false, false);
        assert!(s.send_voice(1, vec![0; 1025], false).is_err());
        s.control(
            kind::CODEC_VERSION,
            proto::CodecVersion {
                alpha: 0,
                beta: 0,
                prefer_alpha: true,
                opus: Some(false),
            }
            .encode_to_vec()
            .into(),
        );
        assert!(s.send_voice(1, vec![1], false).is_err());
        s.disconnected();
        assert_eq!(s.voice_policy(), (true, true));
    }

    fn pki_authority() -> rcgen::CertifiedIssuer<'static, rcgen::KeyPair> {
        let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        p.distinguished_name
            .push(rcgen::DnType::CommonName, "Private browser test root");
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        p.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        rcgen::CertifiedIssuer::self_signed(p, rcgen::KeyPair::generate().unwrap()).unwrap()
    }
    fn pki_leaf(
        issuer: &rcgen::Issuer<'_, rcgen::KeyPair>,
        host: &str,
        expired: bool,
    ) -> (
        CertificateDer<'static>,
        rustls::pki_types::PrivateKeyDer<'static>,
    ) {
        let mut p = rcgen::CertificateParams::new(vec![host.into()]).unwrap();
        p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        if expired {
            p.not_before = rcgen::date_time_ymd(2000, 1, 1);
            p.not_after = rcgen::date_time_ymd(2001, 1, 1);
        }
        let key = rcgen::KeyPair::generate().unwrap();
        (
            p.signed_by(&key, issuer).unwrap().der().clone(),
            rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
    }
    fn pki_handshake(
        pin: Option<String>,
        root: CertificateDer<'static>,
        chain: Vec<CertificateDer<'static>>,
        key: rustls::pki_types::PrivateKeyDer<'static>,
    ) -> (Session, Vec<u16>) {
        let (client, _, frames) = pki_pair(pin, root, chain, key);
        (client, frames)
    }
    fn pki_pair(
        pin: Option<String>,
        root: CertificateDer<'static>,
        chain: Vec<CertificateDer<'static>>,
        key: rustls::pki_types::PrivateKeyDer<'static>,
    ) -> (Session, rustls::ServerConnection, Vec<u16>) {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(root).unwrap();
        let mut client = Session::with_roots(
            Login {
                host: "localhost".into(),
                port: 64738,
                username: "fixture".into(),
                password: Some("private-fixture-password".into()),
                tokens: vec![],
                pin,
            },
            roots,
        )
        .unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
        let mut server = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        client.take_outgoing();
        client.receive(&tunnel::encode(&Frame::Opened { udp: false }).unwrap());
        let mut application = BytesMut::new();
        for _ in 0..32 {
            let mut progress = false;
            for frame in client.take_outgoing() {
                if let Ok(Frame::Stream(mut bytes)) = tunnel::decode(&frame) {
                    progress = true;
                    server.read_tls(&mut bytes).unwrap();
                    if server.process_new_packets().is_err() {
                        break;
                    }
                }
            }
            while server.wants_write() {
                progress = true;
                let mut bytes = Vec::new();
                server.write_tls(&mut bytes).unwrap();
                client.receive(&tunnel::encode(&Frame::Stream(&bytes)).unwrap());
            }
            let mut bytes = [0; 4096];
            while let Ok(count) = server.reader().read(&mut bytes) {
                if count == 0 {
                    break;
                }
                application.extend_from_slice(&bytes[..count]);
            }
            if !progress {
                break;
            }
        }
        let mut frames = vec![];
        while let Ok(Some(frame)) = decode_control(&mut application) {
            frames.push(frame.kind);
        }
        (client, server, frames)
    }

    fn incoming_tls_peer() -> (Session, rustls::ServerConnection) {
        let root = pki_authority();
        let (leaf, key) = pki_leaf(&root, "localhost", false);
        let (mut client, mut server, frames) = pki_pair(None, root.der().clone(), vec![leaf], key);
        assert!(frames.contains(&kind::AUTHENTICATE));
        let mut setup = Vec::new();
        setup.extend_from_slice(
            &encode_control(
                kind::CHANNEL_STATE,
                &proto::ChannelState {
                    channel_id: Some(0),
                    name: Some("Root".into()),
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        for (session, name) in [(3, "fixture"), (5, "peer")] {
            setup.extend_from_slice(
                &encode_control(
                    kind::USER_STATE,
                    &proto::UserState {
                        session: Some(session),
                        name: Some(name.into()),
                        channel_id: Some(0),
                        ..Default::default()
                    },
                )
                .unwrap(),
            );
        }
        setup.extend_from_slice(
            &encode_control(
                kind::SERVER_SYNC,
                &proto::ServerSync {
                    session: Some(3),
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        let encrypted = encrypt_incoming(&mut server, &setup, false);
        deliver_incoming(&mut client, &encrypted, &[tunnel::MAX_STREAM]);
        assert_eq!(client.phase, Phase::LoggedIn);
        client.take_events();
        (client, server)
    }
    fn encrypt_incoming(
        server: &mut rustls::ServerConnection,
        mut plain: &[u8],
        close: bool,
    ) -> Vec<u8> {
        let mut encrypted = Vec::new();
        while !plain.is_empty() {
            let count = server.writer().write(plain).unwrap();
            assert!(count > 0);
            plain = &plain[count..];
            while server.wants_write() {
                assert!(server.write_tls(&mut encrypted).unwrap() > 0);
            }
        }
        if close {
            server.send_close_notify();
            while server.wants_write() {
                assert!(server.write_tls(&mut encrypted).unwrap() > 0);
            }
        }
        encrypted
    }
    fn deliver_incoming(client: &mut Session, encrypted: &[u8], pattern: &[usize]) {
        assert!(!pattern.is_empty());
        let mut at = 0_usize;
        for size in pattern.iter().cycle() {
            assert!((1..=tunnel::MAX_STREAM).contains(size));
            let end = at.saturating_add(*size).min(encrypted.len());
            client.receive(&tunnel::encode(&Frame::Stream(&encrypted[at..end])).unwrap());
            at = end;
            if at == encrypted.len() {
                break;
            }
        }
    }
    fn incoming_text(body: &str) -> Vec<u8> {
        encode_control(
            kind::TEXT_MESSAGE,
            &proto::TextMessage {
                actor: Some(5),
                channel_id: vec![0],
                message: body.into(),
                ..Default::default()
            },
        )
        .unwrap()
        .to_vec()
    }
    #[test]
    fn incoming_tls_original_read_order_hits_plaintext_backpressure() {
        let (mut client, mut server) = incoming_tls_peer();
        let encrypted =
            encrypt_incoming(&mut server, &incoming_text(&"x".repeat(96 * 1024)), false);
        let mut chunk = &encrypted[..tunnel::MAX_STREAM];
        let error = loop {
            match client.tls.read_tls(&mut chunk) {
                Err(error) => break error,
                Ok(count) => assert!(count > 0),
            }
            client.tls.process_new_packets().unwrap();
            assert!(
                !chunk.is_empty(),
                "original order unexpectedly consumed the whole burst"
            );
        };
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(error.to_string(), "received plaintext buffer full");
        eprintln!("original incoming TLS order: {:?}: {}", error.kind(), error);
    }
    #[test]
    fn incoming_tls_large_message_survives_coalescing_and_fragmentation() {
        let body = "x".repeat(96 * 1024);
        for pattern in [
            vec![tunnel::MAX_STREAM],
            vec![1, 3, 7, 4093, tunnel::MAX_STREAM],
            vec![16_383, 2, 16_384, 127],
        ] {
            let (mut client, mut server) = incoming_tls_peer();
            let mut plain = incoming_text(&body);
            plain.extend(incoming_text("healthy tail"));
            let encrypted = encrypt_incoming(&mut server, &plain, false);
            assert!(encrypted.len() > tunnel::MAX_STREAM);
            deliver_incoming(&mut client, &encrypted, &pattern);
            let events = client.take_events();
            assert!(!client.is_closed(), "pattern {pattern:?}: {events:?}");
            let texts = events
                .iter()
                .filter_map(|event| match event {
                    Event::Text { body, .. } => Some(body.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(texts, [body.as_str(), "healthy tail"]);
        }
    }
    #[test]
    fn incoming_tls_four_oversized_frames_do_not_hide_following_healthy_text() {
        let (mut client, mut server) = incoming_tls_peer();
        let mut plain = Vec::new();
        let oversized = format!("<p>{}</p>", "x".repeat(MAX_CONTENT_BYTES));
        for _ in 0..4 {
            plain.extend(incoming_text(&oversized));
        }
        plain.extend(incoming_text("HEALTHY_AFTER_OVERSIZE"));
        let encrypted = encrypt_incoming(&mut server, &plain, false);
        deliver_incoming(&mut client, &encrypted, &[tunnel::MAX_STREAM]);
        let events = client.take_events();
        assert!(!client.is_closed(), "{events:?}");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::Server(ServerEvent::Notice(_))))
                .count(),
            1
        );
        let texts = events
            .iter()
            .filter_map(|event| match event {
                Event::Text { body, .. } => Some(body.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(texts, ["HEALTHY_AFTER_OVERSIZE"]);
    }
    #[test]
    fn incoming_tls_close_notify_keeps_final_complete_plaintext_in_order() {
        let (mut client, mut server) = incoming_tls_peer();
        let mut plain = incoming_text(&"x".repeat(96 * 1024));
        plain.extend(incoming_text("last complete message"));
        let encrypted = encrypt_incoming(&mut server, &plain, true);
        deliver_incoming(&mut client, &encrypted, &[tunnel::MAX_STREAM]);
        let events = client.take_events();
        assert!(client.is_closed());
        assert!(
            matches!(events.as_slice(), [Event::Text { .. }, Event::Text { body, .. }, Event::Closed { reason }] if body == "last complete message" && reason == "The server closed the connection.")
        );
    }
    #[test]
    fn incoming_tls_close_notify_never_admits_a_partial_control_frame() {
        let (mut client, mut server) = incoming_tls_peer();
        let mut plain = incoming_text("complete");
        let mut partial = incoming_text("unfinished");
        partial.truncate(partial.len() - 3);
        plain.extend(partial);
        let encrypted = encrypt_incoming(&mut server, &plain, true);
        deliver_incoming(&mut client, &encrypted, &[tunnel::MAX_STREAM]);
        assert!(client.is_closed());
        assert!(matches!(client.take_events().as_slice(),
            [Event::Text { body, .. }, Event::Closed { reason }]
            if body == "complete" && reason == "The server closed the connection."));
    }
    #[test]
    fn incoming_tls_event_limit_still_closes_without_unbounded_admission() {
        let (mut client, mut server) = incoming_tls_peer();
        let mut plain = Vec::new();
        for _ in 0..=MAX_EVENTS {
            plain.extend(incoming_text("bounded"));
        }
        let encrypted = encrypt_incoming(&mut server, &plain, false);
        deliver_incoming(&mut client, &encrypted, &[tunnel::MAX_STREAM]);
        assert!(client.is_closed());
        let events = client.take_events();
        assert_eq!(events.len(), MAX_EVENTS + 1);
        assert!(matches!(events.last(), Some(Event::Closed { reason })
            if reason == "The server sent too much text at once."));
    }
    #[test]
    fn incoming_tls_partial_record_and_tamper_keep_existing_failure_policy() {
        let (mut client, mut server) = incoming_tls_peer();
        let encrypted = encrypt_incoming(
            &mut server,
            &incoming_text("complete after partial header"),
            false,
        );
        deliver_incoming(&mut client, &encrypted[..1], &[1]);
        client.tls_in(&[]);
        assert!(!client.is_closed());
        assert!(client.take_events().is_empty());
        deliver_incoming(&mut client, &encrypted[1..], &[3, 127]);
        assert!(
            matches!(client.take_events().as_slice(), [Event::Text { body, .. }] if body == "complete after partial header")
        );
        let plain = incoming_text(&"x".repeat(24 * 1024));
        let mut encrypted = encrypt_incoming(&mut server, &plain, false);
        *encrypted.last_mut().unwrap() ^= 1;
        assert!(encrypted.len() < tunnel::MAX_STREAM);
        deliver_incoming(&mut client, &encrypted, &[tunnel::MAX_STREAM]);
        assert!(client.is_closed());
        assert!(
            matches!(client.take_events().as_slice(), [Event::Closed { reason }] if reason.starts_with("TLS failed:"))
        );
    }

    #[test]
    fn private_ca_fullchain_and_leaf_renewal_require_no_pin_or_prompt() {
        let root = pki_authority();
        let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        p.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        let intermediate =
            rcgen::CertifiedIssuer::signed_by(p, rcgen::KeyPair::generate().unwrap(), &root)
                .unwrap();
        let expected = nuschl_core::bookmark::webpki_history_identity("localhost", 64738);
        let mut previous = None;
        for _ in 0..2 {
            let (leaf, key) = pki_leaf(&intermediate, "localhost", false);
            let (mut session, frames) = pki_handshake(
                None,
                root.der().clone(),
                vec![leaf.clone(), intermediate.der().clone()],
                key,
            );
            assert!(frames.contains(&kind::AUTHENTICATE));
            assert_eq!(session.history_identity(), Some(expected.clone()));
            let events = session.take_events();
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, Event::Certificate { .. }))
            );
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, Event::Server(ServerEvent::PeerCertificate(_))))
            );
            let current = hex(&Sha256::digest(leaf.as_ref()));
            if let Some(old) = previous {
                assert_ne!(old, current);
            }
            previous = Some(current);
        }
        for (host, expired, include_intermediate) in [
            ("wrong.test", false, true),
            ("localhost", true, true),
            ("localhost", false, false),
        ] {
            let (leaf, key) = pki_leaf(&intermediate, host, expired);
            let mut chain = vec![leaf];
            if include_intermediate {
                chain.push(intermediate.der().clone());
            }
            let (mut session, frames) = pki_handshake(None, root.der().clone(), chain, key);
            assert!(session.is_closed());
            assert!(session.history_identity().is_none());
            assert!(!frames.contains(&kind::AUTHENTICATE));
            assert!(
                session
                    .take_events()
                    .iter()
                    .any(|e| matches!(e, Event::Certificate { .. }))
            );
        }
        let other = pki_authority();
        let (leaf, key) = pki_leaf(&other, "localhost", false);
        let (mut session, frames) = pki_handshake(None, root.der().clone(), vec![leaf], key);
        assert!(session.is_closed());
        assert!(!frames.contains(&kind::AUTHENTICATE));
        assert!(
            session
                .take_events()
                .iter()
                .any(|e| matches!(e, Event::Certificate { .. }))
        );
        let (leaf, key) = pki_leaf(&intermediate, "localhost", false);
        let (mut session, frames) = pki_handshake(
            Some("00".repeat(32)),
            root.der().clone(),
            vec![leaf, intermediate.der().clone()],
            key,
        );
        assert!(session.is_closed());
        assert!(!frames.contains(&kind::AUTHENTICATE));
        assert!(
            session
                .take_events()
                .iter()
                .any(|e| matches!(e, Event::Certificate { changed: true, .. }))
        );
    }

    #[test]
    fn certificate_identity_requires_completed_tls_not_just_chain_verification() {
        let mut session = session();
        *session.verifier.seen.lock().unwrap() = Some(("ab".repeat(32), false));
        session.report_certificate();
        assert!(session.take_events().is_empty());
        assert!(session.history_identity().is_none());
        assert!(!session.certificate_reported);
        *session.verifier.seen.lock().unwrap() = Some(("ab".repeat(32), true));
        session.report_certificate();
        assert!(matches!(
            session.take_events().first(),
            Some(Event::Certificate { .. })
        ));
    }

    #[test]
    fn oversized_incoming_text_is_refused_once_without_blocking_valid_messages() {
        let mut session = session();
        let wire = |body: String| {
            proto::TextMessage {
                channel_id: vec![0],
                message: body,
                ..Default::default()
            }
            .encode_to_vec()
            .into()
        };
        // Unauthenticated input neither admits a row nor consumes the notice.
        session.control(kind::TEXT_MESSAGE, wire("x".repeat(MAX_CONTENT_BYTES + 1)));
        assert!(!session.oversized_text_reported);
        assert!(session.take_events().is_empty());
        session.phase = Phase::LoggedIn;
        session.channels.insert(
            0,
            Channel {
                id: 0,
                ..Default::default()
            },
        );
        for body in [
            "a".repeat(MAX_CONTENT_BYTES),
            "🙂".repeat(MAX_CONTENT_BYTES / 4),
        ] {
            session.control(kind::TEXT_MESSAGE, wire(body.clone()));
            assert!(
                matches!(session.take_events().as_slice(), [Event::Text { body: received, .. }] if received == &body)
            );
        }
        // The bound is UTF-8 BYTES, not Unicode scalar or UTF-16 length.
        let large = "🙂".repeat(MAX_CONTENT_BYTES / 4 + 1);
        for _ in 0..32 {
            session.control(kind::TEXT_MESSAGE, wire(large.clone()));
        }
        assert!(
            matches!(session.take_events().as_slice(), [Event::Server(ServerEvent::Notice(text))] if text == "Some incoming messages exceeded the local display limit and were not shown.")
        );
        assert!(!session.is_closed());
        session.control(kind::TEXT_MESSAGE, wire("healthy following message".into()));
        assert!(
            matches!(session.take_events().as_slice(), [Event::Text { body, .. }] if body == "healthy following message")
        );
        let fresh = super::Session::new(Login {
            host: "localhost".into(),
            port: 64738,
            username: "fresh".into(),
            password: None,
            tokens: vec![],
            pin: None,
        })
        .unwrap();
        assert!(
            !fresh.oversized_text_reported,
            "the one-time suppression belongs to one connection"
        );
    }

    #[test]
    fn text_fanout_is_bounded_before_body_clones_accumulate() {
        let mut session = session();
        session.phase = Phase::LoggedIn;
        for id in 0..600 {
            session.channels.insert(
                id,
                Channel {
                    id,
                    ..Default::default()
                },
            );
        }
        let text = proto::TextMessage {
            channel_id: (0..600).collect(),
            message: "bounded".into(),
            ..Default::default()
        };
        session.control(kind::TEXT_MESSAGE, text.encode_to_vec().into());
        assert!(session.is_closed());
        assert!(session.take_events().len() <= MAX_EVENTS + 1);
    }

    #[test]
    fn legacy_text_routes_known_channels_and_tree_targets_without_cycles() {
        let mut session = session();
        session.phase = Phase::LoggedIn;
        session.own = Some(1);
        session.channels.insert(
            0,
            Channel {
                id: 0,
                ..Default::default()
            },
        );
        session.channels.insert(
            2,
            Channel {
                id: 2,
                parent: Some(0),
                ..Default::default()
            },
        );
        session.users.insert(
            1,
            User {
                session: 1,
                channel: 2,
                ..Default::default()
            },
        );
        session.users.insert(
            3,
            User {
                session: 3,
                name: "Ada".into(),
                instance: 4,
                ..Default::default()
            },
        );
        let text = proto::TextMessage {
            actor: Some(3),
            channel_id: vec![0, 0, 999],
            tree_id: vec![0],
            message: "one<br><br>two".into(),
            ..Default::default()
        };
        session.control(kind::TEXT_MESSAGE, text.encode_to_vec().into());
        let events = session.take_events();
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| matches!(event, Event::Text { author, html: true, outgoing: false, body, .. } if author == "Ada" && body == "one<br><br>two")));
        let unknown = proto::TextMessage {
            actor: Some(3),
            message: "private".into(),
            ..Default::default()
        };
        session.control(kind::TEXT_MESSAGE, unknown.encode_to_vec().into());
        assert!(matches!(
            session.take_events().first(),
            Some(Event::Text {
                target: TextTarget::Direct(3),
                ..
            })
        ));
        session.channels.get_mut(&2).unwrap().parent = Some(2);
        let cycle = proto::TextMessage {
            actor: Some(3),
            tree_id: vec![0],
            message: "bounded".into(),
            ..Default::default()
        };
        session.control(kind::TEXT_MESSAGE, cycle.encode_to_vec().into());
        assert_eq!(session.take_events().len(), 1);
    }
    #[test]
    fn text_sends_refuse_unconnected_unknown_durable_and_reused_recipients() {
        let mut session = session();
        assert!(
            session
                .send_text(TextTarget::Channel(0), "hi".into(), None)
                .is_err()
        );
        session.phase = Phase::LoggedIn;
        session.users.insert(
            3,
            User {
                session: 3,
                instance: 8,
                ..Default::default()
            },
        );
        assert!(
            session
                .send_text(TextTarget::Channel(999), "hi".into(), None)
                .is_err()
        );
        assert!(
            session
                .send_text(TextTarget::Direct(3), "hi".into(), Some(7))
                .is_err()
        );
        assert!(
            session
                .send_text(TextTarget::Direct(3), "hi".into(), None)
                .is_err()
        );
        assert!(
            session
                .send_text(TextTarget::DirectConversation([0; 16]), "hi".into(), None)
                .is_err()
        );
        assert!(
            session
                .send_text(TextTarget::Direct(3), " ".into(), Some(8))
                .is_err()
        );
        assert!(
            session
                .send_text(
                    TextTarget::Direct(3),
                    "x".repeat(MAX_CONTENT_BYTES + 1),
                    Some(8)
                )
                .is_err()
        );
        assert!(session.take_events().is_empty());
    }

    #[test]
    fn presence_tokens_are_stable_for_updates_and_unique_across_connections() {
        let mut first = session();
        let mut second = session();
        let update = |hash: &str| {
            proto::UserState {
                session: Some(7),
                hash: Some(hash.into()),
                ..Default::default()
            }
            .encode_to_vec()
            .into()
        };
        first.control(kind::USER_STATE, update("abc"));
        let before = first.users[&7].instance;
        first.control(kind::USER_STATE, update("abc"));
        assert_eq!(before, first.users[&7].instance);
        second.control(kind::USER_STATE, update("abc"));
        assert_ne!(before, second.users[&7].instance);
        first.control(kind::USER_STATE, update("different"));
        assert_ne!(before, first.users[&7].instance);
    }

    #[test]
    fn a_sparse_deafen_snapshot_also_means_muted() {
        let mut session = session();
        session.control(
            kind::USER_STATE,
            proto::UserState {
                session: Some(7),
                self_deaf: Some(true),
                ..Default::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert!(session.users[&7].self_muted && session.users[&7].self_deafened);
    }
}
