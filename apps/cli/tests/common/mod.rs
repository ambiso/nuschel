// SPDX-License-Identifier: EUPL-1.2
//! `nuschel-cli` processes against an in-process `nuscheld` on loopback, for the
//! end-to-end tests (each test binary uses what it needs).
#![allow(dead_code)]
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

pub const WAIT: Duration = Duration::from_secs(60);

pub fn cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nuschel-cli"))
}

pub fn worker_present() -> bool {
    cli()
        .with_file_name(format!(
            "nuschel-crypto-worker{}",
            std::env::consts::EXE_SUFFIX
        ))
        .exists()
}

/// An end-to-end server whose clock-driven removals run only when a test asks
/// (`State::mls_sweep`); access changes are acted on at once.
pub async fn server(directory: &Path) -> Running {
    let config = Config::from_toml(&format!(
        "security = \"e2ee\"\nbind = [\"127.0.0.1\"]\nport = 0\nstate_directory = {directory:?}\n\
         message_limit = 1000\nmessage_burst = 1000\n\
         [autoban]\nattempts = 0\n[password_hashing]\nmemory_kib = 64\npasses = 1\n\
         [kt]\npublish_interval = 1\n[mls]\nsweep_interval = 86400\n\
         uploads_per_minute = 600\nclaims_per_minute = 600\n",
    ))
    .unwrap();
    start(
        config.settings().unwrap(),
        Backend::Memory(MemoryStore::shared()),
    )
    .await
    .unwrap()
}

pub struct Cli {
    pub name: String,
    child: Child,
    input: ChildStdin,
    events: mpsc::UnboundedReceiver<Value>,
    pub backlog: VecDeque<Value>,
    pub session: u64,
}

impl Cli {
    pub fn command(server: &Running, name: &str, profile: &Path) -> Command {
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

    pub fn start(name: &str, mut command: Command) -> Self {
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

    pub async fn spawn(
        server: &Running,
        name: &str,
        profile: &Path,
        identity: Option<&Path>,
        extra: &[&std::ffi::OsStr],
    ) -> Self {
        let mut command = Self::command(server, name, profile);
        if let Some(identity) = identity {
            command.arg("--identity-dir").arg(identity);
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
    pub async fn offline(server: &Running, name: &str, profile: &Path, identity: &Path) -> Self {
        let mut command = Self::command(server, name, profile);
        command.arg("--identity-dir").arg(identity).arg("--offline");
        let mut cli = Self::start(name, command);
        cli.wait("offline", |event| {
            (event["kind"] == "offline").then_some(())
        })
        .await;
        cli
    }

    pub async fn send(&mut self, command: Value) {
        let mut line = serde_json::to_vec(&command).unwrap();
        line.push(b'\n');
        self.input.write_all(&line).await.unwrap();
        self.input.flush().await.unwrap();
    }

    pub async fn wait<T>(&mut self, what: &str, mut pick: impl FnMut(&Value) -> Option<T>) -> T {
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
    pub fn assert_no_alarm(&self) {
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

    pub async fn quit(mut self) {
        self.assert_no_alarm();
        self.send(json!({"kind":"disconnect"})).await;
        let _ = timeout(Duration::from_secs(10), self.child.wait()).await;
    }

    pub async fn register(server: &Running, name: &str, profile: &Path) -> u64 {
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

    pub async fn key_packages_uploaded(&mut self) {
        self.wait("key packages", |event| {
            (event["kind"] == "e2ee_key_packages" && event["one_time"].as_u64() >= Some(4))
                .then_some(())
        })
        .await;
    }

    pub async fn established(&mut self, conversation: Option<&str>) -> Value {
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

    pub async fn received(&mut self, text: &str) -> Value {
        self.wait(&format!("the message {text:?}"), |event| {
            (event["kind"] == "e2ee_received"
                && event["received"]["kind"] == "message"
                && event["received"]["text"] == text)
                .then(|| event.clone())
        })
        .await
    }

    pub async fn sent(&mut self, conversation: &str, text: &str) -> u64 {
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
pub async fn link(server: &Running, profile: &Path, identity: &Path, new_identity: &Path) -> Value {
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
