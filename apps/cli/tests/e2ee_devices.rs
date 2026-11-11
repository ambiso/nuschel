// SPDX-License-Identifier: EUPL-1.2
//! Three devices in one end-to-end direct conversation (docs/e2ee.md §3.4, §6):
//! Alice's one device, Bob's laptop and the phone he links to it. The link runs
//! between two `nuschel-cli --offline` processes (the test relays the messages, as
//! the server's linking mailbox will); the phone uses Bob's Mumble certificate, so
//! it is the same account, and the devices connect one after the other (a second
//! login of an account disconnects the first). Build the crypto worker first:
//! `cargo build -p nuschel-crypto-worker`.
use nuscheld::{
    config::Config,
    server::{Running, start},
    storage::{Backend, MemoryStore},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::mpsc,
    time::timeout,
};

const WAIT: Duration = Duration::from_secs(60);

fn cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nuschel-cli"))
}

fn worker_present() -> bool {
    cli()
        .with_file_name(format!(
            "nuschel-crypto-worker{}",
            std::env::consts::EXE_SUFFIX
        ))
        .exists()
}

async fn server(directory: &Path) -> Running {
    let config = Config::from_toml(&format!(
        "bind = [\"127.0.0.1\"]\nport = 0\nstate_directory = {directory:?}\n\
         message_limit = 1000\nmessage_burst = 1000\n\
         [autoban]\nattempts = 0\n[password_hashing]\nmemory_kib = 64\npasses = 1\n\
         [kt]\npublish_interval = 1\n",
    ))
    .unwrap();
    start(
        config.settings().unwrap(),
        Backend::Memory(MemoryStore::shared()),
    )
    .await
    .unwrap()
}

struct Cli {
    name: String,
    child: Child,
    input: ChildStdin,
    events: mpsc::UnboundedReceiver<Value>,
    backlog: VecDeque<Value>,
    session: u64,
}

impl Cli {
    fn command(server: &Running, name: &str, profile: &Path) -> Command {
        let address = server.addresses[0];
        let mut command = Command::new(cli());
        command
            .arg("--host")
            .arg(address.ip().to_string())
            .arg("--port")
            .arg(address.port().to_string())
            .arg("--username")
            .arg(name)
            .arg("--certificate-sha256")
            .arg(&server.fingerprint)
            .arg("--profile-dir")
            .arg(profile)
            .arg("--force-tcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        command
    }

    fn start(name: &str, mut command: Command) -> Self {
        let mut child = command.spawn().expect("nuschel-cli starts");
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (sender, events) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(output).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    let _ = sender.send(value);
                }
            }
        });
        Self {
            name: name.to_owned(),
            child,
            input,
            events,
            backlog: VecDeque::new(),
            session: 0,
        }
    }

    async fn spawn(
        server: &Running,
        name: &str,
        profile: &Path,
        identity: Option<&Path>,
        extra: &[&std::ffi::OsStr],
    ) -> Self {
        let mut command = Self::command(server, name, profile);
        if let Some(identity) = identity {
            command.arg("--identity-dir").arg(identity).arg("--e2ee");
        }
        command.args(extra);
        let mut cli = Self::start(name, command);
        cli.session = cli
            .wait("connected", |event| {
                (event["kind"] == "connected").then(|| event["session"].as_u64().unwrap())
            })
            .await;
        cli
    }

    /// The crypto worker alone, for linking.
    async fn offline(server: &Running, name: &str, profile: &Path, identity: &Path) -> Self {
        let mut command = Self::command(server, name, profile);
        command.arg("--identity-dir").arg(identity).arg("--offline");
        let mut cli = Self::start(name, command);
        cli.wait("offline", |event| {
            (event["kind"] == "offline").then_some(())
        })
        .await;
        cli
    }

    async fn send(&mut self, command: Value) {
        let mut line = serde_json::to_vec(&command).unwrap();
        line.push(b'\n');
        self.input.write_all(&line).await.unwrap();
        self.input.flush().await.unwrap();
    }

    async fn wait<T>(&mut self, what: &str, mut pick: impl FnMut(&Value) -> Option<T>) -> T {
        if let Some(index) = self.backlog.iter().position(|event| pick(event).is_some()) {
            let event = self.backlog.remove(index).unwrap();
            return pick(&event).unwrap();
        }
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(event)) => {
                    if event["kind"] == "link_failed" {
                        panic!("{}: {event}", self.name);
                    }
                    if let Some(found) = pick(&event) {
                        return found;
                    }
                    self.backlog.push_back(event);
                }
                Ok(None) => panic!(
                    "{}: the CLI exited while waiting for {what}; events: {:#?}",
                    self.name, self.backlog
                ),
                Err(_) => panic!(
                    "{}: timed out waiting for {what}; events: {:#?}",
                    self.name,
                    self.backlog
                        .iter()
                        .filter(|event| event["kind"] != "user"
                            && event["kind"] != "channel"
                            && event["kind"] != "transport")
                        .collect::<Vec<_>>()
                ),
            }
        }
    }

    /// No own-key alarm was raised on this device.
    fn assert_no_alarm(&self) {
        for event in &self.backlog {
            assert_ne!(
                event["outcome"]["status"], "own_key_alarm",
                "{}: {event}",
                self.name
            );
            assert!(
                event["view"]["alarms"]
                    .as_array()
                    .is_none_or(|alarms| alarms.is_empty()),
                "{}: {event}",
                self.name
            );
        }
    }

    async fn quit(mut self) {
        self.assert_no_alarm();
        self.send(json!({"kind":"disconnect"})).await;
        let _ = timeout(Duration::from_secs(10), self.child.wait()).await;
    }

    async fn register(server: &Running, name: &str, profile: &Path) -> u64 {
        let mut cli = Self::spawn(server, name, profile, None, &[]).await;
        cli.send(json!({"kind":"register"})).await;
        let session = cli.session;
        let id = cli
            .wait("registration", |event| {
                (event["kind"] == "user" && event["user"]["session"].as_u64() == Some(session))
                    .then(|| event["user"]["user_id"].as_u64())
                    .flatten()
            })
            .await;
        cli.quit().await;
        id
    }

    async fn key_packages_uploaded(&mut self) {
        self.wait("key packages", |event| {
            (event["kind"] == "e2ee_key_packages" && event["one_time"].as_u64() >= Some(4))
                .then_some(())
        })
        .await;
    }

    async fn established(&mut self, conversation: Option<&str>) -> Value {
        self.wait("an established conversation", |event| {
            let view = &event["view"];
            (event["kind"] == "e2ee_conversation"
                && view["pinned"] == true
                && conversation.is_none_or(|c| event["conversation"] == c)
                && (view["lock"]["state"] == "encrypted" || view["lock"]["state"] == "verified"))
                .then(|| event.clone())
        })
        .await
    }

    async fn received(&mut self, text: &str) -> Value {
        self.wait(&format!("the message {text:?}"), |event| {
            (event["kind"] == "e2ee_received"
                && event["received"]["kind"] == "message"
                && event["received"]["text"] == text)
                .then(|| event.clone())
        })
        .await
    }

    async fn sent(&mut self, conversation: &str, text: &str) -> u64 {
        self.send(json!({"kind":"e2ee_send", "conversation":conversation, "text":text}))
            .await;
        self.wait(&format!("the ack of {text:?}"), |event| {
            if event["kind"] == "e2ee_not_sent" {
                panic!("not sent: {event}");
            }
            (event["kind"] == "e2ee_sent" && event["conversation"] == conversation)
                .then(|| event["seq"].as_u64().unwrap())
        })
        .await
    }
}

/// Links a new device (`new_identity`, empty) to the identity in `identity`.
async fn link(server: &Running, profile: &Path, identity: &Path, new_identity: &Path) -> Value {
    let mut existing = Cli::offline(server, "bob", profile, identity).await;
    let mut new = Cli::offline(server, "bob", profile, new_identity).await;
    existing.send(json!({"kind":"link_offer"})).await;
    let offer = existing
        .wait("the link offer", |event| {
            (event["kind"] == "link_offer").then(|| event.clone())
        })
        .await;
    new.send(
        json!({"kind":"link_join", "code":offer["code"], "pake":offer["pake"],
        "device_name":"Bob's phone"}),
    )
    .await;
    let join = new
        .wait("the new device's keys", |event| {
            (event["kind"] == "link_join").then(|| event.clone())
        })
        .await;
    existing
        .send(
            json!({"kind":"link_approve", "pake":join["pake"], "device_keys":join["device_keys"]}),
        )
        .await;
    let bundle = existing
        .wait("the bundle", |event| {
            (event["kind"] == "link_bundle").then(|| event["bundle"].clone())
        })
        .await;
    new.send(json!({"kind":"link_complete", "bundle":bundle}))
        .await;
    let linked = new
        .wait("the linked identity", |event| {
            (event["kind"] == "identity_own").then(|| event["view"].clone())
        })
        .await;
    existing.send(json!({"kind":"identity_own"})).await;
    let own = existing
        .wait("the existing identity", |event| {
            (event["kind"] == "identity_own").then(|| event["view"].clone())
        })
        .await;
    assert_eq!(linked["fingerprint"], own["fingerprint"], "{linked} {own}");
    assert_eq!(linked["can_link"], true, "{linked}");
    existing.quit().await;
    new.quit().await;
    linked
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_devices_in_one_direct_conversation() {
    if !worker_present() {
        panic!("build nuschel-crypto-worker first: cargo build -p nuschel-crypto-worker");
    }
    let state = tempfile::tempdir().unwrap();
    let running = server(state.path()).await;
    let directories: Vec<tempfile::TempDir> =
        (0..6).map(|_| tempfile::tempdir().unwrap()).collect();
    let (alice_profile, alice_identity) = (directories[0].path(), directories[1].path());
    let (bob_profile, laptop_identity) = (directories[2].path(), directories[3].path());
    let (phone_profile, phone_identity) = (directories[4].path(), directories[5].path());
    let _alice_id = Cli::register(&running, "alice", alice_profile).await;
    let bob_id = Cli::register(&running, "bob", bob_profile).await;

    // Bob's laptop: his identity, bound to his account.
    let mut laptop = Cli::spawn(&running, "bob", bob_profile, Some(laptop_identity), &[]).await;
    laptop.key_packages_uploaded().await;
    laptop.quit().await;

    // The phone: Bob's Mumble certificate (the same account), linked to the laptop.
    let certificate = state.path().join("bob.p12");
    let exported = Cli::command(&running, "bob", bob_profile)
        .arg("--export-certificate")
        .arg(&certificate)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(exported.success());
    link(&running, bob_profile, laptop_identity, phone_identity).await;

    // The phone publishes the two-device roster and uploads its key packages (the
    // server accepts them only for a device in the roster).
    let import: [&std::ffi::OsStr; 2] = ["--import-certificate".as_ref(), certificate.as_os_str()];
    let mut phone = Cli::spawn(
        &running,
        "bob",
        phone_profile,
        Some(phone_identity),
        &import,
    )
    .await;
    phone.key_packages_uploaded().await;
    phone.quit().await;

    // Alice opens the conversation: her device and both of Bob's.
    let mut alice = Cli::spawn(&running, "alice", alice_profile, Some(alice_identity), &[]).await;
    alice.key_packages_uploaded().await;
    alice
        .send(json!({"kind":"e2ee_open", "user_id":bob_id}))
        .await;
    let view = alice.established(None).await;
    let conversation = view["conversation"].as_str().unwrap().to_owned();
    let members = view["view"]["members"].as_array().unwrap();
    assert_eq!(members.len(), 3, "{view}");
    assert!(
        members.iter().all(|member| member["verified"] == true),
        "{view}"
    );
    alice.sent(&conversation, "hello to both of you").await;

    // The laptop joins from its mailbox and reads it; nothing alarms it about the
    // binding the phone published.
    let mut laptop = Cli::spawn(&running, "bob", bob_profile, Some(laptop_identity), &[]).await;
    let joined = laptop.established(Some(&conversation)).await;
    assert_eq!(
        joined["view"]["members"].as_array().unwrap().len(),
        3,
        "{joined}"
    );
    laptop.received("hello to both of you").await;
    laptop.quit().await;

    // The phone too, and it answers.
    let mut phone = Cli::spawn(&running, "bob", phone_profile, Some(phone_identity), &[]).await;
    phone.established(Some(&conversation)).await;
    phone.received("hello to both of you").await;
    phone.sent(&conversation, "the phone answers").await;
    phone.quit().await;
    let answer = alice.received("the phone answers").await;
    assert_eq!(answer["received"]["sender"]["own"], false, "{answer}");

    // The laptop reads the phone's answer later: one of Bob's own devices.
    let mut laptop = Cli::spawn(&running, "bob", bob_profile, Some(laptop_identity), &[]).await;
    let answer = laptop.received("the phone answers").await;
    assert_eq!(answer["received"]["sender"]["own"], true, "{answer}");
    assert_eq!(
        answer["received"]["sender"]["device_name"], "Bob's phone",
        "{answer}"
    );
    laptop.quit().await;
    alice.quit().await;
}

/// A device linked after the conversation exists: the laptop sees it in its own
/// roster and adds it; Alice accepts it once key transparency lists it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_linked_device_joins_an_existing_conversation() {
    if !worker_present() {
        panic!("build nuschel-crypto-worker first: cargo build -p nuschel-crypto-worker");
    }
    let state = tempfile::tempdir().unwrap();
    let running = server(state.path()).await;
    let directories: Vec<tempfile::TempDir> =
        (0..6).map(|_| tempfile::tempdir().unwrap()).collect();
    let (alice_profile, alice_identity) = (directories[0].path(), directories[1].path());
    let (bob_profile, laptop_identity) = (directories[2].path(), directories[3].path());
    let (phone_profile, phone_identity) = (directories[4].path(), directories[5].path());
    let _alice_id = Cli::register(&running, "alice", alice_profile).await;
    let bob_id = Cli::register(&running, "bob", bob_profile).await;

    let mut laptop = Cli::spawn(&running, "bob", bob_profile, Some(laptop_identity), &[]).await;
    laptop.key_packages_uploaded().await;
    let mut alice = Cli::spawn(&running, "alice", alice_profile, Some(alice_identity), &[]).await;
    alice.key_packages_uploaded().await;
    alice
        .send(json!({"kind":"e2ee_open", "user_id":bob_id}))
        .await;
    let view = alice.established(None).await;
    let conversation = view["conversation"].as_str().unwrap().to_owned();
    assert_eq!(
        view["view"]["members"].as_array().unwrap().len(),
        2,
        "{view}"
    );
    laptop.established(Some(&conversation)).await;
    laptop.quit().await;

    let certificate = state.path().join("bob.p12");
    let exported = Cli::command(&running, "bob", bob_profile)
        .arg("--export-certificate")
        .arg(&certificate)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(exported.success());
    link(&running, bob_profile, laptop_identity, phone_identity).await;
    let import: [&std::ffi::OsStr; 2] = ["--import-certificate".as_ref(), certificate.as_os_str()];
    let mut phone = Cli::spawn(
        &running,
        "bob",
        phone_profile,
        Some(phone_identity),
        &import,
    )
    .await;
    phone.key_packages_uploaded().await;
    phone.quit().await;

    // The laptop finds the phone in its own roster and adds it.
    let mut laptop = Cli::spawn(&running, "bob", bob_profile, Some(laptop_identity), &[]).await;
    laptop
        .wait("the phone added", |event| {
            (event["kind"] == "e2ee_conversation"
                && event["view"]["members"]
                    .as_array()
                    .is_some_and(|m| m.len() == 3))
            .then_some(())
        })
        .await;
    laptop.quit().await;
    // Alice processes the commit, looks Bob up again, and may send.
    alice
        .wait("three verified members", |event| {
            let view = &event["view"];
            (event["kind"] == "e2ee_conversation"
                && view["members"].as_array().is_some_and(|m| m.len() == 3)
                && view["lock"]["state"] == "encrypted"
                && view["blocked"].is_null())
            .then_some(())
        })
        .await;
    alice.sent(&conversation, "welcome, phone").await;

    let mut phone = Cli::spawn(&running, "bob", phone_profile, Some(phone_identity), &[]).await;
    phone.established(Some(&conversation)).await;
    phone.received("welcome, phone").await;
    phone.quit().await;
    alice.quit().await;
}
