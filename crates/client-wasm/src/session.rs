// SPDX-License-Identifier: EUPL-1.2
//! A Mumble connection through the web relay's tunnel, without I/O
//! (docs/web.md, "The web client's network path"): WebSocket messages in,
//! WebSocket messages and events out. TLS runs here (rustls), so the relay
//! sees only ciphertext, and the server's certificate is checked here: against
//! the saved pin, or the web's certificate authorities, as the apps do.
//!
//! The browser glue (`browser.rs`) moves bytes; tests drive this directly.
//! Legacy text and presence are supported. Voice, modern server archives and
//! MLS are deliberately not negotiated by this adapter.

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
    last_ping: f64,
    certificate_reported: bool,
    allow_html: bool,
}

impl Session {
    /// A session whose first message (the tunnel's `Open`) is waiting in
    /// [`Session::take_outgoing`].
    pub fn new(login: Login) -> Result<Self, String> {
        let open = tunnel::encode(&Frame::Open {
            host: &login.host,
            port: login.port,
        })
        .ok_or("the server's address is not valid")?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let roots = rustls::RootCertStore::from_iter(webpki_roots());
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
            last_ping: 0.0,
            certificate_reported: false,
            allow_html: true,
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
            // Voice comes later; datagrams are ignored until then.
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
                nuschl_core::bookmark::fingerprint(
                    format!("{}:{}:{pin}", self.login.host, self.login.port).as_bytes(),
                )
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
                self_mute: Some(true),
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
            kind::SERVER_SYNC => {
                let Ok(sync) = proto::ServerSync::decode(payload) else {
                    return;
                };
                let Some(session) = sync.session else {
                    return self.close("The server omitted our session number.".into());
                };
                self.phase = Phase::LoggedIn;
                self.own = Some(session);
                self.events
                    .push_back(Event::Server(ServerEvent::Connected { session }));
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
                    if let Some(html) = config.allow_html {
                        self.allow_html = html;
                    }
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
                if let Some(muted) = state.self_mute {
                    user.self_muted = muted;
                }
                if let Some(deafened) = state.self_deaf {
                    user.self_deafened = deafened;
                }
                user.self_muted |= user.self_deafened;
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
