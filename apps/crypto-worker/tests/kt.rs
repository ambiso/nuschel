// SPDX-License-Identifier: EUPL-1.2
//! Key transparency end to end on loopback: `nuscheld` with its directory, the
//! real client session, and this crate's worker in its sandbox (bubblewrap on
//! Linux, as the desktop starts it; directly where no sandbox helper exists).
//!
//! Each user has their own worker directory. Users register on a first connection
//! without key transparency and reconnect with it, as an account exists only after
//! registration.
use nuschel_ipc::crypto::{
    ContactState, ContactView, HeadOutcome, KeyTransparencySettings, MonitorOutcome,
    RecoveryPolicy, WitnessSetting, WitnessStatus,
};
use nuschel_protocol::nuschel::v1;
use nuschel_session::{
    config::{ClientIdentity, ServerBookmark},
    connection::{self, Command, ConnectOptions, SessionEvent},
    identity::{CryptoHandle, IdentityCommand, IdentityEvent, IdentitySetup},
    sticky::Requirement,
};
use nuscheld::{
    config::Config,
    server::{Running, start},
    storage::{Backend, MemoryStore},
};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{sync::mpsc, time::timeout};

const WAIT: Duration = Duration::from_secs(30);

fn worker_executable() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nuschel-crypto-worker"))
}

/// The worker as the desktop starts it: sandboxed when bubblewrap is there.
fn crypto(directory: &Path) -> CryptoHandle {
    let sandboxed =
        nuschel_ipc::host::crypto_sandbox_command(&worker_executable(), directory).is_ok();
    if sandboxed {
        CryptoHandle::start(worker_executable(), directory.to_owned())
    } else {
        let directory = directory.to_owned();
        CryptoHandle::start_with(Box::new(move || {
            let mut command = tokio::process::Command::new(worker_executable());
            command.arg("--data").arg(&directory);
            Ok(command)
        }))
    }
}

struct Server {
    running: Running,
    store: Arc<Mutex<MemoryStore>>,
    directory: tempfile::TempDir,
}

impl Server {
    async fn start() -> Self {
        Self::start_in(tempfile::tempdir().unwrap(), MemoryStore::shared(), 0).await
    }

    /// On `port` (0: any), so that a restart keeps the address clients know.
    async fn start_in(
        directory: tempfile::TempDir,
        store: Arc<Mutex<MemoryStore>>,
        port: u16,
    ) -> Self {
        let config = Config::from_toml(&format!(
            "bind = [\"127.0.0.1\"]\nport = {port}\nstate_directory = {:?}\n\
             message_limit = 1000\nmessage_burst = 1000\n\
             [autoban]\nattempts = 0\n[password_hashing]\nmemory_kib = 64\npasses = 1\n\
             [kt]\npublish_interval = 1\n",
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

    /// With a local witness: the origin is this server's loopback address, and
    /// submissions go out every second.
    async fn start_witnessed(witness: &nuschel_witness::Running) -> Self {
        // The port is part of the origin, so it is chosen before the start.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let directory = tempfile::tempdir().unwrap();
        let store = MemoryStore::shared();
        let config = Config::from_toml(&format!(
            "bind = [\"127.0.0.1\"]\nport = {port}\nstate_directory = {:?}\n\
             message_limit = 1000\nmessage_burst = 1000\n\
             [autoban]\nattempts = 0\n[password_hashing]\nmemory_kib = 64\npasses = 1\n\
             [kt]\npublish_interval = 1\norigins = [\"127.0.0.1:{port}\"]\nwitness_interval = 1\n\
             [[kt.witnesses]]\nkey = {:?}\nurl = \"http://{}\"\n",
            directory.path(),
            witness.vkey,
            witness.address,
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
}

fn mumble_identity(index: usize) -> ClientIdentity {
    static IDENTITIES: OnceLock<Vec<ClientIdentity>> = OnceLock::new();
    IDENTITIES.get_or_init(|| {
        (0..4)
            .map(|_| ClientIdentity::generate().unwrap())
            .collect()
    })[index]
        .clone()
}

struct User {
    name: &'static str,
    index: usize,
    worker: tempfile::TempDir,
    crypto: CryptoHandle,
    witnessing: KeyTransparencySettings,
}

impl User {
    fn new(name: &'static str, index: usize) -> Self {
        let worker = tempfile::tempdir().unwrap();
        let crypto = crypto(worker.path());
        Self {
            name,
            index,
            worker,
            crypto,
            witnessing: KeyTransparencySettings::default(),
        }
    }

    /// A new device for the same Mumble account: a fresh worker, so a new identity.
    fn new_identity(&mut self) {
        self.worker = tempfile::tempdir().unwrap();
        self.crypto = crypto(self.worker.path());
    }

    fn options(&self, server: &Server, kt: bool) -> ConnectOptions {
        let address = server.address();
        let mut bookmark =
            ServerBookmark::new("", &address.ip().to_string(), address.port(), self.name).unwrap();
        bookmark.name = "Test server".into();
        bookmark.certificate_sha256 = Some(server.running.fingerprint.clone());
        ConnectOptions {
            server: bookmark,
            identity: mumble_identity(self.index),
            password: None,
            force_tcp: true,
            relay_only: false,
            muted: true,
            deafened: false,
            channel: None,
            resend: Vec::new(),
            message_rate: Default::default(),
            required: Requirement::default(),
            crypto: kt.then(|| IdentitySetup {
                witnessing: self.witnessing.clone(),
                crypto: self.crypto.clone(),
                device_name: format!("{}'s laptop", self.name),
                recovery: RecoveryPolicy::Offer,
            }),
        }
    }

    /// Registers this user's certificate (without key transparency); returns the
    /// registration number.
    async fn register(&self, server: &Server) -> u32 {
        let mut session = Session::connect(self.options(server, false)).await;
        session.commands.send(Command::RegisterSelf).await.unwrap();
        let me = session.session;
        let id = session
            .wait(|event| match event {
                SessionEvent::User(user) if user.session == me => user.user_id,
                _ => None,
            })
            .await;
        session.disconnect().await;
        id
    }

    async fn connect(&self, server: &Server) -> Session {
        Session::connect(self.options(server, true)).await
    }
}

struct Session {
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<SessionEvent>,
    task: tokio::task::JoinHandle<Result<(), nuschel_session::Error>>,
    session: u32,
    backlog: VecDeque<SessionEvent>,
}

impl Session {
    async fn connect(options: ConnectOptions) -> Self {
        let (commands, receiver) = mpsc::channel(32);
        let (sender, events) = mpsc::channel(256);
        let task = tokio::spawn(connection::run(options, receiver, sender));
        let mut session = Self {
            commands,
            events,
            task,
            session: 0,
            backlog: VecDeque::new(),
        };
        session.session = session
            .wait(|event| match event {
                SessionEvent::Connected { session, .. } => Some(*session),
                _ => None,
            })
            .await;
        session
    }

    async fn disconnect(self) {
        let _ = self.commands.send(Command::Disconnect).await;
        let _ = timeout(WAIT, self.task).await;
    }

    async fn wait<T>(&mut self, mut pick: impl FnMut(&SessionEvent) -> Option<T>) -> T {
        if let Some(index) = self.backlog.iter().position(|event| pick(event).is_some()) {
            let event = self.backlog.remove(index).unwrap();
            return pick(&event).unwrap();
        }
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(event) = timeout(left, self.events.recv()).await.ok().flatten() else {
                panic!(
                    "no matching session event; identity events so far: {:#?}",
                    self.backlog
                        .iter()
                        .filter(|event| matches!(event, SessionEvent::Identity(_)))
                        .collect::<Vec<_>>()
                );
            };
            if let Some(value) = pick(&event) {
                return value;
            }
            self.backlog.push_back(event);
        }
    }

    async fn identity(&mut self, mut pick: impl FnMut(&IdentityEvent) -> bool) -> IdentityEvent {
        self.wait(|event| match event {
            SessionEvent::Identity(event) if pick(event) => Some(event.clone()),
            _ => None,
        })
        .await
    }

    /// Waits until our binding is in the log and self-monitoring found it.
    async fn published(&mut self) {
        self.identity(|event| {
            matches!(
                event,
                IdentityEvent::Monitor(MonitorOutcome::Consistent { current: true, .. })
            )
        })
        .await;
    }

    async fn command(&self, command: IdentityCommand) {
        self.commands
            .send(Command::Identity(command))
            .await
            .unwrap();
    }

    /// Checks a user and returns the first view of them (after the lookup).
    async fn check(&mut self, user_id: u32, direct: bool) -> ContactView {
        self.check_named(user_id, direct, None).await
    }

    /// The same, with the name the server shows for them.
    async fn check_named(&mut self, user_id: u32, direct: bool, name: Option<&str>) -> ContactView {
        self.backlog.retain(|event| {
            !matches!(event, SessionEvent::Identity(IdentityEvent::Contact { .. }))
        });
        self.command(IdentityCommand::Check {
            name: name.map(str::to_owned),
            user_id: Some(user_id),
            account: None,
            direct,
        })
        .await;
        match self
            .identity(|event| {
                matches!(event, IdentityEvent::Contact { user_id: Some(id), .. } if *id == user_id)
            })
            .await
        {
            IdentityEvent::Contact { view, .. } => view,
            _ => unreachable!(),
        }
    }

    async fn contact(&mut self, mut pick: impl FnMut(&ContactView) -> bool) -> ContactView {
        match self
            .identity(|event| matches!(event, IdentityEvent::Contact { view, .. } if pick(view)))
            .await
        {
            IdentityEvent::Contact { view, .. } => view,
            _ => unreachable!(),
        }
    }
}

fn account(view: &ContactView) -> [u8; 16] {
    view.account.clone().try_into().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_look_up_trust_on_first_use_and_verify() {
    let server = Server::start().await;
    let alice = User::new("Alice", 0);
    let bob = User::new("Bob", 1);
    let alice_id = alice.register(&server).await;
    let bob_id = bob.register(&server).await;

    let mut a = alice.connect(&server).await;
    // The identity is created on first need, with a recovery key offered.
    let IdentityEvent::Own(own) = a
        .identity(|event| matches!(event, IdentityEvent::Own(view) if view.created))
        .await
    else {
        unreachable!()
    };
    assert!(own.recovery_pending && own.recoverable && own.can_link);
    let alice_fingerprint = own.fingerprint.clone().unwrap();
    a.published().await;

    let mut b = bob.connect(&server).await;
    b.published().await;
    // Bob opens a direct conversation with Alice: looked up, pinned, unverified.
    let view = b.check(alice_id, true).await;
    assert_eq!(view.state, ContactState::Unverified);
    assert_eq!(view.identity.as_deref(), Some(alice_fingerprint.as_str()));
    let safety = view.safety_number.clone().unwrap();
    assert_eq!(safety.len(), 60 + 11, "12 groups of 5 digits");
    assert_eq!(view.qr.len(), 68);
    assert!(view.direct);

    // Alice sees the same safety number for Bob.
    let reverse = a.check(bob_id, false).await;
    assert_eq!(reverse.safety_number.as_deref(), Some(safety.as_str()));
    assert_eq!(reverse.state, ContactState::Unverified);

    // Bob compares out of band and marks Alice verified.
    b.command(IdentityCommand::MarkVerified {
        account: account(&view),
    })
    .await;
    let verified = b
        .contact(|view| matches!(view.state, ContactState::Verified { .. }))
        .await;
    assert_eq!(verified.state, ContactState::Verified { elsewhere: None });

    // The verification survives a reconnect (it lives in the worker's store), and the
    // next check confirms it from the log.
    b.disconnect().await;
    let mut b = bob.connect(&server).await;
    let again = b.check(alice_id, false).await;
    assert_eq!(again.state, ContactState::Verified { elsewhere: None });
    a.disconnect().await;
    b.disconnect().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_change_is_detected_and_blocks_a_verified_contact() {
    let server = Server::start().await;
    let mut alice = User::new("Alice", 0);
    let bob = User::new("Bob", 1);
    let carol = User::new("Carol", 2);
    let alice_id = alice.register(&server).await;
    bob.register(&server).await;
    carol.register(&server).await;

    let mut a = alice.connect(&server).await;
    a.published().await;
    let mut b = bob.connect(&server).await;
    let mut c = carol.connect(&server).await;
    let first = b.check(alice_id, true).await;
    b.command(IdentityCommand::MarkVerified {
        account: account(&first),
    })
    .await;
    b.contact(|view| matches!(view.state, ContactState::Verified { .. }))
        .await;
    let carols = c.check(alice_id, false).await;
    assert_eq!(carols.state, ContactState::Unverified);
    a.disconnect().await;

    // Alice comes back with a new identity (a new device without the old keys).
    alice.new_identity();
    let mut a = alice.connect(&server).await;
    a.published().await;

    // Bob had verified her: the change blocks until he accepts it, and the verified
    // pin stays meanwhile.
    let changed = b.check(alice_id, true).await;
    assert_eq!(changed.state, ContactState::KeyChanged { blocking: true });
    assert!(changed.notice.as_deref().unwrap().contains("changed"));
    assert_eq!(changed.identity, first.identity, "the old pin is kept");
    // Carol had not: a notice, and the new identity is pinned (unverified).
    let carols = c.check(alice_id, false).await;
    assert_eq!(carols.state, ContactState::KeyChanged { blocking: false });
    assert_ne!(carols.identity, first.identity);

    // Accepting pins the new identity, which is not verified.
    b.command(IdentityCommand::AcceptKeyChange {
        account: account(&changed),
    })
    .await;
    let accepted = b
        .contact(|view| view.state == ContactState::Unverified)
        .await;
    assert_eq!(accepted.identity, carols.identity);
    assert_ne!(accepted.safety_number, changed.safety_number);
    a.disconnect().await;
    b.disconnect().await;
    c.disconnect().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn self_monitoring_raises_the_alarm_when_the_server_binds_another_key() {
    let server = Server::start().await;
    let alice = User::new("Alice", 0);
    let alice_id = alice.register(&server).await;
    let mut a = alice.connect(&server).await;
    a.published().await;
    a.disconnect().await;

    // A malicious operator binds a key of their own to Alice's account, with a
    // statement their identity signed for it (the server checks nothing the
    // operator cannot forge for itself).
    let mallory = tempfile::tempdir().unwrap();
    let forged = forged_publication(&server, alice_id, mallory.path()).await;
    server.publish_directly(alice_id, forged).await;

    let mut a = alice.connect(&server).await;
    let alarm = a
        .identity(|event| {
            matches!(
                event,
                IdentityEvent::Monitor(MonitorOutcome::OwnKeyAlarm { .. })
            )
        })
        .await;
    let IdentityEvent::Monitor(MonitorOutcome::OwnKeyAlarm { reason }) = alarm else {
        unreachable!()
    };
    assert!(reason.contains("wasn't you"), "{reason}");
    // The alarm stays with the identity until the user acknowledges it.
    a.command(IdentityCommand::Own).await;
    let own = a
        .identity(|event| matches!(event, IdentityEvent::Own(view) if !view.alarms.is_empty()))
        .await;
    let IdentityEvent::Own(own) = own else {
        unreachable!()
    };
    assert_eq!(own.alarms[0].label, "Test server");
    a.disconnect().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forked_log_and_a_replaced_log_are_detected() {
    let server = Server::start().await;
    let alice = User::new("Alice", 0);
    let bob = User::new("Bob", 1);
    alice.register(&server).await;
    bob.register(&server).await;
    let mut a = alice.connect(&server).await;
    a.published().await;
    a.disconnect().await;
    let mut b = bob.connect(&server).await;
    b.published().await;
    b.disconnect().await;

    // The operator restarts on an empty directory with the same keys: the same log
    // id, but a different history. Bob's worker knows heads the new log contradicts.
    let port = server.address().port();
    let Server {
        running,
        directory,
        store,
    } = server;
    running.shutdown().await;
    {
        let mut store = store.lock().unwrap();
        store.kt.clear();
    }
    let server = Server::start_in(directory, store, port).await;
    // Bob publishes again into the fresh log, which reaches the epoch he knew with
    // another root (or never gets there: a rollback).
    let mut b = bob.connect(&server).await;
    // Its fresh epoch 0 is signed after the epochs Bob knows: signed evidence that
    // the server hides a newer state from him (stale signing), never accepted.
    let IdentityEvent::Log(HeadOutcome::Misbehaviour { reason }) = b
        .identity(|event| matches!(event, IdentityEvent::Log(_)))
        .await
    else {
        panic!("the forked log was accepted")
    };
    // (The fresh log may already hold an epoch of names and numbers.)
    assert!(
        (reason.contains("signed epoch") && reason.contains("after"))
            || reason.contains("two different roots"),
        "{reason}"
    );
    // The worker keeps the evidence and reports it with the identity.
    b.command(IdentityCommand::Own).await;
    let IdentityEvent::Own(own) = b
        .identity(|event| matches!(event, IdentityEvent::Own(view) if !view.alarms.is_empty()))
        .await
    else {
        unreachable!()
    };
    assert_eq!(own.alarms[0].reason, reason);
    b.disconnect().await;

    // A log with other keys behind the same address is refused outright.
    let Server {
        running,
        directory,
        store,
    } = server;
    running.shutdown().await;
    std::fs::remove_file(directory.path().join("kt-signing.key")).unwrap();
    std::fs::remove_file(directory.path().join("kt-vrf.key")).unwrap();
    store.lock().unwrap().kt.clear();
    let server = Server::start_in(directory, store, port).await;
    let mut b = bob.connect(&server).await;
    let IdentityEvent::Log(HeadOutcome::Misbehaviour { reason }) = b
        .identity(|event| matches!(event, IdentityEvent::Log(HeadOutcome::Misbehaviour { .. })))
        .await
    else {
        unreachable!()
    };
    assert!(reason.contains("log changed"), "{reason}");
    b.disconnect().await;
}

// ---- a malicious operator ----

impl Server {
    /// Publishes into the log as the operator could, bypassing every check.
    async fn publish_directly(&self, user_id: u32, publish: v1::KtPublish) {
        let (account, kt) = {
            let state = self.running.server.lock();
            let account = state.registry.users.get(&user_id).unwrap().account;
            (account, state.kt.clone().unwrap())
        };
        assert!(kt.submit(nuscheld::kt::Task {
            session: 0,
            request_id: 1,
            job: nuscheld::kt::Job::Publish { account, publish },
        }));
        // The next epoch (publish_interval = 1 s).
        tokio::time::sleep(Duration::from_millis(2500)).await;
    }
}

/// Another identity's binding for `user_id`'s account: a worker of the operator's
/// own signs a statement for that account.
async fn forged_publication(server: &Server, user_id: u32, directory: &Path) -> v1::KtPublish {
    let (account, descriptor) = {
        let state = server.running.server.lock();
        let kt = state.kt.clone().unwrap();
        let account = state.registry.users.get(&user_id).unwrap().account;
        (account, kt.descriptor.clone())
    };
    let store = nuschel_crypto_worker::Store::open(
        directory,
        nuschel_crypto_worker::KeySource::File(directory.join("store.key")),
    )
    .unwrap();
    let mut worker = nuschel_crypto_worker::Worker::open(store).unwrap();
    // The operator's worker needs a head to accept the log: any signed head will do.
    let status = server_head(server);
    let response = worker.handle(nuschel_ipc::crypto::CryptoRequest::Connect {
        witnessed: None,
        witnessing: Default::default(),
        registration: None,
        server: nuschel_ipc::crypto::CryptoServer {
            key: "operator".into(),
            label: "operator".into(),
        },
        descriptor,
        account: account.to_vec(),
        head: status,
    });
    assert!(
        matches!(
            response,
            nuschel_ipc::crypto::CryptoResponse::Head(HeadOutcome::Accepted { .. })
        ),
        "{response:?}"
    );
    let nuschel_ipc::crypto::CryptoResponse::Bundle(bundle) =
        worker.handle(nuschel_ipc::crypto::CryptoRequest::PublishBundle {
            server: "operator".into(),
            device_name: "mallory".into(),
            recovery: RecoveryPolicy::Never,
        })
    else {
        panic!("no bundle")
    };
    v1::KtPublish {
        request_id: 1,
        root: bundle.root,
        record: Some(v1::KtRecord {
            device_signing_certificate: bundle.record.device_signing_certificate,
            devices: bundle.record.devices,
            revocations: bundle.record.revocations,
            statement: bundle.record.statement,
        }),
    }
}

/// The server's newest signed head (the newest `h` record of its table), read the
/// way the operator can.
fn server_head(server: &Server) -> Vec<u8> {
    let store = server.store.lock().unwrap();
    store
        .kt
        .iter()
        .filter(|(key, _)| key.first() == Some(&b'h'))
        .map(|(_, value)| value.clone())
        .next_back()
        .expect("a signed head")
}

/// The head check of a new connection.
async fn log_outcome(session: &mut Session) -> HeadOutcome {
    match session
        .identity(|event| matches!(event, IdentityEvent::Log(_)))
        .await
    {
        IdentityEvent::Log(outcome) => outcome,
        _ => unreachable!(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_witness_cosigns_the_log_and_clients_require_it() {
    let witness_state = tempfile::tempdir().unwrap();
    let witness = nuschel_witness::start(nuschel_witness::Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        enrol_private_addresses: true,
        ..nuschel_witness::Config::new("witness.test/nuschel-kt", witness_state.path())
    })
    .await
    .unwrap();
    let server = Server::start_witnessed(&witness).await;
    let required = KeyTransparencySettings {
        witnesses: vec![WitnessSetting {
            key: witness.vkey.clone(),
            enabled: true,
        }],
        ..Default::default()
    };
    let mut alice = User::new("Alice", 0);
    let mut bob = User::new("Bob", 1);
    alice.witnessing = required.clone();
    bob.witnessing = required.clone();
    let alice_id = alice.register(&server).await;
    bob.register(&server).await;
    let mut a = alice.connect(&server).await;
    a.published().await;
    a.disconnect().await;

    // The server submits every second; once the auditor has the newest epoch, a
    // connection is witnessed.
    let mut witnessed = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut b = bob.connect(&server).await;
        let outcome = log_outcome(&mut b).await;
        if matches!(
            outcome,
            HeadOutcome::Accepted {
                witness: WitnessStatus::Witnessed { .. },
                ..
            }
        ) {
            witnessed = Some(b);
            break;
        }
        b.disconnect().await;
    }
    let mut b = witnessed.expect("the server's log was never witnessed");
    // Alice, by her number and the name the server shows: both labels name her
    // account in the witnessed log.
    let view = b.check_named(alice_id, false, Some("Alice")).await;
    assert_eq!(view.state, ContactState::Unverified, "{view:?}");
    assert!(view.identity.is_some());
    b.disconnect().await;

    // A name the log does not give her account is not accepted (a fresh device).
    let mut dave = User::new("Bob", 1);
    dave.witnessing = required.clone();
    let mut d = dave.connect(&server).await;
    let view = d.check_named(alice_id, false, Some("Mallory")).await;
    assert!(view.identity.is_none(), "{view:?}");
    d.disconnect().await;

    // A client that requires a witness this server does not reach: unavailable,
    // and first contact still proceeds as "not verified".
    let stranger = nuschel_kt::witness::note::Cosigner::from_seed("other.test", &[5; 32])
        .unwrap()
        .key()
        .vkey();
    let mut carol = User::new("Carol", 2);
    carol.witnessing = KeyTransparencySettings {
        witnesses: vec![WitnessSetting {
            key: stranger,
            enabled: true,
        }],
        ..Default::default()
    };
    carol.register(&server).await;
    let mut c = carol.connect(&server).await;
    let outcome = log_outcome(&mut c).await;
    assert!(
        matches!(
            outcome,
            HeadOutcome::Accepted {
                witness: WitnessStatus::Unavailable { .. },
                ..
            }
        ),
        "{outcome:?}"
    );
    let view = c.check(alice_id, false).await;
    assert!(
        matches!(
            view.state,
            ContactState::Unavailable {
                verified: false,
                ..
            }
        ),
        "{view:?}"
    );
    assert!(view.identity.is_some());
    c.disconnect().await;
}
