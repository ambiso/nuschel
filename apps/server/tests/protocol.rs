// SPDX-License-Identifier: EUPL-1.2
//! The server on loopback against a small protocol client written for these
//! tests: authentication and rejections, the certificate checks, channels,
//! text, ACL denial, voice over the TLS tunnel and encrypted UDP, persistence
//! across a restart, and hostile frames. Stock-client interoperability is
//! covered by tests/with_local_nuschld.py and the VM test.
use bytes::BytesMut;
use nuschl_protocol::{
    ControlFrame, control as proto, crypt::CryptState, decode_control, encode_control,
    encode_tunnel, kind, udp,
};
use nuschld::{
    config::Config,
    server::{Running, start},
    storage::{Backend, MemoryStore},
};
use prost::Message;
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::{
        ResolvesClientCert,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    sign::CertifiedKey,
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf},
    net::{TcpStream, UdpSocket},
    time::timeout,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

const WAIT: Duration = Duration::from_secs(10);
const SUPERUSER_PASSWORD: &str = "super secret";

struct Identity {
    certificate: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
}

fn identity() -> Identity {
    let generated = rcgen::generate_simple_self_signed(vec!["test client".into()]).unwrap();
    Identity {
        certificate: generated.cert.der().clone(),
        key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            generated.signing_key.serialize_der(),
        )),
    }
}

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

/// Presents a fixed certificate with a fixed (possibly unrelated) key.
#[derive(Debug)]
struct Presenting(Arc<CertifiedKey>);

impl ResolvesClientCert for Presenting {
    fn resolve(&self, _: &[&[u8]], _: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
    fn has_certs(&self) -> bool {
        true
    }
}

async fn tls(
    address: SocketAddr,
    certificate: Option<(CertificateDer<'static>, PrivateKeyDer<'static>)>,
) -> std::io::Result<TlsStream<TcpStream>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyServer(provider.clone())));
    let config = match certificate {
        Some((certificate, key)) => {
            let signing = provider.key_provider.load_private_key(key).unwrap();
            builder.with_client_cert_resolver(Arc::new(Presenting(Arc::new(CertifiedKey::new(
                vec![certificate],
                signing,
            )))))
        }
        None => builder.with_no_client_auth(),
    };
    let stream = TcpStream::connect(address).await?;
    TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), stream)
        .await
}

struct Client {
    reader: ReadHalf<TlsStream<TcpStream>>,
    writer: WriteHalf<TlsStream<TcpStream>>,
    buffer: BytesMut,
    session: u32,
    crypt: Option<CryptState>,
    seen: Vec<ControlFrame>,
}

struct Options<'a> {
    name: &'a str,
    password: Option<&'a str>,
    identity: Option<&'a Identity>,
    version: u64,
}

impl<'a> Options<'a> {
    fn new(name: &'a str, identity: &'a Identity) -> Self {
        Self {
            name,
            password: None,
            identity: Some(identity),
            version: (1 << 48) | (5 << 32),
        }
    }
}

impl Client {
    async fn connect(address: SocketAddr, options: Options<'_>) -> Result<Self, proto::Reject> {
        let certificate = options
            .identity
            .map(|identity| (identity.certificate.clone(), identity.key.clone_key()));
        let stream = tls(address, certificate).await.unwrap();
        let (reader, writer) = tokio::io::split(stream);
        let mut client = Self {
            reader,
            writer,
            buffer: BytesMut::new(),
            session: 0,
            crypt: None,
            seen: Vec::new(),
        };
        let version = client.next().await.expect("server version");
        assert_eq!(version.kind, kind::VERSION, "the server speaks first");
        client
            .send(
                kind::VERSION,
                &proto::Version {
                    version_v2: Some(options.version),
                    release: Some("test client".into()),
                    ..Default::default()
                },
            )
            .await;
        client
            .send(
                kind::AUTHENTICATE,
                &proto::Authenticate {
                    username: Some(options.name.into()),
                    password: options.password.map(str::to_owned),
                    opus: Some(true),
                    ..Default::default()
                },
            )
            .await;
        loop {
            let frame = client.next().await.expect("connection closed during login");
            match frame.kind {
                kind::REJECT => return Err(proto::Reject::decode(frame.payload).unwrap()),
                kind::CRYPT_SETUP => {
                    let setup = proto::CryptSetup::decode(frame.payload.clone()).unwrap();
                    client.crypt = Some(
                        CryptState::client(
                            setup.key.as_deref().unwrap(),
                            setup.client_nonce.as_deref().unwrap(),
                            setup.server_nonce.as_deref().unwrap(),
                        )
                        .unwrap(),
                    );
                    client.seen.push(frame);
                }
                kind::SERVER_SYNC => {
                    client.session = proto::ServerSync::decode(frame.payload.clone())
                        .unwrap()
                        .session();
                    client.seen.push(frame);
                    return Ok(client);
                }
                _ => client.seen.push(frame),
            }
        }
    }

    async fn send(&mut self, kind: u16, message: &impl Message) {
        let frame = encode_control(kind, message).unwrap();
        self.writer.write_all(&frame).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn send_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes).await?;
        self.writer.flush().await
    }

    /// The next frame, or `None` when the connection ends or nothing arrives.
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

    /// Skip frames until one of `kind` satisfies `predicate`.
    async fn expect<M: Message + Default>(
        &mut self,
        kind: u16,
        predicate: impl Fn(&M) -> bool,
    ) -> M {
        loop {
            let frame = self
                .next()
                .await
                .unwrap_or_else(|| panic!("no frame of kind {kind}"));
            self.seen.push(frame.clone());
            if frame.kind == kind {
                let message = M::decode(frame.payload).unwrap();
                if predicate(&message) {
                    return message;
                }
            }
        }
    }

    /// Whether a frame of `kind` arrives within `wait` (frames are consumed).
    async fn receives(&mut self, kind: u16, wait: Duration) -> Option<ControlFrame> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Ok(Some(frame)) = decode_control(&mut self.buffer) {
                if frame.kind == kind {
                    return Some(frame);
                }
                continue;
            }
            match tokio::time::timeout_at(deadline, self.reader.read_buf(&mut self.buffer)).await {
                Ok(Ok(n)) if n > 0 => {}
                _ => return None,
            }
        }
    }

    async fn sync(&mut self) {
        // A ping round trip: everything the server sent before has arrived.
        let stamp = rand_stamp();
        self.send(
            kind::PING,
            &proto::Ping {
                timestamp: Some(stamp),
                ..Default::default()
            },
        )
        .await;
        self.expect::<proto::Ping>(kind::PING, |ping| ping.timestamp == Some(stamp))
            .await;
    }
}

fn rand_stamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

struct Fixture {
    running: Running,
    store: Arc<Mutex<MemoryStore>>,
    directory: tempfile::TempDir,
}

impl Fixture {
    async fn start(extra: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        Self::start_in(directory, MemoryStore::shared(), extra).await
    }

    async fn start_in(
        directory: tempfile::TempDir,
        store: Arc<Mutex<MemoryStore>>,
        extra: &str,
    ) -> Self {
        let password = directory.path().join("superuser");
        std::fs::write(&password, SUPERUSER_PASSWORD).unwrap();
        let config = Config::from_toml(&format!(
            "bind = [\"127.0.0.1\"]\nport = 0\nstate_directory = {:?}\nsuperuser_password_file = {:?}\n\
             welcome_text = \"hello\"\nmessage_limit = 1000\nmessage_burst = 1000\n{extra}\n\
             [autoban]\nattempts = 0\n[password_hashing]\nmemory_kib = 64\npasses = 1\n",
            directory.path(),
            password
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

fn reject_type(reject: &proto::Reject) -> proto::reject::RejectType {
    reject.r#type()
}

#[tokio::test]
async fn login_sends_murmurs_initial_state() {
    let fixture = Fixture::start("").await;
    let alice = identity();
    let client = Client::connect(fixture.address(), Options::new("Alice", &alice))
        .await
        .unwrap();
    let kinds: Vec<u16> = client.seen.iter().map(|frame| frame.kind).collect();
    let position = |wanted| {
        kinds
            .iter()
            .position(|kind| *kind == wanted)
            .unwrap_or_else(|| panic!("missing {wanted}"))
    };
    assert!(position(kind::CRYPT_SETUP) < position(kind::CODEC_VERSION));
    assert!(position(kind::CODEC_VERSION) < position(kind::CHANNEL_STATE));
    assert!(position(kind::CHANNEL_STATE) < position(kind::USER_STATE));
    assert!(position(kind::USER_STATE) < position(kind::SERVER_SYNC));
    let codec = client
        .seen
        .iter()
        .find(|frame| frame.kind == kind::CODEC_VERSION)
        .unwrap();
    assert_eq!(
        proto::CodecVersion::decode(codec.payload.clone())
            .unwrap()
            .opus,
        Some(true)
    );
    let sync = client
        .seen
        .iter()
        .find(|frame| frame.kind == kind::SERVER_SYNC)
        .unwrap();
    let sync = proto::ServerSync::decode(sync.payload.clone()).unwrap();
    // Stock clients are told that text is kept (docs/extension.md §9).
    let welcome = sync.welcome_text.clone().unwrap();
    assert!(welcome.starts_with("hello<br/><br/>Text in this server's channels is kept"));
    assert!(sync.permissions() & 0x4 != 0, "Enter at the root");
    let me = client
        .seen
        .iter()
        .filter(|frame| frame.kind == kind::USER_STATE)
        .map(|frame| proto::UserState::decode(frame.payload.clone()).unwrap())
        .find(|state| state.session == Some(client.session))
        .unwrap();
    assert_eq!(me.name.as_deref(), Some("Alice"));
    assert_eq!(
        me.hash.as_deref().map(str::len),
        Some(40),
        "SHA-1 certificate hash"
    );
    let mut client = client;
    client
        .expect::<proto::ServerConfig>(kind::SERVER_CONFIG, |_| true)
        .await;
    fixture.running.shutdown().await;
}

#[tokio::test]
async fn rejections_use_murmurs_reasons() {
    use proto::reject::RejectType;
    let fixture = Fixture::start("password = \"letmein\"\n").await;
    let alice = identity();
    let mut options = Options::new("Alice", &alice);
    let reject = Client::connect(fixture.address(), options)
        .await
        .err()
        .unwrap();
    assert_eq!(reject_type(&reject), RejectType::WrongServerPw);
    options = Options::new("bad>name", &alice);
    options.password = Some("letmein");
    assert_eq!(
        reject_type(
            &Client::connect(fixture.address(), options)
                .await
                .err()
                .unwrap()
        ),
        RejectType::InvalidUsername
    );
    // SuperUser needs its own password; the server password does not do.
    options = Options::new("SuperUser", &alice);
    options.password = Some("letmein");
    assert_eq!(
        reject_type(
            &Client::connect(fixture.address(), options)
                .await
                .err()
                .unwrap()
        ),
        RejectType::WrongUserPw
    );
    options = Options::new("Alice", &alice);
    options.password = Some("letmein");
    let _first = Client::connect(fixture.address(), options).await.unwrap();
    // Someone else (another certificate) cannot take an unregistered name.
    let mallory = identity();
    options = Options::new("alice", &mallory);
    options.password = Some("letmein");
    assert_eq!(
        reject_type(
            &Client::connect(fixture.address(), options)
                .await
                .err()
                .unwrap()
        ),
        RejectType::UsernameInUse
    );

    // Password guesses against an account are limited: after ten failures even
    // the right password is answered with "try again later" for a while (the
    // SuperUser attempt with the server password above was the first).
    for _ in 0..9 {
        let mut guess = Options::new("SuperUser", &alice);
        guess.password = Some("wrong");
        assert_eq!(
            reject_type(
                &Client::connect(fixture.address(), guess)
                    .await
                    .err()
                    .unwrap()
            ),
            RejectType::WrongUserPw
        );
    }
    let mut right = Options::new("SuperUser", &alice);
    right.password = Some(SUPERUSER_PASSWORD);
    assert_eq!(
        reject_type(
            &Client::connect(fixture.address(), right)
                .await
                .err()
                .unwrap()
        ),
        RejectType::AuthenticatorFail
    );
    fixture.running.shutdown().await;

    let fixture = Fixture::start("certificate_required = true\n").await;
    let options = Options {
        identity: None,
        ..Options::new("Anonymous", &alice)
    };
    assert_eq!(
        reject_type(
            &Client::connect(fixture.address(), options)
                .await
                .err()
                .unwrap()
        ),
        RejectType::NoCertificate
    );
    fixture.running.shutdown().await;
}

/// R1: presenting someone else's certificate without its private key must fail.
#[tokio::test]
async fn a_certificate_without_its_key_is_refused() {
    let fixture = Fixture::start("").await;
    let victim = identity();
    let attacker = identity();
    let mut connected = false;
    if let Ok(stream) = tls(
        fixture.address(),
        Some((victim.certificate.clone(), attacker.key.clone_key())),
    )
    .await
    {
        let (mut reader, _writer) = tokio::io::split(stream);
        let mut buffer = [0_u8; 64];
        // TLS 1.3 checks the client's signature after the client finished; the
        // server then aborts instead of sending its Version.
        connected = matches!(timeout(WAIT, reader.read(&mut buffer)).await, Ok(Ok(n)) if n > 0);
    }
    assert!(
        !connected,
        "the server accepted a certificate without proof of its key"
    );
    assert!(fixture.running.server.lock().users.is_empty());
    fixture.running.shutdown().await;
}

/// A client certificate issued by a configured authority is strong (Murmur's
/// `bVerified`, `strong_certificate_authorities`); a self-signed one logs in as
/// before and is not. R1 still holds for an issued certificate.
#[tokio::test]
async fn certificates_from_a_configured_authority_are_strong() {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Test CA");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
    let ca =
        rcgen::CertifiedIssuer::self_signed(params, rcgen::KeyPair::generate().unwrap()).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Carol");
    let issued = Identity {
        certificate: params.signed_by(&key, &ca).unwrap().der().clone(),
        key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
    };
    let directory = tempfile::tempdir().unwrap();
    let authorities = directory.path().join("authorities.pem");
    std::fs::write(&authorities, ca.pem()).unwrap();
    let fixture = Fixture::start(&format!(
        "strong_certificate_authorities = [{authorities:?}]\n"
    ))
    .await;
    let plain = identity();
    let mut carol = Client::connect(fixture.address(), Options::new("Carol", &issued))
        .await
        .unwrap();
    let mut dave = Client::connect(fixture.address(), Options::new("Dave", &plain))
        .await
        .unwrap();
    for (client, strong) in [(&mut carol, true), (&mut dave, false)] {
        let session = client.session;
        client
            .send(
                kind::USER_STATS,
                &proto::UserStats {
                    session: Some(session),
                    ..Default::default()
                },
            )
            .await;
        let stats: proto::UserStats = client
            .expect(kind::USER_STATS, |stats: &proto::UserStats| {
                stats.session == Some(session)
            })
            .await;
        assert_eq!(stats.strong_certificate, Some(strong));
        assert_eq!(stats.certificates.len(), 1);
    }
    {
        let server = fixture.running.server.lock();
        assert!(server.users[&carol.session].strong);
        assert!(!server.users[&dave.session].strong);
    }
    // The issued certificate with someone else's key is still refused.
    let mut connected = false;
    if let Ok(stream) = tls(
        fixture.address(),
        Some((issued.certificate.clone(), plain.key.clone_key())),
    )
    .await
    {
        let (mut reader, _writer) = tokio::io::split(stream);
        let mut buffer = [0_u8; 64];
        connected = matches!(timeout(WAIT, reader.read(&mut buffer)).await, Ok(Ok(n)) if n > 0);
    }
    assert!(
        !connected,
        "an issued certificate without its key logged in"
    );
    fixture.running.shutdown().await;
}

#[tokio::test]
async fn unauthenticated_peers_cannot_send_large_or_unexpected_frames() {
    let fixture = Fixture::start("").await;
    for header in [
        // An 8 MiB ACL before authentication.
        [0, 13, 0, 0x7f, 0xff, 0xff],
        // A 1 MiB Authenticate.
        [0, 2, 0, 0x10, 0, 0],
    ] {
        let stream = tls(fixture.address(), None).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(stream);
        writer.write_all(&header).await.unwrap();
        writer.flush().await.unwrap();
        let mut sink = Vec::new();
        // The server closes the connection (after its Version) without waiting for the body.
        let closed = timeout(WAIT, reader.read_to_end(&mut sink)).await;
        assert!(closed.is_ok(), "connection stayed open");
    }
    fixture.running.shutdown().await;
}

#[tokio::test]
async fn plugin_data_is_relayed_and_oversized_plugin_frames_are_dropped() {
    let fixture = Fixture::start("").await;
    let (alice_id, bob_id) = (identity(), identity());
    let mut alice = Client::connect(fixture.address(), Options::new("Alice", &alice_id))
        .await
        .unwrap();
    let mut bob = Client::connect(fixture.address(), Options::new("Bob", &bob_id))
        .await
        .unwrap();
    let message = |receivers: Vec<u32>, data: &[u8]| proto::PluginDataTransmission {
        sender_session: Some(9999),
        receiver_sessions: receivers,
        data: Some(data.to_vec()),
        data_id: Some("game".into()),
    };
    alice
        .send(
            kind::PLUGIN_DATA_TRANSMISSION,
            &message(vec![bob.session, bob.session, 9999], b"one"),
        )
        .await;
    let got = bob
        .expect::<proto::PluginDataTransmission>(kind::PLUGIN_DATA_TRANSMISSION, |_| true)
        .await;
    assert_eq!(got.sender_session, Some(alice.session));
    assert!(got.receiver_sessions.is_empty());
    assert_eq!(got.data.as_deref(), Some(&b"one"[..]));
    // Above the fixed ceiling yet within stock Mumble's frame limit: read
    // past and dropped, as Murmur drops it; the connection goes on.
    let ceiling = nuschl_protocol::plugin::max_body(nuschld::connection::PLUGIN_FRAME_RECEIVERS);
    let mut oversized = Vec::new();
    oversized.extend_from_slice(&kind::PLUGIN_DATA_TRANSMISSION.to_be_bytes());
    oversized.extend_from_slice(&((ceiling + 1) as u32).to_be_bytes());
    oversized.resize(oversized.len() + ceiling + 1, 0x2a);
    alice.send_raw(&oversized).await.unwrap();
    alice
        .send(
            kind::PLUGIN_DATA_TRANSMISSION,
            &message(vec![bob.session], b"two"),
        )
        .await;
    let got = bob
        .expect::<proto::PluginDataTransmission>(kind::PLUGIN_DATA_TRANSMISSION, |_| true)
        .await;
    assert_eq!(got.data.as_deref(), Some(&b"two"[..]));
    alice.sync().await;
    // Beyond stock Mumble's frame limit the connection closes, as Murmur's does.
    alice.send_raw(&[0, 26, 0, 0x80, 0, 0]).await.unwrap();
    let mut sink = Vec::new();
    let closed = timeout(WAIT, alice.reader.read_to_end(&mut sink)).await;
    assert!(closed.is_ok(), "connection stayed open");
    bob.sync().await;
    fixture.running.shutdown().await;
}

async fn superuser(address: SocketAddr, identity: &Identity) -> Client {
    let mut options = Options::new("SuperUser", identity);
    options.password = Some(SUPERUSER_PASSWORD);
    Client::connect(address, options).await.unwrap()
}

#[tokio::test]
async fn channels_text_registration_and_acl_denial() {
    use proto::permission_denied::DenyType;
    let fixture = Fixture::start("").await;
    let (alice_id, bob_id, carol_id, admin_id) = (identity(), identity(), identity(), identity());
    let mut admin = superuser(fixture.address(), &admin_id).await;
    let mut alice = Client::connect(fixture.address(), Options::new("Alice", &alice_id))
        .await
        .unwrap();
    let mut bob = Client::connect(fixture.address(), Options::new("Bob", &bob_id))
        .await
        .unwrap();
    let mut carol = Client::connect(fixture.address(), Options::new("Carol", &carol_id))
        .await
        .unwrap();

    // Unregistered users may not create permanent channels (Murmur's default ACLs).
    alice
        .send(
            kind::CHANNEL_STATE,
            &proto::ChannelState {
                parent: Some(0),
                name: Some("Mine".into()),
                ..Default::default()
            },
        )
        .await;
    let denied = alice
        .expect::<proto::PermissionDenied>(kind::PERMISSION_DENIED, |_| true)
        .await;
    assert_eq!(
        (denied.r#type(), denied.permission),
        (DenyType::Permission, Some(0x40))
    );

    // SuperUser creates one; everyone sees it.
    admin
        .send(
            kind::CHANNEL_STATE,
            &proto::ChannelState {
                parent: Some(0),
                name: Some("Lobby".into()),
                ..Default::default()
            },
        )
        .await;
    let lobby = bob
        .expect::<proto::ChannelState>(kind::CHANNEL_STATE, |c| c.name.as_deref() == Some("Lobby"))
        .await;
    let lobby = lobby.channel_id.unwrap();
    assert_eq!(lobby, 1);
    // Duplicate sibling names are refused.
    admin
        .send(
            kind::CHANNEL_STATE,
            &proto::ChannelState {
                parent: Some(0),
                name: Some("Lobby".into()),
                ..Default::default()
            },
        )
        .await;
    let denied = admin
        .expect::<proto::PermissionDenied>(kind::PERMISSION_DENIED, |_| true)
        .await;
    assert_eq!(denied.r#type(), DenyType::ChannelName);

    // Self-registration, then a temporary channel the creator is moved into.
    alice
        .send(
            kind::USER_STATE,
            &proto::UserState {
                session: Some(alice.session),
                user_id: Some(0),
                ..Default::default()
            },
        )
        .await;
    let registered = alice
        .expect::<proto::UserState>(kind::USER_STATE, |s| s.user_id.is_some())
        .await;
    assert_eq!(registered.user_id, Some(1));
    alice
        .send(
            kind::CHANNEL_STATE,
            &proto::ChannelState {
                parent: Some(lobby),
                name: Some("Temp".into()),
                temporary: Some(true),
                ..Default::default()
            },
        )
        .await;
    let temporary = alice
        .expect::<proto::ChannelState>(kind::CHANNEL_STATE, |c| c.name.as_deref() == Some("Temp"))
        .await;
    let temporary = temporary.channel_id.unwrap();
    assert_eq!(
        temporary,
        i32::MAX as u32,
        "temporary channels count down from i32::MAX"
    );
    {
        let me = alice.session;
        alice
            .expect::<proto::UserState>(kind::USER_STATE, move |s| {
                s.session == Some(me) && s.channel_id == Some(temporary)
            })
            .await;
    }

    // Text: Bob joins the temporary channel and hears Alice; Carol (root) does not.
    bob.send(
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(temporary),
            ..Default::default()
        },
    )
    .await;
    {
        let me = bob.session;
        bob.expect::<proto::UserState>(kind::USER_STATE, move |s| {
            s.session == Some(me) && s.channel_id == Some(temporary)
        })
        .await;
    }
    alice.sync().await;
    alice
        .send(
            kind::TEXT_MESSAGE,
            &proto::TextMessage {
                channel_id: vec![temporary],
                message: "hi <b>there</b>".into(),
                ..Default::default()
            },
        )
        .await;
    let text = bob
        .expect::<proto::TextMessage>(kind::TEXT_MESSAGE, |_| true)
        .await;
    assert_eq!(
        (text.actor, text.message.as_str()),
        (Some(alice.session), "hi <b>there</b>")
    );
    assert!(
        carol
            .receives(kind::TEXT_MESSAGE, Duration::from_millis(300))
            .await
            .is_none()
    );
    // Over the length limit (5000 UTF-16 units by default).
    alice
        .send(
            kind::TEXT_MESSAGE,
            &proto::TextMessage {
                channel_id: vec![temporary],
                message: "x".repeat(5001),
                ..Default::default()
            },
        )
        .await;
    let denied = alice
        .expect::<proto::PermissionDenied>(kind::PERMISSION_DENIED, |_| true)
        .await;
    assert_eq!(denied.r#type(), DenyType::TextTooLong);

    // ACL: SuperUser denies TextMessage to everyone in the Lobby tree.
    admin
        .send(
            kind::ACL,
            &proto::Acl {
                channel_id: lobby,
                inherit_acls: Some(true),
                acls: vec![proto::acl::ChanAcl {
                    apply_here: Some(true),
                    apply_subs: Some(true),
                    group: Some("all".into()),
                    grant: Some(0),
                    deny: Some(0x200),
                    ..Default::default()
                }],
                ..Default::default()
            },
        )
        .await;
    admin.sync().await;
    // Bob is denied; Alice keeps the right through Write in her own channel,
    // which implies TextMessage (Murmur's rule).
    bob.send(
        kind::TEXT_MESSAGE,
        &proto::TextMessage {
            channel_id: vec![temporary],
            message: "denied".into(),
            ..Default::default()
        },
    )
    .await;
    let denied = bob
        .expect::<proto::PermissionDenied>(kind::PERMISSION_DENIED, |_| true)
        .await;
    assert_eq!(
        (denied.r#type(), denied.permission, denied.channel_id),
        (DenyType::Permission, Some(0x200), Some(temporary))
    );
    // The ACL editor shows the inherited root entries and the new one.
    admin
        .send(
            kind::ACL,
            &proto::Acl {
                channel_id: lobby,
                query: Some(true),
                ..Default::default()
            },
        )
        .await;
    let acl = admin.expect::<proto::Acl>(kind::ACL, |_| true).await;
    assert!(
        acl.acls
            .iter()
            .any(|entry| entry.inherited() && entry.group.as_deref() == Some("admin"))
    );
    assert!(
        acl.acls
            .iter()
            .any(|entry| !entry.inherited() && entry.deny == Some(0x200))
    );

    // Kicking needs Kick; SuperUser may.
    bob.send(
        kind::USER_REMOVE,
        &proto::UserRemove {
            session: carol.session,
            ..Default::default()
        },
    )
    .await;
    let denied = bob
        .expect::<proto::PermissionDenied>(kind::PERMISSION_DENIED, |_| true)
        .await;
    assert_eq!(denied.permission, Some(0x10000 | 0x20000));
    admin
        .send(
            kind::USER_REMOVE,
            &proto::UserRemove {
                session: carol.session,
                reason: Some("bye".into()),
                ..Default::default()
            },
        )
        .await;
    let removed = bob
        .expect::<proto::UserRemove>(kind::USER_REMOVE, |r| r.session == carol.session)
        .await;
    assert_eq!(
        (removed.actor, removed.reason.as_deref()),
        (Some(admin.session), Some("bye"))
    );
    assert!(
        carol
            .receives(kind::PING, Duration::from_secs(2))
            .await
            .is_none()
    );

    // The temporary channel disappears when its last user leaves.
    alice
        .send(
            kind::USER_STATE,
            &proto::UserState {
                channel_id: Some(lobby),
                ..Default::default()
            },
        )
        .await;
    bob.send(
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(0),
            ..Default::default()
        },
    )
    .await;
    let removed = admin
        .expect::<proto::ChannelRemove>(kind::CHANNEL_REMOVE, |_| true)
        .await;
    assert_eq!(removed.channel_id, temporary);
    fixture.running.shutdown().await;
}

fn voice(target: u32, sequence: u64, opus: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0];
    udp::Audio {
        header: Some(udp::audio::Header::Target(target)),
        frame_number: sequence,
        opus_data: opus.to_vec(),
        ..Default::default()
    }
    .encode(&mut bytes)
    .unwrap();
    bytes
}

fn received_audio(payload: &[u8]) -> udp::Audio {
    assert_eq!(payload[0], 0, "protobuf audio");
    udp::Audio::decode(&payload[1..]).unwrap()
}

#[tokio::test]
async fn voice_over_the_tunnel_udp_legacy_clients_and_whisper() {
    let fixture = Fixture::start("").await;
    let (alice_id, bob_id, carol_id, dave_id) = (identity(), identity(), identity(), identity());
    let mut alice = Client::connect(fixture.address(), Options::new("Alice", &alice_id))
        .await
        .unwrap();
    let mut bob = Client::connect(fixture.address(), Options::new("Bob", &bob_id))
        .await
        .unwrap();
    let mut legacy = Options::new("Carol", &carol_id);
    legacy.version = (1 << 48) | (4 << 32);
    let mut carol = Client::connect(fixture.address(), legacy).await.unwrap();
    let mut dave = Client::connect(fixture.address(), Options::new("Dave", &dave_id))
        .await
        .unwrap();
    for client in [&mut alice, &mut bob, &mut carol, &mut dave] {
        client.sync().await;
    }
    // Bob proves his UDP address with an encrypted ping; the server answers.
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket.connect(fixture.address()).await.unwrap();
    let crypt = bob.crypt.as_mut().unwrap();
    let mut ping = vec![1];
    udp::Ping {
        timestamp: 42,
        ..Default::default()
    }
    .encode(&mut ping)
    .unwrap();
    socket.send(&crypt.encrypt(&ping)).await.unwrap();
    let mut buffer = [0_u8; 2048];
    let length = timeout(WAIT, socket.recv(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    let reply = crypt
        .decrypt(&buffer[..length])
        .expect("encrypted ping reply");
    assert_eq!(udp::Ping::decode(&reply[1..]).unwrap().timestamp, 42);

    // Alice talks through the tunnel: Bob gets UDP, Carol (1.4) the legacy format, Dave the tunnel.
    // Clients start unmuted; give the bandwidth limiter a moment after login.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let opus = [0xf8, 0xff, 0xfe, 0x01];
    alice
        .send_raw(&encode_tunnel(&voice(0, 7, &opus)))
        .await
        .unwrap();
    let length = timeout(WAIT, socket.recv(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    let audio = received_audio(
        &bob.crypt
            .as_mut()
            .unwrap()
            .decrypt(&buffer[..length])
            .unwrap(),
    );
    assert_eq!(
        (
            audio.sender_session,
            audio.frame_number,
            audio.opus_data.as_slice()
        ),
        (alice.session, 7, &opus[..])
    );
    assert_eq!(audio.header, Some(udp::audio::Header::Context(0)));
    let legacy = carol.receives(kind::UDP_TUNNEL, WAIT).await.unwrap();
    let decoded = nuschl_protocol::voice::decode_voice(&legacy.payload).unwrap();
    assert_eq!(
        legacy.payload[0] >> 5,
        4,
        "legacy Opus header for a 1.4 client"
    );
    assert_eq!(
        (decoded.sender, decoded.sequence, decoded.opus.as_slice()),
        (alice.session, 7, &opus[..])
    );
    let tunneled = dave.receives(kind::UDP_TUNNEL, WAIT).await.unwrap();
    assert_eq!(
        received_audio(&tunneled.payload).sender_session,
        alice.session
    );
    // The speaker does not hear herself, except through loopback (target 31).
    assert!(
        alice
            .receives(kind::UDP_TUNNEL, Duration::from_millis(200))
            .await
            .is_none()
    );
    alice
        .send_raw(&encode_tunnel(&voice(31, 9, &opus)))
        .await
        .unwrap();
    assert_eq!(
        received_audio(
            &alice
                .receives(kind::UDP_TUNNEL, WAIT)
                .await
                .unwrap()
                .payload
        )
        .frame_number,
        9
    );

    // Bob talks over UDP: Alice receives it through the tunnel she used.
    let packet = bob.crypt.as_mut().unwrap().encrypt(&voice(0, 11, &opus));
    socket.send(&packet).await.unwrap();
    let audio = received_audio(
        &alice
            .receives(kind::UDP_TUNNEL, WAIT)
            .await
            .unwrap()
            .payload,
    );
    assert_eq!(
        (audio.sender_session, audio.frame_number),
        (bob.session, 11)
    );
    // A replayed datagram is dropped.
    socket.send(&packet).await.unwrap();

    // Whisper to Carol only (after the others received Bob's packet).
    carol.sync().await;
    dave.sync().await;
    alice
        .send(
            kind::VOICE_TARGET,
            &proto::VoiceTarget {
                id: Some(2),
                targets: vec![proto::voice_target::Target {
                    session: vec![carol.session],
                    ..Default::default()
                }],
            },
        )
        .await;
    alice.sync().await;
    alice
        .send_raw(&encode_tunnel(&voice(2, 13, &opus)))
        .await
        .unwrap();
    let whisper = carol.receives(kind::UDP_TUNNEL, WAIT).await.unwrap();
    assert_eq!(whisper.payload[0] & 0x1f, 2, "whisper context");
    assert!(
        dave.receives(kind::UDP_TUNNEL, Duration::from_millis(300))
            .await
            .is_none()
    );
    let mut stray = [0_u8; 2048];
    assert!(
        timeout(Duration::from_millis(300), socket.recv(&mut stray))
            .await
            .is_err(),
        "Bob heard the whisper or a replay"
    );

    // A self-muted speaker is not routed.
    alice
        .send(
            kind::USER_STATE,
            &proto::UserState {
                self_mute: Some(true),
                ..Default::default()
            },
        )
        .await;
    alice.sync().await;
    alice
        .send_raw(&encode_tunnel(&voice(0, 15, &opus)))
        .await
        .unwrap();
    assert!(
        dave.receives(kind::UDP_TUNNEL, Duration::from_millis(300))
            .await
            .is_none()
    );
    fixture.running.shutdown().await;
}

#[tokio::test]
async fn state_survives_a_restart() {
    let fixture = Fixture::start("").await;
    let (admin_id, alice_id) = (identity(), identity());
    {
        let mut admin = superuser(fixture.address(), &admin_id).await;
        admin
            .send(
                kind::CHANNEL_STATE,
                &proto::ChannelState {
                    parent: Some(0),
                    name: Some("Kept".into()),
                    ..Default::default()
                },
            )
            .await;
        admin
            .expect::<proto::ChannelState>(kind::CHANNEL_STATE, |c| {
                c.name.as_deref() == Some("Kept")
            })
            .await;
        let mut alice = Client::connect(fixture.address(), Options::new("Alice", &alice_id))
            .await
            .unwrap();
        alice
            .send(
                kind::USER_STATE,
                &proto::UserState {
                    session: Some(alice.session),
                    user_id: Some(0),
                    ..Default::default()
                },
            )
            .await;
        alice
            .expect::<proto::UserState>(kind::USER_STATE, |s| s.user_id.is_some())
            .await;
        alice
            .send(
                kind::USER_STATE,
                &proto::UserState {
                    channel_id: Some(1),
                    ..Default::default()
                },
            )
            .await;
        alice
            .expect::<proto::UserState>(kind::USER_STATE, |s| s.channel_id == Some(1))
            .await;
        alice
            .send(
                kind::USER_STATE,
                &proto::UserState {
                    comment: Some("remember me".into()),
                    ..Default::default()
                },
            )
            .await;
        alice.sync().await;
    }
    let fixture = fixture.restart("").await;
    // The registered user comes back by certificate, under any name, into her last channel.
    let alice = Client::connect(fixture.address(), Options::new("NotAlice", &alice_id))
        .await
        .unwrap();
    let channel_names: Vec<String> = alice
        .seen
        .iter()
        .filter(|frame| frame.kind == kind::CHANNEL_STATE)
        .filter_map(|frame| {
            proto::ChannelState::decode(frame.payload.clone())
                .unwrap()
                .name
        })
        .collect();
    assert!(
        channel_names.contains(&"Kept".to_string()),
        "{channel_names:?}"
    );
    let me = alice
        .seen
        .iter()
        .filter(|frame| frame.kind == kind::USER_STATE)
        .map(|frame| proto::UserState::decode(frame.payload.clone()).unwrap())
        .find(|state| state.session == Some(alice.session))
        .unwrap();
    assert_eq!(
        (me.name.as_deref(), me.user_id, me.channel_id),
        (Some("Alice"), Some(1), Some(1))
    );
    assert_eq!(me.comment.as_deref(), Some("remember me"));
    fixture.running.shutdown().await;
}

/// Random frames of every type from an authenticated client must not take the
/// server down or disturb other users.
#[tokio::test]
async fn hostile_frames_leave_the_server_running() {
    let fixture = Fixture::start("").await;
    let (attacker_id, witness_id) = (identity(), identity());
    let mut witness = Client::connect(fixture.address(), Options::new("Witness", &witness_id))
        .await
        .unwrap();
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for round in 0..20 {
        let Ok(mut attacker) = Client::connect(
            fixture.address(),
            Options::new(&format!("Attacker{round}"), &attacker_id),
        )
        .await
        else {
            continue;
        };
        for _ in 0..200 {
            let kind = (next() % 30) as u16;
            let length = (next() % 300) as usize;
            let mut payload: Vec<u8> = (0..length).map(|_| next() as u8).collect();
            // Often a valid protobuf prefix, so handlers get past decoding.
            if !payload.is_empty() && next() % 2 == 0 {
                payload[0] = [0x08, 0x10, 0x18, 0x22, 0x2a][(next() % 5) as usize];
            }
            let mut frame = Vec::with_capacity(6 + length);
            frame.extend_from_slice(&kind.to_be_bytes());
            frame.extend_from_slice(&(length as u32).to_be_bytes());
            frame.extend_from_slice(&payload);
            if attacker.send_raw(&frame).await.is_err() {
                break;
            }
        }
        let _ = timeout(Duration::from_millis(100), attacker.next()).await;
    }
    witness.sync().await;
    let witness2 = identity();
    let late = Client::connect(fixture.address(), Options::new("Late", &witness2))
        .await
        .unwrap();
    assert!(late.session > 0);
    fixture.running.shutdown().await;
}

fn fixture_text() -> String {
    let file = option_env!("NUSCHL_TEST_MURMUR_FIXTURE").unwrap_or(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/murmur-1.5.json"
    ));
    std::fs::read_to_string(file).unwrap()
}

fn plain_settings(directory: &std::path::Path, extra: &str) -> nuschld::config::Settings {
    Config::from_toml(&format!(
        "bind = [\"127.0.0.1\"]\nport = 0\nstate_directory = {directory:?}\n\
         message_limit = 1000\nmessage_burst = 1000\n{extra}\n\
         [autoban]\nattempts = 0\n[password_hashing]\nmemory_kib = 64\npasses = 1\n"
    ))
    .unwrap()
    .settings()
    .unwrap()
}

/// A server without the SuperUser password file (the imported one counts).
async fn start_plain(
    directory: tempfile::TempDir,
    store: Arc<Mutex<MemoryStore>>,
    extra: &str,
) -> Fixture {
    let running = start(
        plain_settings(directory.path(), extra),
        Backend::Memory(store.clone()),
    )
    .await
    .unwrap();
    Fixture {
        running,
        store,
        directory,
    }
}

/// Import Murmur's rows (as `nuschl-storage-worker --read-murmur` reads
/// tests/fixtures/murmur-1.5.sql) into `store`.
async fn import_murmur(
    directory: &std::path::Path,
    store: &Arc<Mutex<MemoryStore>>,
    dump: nuschl_ipc::murmur::MurmurDump,
    replace: bool,
) -> Result<nuschld::murmur::Report, nuschld::murmur::ImportError> {
    let settings = plain_settings(directory, "");
    let storage = nuschld::storage::start(Backend::Memory(store.clone()));
    let counters = Default::default();
    let mut state = nuschld::server::load_state_for_admin(&settings, &storage, &counters)
        .await
        .unwrap();
    let report = nuschld::murmur::apply(&mut state, dump, replace);
    storage.flush().await;
    report
}

#[tokio::test]
async fn a_murmur_database_imports_with_its_passwords_and_acls() {
    let carol = identity();
    let mut dump = nuschl_ipc::murmur::MurmurDump::from_json(&fixture_text()).unwrap();
    for user in &mut dump.users {
        if user.name == "carol" {
            // Murmur's hashes are lowercase; an uppercase one is normalised.
            user.certificate_hash =
                Some(nuschld::tls::sha1_hex(carol.certificate.as_ref()).to_uppercase());
        }
    }
    // Two channels that are each other's parent.
    for (id, parent) in [(20, 21), (21, 20)] {
        dump.channels.push(nuschl_ipc::murmur::MurmurChannel {
            id,
            parent: Some(parent),
            name: format!("Loop {id}"),
            inherit_acl: true,
            ..Default::default()
        });
    }
    let directory = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let report = import_murmur(directory.path(), &store, dump.clone(), false)
        .await
        .unwrap();
    let rendered = report.render();
    for (what, count) in [
        ("registered users", 5),
        ("channels", 3),
        ("ACL entries", 6),
        ("groups", 2),
        ("links", 1),
        ("channel listeners", 1),
        ("bans", 1),
        ("expired bans (not imported)", 1),
        ("comments", 1),
        ("avatars", 1),
    ] {
        assert_eq!(
            report.imported.get(what),
            Some(&count),
            "{what}: {rendered}"
        );
    }
    assert_eq!((report.pbkdf2_passwords, report.sha1_passwords), (3, 1));
    for expected in [
        "user 4: the name does not match",
        "user 5: another account has the same name",
        "user 6: malformed PBKDF2 password",
        "user 6: malformed certificate hash",
        "channel 4 and its subchannels",
        "channel 5: its parent 99",
        "channel 20: its parent 21 does not exist, or the parents form a cycle",
        "channel 21: its parent 20",
        "ACL 2 of channel 1: user 999 was not imported",
        "member 999 was not imported",
        "link 1–99",
        "ban 3: no valid address",
        "ban 4: no valid address",
        "listener of user 1 in channel 99",
    ] {
        assert!(rendered.contains(expected), "{expected} in {rendered}");
    }
    assert!(
        rendered.contains("channels: channel_id: malformed"),
        "{rendered}"
    );
    // Nothing secret in the report.
    assert!(!rendered.contains("serverpw") && !rendered.contains("c2fa5023"));

    // A second import refuses to touch a server that has state, and says so.
    let error = import_murmur(directory.path(), &store, dump.clone(), false)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("--replace-existing"), "{error}");
    let report = import_murmur(directory.path(), &store, dump, true)
        .await
        .unwrap();
    assert_eq!(report.imported.get("registered users"), Some(&5));
    assert!(
        report
            .advice
            .iter()
            .any(|line| line.contains("replaced 3 channels, 5 registered users"))
    );

    let fixture = start_plain(directory, store, "").await;
    let address = fixture.address();
    let login = |name: &'static str, password: &'static str, identity: &'static Identity| {
        Client::connect(
            address,
            Options {
                password: Some(password),
                ..Options::new(name, identity)
            },
        )
    };
    let leak = |identity: Identity| -> &'static Identity { Box::leak(Box::new(identity)) };
    // Murmur's SuperUser password (this server had none), PBKDF2, the legacy
    // SHA-1, and PBKDF2 of a non-ASCII password.
    login("SuperUser", "superpw", leak(identity()))
        .await
        .unwrap();
    let mut alice = login("alice", "hunter2", leak(identity())).await.unwrap();
    let mut bob = login("bob", "password", leak(identity())).await.unwrap();
    login("eve", "pässwörd", leak(identity())).await.unwrap();
    let wrong = login("bob", "Password", leak(identity()))
        .await
        .err()
        .unwrap();
    assert_eq!(reject_type(&wrong), proto::reject::RejectType::WrongUserPw);
    let wrong = login("alice", "hunter3", leak(identity()))
        .await
        .err()
        .unwrap();
    assert_eq!(reject_type(&wrong), proto::reject::RejectType::WrongUserPw);
    // Certificate login from the imported hash.
    let mut carol = Client::connect(address, Options::new("carol", &carol))
        .await
        .unwrap();
    // Successful logins replaced Murmur's hashes with argon2id.
    {
        let state = fixture.running.server.lock();
        for id in [0, 1, 2, 7] {
            let hash = state.registry.users[&id].password_hash.clone().unwrap();
            assert!(hash.starts_with("$argon2id$"), "user {id}: {hash}");
        }
        let alice = &state.registry.users[&1];
        assert_eq!(alice.last_channel, Some(1));
        let lobby = state.channels.get(1).unwrap();
        assert_eq!((lobby.position, lobby.max_users), (5, 10));
        assert!(lobby.links.contains(&3));
        assert_eq!(state.bans.len(), 1);
    }

    // The ACLs: only "insiders" (bob) enter Secret; bob starts there (his
    // last channel), alice in Lobby.
    let own_channel = |client: &Client| {
        client
            .seen
            .iter()
            .filter(|frame| frame.kind == kind::USER_STATE)
            .filter_map(|frame| proto::UserState::decode(frame.payload.clone()).ok())
            .find(|state| state.session == Some(client.session))
            .and_then(|state| state.channel_id)
    };
    assert_eq!(own_channel(&bob), Some(2));
    assert_eq!(own_channel(&alice), Some(1));
    carol
        .send(
            kind::USER_STATE,
            &proto::UserState {
                session: Some(carol.session),
                channel_id: Some(2),
                ..Default::default()
            },
        )
        .await;
    let denied = carol
        .expect::<proto::PermissionDenied>(kind::PERMISSION_DENIED, |_| true)
        .await;
    assert_eq!(denied.permission, Some(0x4));
    // Bob's user ACL in Lobby grants MakeChannel there.
    bob.send(
        kind::CHANNEL_STATE,
        &proto::ChannelState {
            parent: Some(1),
            name: Some("Bob's".into()),
            ..Default::default()
        },
    )
    .await;
    alice
        .expect::<proto::ChannelState>(kind::CHANNEL_STATE, |state| {
            state.name.as_deref() == Some("Bob's")
        })
        .await;
    // The upgraded hash verifies (this login replaces alice's first session).
    login("alice", "hunter2", leak(identity())).await.unwrap();
}

/// Murmur keeps registered users' listeners (`channel_listeners`): they come
/// back on reconnect and after a restart, with their volume; removing one
/// forgets it; an unregistered user's listeners end with the connection.
#[tokio::test]
async fn registered_users_keep_their_listeners() {
    let fixture = Fixture::start("").await;
    let (admin_id, alice_id, guest_id) = (identity(), identity(), identity());
    let mut admin = superuser(fixture.address(), &admin_id).await;
    admin
        .send(
            kind::CHANNEL_STATE,
            &proto::ChannelState {
                parent: Some(0),
                name: Some("Radio".into()),
                ..Default::default()
            },
        )
        .await;
    let radio = admin
        .expect::<proto::ChannelState>(kind::CHANNEL_STATE, |c| c.name.as_deref() == Some("Radio"))
        .await
        .channel_id
        .unwrap();
    let listen =
        |session: u32, add: Vec<u32>, remove: Vec<u32>, volume: Option<f32>| proto::UserState {
            session: Some(session),
            listening_channel_add: add,
            listening_channel_remove: remove,
            listening_volume_adjustment: volume
                .map(|volume| proto::user_state::VolumeAdjustment {
                    listening_channel: Some(radio),
                    volume_adjustment: Some(volume),
                })
                .into_iter()
                .collect(),
            ..Default::default()
        };
    let mut alice = Client::connect(fixture.address(), Options::new("Alice", &alice_id))
        .await
        .unwrap();
    alice
        .send(
            kind::USER_STATE,
            &proto::UserState {
                session: Some(alice.session),
                user_id: Some(0),
                ..Default::default()
            },
        )
        .await;
    alice
        .expect::<proto::UserState>(kind::USER_STATE, |s| s.user_id.is_some())
        .await;
    alice
        .send(
            kind::USER_STATE,
            &listen(alice.session, vec![radio], vec![], None),
        )
        .await;
    alice
        .send(
            kind::USER_STATE,
            &listen(alice.session, vec![], vec![], Some(0.5)),
        )
        .await;
    alice.sync().await;
    // An unregistered user's listener is not kept.
    let mut guest = Client::connect(fixture.address(), Options::new("Guest", &guest_id))
        .await
        .unwrap();
    guest
        .send(
            kind::USER_STATE,
            &listen(guest.session, vec![radio], vec![], None),
        )
        .await;
    guest.sync().await;
    drop(guest);
    drop(alice);
    admin.sync().await;

    let restored = |client: &Client| {
        client
            .seen
            .iter()
            .filter(|frame| frame.kind == kind::USER_STATE)
            .filter_map(|frame| proto::UserState::decode(frame.payload.clone()).ok())
            .find(|state| {
                state.session == Some(client.session) && !state.listening_channel_add.is_empty()
            })
    };
    let fixture = fixture.restart("").await;
    let mut admin = superuser(fixture.address(), &admin_id).await;
    let mut alice = Client::connect(fixture.address(), Options::new("Alice", &alice_id))
        .await
        .unwrap();
    alice.sync().await;
    let state = restored(&alice).expect("alice's listener comes back");
    assert_eq!(state.listening_channel_add, [radio]);
    assert_eq!(state.listening_volume_adjustment.len(), 1);
    assert_eq!(
        state.listening_volume_adjustment[0].volume_adjustment,
        Some(0.5)
    );
    // Others learn the channel, not the volume.
    let seen = admin
        .expect::<proto::UserState>(kind::USER_STATE, |s| {
            s.session == Some(alice.session) && !s.listening_channel_add.is_empty()
        })
        .await;
    assert!(seen.listening_volume_adjustment.is_empty());
    {
        let server = fixture.running.server.lock();
        let channel = server.channels.get(radio).unwrap();
        assert!(channel.listeners.contains(&alice.session));
        assert_eq!(channel.listeners.len(), 1, "the guest's listener is gone");
    }
    // Removing it forgets it.
    alice
        .send(
            kind::USER_STATE,
            &listen(alice.session, vec![], vec![radio], None),
        )
        .await;
    alice.sync().await;
    drop(alice);
    admin.sync().await;
    let mut alice = Client::connect(fixture.address(), Options::new("Alice", &alice_id))
        .await
        .unwrap();
    alice.sync().await;
    assert!(restored(&alice).is_none());
}

/// A TLS handshake with the given versions; the server's leaf certificate and
/// the negotiated version, or `None` if it fails.
async fn handshake(
    address: SocketAddr,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Option<(Vec<u8>, rustls::ProtocolVersion)> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(versions)
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyServer(provider)))
        .with_no_client_auth();
    let stream = TcpStream::connect(address).await.ok()?;
    let tls = timeout(
        WAIT,
        TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from("localhost").unwrap(), stream),
    )
    .await
    .ok()?
    .ok()?;
    let connection = tls.get_ref().1;
    Some((
        connection.peer_certificates()?.first()?.to_vec(),
        connection.protocol_version()?,
    ))
}

fn write_certificate(directory: &std::path::Path, name: &str) -> Vec<u8> {
    let generated = rcgen::generate_simple_self_signed(vec![name.into()]).unwrap();
    std::fs::write(directory.join("tls.pem"), generated.cert.pem()).unwrap();
    std::fs::write(
        directory.join("tls.key"),
        generated.signing_key.serialize_pem(),
    )
    .unwrap();
    generated.cert.der().to_vec()
}

/// SIGHUP's reload: new handshakes get the new certificate, connections keep
/// theirs, and a certificate that does not load keeps the current one.
#[tokio::test]
async fn the_certificate_reloads_and_a_broken_one_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let first = write_certificate(directory.path(), "first");
    let extra = format!(
        "certificate = {:?}\nprivate_key = {:?}",
        directory.path().join("tls.pem"),
        directory.path().join("tls.key")
    );
    let fixture = Fixture::start_in(directory, MemoryStore::shared(), &extra).await;
    let versions = rustls::DEFAULT_VERSIONS;
    assert_eq!(
        handshake(fixture.address(), versions).await.unwrap().0,
        first
    );
    let admin_id = identity();
    let mut admin = superuser(fixture.address(), &admin_id).await;

    let second = write_certificate(fixture.directory.path(), "second");
    let fingerprint = fixture.running.server.reload_certificate().unwrap();
    assert_eq!(fingerprint, nuschld::tls::sha256_hex(&second));
    assert_eq!(
        handshake(fixture.address(), versions).await.unwrap().0,
        second
    );
    // The connection from before keeps working.
    admin.sync().await;

    // A key that does not belong to the certificate, then garbage: refused,
    // and the second certificate stays.
    let other = rcgen::generate_simple_self_signed(vec!["other".into()]).unwrap();
    std::fs::write(
        fixture.directory.path().join("tls.key"),
        other.signing_key.serialize_pem(),
    )
    .unwrap();
    assert!(fixture.running.server.reload_certificate().is_err());
    std::fs::write(
        fixture.directory.path().join("tls.pem"),
        "not a certificate",
    )
    .unwrap();
    assert!(fixture.running.server.reload_certificate().is_err());
    assert_eq!(
        handshake(fixture.address(), versions).await.unwrap().0,
        second
    );
    admin.sync().await;
}

/// A generated certificate is re-read on reload, never generated again.
#[tokio::test]
async fn a_generated_certificate_is_not_regenerated_on_reload() {
    let fixture = Fixture::start("").await;
    let (leaf, _) = handshake(fixture.address(), rustls::DEFAULT_VERSIONS)
        .await
        .unwrap();
    assert_eq!(
        fixture.running.server.reload_certificate().unwrap(),
        nuschld::tls::sha256_hex(&leaf)
    );
    std::fs::remove_file(fixture.directory.path().join("certificate.pem")).unwrap();
    assert!(fixture.running.server.reload_certificate().is_err());
    assert!(!fixture.directory.path().join("certificate.pem").exists());
    assert_eq!(
        handshake(fixture.address(), rustls::DEFAULT_VERSIONS)
            .await
            .unwrap()
            .0,
        leaf
    );
}

#[tokio::test]
async fn tls_13_can_be_required() {
    let tls12 = &[&rustls::version::TLS12][..];
    let fixture = Fixture::start("").await;
    assert_eq!(
        handshake(fixture.address(), tls12).await.unwrap().1,
        rustls::ProtocolVersion::TLSv1_2
    );
    let fixture = Fixture::start("minimum_tls_version = \"1.3\"").await;
    assert!(handshake(fixture.address(), tls12).await.is_none());
    assert_eq!(
        handshake(fixture.address(), rustls::DEFAULT_VERSIONS)
            .await
            .unwrap()
            .1,
        rustls::ProtocolVersion::TLSv1_3
    );
    // Logins work as before.
    superuser(fixture.address(), &identity()).await;
    assert!(Config::from_toml("minimum_tls_version = \"1.1\"").is_err());
}

/// `temporary_channels_per_user`: one more is refused with the reason, and
/// a channel that goes away frees the slot.
#[tokio::test]
async fn temporary_channels_per_user_are_capped_with_a_reason() {
    let fixture = Fixture::start("temporary_channels_per_user = 2").await;
    let (admin_id, guest_id) = (identity(), identity());
    let mut admin = superuser(fixture.address(), &admin_id).await;
    let mut guest = Client::connect(fixture.address(), Options::new("Guest", &guest_id))
        .await
        .unwrap();
    let create = |name: &str| proto::ChannelState {
        parent: Some(0),
        name: Some(name.into()),
        temporary: Some(true),
        ..Default::default()
    };
    admin.send(kind::CHANNEL_STATE, &create("One")).await;
    let one = admin
        .expect::<proto::ChannelState>(kind::CHANNEL_STATE, |c| c.name.as_deref() == Some("One"))
        .await
        .channel_id
        .unwrap();
    // The guest stays in One, so it outlives the admin's move to Two.
    guest
        .send(
            kind::USER_STATE,
            &proto::UserState {
                channel_id: Some(one),
                ..Default::default()
            },
        )
        .await;
    let session = guest.session;
    guest
        .expect::<proto::UserState>(kind::USER_STATE, move |s| {
            s.session == Some(session) && s.channel_id == Some(one)
        })
        .await;
    admin.send(kind::CHANNEL_STATE, &create("Two")).await;
    admin
        .expect::<proto::ChannelState>(kind::CHANNEL_STATE, |c| c.name.as_deref() == Some("Two"))
        .await;
    admin.send(kind::CHANNEL_STATE, &create("Three")).await;
    let denied = admin
        .expect::<proto::PermissionDenied>(kind::PERMISSION_DENIED, |_| true)
        .await;
    assert_eq!(denied.r#type(), proto::permission_denied::DenyType::Text);
    assert!(
        denied.reason().contains("2 temporary channels"),
        "{denied:?}"
    );
    // The guest leaves One, which goes away; now there is room again.
    guest
        .send(
            kind::USER_STATE,
            &proto::UserState {
                channel_id: Some(0),
                ..Default::default()
            },
        )
        .await;
    admin
        .expect::<proto::ChannelRemove>(kind::CHANNEL_REMOVE, |r| r.channel_id == one)
        .await;
    admin.send(kind::CHANNEL_STATE, &create("Three")).await;
    admin
        .expect::<proto::ChannelState>(kind::CHANNEL_STATE, |c| c.name.as_deref() == Some("Three"))
        .await;
}

/// `whisper_fanout_per_second`: a whispered packet over the budget reaches
/// nobody and the speaker is told why; channel voice is not counted.
#[tokio::test]
async fn whispers_over_the_fanout_budget_are_dropped_with_a_notice() {
    let fixture = Fixture::start("whisper_fanout_per_second = 2").await;
    let (alice_id, bob_id, carol_id) = (identity(), identity(), identity());
    let mut alice = Client::connect(fixture.address(), Options::new("Alice", &alice_id))
        .await
        .unwrap();
    let mut bob = Client::connect(fixture.address(), Options::new("Bob", &bob_id))
        .await
        .unwrap();
    let mut carol = Client::connect(fixture.address(), Options::new("Carol", &carol_id))
        .await
        .unwrap();
    for client in [&mut alice, &mut bob, &mut carol] {
        client.sync().await;
    }
    alice
        .send(
            kind::VOICE_TARGET,
            &proto::VoiceTarget {
                id: Some(1),
                targets: vec![proto::voice_target::Target {
                    session: vec![bob.session, carol.session],
                    ..Default::default()
                }],
            },
        )
        .await;
    alice.sync().await;
    let opus = [0xf8, 0xff, 0xfe, 0x01];
    // Two receivers: the whole budget.
    alice
        .send_raw(&encode_tunnel(&voice(1, 1, &opus)))
        .await
        .unwrap();
    assert!(bob.receives(kind::UDP_TUNNEL, WAIT).await.is_some());
    assert!(carol.receives(kind::UDP_TUNNEL, WAIT).await.is_some());
    alice
        .send_raw(&encode_tunnel(&voice(1, 2, &opus)))
        .await
        .unwrap();
    let denied = alice
        .expect::<proto::PermissionDenied>(kind::PERMISSION_DENIED, |_| true)
        .await;
    assert_eq!(denied.r#type(), proto::permission_denied::DenyType::Text);
    assert!(denied.reason().contains("busy"), "{denied:?}");
    assert!(
        bob.receives(kind::UDP_TUNNEL, Duration::from_millis(300))
            .await
            .is_none()
    );
    // A second drop within ten seconds is not announced again.
    alice
        .send_raw(&encode_tunnel(&voice(1, 3, &opus)))
        .await
        .unwrap();
    assert!(
        alice
            .receives(kind::PERMISSION_DENIED, Duration::from_millis(300))
            .await
            .is_none()
    );
    // Talking in the channel is not limited by it.
    alice
        .send_raw(&encode_tunnel(&voice(0, 4, &opus)))
        .await
        .unwrap();
    assert!(bob.receives(kind::UDP_TUNNEL, WAIT).await.is_some());
    assert!(
        fixture
            .running
            .server
            .counters
            .fanout_dropped
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 2
    );
}

/// Clients before 1.2.2 are turned away with the reason; 1.2.2 and clients
/// that report no version are admitted.
#[tokio::test]
async fn clients_before_122_are_refused_with_the_reason() {
    let fixture = Fixture::start("").await;
    let old = identity();
    let mut options = Options::new("Old", &old);
    options.version = (1 << 48) | (2 << 32) | (1 << 16);
    let rejected = Client::connect(fixture.address(), options)
        .await
        .err()
        .unwrap();
    assert_eq!(
        reject_type(&rejected),
        proto::reject::RejectType::WrongVersion
    );
    assert!(rejected.reason().contains("1.2.2"));
    for version in [(1 << 48) | (2 << 32) | (2 << 16), 0] {
        let mut options = Options::new("Older", &old);
        options.version = version;
        Client::connect(fixture.address(), options).await.unwrap();
    }
}

/// The counters behind the periodic statistics line.
#[tokio::test]
async fn statistics_count_connections_messages_and_voice() {
    let fixture = Fixture::start("").await;
    let before = nuschld::server::Statistics::totals(&fixture.running.server.counters);
    let (alice_id, bob_id) = (identity(), identity());
    let mut alice = Client::connect(fixture.address(), Options::new("Alice", &alice_id))
        .await
        .unwrap();
    let mut bob = Client::connect(fixture.address(), Options::new("Bob", &bob_id))
        .await
        .unwrap();
    bob.sync().await;
    let wrong = Client::connect(
        fixture.address(),
        Options {
            password: Some("wrong"),
            ..Options::new("SuperUser", &identity())
        },
    )
    .await;
    assert!(wrong.is_err());
    alice
        .send_raw(&encode_tunnel(&voice(0, 1, &[0xf8, 0xff, 0xfe, 0x01])))
        .await
        .unwrap();
    assert!(bob.receives(kind::UDP_TUNNEL, WAIT).await.is_some());
    alice.sync().await;
    let delta =
        nuschld::server::Statistics::totals(&fixture.running.server.counters).since(&before);
    assert_eq!((delta.connections, delta.logins, delta.rejected), (3, 2, 1));
    assert!(delta.control_messages >= 2 && delta.control_bytes > 0);
    assert_eq!(delta.voice_packets_in, 1);
    assert_eq!(delta.voice_packets_out, 1);
    assert!(delta.voice_bytes_out > 0);
}

/// Murmur 1.6's root is its own parent; permissions may be negative (Murmur
/// stores a signed int); a database without a root for the virtual server
/// fails before anything is replaced.
#[tokio::test]
async fn murmur_16_details_and_failures_before_any_change() {
    let mut dump = nuschl_ipc::murmur::MurmurDump::from_json(&fixture_text()).unwrap();
    dump.header.layout = "1.6".into();
    dump.channels[0].parent = Some(0);
    dump.acls.push(nuschl_ipc::murmur::MurmurAcl {
        channel: 3,
        priority: 1,
        user_id: None,
        group: Some("all".into()),
        apply_here: true,
        apply_subs: true,
        grant: 0,
        deny: -1,
    });
    let directory = tempfile::tempdir().unwrap();
    let store = MemoryStore::shared();
    let report = import_murmur(directory.path(), &store, dump.clone(), false)
        .await
        .unwrap();
    assert_eq!(report.imported.get("channels"), Some(&3));
    assert_eq!(report.imported.get("ACL entries"), Some(&7));
    // No root for this virtual server: refused, and nothing is replaced.
    let mut rootless = dump.clone();
    rootless.channels.retain(|channel| channel.id != 0);
    let error = import_murmur(directory.path(), &store, rootless, true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no root channel"), "{error}");
    let error = import_murmur(directory.path(), &store, dump, false)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("3 channels, 5 registered users and 1 bans"),
        "{error}"
    );
}
