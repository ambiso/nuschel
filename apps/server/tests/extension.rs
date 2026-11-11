// SPDX-License-Identifier: EUPL-1.2
//! The Nuschel extension (docs/extension.md) on loopback: the real client
//! session (`nuschel-session`) and a raw protocol client against `nuscheld`,
//! and the client against fake servers that answer weaker than they should.
use bytes::BytesMut;
use nuschel_protocol::{
    ControlFrame, control as proto, decode_control, encode_control, kind,
    nuschel::{self, v1},
};
use nuschel_session::{
    Error as SessionError,
    config::{ClientIdentity, ServerBookmark},
    connection::{self, Command, ConnectOptions, SessionEvent},
    sticky::Requirement,
};
use nuscheld::{
    config::Config,
    server::{Running, start},
    storage::{Backend, MemoryStore},
};
use prost::Message;
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, TlsConnector, client::TlsStream};

const WAIT: Duration = Duration::from_secs(10);

// ---- the server ----

struct Fixture {
    running: Running,
    store: Arc<Mutex<MemoryStore>>,
    directory: tempfile::TempDir,
}

impl Fixture {
    async fn start(extra: &str) -> Self {
        Self::start_in(tempfile::tempdir().unwrap(), MemoryStore::shared(), extra).await
    }

    async fn start_in(
        directory: tempfile::TempDir,
        store: Arc<Mutex<MemoryStore>>,
        extra: &str,
    ) -> Self {
        let config = Config::from_toml(&format!(
            "bind = [\"127.0.0.1\"]\nport = 0\nstate_directory = {:?}\n\
             message_limit = 1000\nmessage_burst = 1000\n{extra}\n\
             [autoban]\nattempts = 0\n[password_hashing]\nmemory_kib = 64\npasses = 1\n",
            directory.path(),
        ))
        .unwrap();
        let running = start(config.settings().unwrap(), Backend::Memory(store.clone()))
            .await
            .unwrap();
        Self {
            running,
            store,
            directory,
        }
    }

    fn address(&self) -> SocketAddr {
        self.running.addresses[0]
    }

    fn users(&self) -> usize {
        self.running.server.lock().users.len()
    }

    async fn restart(self, extra: &str) -> Self {
        let Self {
            running,
            store,
            directory,
        } = self;
        running.shutdown().await;
        Self::start_in(directory, store, extra).await
    }
}

// ---- the real client session ----

/// Generating RSA identities is slow; tests share a few.
fn session_identity(index: usize) -> ClientIdentity {
    static IDENTITIES: OnceLock<Vec<ClientIdentity>> = OnceLock::new();
    IDENTITIES.get_or_init(|| {
        (0..4)
            .map(|_| ClientIdentity::generate().unwrap())
            .collect()
    })[index]
        .clone()
}

struct Session {
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<SessionEvent>,
    task: tokio::task::JoinHandle<Result<(), SessionError>>,
    pub session: u32,
}

fn options(address: SocketAddr, pin: &str, name: &str, identity: usize) -> ConnectOptions {
    let mut server =
        ServerBookmark::new("", &address.ip().to_string(), address.port(), name).unwrap();
    server.certificate_sha256 = Some(pin.to_owned());
    ConnectOptions {
        server,
        identity: session_identity(identity),
        password: None,
        force_tcp: true,
        muted: true,
        deafened: false,
        channel: None,
        message_rate: Default::default(),
        required: Requirement::default(),
    }
}

impl Session {
    fn spawn(options: ConnectOptions) -> Self {
        let (commands, receiver) = mpsc::channel(32);
        let (sender, events) = mpsc::channel(256);
        let task = tokio::spawn(connection::run(options, receiver, sender));
        Self {
            commands,
            events,
            task,
            session: 0,
        }
    }

    /// Connect and wait for the login to complete; returns the negotiation.
    async fn connect(options: ConnectOptions) -> (Self, Option<connection::Negotiated>) {
        let mut session = Self::spawn(options);
        let mut negotiated = None;
        loop {
            match session.next().await {
                Some(SessionEvent::Negotiated(value)) => negotiated = Some(value),
                Some(SessionEvent::Connected { session: id, .. }) => {
                    session.session = id;
                    return (session, negotiated);
                }
                Some(_) => {}
                None => {
                    let result = session.task.await.unwrap();
                    panic!("the session ended before logging in: {result:?}");
                }
            }
        }
    }

    async fn next(&mut self) -> Option<SessionEvent> {
        timeout(WAIT, self.events.recv()).await.ok().flatten()
    }

    /// The error a session ended with.
    async fn failure(mut self) -> SessionError {
        while self.next().await.is_some() {}
        timeout(WAIT, self.task)
            .await
            .expect("the session did not end")
            .unwrap()
            .expect_err("the session should fail")
    }

    /// Wait for an event matching `pick`.
    async fn wait<T>(&mut self, mut pick: impl FnMut(SessionEvent) -> Option<T>) -> T {
        loop {
            let event = self.next().await.expect("no matching session event");
            if let Some(value) = pick(event) {
                return value;
            }
        }
    }

    async fn command(&self, command: Command) {
        self.commands.send(command).await.unwrap();
    }
}

// ---- a raw protocol client, for what the session never does ----

#[derive(Debug)]
struct AnyServer(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AnyServer {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn certificate() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    (
        generated.cert.der().clone(),
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            generated.signing_key.serialize_der(),
        )),
    )
}

struct Raw {
    reader: ReadHalf<TlsStream<TcpStream>>,
    writer: WriteHalf<TlsStream<TcpStream>>,
    buffer: BytesMut,
    alpn: Option<Vec<u8>>,
}

impl Raw {
    async fn connect(address: SocketAddr, alpn: &[&[u8]]) -> Self {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let (certificate, key) = certificate();
        let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyServer(provider)))
            .with_client_auth_cert(vec![certificate], key)
            .unwrap();
        config.alpn_protocols = alpn.iter().map(|value| value.to_vec()).collect();
        let stream = TcpStream::connect(address).await.unwrap();
        let tls = TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from("localhost").unwrap(), stream)
            .await
            .unwrap();
        let alpn = tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
        let (reader, writer) = tokio::io::split(tls);
        Self {
            reader,
            writer,
            buffer: BytesMut::new(),
            alpn,
        }
    }

    async fn send_bytes(&mut self, frame: &[u8]) {
        let _ = self.writer.write_all(frame).await;
        let _ = self.writer.flush().await;
    }

    async fn send(&mut self, kind: u16, message: &impl Message) {
        self.send_bytes(&encode_control(kind, message).unwrap())
            .await;
    }

    async fn version(&mut self, capabilities: Option<&[&str]>) {
        let hello = capabilities.map(|capabilities| v1::Hello {
            protocol: nuschel::PROTOCOL,
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
            policy: None,
        });
        let frame = nuschel::version_frame(
            &proto::Version {
                version_v2: Some((1 << 48) | (5 << 32)),
                ..Default::default()
            },
            hello.as_ref(),
        )
        .unwrap();
        self.send_bytes(&frame).await;
    }

    async fn authenticate(&mut self, name: &str) {
        self.send(
            kind::AUTHENTICATE,
            &proto::Authenticate {
                username: Some(name.into()),
                opus: Some(true),
                ..Default::default()
            },
        )
        .await;
    }

    async fn next(&mut self) -> Option<ControlFrame> {
        loop {
            if let Ok(Some(frame)) = decode_control(&mut self.buffer) {
                return Some(frame);
            }
            match timeout(WAIT, self.reader.read_buf(&mut self.buffer)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return None,
                Ok(Ok(_)) => {}
            }
        }
    }

    /// Frames until `kind` (`None` if the connection closes first).
    async fn until(&mut self, kind: u16) -> Option<ControlFrame> {
        loop {
            let frame = self.next().await?;
            if frame.kind == kind {
                return Some(frame);
            }
        }
    }

    /// Whether the server closes the connection (rather than logging in).
    async fn closed(&mut self) -> bool {
        loop {
            match self.next().await {
                None => return true,
                Some(frame) if frame.kind == kind::SERVER_SYNC => return false,
                Some(_) => {}
            }
        }
    }
}

// ---- a fake server that answers as configured ----

/// Accepts one TLS connection with the given ALPN choice, sends `version`,
/// and reports every frame type the client sends until it closes.
async fn fake_server(
    alpn: Option<&'static [u8]>,
    version: bytes::Bytes,
) -> (SocketAddr, String, tokio::task::JoinHandle<Vec<u16>>) {
    let (certificate, key) = certificate();
    let pin = nuschel_session::config::fingerprint(certificate.as_ref());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .unwrap();
    // Without ALPN configured, rustls ignores the client's offer (as Murmur does).
    config.alpn_protocols = alpn.map(|value| vec![value.to_vec()]).unwrap_or_default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut tls = TlsAcceptor::from(Arc::new(config))
            .accept(stream)
            .await
            .unwrap();
        // The client may already have hung up (that is the point of some tests).
        let _ = tls.write_all(&version).await;
        let _ = tls.flush().await;
        let mut buffer = BytesMut::new();
        let mut kinds = Vec::new();
        loop {
            while let Ok(Some(frame)) = decode_control(&mut buffer) {
                kinds.push(frame.kind);
            }
            match timeout(Duration::from_secs(3), tls.read_buf(&mut buffer)).await {
                Ok(Ok(n)) if n > 0 => {}
                _ => return kinds,
            }
        }
    });
    (address, pin, task)
}

fn stock_version() -> bytes::Bytes {
    nuschel::version_frame(
        &proto::Version {
            version_v2: Some((1 << 48) | (5 << 32)),
            release: Some("Murmur".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap()
}

fn hello_version(capabilities: &[&str]) -> bytes::Bytes {
    nuschel::version_frame(
        &proto::Version {
            version_v2: Some((1 << 48) | (5 << 32)),
            ..Default::default()
        },
        Some(&v1::Hello {
            protocol: nuschel::PROTOCOL,
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
            policy: None,
        }),
    )
    .unwrap()
}

fn requirement(nuschel: bool, capabilities: &[&str]) -> Requirement {
    Requirement {
        nuschel,
        capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
    }
}

// ---- §1 negotiation ----

#[tokio::test]
async fn the_session_negotiates_the_extension_with_nuscheld() {
    let fixture = Fixture::start("").await;
    let (session, negotiated) = Session::connect(options(
        fixture.address(),
        &fixture.running.fingerprint,
        "Alice",
        0,
    ))
    .await;
    let negotiated = negotiated.expect("negotiated");
    assert!(negotiated.nuschel);
    assert_eq!(
        negotiated.capabilities,
        ["history/1", "subscribe/1", "typing/1"]
    );
    let policy = negotiated.policy.unwrap();
    assert_eq!(policy.guests, v1::policy::Guests::None as i32);
    assert_eq!(policy.retention_days, 0, "forever by default");
    {
        let state = fixture.running.server.lock();
        let user = state.users.get(&session.session).unwrap();
        assert!(user.extension.as_ref().unwrap().has("history/1"));
    }
    // A record from this negotiation is met on the next connection.
    let mut again = options(fixture.address(), &fixture.running.fingerprint, "Bob", 1);
    again.required = requirement(true, &["history/1", "subscribe/1", "typing/1"]);
    Session::connect(again).await;
}

#[tokio::test]
async fn a_record_the_server_no_longer_meets_fails_before_authenticating() {
    let fixture = Fixture::start("").await;
    let mut wanting = options(fixture.address(), &fixture.running.fingerprint, "Alice", 0);
    wanting.required = requirement(true, &["mls/1"]);
    let error = Session::spawn(wanting).failure().await;
    assert!(
        matches!(&error, SessionError::Downgrade(text) if text.contains("mls/1")),
        "{error:?}"
    );
    assert!(!error.transient(), "no automatic retry, never a fallback");
    assert_eq!(fixture.users(), 0, "no credentials were sent");
}

#[tokio::test]
async fn a_stock_answer_to_a_sticky_client_sends_nothing() {
    // A server (or an impersonator) that ignores ALPN, like stock Murmur.
    let (address, pin, received) = fake_server(None, stock_version()).await;
    let mut sticky = options(address, &pin, "Alice", 0);
    sticky.required = requirement(true, &[]);
    let error = Session::spawn(sticky).failure().await;
    assert!(matches!(error, SessionError::Downgrade(_)), "{error:?}");
    assert!(
        received.await.unwrap().is_empty(),
        "not even Version is sent"
    );
}

#[tokio::test]
async fn a_stock_server_is_fine_without_a_record() {
    let (address, pin, received) = fake_server(None, stock_version()).await;
    let session = Session::spawn(options(address, &pin, "Alice", 0));
    // The fake never completes the login; it only records what arrives.
    let kinds = received.await.unwrap();
    assert_eq!(
        &kinds[..2],
        [kind::VERSION, kind::AUTHENTICATE],
        "stock mode: no waiting for the server"
    );
    session.task.abort();
}

#[tokio::test]
async fn alpn_without_a_hello_or_with_fewer_capabilities_never_authenticates() {
    // ALPN nuschel/1 with a plain Version: a protocol error, not stock mode.
    let (address, pin, received) = fake_server(Some(nuschel::ALPN), stock_version()).await;
    let error = Session::spawn(options(address, &pin, "Alice", 0))
        .failure()
        .await;
    assert!(matches!(error, SessionError::Protocol(_)), "{error:?}");
    assert_eq!(received.await.unwrap(), [kind::VERSION]);
    // A hello lacking a remembered capability: a downgrade.
    let (address, pin, received) =
        fake_server(Some(nuschel::ALPN), hello_version(&["typing/1"])).await;
    let mut sticky = options(address, &pin, "Alice", 0);
    sticky.required = requirement(true, &["history/1"]);
    let error = Session::spawn(sticky).failure().await;
    assert!(matches!(error, SessionError::Downgrade(_)), "{error:?}");
    assert_eq!(received.await.unwrap(), [kind::VERSION]);
    // A malformed hello fails closed too.
    let (address, pin, received) =
        fake_server(Some(nuschel::ALPN), hello_version(&["NOT VALID"])).await;
    let error = Session::spawn(options(address, &pin, "Alice", 0))
        .failure()
        .await;
    assert!(matches!(error, SessionError::Protocol(_)), "{error:?}");
    assert_eq!(received.await.unwrap(), [kind::VERSION]);
}

#[tokio::test]
async fn the_server_fails_closed_on_mismatched_negotiation() {
    let fixture = Fixture::start("").await;
    // ALPN nuschel/1 but no hello: closed at Authenticate, nobody logged in.
    let mut raw = Raw::connect(fixture.address(), &[nuschel::ALPN, nuschel::ALPN_MUMBLE]).await;
    assert_eq!(raw.alpn.as_deref(), Some(nuschel::ALPN));
    let version = raw.until(kind::VERSION).await.unwrap();
    assert!(
        nuschel::hello(&version.payload).unwrap().is_some(),
        "the server's hello"
    );
    raw.version(None).await;
    raw.authenticate("NoHello").await;
    assert!(raw.closed().await);
    // A malformed hello closes the connection.
    let mut raw = Raw::connect(fixture.address(), &[nuschel::ALPN]).await;
    raw.version(Some(&["Bad Capability"])).await;
    raw.authenticate("BadHello").await;
    assert!(raw.closed().await);
    // A hello without ALPN is ignored: a stock login, and no hello back.
    let mut raw = Raw::connect(fixture.address(), &[]).await;
    assert_eq!(raw.alpn, None);
    let version = raw.until(kind::VERSION).await.unwrap();
    assert_eq!(nuschel::hello(&version.payload).unwrap(), None);
    raw.version(Some(&["history/1"])).await;
    raw.authenticate("Stock").await;
    assert!(!raw.closed().await);
    let state = fixture.running.server.lock();
    assert_eq!(state.users.len(), 1);
    assert!(state.users.values().all(|user| user.extension.is_none()));
}

#[tokio::test]
async fn extension_frames_before_authentication_close_the_connection() {
    let fixture = Fixture::start("").await;
    let mut raw = Raw::connect(fixture.address(), &[nuschel::ALPN]).await;
    raw.version(Some(&["history/1"])).await;
    raw.send(
        nuschel::kind::SUBSCRIBE,
        &v1::Subscribe {
            request_id: 1,
            conversations: vec![vec![0; 16]],
        },
    )
    .await;
    raw.authenticate("Early").await;
    assert!(raw.closed().await);
    assert_eq!(fixture.users(), 0);
}
