// SPDX-License-Identifier: EUPL-1.2
//! The desktop sees typed events; transport, parsing, audio and history run off its thread.
use nuschel_client::HistoryTicket;
use nuschel_core::audio::{AudioDevice, EchoSource, Sound, TransmitMode};
use nuschel_core::{ChatMessage, ContentBlock, HistoryPage, MessageDocument};
use nuschel_ipc::{LiveMediaEvent, LiveMediaRequest};
use nuschel_protocol::nuschel::v1;
use nuschel_session::{
    Error,
    config::{ClientSettings, Profile, ServerBookmark},
    connection::{
        self, Command, ConnectOptions, IncomingText, NuschelEvent, NuschelRequest, PendingChat,
        SessionEvent, TextTarget,
    },
    model::RejectKind,
    workers::{self, Media, Storage},
};
use std::{collections::BTreeMap, path::PathBuf};
use tokio::{
    runtime::Handle,
    sync::{mpsc, watch},
};

pub struct RenderedMessage {
    /// The local row id, or on a Nuschel server the server's seq.
    pub id: Option<i64>,
    pub message: ChatMessage,
    pub document: MessageDocument,
    pub outgoing: bool,
    /// Our message on a Nuschel server until it is acknowledged: its key.
    pub pending: Option<[u8; 16]>,
}

/// Server history pages asked for, by request id: server, ticket and cursor.
type ServerPages = BTreeMap<u64, (String, HistoryTicket, Option<i64>)>;
static SERVER_PAGES: std::sync::Mutex<ServerPages> = std::sync::Mutex::new(BTreeMap::new());

fn server_pages() -> std::sync::MutexGuard<'static, ServerPages> {
    SERVER_PAGES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Render a Nuschel server's history page like a local one: text in one
/// content worker, stock clients' HTML made inert first. Ids are seqs; edits,
/// deletions and voids only move the cursor.
async fn render_server_page(page: v1::HistoryPage) -> (Vec<RenderedMessage>, Option<i64>) {
    let next_before = page
        .more_before
        .then(|| page.messages.first().map(|message| message.seq as i64))
        .flatten();
    let shown: Vec<v1::Message> = page
        .messages
        .into_iter()
        .filter(|message| message.kind == v1::message::Kind::Message as i32)
        .collect();
    let mut out = Vec::with_capacity(shown.len());
    for message in shown {
        let author = message.author.clone().unwrap_or_default();
        let author = if author.name.is_empty() && author.account.is_empty() {
            "Unknown".to_owned()
        } else if author.name.is_empty() {
            "Deleted user".to_owned()
        } else {
            author.name
        };
        let (source, document) = if message.deleted_ms != 0 {
            let text = "This message was deleted.".to_owned();
            (text.clone(), plain(&text))
        } else {
            prepare(
                &message.content,
                message.format == v1::Format::LegacyHtml as i32,
            )
            .await
        };
        out.push(RenderedMessage {
            id: Some(message.seq as i64),
            message: ChatMessage {
                server: String::new(),
                channel: String::new(),
                author,
                observed_at_ms: message.sent_ms,
                body: source,
            },
            document,
            outgoing: false,
            pending: None,
        });
    }
    (out, next_before)
}
pub enum Event {
    /// The crypto worker's answer to [`Backend::identity`].
    OwnIdentity(Result<nuschel_ipc::crypto::CryptoResponse, String>),
    Protocol {
        server: String,
        generation: u64,
        event: SessionEvent,
    },
    Certificate {
        server: String,
        generation: u64,
        sha256: String,
        changed: bool,
        reason: String,
    },
    Disconnected {
        server: String,
        generation: u64,
        reason: String,
        /// Set when the server refused the login, so the UI can ask for a password or name.
        reject: Option<RejectKind>,
    },
    /// The connection dropped; a new attempt starts after `delay_secs`.
    Reconnecting {
        server: String,
        generation: u64,
        attempt: u32,
        delay_secs: u64,
        reason: String,
    },
    /// We were not connected between these times; history may be missing messages.
    Gap {
        server: String,
        generation: u64,
        from_ms: i64,
        to_ms: i64,
    },
    AudioStatus {
        server: String,
        generation: u64,
        message: String,
    },
    Message {
        server: String,
        generation: u64,
        target: TextTarget,
        storage_key: String,
        peer: Option<connection::User>,
        message: RenderedMessage,
    },
    History {
        server: String,
        ticket: HistoryTicket,
        /// The cursor this page was asked for: None is the latest page.
        before: Option<i64>,
        messages: Vec<RenderedMessage>,
        next_before: Option<i64>,
    },
    Sent {
        server: String,
        generation: u64,
        target: TextTarget,
        body: String,
    },
    /// A page read around or after a message (`Page::Around`, `Page::After`).
    HistoryWindow {
        server: String,
        ticket: HistoryTicket,
        page: Page,
        messages: Vec<RenderedMessage>,
        next_before: Option<i64>,
        next_after: Option<i64>,
    },
    HistoryError(String),
    /// A server history page could not be loaded.
    HistoryFailed {
        server: String,
        ticket: HistoryTicket,
        reason: String,
    },
    /// The server offers less than it did before (the sticky policy).
    Downgrade {
        server: String,
        generation: u64,
        reason: String,
    },
    /// Results for search `request` (see `Backend::search`).
    SearchResults {
        request: u64,
        result: Result<nuschel_core::SearchPage, String>,
    },
    /// History writes work again after an earlier failure.
    HistoryRecovered,
    AudioDevices {
        server: String,
        generation: u64,
        inputs: Vec<AudioDevice>,
        outputs: Vec<AudioDevice>,
        input: Option<String>,
        output: Option<String>,
        echo: EchoSource,
        noise: Option<String>,
        reference: Option<String>,
    },
    /// The local audio check in Settings.
    Preview(crate::preview::Event),
    Update(crate::update::UpdateEvent),
    /// The image viewer's larger decode (`None` if the worker refused it).
    /// A server's website icon, decoded by the content worker (`None`: none found).
    ServerIcon {
        server: String,
        icon: Option<nuschel_core::ContentBlock>,
    },
    Viewed {
        request: u64,
        document: Option<MessageDocument>,
    },
    Echo {
        server: String,
        generation: u64,
        erle_db: Option<f32>,
        residual_likelihood: Option<f32>,
    },
    /// The media worker's adaptive controller changed its view of the graph.
    AudioHealth {
        server: String,
        generation: u64,
        health: crate::state::AudioHealth,
    },
    /// Inert rendering of untrusted Mumble HTML (descriptions, comments).
    Rendered {
        server: String,
        generation: u64,
        key: RenderKey,
        source: String,
        document: MessageDocument,
    },
    /// An attached image re-encoded for sending, or why it cannot be sent.
    Prepared {
        id: u64,
        result: Result<nuschel_ipc::EncodedImage, String>,
    },
    /// A decoded avatar thumbnail for a texture hash.
    Avatar {
        server: String,
        generation: u64,
        hash: String,
        document: MessageDocument,
    },
    /// Voice activity for a session (including our own) on one connection generation.
    Talking {
        server: String,
        generation: u64,
        session: u32,
        talking: bool,
    },
}
/// Which page of a conversation's local history to load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    /// Older than this message; None is the latest page.
    Before(Option<i64>),
    /// Newer than this message.
    After(i64),
    /// Around this message, which a search result jumps to.
    Around(i64),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderKey {
    Description(u32),
    Comment(u32),
}
struct Connection {
    commands: mpsc::Sender<Command>,
    sounds: mpsc::Sender<Sound>,
    task: tokio::task::JoinHandle<()>,
}
pub struct Backend {
    handle: Handle,
    profile: Profile,
    /// The crypto worker, shared by every connection (identity and key transparency).
    crypto: nuschel_session::identity::CryptoHandle,
    sender: mpsc::Sender<Event>,
    storage: mpsc::Sender<StorageCommand>,
    content: mpsc::Sender<ContentJob>,
    connections: BTreeMap<String, Connection>,
    generation: u64,
    settings: watch::Sender<ClientSettings>,
    voice_keys: watch::Sender<VoiceKeys>,
    render_permits: std::sync::Arc<tokio::sync::Semaphore>,
    image_permits: std::sync::Arc<tokio::sync::Semaphore>,
    storage_task: tokio::task::JoinHandle<()>,
    content_task: tokio::task::JoinHandle<()>,
    preview: Option<(
        mpsc::Sender<crate::preview::Command>,
        tokio::task::JoinHandle<()>,
    )>,
}
struct ContentJob {
    server: String,
    generation: u64,
    identity: String,
    nonce: String,
    text: IncomingText,
}
enum StorageCommand {
    Append {
        job: Box<ContentJob>,
        source: String,
        document: MessageDocument,
    },
    Load {
        server: String,
        identity: String,
        channel: String,
        ticket: HistoryTicket,
        page: Page,
    },
    Search {
        request: u64,
        scope: (Vec<String>, Option<String>),
        query: String,
        after: Option<nuschel_core::SearchCursor>,
    },
}
impl Backend {
    /// The backend, and the events it reports for the desktop to apply.
    pub fn new(
        handle: Handle,
        profile: Profile,
        settings: ClientSettings,
    ) -> (Self, mpsc::Receiver<Event>) {
        let (sender, events) = mpsc::channel(512);
        let (storage, storage_rx) = mpsc::channel(128);
        let (content, content_rx) = mpsc::channel(64);
        let storage_task = handle.spawn(storage_loop(
            profile.data_dir.clone(),
            storage_rx,
            sender.clone(),
        ));
        let content_task = handle.spawn(content_loop(content_rx, storage.clone(), sender.clone()));
        // Started on first use, in its sandbox, on its own directory beside the
        // configuration (docs/e2ee.md §3.7).
        let crypto = {
            let _runtime = handle.enter();
            nuschel_session::identity::CryptoHandle::start(
                nuschel_session::workers::sibling("nuschel-crypto-worker")
                    .unwrap_or_else(|_| "nuschel-crypto-worker".into()),
                profile.config_dir.join("nuschel-identity"),
            )
        };
        let backend = Self {
            handle,
            crypto,
            profile,
            sender,
            storage,
            content,
            connections: BTreeMap::new(),
            generation: 0,
            settings: watch::Sender::new(settings),
            voice_keys: watch::Sender::new(VoiceKeys::default()),
            render_permits: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
            image_permits: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
            storage_task,
            content_task,
            preview: None,
        };
        (backend, events)
    }
    /// Start the Settings audio check (devices only until the microphone test starts).
    pub fn start_preview(&mut self) {
        if self.preview.is_some() {
            return;
        }
        let (commands, receiver) = mpsc::channel(8);
        let task = self.handle.spawn(crate::preview::run(
            receiver,
            self.settings.subscribe(),
            self.sender.clone(),
        ));
        self.preview = Some((commands, task));
    }
    pub fn preview(&self, command: crate::preview::Command) {
        if let Some((commands, _)) = &self.preview {
            let _ = commands.try_send(command);
        }
    }
    /// Closing the sender lets the task shut its worker down cleanly.
    pub fn stop_preview(&mut self) {
        self.preview.take();
    }
    /// Decode an inline image at the viewer's size in the sandboxed content worker.
    /// Look for the server website's icon (see `icons`).
    pub fn fetch_icon(&self, server: String, host: String) {
        let events = self.sender.clone();
        self.handle.spawn(async move {
            let bytes = tokio::task::spawn_blocking(move || crate::icons::fetch(&host))
                .await
                .ok()
                .flatten();
            let icon = match (bytes, content_worker()) {
                (Some(bytes), Some(worker)) => nuschel_ipc::host::decode_icon(worker, &bytes)
                    .await
                    .ok()
                    .and_then(|document| document.blocks.into_iter().next()),
                _ => None,
            };
            let _ = events.send(Event::ServerIcon { server, icon }).await;
        });
    }
    pub fn decode_view(&self, request: u64, uri: String) {
        let events = self.sender.clone();
        self.handle.spawn(async move {
            let document = match content_worker() {
                Some(worker) => nuschel_ipc::host::decode_view(worker, &uri).await.ok(),
                None => None,
            };
            let _ = events.send(Event::Viewed { request, document }).await;
        });
    }
    /// Ask the release server for a newer version now.
    pub fn check_update(&self) {
        let events = self.sender.clone();
        self.handle.spawn(async move {
            let result = tokio::task::spawn_blocking(crate::update::check)
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
            let _ = events
                .send(Event::Update(crate::update::UpdateEvent::Checked(result)))
                .await;
        });
    }
    /// Check shortly after start and then periodically, while the setting is on.
    pub fn schedule_update_checks(&self) {
        let events = self.sender.clone();
        let settings = self.settings.subscribe();
        self.handle.spawn(async move {
            let mut delay = crate::update::FIRST_CHECK;
            loop {
                tokio::time::sleep(delay).await;
                delay = crate::update::INTERVAL;
                if !settings.borrow().check_updates {
                    continue;
                }
                let result = tokio::task::spawn_blocking(crate::update::check)
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()));
                if events
                    .send(Event::Update(crate::update::UpdateEvent::Checked(result)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    /// Download, verify and swap in an update.
    pub fn install_update(&self, update: crate::update::Update) {
        let events = self.sender.clone();
        self.handle.spawn(async move {
            let progress = events.clone();
            let result = tokio::task::spawn_blocking(move || {
                let mut last = 0;
                crate::update::install(&update, |done, total| {
                    // About every 5% (try_send: progress may be dropped).
                    if done == total || done.saturating_sub(last) > total / 20 {
                        last = done;
                        let _ = progress.try_send(Event::Update(
                            crate::update::UpdateEvent::Progress { done, total },
                        ));
                    }
                })
            })
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
            let _ = events
                .send(Event::Update(crate::update::UpdateEvent::Installed(result)))
                .await;
        });
    }
    /// Connect self-muted; `deafened` also deafens (automatic connections at startup).
    pub fn connect(
        &mut self,
        server: ServerBookmark,
        password: Option<String>,
        deafened: bool,
    ) -> Result<u64, String> {
        if self.connections.len() >= 8 && !self.connections.contains_key(&server.id) {
            return Err("Disconnect a server before opening another connection (limit: 8).".into());
        }
        if let Some(old) = self.connections.remove(&server.id) {
            old.task.abort();
        }
        self.generation += 1;
        let generation = self.generation;
        let (commands, receiver) = mpsc::channel(32);
        let (sounds, sound_requests) = mpsc::channel(8);
        let id = server.id.clone();
        let task = self.handle.spawn(connection_loop(
            (self.profile.clone(), self.crypto.clone()),
            server,
            (password, deafened),
            generation,
            (receiver, sound_requests),
            self.sender.clone(),
            self.content.clone(),
            (self.settings.subscribe(), self.voice_keys.subscribe()),
        ));
        self.connections.insert(
            id,
            Connection {
                commands,
                sounds,
                task,
            },
        );
        Ok(generation)
    }
    /// A page of a Nuschel server's history (`before` a seq, or the latest).
    pub fn server_history(
        &self,
        server: String,
        conversation: [u8; 16],
        ticket: HistoryTicket,
        before: Option<i64>,
    ) -> Result<(), String> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let request_id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let anchor = match before {
            Some(seq) => v1::history_request::Anchor::Before(seq.max(0) as u64),
            None => v1::history_request::Anchor::Latest(true),
        };
        {
            let mut pages = server_pages();
            // Answers that never came (a dropped connection) do not pile up.
            while pages.len() >= 64 {
                pages.pop_first();
            }
            pages.insert(request_id, (server.clone(), ticket, before));
        }
        let result = self.command(
            &server,
            Command::Nuschel(NuschelRequest::History(v1::HistoryRequest {
                request_id,
                conversation: conversation.to_vec(),
                anchor: Some(anchor),
                limit: 50,
            })),
        );
        if result.is_err() {
            server_pages().remove(&request_id);
        }
        result
    }
    /// A request to the crypto worker that concerns no server (Settings → Identity).
    pub fn identity(&self, request: nuschel_ipc::crypto::CryptoRequest) {
        let crypto = self.crypto.clone();
        let events = self.sender.clone();
        self.handle.spawn(async move {
            let result = crypto
                .request(request)
                .await
                .map_err(|error| error.to_string());
            let _ = events.send(Event::OwnIdentity(result)).await;
        });
    }
    pub fn command(&self, server: &str, command: Command) -> Result<(), String> {
        self.connections
            .get(server)
            .ok_or("Server is not connected.")?
            .commands
            .try_send(command)
            .map_err(|_| "Connection is busy or has ended; reconnect to try again.".into())
    }
    /// Render untrusted Mumble HTML in the sandboxed content worker.
    pub fn render_html(&self, server: String, generation: u64, key: RenderKey, source: String) {
        let events = self.sender.clone();
        let permits = self.render_permits.clone();
        self.handle.spawn(async move {
            let Ok(_permit) = permits.acquire().await else {
                return;
            };
            let (_, document) = prepare(&source, true).await;
            let _ = events
                .send(Event::Rendered {
                    server,
                    generation,
                    key,
                    source,
                    document,
                })
                .await;
        });
    }
    /// Re-encode an attached image as stock Mumble sends one: in the
    /// sandboxed content worker, into an `<img>` of at most `limit` bytes.
    /// Two at a time; a file is read only when its turn comes.
    pub fn prepare_image(&self, id: u64, source: crate::attachments::Source, limit: usize) {
        let events = self.sender.clone();
        let permits = self.image_permits.clone();
        self.handle.spawn(async move {
            let result = async {
                let _permit = permits.acquire().await.map_err(|error| error.to_string())?;
                let worker =
                    content_worker().ok_or("images need the content worker, which is missing")?;
                let image = match source {
                    crate::attachments::Source::Bytes(bytes) => bytes,
                    crate::attachments::Source::File(path) => {
                        tokio::task::spawn_blocking(move || crate::attachments::read(&path))
                            .await
                            .map_err(|error| error.to_string())??
                    }
                };
                if image.len() as u64 > crate::attachments::MAX_IMAGE_BYTES {
                    return Err("it is larger than 8 MB".to_owned());
                }
                match nuschel_ipc::host::encode_image(worker, &image, limit as u32).await {
                    Ok(encoded) => Ok(encoded),
                    Err(nuschel_ipc::host::Error::Rejected(reason)) => Err(reason),
                    Err(error) => Err(error.to_string()),
                }
            }
            .await;
            let _ = events.send(Event::Prepared { id, result }).await;
        });
    }
    /// Search the history of these server identities, or of one
    /// conversation (its storage key) of a single one; results come back
    /// as `Event::SearchResults` for `request`.
    pub fn search(
        &self,
        request: u64,
        scope: (Vec<String>, Option<String>),
        query: String,
        after: Option<nuschel_core::SearchCursor>,
    ) -> Result<(), String> {
        self.storage
            .try_send(StorageCommand::Search {
                request,
                scope,
                query,
                after,
            })
            .map_err(|_| "History is busy; try again shortly.".into())
    }
    pub fn runtime(&self) -> &Handle {
        &self.handle
    }
    /// Decode an avatar image in the sandboxed content worker.
    pub fn decode_avatar(&self, server: String, generation: u64, hash: String, encoded: Vec<u8>) {
        let events = self.sender.clone();
        let permits = self.render_permits.clone();
        self.handle.spawn(async move {
            let Ok(_permit) = permits.acquire().await else {
                return;
            };
            let Some(worker) = content_worker() else {
                return;
            };
            if let Ok(document) = nuschel_ipc::host::decode_avatar(worker, &encoded).await {
                let _ = events
                    .send(Event::Avatar {
                        server,
                        generation,
                        hash,
                        document,
                    })
                    .await;
            }
        });
    }
    /// Apply new preferences to every live connection and its media worker.
    pub fn apply_settings(&self, settings: ClientSettings) {
        self.settings.send_replace(settings);
    }
    /// Play a sound effect on this server's media worker (see `connect_once`).
    pub fn play_sound(&self, server: &str, sound: Sound) {
        if let Some(connection) = self.connections.get(server) {
            let _ = connection.sounds.try_send(sound);
        }
    }
    /// Play a sound effect where no connection's worker can, as after a disconnect.
    pub fn chime(&self, sound: Sound) {
        let settings = self.settings.borrow();
        if settings.sound_effects {
            let audio = settings.audio.clone();
            self.handle
                .spawn(chime(audio, settings.sound_volume, None, sound));
        }
    }
    pub fn set_push_to_talk(&self, active: bool) {
        self.voice_keys
            .send_if_modified(|keys| std::mem::replace(&mut keys.talk, active) != active);
    }
    pub fn set_push_to_mute(&self, active: bool) {
        self.voice_keys
            .send_if_modified(|keys| std::mem::replace(&mut keys.mute, active) != active);
    }
    pub fn disconnect(&mut self, server: &str) {
        if let Some(connection) = self.connections.remove(server) {
            connection.task.abort();
        }
    }
    pub fn history(
        &self,
        server: String,
        identity: String,
        channel: String,
        ticket: HistoryTicket,
        before: Option<i64>,
    ) -> Result<(), String> {
        self.history_page(server, identity, channel, ticket, Page::Before(before))
    }
    pub fn history_page(
        &self,
        server: String,
        identity: String,
        channel: String,
        ticket: HistoryTicket,
        page: Page,
    ) -> Result<(), String> {
        self.storage
            .try_send(StorageCommand::Load {
                server,
                identity,
                channel,
                ticket,
                page,
            })
            .map_err(|_| "History is busy; try again shortly.".into())
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        for connection in self.connections.values() {
            connection.task.abort();
        }
        self.storage_task.abort();
        self.content_task.abort();
    }
}

/// What to restore when a dropped connection is re-established.
#[derive(Default)]
struct Restore {
    channel: Option<u32>,
    muted: bool,
    deafened: bool,
    connected: bool,
    disconnected_at_ms: Option<i64>,
    /// Our messages a Nuschel server has not acknowledged yet; sent again
    /// with their keys after a reconnect.
    unacked: Vec<PendingChat>,
}

enum Outcome {
    Ended(Result<Result<(), Error>, tokio::task::JoinError>),
    /// The desktop dropped this connection.
    Closed,
}

/// Supervise one server connection: reconnect with backoff after transient
/// failures, restoring channel and mute/deafen, and report gaps in history.
#[allow(clippy::too_many_arguments)]
async fn connection_loop(
    (profile, crypto): (Profile, nuschel_session::identity::CryptoHandle),
    server: ServerBookmark,
    (password, deafened): (Option<String>, bool),
    generation: u64,
    (mut commands, mut sounds): (mpsc::Receiver<Command>, mpsc::Receiver<Sound>),
    events: mpsc::Sender<Event>,
    content: mpsc::Sender<ContentJob>,
    (mut settings, mut voice_keys): (watch::Receiver<ClientSettings>, watch::Receiver<VoiceKeys>),
) {
    let id = server.id.clone();
    // A fresh connection rejoins the channel you were last in, like a reconnect.
    let mut restore = Restore {
        channel: server.last_channel,
        muted: true,
        deafened,
        ..Restore::default()
    };
    let mut attempt = 0_u32;
    loop {
        let started = tokio::time::Instant::now();
        let outcome = connect_once(
            (&profile, &crypto),
            &server,
            password.clone(),
            generation,
            (&mut commands, &mut sounds),
            &events,
            &content,
            (&mut settings, &mut voice_keys),
            &mut restore,
        )
        .await;
        let result = match outcome {
            Outcome::Closed => return,
            Outcome::Ended(result) => result,
        };
        if started.elapsed() > std::time::Duration::from_secs(30) {
            attempt = 0;
        }
        let error = match result {
            Ok(Ok(())) => {
                let _ = events
                    .send(Event::Disconnected {
                        server: id.clone(),
                        generation,
                        reason: "Disconnected".into(),
                        reject: None,
                    })
                    .await;
                return;
            }
            Ok(Err(Error::Downgrade(reason))) => {
                let _ = events
                    .send(Event::Downgrade {
                        server: id.clone(),
                        generation,
                        reason,
                    })
                    .await;
                return;
            }
            Ok(Err(Error::Certificate {
                sha256,
                changed,
                reason,
            })) => {
                let _ = events
                    .send(Event::Certificate {
                        server: id.clone(),
                        generation,
                        sha256,
                        changed,
                        reason,
                    })
                    .await;
                return;
            }
            Ok(Err(error)) => error,
            Err(error) => {
                let _ = events
                    .send(Event::Disconnected {
                        server: id.clone(),
                        generation,
                        reason: format!("Connection task failed: {error}"),
                        reject: None,
                    })
                    .await;
                return;
            }
        };
        let reject = match &error {
            Error::Rejected { kind, .. } => Some(*kind),
            _ => None,
        };
        if !(error.transient() && restore.connected && settings.borrow().auto_reconnect) {
            let _ = events
                .send(Event::Disconnected {
                    server: id.clone(),
                    generation,
                    reason: error.to_string(),
                    reject,
                })
                .await;
            return;
        }
        restore
            .disconnected_at_ms
            .get_or_insert_with(connection::now_ms);
        attempt += 1;
        let delay = [1, 2, 4, 8, 15, 30][(attempt as usize - 1).min(5)];
        let _ = events
            .send(Event::Reconnecting {
                server: id.clone(),
                generation,
                attempt,
                delay_secs: delay,
                reason: error.to_string(),
            })
            .await;
        let wait = tokio::time::sleep(std::time::Duration::from_secs(delay));
        tokio::pin!(wait);
        loop {
            tokio::select! {
                _ = &mut wait => break,
                command = commands.recv() => match command {
                    None | Some(Command::Disconnect) => return,
                    // Commands made while offline are dropped; the UI shows the state.
                    Some(_) => {}
                },
            }
        }
    }
}

/// This device's name in its identity certificate: the host's name when known.
fn device_name() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().chars().take(48).collect::<String>())
        .filter(|name| !name.is_empty() && !name.chars().any(char::is_control));
    match host {
        Some(host) => format!("Nuschel on {host}"),
        None => format!("Nuschel ({})", std::env::consts::OS),
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_once(
    (profile, crypto): (&Profile, &nuschel_session::identity::CryptoHandle),
    server: &ServerBookmark,
    password: Option<String>,
    generation: u64,
    (commands, sounds): (&mut mpsc::Receiver<Command>, &mut mpsc::Receiver<Sound>),
    events: &mpsc::Sender<Event>,
    content: &mpsc::Sender<ContentJob>,
    (settings, voice_keys): (
        &mut watch::Receiver<ClientSettings>,
        &mut watch::Receiver<VoiceKeys>,
    ),
    restore: &mut Restore,
) -> Outcome {
    let id = server.id.clone();
    // The sticky policy: an unreadable record fails closed, never empty.
    let required = nuschel_session::sticky::PolicyStore::load(&profile.config_dir)
        .map(|store| store.get(&server.host, server.port))
        .map_err(|error| format!("Nuschel's server security records could not be read: {error}"));
    let options = match profile
        .identity()
        .map_err(|error| error.to_string())
        .and_then(|identity| Ok((identity, required?)))
    {
        Ok((identity, required)) => ConnectOptions {
            server: server.clone(),
            identity,
            password,
            force_tcp: settings.borrow().force_tcp,
            muted: restore.muted,
            deafened: restore.deafened,
            channel: restore.channel,
            resend: restore.unacked.clone(),
            message_rate: Default::default(),
            required,
            crypto: Some(nuschel_session::identity::IdentitySetup {
                crypto: crypto.clone(),
                device_name: device_name(),
                recovery: settings.borrow().recovery_key,
            }),
        },
        Err(error) => {
            let _ = events
                .send(Event::Disconnected {
                    server: id,
                    generation,
                    reason: error,
                    reject: None,
                })
                .await;
            return Outcome::Closed;
        }
    };
    let (session_commands, receiver) = mpsc::channel(32);
    let (session_events, mut incoming) = mpsc::channel(128);
    let mut session = tokio::spawn(connection::run(options, receiver, session_events));
    let mut media: Option<Media> = None;
    let mut peer = None;
    let nonce = format!("{}:{generation}:{}", id, connection::now_ms());
    let mut muted = restore.muted;
    let mut deafened = restore.deafened;
    let mut sequence = 0;
    let mut content_gap = false;
    let mut capture_epoch = 0;
    let mut audio_enabled = false;
    let mut bitrate = 48_000;
    let mut retry_audio = tokio::time::Instant::now();
    // The audio server that last worked, pinned for a while after a failure (see pinned_host).
    let mut audio_host: (Option<String>, tokio::time::Instant) =
        (None, tokio::time::Instant::now());
    let mut finished = false;
    let mut outcome = None;
    let mut own_session = None;
    let mut hashes = BTreeMap::<u32, Option<String>>::new();
    let mut applied_audio = settings.borrow().audio.clone();
    let mut loss_percent = 5_u8;
    // Our own voice is on the air (no terminator sent yet).
    let mut transmitting = false;
    let mut talk_key = voice_keys.borrow().talk;
    let mut speaking = BTreeMap::<u32, tokio::time::Instant>::new();
    let mut talk_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    talk_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Sound effects for the next worker while this one is being replaced.
    let mut pending = Vec::<(Sound, tokio::time::Instant)>::new();
    // Requests from before a reconnection are stale.
    while sounds.try_recv().is_ok() {}
    loop {
        tokio::select! {
            changed = settings.changed() => {
                if changed.is_err() { break; }
                let audio = settings.borrow_and_update().audio.clone();
                if !audio.same_route(&applied_audio) {
                    // New devices need new native streams: replace the worker, as for mute.
                    media.take();
                    capture_epoch += 1;
                    retry_audio = tokio::time::Instant::now();
                } else if audio != applied_audio && let Some(media) = &media {
                    configure(media, &audio, (bitrate, loss_percent), pinned_host(&audio_host));
                }
                applied_audio = audio;
            },
            changed = voice_keys.changed() => {
                if changed.is_err() { break; }
                let keys = *voice_keys.borrow_and_update();
                // The talk key's own changes reach the worker, with their sound.
                if std::mem::replace(&mut talk_key, keys.talk) != keys.talk && let Some(media) = &media {
                    let _ = media.commands.try_send(LiveMediaRequest::PushToTalk { active: keys.talk });
                    let current = settings.borrow();
                    if !muted && current.audio.transmit == TransmitMode::PushToTalk {
                        play(media, &current, if keys.talk { Sound::TalkOn } else { Sound::TalkOff });
                    }
                }
                // Push to mute holds back what the worker sends, without replacing it.
                if keys.mute && std::mem::take(&mut transmitting) {
                    let _ = session_commands.try_send(silent_terminator(&mut sequence));
                    if let Some(session) = own_session { mark_talking(events, &id, generation, &mut speaking, session, false).await; }
                }
            },
            _ = talk_tick.tick(), if !speaking.is_empty() => {
                let now = tokio::time::Instant::now();
                let silent = speaking.iter().filter(|(_, last)| now.duration_since(**last) > TALK_HOLD).map(|(session, _)| *session).collect::<Vec<_>>();
                for session in silent {
                    speaking.remove(&session);
                    let _ = events.send(Event::Talking { server: id.clone(), generation, session, talking: false }).await;
                }
            },
            result = &mut session => {
                finished = true;
                outcome = Some(Outcome::Ended(result));
                break;
            },
            Some(sound) = sounds.recv() => {
                // Only the deafen sound (below) plays while deafened, as in stock Mumble.
                if deafened { continue; }
                match &media {
                    Some(media) => play(media, &settings.borrow(), sound),
                    None => queue(&mut pending, sound),
                }
            },
            Some(command) = commands.recv() => {
                if let Command::SetSelf { muted: mute, deafened: deaf } = &command {
                    let cue = crate::sounds::self_state((muted, deafened), (*mute || *deaf, *deaf));
                    muted = *mute || *deaf; deafened = *deaf; capture_epoch += 1;
                    if muted && std::mem::take(&mut transmitting) {
                        let _ = session_commands.try_send(silent_terminator(&mut sequence));
                        if let Some(session) = own_session { mark_talking(events, &id, generation, &mut speaking, session, false).await; }
                    }
                    // Native stream teardown can block inside a backend (including Pulse).
                    // Replacing the isolated worker closes its audio capability even then;
                    // no old callback or queued capture can survive a mute transition.
                    media.take();
                    retry_audio = tokio::time::Instant::now();
                    let current = settings.borrow();
                    match cue.filter(|_| current.sound_effects) {
                        // No worker runs while deafened: one that opens only the output plays it.
                        Some(sound) if deafened => { tokio::spawn(chime(current.audio.clone(), current.sound_volume, pinned_host(&audio_host), sound)); },
                        Some(sound) => queue(&mut pending, sound),
                        None => {},
                    }
                }
                if session_commands.send(command).await.is_err() { break; }
            },
            Some(event) = incoming.recv() => match event {
                SessionEvent::ChatPending(pending) => {
                    if restore.unacked.len() < 256 { restore.unacked.push(pending); }
                },
                SessionEvent::ChatAcked { key, seq } => {
                    restore.unacked.retain(|pending| pending.key != key);
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::ChatAcked { key, seq } }).await;
                },
                SessionEvent::ChatFailed { key, reason } => {
                    restore.unacked.retain(|pending| pending.key != key);
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::ChatFailed { key, reason } }).await;
                },
                SessionEvent::Nuschel(NuschelEvent::Page(page)) => {
                    let asked = server_pages().remove(&page.request_id);
                    if let Some((server, ticket, before)) = asked {
                        let events = events.clone();
                        tokio::spawn(async move {
                            let (messages, next_before) = render_server_page(page).await;
                            let _ = events.send(Event::History { server, ticket, before, messages, next_before }).await;
                        });
                    }
                },
                SessionEvent::Nuschel(NuschelEvent::Error(error)) => {
                    let asked = server_pages().remove(&error.request_id);
                    match asked {
                        Some((server, ticket, _)) => { let _ = events.send(Event::HistoryFailed { server, ticket, reason: error.detail }).await; },
                        None => { let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::Nuschel(NuschelEvent::Error(error)) }).await; },
                    }
                },
                SessionEvent::Negotiated(negotiated) => {
                    // Remember what this server offers: a weaker answer later fails.
                    let raised = nuschel_session::sticky::PolicyStore::load(&profile.config_dir)
                        .and_then(|mut store| store.raise(&server.host, server.port, negotiated.nuschel, &negotiated.capabilities));
                    if let Err(error) = raised { let _ = events.send(Event::HistoryError(format!("Could not save this server's security record: {error}"))).await; }
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::Negotiated(negotiated) }).await;
                },
                SessionEvent::PeerCertificate(sha256) => { peer = Some(sha256.clone()); let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::PeerCertificate(sha256) }).await; },
                SessionEvent::Connected { session: connected_session, max_bandwidth } => {
                    restore.connected = true;
                    if let Some(from_ms) = restore.disconnected_at_ms.take() {
                        let _ = events.send(Event::Gap { server: id.clone(), generation, from_ms, to_ms: connection::now_ms() }).await;
                    }
                    own_session = Some(connected_session);
                    let own_session = connected_session;
                    audio_enabled = true;
                    bitrate = max_bandwidth.saturating_sub(24_000).clamp(6000, 48_000);
                    retry_audio = tokio::time::Instant::now();
                    if !deafened { queue(&mut pending, Sound::Connected); }
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::Connected { session: own_session, max_bandwidth } }).await;
                },
                SessionEvent::Audio(packet) => {
                    let hash = hashes.get(&packet.sender).cloned().flatten();
                    let (muted_locally, volume) = {
                        let current = settings.borrow();
                        (current.locally_muted(hash.as_deref()), current.volume_for(hash.as_deref()))
                    };
                    if muted_locally { continue; }
                    mark_talking(events, &id, generation, &mut speaking, packet.sender, !packet.end).await;
                    if let Some(media) = &media {
                        let _ = media.commands.try_send(LiveMediaRequest::Playback { sender: packet.sender, sequence: packet.sequence, opus: packet.opus, end: packet.end, volume: packet.volume * volume });
                    }
                },
                SessionEvent::User(user) => {
                    hashes.insert(user.session, user.certificate_hash.clone());
                    if Some(user.session) == own_session {
                        restore.channel = Some(user.channel);
                    }
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::User(user) }).await;
                },
                SessionEvent::UserRemoved(session) => {
                    hashes.remove(&session);
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::UserRemoved(session) }).await;
                },
                SessionEvent::Transport(stats) => {
                    let total = stats.good.saturating_add(stats.lost).max(1);
                    let measured = ((stats.lost as u64 * 100 / total as u64) as u8).clamp(1, 30);
                    if measured.abs_diff(loss_percent) >= 3 {
                        loss_percent = measured;
                        if let Some(media) = &media {
                            configure(media, &applied_audio, (bitrate, loss_percent), pinned_host(&audio_host));
                        }
                    }
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::Transport(stats) }).await;
                },
                SessionEvent::Text(text) => if let Some(peer) = &peer {
                    if text.outgoing { let _ = events.send(Event::Sent { server: id.clone(), generation, target: text.target.clone(), body: text.body.clone() }).await; }
                    let job = ContentJob { server: id.clone(), generation, identity: server.history_identity(peer), nonce: nonce.clone(), text };
                    if content.try_send(job).is_err() {
                        if !content_gap { let _ = events.send(Event::HistoryError("Chat processing is overloaded; some received messages could not be saved.".into())).await; content_gap = true; }
                    } else { content_gap = false; }
                },
                SessionEvent::Welcome(body) => { let (text, _) = prepare(&body, true).await; let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::Welcome(text) }).await; },
                event => { let _ = events.send(Event::Protocol { server: id.clone(), generation, event }).await; },
            },
            _ = tokio::time::sleep_until(retry_audio), if audio_enabled && !deafened && media.is_none() => {
                capture_epoch += 1;
                match Media::start() {
                    Ok(worker) => {
                        configure(&worker, &applied_audio, (bitrate, loss_percent), pinned_host(&audio_host));
                        let _ = worker.commands.try_send(LiveMediaRequest::SetState { epoch: capture_epoch, muted, deafened });
                        let _ = worker.commands.try_send(LiveMediaRequest::PushToTalk { active: voice_keys.borrow().talk });
                        for (sound, requested) in pending.drain(..) {
                            if requested.elapsed() < MAX_SOUND_DELAY { play(&worker, &settings.borrow(), sound); }
                        }
                        media = Some(worker);
                        transmitting = false;
                    },
                    Err(error) => { retry_audio = tokio::time::Instant::now() + std::time::Duration::from_secs(3); let _ = events.send(Event::AudioStatus { server: id.clone(), generation, message: error.to_string() }).await; },
                }
            },
            event = async { media.as_mut().unwrap().events.recv().await }, if media.is_some() => match event {
                Some(LiveMediaEvent::Packet { epoch, opus, end, .. }) => {
                    // A muted worker never encodes, and nothing from an older one leaves.
                    if epoch != capture_epoch || muted || voice_keys.borrow().mute { continue; }
                    let packet = nuschel_protocol::voice::VoicePacket { sender: 0, sequence, opus, end, volume: 1. };
                    sequence += 2;
                    transmitting = !end;
                    let _ = session_commands.try_send(Command::Voice { target: 0, packet });
                    if let Some(session) = own_session { mark_talking(events, &id, generation, &mut speaking, session, !end).await; }
                },
                Some(LiveMediaEvent::Devices { inputs, outputs, input, output, echo, noise, reference }) => {
                    if let Some((host, _)) = output.as_deref().and_then(|id| id.split_once(':')) {
                        audio_host.0 = Some(host.to_string());
                    }
                    let _ = events.send(Event::AudioDevices { server: id.clone(), generation, inputs, outputs, input, output, echo, noise, reference }).await;
                },
                Some(LiveMediaEvent::Echo { erle_db, residual_likelihood, .. }) => {
                    let _ = events.try_send(Event::Echo { server: id.clone(), generation, erle_db, residual_likelihood });
                },
                Some(LiveMediaEvent::Health { clipping, echo_guard, double_talk, underruns, playback_ms }) => {
                    let health = crate::state::AudioHealth { clipping, echo_guard, double_talk, underruns, playback_ms };
                    let _ = events.send(Event::AudioHealth { server: id.clone(), generation, health }).await;
                },
                Some(LiveMediaEvent::Status { ready, message }) => {
                    if !ready { audio_host.1 = tokio::time::Instant::now() + HOST_PIN; media.take(); retry_audio = tokio::time::Instant::now() + std::time::Duration::from_millis(if message.starts_with("Audio reconnecting") { 2000 } else { 200 }); }
                    let _ = events.send(Event::AudioStatus { server: id.clone(), generation, message }).await;
                },
                // The meter and the echo test are the Settings check's (preview.rs).
                Some(LiveMediaEvent::Level { .. } | LiveMediaEvent::EchoTest { .. }) => {},
                None => { media.take(); retry_audio = tokio::time::Instant::now() + std::time::Duration::from_secs(2); let _ = events.send(Event::AudioStatus { server: id.clone(), generation, message: "Audio worker restarting…".into() }).await; },
            },
        }
    }
    restore.muted = muted;
    restore.deafened = deafened;
    if !finished {
        session.abort();
        let _ = session.await;
    }
    outcome.unwrap_or(Outcome::Closed)
}

const TALK_HOLD: std::time::Duration = std::time::Duration::from_millis(280);

/// Push-to-talk and push-to-mute keys, held or not.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VoiceKeys {
    pub talk: bool,
    pub mute: bool,
}

/// Ends our transmission. Receivers stop at once, as with stock Mumble, but
/// the terminator is silence: nothing the microphone captured after muting leaves.
fn silent_terminator(sequence: &mut u64) -> Command {
    let packet = nuschel_protocol::voice::VoicePacket {
        sender: 0,
        sequence: *sequence,
        opus: nuschel_core::audio::SILENT_OPUS.to_vec(),
        end: true,
        volume: 1.,
    };
    *sequence += 2;
    Command::Voice { target: 0, packet }
}

async fn mark_talking(
    events: &mpsc::Sender<Event>,
    server: &str,
    generation: u64,
    speaking: &mut BTreeMap<u32, tokio::time::Instant>,
    session: u32,
    talking: bool,
) {
    let changed = if talking {
        speaking
            .insert(session, tokio::time::Instant::now())
            .is_none()
    } else {
        speaking.remove(&session).is_some()
    };
    if changed {
        let _ = events
            .send(Event::Talking {
                server: server.into(),
                generation,
                session,
                talking,
            })
            .await;
    }
}

/// The content worker next to this executable, looked up once.
pub fn content_worker() -> Option<&'static std::path::Path> {
    static WORKER: std::sync::LazyLock<Option<PathBuf>> =
        std::sync::LazyLock::new(|| workers::sibling("nuschel-content-worker").ok());
    WORKER.as_deref()
}

/// What is shown when the content worker cannot render a message.
fn plain(source: &str) -> MessageDocument {
    MessageDocument {
        blocks: vec![ContentBlock::text(source)],
    }
}

async fn prepare(source: &str, html: bool) -> (String, MessageDocument) {
    let prepared = match content_worker() {
        Some(worker) => nuschel_ipc::host::prepare_content(worker, source, html)
            .await
            .ok(),
        None => None,
    };
    match prepared {
        Some(prepared) => (prepared.source, prepared.document),
        None => (source.into(), plain(source)),
    }
}
async fn content_loop(
    mut jobs: mpsc::Receiver<ContentJob>,
    storage: mpsc::Sender<StorageCommand>,
    events: mpsc::Sender<Event>,
) {
    while let Some(job) = jobs.recv().await {
        let (source, document) = prepare(&job.text.body, job.text.html).await;
        if storage
            .send(StorageCommand::Append {
                job: Box::new(job),
                source,
                document,
            })
            .await
            .is_err()
        {
            let _ = events
                .send(Event::HistoryError(
                    "Local history worker has stopped.".into(),
                ))
                .await;
            break;
        }
    }
}
async fn storage_loop(
    directory: PathBuf,
    mut jobs: mpsc::Receiver<StorageCommand>,
    events: mpsc::Sender<Event>,
) {
    let mut storage: Option<Storage> = None;
    // Pages render in their own task, so appends of new messages never wait
    // for one; they arrive in the order they were loaded.
    let (pages, rendering) = mpsc::channel(16);
    tokio::spawn(history_loop(rendering, events.clone()));
    while let Some(job) = jobs.recv().await {
        if storage.is_none() {
            match Storage::start(&directory) {
                Ok(worker) => storage = Some(worker),
                Err(error) => {
                    let _ = events.send(Event::HistoryError(error.to_string())).await;
                }
            }
        }
        match job {
            StorageCommand::Append {
                job,
                source,
                document,
            } => {
                let storage_key = match &job.text.target {
                    TextTarget::Channel(id) => id.to_string(),
                    TextTarget::Direct(id) => job
                        .text
                        .peer
                        .as_ref()
                        .and_then(|peer| peer.certificate_hash.as_ref())
                        .map(|hash| format!("direct:{hash}"))
                        .unwrap_or_else(|| {
                            format!(
                                "direct:{}:{id}:{}",
                                job.nonce,
                                job.text
                                    .peer
                                    .as_ref()
                                    .map(|peer| peer.instance)
                                    .unwrap_or(0)
                            )
                        }),
                };
                let message = ChatMessage {
                    server: job.identity,
                    channel: storage_key.clone(),
                    author: job.text.author,
                    observed_at_ms: job.text.observed_at_ms,
                    body: source,
                };
                // A crashed worker is replaced and the append retried once, so the
                // message that discovered the failure is not lost.
                let mut id = None;
                for attempt in 0..2 {
                    if storage.is_none() {
                        storage = Storage::start(&directory).ok();
                    }
                    let Some(worker) = &mut storage else { break };
                    match worker.append(message.clone()).await {
                        Ok(saved) => {
                            id = Some(saved);
                            break;
                        }
                        Err(error) => {
                            storage.take();
                            if attempt == 1 {
                                let _ = events.send(Event::HistoryError(error.to_string())).await;
                            }
                        }
                    }
                }
                if id.is_some() {
                    let _ = events.send(Event::HistoryRecovered).await;
                }
                // On a Nuschel server the timeline counts in the server's seqs;
                // the local copy is the offline cache. Our own message has
                // none until it is acknowledged.
                let id = match (job.text.seq, job.text.pending) {
                    (Some(seq), _) => Some(seq as i64),
                    (None, Some(_)) => None,
                    (None, None) => id,
                };
                let _ = events
                    .send(Event::Message {
                        server: job.server,
                        generation: job.generation,
                        target: job.text.target,
                        storage_key,
                        peer: job.text.peer,
                        message: RenderedMessage {
                            id,
                            message,
                            document,
                            outgoing: job.text.outgoing,
                            pending: job.text.pending,
                        },
                    })
                    .await;
            }
            StorageCommand::Search {
                request,
                scope: (servers, channel),
                query,
                after,
            } => {
                let result = match &mut storage {
                    Some(worker) => worker.search(servers, channel, query, after).await,
                    None => Err(Error::Worker("Local history is unavailable.".into())),
                };
                if result.is_err() {
                    storage.take();
                }
                let result = result.map_err(|error| error.to_string());
                let _ = events.send(Event::SearchResults { request, result }).await;
            }
            StorageCommand::Load {
                server,
                identity,
                channel,
                ticket,
                page,
            } => {
                let result = if let Some(worker) = &mut storage {
                    match page {
                        Page::Before(before) => worker.page(identity, channel, before).await,
                        Page::After(after) => worker.page_after(identity, channel, after).await,
                        Page::Around(id) => worker.page_around(identity, channel, id).await,
                    }
                } else {
                    Err(Error::Worker("Local history is unavailable.".into()))
                };
                match result {
                    Ok(history) => {
                        let _ = pages.send((server, ticket, page, history)).await;
                    }
                    Err(error) => {
                        storage.take();
                        let _ = events.send(Event::HistoryError(error.to_string())).await;
                    }
                }
            }
        }
    }
    if let Some(worker) = storage {
        worker.stop().await;
    }
}

/// Render loaded history pages, each in one content worker.
async fn history_loop(
    mut pages: mpsc::Receiver<(String, HistoryTicket, Page, HistoryPage)>,
    events: mpsc::Sender<Event>,
) {
    while let Some((server, ticket, kind, page)) = pages.recv().await {
        let sources: Vec<String> = page
            .messages
            .iter()
            .map(|entry| entry.message.body.clone())
            .collect();
        let documents = match content_worker() {
            Some(worker) => nuschel_ipc::host::render_many(worker, &sources).await,
            None => vec![None; sources.len()],
        };
        let messages = page
            .messages
            .into_iter()
            .zip(documents)
            .map(|(entry, document)| RenderedMessage {
                id: Some(entry.id),
                document: document.unwrap_or_else(|| plain(&entry.message.body)),
                message: entry.message,
                outgoing: false,
                pending: None,
            })
            .collect();
        let history = match kind {
            Page::Before(before) => Event::History {
                server,
                ticket,
                before,
                messages,
                next_before: page.next_before,
            },
            page_kind => Event::HistoryWindow {
                server,
                ticket,
                page: page_kind,
                messages,
                next_before: page.next_before,
                next_after: page.next_after,
            },
        };
        if events.send(history).await.is_err() {
            break;
        }
    }
}

/// After an audio failure, new media workers use only the audio server that
/// worked before, for this long. A restarting PipeWire or PulseAudio must not
/// lose its hardware device to a fallback that opens ALSA directly.
const HOST_PIN: std::time::Duration = std::time::Duration::from_secs(60);

/// The audio settings, and the bitrate and packet loss the encoder plans for.
fn configure(
    media: &Media,
    audio: &nuschel_core::audio::AudioSettings,
    (bitrate, packet_loss_percent): (u32, u8),
    host: Option<String>,
) {
    let _ = media.commands.try_send(LiveMediaRequest::Configure {
        bitrate,
        packet_loss_percent,
        settings: audio.clone(),
        host,
    });
}

/// Sound effects older than this when a worker starts are stale.
const MAX_SOUND_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// Keep a sound effect for the worker that is starting (a few at most).
fn queue(pending: &mut Vec<(Sound, tokio::time::Instant)>, sound: Sound) {
    if pending.len() < 4 {
        pending.push((sound, tokio::time::Instant::now()));
    }
}

fn play(media: &Media, settings: &ClientSettings, sound: Sound) {
    if settings.sound_effects {
        let _ = media.commands.try_send(LiveMediaRequest::PlaySound {
            sound,
            volume: settings.sound_volume,
        });
    }
}

/// Play a sound where no connection's worker can (deafened or disconnected):
/// a worker that opens only the output plays it and exits.
async fn chime(
    audio: nuschel_core::audio::AudioSettings,
    volume: f32,
    host: Option<String>,
    sound: Sound,
) {
    let Ok(mut worker) = Media::start() else {
        return;
    };
    configure(&worker, &audio, (48_000, 5), host);
    let _ = worker.commands.try_send(LiveMediaRequest::SetState {
        epoch: 0,
        muted: true,
        deafened: false,
    });
    let _ = worker
        .commands
        .try_send(LiveMediaRequest::PlaySound { sound, volume });
    // Until the sound has played (the longest is 0.8 s), or the devices failed.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(event) = worker.events.recv().await {
            if let LiveMediaEvent::Status { ready, .. } = event {
                if ready {
                    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                }
                break;
            }
        }
    })
    .await;
}

fn pinned_host(audio_host: &(Option<String>, tokio::time::Instant)) -> Option<String> {
    audio_host
        .0
        .clone()
        .filter(|_| tokio::time::Instant::now() < audio_host.1)
}
