// SPDX-License-Identifier: EUPL-1.2
//! The MLS delivery service (`mls/1`, docs/e2ee.md §6.3) on loopback: raw protocol
//! clients that register, publish an identity in key transparency and drive MLS
//! with nuschel-e2ee's client half, against `nuscheld`.
use bytes::BytesMut;
use nuschel_e2ee::{
    ConversationBinding, SUITE_KEM_PUBLIC_KEY_LEN,
    client::{Client as MlsClient, Incoming, ValidKeyPackage},
};
use nuschel_identity::{DisplayName, NewIdentity, Validity};
use nuschel_protocol::{
    ControlFrame, control as proto, decode_control, encode_control, kind,
    nuschel::{self, kind as nk, v1},
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
    collections::VecDeque,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

const WAIT: Duration = Duration::from_secs(15);
const CAPABILITIES: [&str; 6] = [
    "history/1",
    "subscribe/1",
    "typing/1",
    "kt/1",
    "mls/1",
    "e2ee/1",
];

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn request_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn key() -> [u8; 16] {
    nuscheld::state::random()
}

// ---- the server ----

struct Fixture {
    running: Running,
    store: Arc<Mutex<MemoryStore>>,
    directory: tempfile::TempDir,
}

impl Fixture {
    async fn start() -> Self {
        Self::start_in(tempfile::tempdir().unwrap(), MemoryStore::shared(), 0).await
    }

    async fn start_in(
        directory: tempfile::TempDir,
        store: Arc<Mutex<MemoryStore>>,
        port: u16,
    ) -> Self {
        let config = Config::from_toml(&format!(
            "security = \"e2ee\"\nbind = [\"127.0.0.1\"]\nport = {port}\nstate_directory = {:?}\n\
             message_limit = 1000\nmessage_burst = 1000\n\
             [autoban]\nattempts = 0\n[password_hashing]\nmemory_kib = 64\npasses = 1\n\
             [kt]\npublish_interval = 1\n\
             [mls]\nuploads_per_minute = 600\nclaims_per_minute = 600\n",
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

    async fn restart(self) -> Self {
        let Self {
            running,
            store,
            directory,
        } = self;
        let port = running.addresses[0].port();
        running.shutdown().await;
        Self::start_in(directory, store, port).await
    }
}

// ---- a raw client ----

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

/// A user: their Mumble certificate, identity and MLS state survive reconnects.
struct User {
    name: &'static str,
    certificate: CertificateDer<'static>,
    key: Vec<u8>,
    identity: NewIdentity,
    mls: MlsClient,
    user_id: u32,
    account: [u8; 16],
}

impl User {
    fn new(name: &'static str) -> Self {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let validity = Validity::starting_at(now_s() - 3600, 365 * 86_400).unwrap();
        Self {
            name,
            certificate: generated.cert.der().clone(),
            key: generated.signing_key.serialize_der(),
            identity: NewIdentity::create(&DisplayName::new(name).unwrap(), validity, now_s())
                .unwrap(),
            mls: MlsClient::new().unwrap(),
            user_id: 0,
            account: [0; 16],
        }
    }

    fn device(&self) -> [u8; 16] {
        *self.identity.device_credential.device_id().as_bytes()
    }

    fn key_package(&self, last_resort: bool) -> Vec<u8> {
        self.mls
            .key_package(
                &self.identity.device,
                &self.identity.device_credential,
                last_resort,
            )
            .unwrap()
    }
}

struct Connection {
    reader: ReadHalf<TlsStream<TcpStream>>,
    writer: WriteHalf<TlsStream<TcpStream>>,
    buffer: BytesMut,
    backlog: VecDeque<ControlFrame>,
    session: u32,
    log_id: [u8; 32],
    /// The log's head-signing key: the groups' external sender.
    server_key: [u8; 32],
    capabilities: Vec<String>,
}

impl Connection {
    async fn connect(address: SocketAddr, user: &User) -> Self {
        Self::connect_as(address, user, &CAPABILITIES)
            .await
            .expect("admitted")
    }

    /// A TLS connection offering `alpn`.
    async fn tls(address: SocketAddr, user: &User, alpn: &[u8]) -> Self {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyServer(provider)))
            .with_client_auth_cert(
                vec![user.certificate.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(user.key.clone())),
            )
            .unwrap();
        config.alpn_protocols = vec![alpn.to_vec()];
        let stream = TcpStream::connect(address).await.unwrap();
        let tls = TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from("localhost").unwrap(), stream)
            .await
            .unwrap();
        let (reader, writer) = tokio::io::split(tls);
        Self {
            reader,
            writer,
            buffer: BytesMut::new(),
            backlog: VecDeque::new(),
            session: 0,
            log_id: [0; 32],
            server_key: [0; 32],
            capabilities: Vec::new(),
        }
    }

    /// Logs in as a Nuschel client offering `capabilities`; the server's `Reject`
    /// if it refuses.
    async fn connect_as(
        address: SocketAddr,
        user: &User,
        capabilities: &[&str],
    ) -> Result<Self, proto::Reject> {
        let mut connection = Self::tls(address, user, nuschel::ALPN).await;
        let hello = v1::Hello {
            protocol: nuschel::PROTOCOL,
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
            policy: None,
            kt_log: Vec::new(),
            client_version: String::new(),
            relay_only: false,
        };
        let frame = nuschel::version_frame(
            &proto::Version {
                version_v2: Some((1 << 48) | (5 << 32)),
                ..Default::default()
            },
            Some(&hello),
        )
        .unwrap();
        connection.send_bytes(&frame).await;
        let version = connection.until(kind::VERSION).await;
        let hello = nuschel::hello(&version.payload).unwrap().unwrap();
        connection.capabilities = hello.capabilities.clone();
        if !hello.kt_log.is_empty() {
            let descriptor = nuschel_kt::LogDescriptor::decode(&hello.kt_log).unwrap();
            connection.log_id = descriptor.log_id().0;
            connection.server_key = descriptor.signing_key().to_bytes().try_into().unwrap();
        }
        connection
            .send(
                kind::AUTHENTICATE,
                &proto::Authenticate {
                    username: Some(user.name.into()),
                    opus: Some(true),
                    ..Default::default()
                },
            )
            .await;
        let answer = connection
            .wait(|frame| {
                matches!(frame.kind, kind::SERVER_SYNC | kind::REJECT).then(|| frame.clone())
            })
            .await;
        if answer.kind == kind::REJECT {
            return Err(proto::Reject::decode(answer.payload).unwrap());
        }
        connection.session = proto::ServerSync::decode(answer.payload)
            .unwrap()
            .session
            .unwrap();
        Ok(connection)
    }

    async fn send_bytes(&mut self, frame: &[u8]) {
        self.writer.write_all(frame).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn send(&mut self, kind: u16, message: &impl Message) {
        self.send_bytes(&encode_control(kind, message).unwrap())
            .await;
    }

    async fn read(&mut self, deadline: tokio::time::Instant) -> Option<ControlFrame> {
        loop {
            if let Ok(Some(frame)) = decode_control(&mut self.buffer) {
                return Some(frame);
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match timeout(left, self.reader.read_buf(&mut self.buffer)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return None,
                Ok(Ok(_)) => {}
            }
        }
    }

    /// The first frame matching `pick`; others stay in the backlog.
    async fn wait<T>(&mut self, mut pick: impl FnMut(&ControlFrame) -> Option<T>) -> T {
        if let Some(index) = self.backlog.iter().position(|frame| pick(frame).is_some()) {
            let frame = self.backlog.remove(index).unwrap();
            return pick(&frame).unwrap();
        }
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let frame = self
                .read(deadline)
                .await
                .expect("no matching frame from the server");
            if let Some(value) = pick(&frame) {
                return value;
            }
            self.backlog.push_back(frame);
        }
    }

    async fn until(&mut self, wanted: u16) -> ControlFrame {
        self.wait(|frame| (frame.kind == wanted).then(|| frame.clone()))
            .await
    }

    /// Sends a request and returns its answer (a response frame or an error).
    async fn request(&mut self, kind: u16, message: &impl Message, id: u64) -> ControlFrame {
        self.send(kind, message).await;
        self.wait(|frame| {
            let answer = matches!(
                frame.kind,
                nk::ERROR
                    | nk::DONE
                    | nk::MLS_KEY_PACKAGE_STATUS
                    | nk::MLS_KEY_PACKAGES
                    | nk::MLS_ACK
                    | nk::MLS_WELCOMES
                    | nk::MLS_JOIN_INFO
                    | nk::KT_STATUS_REPLY
                    | nk::KT_PUBLISHED
                    | nk::DIRECT_OPENED
                    | nk::CHAT_ACK
                    | nk::HISTORY_PAGE
            );
            (answer
                && v1::Done::decode(frame.payload.clone()).is_ok_and(|done| done.request_id == id))
            .then(|| frame.clone())
        })
        .await
    }

    async fn register(&mut self, user: &mut User) {
        let session = self.session;
        self.send(
            kind::USER_STATE,
            &proto::UserState {
                session: Some(session),
                user_id: Some(0),
                ..Default::default()
            },
        )
        .await;
        user.user_id = self
            .wait(|frame| {
                (frame.kind == kind::USER_STATE)
                    .then(|| proto::UserState::decode(frame.payload.clone()).ok())
                    .flatten()
                    .filter(|state| state.session == Some(session))
                    .and_then(|state| state.user_id)
            })
            .await;
    }

    /// The account id, and the identity bound to it in key transparency.
    async fn publish(&mut self, user: &mut User) {
        let id = request_id();
        let status = self
            .request(
                nk::KT_STATUS,
                &v1::KtStatus {
                    request_id: id,
                    origin: String::new(),
                },
                id,
            )
            .await;
        user.account = v1::KtStatusReply::decode(status.payload)
            .unwrap()
            .account
            .try_into()
            .unwrap();
        let statement = user
            .identity
            .device_signing
            .sign_account(self.log_id, user.account, now_s())
            .unwrap();
        let id = request_id();
        let publish = v1::KtPublish {
            request_id: id,
            root: user.identity.root.public_key().encode(),
            record: Some(v1::KtRecord {
                device_signing_certificate: user.identity.device_signing.certificate().encode(),
                devices: vec![user.identity.device_certificate.encode()],
                revocations: Vec::new(),
                statement: statement.encode(),
            }),
        };
        let answer = self.request(nk::KT_PUBLISH, &publish, id).await;
        assert_eq!(answer.kind, nk::KT_PUBLISHED, "{answer:?}");
    }

    /// Uploads key packages, waiting for the binding to reach its epoch.
    async fn upload(&mut self, packages: Vec<Vec<u8>>, last_resort: Vec<u8>) -> ControlFrame {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let id = request_id();
            let answer = self
                .request(
                    nk::MLS_KEY_PACKAGE_UPLOAD,
                    &v1::MlsKeyPackageUpload {
                        request_id: id,
                        key_packages: packages.clone(),
                        last_resort: last_resort.clone(),
                    },
                    id,
                )
                .await;
            if answer.kind == nk::MLS_KEY_PACKAGE_STATUS || tokio::time::Instant::now() > deadline {
                return answer;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    async fn open_direct(&mut self, peer: &User) -> [u8; 16] {
        let id = request_id();
        let answer = self
            .request(
                nk::OPEN_DIRECT,
                &v1::OpenDirect {
                    request_id: id,
                    user_id: peer.user_id,
                },
                id,
            )
            .await;
        v1::DirectOpened::decode(answer.payload)
            .unwrap()
            .direct
            .unwrap()
            .conversation
            .try_into()
            .unwrap()
    }

    async fn claim(&mut self, account: [u8; 16]) -> ControlFrame {
        let id = request_id();
        self.request(
            nk::MLS_KEY_PACKAGE_CLAIM,
            &v1::MlsKeyPackageClaim {
                request_id: id,
                account: account.to_vec(),
            },
            id,
        )
        .await
    }

    async fn commit(&mut self, mut commit: v1::MlsCommit) -> ControlFrame {
        let id = request_id();
        commit.request_id = id;
        self.request(nk::MLS_COMMIT, &commit, id).await
    }

    async fn send_mls(
        &mut self,
        conversation: [u8; 16],
        key: [u8; 16],
        message: Vec<u8>,
    ) -> ControlFrame {
        let id = request_id();
        self.request(
            nk::MLS_SEND,
            &v1::MlsSend {
                archive: Vec::new(),
                request_id: id,
                conversation: conversation.to_vec(),
                idempotency_key: key.to_vec(),
                message,
            },
            id,
        )
        .await
    }

    async fn welcomes(&mut self, device: [u8; 16]) -> Vec<v1::MlsWelcome> {
        let id = request_id();
        let answer = self
            .request(
                nk::MLS_WELCOME_FETCH,
                &v1::MlsWelcomeFetch {
                    request_id: id,
                    device: device.to_vec(),
                },
                id,
            )
            .await;
        assert_eq!(answer.kind, nk::MLS_WELCOMES, "{answer:?}");
        v1::MlsWelcomes::decode(answer.payload).unwrap().welcomes
    }

    async fn join_info(&mut self, conversation: [u8; 16]) -> ControlFrame {
        let id = request_id();
        self.request(
            nk::MLS_JOIN_INFO_REQUEST,
            &v1::MlsJoinInfoRequest {
                request_id: id,
                conversation: conversation.to_vec(),
                tree: true,
            },
            id,
        )
        .await
    }

    /// The next live chat event.
    async fn chat(&mut self) -> v1::Message {
        let frame = self.until(nk::CHAT_EVENT).await;
        v1::ChatEvent::decode(frame.payload)
            .unwrap()
            .message
            .unwrap()
    }
}

fn ack(frame: &ControlFrame) -> v1::MlsAck {
    assert_eq!(
        frame.kind,
        nk::MLS_ACK,
        "expected an MlsAck: {frame:?} {:?}",
        error(frame)
    );
    v1::MlsAck::decode(frame.payload.clone()).unwrap()
}

fn error(frame: &ControlFrame) -> Option<v1::Error> {
    (frame.kind == nk::ERROR).then(|| v1::Error::decode(frame.payload.clone()).unwrap())
}

fn code(frame: &ControlFrame) -> Option<v1::error::Code> {
    error(frame).and_then(|error| v1::error::Code::try_from(error.code).ok())
}

/// A user who is registered, bound in key transparency and connected.
async fn join(fixture: &Fixture, user: &mut User) -> Connection {
    let mut connection = Connection::connect(fixture.address(), user).await;
    assert!(connection.capabilities.iter().any(|c| c == "mls/1"));
    connection.register(user).await;
    connection.publish(user).await;
    connection
}

fn create_commit(
    creator: &User,
    log_id: [u8; 32],
    server_key: [u8; 32],
    conversation: [u8; 16],
    packages: &[Vec<u8>],
) -> (Vec<u8>, v1::MlsCommit) {
    let valid: Vec<ValidKeyPackage> = packages
        .iter()
        .map(|bytes| ValidKeyPackage::check(bytes).unwrap())
        .collect();
    let created = creator
        .mls
        .create(
            &creator.identity.device,
            &creator.identity.device_credential,
            &ConversationBinding::direct(log_id, conversation),
            &server_key,
            &valid,
        )
        .unwrap();
    let message = v1::MlsCommit {
        request_id: 0,
        conversation: conversation.to_vec(),
        idempotency_key: key().to_vec(),
        commit: created.first.commit,
        group_info: created.first.group_info,
        welcome: created.first.welcome.unwrap_or_default(),
        create: Some(v1::MlsGroupInit {
            group_info: created.init_group_info,
            tree: created.init_tree,
        }),
    };
    (created.group_id, message)
}

/// Re-signs a key package with a 32-byte X-Wing init key: valid for OpenMLS 0.9.2,
/// fatal for libcrux-kem 0.0.9 when someone encapsulates to it.
fn short_init_key_package(user: &User) -> Vec<u8> {
    let valid = user.key_package(false);
    assert_eq!(&valid[4..6], &[0x44, 0xc0]);
    let rest = &valid[6 + SUITE_KEM_PUBLIC_KEY_LEN..];
    let unsigned_rest = &rest[..rest.len() - 66];
    let mut tbs = valid[..4].to_vec();
    tbs.push(32);
    tbs.extend_from_slice(&[0x55; 32]);
    tbs.extend_from_slice(unsigned_rest);
    let label = b"MLS 1.0 KeyPackageTBS";
    let mut content = vec![label.len() as u8];
    content.extend_from_slice(label);
    let length = tbs.len();
    content.extend_from_slice(&[0x40 | (length >> 8) as u8, length as u8]);
    content.extend_from_slice(&tbs);
    let signature = user.identity.device.sign_leaf(&content).unwrap();
    let mut package = tbs;
    package.extend_from_slice(&[0x40, 0x40]);
    package.extend_from_slice(&signature);
    package
}

/// Alice and Bob in a direct conversation with an MLS group; Bob joined.
struct Pair {
    alice: User,
    bob: User,
    a: Connection,
    b: Connection,
    conversation: [u8; 16],
    group: Vec<u8>,
    create: v1::MlsCommit,
    created_seq: u64,
}

async fn pair(fixture: &Fixture) -> Pair {
    let mut alice = User::new("alice");
    let mut bob = User::new("bob");
    let mut a = join(fixture, &mut alice).await;
    let mut b = join(fixture, &mut bob).await;
    let status = b
        .upload(
            vec![bob.key_package(false), bob.key_package(false)],
            bob.key_package(true),
        )
        .await;
    assert_eq!(
        status.kind,
        nk::MLS_KEY_PACKAGE_STATUS,
        "{:?}",
        error(&status)
    );
    let status = v1::MlsKeyPackageStatus::decode(status.payload).unwrap();
    assert_eq!((status.one_time, status.last_resort), (2, true));
    // Bob's connection listens for Welcomes before there is one.
    assert!(b.welcomes(bob.device()).await.is_empty());
    let conversation = a.open_direct(&bob).await;
    let claimed = a.claim(bob.account).await;
    assert_eq!(claimed.kind, nk::MLS_KEY_PACKAGES, "{:?}", error(&claimed));
    let packages = v1::MlsKeyPackages::decode(claimed.payload)
        .unwrap()
        .key_packages;
    assert_eq!(packages.len(), 1);
    let (group, create) = create_commit(&alice, a.log_id, a.server_key, conversation, &packages);
    let acked = ack(&a.commit(create.clone()).await);
    assert_eq!(acked.epoch, 1);
    alice.mls.merge_pending(&group).unwrap();
    // The Welcome is pushed to Bob's connection, the commit is in the stream.
    let push = b.until(nk::MLS_WELCOME_PUSH).await;
    let welcome = v1::MlsWelcomePush::decode(push.payload)
        .unwrap()
        .welcome
        .unwrap();
    assert_eq!((welcome.seq, welcome.epoch), (acked.seq, 1));
    let event = b.chat().await;
    assert_eq!(event.seq, acked.seq);
    assert_eq!(event.format, v1::Format::MlsHandshake as i32);
    assert_eq!(event.mls, create.commit);
    assert!(event.content.is_empty());
    let info = b.join_info(conversation).await;
    assert_eq!(info.kind, nk::MLS_JOIN_INFO, "{:?}", error(&info));
    let info = v1::MlsJoinInfo::decode(info.payload).unwrap();
    assert_eq!((info.epoch, info.commit_seq), (1, acked.seq));
    let pending = bob.mls.stage_welcome(&welcome.welcome, &info.tree).unwrap();
    assert_eq!(bob.mls.join(pending).unwrap(), group);
    Pair {
        alice,
        bob,
        a,
        b,
        conversation,
        group,
        create,
        created_seq: acked.seq,
    }
}

#[tokio::test]
async fn a_direct_conversation_through_the_delivery_service() {
    let fixture = Fixture::start().await;
    let Pair {
        alice,
        bob,
        mut a,
        mut b,
        conversation,
        group,
        create,
        created_seq,
    } = pair(&fixture).await;

    // A retried creation gets the original acknowledgement; another one is stale.
    assert_eq!(ack(&a.commit(create.clone()).await).seq, created_seq);
    let mut again = create.clone();
    again.idempotency_key = key().to_vec();
    let stale = a.commit(again).await;
    assert_eq!(code(&stale), Some(v1::error::Code::StaleEpoch));
    assert_eq!(error(&stale).unwrap().epoch, 1);

    // The mailbox keeps the Welcome until acknowledged.
    let waiting = b.welcomes(bob.device()).await;
    assert_eq!(waiting.len(), 1);
    let id = request_id();
    let done = b
        .request(
            nk::MLS_WELCOME_ACK,
            &v1::MlsWelcomeAck {
                request_id: id,
                device: bob.device().to_vec(),
                ids: vec![waiting[0].id.clone()],
            },
            id,
        )
        .await;
    assert_eq!(done.kind, nk::DONE);
    assert!(b.welcomes(bob.device()).await.is_empty());

    // Application messages: stored, fanned out, decrypted by Bob.
    let message_key = key();
    let hello = alice
        .mls
        .encrypt(&alice.identity.device, &group, b"hello bob")
        .unwrap();
    let sent = ack(&a.send_mls(conversation, message_key, hello).await);
    assert_eq!(sent.epoch, 1);
    let event = b.chat().await;
    assert_eq!(event.format, v1::Format::MlsApplication as i32);
    match bob.mls.process(&group, &event.mls).unwrap() {
        Incoming::Application { data, .. } => assert_eq!(data, b"hello bob"),
        _ => panic!("expected application data"),
    }
    // A resend with the same key (re-encrypted) returns the original seq.
    let again = alice
        .mls
        .encrypt(&alice.identity.device, &group, b"hello bob")
        .unwrap();
    assert_eq!(
        ack(&a.send_mls(conversation, message_key, again).await).seq,
        sent.seq
    );

    // Pages replay commits and application messages in order.
    let id = request_id();
    let page = b
        .request(
            nk::HISTORY_REQUEST,
            &v1::HistoryRequest {
                request_id: id,
                conversation: conversation.to_vec(),
                anchor: Some(v1::history_request::Anchor::After(0)),
                limit: 100,
            },
            id,
        )
        .await;
    let page = v1::HistoryPage::decode(page.payload).unwrap();
    // The resend's seq is kept explicit as a void row.
    assert_eq!(
        page.messages.last().map(|m| m.kind),
        Some(v1::message::Kind::Void as i32)
    );
    let rows: Vec<(u64, i32, bool)> = page
        .messages
        .iter()
        .filter(|m| m.kind != v1::message::Kind::Void as i32)
        .map(|m| (m.seq, m.format, !m.mls.is_empty() && m.content.is_empty()))
        .collect();
    assert_eq!(
        rows,
        [
            (created_seq, v1::Format::MlsHandshake as i32, true),
            (sent.seq, v1::Format::MlsApplication as i32, true)
        ]
    );

    // Epoch fencing: Bob's update wins; Alice's old-epoch message and commit are stale.
    let late = alice
        .mls
        .encrypt(&alice.identity.device, &group, b"late")
        .unwrap();
    let update = bob.mls.self_update(&bob.identity.device, &group).unwrap();
    let updated = ack(&b
        .commit(v1::MlsCommit {
            request_id: 0,
            conversation: conversation.to_vec(),
            idempotency_key: key().to_vec(),
            commit: update.commit.clone(),
            group_info: update.group_info,
            welcome: Vec::new(),
            create: None,
        })
        .await);
    assert_eq!(updated.epoch, 2);
    bob.mls.merge_pending(&group).unwrap();
    let stale = a.send_mls(conversation, key(), late).await;
    assert_eq!(code(&stale), Some(v1::error::Code::StaleEpoch));
    assert_eq!(
        (error(&stale).unwrap().epoch, error(&stale).unwrap().head),
        (2, updated.seq)
    );
    let racing = alice
        .mls
        .self_update(&alice.identity.device, &group)
        .unwrap();
    let stale = a
        .commit(v1::MlsCommit {
            request_id: 0,
            conversation: conversation.to_vec(),
            idempotency_key: key().to_vec(),
            commit: racing.commit,
            group_info: racing.group_info,
            welcome: Vec::new(),
            create: None,
        })
        .await;
    assert_eq!(code(&stale), Some(v1::error::Code::StaleEpoch));
    alice.mls.clear_pending(&group).unwrap();
    // Alice catches up from the stream and her resend goes through.
    let commit_event = a
        .wait(|frame| {
            (frame.kind == nk::CHAT_EVENT)
                .then(|| {
                    v1::ChatEvent::decode(frame.payload.clone())
                        .unwrap()
                        .message
                })
                .flatten()
                .filter(|message| message.seq == updated.seq)
        })
        .await;
    match alice.mls.process(&group, &commit_event.mls).unwrap() {
        Incoming::Commit(commit) => assert_eq!(alice.mls.merge(&group, commit).unwrap(), 2),
        _ => panic!("expected a commit"),
    }
    let resend = alice
        .mls
        .encrypt(&alice.identity.device, &group, b"late")
        .unwrap();
    assert_eq!(ack(&a.send_mls(conversation, key(), resend).await).epoch, 2);

    // Plaintext, edits and deletions are refused for the conversation now.
    let id = request_id();
    let plain = a
        .request(
            nk::CHAT_SEND,
            &v1::ChatSend {
                request_id: id,
                conversation: conversation.to_vec(),
                idempotency_key: key().to_vec(),
                format: v1::Format::Text as i32,
                content: "plaintext".into(),
                reply_to: 0,
            },
            id,
        )
        .await;
    assert_eq!(code(&plain), Some(v1::error::Code::Unsupported));
    let id = request_id();
    let edit = a
        .request(
            nk::CHAT_EDIT,
            &v1::ChatEdit {
                request_id: id,
                conversation: conversation.to_vec(),
                target: sent.seq,
                format: v1::Format::Text as i32,
                content: "edited".into(),
            },
            id,
        )
        .await;
    assert_eq!(code(&edit), Some(v1::error::Code::Unsupported));
}

/// End-to-end mode is a server-level switch (docs/e2ee.md §10, answers 7 and 12):
/// stock clients and Nuschel clients that cannot take part are turned away before
/// their credentials are read, nothing is sent in plaintext, typing is not offered,
/// and unregistered connections may register but take no part.
#[tokio::test]
async fn the_end_to_end_switch_admits_only_what_can_take_part() {
    let fixture = Fixture::start().await;
    let guest = User::new("guest");

    // A stock client (ALPN `mumble` only) is rejected with a reason it can show.
    let mut stock = Connection::tls(
        fixture.address(),
        &guest,
        nuschel_protocol::nuschel::ALPN_MUMBLE,
    )
    .await;
    let reject = stock.until(kind::REJECT).await;
    let reject = proto::Reject::decode(reject.payload).unwrap();
    assert!(
        reject.reason().contains("requires the Nuschel client"),
        "{reject:?}"
    );

    // A Nuschel client without `e2ee/1` (an older one) too, at Authenticate.
    let old = Connection::connect_as(fixture.address(), &guest, &CAPABILITIES[..5])
        .await
        .err()
        .expect("refused");
    assert_eq!(old.r#type(), proto::reject::RejectType::WrongVersion);
    assert!(old.reason().contains("end to end"), "{old:?}");
    assert_eq!(fixture.running.server.lock().users.len(), 0);

    // An unregistered Nuschel client is admitted: it sees the policy, no typing.
    let mut g = Connection::connect(fixture.address(), &guest).await;
    for wanted in ["kt/1", "mls/1", "e2ee/1"] {
        assert!(
            g.capabilities.iter().any(|c| c == wanted),
            "{:?}",
            g.capabilities
        );
    }
    assert!(!g.capabilities.iter().any(|c| c == "typing/1"));
    // It may not write anywhere: the lobby's conversation refuses it.
    let lobby = {
        let state = fixture.running.server.lock();
        state.conversation_of_channel(0).unwrap()
    };
    let id = request_id();
    let plain = g
        .request(
            nk::CHAT_SEND,
            &v1::ChatSend {
                request_id: id,
                conversation: lobby.to_vec(),
                idempotency_key: key().to_vec(),
                format: v1::Format::Text as i32,
                content: "hello".into(),
                reply_to: 0,
            },
            id,
        )
        .await;
    assert_eq!(code(&plain), Some(v1::error::Code::Unsupported));
    let id = request_id();
    let join = g
        .request(
            nk::MLS_JOIN_INFO_REQUEST,
            &v1::MlsJoinInfoRequest {
                request_id: id,
                conversation: lobby.to_vec(),
                tree: false,
            },
            id,
        )
        .await;
    assert_eq!(code(&join), Some(v1::error::Code::NotFound));
    // Stock text is refused with a reason, from anyone.
    g.send(
        kind::TEXT_MESSAGE,
        &proto::TextMessage {
            channel_id: vec![0],
            message: "in the clear".into(),
            ..Default::default()
        },
    )
    .await;
    let denied = g.until(kind::PERMISSION_DENIED).await;
    let denied = proto::PermissionDenied::decode(denied.payload).unwrap();
    assert!(denied.reason().contains("end-to-end"), "{denied:?}");

    // Registration is still possible; a registered account sends no plaintext
    // either (its conversations are MLS groups).
    let mut guest = guest;
    g.register(&mut guest).await;
    assert_ne!(guest.user_id, 0);
    let id = request_id();
    let plain = g
        .request(
            nk::CHAT_SEND,
            &v1::ChatSend {
                request_id: id,
                conversation: lobby.to_vec(),
                idempotency_key: key().to_vec(),
                format: v1::Format::Text as i32,
                content: "hello".into(),
                reply_to: 0,
            },
            id,
        )
        .await;
    assert_eq!(code(&plain), Some(v1::error::Code::Unsupported));
}

#[tokio::test]
async fn hostile_key_packages_are_refused_and_the_server_stays_up() {
    let fixture = Fixture::start().await;
    let mut mallory = User::new("mallory");
    let mut m = join(&fixture, &mut mallory).await;
    let poisoned = short_init_key_package(&mallory);
    // Wait for the binding with a valid upload first.
    let status = m.upload(vec![mallory.key_package(false)], Vec::new()).await;
    assert_eq!(
        status.kind,
        nk::MLS_KEY_PACKAGE_STATUS,
        "{:?}",
        error(&status)
    );
    let id = request_id();
    let refused = m
        .request(
            nk::MLS_KEY_PACKAGE_UPLOAD,
            &v1::MlsKeyPackageUpload {
                request_id: id,
                key_packages: vec![poisoned.clone()],
                last_resort: Vec::new(),
            },
            id,
        )
        .await;
    assert_eq!(code(&refused), Some(v1::error::Code::Malformed));
    // As a last-resort package, and as garbage: the same.
    let id = request_id();
    let refused = m
        .request(
            nk::MLS_KEY_PACKAGE_UPLOAD,
            &v1::MlsKeyPackageUpload {
                request_id: id,
                key_packages: vec![vec![0xff; 300]],
                last_resort: poisoned,
            },
            id,
        )
        .await;
    assert_eq!(code(&refused), Some(v1::error::Code::Malformed));
    // A key package for another identity's device is refused too.
    let other = User::new("other");
    let id = request_id();
    let refused = m
        .request(
            nk::MLS_KEY_PACKAGE_UPLOAD,
            &v1::MlsKeyPackageUpload {
                request_id: id,
                key_packages: vec![other.key_package(false)],
                last_resort: Vec::new(),
            },
            id,
        )
        .await;
    assert_eq!(code(&refused), Some(v1::error::Code::Malformed));
    // The server is fine: another upload succeeds.
    let status = m.upload(vec![mallory.key_package(false)], Vec::new()).await;
    let status = v1::MlsKeyPackageStatus::decode(status.payload).unwrap();
    assert_eq!(status.one_time, 2);
    // Garbage commits and messages for a conversation without a group.
    let conversation = [9u8; 16];
    let answer = m.send_mls(conversation, key(), vec![0, 1, 0, 2, 9]).await;
    assert_eq!(code(&answer), Some(v1::error::Code::NotFound));
    // At most 50 one-time packages per device; dropped ones lose their routing.
    let mut last = None;
    for _ in 0..4 {
        let batch = (0..16).map(|_| mallory.key_package(false)).collect();
        last = Some(m.upload(batch, Vec::new()).await);
    }
    let status = v1::MlsKeyPackageStatus::decode(last.unwrap().payload).unwrap();
    assert_eq!(status.one_time, 50);
    let references = fixture
        .store
        .lock()
        .unwrap()
        .kt
        .keys()
        .filter(|key| key.first() == Some(&b'K'))
        .count();
    assert_eq!(references, 50);
}

#[tokio::test]
async fn strangers_cannot_be_added_and_claims_need_a_conversation() {
    let fixture = Fixture::start().await;
    let Pair {
        alice,
        mut a,
        conversation,
        group,
        ..
    } = pair(&fixture).await;
    let mut carol = User::new("carol");
    let mut c = join(&fixture, &mut carol).await;
    let status = c.upload(vec![carol.key_package(false)], Vec::new()).await;
    assert_eq!(
        status.kind,
        nk::MLS_KEY_PACKAGE_STATUS,
        "{:?}",
        error(&status)
    );
    // No direct conversation with Carol: her key packages are not Alice's to claim.
    assert_eq!(
        code(&a.claim(carol.account).await),
        Some(v1::error::Code::NotFound)
    );
    let _ = a.open_direct(&carol).await;
    let claimed = a.claim(carol.account).await;
    let packages = v1::MlsKeyPackages::decode(claimed.payload)
        .unwrap()
        .key_packages;
    assert_eq!(packages.len(), 1);
    // Adding Carol to Alice and Bob's conversation is refused...
    let add = alice
        .mls
        .add(
            &alice.identity.device,
            &group,
            &[ValidKeyPackage::check(&packages[0]).unwrap()],
        )
        .unwrap();
    let refused = a
        .commit(v1::MlsCommit {
            request_id: 0,
            conversation: conversation.to_vec(),
            idempotency_key: key().to_vec(),
            commit: add.commit,
            group_info: add.group_info,
            welcome: add.welcome.unwrap_or_default(),
            create: None,
        })
        .await;
    assert_eq!(code(&refused), Some(v1::error::Code::Malformed));
    alice.mls.clear_pending(&group).unwrap();
    // ...and the group is not stuck.
    let update = alice
        .mls
        .self_update(&alice.identity.device, &group)
        .unwrap();
    let accepted = ack(&a
        .commit(v1::MlsCommit {
            request_id: 0,
            conversation: conversation.to_vec(),
            idempotency_key: key().to_vec(),
            commit: update.commit,
            group_info: update.group_info,
            welcome: Vec::new(),
            create: None,
        })
        .await);
    assert_eq!(accepted.epoch, 2);
}

#[tokio::test]
async fn groups_and_mailboxes_survive_a_restart() {
    let fixture = Fixture::start().await;
    let Pair {
        alice,
        bob,
        a,
        b,
        conversation,
        group,
        created_seq,
        ..
    } = pair(&fixture).await;
    drop((a, b));
    let fixture = fixture.restart().await;
    let mut a = Connection::connect(fixture.address(), &alice).await;
    let mut b = Connection::connect(fixture.address(), &bob).await;
    // The Welcome is still waiting; the group is where it was.
    let waiting = b.welcomes(bob.device()).await;
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].seq, created_seq);
    let info = v1::MlsJoinInfo::decode(b.join_info(conversation).await.payload).unwrap();
    assert_eq!((info.epoch, info.commit_seq), (1, created_seq));
    // Fencing works from the restored epoch; Bob decrypts from the stream.
    let message = alice
        .mls
        .encrypt(&alice.identity.device, &group, b"after the restart")
        .unwrap();
    let sent = ack(&a.send_mls(conversation, key(), message).await);
    let id = request_id();
    let page = b
        .request(
            nk::HISTORY_REQUEST,
            &v1::HistoryRequest {
                request_id: id,
                conversation: conversation.to_vec(),
                anchor: Some(v1::history_request::Anchor::After(created_seq)),
                limit: 100,
            },
            id,
        )
        .await;
    let page = v1::HistoryPage::decode(page.payload).unwrap();
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].seq, sent.seq);
    match bob.mls.process(&group, &page.messages[0].mls).unwrap() {
        Incoming::Application { data, .. } => assert_eq!(data, b"after the restart"),
        _ => panic!("expected application data"),
    }
    // A commit after the restart is fenced against the restored state.
    let update = bob.mls.self_update(&bob.identity.device, &group).unwrap();
    let accepted = ack(&b
        .commit(v1::MlsCommit {
            request_id: 0,
            conversation: conversation.to_vec(),
            idempotency_key: key().to_vec(),
            commit: update.commit,
            group_info: update.group_info,
            welcome: Vec::new(),
            create: None,
        })
        .await);
    assert_eq!(accepted.epoch, 2);
    let _ = fixture.store.lock().unwrap().kt.len();
    let _ = fixture.directory.path();
}

fn record_key(prefix: u8, group: &[u8]) -> Vec<u8> {
    let mut key = vec![prefix];
    key.extend_from_slice(group);
    key
}

/// Waits until the service saved the group at `epoch` (it does so after the
/// acknowledgement) and no commit is pending.
async fn saved_at(fixture: &Fixture, group: &[u8], epoch: u64) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        {
            let store = fixture.store.lock().unwrap();
            let saved = store
                .kt
                .get(&record_key(b'E', group))
                .is_some_and(|value| value[..8] == epoch.to_be_bytes());
            if saved && !store.kt.contains_key(&record_key(b'N', group)) {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the group was not saved"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Stops the server, changes its stored state, starts it again on the same port.
async fn restart_with(fixture: Fixture, edit: impl FnOnce(&mut MemoryStore)) -> Fixture {
    let Fixture {
        running,
        store,
        directory,
    } = fixture;
    let port = running.addresses[0].port();
    running.shutdown().await;
    edit(&mut store.lock().unwrap());
    Fixture::start_in(directory, store, port).await
}

async fn page_after(
    connection: &mut Connection,
    conversation: [u8; 16],
    after: u64,
) -> Vec<v1::Message> {
    let id = request_id();
    let page = connection
        .request(
            nk::HISTORY_REQUEST,
            &v1::HistoryRequest {
                request_id: id,
                conversation: conversation.to_vec(),
                anchor: Some(v1::history_request::Anchor::After(after)),
                limit: 100,
            },
            id,
        )
        .await;
    v1::HistoryPage::decode(page.payload).unwrap().messages
}

fn update_commit(user: &User, conversation: [u8; 16], group: &[u8]) -> v1::MlsCommit {
    let update = user.mls.self_update(&user.identity.device, group).unwrap();
    v1::MlsCommit {
        request_id: 0,
        conversation: conversation.to_vec(),
        idempotency_key: key().to_vec(),
        commit: update.commit,
        group_info: update.group_info,
        welcome: Vec::new(),
        create: None,
    }
}

#[tokio::test]
async fn a_commit_stored_before_a_crash_is_adopted_on_restart() {
    let fixture = Fixture::start().await;
    let Pair {
        alice,
        bob,
        a,
        mut b,
        conversation,
        group,
        created_seq,
        ..
    } = pair(&fixture).await;
    saved_at(&fixture, &group, 1).await;
    let (old_group, old_epoch) = {
        let store = fixture.store.lock().unwrap();
        (
            store.kt[&record_key(b'G', &group)].clone(),
            store.kt[&record_key(b'E', &group)].clone(),
        )
    };
    // Bob's update is stored and acknowledged ...
    let update = update_commit(&bob, conversation, &group);
    let updated = ack(&b.commit(update.clone()).await);
    assert_eq!(updated.epoch, 2);
    bob.mls.merge_pending(&group).unwrap();
    saved_at(&fixture, &group, 2).await;
    drop((a, b));
    // ... and the server crashes after storing its row, before saving the group:
    // the old state and the pending record are what is left.
    let fixture = restart_with(fixture, |store| {
        let new_group = store.kt[&record_key(b'G', &group)].clone();
        store.kt.insert(
            record_key(b'N', &group),
            nuscheld::mls::pending_record(created_seq, 2, &update.commit, &new_group),
        );
        store.kt.insert(record_key(b'G', &group), old_group);
        store.kt.insert(record_key(b'E', &group), old_epoch);
    })
    .await;
    assert!(
        !fixture
            .store
            .lock()
            .unwrap()
            .kt
            .contains_key(&record_key(b'N', &group))
    );
    let mut a = Connection::connect(fixture.address(), &alice).await;
    let mut b = Connection::connect(fixture.address(), &bob).await;
    let info = v1::MlsJoinInfo::decode(b.join_info(conversation).await.payload).unwrap();
    assert_eq!((info.epoch, info.commit_seq), (2, updated.seq));
    // Alice catches up from the stream; her message and Bob's next commit are
    // accepted at the adopted epoch.
    let rows = page_after(&mut a, conversation, created_seq).await;
    let commit = rows.iter().find(|row| row.seq == updated.seq).unwrap();
    match alice.mls.process(&group, &commit.mls).unwrap() {
        Incoming::Commit(commit) => assert_eq!(alice.mls.merge(&group, commit).unwrap(), 2),
        _ => panic!("expected a commit"),
    }
    let message = alice
        .mls
        .encrypt(&alice.identity.device, &group, b"after the crash")
        .unwrap();
    assert_eq!(
        ack(&a.send_mls(conversation, key(), message).await).epoch,
        2
    );
    let next = ack(&b.commit(update_commit(&bob, conversation, &group)).await);
    assert_eq!(next.epoch, 3);
}

#[tokio::test]
async fn a_pending_commit_without_its_row_is_dropped_on_restart() {
    let fixture = Fixture::start().await;
    let Pair {
        alice,
        bob,
        a,
        b,
        conversation,
        group,
        created_seq,
        ..
    } = pair(&fixture).await;
    saved_at(&fixture, &group, 1).await;
    drop((a, b));
    // The server crashed after writing a commit's pending record, before its row.
    let fixture = restart_with(fixture, |store| {
        let current = store.kt[&record_key(b'G', &group)].clone();
        store.kt.insert(
            record_key(b'N', &group),
            nuscheld::mls::pending_record(created_seq, 2, b"a commit never stored", &current),
        );
    })
    .await;
    assert!(
        !fixture
            .store
            .lock()
            .unwrap()
            .kt
            .contains_key(&record_key(b'N', &group))
    );
    let mut a = Connection::connect(fixture.address(), &alice).await;
    let mut b = Connection::connect(fixture.address(), &bob).await;
    let info = v1::MlsJoinInfo::decode(b.join_info(conversation).await.payload).unwrap();
    assert_eq!((info.epoch, info.commit_seq), (1, created_seq));
    let message = alice
        .mls
        .encrypt(&alice.identity.device, &group, b"still epoch 1")
        .unwrap();
    assert_eq!(
        ack(&a.send_mls(conversation, key(), message).await).epoch,
        1
    );
    let next = ack(&b.commit(update_commit(&bob, conversation, &group)).await);
    assert_eq!(next.epoch, 2);
}
