// SPDX-License-Identifier: EUPL-1.2
//! End-to-end-encrypted direct conversations (`mls/1`, docs/e2ee.md §6) between
//! `nuschl-cli` processes and `nuschld` on loopback. Each CLI runs the real
//! session and its own crypto worker (sandboxed when bubblewrap exists), found
//! beside the CLI binary: build it first (`cargo build -p nuschl-crypto-worker`).
//!
//! Users register on a first connection without an identity directory and come
//! back with one, as an account exists only after registration.
use nuschld::{
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
    PathBuf::from(env!("CARGO_BIN_EXE_nuschl-cli"))
}

fn worker_present() -> bool {
    cli()
        .with_file_name(format!(
            "nuschl-crypto-worker{}",
            std::env::consts::EXE_SUFFIX
        ))
        .exists()
}

/// An end-to-end server (`security = "e2ee"`), or a compatible one.
async fn server(directory: &Path, e2ee: bool) -> Running {
    let security = if e2ee { "e2ee" } else { "compatible" };
    let config = Config::from_toml(&format!(
        "security = {security:?}\nbind = [\"127.0.0.1\"]\nport = 0\nstate_directory = {directory:?}\n\
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
    async fn spawn(server: &Running, name: &str, profile: &Path, identity: Option<&Path>) -> Self {
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
        if let Some(identity) = identity {
            command.arg("--identity-dir").arg(identity);
        }
        let mut child = command.spawn().expect("nuschl-cli starts");
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
        let mut cli = Self {
            name: name.to_owned(),
            child,
            input,
            events,
            backlog: VecDeque::new(),
            session: 0,
        };
        cli.session = cli
            .wait("connected", |event| {
                (event["kind"] == "connected").then(|| event["session"].as_u64().unwrap())
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

    /// The first event (backlog first) for which `pick` returns something; others
    /// stay in the backlog. Fails on `e2ee_failed`-free timeouts with the backlog.
    async fn wait<T>(&mut self, what: &str, mut pick: impl FnMut(&Value) -> Option<T>) -> T {
        if let Some(index) = self.backlog.iter().position(|event| pick(event).is_some()) {
            let event = self.backlog.remove(index).unwrap();
            return pick(&event).unwrap();
        }
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(event)) => {
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

    async fn quit(mut self) {
        self.send(json!({"kind":"disconnect"})).await;
        let _ = timeout(Duration::from_secs(10), self.child.wait()).await;
    }

    /// Registers this user (first connection, no identity); returns the user id.
    async fn register(server: &Running, name: &str, profile: &Path) -> u64 {
        let mut cli = Self::spawn(server, name, profile, None).await;
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

    /// The conversation view once it is pinned and encrypted.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_devices_exchange_direct_messages_through_nuschld() {
    if !worker_present() {
        panic!("build nuschl-crypto-worker first: cargo build -p nuschl-crypto-worker");
    }
    let state = tempfile::tempdir().unwrap();
    let running = server(state.path(), true).await;
    let directories: Vec<tempfile::TempDir> =
        (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let (alice_profile, alice_identity) = (directories[0].path(), directories[1].path());
    let (bob_profile, bob_identity) = (directories[2].path(), directories[3].path());
    let _alice_id = Cli::register(&running, "alice", alice_profile).await;
    let bob_id = Cli::register(&running, "bob", bob_profile).await;

    let mut bob = Cli::spawn(&running, "bob", bob_profile, Some(bob_identity)).await;
    bob.key_packages_uploaded().await;
    let mut alice = Cli::spawn(&running, "alice", alice_profile, Some(alice_identity)).await;
    alice.key_packages_uploaded().await;

    // Alice opens the conversation: the group is created with Bob's device.
    alice
        .send(json!({"kind":"e2ee_open", "user_id":bob_id}))
        .await;
    let view = alice.established(None).await;
    let conversation = view["conversation"].as_str().unwrap().to_owned();
    let members = view["view"]["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "{view}");
    assert!(
        members.iter().all(|member| member["verified"] == true),
        "{view}"
    );
    // Bob joins from the pushed Welcome.
    bob.established(Some(&conversation)).await;

    let first = alice.sent(&conversation, "hello bob").await;
    let received = bob.received("hello bob").await;
    assert_eq!(received["received"]["seq"], first);
    assert_eq!(received["received"]["order"]["order"], "in_order");
    assert_eq!(received["received"]["sender"]["own"], false);

    bob.sent(&conversation, "hi alice").await;
    alice.received("hi alice").await;

    // Offline delivery: Bob's device is away; the stream keeps the message, and a
    // new worker process (persistence) reads it on reconnect.
    bob.quit().await;
    alice.sent(&conversation, "while you were away").await;
    let mut bob = Cli::spawn(&running, "bob", bob_profile, Some(bob_identity)).await;
    bob.received("while you were away").await;

    // Plaintext into the end-to-end conversation is refused by the server.
    alice
        .send(
            json!({"kind":"chat_send", "request_id":7, "conversation":conversation,
            "content":"in the clear"}),
        )
        .await;
    alice
        .wait("the plaintext refusal", |event| {
            (event["kind"] == "nuschl_error" && event["request_id"] == 7).then_some(())
        })
        .await;

    bob.quit().await;
    alice.quit().await;
}

/// End-to-end encryption is a server-level switch (docs/e2ee.md §10, answer 7): a
/// compatible server offers no MLS, so a device uploads no key packages and creates
/// no groups there, and its direct conversations are plaintext (which the client
/// labels "not end-to-end encrypted").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compatible_server_has_no_end_to_end_encryption() {
    if !worker_present() {
        panic!("build nuschl-crypto-worker first: cargo build -p nuschl-crypto-worker");
    }
    let state = tempfile::tempdir().unwrap();
    let running = server(state.path(), false).await;
    let directories: Vec<tempfile::TempDir> =
        (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let (alice_profile, alice_identity) = (directories[0].path(), directories[1].path());
    let (bob_profile, bob_identity) = (directories[2].path(), directories[3].path());
    let _alice_id = Cli::register(&running, "alice", alice_profile).await;
    let bob_id = Cli::register(&running, "bob", bob_profile).await;

    let bob = Cli::spawn(&running, "bob", bob_profile, Some(bob_identity)).await;
    let mut alice = Cli::spawn(&running, "alice", alice_profile, Some(alice_identity)).await;

    // No end-to-end conversations here...
    alice
        .send(json!({"kind":"e2ee_open", "user_id":bob_id}))
        .await;
    let refused = alice
        .wait("the refusal", |event| {
            (event["kind"] == "e2ee_failed").then(|| event["reason"].clone())
        })
        .await;
    assert!(
        refused.as_str().unwrap().contains("not end-to-end"),
        "{refused}"
    );
    // ...and no key packages, so nobody can put a device into a group.
    for cli in [&alice, &bob] {
        assert!(
            !cli.backlog
                .iter()
                .any(|event| event["kind"] == "e2ee_key_packages"),
            "no key packages on a compatible server"
        );
    }

    // The direct conversation is plaintext and works.
    alice
        .send(json!({"kind":"open_direct", "request_id":11, "user_id":bob_id}))
        .await;
    let conversation = alice
        .wait("the direct conversation", |event| {
            (event["kind"] == "direct_opened" && event["request_id"] == 11)
                .then(|| event["direct"]["conversation"].as_str().map(str::to_owned))
                .flatten()
        })
        .await;
    alice
        .send(
            json!({"kind":"chat_send", "request_id":12, "conversation":conversation,
            "content":"in the clear, as labelled"}),
        )
        .await;
    alice
        .wait("the plaintext acknowledgement", |event| {
            if event["kind"] == "nuschl_error" && event["request_id"] == 12 {
                panic!("plaintext refused: {event}");
            }
            (event["kind"] == "chat_ack" && event["request_id"] == 12).then_some(())
        })
        .await;
    bob.quit().await;
    alice.quit().await;
}
