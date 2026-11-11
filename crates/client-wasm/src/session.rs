// SPDX-License-Identifier: EUPL-1.2
//! A Mumble connection through the web relay's tunnel, without I/O
//! (docs/web.md, "The web client's network path"): WebSocket messages in,
//! WebSocket messages and events out. TLS runs here (rustls), so the relay
//! sees only ciphertext, and the server's certificate is checked here: against
//! the saved pin, or the web's certificate authorities, as the apps do.
//!
//! The browser glue (`browser.rs`) moves bytes; tests drive this directly.
//! For now it logs in and keeps the channel tree and the users; text, voice
//! and the Nuschl extension come with the web client.

use bytes::BytesMut;
use nuschl_client::input::ServerEvent;
use nuschl_core::model::{Channel, User};
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
    /// The certificate is neither pinned nor vouched for: the user decides.
    Certificate { sha256: String, changed: bool },
    /// The connection is over.
    Closed { reason: String },
}

/// Mumble's server drops a client that is silent for 30 seconds.
const PING_MS: f64 = 10_000.0;
/// Plaintext waiting to be framed; a server sending more is cut off.
const MAX_BUFFER: usize = 16 * 1024 * 1024;

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
            match decode_control(&mut self.plaintext) {
                Ok(Some(frame)) => self.control(frame.kind, frame.payload),
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
                let session = sync.session.unwrap_or_default();
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
