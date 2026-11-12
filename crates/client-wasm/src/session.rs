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
    Opened {
        udp: bool,
    },
    /// What the SDK models.
    Server(ServerEvent),
    /// Bounded legacy welcome markup; rendered only in the SDK worker.
    Welcome(String),
    /// The certificate is neither pinned nor vouched for: the user decides.
    Certificate {
        sha256: String,
        changed: bool,
    },
    /// Bounded legacy text. The platform renders and persists it before
    /// reporting a Message to the shared SDK; the relay never sees plaintext.
    Text {
        target: TextTarget,
        peer: Option<User>,
        author: String,
        body: String,
        html: bool,
        outgoing: bool,
    },
    /// Bounded encoded Opus; decoding belongs to the separate audio worker.
    Voice(nuschl_protocol::voice::VoicePacket),
    Sent {
        target: TextTarget,
        body: String,
    },
    /// The connection is over.
    Closed {
        reason: String,
    },
}

/// Mumble's server drops a client that is silent for 30 seconds.
const PING_MS: f64 = 10_000.0;
/// Plaintext waiting to be framed; a server sending more is cut off.
const MAX_BUFFER: usize = 16 * 1024 * 1024;
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
    events: VecDeque<Event>,
    channels: BTreeMap<u32, Channel>,
    users: BTreeMap<u32, User>,
    own: Option<u32>,
    welcome_source: Option<String>,
    last_ping: f64,
    certificate_reported: bool,
    allow_html: bool,
    voice_version: u64,
    opus_allowed: bool,
    max_bandwidth: u32,
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
            outgoing: vec![open],
            events: VecDeque::new(),
            channels: BTreeMap::new(),
            users: BTreeMap::new(),
            own: None,
            welcome_source: None,
            last_ping: 0.0,
            certificate_reported: false,
            allow_html: true,
            voice_version: 0,
            opus_allowed: true,
            max_bandwidth: nuschl_core::audio::DEFAULT_SERVER_BANDWIDTH,
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
        std::mem::take(&mut self.outgoing)
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

    /// Effective mono target for this authenticated connection, including the
    /// conservative server-accounting overhead reserve.
    pub fn voice_bitrate(&self) -> u32 {
        nuschl_core::audio::voice_bitrate(self.max_bandwidth)
    }

    pub fn send_voice(&mut self, sequence: u64, opus: Vec<u8>, end: bool) -> Result<(), String> {
        if self.voice_policy().0
            || self.voice_bitrate() == 0
            || opus.len() > (self.voice_bitrate() / 400) as usize
        {
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
        if self.tls.writer().write_all(&frame).is_err() {
            self.close("TLS failed.".into());
            return Err("TLS failed.".into());
        }
        self.flush_tls();
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
        // Escape text for legacy HTML-capable peers, preserving every newline.
        wire.message = if self.allow_html {
            body.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('\n', "<br>")
        } else {
            body.clone()
        };
        self.send(kind::TEXT_MESSAGE, &wire);
        if self.is_closed() {
            return Err("TLS failed while sending.".into());
        }
        let peer = match target {
            TextTarget::Direct(id) => self.users.get(&id).cloned(),
            _ => None,
        };
        self.events.push_back(Event::Text {
            target: target.clone(),
            peer,
            author: self.login.username.clone(),
            body: body.clone(),
            html: false,
            outgoing: true,
        });
        self.events.push_back(Event::Sent { target, body });
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
        }
        if self.phase == Phase::LoggingIn
            && !self.tls.is_handshaking()
            && !self.certificate_reported
        {
            self.report_certificate();
            self.log_in();
        }
        let mut chunk = [0_u8; 16 * 1024];
        loop {
            match self.tls.reader().read(&mut chunk) {
                Ok(0) => return self.close("The server closed the connection.".into()),
                Ok(size) => {
                    if self.plaintext.len() + size > MAX_BUFFER {
                        return self.close("The server sent too much at once.".into());
                    }
                    self.plaintext.extend_from_slice(&chunk[..size]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return self.close("TLS failed.".into()),
            }
        }
        self.frames();
        self.flush_tls();
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
        let Some(frame) = encode_control(kind, message) else {
            return;
        };
        if self.tls.writer().write_all(&frame).is_err() {
            return self.close("TLS failed.".into());
        }
        self.flush_tls();
    }

    fn flush_tls(&mut self) {
        while self.tls.wants_write() {
            let mut bytes = Vec::new();
            if self.tls.write_tls(&mut bytes).is_err() {
                return self.close("TLS failed.".into());
            }
            for chunk in bytes.chunks(tunnel::MAX_STREAM) {
                if let Some(frame) = tunnel::encode(&Frame::Stream(chunk)) {
                    self.outgoing.push(frame);
                }
            }
        }
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
                }
            }
            kind::UDP_TUNNEL => {
                if self.phase != Phase::LoggedIn || self.voice_policy().1 || !self.opus_allowed {
                    return;
                }
                if let Ok(packet) = nuschl_protocol::voice::decode_voice(&payload) {
                    if self.users.contains_key(&packet.sender) && self.events.len() < MAX_EVENTS {
                        self.events.push_back(Event::Voice(packet));
                    }
                }
            }
            kind::SERVER_SYNC => {
                let Ok(sync) = proto::ServerSync::decode(payload) else {
                    return;
                };
                let Some(session) = sync.session else {
                    return self.close("The server omitted our session number.".into());
                };
                self.max_bandwidth = sync
                    .max_bandwidth
                    .unwrap_or(nuschl_core::audio::DEFAULT_SERVER_BANDWIDTH);
                self.phase = Phase::LoggedIn;
                self.own = Some(session);
                self.events
                    .push_back(Event::Server(ServerEvent::Connected { session }));
                if let Some(source) = sync.welcome_text {
                    self.welcome(source);
                }
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
                    if config.max_bandwidth.is_none()
                        && config.allow_html.is_none()
                        && config.message_length.is_none()
                        && config.image_message_length.is_none()
                        && config.max_users.is_none()
                        && config.recording_allowed.is_none()
                    {
                        return;
                    }
                    if let Some(html) = config.allow_html {
                        self.allow_html = html;
                    }
                    if let Some(bandwidth) = config.max_bandwidth {
                        self.max_bandwidth = bandwidth;
                    }
                    self.events.push_back(Event::Server(ServerEvent::Limits(
                        nuschl_core::model::ServerLimits {
                            allow_html: self.allow_html,
                            message_length: config
                                .message_length
                                .filter(|n| *n != 0)
                                .map(|n| n as usize)
                                .unwrap_or(MAX_CONTENT_BYTES)
                                .min(MAX_CONTENT_BYTES),
                            image_message_length: config
                                .image_message_length
                                .filter(|n| *n != 0)
                                .map(|n| n as usize)
                                .unwrap_or(1024 * 1024)
                                .min(1024 * 1024),
                            max_users: config.max_users,
                            recording_allowed: config.recording_allowed.unwrap_or(true),
                            max_bandwidth: Some(self.max_bandwidth),
                        },
                    )));
                }
            }
            kind::PERMISSION_QUERY => {
                if let Ok(query) = proto::PermissionQuery::decode(payload) {
                    if let (Some(channel), Some(permissions)) =
                        (query.channel_id, query.permissions)
                    {
                        self.events
                            .push_back(Event::Server(ServerEvent::Permissions {
                                channel,
                                permissions: permissions as u32,
                                flush: query.flush.unwrap_or(false),
                            }));
                    }
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
                if self.phase != Phase::LoggedIn || message.message.len() > MAX_CONTENT_BYTES {
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
                    self.events.push_back(Event::Text {
                        target,
                        peer: peer.clone(),
                        author: author.clone(),
                        body: message.message.clone(),
                        html: self.allow_html,
                        outgoing: false,
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
            s.max_bandwidth,
            s.allow_html,
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
                    s.max_bandwidth,
                    s.allow_html,
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
        assert!(!s.allow_html);
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
        (client, frames)
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
