// SPDX-License-Identifier: EUPL-1.2
//! The Nuschl extension (docs/extension.md) on loopback: the real client
//! session (`nuschl-session`) and a raw protocol client against `nuschld`,
//! and the client against fake servers that answer weaker than they should.
use bytes::BytesMut;
use nuschl_protocol::{
    ControlFrame, control as proto, decode_control, encode_control, kind,
    nuschl::{self, v1},
};
use nuschl_session::{
    Error as SessionError,
    config::{ClientIdentity, ServerBookmark},
    connection::{self, Command, ConnectOptions, NuschlEvent, NuschlRequest, SessionEvent},
    sticky::Requirement,
};
use nuschld::{
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
    collections::{BTreeSet, HashMap, VecDeque},
    net::SocketAddr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
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
    session: u32,
    /// Events read while waiting for something else.
    backlog: VecDeque<SessionEvent>,
    /// Channel → conversation id, from the extension fields of ChannelState.
    conversations: HashMap<u32, [u8; 16]>,
}

fn request_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn request_of(event: &NuschlEvent) -> Option<u64> {
    match event {
        NuschlEvent::Error(error) => Some(error.request_id),
        NuschlEvent::Ack(ack) => Some(ack.request_id),
        NuschlEvent::Page(page) => Some(page.request_id),
        NuschlEvent::Subscribed(subscribed) => Some(subscribed.request_id),
        NuschlEvent::DirectOpened(opened) => Some(opened.request_id),
        NuschlEvent::DirectList(list) => Some(list.request_id),
        NuschlEvent::Done(done) => Some(done.request_id),
        NuschlEvent::ShareLink(link) => Some(link.request_id),
        _ => None,
    }
}

fn latest() -> v1::history_request::Anchor {
    v1::history_request::Anchor::Latest(true)
}

fn seqs(page: &v1::HistoryPage) -> Vec<u64> {
    page.messages.iter().map(|message| message.seq).collect()
}

fn options(address: SocketAddr, pin: &str, name: &str, identity: usize) -> ConnectOptions {
    let mut server =
        ServerBookmark::new("", &address.ip().to_string(), address.port(), name).unwrap();
    server.certificate_sha256 = Some(pin.to_owned());
    ConnectOptions {
        voice_route: Default::default(),
        server,
        identity: session_identity(identity),
        password: None,
        force_tcp: true,
        relay_only: false,
        muted: true,
        deafened: false,
        channel: None,
        resend: Vec::new(),
        message_rate: Default::default(),
        required: Requirement::default(),
        crypto: None,
    }
}

#[tokio::test]
async fn relay_only_hides_addresses_before_presence_and_through_ban_roundtrips() {
    let fixture = Fixture::start("").await;
    let pin = fixture.running.fingerprint.clone();
    let (mut viewer, _) = Session::connect(options(fixture.address(), &pin, "Viewer", 0)).await;
    // Seed administration permission; the actual query and ban paths below go
    // through the real session/TLS protocol, including stock UserStats/BanList.
    fixture
        .running
        .server
        .lock()
        .users
        .get_mut(&viewer.session)
        .unwrap()
        .user_id = Some(0);
    let mut private_options = options(fixture.address(), &pin, "Private", 1);
    private_options.relay_only = true;
    let (mut private, negotiated) = Session::connect(private_options).await;
    assert!(
        negotiated
            .unwrap()
            .capabilities
            .iter()
            .any(|cap| cap == "privacy/1")
    );
    assert!(fixture.running.server.lock().users[&private.session].relay_only());

    viewer.command(Command::RequestStats(private.session)).await;
    let stats = viewer
        .wait(|event| match event {
            SessionEvent::Stats(stats) if stats.session == private.session => Some(stats.clone()),
            _ => None,
        })
        .await;
    assert!(
        stats.address.is_none(),
        "even an administrator must not receive a private address"
    );
    private
        .command(Command::RequestStats(private.session))
        .await;
    let own_stats = private
        .wait(|event| match event {
            SessionEvent::Stats(stats) => Some(stats.clone()),
            _ => None,
        })
        .await;
    assert!(
        own_stats.address.is_some(),
        "self-inspection stays available"
    );

    let (mut ordinary, _) = Session::connect(options(fixture.address(), &pin, "Ordinary", 2)).await;
    viewer
        .command(Command::RequestStats(ordinary.session))
        .await;
    let stats = viewer
        .wait(|event| match event {
            SessionEvent::Stats(stats) if stats.session == ordinary.session => Some(stats.clone()),
            _ => None,
        })
        .await;
    assert!(
        stats.address.is_some(),
        "stock administration semantics remain for other clients"
    );
    ordinary.command(Command::RelayOnly(true)).await;
    timeout(WAIT, async {
        while !fixture.running.server.lock().users[&ordinary.session].relay_only() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    viewer
        .command(Command::RequestStats(ordinary.session))
        .await;
    let stats = viewer
        .wait(|event| match event {
            SessionEvent::Stats(stats) if stats.session == ordinary.session => Some(stats.clone()),
            _ => None,
        })
        .await;
    assert!(stats.address.is_none());

    viewer
        .command(Command::User {
            session: private.session,
            action: nuschl_session::model::UserAction::Ban,
            reason: Some("privacy regression".into()),
        })
        .await;
    viewer.command(Command::QueryBans).await;
    let bans = viewer
        .wait(|event| match event {
            SessionEvent::Bans(bans) => Some(bans.clone()),
            _ => None,
        })
        .await;
    assert_eq!(bans.len(), 1);
    assert!(
        bans[0].address.iter().all(|byte| *byte == 0),
        "the ban list must not bypass address privacy"
    );
    viewer.command(Command::UpdateBans(bans)).await;
    viewer.command(Command::QueryBans).await;
    let bans = viewer
        .wait(|event| match event {
            SessionEvent::Bans(bans) => Some(bans.clone()),
            _ => None,
        })
        .await;
    assert!(bans[0].address.iter().all(|byte| *byte == 0));
    {
        let state = fixture.running.server.lock();
        assert!(state.bans[0].private_address);
        assert!(
            state.bans[0].address.is_some(),
            "redacting must not disable an address ban"
        );
    }
    ordinary.command(Command::RelayOnly(false)).await;
    ordinary
        .command(Command::RequestStats(ordinary.session))
        .await;
    ordinary
        .wait(|event| matches!(event, SessionEvent::Stats(_)).then_some(()))
        .await;
    assert!(
        fixture.running.server.lock().users[&ordinary.session].relay_only(),
        "a stale disable is harmless"
    );
    let fixture = fixture.restart("").await;
    assert!(fixture.running.server.lock().bans[0].private_address);
    assert!(fixture.running.server.lock().bans[0].address.is_some());
    fixture.running.shutdown().await;
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
            backlog: VecDeque::new(),
            conversations: HashMap::new(),
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
        if let Some(event) = self.backlog.pop_front() {
            return Some(event);
        }
        self.receive(WAIT).await
    }

    async fn receive(&mut self, wait: Duration) -> Option<SessionEvent> {
        let event = timeout(wait, self.events.recv()).await.ok().flatten()?;
        if let SessionEvent::ChannelInfo { channel, info } = &event {
            self.conversations
                .insert(*channel, info.conversation.clone().try_into().unwrap());
        }
        Some(event)
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

    /// Wait for an event matching `pick`; others stay in the backlog.
    async fn wait<T>(&mut self, mut pick: impl FnMut(&SessionEvent) -> Option<T>) -> T {
        if let Some(index) = self.backlog.iter().position(|event| pick(event).is_some()) {
            let event = self.backlog.remove(index).unwrap();
            return pick(&event).unwrap();
        }
        // One deadline for the whole wait: unrelated events (transport
        // statistics every few seconds) must not extend it.
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(event) = self.receive(left).await else {
                let ended = if self.task.is_finished() {
                    format!("{:?}", (&mut self.task).await)
                } else {
                    "no".into()
                };
                panic!("no matching session event (ended: {ended})");
            };
            if let Some(value) = pick(&event) {
                return value;
            }
            self.backlog.push_back(event);
        }
    }

    /// Whether an event matching `pick` arrives within `wait` (others are kept).
    async fn arrives(
        &mut self,
        wait: Duration,
        mut pick: impl FnMut(&SessionEvent) -> bool,
    ) -> bool {
        if let Some(index) = self.backlog.iter().position(&mut pick) {
            self.backlog.remove(index);
            return true;
        }
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(event) = self.receive(left).await else {
                return false;
            };
            if pick(&event) {
                return true;
            }
            self.backlog.push_back(event);
        }
    }

    async fn command(&self, command: Command) {
        self.commands.send(command).await.unwrap();
    }

    /// Send an extension request and wait for its answer.
    async fn request(&mut self, request: NuschlRequest) -> NuschlEvent {
        let id = match &request {
            NuschlRequest::Send(message) => message.request_id,
            NuschlRequest::Edit(message) => message.request_id,
            NuschlRequest::Delete(message) => message.request_id,
            NuschlRequest::History(message) => message.request_id,
            NuschlRequest::Subscribe(message) => message.request_id,
            NuschlRequest::OpenDirect(message) => message.request_id,
            NuschlRequest::ListDirect(message) => message.request_id,
            NuschlRequest::ChannelSettings(message) => message.request_id,
            NuschlRequest::ShareLinkCreate(message) => message.request_id,
            NuschlRequest::ShareLinkRevoke(message) => message.request_id,
            NuschlRequest::ShareLinkResolve(message) => message.request_id,
            _ => 0,
        };
        self.command(Command::Nuschl(request)).await;
        self.wait(|event| match event {
            SessionEvent::Nuschl(event) if request_of(event) == Some(id) => Some(event.clone()),
            _ => None,
        })
        .await
    }

    async fn send_keyed(
        &mut self,
        conversation: [u8; 16],
        key: [u8; 16],
        content: &str,
    ) -> NuschlEvent {
        self.request(NuschlRequest::Send(v1::ChatSend {
            request_id: request_id(),
            conversation: conversation.to_vec(),
            idempotency_key: key.to_vec(),
            format: v1::Format::Text as i32,
            content: content.into(),
            reply_to: 0,
        }))
        .await
    }

    /// Send and expect an acknowledgement; returns the seq.
    async fn send(&mut self, conversation: [u8; 16], content: &str) -> u64 {
        match self
            .send_keyed(conversation, connection::random_key(), content)
            .await
        {
            NuschlEvent::Ack(ack) => ack.seq,
            other => panic!("expected an acknowledgement, got {other:?}"),
        }
    }

    async fn history(
        &mut self,
        conversation: [u8; 16],
        anchor: v1::history_request::Anchor,
        limit: u32,
    ) -> NuschlEvent {
        self.request(NuschlRequest::History(v1::HistoryRequest {
            request_id: request_id(),
            conversation: conversation.to_vec(),
            anchor: Some(anchor),
            limit,
        }))
        .await
    }

    async fn page(
        &mut self,
        conversation: [u8; 16],
        anchor: v1::history_request::Anchor,
        limit: u32,
    ) -> v1::HistoryPage {
        match self.history(conversation, anchor, limit).await {
            NuschlEvent::Page(page) => page,
            other => panic!("expected a page, got {other:?}"),
        }
    }

    async fn subscribe(&mut self, conversations: &[[u8; 16]]) -> v1::Subscribed {
        match self
            .request(NuschlRequest::Subscribe(v1::Subscribe {
                request_id: request_id(),
                conversations: conversations.iter().map(|id| id.to_vec()).collect(),
            }))
            .await
        {
            NuschlEvent::Subscribed(subscribed) => subscribed,
            other => panic!("expected Subscribed, got {other:?}"),
        }
    }

    /// The next live chat event.
    async fn chat(&mut self) -> v1::ChatEvent {
        self.wait(|event| match event {
            SessionEvent::Nuschl(NuschlEvent::Chat(chat)) => Some(chat.clone()),
            _ => None,
        })
        .await
    }

    /// Register this connection's certificate; returns the registration number.
    async fn register(&mut self) -> u32 {
        self.command(Command::RegisterSelf).await;
        let me = self.session;
        self.wait(|event| match event {
            SessionEvent::User(user) if user.session == me => user.user_id,
            _ => None,
        })
        .await
    }
}

fn error_code(event: &NuschlEvent) -> Option<v1::error::Code> {
    match event {
        NuschlEvent::Error(error) => v1::error::Code::try_from(error.code).ok(),
        _ => None,
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
            protocol: nuschl::PROTOCOL,
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
            policy: None,
            kt_log: Vec::new(),
            client_version: String::new(),
            relay_only: false,
        });
        let frame = nuschl::version_frame(
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
    let pin = nuschl_session::config::fingerprint(certificate.as_ref());
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
    nuschl::version_frame(
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
    nuschl::version_frame(
        &proto::Version {
            version_v2: Some((1 << 48) | (5 << 32)),
            ..Default::default()
        },
        Some(&v1::Hello {
            protocol: nuschl::PROTOCOL,
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
            policy: None,
            kt_log: Vec::new(),
            client_version: String::new(),
            relay_only: false,
        }),
    )
    .unwrap()
}

fn requirement(nuschl: bool, capabilities: &[&str]) -> Requirement {
    Requirement {
        nuschl,
        capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
    }
}

// ---- §1 negotiation ----

#[tokio::test]
async fn the_session_negotiates_the_extension_with_nuschld() {
    let fixture = Fixture::start("").await;
    let (session, negotiated) = Session::connect(options(
        fixture.address(),
        &fixture.running.fingerprint,
        "Alice",
        0,
    ))
    .await;
    let negotiated = negotiated.expect("negotiated");
    assert!(negotiated.nuschl);
    // A compatible server has no end-to-end encryption: no `mls/1`, no `e2ee/1`.
    assert_eq!(
        negotiated.capabilities,
        [
            "history/1",
            "subscribe/1",
            "typing/1",
            "kt/1",
            "kt-witness/1",
            "share/1",
            "privacy/1"
        ]
    );
    // The key-transparency log comes with `kt/1`.
    assert_eq!(negotiated.kt_log.len(), 71);
    let policy = negotiated.policy.unwrap();
    assert_eq!(policy.guests, v1::policy::Guests::None as i32);
    assert_eq!(policy.retention_days, 0, "forever by default");
    assert_eq!(policy.mode, v1::policy::Mode::Compatible as i32);
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
    // An end-to-end server that became a compatible one (docs/e2ee.md §7.1): it
    // no longer offers `mls/1` and `e2ee/1`.
    let fixture = Fixture::start("").await;
    let mut wanting = options(fixture.address(), &fixture.running.fingerprint, "Alice", 0);
    wanting.required = requirement(true, &["mls/1", "e2ee/1"]);
    let error = Session::spawn(wanting).failure().await;
    assert!(
        matches!(&error, SessionError::Downgrade(text) if text.contains("e2ee/1, mls/1")),
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
    // ALPN nuschl/1 with a plain Version: a protocol error, not stock mode.
    let (address, pin, received) = fake_server(Some(nuschl::ALPN), stock_version()).await;
    let error = Session::spawn(options(address, &pin, "Alice", 0))
        .failure()
        .await;
    assert!(matches!(error, SessionError::Protocol(_)), "{error:?}");
    assert_eq!(received.await.unwrap(), [kind::VERSION]);
    // A hello lacking a remembered capability: a downgrade.
    let (address, pin, received) =
        fake_server(Some(nuschl::ALPN), hello_version(&["typing/1"])).await;
    let mut sticky = options(address, &pin, "Alice", 0);
    sticky.required = requirement(true, &["history/1"]);
    let error = Session::spawn(sticky).failure().await;
    assert!(matches!(error, SessionError::Downgrade(_)), "{error:?}");
    assert_eq!(received.await.unwrap(), [kind::VERSION]);
    // A malformed hello fails closed too.
    let (address, pin, received) =
        fake_server(Some(nuschl::ALPN), hello_version(&["NOT VALID"])).await;
    let error = Session::spawn(options(address, &pin, "Alice", 0))
        .failure()
        .await;
    assert!(matches!(error, SessionError::Protocol(_)), "{error:?}");
    assert_eq!(received.await.unwrap(), [kind::VERSION]);
}

#[tokio::test]
async fn the_server_fails_closed_on_mismatched_negotiation() {
    let fixture = Fixture::start("").await;
    // ALPN nuschl/1 but no hello: closed at Authenticate, nobody logged in.
    let mut raw = Raw::connect(fixture.address(), &[nuschl::ALPN, nuschl::ALPN_MUMBLE]).await;
    assert_eq!(raw.alpn.as_deref(), Some(nuschl::ALPN));
    let version = raw.until(kind::VERSION).await.unwrap();
    assert!(
        nuschl::hello(&version.payload).unwrap().is_some(),
        "the server's hello"
    );
    raw.version(None).await;
    raw.authenticate("NoHello").await;
    assert!(raw.closed().await);
    // A malformed hello closes the connection.
    let mut raw = Raw::connect(fixture.address(), &[nuschl::ALPN]).await;
    raw.version(Some(&["Bad Capability"])).await;
    raw.authenticate("BadHello").await;
    assert!(raw.closed().await);
    // A hello without ALPN is ignored: a stock login, and no hello back.
    let mut raw = Raw::connect(fixture.address(), &[]).await;
    assert_eq!(raw.alpn, None);
    let version = raw.until(kind::VERSION).await.unwrap();
    assert_eq!(nuschl::hello(&version.payload).unwrap(), None);
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
    let mut raw = Raw::connect(fixture.address(), &[nuschl::ALPN]).await;
    raw.version(Some(&["history/1"])).await;
    raw.send(
        nuschl::kind::SUBSCRIBE,
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

// ---- minimum client versions (§1, "Minimum versions") ----

#[tokio::test]
async fn an_outdated_nuschl_client_is_refused_with_the_version_and_stock_clients_are_not() {
    let fixture = Fixture::start("[history]\nminimum_client = \"99.0.0\"").await;
    // The real session: refused with the version to update to, never retried,
    // and the server keeps nobody.
    let error = Session::spawn(options(
        fixture.address(),
        &fixture.running.fingerprint,
        "Alice",
        0,
    ))
    .failure()
    .await;
    assert!(
        matches!(&error, SessionError::Outdated { minimum, current }
            if minimum == "99.0.0" && current == env!("CARGO_PKG_VERSION")),
        "{error:?}"
    );
    assert!(!error.transient(), "no automatic retry, never a fallback");
    assert!(
        error
            .to_string()
            .contains("requires Nuschl 99.0.0 or newer")
    );
    assert_eq!(fixture.users(), 0);
    // A Nuschl hello without a version is older than any minimum; the reject
    // says why, as a Mumble `Reject` any client can show.
    let mut raw = Raw::connect(fixture.address(), &[nuschl::ALPN]).await;
    raw.version(Some(&["history/1"])).await;
    raw.authenticate("Unversioned").await;
    let reject = raw.until(kind::REJECT).await.expect("a reject");
    let reject = proto::Reject::decode(reject.payload).unwrap();
    assert_eq!(
        reject.r#type,
        Some(proto::reject::RejectType::WrongVersion as i32)
    );
    assert_eq!(
        reject.reason.as_deref(),
        Some("This server requires Nuschl 99.0.0 or newer. Update Nuschl to connect.")
    );
    // Stock Mumble clients are not affected by the minimum.
    let mut stock = Raw::connect(fixture.address(), &[]).await;
    stock.version(None).await;
    stock.authenticate("Stock").await;
    assert!(!stock.closed().await, "stock clients log in");
    assert_eq!(fixture.users(), 1);
}

#[tokio::test]
async fn before_the_date_an_outdated_client_is_admitted_and_told_when() {
    let fixture =
        Fixture::start("[history]\nminimum_client = \"99.0.0\"\nminimum_client_from = 2999-01-01")
            .await;
    let (session, negotiated) = Session::connect(options(
        fixture.address(),
        &fixture.running.fingerprint,
        "Alice",
        0,
    ))
    .await;
    let negotiated = negotiated.expect("negotiated");
    let policy = negotiated.policy.as_ref().unwrap();
    assert_eq!(policy.minimum_client, 99 << 48);
    // 2999-01-01T00:00:00Z.
    assert_eq!(policy.minimum_client_from, 32_472_144_000);
    assert_eq!(
        negotiated.standing(connection::now_ms()),
        nuschl::version::Standing::Required {
            version: nuschl::version::Version::new(99, 0, 0),
            from: 32_472_144_000,
        }
    );
    assert!(session.session != 0, "logged in");
    drop(session);
    // A recommended version only suggests an update.
    let fixture = Fixture::start("[history]\nrecommended_client = \"99.1.0\"").await;
    let (_session, negotiated) = Session::connect(options(
        fixture.address(),
        &fixture.running.fingerprint,
        "Bob",
        1,
    ))
    .await;
    assert_eq!(
        negotiated.unwrap().standing(connection::now_ms()),
        nuschl::version::Standing::Recommended {
            version: nuschl::version::Version::new(99, 1, 0)
        }
    );
}

// ---- helpers for stages 2–5 ----

/// A Nuschl connection to the fixture.
async fn nuschl(fixture: &Fixture, name: &str, identity: usize) -> Session {
    Session::connect(options(
        fixture.address(),
        &fixture.running.fingerprint,
        name,
        identity,
    ))
    .await
    .0
}

impl Raw {
    /// A stock client logged in (no ALPN, no hello).
    async fn login(address: SocketAddr, name: &str) -> Self {
        let mut raw = Self::connect(address, &[]).await;
        raw.version(None).await;
        raw.authenticate(name).await;
        raw.until(kind::SERVER_SYNC).await.expect("stock login");
        raw
    }

    async fn text(&mut self) -> proto::TextMessage {
        let frame = self
            .until(kind::TEXT_MESSAGE)
            .await
            .expect("a text message");
        proto::TextMessage::decode(frame.payload).unwrap()
    }
}

impl Fixture {
    /// Add a channel under the root, as an administrator would, with these
    /// ACL entries; returns its id and conversation.
    fn add_channel(
        &self,
        name: &str,
        acls: Vec<nuschld::acl::AclEntry>,
        groups: Vec<(&str, Vec<u32>)>,
    ) -> (u32, [u8; 16]) {
        let mut state = self.running.server.lock();
        let id = state.channels.0.keys().max().unwrap() + 1;
        let mut channel = nuschld::state::Channel::new(id, Some(0), name.into());
        channel.node.acls = acls;
        for (group, members) in groups {
            let mut entry = nuschld::acl::Group::new();
            entry.add = members.into_iter().collect();
            channel.node.groups.insert(group.into(), entry);
        }
        let conversation = channel.conversation_id;
        state.conversations.add_channel(conversation, id);
        state.channels.0.insert(id, channel);
        state.channels.get_mut(0).unwrap().children.push(id);
        state.permissions_changed(None);
        state.authz_changed(nuschld::conversations::Scope::All);
        state.finish();
        (id, conversation)
    }

    /// Change a channel as an ACL edit would, then let the server re-evaluate.
    fn edit_channel(&self, id: u32, edit: impl FnOnce(&mut nuschld::state::Channel)) {
        let mut state = self.running.server.lock();
        edit(state.channels.get_mut(id).unwrap());
        state.permissions_changed(None);
        state.authz_changed(nuschld::conversations::Scope::All);
        state.finish();
    }
}

fn entry(group: &str, user: Option<u32>, grant: u32, deny: u32) -> nuschld::acl::AclEntry {
    nuschld::acl::AclEntry {
        apply_here: true,
        apply_subs: true,
        user_id: user,
        group: group.into(),
        grant,
        deny,
    }
}

use nuschld::acl::perm;

const QUIET: Duration = Duration::from_millis(400);

/// A stock `TextMessage` (live text from history carries its seq).
fn stock_text(event: &SessionEvent) -> bool {
    matches!(event, SessionEvent::Text(text) if text.seq.is_none())
}

fn is_chat(event: &SessionEvent) -> bool {
    matches!(event, SessionEvent::Nuschl(NuschlEvent::Chat(_)))
}

// ---- §4 sending, stock copies ----

#[tokio::test]
async fn messages_are_stored_acknowledged_and_copied_for_stock_clients() {
    let fixture = Fixture::start("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    alice.register().await;
    let mut bob = nuschl(&fixture, "Bob", 1).await;
    let mut carol = Raw::login(fixture.address(), "Carol").await;
    let root = alice.conversations[&0];
    assert_eq!(bob.conversations[&0], root, "one conversation per channel");
    let content = "hi <b> https://example.org\n```\ncode\n```";
    assert_eq!(alice.send(root, content).await, 1);
    let event = bob.chat().await;
    let message = event.message.unwrap();
    assert_eq!((message.seq, message.content.as_str()), (1, content));
    assert_eq!(message.format, v1::Format::Text as i32);
    assert_eq!(message.author.unwrap().name, "Alice");
    // The stock copy is made by the server from the same text.
    let text = carol.text().await;
    assert_eq!(text.message, nuschl_legacy_html::legacy_body(content, true));
    assert_eq!(text.actor, Some(alice.session));
    assert_eq!(text.channel_id, [0]);
    assert!(
        !bob.arrives(QUIET, stock_text).await,
        "Nuschl clients get no TextMessage duplicate"
    );
    // A stock sender's message reaches Nuschl readers from history.
    carol
        .send(
            kind::TEXT_MESSAGE,
            &proto::TextMessage {
                channel_id: vec![0, 0],
                message: "<i>stock</i>".into(),
                ..Default::default()
            },
        )
        .await;
    for session in [&mut alice, &mut bob] {
        let message = session.chat().await.message.unwrap();
        assert_eq!(message.seq, 2);
        assert_eq!(message.format, v1::Format::LegacyHtml as i32);
        assert_eq!(message.content, "<i>stock</i>");
    }
    assert!(
        !alice.arrives(QUIET, stock_text).await,
        "stored messages arrive from history only"
    );
    let page = alice.page(root, latest(), 10).await;
    assert_eq!(seqs(&page), [1, 2]);
    assert_eq!(page.head, 2);
    assert!(!page.more_before && !page.more_after && !page.restricted);
}

#[tokio::test]
async fn retries_return_the_first_seq_and_conflicts_fail() {
    let fixture = Fixture::start("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    let mut bob = nuschl(&fixture, "Bob", 1).await;
    alice.register().await;
    bob.register().await;
    let root = alice.conversations[&0];
    let key = connection::random_key();
    let first = alice.send_keyed(root, key, "once").await;
    assert!(matches!(first, NuschlEvent::Ack(ref ack) if ack.seq == 1));
    assert_eq!(bob.chat().await.message.unwrap().seq, 1);
    let retry = alice.send_keyed(root, key, "once").await;
    assert!(
        matches!(retry, NuschlEvent::Ack(ref ack) if ack.seq == 1),
        "{retry:?}"
    );
    let conflict = alice.send_keyed(root, key, "something else").await;
    assert_eq!(
        error_code(&conflict),
        Some(v1::error::Code::IdempotencyConflict)
    );
    assert!(
        !bob.arrives(QUIET, is_chat).await,
        "a retry is not delivered twice"
    );
    // The seqs assigned to the retry and the conflict stay explicit.
    let page = bob.page(root, latest(), 10).await;
    let kinds: Vec<i32> = page.messages.iter().map(|message| message.kind).collect();
    assert_eq!(seqs(&page), [1, 2, 3]);
    assert_eq!(
        kinds,
        [
            v1::message::Kind::Message as i32,
            v1::message::Kind::Void as i32,
            v1::message::Kind::Void as i32
        ]
    );
    // Durable across a restart: the same account's retry still gets seq 1.
    drop((alice, bob));
    let fixture = fixture.restart("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    let retry = alice.send_keyed(root, key, "once").await;
    assert!(
        matches!(retry, NuschlEvent::Ack(ref ack) if ack.seq == 1),
        "{retry:?}"
    );
    assert_eq!(
        alice.send(root, "next").await,
        5,
        "seq continues after a restart"
    );
    // Unregistered senders have no durable identity: no idempotency.
    let mut guest = nuschl(&fixture, "Guest", 2).await;
    let key = connection::random_key();
    for expected in [6, 7] {
        let ack = guest.send_keyed(root, key, "same").await;
        assert!(
            matches!(ack, NuschlEvent::Ack(ref ack) if ack.seq == expected),
            "{ack:?}"
        );
    }
}

// ---- §6 history pages ----

#[tokio::test]
async fn history_pages_every_anchor_and_never_skip_at_the_byte_cap() {
    let fixture = Fixture::start("text_message_length = 0\nimage_message_length = 0").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    alice.register().await;
    let root = alice.conversations[&0];
    for n in 1..=10 {
        alice.send(root, &format!("m{n}")).await;
    }
    use v1::history_request::Anchor;
    let page = alice.page(root, latest(), 3).await;
    assert_eq!(seqs(&page), [8, 9, 10]);
    assert!(page.more_before && !page.more_after);
    let page = alice.page(root, Anchor::Before(8), 3).await;
    assert_eq!(seqs(&page), [5, 6, 7]);
    assert!(page.more_before && page.more_after);
    let page = alice.page(root, Anchor::After(7), 2).await;
    assert_eq!(seqs(&page), [8, 9]);
    assert!(page.more_before && page.more_after);
    let page = alice.page(root, Anchor::After(9), 5).await;
    assert_eq!(seqs(&page), [10]);
    assert!(!page.more_after);
    let page = alice.page(root, Anchor::Around(5), 4).await;
    assert_eq!(seqs(&page), [4, 5, 6, 7]);
    assert!(page.more_before && page.more_after);
    let page = alice.page(root, Anchor::Before(1), 5).await;
    assert!(page.messages.is_empty() && !page.more_before && page.more_after);
    let page = alice.page(root, latest(), 0).await;
    assert_eq!(page.messages.len(), 10, "0 means the default page size");
    assert_eq!(
        error_code(&alice.history(root, latest(), 101).await),
        Some(v1::error::Code::Malformed)
    );
    // The byte cap ends a page early but never skips a message.
    let big = "x".repeat(60_000);
    for _ in 0..5 {
        alice.send(root, &big).await;
    }
    let page = alice.page(root, latest(), 10).await;
    assert_eq!(seqs(&page), [12, 13, 14, 15]);
    assert!(page.more_before);
    let page = alice.page(root, Anchor::Before(12), 10).await;
    assert_eq!(seqs(&page), [2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
    // A message larger than the cap comes alone (a stock client's).
    let mut carol = Raw::login(fixture.address(), "Carol").await;
    carol
        .send(
            kind::TEXT_MESSAGE,
            &proto::TextMessage {
                channel_id: vec![0],
                message: "y".repeat(300_000),
                ..Default::default()
            },
        )
        .await;
    alice.chat().await;
    let page = alice.page(root, latest(), 10).await;
    assert_eq!(seqs(&page), [16]);
    assert!(page.more_before);
}

// ---- §2 limits and uniform errors ----

#[tokio::test]
async fn errors_are_uniform_and_ids_cannot_be_probed() {
    let fixture = Fixture::start("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    alice.register().await;
    let (_, secret) = fixture.add_channel(
        "Secret",
        vec![entry("all", None, 0, perm::ENTER | perm::TEXT_MESSAGE)],
        vec![],
    );
    let unknown = connection::random_key();
    let mut details = Vec::new();
    for conversation in [secret, unknown] {
        for event in [
            alice.history(conversation, latest(), 10).await,
            alice
                .send_keyed(conversation, connection::random_key(), "x")
                .await,
        ] {
            let NuschlEvent::Error(error) = event else {
                panic!("expected an error, got {event:?}");
            };
            assert_eq!(error.code, v1::error::Code::NotFound as i32);
            details.push(error.detail);
        }
    }
    details.dedup();
    assert_eq!(details, [nuschl::detail(v1::error::Code::NotFound)]);
    assert!(
        alice
            .subscribe(&[secret, unknown])
            .await
            .conversations
            .is_empty()
    );
    let mut malformed = v1::HistoryRequest {
        request_id: request_id(),
        conversation: vec![1; 5],
        anchor: Some(latest()),
        limit: 1,
    };
    assert_eq!(
        error_code(
            &alice
                .request(NuschlRequest::History(malformed.clone()))
                .await
        ),
        Some(v1::error::Code::Malformed)
    );
    malformed.conversation = alice.conversations[&0].to_vec();
    malformed.anchor = None;
    malformed.request_id = request_id();
    assert_eq!(
        error_code(&alice.request(NuschlRequest::History(malformed)).await),
        Some(v1::error::Code::Malformed)
    );
    // Over the wire: an unnegotiated capability and a server-only type are
    // refused, never processed.
    let mut raw = Raw::connect(fixture.address(), &[nuschl::ALPN]).await;
    raw.version(Some(&["history/1"])).await;
    raw.authenticate("Raw").await;
    raw.until(kind::SERVER_SYNC).await.unwrap();
    for (frame_kind, body) in [
        (
            nuschl::kind::TYPING,
            v1::Typing {
                conversation: vec![0; 16],
                session: 0,
            }
            .encode_to_vec(),
        ),
        (
            nuschl::kind::CHAT_ACK,
            v1::ChatAck {
                request_id: 7,
                ..Default::default()
            }
            .encode_to_vec(),
        ),
    ] {
        raw.send_bytes(
            &[
                &frame_kind.to_be_bytes()[..],
                &(body.len() as u32).to_be_bytes(),
                &body,
            ]
            .concat(),
        )
        .await;
        let error =
            v1::Error::decode(raw.until(nuschl::kind::ERROR).await.unwrap().payload).unwrap();
        assert_eq!(error.code, v1::error::Code::Unsupported as i32);
    }
    // An oversized frame from the header closes the connection.
    raw.send_bytes(
        &[
            &nuschl::kind::HISTORY_REQUEST.to_be_bytes()[..],
            &4096_u32.to_be_bytes(),
        ]
        .concat(),
    )
    .await;
    assert!(raw.closed().await);
}

// ---- §5 who may read ----

#[tokio::test]
async fn guests_see_live_messages_but_read_no_history() {
    let fixture = Fixture::start("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    alice.register().await;
    let mut guest = nuschl(&fixture, "Guest", 1).await;
    let root = alice.conversations[&0];
    alice.send(root, "live").await;
    assert_eq!(guest.chat().await.message.unwrap().content, "live");
    assert_eq!(
        error_code(&guest.history(root, latest(), 10).await),
        Some(v1::error::Code::NotFound)
    );
    assert!(guest.subscribe(&[root]).await.conversations.is_empty());
    drop((alice, guest));
    let fixture = fixture.restart("[history]\nguests = \"full\"").await;
    let mut guest = nuschl(&fixture, "Guest", 1).await;
    assert_eq!(seqs(&guest.page(root, latest(), 10).await), [1]);
}

#[tokio::test]
async fn revoking_access_ends_subscriptions_fan_out_and_pages_in_flight() {
    let fixture = Fixture::start("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    let alice_id = alice.register().await;
    let mut bob = nuschl(&fixture, "Bob", 1).await;
    bob.register().await;
    let (channel, conversation) = fixture.add_channel("Side", vec![], vec![]);
    let subscribed = alice.subscribe(&[conversation]).await;
    assert_eq!(subscribed.conversations.len(), 1);
    bob.send(conversation, "before").await;
    assert_eq!(alice.chat().await.message.unwrap().content, "before");
    // An ACL edit takes Enter away: unsubscribed, and nothing more arrives.
    fixture.edit_channel(channel, |channel| {
        channel
            .node
            .acls
            .push(entry("", Some(alice_id), 0, perm::ENTER));
    });
    let gone = alice
        .wait(|event| match event {
            SessionEvent::Nuschl(NuschlEvent::Unsubscribed(gone)) => Some(gone.clone()),
            _ => None,
        })
        .await;
    assert_eq!(gone.conversation, conversation.to_vec());
    bob.send(conversation, "after").await;
    assert!(!alice.arrives(QUIET, is_chat).await);
    assert_eq!(
        error_code(&alice.history(conversation, latest(), 10).await),
        Some(v1::error::Code::NotFound)
    );
    assert!(
        alice
            .subscribe(&[conversation])
            .await
            .conversations
            .is_empty()
    );
    // A page authorised before a revocation and finished after it is
    // re-checked and never sent.
    fixture.edit_channel(channel, |channel| channel.node.acls.clear());
    let page = nuschld::history::Read {
        session: alice.session,
        request_id: 4242,
        conversation,
        query: nuschl_ipc::server::PageQuery {
            conversation: nuschld::conversations::hex(&conversation),
            anchor: nuschl_ipc::server::Anchor::Latest,
            limit: 10,
            ranges: vec![(1, u64::MAX)],
            max_bytes: 1024,
        },
        epoch: fixture.running.server.lock().authz_epoch,
        clipped: false,
    };
    let rows = nuschl_ipc::server::StoredPage {
        rows: vec![nuschl_ipc::server::StoredMessage {
            conversation: nuschld::conversations::hex(&conversation),
            seq: 1,
            content: "secret".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    fixture.edit_channel(channel, |channel| {
        channel
            .node
            .acls
            .push(entry("", Some(alice_id), 0, perm::ENTER));
    });
    fixture.running.server.lock().page_read(page, Some(rows));
    let answer = alice
        .wait(|event| match event {
            SessionEvent::Nuschl(event) if request_of(event) == Some(4242) => Some(event.clone()),
            _ => None,
        })
        .await;
    assert_eq!(error_code(&answer), Some(v1::error::Code::NotFound));
    // Being in the channel or listening still reads it (Mumble's live audience).
    let mut state = fixture.running.server.lock();
    assert!(!state.allowed(
        alice.session,
        nuschld::authz::Action::ReadHistory(&conversation)
    ));
    state
        .users
        .get_mut(&alice.session)
        .unwrap()
        .listening
        .insert(channel, 1.0);
    assert!(state.allowed(
        alice.session,
        nuschld::authz::Action::ReadHistory(&conversation)
    ));
}

/// Listeners follow Listen (the user's decision on review D3.4; Murmur keeps
/// them): an ACL edit that takes Listen away ends the listener of a Nuschl
/// client and of a stock client at once. Both learn it as from Murmur and by a
/// `PermissionDenied`; live text stops; the channel is no longer readable
/// through the listener, neither for new pages nor for one in flight; the
/// remembered listener is forgotten; a listener that keeps Listen stays.
#[tokio::test]
async fn a_listener_that_loses_listen_ends_for_stock_and_nuschl_clients() {
    let fixture = Fixture::start("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    let alice_id = alice.register().await;
    let mut carol = nuschl(&fixture, "Carol", 1).await;
    let carol_id = carol.register().await;
    let mut bob = nuschl(&fixture, "Bob", 2).await;
    bob.register().await;
    let mut dave = Raw::login(fixture.address(), "Dave").await;
    let dave_session = {
        let state = fixture.running.server.lock();
        state
            .users
            .values()
            .find(|user| user.name == "Dave")
            .unwrap()
            .session
    };
    // Nobody may enter: listening is the only way to read the channel.
    let (radio, conversation) =
        fixture.add_channel("Radio", vec![entry("all", None, 0, perm::ENTER)], vec![]);
    {
        // Announced and stored as an administrator's new channel would be.
        let state = fixture.running.server.lock();
        state.persist_channel(radio);
        let message = state.channel_state(state.channels.get(radio).unwrap(), 0);
        state.broadcast_channel_state(&message);
    }
    for client in [&mut alice, &mut carol] {
        client
            .wait(|event| match event {
                SessionEvent::Channel(channel) if channel.id == radio => Some(()),
                _ => None,
            })
            .await;
        client
            .command(Command::Listen {
                channel: radio,
                listen: true,
            })
            .await;
    }
    dave.send(
        kind::USER_STATE,
        &proto::UserState {
            session: Some(dave_session),
            listening_channel_add: vec![radio],
            ..Default::default()
        },
    )
    .await;
    let listeners = || {
        fixture
            .running
            .server
            .lock()
            .channels
            .get(radio)
            .unwrap()
            .listeners
            .clone()
    };
    for _ in 0..200 {
        if listeners().len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(listeners().len(), 3);
    assert_eq!(
        alice.subscribe(&[conversation]).await.conversations.len(),
        1
    );
    assert_eq!(
        error_code(&alice.history(conversation, latest(), 10).await),
        None
    );
    bob.send(conversation, "before").await;
    assert_eq!(alice.chat().await.message.unwrap().content, "before");
    assert_eq!(carol.chat().await.message.unwrap().content, "before");
    assert!(dave.text().await.message.contains("before"));
    // Remembered listeners are written behind the state lock: wait for them.
    let stored = |id: u32, expected: Vec<(u32, f32)>| {
        let storage = fixture.running.server.storage.clone();
        async move {
            for _ in 0..200 {
                if storage.user_listeners(id).await == expected {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            false
        }
    };
    assert!(stored(alice_id, vec![(radio, 1.0)]).await);
    // A page authorised now and finished after the revocation.
    let page = nuschld::history::Read {
        session: alice.session,
        request_id: 4343,
        conversation,
        query: nuschl_ipc::server::PageQuery {
            conversation: nuschld::conversations::hex(&conversation),
            anchor: nuschl_ipc::server::Anchor::Latest,
            limit: 10,
            ranges: vec![(1, u64::MAX)],
            max_bytes: 1024,
        },
        epoch: fixture.running.server.lock().authz_epoch,
        clipped: false,
    };

    // Listen goes for everyone but Carol.
    fixture.edit_channel(radio, |channel| {
        channel.node.acls.push(entry("all", None, 0, perm::LISTEN));
        channel
            .node
            .acls
            .push(entry("", Some(carol_id), perm::LISTEN, 0));
    });
    assert_eq!(listeners(), BTreeSet::from([carol.session]));
    // The Nuschl client is told why, then loses the subscription.
    let told = alice
        .wait(|event| match event {
            SessionEvent::PermissionDenied(text) => Some(text.clone()),
            _ => None,
        })
        .await;
    assert!(told.contains("Radio") && told.contains("stopped"), "{told}");
    alice
        .wait(|event| match event {
            SessionEvent::Nuschl(NuschlEvent::Unsubscribed(gone))
                if gone.conversation == conversation.to_vec() =>
            {
                Some(())
            }
            _ => None,
        })
        .await;
    // The stock client the Murmur way, and by a PermissionDenied for Listen.
    loop {
        let frame = dave.until(kind::USER_STATE).await.expect("a UserState");
        let state = proto::UserState::decode(frame.payload).unwrap();
        if state.session == Some(dave_session) && !state.listening_channel_remove.is_empty() {
            assert_eq!(state.listening_channel_remove, [radio]);
            assert_eq!(state.actor, None);
            break;
        }
    }
    let denied = proto::PermissionDenied::decode(
        dave.until(kind::PERMISSION_DENIED)
            .await
            .expect("a PermissionDenied")
            .payload,
    )
    .unwrap();
    assert_eq!(denied.permission, Some(perm::LISTEN));
    assert_eq!(denied.channel_id, Some(radio));
    assert_eq!(denied.session, Some(dave_session));
    assert_eq!(
        denied.r#type,
        Some(proto::permission_denied::DenyType::Permission as i32)
    );
    // Live delivery stops for them, not for Carol.
    bob.send(conversation, "after").await;
    assert_eq!(carol.chat().await.message.unwrap().content, "after");
    assert!(!alice.arrives(QUIET, is_chat).await);
    assert!(
        timeout(QUIET, dave.until(kind::TEXT_MESSAGE))
            .await
            .is_err(),
        "the stock client still got the channel's text"
    );
    // No history through the listener: new pages and the one in flight.
    assert_eq!(
        error_code(&alice.history(conversation, latest(), 10).await),
        Some(v1::error::Code::NotFound)
    );
    fixture.running.server.lock().page_read(
        page,
        Some(nuschl_ipc::server::StoredPage {
            rows: vec![nuschl_ipc::server::StoredMessage {
                conversation: nuschld::conversations::hex(&conversation),
                seq: 1,
                content: "secret".into(),
                ..Default::default()
            }],
            ..Default::default()
        }),
    );
    let answer = alice
        .wait(|event| match event {
            SessionEvent::Nuschl(event) if request_of(event) == Some(4343) => Some(event.clone()),
            _ => None,
        })
        .await;
    assert_eq!(error_code(&answer), Some(v1::error::Code::NotFound));
    // The remembered listener is forgotten; Carol's stays.
    assert!(stored(alice_id, vec![]).await);
    assert!(stored(carol_id, vec![(radio, 1.0)]).await);
}

#[tokio::test]
async fn since_access_shows_only_what_was_said_while_the_reader_had_access() {
    let fixture = Fixture::start("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    let alice_id = alice.register().await;
    let mut bob = nuschl(&fixture, "Bob", 1).await;
    let bob_id = bob.register().await;
    let (channel, conversation) = fixture.add_channel(
        "Private",
        vec![
            entry("all", None, 0, perm::ENTER),
            entry("members", None, perm::ENTER, 0),
            entry("", Some(bob_id), perm::WRITE, 0),
        ],
        vec![("members", vec![alice_id, bob_id])],
    );
    // Bob (Write on the channel) switches it to since_access.
    let done = bob
        .request(NuschlRequest::ChannelSettings(v1::ChannelSettings {
            request_id: request_id(),
            channel,
            visibility: Some(v1::Visibility::SinceAccess as i32),
            ..Default::default()
        }))
        .await;
    assert!(matches!(done, NuschlEvent::Done(_)), "{done:?}");
    let info = alice
        .wait(|event| match event {
            SessionEvent::ChannelInfo { channel: id, info } if *id == channel => Some(info.clone()),
            _ => None,
        })
        .await;
    assert_eq!(info.visibility, v1::Visibility::SinceAccess as i32);
    let members = |state: &mut nuschld::state::Channel, add: bool| {
        let group = state.node.groups.get_mut("members").unwrap();
        if add {
            group.add.insert(alice_id);
        } else {
            group.add.remove(&alice_id);
        }
    };
    bob.send(conversation, "m1").await;
    bob.send(conversation, "m2").await;
    fixture.edit_channel(channel, |channel| members(channel, false));
    bob.send(conversation, "m3").await;
    bob.send(conversation, "m4").await;
    fixture.edit_channel(channel, |channel| members(channel, true));
    bob.send(conversation, "m5").await;
    use v1::history_request::Anchor;
    let page = alice.page(conversation, latest(), 10).await;
    assert_eq!(seqs(&page), [1, 2, 5]);
    assert!(page.restricted);
    // Every anchor is clipped; outside is indistinguishable from nothing.
    assert_eq!(
        seqs(&alice.page(conversation, Anchor::Around(3), 10).await),
        [1, 2, 5]
    );
    assert_eq!(
        seqs(&alice.page(conversation, Anchor::Before(5), 10).await),
        [1, 2]
    );
    assert_eq!(
        seqs(&alice.page(conversation, Anchor::After(2), 10).await),
        [5]
    );
    assert_eq!(
        seqs(&bob.page(conversation, latest(), 10).await),
        [1, 2, 3, 4, 5]
    );
    // Someone admitted later reads only from then on.
    let mut carol = nuschl(&fixture, "Carol", 2).await;
    let carol_id = carol.register().await;
    fixture.edit_channel(channel, |channel| {
        channel
            .node
            .groups
            .get_mut("members")
            .unwrap()
            .add
            .insert(carol_id);
    });
    assert!(
        carol
            .page(conversation, Anchor::Around(1), 10)
            .await
            .messages
            .is_empty()
    );
    bob.send(conversation, "m6").await;
    assert_eq!(seqs(&carol.page(conversation, latest(), 10).await), [6]);
    // The intervals are durable.
    drop((alice, bob, carol));
    let fixture = fixture.restart("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    assert_eq!(
        seqs(&alice.page(conversation, latest(), 10).await),
        [1, 2, 5, 6]
    );
}

// ---- §6 edits and deletions ----

impl Session {
    async fn edit(&mut self, conversation: [u8; 16], target: u64, content: &str) -> NuschlEvent {
        self.request(NuschlRequest::Edit(v1::ChatEdit {
            request_id: request_id(),
            conversation: conversation.to_vec(),
            target,
            format: v1::Format::Text as i32,
            content: content.into(),
        }))
        .await
    }

    async fn delete(&mut self, conversation: [u8; 16], target: u64) -> NuschlEvent {
        self.request(NuschlRequest::Delete(v1::ChatDelete {
            request_id: request_id(),
            conversation: conversation.to_vec(),
            target,
        }))
        .await
    }
}

#[tokio::test]
async fn edits_replace_content_and_deletions_leave_tombstones() {
    let fixture = Fixture::start("[history]\nedit_limit = 1").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    alice.register().await;
    let mut bob = nuschl(&fixture, "Bob", 1).await;
    let bob_id = bob.register().await;
    let mut carol = Raw::login(fixture.address(), "Carol").await;
    let root = alice.conversations[&0];
    assert_eq!(alice.send(root, "a secret pasted by mistake").await, 1);
    bob.chat().await;
    carol.text().await;
    let edited = alice.edit(root, 1, "fixed").await;
    let NuschlEvent::Ack(ack) = edited else {
        panic!("{edited:?}");
    };
    assert_eq!(ack.seq, 2, "an edit is an event with its own seq");
    let event = bob.chat().await.message.unwrap();
    assert_eq!(event.kind, v1::message::Kind::Edit as i32);
    assert_eq!((event.target, event.content.as_str()), (1, "fixed"));
    // Only the author edits, and only so often.
    assert_eq!(
        error_code(&bob.edit(root, 1, "mine now").await),
        Some(v1::error::Code::NotFound)
    );
    assert_eq!(
        error_code(&alice.edit(root, 1, "again").await),
        Some(v1::error::Code::LimitReached)
    );
    let page = bob.page(root, latest(), 10).await;
    let first = &page.messages[0];
    assert_eq!((first.content.as_str(), first.edits), ("fixed", 1));
    assert!(first.edited_ms > 0);
    assert!(
        page.messages
            .iter()
            .all(|message| !message.content.contains("secret")),
        "earlier versions are never served"
    );
    assert!(
        fixture
            .store
            .lock()
            .unwrap()
            .history
            .messages
            .values()
            .all(|row| !row.content.contains("secret")),
        "and not kept"
    );
    // Stock clients see neither edits nor deletions.
    assert!(carol.next_text(QUIET).await.is_none());
    // The author deletes; the tombstone keeps the seq.
    assert!(matches!(alice.delete(root, 1).await, NuschlEvent::Ack(_)));
    let event = bob.chat().await.message.unwrap();
    assert_eq!(event.kind, v1::message::Kind::Delete as i32);
    assert_eq!(event.target, 1);
    // A moderator (Write) deletes someone else's message, with a reason.
    let seq = bob.send(root, "spam").await;
    fixture.edit_channel(0, |root| {
        root.node.acls.push(entry("", Some(1), perm::WRITE, 0));
    });
    assert!(matches!(alice.delete(root, seq).await, NuschlEvent::Ack(_)));
    assert_eq!(
        error_code(&bob.delete(root, 1).await),
        Some(v1::error::Code::NotFound),
        "nobody else's"
    );
    let page = bob.page(root, latest(), 20).await;
    let tombstone = page
        .messages
        .iter()
        .find(|message| message.seq == 1)
        .unwrap();
    assert!(tombstone.deleted_ms > 0 && tombstone.content.is_empty());
    assert_eq!(tombstone.delete_reason, 0);
    let moderated = page
        .messages
        .iter()
        .find(|message| message.seq == seq)
        .unwrap();
    assert_eq!(moderated.delete_reason, 1);
    let _ = bob_id;
}

impl Raw {
    async fn next_text(&mut self, wait: Duration) -> Option<proto::TextMessage> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Ok(Some(frame)) = decode_control(&mut self.buffer) {
                if frame.kind == kind::TEXT_MESSAGE {
                    return Some(proto::TextMessage::decode(frame.payload).unwrap());
                }
                continue;
            }
            match tokio::time::timeout_at(deadline, self.reader.read_buf(&mut self.buffer)).await {
                Ok(Ok(n)) if n > 0 => {}
                _ => return None,
            }
        }
    }
}

// ---- §3 direct conversations, accounts ----

impl Session {
    async fn open_direct(&mut self, user_id: u32) -> NuschlEvent {
        self.request(NuschlRequest::OpenDirect(v1::OpenDirect {
            request_id: request_id(),
            user_id,
        }))
        .await
    }
}

#[tokio::test]
async fn direct_conversations_belong_to_two_accounts_for_good() {
    let fixture = Fixture::start("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    alice.register().await;
    let mut bob = nuschl(&fixture, "Bob", 1).await;
    let bob_id = bob.register().await;
    let mut carol = nuschl(&fixture, "Carol", 2).await;
    carol.register().await;
    let NuschlEvent::DirectOpened(opened) = alice.open_direct(bob_id).await else {
        panic!("expected a direct conversation");
    };
    let direct = opened.direct.unwrap();
    assert_eq!((direct.user_id, direct.name.as_str()), (bob_id, "Bob"));
    let conversation: [u8; 16] = direct.conversation.clone().try_into().unwrap();
    // The same pair always gets the same conversation.
    let NuschlEvent::DirectOpened(again) = alice.open_direct(bob_id).await else {
        panic!();
    };
    assert_eq!(again.direct.unwrap().conversation, direct.conversation);
    assert_eq!(alice.send(conversation, "hi Bob").await, 1);
    assert_eq!(bob.chat().await.message.unwrap().content, "hi Bob");
    assert!(!carol.arrives(QUIET, is_chat).await);
    assert_eq!(
        error_code(&carol.history(conversation, latest(), 10).await),
        Some(v1::error::Code::NotFound)
    );
    let NuschlEvent::DirectList(list) = bob
        .request(NuschlRequest::ListDirect(v1::ListDirect {
            request_id: request_id(),
        }))
        .await
    else {
        panic!();
    };
    assert_eq!(list.directs.len(), 1);
    assert_eq!(list.directs[0].name, "Alice");
    // Guests have no durable identity and no direct conversations.
    let mut guest = nuschl(&fixture, "Guest", 3).await;
    assert_eq!(
        error_code(&guest.open_direct(bob_id).await),
        Some(v1::error::Code::NotFound)
    );
    // Deleting Bob's account freezes the conversation; a new "Bob" (with a new
    // account id) inherits nothing.
    drop(bob);
    {
        let mut state = fixture.running.server.lock();
        let bob = state.registry.by_name("Bob").unwrap().clone();
        state.registry.remove(bob.id);
        state.authz_changed(nuschld::conversations::Scope::All);
        state.finish();
    }
    let page = alice.page(conversation, latest(), 10).await;
    assert_eq!(page.messages[0].content, "hi Bob");
    assert_eq!(
        error_code(
            &alice
                .send_keyed(conversation, connection::random_key(), "anyone?")
                .await
        ),
        Some(v1::error::Code::NotFound)
    );
    let mut new_bob = nuschl(&fixture, "Bob", 1).await;
    let new_id = new_bob.register().await;
    assert_ne!(new_id, bob_id, "registration numbers are never reused");
    assert_eq!(
        error_code(&new_bob.history(conversation, latest(), 10).await),
        Some(v1::error::Code::NotFound)
    );
}

// ---- §7 read markers and typing ----

#[tokio::test]
async fn read_markers_sync_across_devices_and_typing_is_never_stored() {
    let fixture = Fixture::start("").await;
    let mut phone = nuschl(&fixture, "Alice", 0).await;
    phone.register().await;
    let root = phone.conversations[&0];
    for n in 1..=3 {
        phone.send(root, &format!("m{n}")).await;
    }
    // Another device of the same account (Mumble allows one connection per
    // account at a time: this login replaces the first).
    let mut laptop = nuschl(&fixture, "Alice", 0).await;
    drop(phone);
    let marker = |seq| {
        NuschlRequest::ReadMarker(v1::ReadMarker {
            conversation: root.to_vec(),
            seq,
        })
    };
    laptop.command(Command::Nuschl(marker(2))).await;
    laptop.command(Command::Nuschl(marker(1))).await;
    laptop.command(Command::Nuschl(marker(99))).await;
    let state = laptop.subscribe(&[root]).await;
    assert_eq!(
        state.conversations[0].read, 2,
        "monotonic and at most the head"
    );
    assert_eq!(state.conversations[0].head, 3);
    // Durable: after a restart the marker is still there.
    drop(laptop);
    let fixture = fixture.restart("").await;
    let mut laptop = nuschl(&fixture, "Alice", 0).await;
    assert_eq!(laptop.subscribe(&[root]).await.conversations[0].read, 2);
    // Typing reaches readers and is rate-limited, never stored.
    let mut bob = nuschl(&fixture, "Bob", 1).await;
    let typing = NuschlRequest::Typing(v1::Typing {
        conversation: root.to_vec(),
        session: 0,
    });
    laptop.command(Command::Nuschl(typing.clone())).await;
    laptop.command(Command::Nuschl(typing)).await;
    let typer = bob
        .wait(|event| match event {
            SessionEvent::Nuschl(NuschlEvent::Typing(typing)) => Some(typing.session),
            _ => None,
        })
        .await;
    assert_eq!(typer, laptop.session);
    assert!(
        !bob.arrives(QUIET, |event| matches!(
            event,
            SessionEvent::Nuschl(NuschlEvent::Typing(_))
        ))
        .await,
        "one per three seconds"
    );
    assert_eq!(
        laptop.page(root, latest(), 10).await.head,
        3,
        "nothing stored"
    );
}

// ---- §3 channel lifecycle, §9 stock users ----

#[tokio::test]
async fn temporary_history_goes_with_its_channel_and_deleted_channels_are_archived() {
    let fixture = Fixture::start("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    alice.register().await;
    alice
        .command(Command::CreateChannel {
            parent: 0,
            name: "Temp".into(),
            temporary: true,
        })
        .await;
    let (channel, conversation) = alice
        .wait(|event| match event {
            SessionEvent::ChannelInfo { channel, info } if *channel != 0 => {
                Some((*channel, info.conversation.clone()))
            }
            _ => None,
        })
        .await;
    let conversation: [u8; 16] = conversation.try_into().unwrap();
    alice.send(conversation, "temporary words").await;
    // Leaving empties and removes the temporary channel, and its history.
    alice.command(Command::Join(0)).await;
    alice
        .wait(|event| match event {
            SessionEvent::ChannelRemoved(id) if *id == channel => Some(()),
            _ => None,
        })
        .await;
    assert_eq!(
        error_code(&alice.history(conversation, latest(), 10).await),
        Some(v1::error::Code::NotFound)
    );
    // The purge was queued before this write, so it is done once acknowledged.
    let flushed = alice.send(alice.conversations[&0], "sync").await;
    assert_eq!(flushed, 1);
    assert!(
        fixture
            .store
            .lock()
            .unwrap()
            .history
            .messages
            .values()
            .all(|row| row.content != "temporary words")
    );
    // A deleted permanent channel's history stays, for administrators only.
    let (side, side_conversation) = fixture.add_channel("Side", vec![], vec![]);
    alice.send(side_conversation, "kept").await;
    {
        let mut state = fixture.running.server.lock();
        state.remove_channel(side, None);
        state.finish();
    }
    assert_eq!(
        error_code(&alice.history(side_conversation, latest(), 10).await),
        Some(v1::error::Code::NotFound)
    );
    let state = fixture.running.server.lock();
    assert_eq!(
        state.conversations.kind(&side_conversation),
        Some(nuschld::conversations::Kind::Archived)
    );
}

#[tokio::test]
async fn stock_clients_are_told_that_text_is_kept() {
    let fixture = Fixture::start("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    alice.register().await;
    let (side, side_conversation) = fixture.add_channel("Side", vec![], vec![]);
    alice.send(side_conversation, "history here").await;
    let mut carol = Raw::connect(fixture.address(), &[]).await;
    carol.version(None).await;
    carol.authenticate("Carol").await;
    let sync =
        proto::ServerSync::decode(carol.until(kind::SERVER_SYNC).await.unwrap().payload).unwrap();
    assert!(
        sync.welcome_text
            .unwrap()
            .contains("kept without a time limit")
    );
    carol
        .send(
            kind::USER_STATE,
            &proto::UserState {
                channel_id: Some(side),
                ..Default::default()
            },
        )
        .await;
    let notice = carol.text().await;
    assert!(
        notice
            .message
            .contains("people who join a channel later can read it")
    );
    assert_eq!(notice.actor, None, "from the server");
    // Nuschl clients are not told this way (their client knows).
    assert!(
        !alice
            .arrives(
                QUIET,
                |event| matches!(event, SessionEvent::Welcome(text) if text.contains("kept"))
            )
            .await
    );
}

#[tokio::test]
async fn the_operator_erases_a_users_messages() {
    let fixture = Fixture::start("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    alice.register().await;
    let root = alice.conversations[&0];
    alice.send(root, "personal data").await;
    let mut bob = nuschl(&fixture, "Bob", 1).await;
    bob.register().await;
    bob.send(root, "stays").await;
    let (account, name) =
        nuschld::server::find_account(&fixture.running.server.lock(), "alice").unwrap();
    assert_eq!(name, "Alice");
    let erased = nuschld::server::erase_account(&fixture.running.server.storage, account).await;
    assert_eq!(erased.unwrap(), 1);
    let page = bob.page(root, latest(), 10).await;
    assert!(page.messages[0].content.is_empty() && page.messages[0].deleted_ms > 0);
    assert_eq!(page.messages[0].delete_reason, 3);
    assert_eq!(page.messages[1].content, "stays");
}

// ---- §6 retention ----

/// `ChannelSettings` with only a retention.
fn retention_settings(channel: u32, kind: v1::retention::Kind, confirm: bool) -> NuschlRequest {
    NuschlRequest::ChannelSettings(v1::ChannelSettings {
        request_id: request_id(),
        channel,
        retention: Some(v1::Retention { kind: Some(kind) }),
        confirm_expiry: confirm,
        ..Default::default()
    })
}

impl Session {
    /// The next `ChannelInfo` for `channel`.
    async fn channel_info(&mut self, channel: u32) -> v1::ChannelInfo {
        self.wait(|event| match event {
            SessionEvent::ChannelInfo { channel: id, info } if *id == channel => Some(info.clone()),
            _ => None,
        })
        .await
    }
}

/// Make a stored message look `days` old.
fn backdate(fixture: &Fixture, conversation: &[u8; 16], seq: u64, days: i64) {
    let mut store = fixture.store.lock().unwrap();
    let row = store
        .history
        .messages
        .get_mut(&(nuschld::conversations::hex(conversation), seq))
        .unwrap();
    row.sent_ms -= days * 86_400_000;
}

#[tokio::test]
async fn channel_retention_needs_write_and_shortening_needs_confirmation() {
    use v1::retention::Kind;
    let fixture = Fixture::start("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    alice.register().await;
    let mut bob = nuschl(&fixture, "Bob", 1).await;
    let bob_id = bob.register().await;
    let (channel, conversation) = fixture.add_channel(
        "Kept",
        vec![entry("", Some(bob_id), perm::WRITE, 0)],
        vec![],
    );
    let old = alice.send(conversation, "old words").await;
    let recent = alice.send(conversation, "recent words").await;
    backdate(&fixture, &conversation, old, 10);
    // Forever by default, and shown per channel.
    let info = {
        let state = fixture.running.server.lock();
        state.channel_info(state.channels.get(channel).unwrap())
    };
    assert_eq!(
        (info.retention_days, info.live_only, info.own_retention),
        (0, false, false)
    );
    // Without Write it is the uniform error.
    let refused = alice
        .request(retention_settings(channel, Kind::Days(7), true))
        .await;
    assert_eq!(error_code(&refused), Some(v1::error::Code::NotFound));
    // Shortening removes stored messages: refused until confirmed.
    let unconfirmed = bob
        .request(retention_settings(channel, Kind::Days(7), false))
        .await;
    assert_eq!(
        error_code(&unconfirmed),
        Some(v1::error::Code::ConfirmationRequired)
    );
    assert_eq!(
        fixture.running.server.lock().retention(&conversation),
        nuschl_ipc::server::Retention::Forever,
        "nothing changed"
    );
    let done = bob
        .request(retention_settings(channel, Kind::Days(7), true))
        .await;
    assert!(matches!(done, NuschlEvent::Done(_)), "{done:?}");
    let info = alice.channel_info(channel).await;
    assert_eq!(
        (info.retention_days, info.live_only, info.own_retention),
        (7, false, true)
    );
    // Older messages are not served at once, before the expiry runs.
    let page = alice.page(conversation, latest(), 10).await;
    assert_eq!(seqs(&page), [old, recent]);
    assert!(page.messages[0].content.is_empty() && page.messages[0].deleted_ms > 0);
    assert_eq!(page.messages[0].delete_reason, 2, "expired");
    assert_eq!(page.messages[1].content, "recent words");
    // The expiry tombstones them in storage.
    let server = &fixture.running.server;
    let expired = nuschld::retention::expire_now(server, &server.storage).await;
    assert_eq!(expired, Some(1));
    {
        let store = fixture.store.lock().unwrap();
        let rows = &store.history.messages;
        let hex = nuschld::conversations::hex(&conversation);
        assert_eq!(rows[&(hex.clone(), old)].content, "");
        assert_eq!(rows[&(hex.clone(), old)].delete_reason, Some(2));
        assert_eq!(rows[&(hex, recent)].content, "recent words");
    }
    assert_eq!(
        nuschld::retention::expire_now(server, &server.storage).await,
        Some(0)
    );
    // Keeping more needs no confirmation; back to the server's default.
    let done = bob
        .request(retention_settings(channel, Kind::Forever(true), false))
        .await;
    assert!(matches!(done, NuschlEvent::Done(_)), "{done:?}");
    let done = bob
        .request(retention_settings(
            channel,
            Kind::ServerDefault(true),
            false,
        ))
        .await;
    assert!(matches!(done, NuschlEvent::Done(_)), "{done:?}");
    // Malformed: zero days.
    let malformed = bob
        .request(retention_settings(channel, Kind::Days(0), true))
        .await;
    assert_eq!(error_code(&malformed), Some(v1::error::Code::Malformed));
    // A channel's own retention survives a restart, and so does its archive's.
    let done = bob
        .request(retention_settings(channel, Kind::Days(30), true))
        .await;
    assert!(matches!(done, NuschlEvent::Done(_)), "{done:?}");
    // Synchronise with the history writer before restarting.
    alice.send(conversation, "sync").await;
    drop((alice, bob));
    let fixture = fixture.restart("").await;
    let state = fixture.running.server.lock();
    assert_eq!(
        state.retention(&conversation),
        nuschl_ipc::server::Retention::Days(30)
    );
}

#[tokio::test]
async fn live_only_channels_deliver_without_storing() {
    use v1::retention::Kind;
    let fixture = Fixture::start("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    alice.register().await;
    let mut bob = nuschl(&fixture, "Bob", 1).await;
    let bob_id = bob.register().await;
    let (channel, conversation) = fixture.add_channel(
        "Live",
        vec![entry("", Some(bob_id), perm::WRITE, 0)],
        vec![],
    );
    let before = alice.send(conversation, "said before").await;
    let done = bob
        .request(retention_settings(channel, Kind::LiveOnly(true), true))
        .await;
    assert!(matches!(done, NuschlEvent::Done(_)), "{done:?}");
    let info = alice.channel_info(channel).await;
    assert!(info.live_only && info.own_retention);
    bob.subscribe(&[conversation]).await;
    // Acknowledged with the next seq and delivered live, but not stored.
    let seq = alice.send(conversation, "live words").await;
    assert_eq!(seq, before + 1);
    let chat = bob.chat().await;
    assert_eq!(chat.message.unwrap().content, "live words");
    let hex = nuschld::conversations::hex(&conversation);
    {
        let store = fixture.store.lock().unwrap();
        assert!(
            store
                .history
                .messages
                .values()
                .all(|row| row.content != "live words")
        );
    }
    // No pages, not even of what was kept before; no edits.
    let page = alice.page(conversation, latest(), 10).await;
    assert!(page.messages.is_empty() && !page.more_before);
    assert_eq!(page.head, seq);
    let edit = alice
        .request(NuschlRequest::Edit(v1::ChatEdit {
            request_id: request_id(),
            conversation: conversation.to_vec(),
            target: seq,
            format: v1::Format::Text as i32,
            content: "changed".into(),
        }))
        .await;
    assert_eq!(error_code(&edit), Some(v1::error::Code::NotFound));
    // Stock clients entering it are told; a stock sender's text is not kept.
    let mut carol = Raw::login(fixture.address(), "Carol").await;
    carol
        .send(
            kind::USER_STATE,
            &proto::UserState {
                channel_id: Some(channel),
                ..Default::default()
            },
        )
        .await;
    let notice = carol.text().await;
    assert!(
        notice.message.contains("not archived"),
        "{}",
        notice.message
    );
    carol
        .send(
            kind::TEXT_MESSAGE,
            &proto::TextMessage {
                channel_id: vec![channel],
                message: "stock live words".into(),
                ..Default::default()
            },
        )
        .await;
    let chat = bob.chat().await;
    let message = chat.message.unwrap();
    assert_eq!(
        (message.seq, message.content.as_str()),
        (seq + 1, "stock live words")
    );
    // What was stored before expires.
    alice.send(conversation, "sync").await;
    let server = &fixture.running.server;
    assert_eq!(
        nuschld::retention::expire_now(server, &server.storage).await,
        Some(1)
    );
    {
        let store = fixture.store.lock().unwrap();
        assert_eq!(store.history.messages[&(hex.clone(), before)].content, "");
        assert!(
            store
                .history
                .messages
                .values()
                .all(|row| !row.content.contains("live words"))
        );
    }
    // Sequence numbers are never reused after a restart.
    drop((alice, bob, carol));
    let fixture = fixture.restart("").await;
    let head = fixture
        .running
        .server
        .lock()
        .conversations
        .head(&conversation);
    assert_eq!(head, seq + 2);
}

#[tokio::test]
async fn the_server_default_and_maximum_retention() {
    let fixture = Fixture::start("[history]\nretention = \"none\"").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    let root = alice.conversations[&0];
    let policy = {
        let state = fixture.running.server.lock();
        state.channel_info(state.channels.get(0).unwrap())
    };
    assert!(policy.live_only && !policy.own_retention);
    alice.send(root, "not kept").await;
    alice.send(root, "sync").await;
    assert!(fixture.store.lock().unwrap().history.messages.is_empty());
    let mut carol = Raw::connect(fixture.address(), &[]).await;
    carol.version(None).await;
    carol.authenticate("Carol").await;
    let sync =
        proto::ServerSync::decode(carol.until(kind::SERVER_SYNC).await.unwrap().payload).unwrap();
    assert!(sync.welcome_text.unwrap().contains("not archived"));
    drop((alice, carol));
    // A maximum caps channels that keep more.
    let fixture = fixture.restart("[history]\nmax_retention_days = 30").await;
    let mut state = fixture.running.server.lock();
    state.set_retention(root, Some(nuschl_ipc::server::Retention::Forever));
    assert_eq!(
        state.retention(&root),
        nuschl_ipc::server::Retention::Days(30)
    );
    let info = state.channel_info(state.channels.get(0).unwrap());
    assert_eq!((info.retention_days, info.own_retention), (30, true));
}

#[tokio::test]
async fn history_pages_are_limited_per_network_and_account() {
    let fixture =
        Fixture::start("[history]\nguests = \"full\"\nnetwork_pages_per_minute = 6").await;
    // Two unregistered connections from 127.0.0.0/24 share a burst of two.
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    let mut bob = nuschl(&fixture, "Bob", 1).await;
    let root = alice.conversations[&0];
    alice.page(root, latest(), 10).await;
    bob.page(root, latest(), 10).await;
    let limited = alice.history(root, latest(), 10).await;
    assert_eq!(error_code(&limited), Some(v1::error::Code::RateLimited));
    let NuschlEvent::Error(error) = limited else {
        unreachable!()
    };
    assert!(
        (1..=10_000).contains(&error.retry_after_ms),
        "a hint the client can honour: {}",
        error.retry_after_ms
    );
    // A registered user has a budget of their own, and the connection stays.
    let mut carol = nuschl(&fixture, "Carol", 2).await;
    carol.register().await;
    carol.page(root, latest(), 10).await;
    carol.page(root, latest(), 10).await;
    alice.send(root, "still connected").await;
    drop((alice, bob, carol));
    // Exempt networks are not limited per network.
    let fixture = fixture
        .restart(
            "[history]\nguests = \"full\"\nnetwork_pages_per_minute = 6\n\
             unlimited_networks = [\"127.0.0.0/8\"]",
        )
        .await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    for _ in 0..5 {
        alice.page(root, latest(), 10).await;
    }
}

#[tokio::test]
async fn stock_direct_messages_are_kept_only_when_the_operator_says_so() {
    for archive in [false, true] {
        let fixture = Fixture::start(&format!(
            "[history]\narchive_stock_direct_messages = {archive}"
        ))
        .await;
        let mut alice = nuschl(&fixture, "Alice", 0).await;
        alice.register().await;
        let mut carol = Raw::login(fixture.address(), "Carol").await;
        carol
            .send(
                kind::USER_STATE,
                &proto::UserState {
                    user_id: Some(0),
                    ..Default::default()
                },
            )
            .await;
        loop {
            let frame = carol.until(kind::USER_STATE).await.unwrap();
            let state = proto::UserState::decode(frame.payload).unwrap();
            if state.name.is_none() && state.user_id.is_some_and(|id| id != u32::MAX) {
                break;
            }
        }
        carol
            .send(
                kind::TEXT_MESSAGE,
                &proto::TextMessage {
                    session: vec![alice.session],
                    message: "psst".into(),
                    ..Default::default()
                },
            )
            .await;
        if archive {
            let event = alice.chat().await;
            let message = event.message.unwrap();
            assert_eq!(message.format, v1::Format::LegacyHtml as i32);
            assert_eq!(message.content, "psst");
            let kind = fixture
                .running
                .server
                .lock()
                .conversations
                .kind(&event.conversation.try_into().unwrap());
            assert!(matches!(
                kind,
                Some(nuschld::conversations::Kind::Direct(..))
            ));
        } else {
            let text = alice
                .wait(|event| match event {
                    SessionEvent::Text(text) => Some(text.body.clone()),
                    _ => None,
                })
                .await;
            assert_eq!(text, "psst");
            assert!(fixture.store.lock().unwrap().history.messages.is_empty());
        }
    }
}

// ---- §8 the client: channel text as durable chat ----

#[tokio::test]
async fn the_session_sends_channel_text_durably_and_resends_after_a_reconnect() {
    let fixture = Fixture::start("").await;
    let mut alice = nuschl(&fixture, "Alice", 0).await;
    alice.register().await;
    let mut bob = nuschl(&fixture, "Bob", 1).await;
    bob.register().await;
    let root = alice.conversations[&0];
    // Plain SendText becomes ChatSend: pending, then acknowledged with a seq.
    alice
        .command(Command::SendText {
            target: connection::TextTarget::Channel(0),
            body: "hello there".into(),
            expected_peer: None,
        })
        .await;
    let pending = alice
        .wait(|event| match event {
            SessionEvent::ChatPending(pending) => Some(pending.clone()),
            _ => None,
        })
        .await;
    let shown = alice
        .wait(|event| match event {
            SessionEvent::Text(text) if text.outgoing => Some(text.clone()),
            _ => None,
        })
        .await;
    assert_eq!(shown.pending, Some(pending.key));
    let (key, seq) = alice
        .wait(|event| match event {
            SessionEvent::ChatAcked { key, seq } => Some((*key, *seq)),
            _ => None,
        })
        .await;
    assert_eq!((key, seq), (pending.key, 1));
    // Bob sees it as text with its seq, not as a stock TextMessage.
    let text = bob
        .wait(|event| match event {
            SessionEvent::Text(text) => Some(text.clone()),
            _ => None,
        })
        .await;
    assert_eq!((text.body.as_str(), text.seq), ("hello there", Some(1)));
    assert_eq!(text.author, "Alice");
    // After a reconnect, an unacknowledged message goes again with its key:
    // stored once.
    drop(alice);
    let mut again = options(fixture.address(), &fixture.running.fingerprint, "Alice", 0);
    again.resend = vec![pending.clone()];
    let (mut alice, _) = Session::connect(again).await;
    let (key, seq) = alice
        .wait(|event| match event {
            SessionEvent::ChatAcked { key, seq } => Some((*key, *seq)),
            _ => None,
        })
        .await;
    assert_eq!(
        (key, seq),
        (pending.key, 1),
        "the original seq, not a second copy"
    );
    assert!(
        !bob.arrives(QUIET, |event| matches!(event, SessionEvent::Text(_)))
            .await
    );
    // The session subscribed to the channels and reports their state.
    let state = alice
        .wait(|event| match event {
            SessionEvent::ConversationState {
                channel: 0, head, ..
            } => Some(*head),
            _ => None,
        })
        .await;
    assert!(state >= 1);
    // Read markers from the session reach the server.
    alice
        .command(Command::MarkRead { channel: 0, seq: 2 })
        .await;
    let marker = alice.subscribe(&[root]).await.conversations[0].read;
    assert_eq!(marker, 2);
}

// ---- share/1: share links (docs/web.md) ----

fn create(channel: u32, expires_at: u64, access_token: &str) -> NuschlRequest {
    NuschlRequest::ShareLinkCreate(v1::ShareLinkCreate {
        request_id: request_id(),
        channel,
        expires_at,
        access_token: access_token.into(),
    })
}

fn resolve(token: &[u8]) -> NuschlRequest {
    NuschlRequest::ShareLinkResolve(v1::ShareLinkResolve {
        request_id: request_id(),
        token: token.to_vec(),
    })
}

fn revoke(token: &[u8]) -> NuschlRequest {
    NuschlRequest::ShareLinkRevoke(v1::ShareLinkRevoke {
        request_id: request_id(),
        token: token.to_vec(),
    })
}

fn share_link(event: NuschlEvent) -> v1::ShareLink {
    match event {
        NuschlEvent::ShareLink(link) => link,
        other => panic!("expected a share link, got {other:?}"),
    }
}

#[tokio::test]
async fn share_links_open_channels_until_revoked_or_expired() {
    let fixture = Fixture::start("[share_links]\nweb_host = \"web.example.org\"").await;
    // A private channel: only the access token "door" may enter.
    let (private, _) = fixture.add_channel(
        "Private",
        vec![
            entry("all", None, 0, perm::ENTER),
            entry("#door", None, perm::ENTER, 0),
        ],
        vec![],
    );
    let (open, _) = fixture.add_channel("Open", vec![], vec![]);
    let mut owner = nuschl(&fixture, "owner", 0).await;
    let owner_id = owner.register().await;
    let mut guest = nuschl(&fixture, "guest", 1).await;

    // Anyone registered who may enter makes a plain link; the guest may not
    // (no durable identity), and nobody may for a channel they cannot enter.
    let plain = share_link(owner.request(create(open, 0, "")).await);
    assert_eq!(plain.channel, open);
    assert_eq!(plain.token.len(), 16);
    assert_eq!(plain.web_host, "web.example.org");
    assert!(plain.access_token.is_empty());
    let answer = guest.request(create(open, 0, "")).await;
    assert_eq!(error_code(&answer), Some(v1::error::Code::NotFound));
    for channel in [private, 9999] {
        let answer = owner.request(create(channel, 0, "")).await;
        assert_eq!(error_code(&answer), Some(v1::error::Code::NotFound));
    }

    // With Write on the channel, a link may carry its access token.
    fixture.edit_channel(private, |channel| {
        channel
            .node
            .acls
            .push(entry("", Some(owner_id), perm::ENTER | perm::WRITE, 0));
    });
    let door = share_link(owner.request(create(private, 0, "door")).await);
    assert_eq!(door.access_token, "door");
    let past = owner.request(create(private, 1, "door")).await;
    assert_eq!(error_code(&past), Some(v1::error::Code::Malformed));

    // A guest resolves it: the channel and the token that lets them in.
    let resolved = share_link(guest.request(resolve(&door.token)).await);
    assert_eq!(
        (resolved.channel, resolved.access_token.as_str()),
        (private, "door")
    );
    let unknown = guest.request(resolve(&[0; 16])).await;
    assert_eq!(error_code(&unknown), Some(v1::error::Code::NotFound));
    let malformed = guest.request(resolve(&[0; 3])).await;
    assert_eq!(error_code(&malformed), Some(v1::error::Code::Malformed));
    // Only the creator (or Write on the channel) revokes.
    let refused = guest.request(revoke(&door.token)).await;
    assert_eq!(error_code(&refused), Some(v1::error::Code::NotFound));

    // Links survive a restart; revoked ones do not resolve.
    {
        let state = fixture.running.server.lock();
        state.persist_channel(private);
        state.persist_channel(open);
    }
    drop(owner);
    drop(guest);
    let fixture = fixture.restart("").await;
    let mut owner = nuschl(&fixture, "owner", 0).await;
    let again = share_link(owner.request(resolve(&door.token)).await);
    assert_eq!(again.channel, private);
    assert!(
        again.web_host.is_empty(),
        "the web host is the configuration's"
    );
    let done = owner.request(revoke(&door.token)).await;
    assert!(matches!(done, NuschlEvent::Done(_)), "{done:?}");
    let gone = owner.request(resolve(&door.token)).await;
    assert_eq!(error_code(&gone), Some(v1::error::Code::NotFound));
    let plain = share_link(owner.request(resolve(&plain.token)).await);
    assert_eq!(plain.channel, open);
    let _ = fixture;
}
