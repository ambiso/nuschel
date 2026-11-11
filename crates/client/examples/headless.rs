// SPDX-License-Identifier: EUPL-1.2
//! Clients without a user interface, through the SDK alone: three people
//! connect to a Nuschel server, register, and join a channel; Andi sends a
//! message that Ada receives, then edits it and Ada sees the edit; Grace,
//! who connects afterwards, loads the channel's history from the server and
//! finds the edited message. This is what a terminal client does, minus the
//! drawing.
//!
//!     cargo run -p nuschel-client --example headless -- 127.0.0.1 PORT
//!
//! Workers are found beside the executable, as for the desktop; without
//! them messages show as plain text and nothing is kept in local history,
//! which this scenario does not need. Only for a server you run (loopback):
//! it trusts the server's certificate on first use.
use nuschel_client::{
    Client, Update,
    model::TextTarget,
    native::{self, Driver, Event},
    server::Status,
};
use nuschel_session::config::{ClientSettings, Profile, ServerBookmark};
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const TIMEOUT: Duration = Duration::from_secs(30);

/// One person: the SDK's client over the native driver.
struct Person {
    name: &'static str,
    client: Client,
    events: mpsc::Receiver<Event>,
    _driver: Arc<Driver>,
    started: Instant,
}

impl Person {
    fn start(name: &'static str, (host, port): (&str, u16), root: &Path) -> Result<Self, String> {
        let profile = Profile::open(Some(root.join(name))).map_err(|error| error.to_string())?;
        let settings = ClientSettings::default();
        let (driver, events) =
            Driver::new(tokio::runtime::Handle::current(), profile, settings.clone());
        let driver = Arc::new(driver);
        let bookmark = ServerBookmark::new("Local", host, port, name)?;
        let mut client = Client::new(
            Box::new(driver.clone()),
            vec![bookmark],
            native::preferences(&settings),
        );
        // Deafened: no audio devices are opened.
        client.connect(0, None, true);
        Ok(Self {
            name,
            client,
            events,
            _driver: driver,
            started: Instant::now(),
        })
    }

    fn say(&self, what: impl std::fmt::Display) {
        println!(
            "{:>6} {:>5} ms  {what}",
            self.name,
            self.started.elapsed().as_millis()
        );
    }

    /// Apply what the driver reports until `done` holds.
    async fn until(
        &mut self,
        what: &str,
        mut done: impl FnMut(&Client) -> bool,
    ) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while !done(&self.client) {
            let event = tokio::time::timeout_at(deadline, self.events.recv())
                .await
                .map_err(|_| format!("{}: timed out waiting until {what}", self.name))?
                .ok_or("the driver stopped")?;
            self.client
                .set_time(self.started.elapsed().as_millis() as u64);
            if let Ok(input) = event.into_input() {
                self.client.handle(input);
            }
            self.client.settle();
            for update in self.client.take_updates() {
                self.follow(update)?;
            }
            let server = &self.client.servers[0];
            if server.status == Status::Disconnected && server.certificate.is_none() {
                return Err(format!(
                    "{}: disconnected: {}",
                    self.name,
                    server.notice.clone().unwrap_or_default()
                ));
            }
            // A certificate seen for the first time: trust it (a server we run).
            if let Some(prompt) = &self.client.servers[0].certificate {
                self.say(format!("trusts the certificate {}…", &prompt.sha256[..16]));
                self.client.trust_certificate(0, true);
            }
        }
        self.say(what);
        Ok(())
    }

    fn follow(&mut self, update: Update) -> Result<(), String> {
        match update {
            Update::Browse { channel, .. } => {
                self.client.open(0, channel)?;
            }
            Update::HistoryFailed(error) if error.starts_with("Local history") => {}
            Update::HistoryFailed(error) => return Err(format!("{}: {error}", self.name)),
            _ => {}
        }
        Ok(())
    }

    fn connected(&self) -> bool {
        self.client.servers[0].status == Status::Connected
    }

    fn registered(&self) -> bool {
        self.client.servers[0].own_user_id().is_some()
    }

    /// The channel everyone meets in: the first below the root, else the root.
    fn meeting_channel(&self) -> u32 {
        self.client.servers[0]
            .channels
            .iter()
            .filter_map(|channel| match channel.target {
                TextTarget::Channel(id) if id != 0 => Some(id),
                _ => None,
            })
            .min()
            .unwrap_or(0)
    }

    fn position(&self, channel: u32) -> usize {
        self.client.servers[0]
            .channels
            .iter()
            .position(|entry| entry.id == channel)
            .expect("the channel is known")
    }

    /// Connect, register and meet in the channel, with its timeline open.
    async fn arrive(&mut self) -> Result<u32, String> {
        self.until("is connected", |client| {
            client.servers[0].status == Status::Connected
        })
        .await?;
        if !self.registered() {
            self.client
                .request(0, nuschel_client::platform::Request::RegisterSelf)?;
            self.until("is registered", |client| {
                client.servers[0].own_user_id().is_some()
            })
            .await?;
        }
        let channel = self.meeting_channel();
        self.client.join(0, channel);
        self.until("is in the channel", |client| {
            client
                .navigation
                .voice()
                .is_some_and(|voice| voice.channel == channel)
        })
        .await?;
        self.client.open(0, channel)?;
        let position = self.position(channel);
        let archive = self.client.servers[0].has("history/1");
        self.until("has the channel's latest page", |client| {
            let channel = &client.servers[0].channels[position];
            // On a Nuschel server the page comes once the server named the
            // channel's conversation.
            channel.timeline.loaded && (!archive || channel.conversation.is_some())
        })
        .await?;
        let state = &self.client.servers[0].channels[position];
        self.say(format!(
            "reads #{} ({}, {} messages)",
            state.name,
            if archive {
                "server history"
            } else {
                "local history"
            },
            state.timeline.messages.len()
        ));
        Ok(channel)
    }
}

fn texts(client: &Client, position: usize) -> Vec<String> {
    client.servers[0].channels[position]
        .timeline
        .messages
        .iter()
        .map(|message| message.source.clone())
        .collect()
}

async fn scenario(host: &str, port: u16, root: &Path) -> Result<(), String> {
    let mut ada = Person::start("Ada", (host, port), root)?;
    let mut andi = Person::start("Andi", (host, port), root)?;
    let channel = ada.arrive().await?;
    andi.arrive().await?;
    assert!(ada.connected() && andi.connected());
    let (at_ada, at_andi) = (ada.position(channel), andi.position(channel));

    // Send, and receive.
    let hello = "Hello from the SDK";
    andi.client
        .send_text(hello.into())
        .map_err(|error| format!("not sent: {error:?}"))?;
    andi.until("sent it", |client| !client.sending()).await?;
    ada.until("received it", |client| {
        texts(client, at_ada).iter().any(|text| text == hello)
    })
    .await?;

    // Edit our own message once the server stored it (it has its seq).
    andi.until("has it acknowledged", |client| {
        client.servers[0].channels[at_andi]
            .timeline
            .messages
            .iter()
            .any(|message| message.source == hello && message.seq.is_some())
    })
    .await?;
    let seq = andi.client.servers[0].channels[at_andi]
        .timeline
        .messages
        .iter()
        .find(|message| message.source == hello)
        .and_then(|message| message.seq)
        .ok_or("no seq")?;
    let edited = "Hello from the SDK, edited";
    andi.client.edit(0, at_andi, seq, edited.into())?;
    andi.until("edited it", |client| {
        texts(client, at_andi).iter().any(|text| text == edited)
    })
    .await?;
    ada.until("sees the edit", |client| {
        client.servers[0].channels[at_ada]
            .timeline
            .messages
            .iter()
            .any(|message| message.source == edited && message.edited_ms.is_some())
    })
    .await?;

    // Someone who was not there reads it from the server's history.
    let mut grace = Person::start("Grace", (host, port), root)?;
    grace.arrive().await?;
    let at_grace = grace.position(channel);
    grace
        .until("loaded the history with the edit", |client| {
            texts(client, at_grace).iter().any(|text| text == edited)
        })
        .await?;
    let history = grace.client.servers[0].channels[at_grace].timeline.clone();
    println!(
        "Grace's page: {} messages, {} older page{}",
        history.messages.len(),
        if history.next_before.is_some() {
            "an"
        } else {
            "no"
        },
        if history.next_before.is_some() {
            ""
        } else {
            "s"
        },
    );
    Ok(())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "127.0.0.1".into());
    let port = args
        .next()
        .and_then(|port| port.parse().ok())
        .unwrap_or(64738);
    let root = std::env::temp_dir().join(format!("nuschel-headless-{}", std::process::id()));
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let result = runtime.block_on(scenario(&host, port, &root));
    let _ = std::fs::remove_dir_all(&root);
    match result {
        Ok(()) => println!("Headless clients through the SDK: ok"),
        Err(error) => {
            eprintln!("Headless clients through the SDK failed: {error}");
            std::process::exit(1);
        }
    }
}
