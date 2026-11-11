// SPDX-License-Identifier: EUPL-1.2
//! The native driver: the SDK's platform services on Linux, Windows and
//! macOS, on tokio. It supervises one connection per server (`nuschel-session`)
//! with reconnects and backoff, runs each connection's media worker, renders
//! messages in the sandboxed content worker and keeps local history in the
//! storage worker, and reports what happens as typed [`Event`]s for the
//! client to apply. Nothing here runs on the caller's thread: every method
//! returns at once, and answers arrive as events.
use crate::{
    HistoryTicket,
    input::{Input, ServerEvent},
    platform::{CacheKey, ChangeRequest, Platform, RenderKey, Request, request_id},
    server::{AudioHealth, direct_conversation_key},
    timeline::{Deletion, IncomingMessage, Page},
};
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

/// Server history pages asked for, by request id: server, ticket, cursor,
/// and where the page is kept in local history.
type ServerPages = BTreeMap<u64, (String, HistoryTicket, Page, Option<CacheKey>)>;
static SERVER_PAGES: std::sync::Mutex<ServerPages> = std::sync::Mutex::new(BTreeMap::new());

fn server_pages() -> std::sync::MutexGuard<'static, ServerPages> {
    SERVER_PAGES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

static CHANGES: std::sync::Mutex<BTreeMap<u64, ChangeRequest>> =
    std::sync::Mutex::new(BTreeMap::new());

fn changes() -> std::sync::MutexGuard<'static, BTreeMap<u64, ChangeRequest>> {
    CHANGES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The author shown for a server message.
fn author_name(author: &v1::Author) -> String {
    if author.name.is_empty() && author.account.is_empty() {
        "Unknown".to_owned()
    } else if author.name.is_empty() {
        "Deleted user".to_owned()
    } else {
        author.name.clone()
    }
}

/// Render a Nuschel server's history page like a local one: text in one
/// content worker, stock clients' HTML made inert first. Ids are seqs;
/// messages come in their current state (edited, or a tombstone); edit,
/// deletion and void rows only move the cursor. Also returns the records
/// that keep the page in local history (`cache`).
async fn render_server_page(
    page: v1::HistoryPage,
    cache: Option<&CacheKey>,
) -> (
    Vec<IncomingMessage>,
    (Option<i64>, Option<i64>),
    Vec<nuschel_core::ServerRecord>,
) {
    let next_before = page
        .more_before
        .then(|| page.messages.first().map(|message| message.seq as i64))
        .flatten();
    let next_after = page
        .more_after
        .then(|| page.messages.last().map(|message| message.seq as i64))
        .flatten();
    let shown: Vec<v1::Message> = page
        .messages
        .into_iter()
        // MLS rows (`mls/1`) are ciphertext: the session hands them to the crypto
        // worker, never to the timeline as text.
        .filter(|message| {
            message.kind == v1::message::Kind::Message as i32
                && !nuschel_protocol::nuschel::is_mls_format(message.format)
        })
        .collect();
    let mut out = Vec::with_capacity(shown.len());
    let mut records = Vec::new();
    for message in shown {
        let author = message.author.clone().unwrap_or_default();
        let name = author_name(&author);
        let deleted = (message.deleted_ms != 0).then_some(Deletion {
            at_ms: message.deleted_ms,
            moderator: message.delete_reason == 1,
        });
        let (source, document) = if deleted.is_some() {
            (String::new(), MessageDocument { blocks: Vec::new() })
        } else {
            prepare(
                &message.content,
                message.format == v1::Format::LegacyHtml as i32,
            )
            .await
        };
        let edited_ms = (message.edited_ms != 0).then_some(message.edited_ms);
        if let Some(cache) = cache {
            records.push(nuschel_core::ServerRecord {
                message: ChatMessage {
                    server: cache.identity.clone(),
                    channel: cache.channel.clone(),
                    author: name.clone(),
                    observed_at_ms: message.sent_ms,
                    body: source.clone(),
                },
                seq: message.seq as i64,
                edited_ms,
                deleted_ms: deleted.map(|deleted| deleted.at_ms),
            });
        }
        out.push(IncomingMessage {
            id: Some(message.seq as i64),
            message: ChatMessage {
                server: String::new(),
                channel: String::new(),
                author: name,
                observed_at_ms: message.sent_ms,
                body: source,
            },
            document,
            outgoing: false,
            pending: None,
            seq: Some(message.seq),
            author_id: (author.user_id != 0).then_some(author.user_id),
            edited_ms,
            deleted,
        });
    }
    (out, (next_before, next_after), records)
}
pub enum Event {
    /// The crypto worker's answer to [`Driver::identity`].
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
        message: IncomingMessage,
    },
    History {
        server: String,
        ticket: HistoryTicket,
        /// The cursor this page was asked for: None is the latest page.
        before: Option<i64>,
        messages: Vec<IncomingMessage>,
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
        messages: Vec<IncomingMessage>,
        next_before: Option<i64>,
        next_after: Option<i64>,
    },
    HistoryError(String),
    /// A message on a Nuschel server was edited (by its author, perhaps us).
    Edited {
        server: String,
        conversation: [u8; 16],
        seq: u64,
        source: String,
        document: MessageDocument,
        edited_ms: i64,
    },
    /// A message on a Nuschel server became a tombstone.
    Deleted {
        server: String,
        conversation: [u8; 16],
        seq: u64,
        deletion: Deletion,
    },
    /// The server refused our edit or deletion.
    ChangeFailed {
        server: String,
        delete: bool,
        reason: String,
    },
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
    /// Results for search `request` (see `Driver::search`).
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
        health: AudioHealth,
    },
    /// Inert rendering of untrusted Mumble HTML (descriptions, comments).
    Rendered {
        server: String,
        generation: u64,
        key: RenderKey,
        source: String,
        document: MessageDocument,
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
struct Connection {
    commands: mpsc::Sender<Command>,
    sounds: mpsc::Sender<Sound>,
    task: tokio::task::JoinHandle<()>,
}
pub struct Driver {
    handle: Handle,
    profile: Profile,
    /// The crypto worker, shared by every connection (identity and key transparency).
    crypto: nuschel_session::identity::CryptoHandle,
    sender: mpsc::Sender<Event>,
    storage: mpsc::Sender<StorageCommand>,
    content: mpsc::Sender<ContentWork>,
    connections: std::sync::Mutex<BTreeMap<String, Connection>>,
    generation: std::sync::atomic::AtomicU64,
    settings: watch::Sender<ClientSettings>,
    voice_keys: watch::Sender<VoiceKeys>,
    render_permits: std::sync::Arc<tokio::sync::Semaphore>,
    storage_task: tokio::task::JoinHandle<()>,
    content_task: tokio::task::JoinHandle<()>,
}
struct ContentJob {
    server: String,
    generation: u64,
    identity: String,
    nonce: String,
    text: IncomingText,
    /// The conversation's timeline is the server's history (its ids are
    /// seqs): a message without a seq gets no id there.
    archived: bool,
}
/// What the content worker prepares, in the order it arrived: messages, and
/// edits and deletions of them (so a change never overtakes its message on
/// the way to local history and the timeline).
enum ContentWork {
    Text(Box<ContentJob>),
    Change(ChangeJob),
}
struct ChangeJob {
    server: String,
    conversation: [u8; 16],
    seq: u64,
    cache: Option<CacheKey>,
    change: ChangeKind,
}
enum ChangeKind {
    Edit { content: String, edited_ms: i64 },
    Delete(Deletion),
}
enum StorageCommand {
    Append {
        job: Box<ContentJob>,
        source: String,
        document: MessageDocument,
    },
    /// A Nuschel server's messages by seq (a history page): kept, or
    /// replaced where already kept.
    StoreServer {
        records: Vec<nuschel_core::ServerRecord>,
    },
    /// Our message (by its key) was stored by the server as `seq`.
    Acked { key: [u8; 16], seq: u64 },
    /// A message was edited or deleted: kept so in local history, then
    /// shown (`Event::Edited`, `Event::Deleted`).
    Change {
        server: String,
        conversation: [u8; 16],
        seq: u64,
        cache: Option<CacheKey>,
        change: CacheChange,
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
enum CacheChange {
    Edit {
        source: String,
        document: MessageDocument,
        edited_ms: i64,
    },
    Delete(Deletion),
}
impl Driver {
    /// The driver, and the events it reports for the client to apply.
    pub fn new(
        handle: Handle,
        profile: Profile,
        settings: ClientSettings,
    ) -> (Self, mpsc::Receiver<Event>) {
        let (sender, events) = mpsc::channel(512);
        let (storage, storage_rx) = mpsc::channel(128);
        let (content, content_rx) = mpsc::channel(64);
        let storage_task = handle.spawn(storage_loop(
            profile.history_dir().to_owned(),
            storage_rx,
            sender.clone(),
        ));
        let content_task = handle.spawn(content_loop(content_rx, storage.clone(), sender.clone()));
        // Started on first use, in its sandbox, on its own folder in the data
        // folder (docs/e2ee.md §3.7).
        let crypto = {
            let _runtime = handle.enter();
            nuschel_session::identity::CryptoHandle::start(
                nuschel_session::workers::sibling("nuschel-crypto-worker")
                    .unwrap_or_else(|_| "nuschel-crypto-worker".into()),
                profile.keys_dir().to_owned(),
            )
        };
        let backend = Self {
            handle,
            crypto,
            profile,
            sender,
            storage,
            content,
            connections: Default::default(),
            generation: Default::default(),
            settings: watch::Sender::new(settings),
            voice_keys: watch::Sender::new(VoiceKeys::default()),
            render_permits: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
            storage_task,
            content_task,
        };
        (backend, events)
    }
    /// Connect self-muted; `deafened` also deafens (automatic connections at startup).
    pub fn connect(
        &self,
        server: ServerBookmark,
        password: Option<String>,
        deafened: bool,
    ) -> Result<u64, String> {
        let mut connections = self.connections();
        if connections.len() >= 8 && !connections.contains_key(&server.id) {
            return Err("Disconnect a server before opening another connection (limit: 8).".into());
        }
        if let Some(old) = connections.remove(&server.id) {
            old.task.abort();
        }
        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
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
            (self.content.clone(), self.storage.clone()),
            (self.settings.subscribe(), self.voice_keys.subscribe()),
        ));
        connections.insert(
            id,
            Connection {
                commands,
                sounds,
                task,
            },
        );
        Ok(generation)
    }
    /// A page of a Nuschel server's history: before a seq (or the latest),
    /// after one, or around one (a search result); ids are seqs.
    pub fn server_history(
        &self,
        server: String,
        conversation: [u8; 16],
        (ticket, cache): (HistoryTicket, Option<CacheKey>),
        page: Page,
    ) -> Result<(), String> {
        use v1::history_request::Anchor;
        let request_id = request_id();
        let seq = |seq: i64| seq.max(0) as u64;
        let anchor = match page {
            Page::Before(Some(before)) => Anchor::Before(seq(before)),
            Page::Before(None) => Anchor::Latest(true),
            Page::After(after) => Anchor::After(seq(after)),
            Page::Around(around) => Anchor::Around(seq(around)),
        };
        {
            let mut pages = server_pages();
            // Answers that never came (a dropped connection) do not pile up.
            while pages.len() >= 64 {
                pages.pop_first();
            }
            pages.insert(request_id, (server.clone(), ticket, page, cache));
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
    /// Edit (`content`) or delete a message on a Nuschel server. The answer
    /// arrives as `Event::Edited`/`Event::Deleted` or `Event::ChangeFailed`.
    pub fn change_message(&self, change: ChangeRequest) -> Result<(), String> {
        let request_id = request_id();
        let conversation = change.conversation.to_vec();
        let request = match &change.content {
            Some(content) => NuschelRequest::Edit(v1::ChatEdit {
                request_id,
                conversation,
                target: change.seq,
                format: v1::Format::Text as i32,
                content: content.clone(),
            }),
            None => NuschelRequest::Delete(v1::ChatDelete {
                request_id,
                conversation,
                target: change.seq,
            }),
        };
        let server = change.server.clone();
        {
            let mut pending = changes();
            while pending.len() >= 64 {
                pending.pop_first();
            }
            pending.insert(request_id, change);
        }
        let result = self.command(&server, Command::Nuschel(request));
        if result.is_err() {
            changes().remove(&request_id);
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
    fn connections(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Connection>> {
        self.connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    /// Send a command to a server's session.
    pub fn command(&self, server: &str, command: Command) -> Result<(), String> {
        self.connections()
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
    /// The runtime the driver's tasks run on.
    pub fn runtime(&self) -> &Handle {
        &self.handle
    }
    /// The preferences as they are now, and as they change.
    pub fn settings(&self) -> watch::Receiver<ClientSettings> {
        self.settings.subscribe()
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
        if let Some(connection) = self.connections().get(server) {
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
    pub fn disconnect(&self, server: &str) {
        if let Some(connection) = self.connections().remove(server) {
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
impl Drop for Driver {
    fn drop(&mut self) {
        for connection in self.connections().values() {
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
    /// The client dropped this connection.
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
    (content, storage): (mpsc::Sender<ContentWork>, mpsc::Sender<StorageCommand>),
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
            (&content, &storage),
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
    (content, storage): (&mpsc::Sender<ContentWork>, &mpsc::Sender<StorageCommand>),
    (settings, voice_keys): (
        &mut watch::Receiver<ClientSettings>,
        &mut watch::Receiver<VoiceKeys>,
    ),
    restore: &mut Restore,
) -> Outcome {
    let id = server.id.clone();
    // The sticky policy: an unreadable record fails closed, never empty.
    let required = profile
        .policy()
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
                e2ee_direct: settings.borrow().e2ee_direct_messages,
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
    let mut peer: Option<String> = None;
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
    // Where each channel's conversation is kept in local history.
    let mut conversation_keys = BTreeMap::<[u8; 16], String>::new();
    // Timelines here are the server's history (the extension's history/1).
    let mut server_history = false;
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
                    let _ = storage.send(StorageCommand::Acked { key, seq }).await;
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::ChatAcked { key, seq } }).await;
                },
                SessionEvent::ChatFailed { key, reason } => {
                    restore.unacked.retain(|pending| pending.key != key);
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::ChatFailed { key, reason } }).await;
                },
                SessionEvent::Nuschel(NuschelEvent::Page(page)) => {
                    let asked = server_pages().remove(&page.request_id);
                    if let Some((server, ticket, asked, cache)) = asked {
                        let (events, storage) = (events.clone(), storage.clone());
                        tokio::spawn(async move {
                            let (messages, (next_before, next_after), records) = render_server_page(page, cache.as_ref()).await;
                            // Kept for offline reading and search.
                            if !records.is_empty() { let _ = storage.send(StorageCommand::StoreServer { records }).await; }
                            let event = match asked {
                                Page::Before(before) => Event::History { server, ticket, before, messages, next_before },
                                page => Event::HistoryWindow { server, ticket, page, messages, next_before, next_after },
                            };
                            let _ = events.send(event).await;
                        });
                    }
                },
                SessionEvent::Nuschel(NuschelEvent::Ack(ack)) if changes().contains_key(&ack.request_id) => {
                    let change = changes().remove(&ack.request_id);
                    if let Some(change) = change {
                        // The server tells others, never us: apply it here.
                        let at_ms = ack.server_ms;
                        let kind = match change.content {
                            Some(content) => ChangeKind::Edit { content, edited_ms: at_ms },
                            None => ChangeKind::Delete(Deletion { at_ms, moderator: change.moderator }),
                        };
                        let job = ChangeJob { server: change.server, conversation: change.conversation, seq: change.seq, cache: change.cache, change: kind };
                        queue_change(content, events, job).await;
                    }
                },
                SessionEvent::Nuschel(NuschelEvent::Error(error)) => {
                    let asked = server_pages().remove(&error.request_id);
                    let change = changes().remove(&error.request_id);
                    match (asked, change) {
                        (Some((server, ticket, _, _)), _) => { let _ = events.send(Event::HistoryFailed { server, ticket, reason: error.detail }).await; },
                        (None, Some(change)) => { let _ = events.send(Event::ChangeFailed { server: change.server, delete: change.content.is_none(), reason: error.detail }).await; },
                        (None, None) => { let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::Nuschel(NuschelEvent::Error(error)) }).await; },
                    }
                },
                SessionEvent::Nuschel(NuschelEvent::Chat(chat)) => {
                    // New messages arrive as `Text`; edits and deletions here.
                    let Some(message) = chat.message else { continue };
                    let Some(conversation) = nuschel_protocol::nuschel::id(&chat.conversation) else { continue };
                    let cache = peer.as_deref().map(|peer| CacheKey {
                        identity: server.history_identity(peer),
                        channel: conversation_keys.get(&conversation).cloned().unwrap_or_else(|| direct_conversation_key(&conversation)),
                    });
                    let kind = if message.kind == v1::message::Kind::Edit as i32 {
                        ChangeKind::Edit { content: message.content, edited_ms: message.edited_ms }
                    } else if message.kind == v1::message::Kind::Delete as i32 {
                        ChangeKind::Delete(Deletion { at_ms: message.deleted_ms, moderator: message.delete_reason == 1 })
                    } else {
                        continue;
                    };
                    let job = ChangeJob { server: id.clone(), conversation, seq: message.target, cache, change: kind };
                    queue_change(content, events, job).await;
                },
                SessionEvent::ChannelInfo { channel, info } => {
                    if let Some(conversation) = nuschel_protocol::nuschel::id(&info.conversation)
                        && conversation_keys.len() < 4096
                    {
                        conversation_keys.insert(conversation, channel.to_string());
                    }
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::ChannelInfo { channel, info } }).await;
                },
                SessionEvent::Negotiated(negotiated) => {
                    server_history = negotiated.capabilities.iter().any(|capability| capability == "history/1");
                    // Remember what this server offers: a weaker answer later fails.
                    let raised = profile.policy()
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
                    let archived = server_history && matches!(text.target, TextTarget::Channel(_) | TextTarget::DirectConversation(_));
                    let job = ContentJob { server: id.clone(), generation, identity: server.history_identity(peer), nonce: nonce.clone(), text, archived };
                    if content.try_send(ContentWork::Text(Box::new(job))).is_err() {
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
                    let health = AudioHealth { clipping, echo_guard, double_talk, underruns, playback_ms };
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

/// An edit or deletion joins the messages on their way to the timeline.
async fn queue_change(
    content: &mpsc::Sender<ContentWork>,
    events: &mpsc::Sender<Event>,
    job: ChangeJob,
) {
    if content.try_send(ContentWork::Change(job)).is_err() {
        let _ = events
            .send(Event::HistoryError(
                "Chat processing is overloaded; an edit or deletion was not shown.".into(),
            ))
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
    mut jobs: mpsc::Receiver<ContentWork>,
    storage: mpsc::Sender<StorageCommand>,
    events: mpsc::Sender<Event>,
) {
    while let Some(work) = jobs.recv().await {
        let command = match work {
            ContentWork::Text(job) => {
                let (source, document) = prepare(&job.text.body, job.text.html).await;
                StorageCommand::Append {
                    job,
                    source,
                    document,
                }
            }
            ContentWork::Change(job) => StorageCommand::Change {
                server: job.server,
                conversation: job.conversation,
                seq: job.seq,
                cache: job.cache,
                change: match job.change {
                    ChangeKind::Edit { content, edited_ms } => {
                        let (source, document) = prepare(&content, false).await;
                        CacheChange::Edit {
                            source,
                            document,
                            edited_ms,
                        }
                    }
                    ChangeKind::Delete(deletion) => CacheChange::Delete(deletion),
                },
            },
        };
        if storage.send(command).await.is_err() {
            let _ = events
                .send(Event::HistoryError(
                    "Local history worker has stopped.".into(),
                ))
                .await;
            break;
        }
    }
}
/// Edits and deletions kept in local history recently, by where they are
/// kept and seq: a history page rendered meanwhile still has the older text,
/// and must not bring it (or a deleted message) back.
#[derive(Default)]
struct RecentChanges {
    changes: BTreeMap<Kept, RecentChange>,
    order: std::collections::VecDeque<Kept>,
}
/// Where a message is kept: server identity, storage key and seq.
type Kept = (String, String, i64);
/// The newest edit (text and time) and the deletion time, if any.
#[derive(Default)]
struct RecentChange {
    edit: Option<(String, i64)>,
    deleted: Option<i64>,
}
impl RecentChanges {
    const KEEP: usize = 1024;
    fn note(&mut self, cache: &CacheKey, seq: i64, change: &CacheChange) {
        let key = (cache.identity.clone(), cache.channel.clone(), seq);
        if !self.changes.contains_key(&key) {
            self.order.push_back(key.clone());
            while self.order.len() > Self::KEEP {
                if let Some(old) = self.order.pop_front() {
                    self.changes.remove(&old);
                }
            }
        }
        let entry = self.changes.entry(key).or_default();
        match change {
            CacheChange::Edit {
                source, edited_ms, ..
            } => {
                if entry.edit.as_ref().is_none_or(|(_, at)| at <= edited_ms) {
                    entry.edit = Some((source.clone(), *edited_ms));
                }
            }
            CacheChange::Delete(deletion) => entry.deleted = Some(deletion.at_ms),
        }
    }
    fn patch(&self, records: &mut [nuschel_core::ServerRecord]) {
        for record in records {
            let key = (
                record.message.server.clone(),
                record.message.channel.clone(),
                record.seq,
            );
            let Some(RecentChange { edit, deleted }) = self.changes.get(&key) else {
                continue;
            };
            if let Some((body, at)) = edit
                && record.edited_ms.is_none_or(|stored| stored < *at)
            {
                record.message.body.clone_from(body);
                record.edited_ms = Some(*at);
            }
            if deleted.is_some() && record.deleted_ms.is_none() {
                record.deleted_ms = *deleted;
                record.message.body.clear();
            }
        }
    }
}
async fn storage_loop(
    directory: PathBuf,
    mut jobs: mpsc::Receiver<StorageCommand>,
    events: mpsc::Sender<Event>,
) {
    let mut storage: Option<Storage> = None;
    // Our own messages on Nuschel servers, kept before the server stored
    // them: their rows by key, and acknowledgements that came first.
    let mut unacked_rows = BTreeMap::<[u8; 16], i64>::new();
    let mut early_acks = BTreeMap::<[u8; 16], u64>::new();
    let mut recent = RecentChanges::default();
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
                    TextTarget::DirectConversation(id) => direct_conversation_key(id),
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
                // A Nuschel server's message is kept once by its seq (a page
                // may have brought it already); ours gets its seq when the
                // server acknowledges it.
                let seq = job.text.seq.map(|seq| seq as i64).or_else(|| {
                    job.text
                        .pending
                        .and_then(|key| early_acks.remove(&key))
                        .map(|seq| seq as i64)
                });
                // A crashed worker is replaced and the append retried once, so the
                // message that discovered the failure is not lost.
                let mut id = None;
                for attempt in 0..2 {
                    if storage.is_none() {
                        storage = Storage::start(&directory).ok();
                    }
                    let Some(worker) = &mut storage else { break };
                    let saved = match seq {
                        Some(seq) => {
                            let mut records = vec![nuschel_core::ServerRecord {
                                message: message.clone(),
                                seq,
                                edited_ms: None,
                                deleted_ms: None,
                            }];
                            recent.patch(&mut records);
                            worker.store_server(records).await.and_then(|ids| {
                                ids.first().copied().ok_or_else(|| {
                                    Error::Worker("unexpected storage response".into())
                                })
                            })
                        }
                        None => worker.append(message.clone()).await,
                    };
                    match saved {
                        Ok(saved) => {
                            id = Some(saved);
                            if let (None, Some(key)) = (seq, job.text.pending) {
                                while unacked_rows.len() >= 256 {
                                    unacked_rows.pop_first();
                                }
                                unacked_rows.insert(key, saved);
                            }
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
                    // Not in the server's history (sent the stock way): no
                    // id that could be taken for a seq.
                    (None, None) if job.archived => None,
                    (None, None) => id,
                };
                let author_id = job
                    .text
                    .peer
                    .as_ref()
                    .filter(|_| !job.text.outgoing)
                    .and_then(|peer| peer.user_id);
                let _ = events
                    .send(Event::Message {
                        server: job.server,
                        generation: job.generation,
                        target: job.text.target,
                        storage_key,
                        peer: job.text.peer,
                        message: IncomingMessage {
                            id,
                            message,
                            document,
                            outgoing: job.text.outgoing,
                            pending: job.text.pending,
                            seq: job.text.seq,
                            author_id,
                            edited_ms: None,
                            deleted: None,
                        },
                    })
                    .await;
            }
            StorageCommand::StoreServer { mut records } => {
                recent.patch(&mut records);
                let result = match &mut storage {
                    Some(worker) => worker.store_server(records).await.map(|_| ()),
                    None => Err(Error::Worker("Local history is unavailable.".into())),
                };
                if let Err(error) = result {
                    storage.take();
                    let _ = events.send(Event::HistoryError(error.to_string())).await;
                }
            }
            StorageCommand::Acked { key, seq } => match unacked_rows.remove(&key) {
                Some(id) => {
                    if let Some(worker) = &mut storage
                        && worker.set_seq(id, seq as i64).await.is_err()
                    {
                        storage.take();
                    }
                }
                // Acknowledged before it was kept (it is still rendering).
                None => {
                    while early_acks.len() >= 256 {
                        early_acks.pop_first();
                    }
                    early_acks.insert(key, seq);
                }
            },
            StorageCommand::Change {
                server,
                conversation,
                seq,
                cache,
                change,
            } => {
                if let Some(cache) = cache {
                    // Remembered for a page already on its way with the
                    // older text, which is kept after this.
                    recent.note(&cache, seq as i64, &change);
                    if let Some(worker) = &mut storage {
                        let result = match &change {
                            CacheChange::Edit {
                                source, edited_ms, ..
                            } => {
                                worker
                                    .edit(
                                        cache.identity,
                                        cache.channel,
                                        seq as i64,
                                        source.clone(),
                                        *edited_ms,
                                    )
                                    .await
                            }
                            CacheChange::Delete(deletion) => {
                                worker
                                    .delete(
                                        cache.identity,
                                        cache.channel,
                                        seq as i64,
                                        deletion.at_ms,
                                    )
                                    .await
                            }
                        };
                        if result.is_err() {
                            storage.take();
                        }
                    }
                }
                let event = match change {
                    CacheChange::Edit {
                        source,
                        document,
                        edited_ms,
                    } => Event::Edited {
                        server,
                        conversation,
                        seq,
                        source,
                        document,
                        edited_ms,
                    },
                    CacheChange::Delete(deletion) => Event::Deleted {
                        server,
                        conversation,
                        seq,
                        deletion,
                    },
                };
                let _ = events.send(event).await;
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
            .map(|(entry, document)| IncomingMessage {
                id: Some(entry.id),
                document: match entry.deleted_ms {
                    Some(_) => MessageDocument { blocks: Vec::new() },
                    None => document.unwrap_or_else(|| plain(&entry.message.body)),
                },
                message: entry.message,
                outgoing: false,
                pending: None,
                seq: entry.seq.and_then(|seq| u64::try_from(seq).ok()),
                author_id: None,
                edited_ms: entry.edited_ms,
                deleted: entry.deleted_ms.map(|at_ms| Deletion {
                    at_ms,
                    moderator: false,
                }),
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

impl Platform for Driver {
    fn connect(
        &self,
        bookmark: ServerBookmark,
        password: Option<String>,
        deafened: bool,
    ) -> Result<u64, String> {
        Driver::connect(self, bookmark, password, deafened)
    }
    fn disconnect(&self, server: &str) {
        Driver::disconnect(self, server)
    }
    fn request(&self, server: &str, request: Request) -> Result<(), String> {
        let command = match request {
            Request::Join(channel) => Command::Join(channel),
            Request::SendText {
                target,
                body,
                expected_peer,
            } => Command::SendText {
                target,
                body,
                expected_peer,
            },
            Request::SendHtml {
                target,
                html,
                expected_peer,
            } => Command::SendHtml {
                target,
                html,
                expected_peer,
            },
            Request::SetSelf { muted, deafened } => Command::SetSelf { muted, deafened },
            Request::RequestPermissions(channel) => Command::RequestPermissions(channel),
            Request::RequestBlobs {
                textures,
                comments,
                descriptions,
            } => Command::RequestBlobs {
                textures,
                comments,
                descriptions,
            },
            Request::QueryUserNames(ids) => Command::QueryUserNames(ids),
            Request::RegisterSelf => Command::RegisterSelf,
            Request::MarkRead { channel, seq } => Command::MarkRead { channel, seq },
            Request::ReadMarker { conversation, seq } => {
                Command::Nuschel(NuschelRequest::ReadMarker(v1::ReadMarker {
                    conversation: conversation.to_vec(),
                    seq,
                }))
            }
            Request::Typing { conversation } => {
                Command::Nuschel(NuschelRequest::Typing(v1::Typing {
                    conversation: conversation.to_vec(),
                    session: 0,
                }))
            }
            Request::OpenDirect {
                request_id,
                user_id,
            } => Command::Nuschel(NuschelRequest::OpenDirect(v1::OpenDirect {
                request_id,
                user_id,
            })),
        };
        self.command(server, command)
    }
    fn local_history(
        &self,
        server: String,
        identity: String,
        channel: String,
        ticket: HistoryTicket,
        page: Page,
    ) -> Result<(), String> {
        self.history_page(server, identity, channel, ticket, page)
    }
    fn server_history(
        &self,
        server: String,
        conversation: [u8; 16],
        ticket: HistoryTicket,
        cache: Option<CacheKey>,
        page: Page,
    ) -> Result<(), String> {
        Driver::server_history(self, server, conversation, (ticket, cache), page)
    }
    fn change_message(&self, change: ChangeRequest) -> Result<(), String> {
        Driver::change_message(self, change)
    }
    fn render_html(&self, server: String, generation: u64, key: RenderKey, source: String) {
        Driver::render_html(self, server, generation, key, source)
    }
    fn decode_avatar(&self, server: String, generation: u64, hash: String, encoded: Vec<u8>) {
        Driver::decode_avatar(self, server, generation, hash, encoded)
    }
    fn play_sound(&self, server: &str, sound: Sound) {
        Driver::play_sound(self, server, sound)
    }
    fn chime(&self, sound: Sound) {
        Driver::chime(self, sound)
    }
    fn search(
        &self,
        request: u64,
        scope: (Vec<String>, Option<String>),
        query: String,
        after: Option<nuschel_core::SearchCursor>,
    ) -> Result<(), String> {
        Driver::search(self, request, scope, query, after)
    }
}

/// What the driver reports that the client does not model (yet): the
/// crypto worker's identity and key-transparency state.
pub enum Native {
    /// The crypto worker's answer to [`Driver::identity`].
    OwnIdentity(Box<Result<nuschel_ipc::crypto::CryptoResponse, String>>),
    /// Identity and key transparency on one connection.
    Identity {
        server: String,
        generation: u64,
        event: Box<nuschel_session::identity::IdentityEvent>,
    },
    /// End-to-end conversations on one connection (`mls/1`).
    Mls {
        server: String,
        generation: u64,
        event: Box<nuschel_session::mls::MlsEvent>,
    },
    /// Nothing anyone needs.
    Ignored,
}

impl Event {
    /// The client's input for this event, or what the client does not model.
    pub fn into_input(self) -> Result<Input, Native> {
        Ok(match self {
            Event::OwnIdentity(result) => return Err(Native::OwnIdentity(Box::new(result))),
            Event::Protocol {
                server,
                generation,
                event,
            } => {
                let event = match server_event(event) {
                    Ok(event) => event,
                    Err(Some(Unmodelled::Identity(event))) => {
                        return Err(Native::Identity {
                            server,
                            generation,
                            event,
                        });
                    }
                    Err(Some(Unmodelled::Mls(event))) => {
                        return Err(Native::Mls {
                            server,
                            generation,
                            event,
                        });
                    }
                    Err(None) => return Err(Native::Ignored),
                };
                Input::Server {
                    server,
                    generation,
                    event,
                }
            }
            Event::Certificate {
                server,
                generation,
                sha256,
                changed,
                reason,
            } => Input::Certificate {
                server,
                generation,
                sha256,
                changed,
                reason,
            },
            Event::Disconnected {
                server,
                generation,
                reason,
                reject,
            } => Input::Disconnected {
                server,
                generation,
                reason,
                reject,
            },
            Event::Reconnecting {
                server,
                generation,
                attempt,
                delay_secs,
                reason,
            } => Input::Reconnecting {
                server,
                generation,
                attempt,
                delay_secs,
                reason,
            },
            Event::Gap {
                server,
                generation,
                from_ms,
                to_ms,
            } => Input::Gap {
                server,
                generation,
                from_ms,
                to_ms,
            },
            Event::AudioStatus {
                server,
                generation,
                message,
            } => Input::AudioStatus {
                server,
                generation,
                message,
            },
            Event::Message {
                server,
                generation,
                target,
                storage_key,
                peer,
                message,
            } => Input::Message {
                server,
                generation,
                target,
                storage_key,
                peer,
                message,
            },
            Event::History {
                server,
                ticket,
                before,
                messages,
                next_before,
            } => Input::History {
                server,
                ticket,
                before,
                messages,
                next_before,
            },
            Event::Sent {
                server,
                generation,
                target,
                body,
            } => Input::Sent {
                server,
                generation,
                target,
                body,
            },
            Event::HistoryWindow {
                server,
                ticket,
                page,
                messages,
                next_before,
                next_after,
            } => Input::HistoryWindow {
                server,
                ticket,
                page,
                messages,
                next_before,
                next_after,
            },
            Event::HistoryError(error) => Input::HistoryError(error),
            Event::Edited {
                server,
                conversation,
                seq,
                source,
                document,
                edited_ms,
            } => Input::Edited {
                server,
                conversation,
                seq,
                source,
                document,
                edited_ms,
            },
            Event::Deleted {
                server,
                conversation,
                seq,
                deletion,
            } => Input::Deleted {
                server,
                conversation,
                seq,
                deletion,
            },
            Event::ChangeFailed {
                server,
                delete,
                reason,
            } => Input::ChangeFailed {
                server,
                delete,
                reason,
            },
            Event::HistoryFailed {
                server,
                ticket,
                reason,
            } => Input::HistoryFailed {
                server,
                ticket,
                reason,
            },
            Event::Downgrade {
                server,
                generation,
                reason,
            } => Input::Downgrade {
                server,
                generation,
                reason,
            },
            Event::SearchResults { request, result } => Input::SearchResults { request, result },
            Event::HistoryRecovered => Input::HistoryRecovered,
            Event::AudioDevices {
                server,
                generation,
                inputs,
                outputs,
                input,
                output,
                echo,
                noise,
                reference,
            } => Input::AudioDevices {
                server,
                generation,
                inputs,
                outputs,
                input,
                output,
                echo,
                noise,
                reference,
            },
            Event::Echo {
                server,
                generation,
                erle_db,
                residual_likelihood,
            } => Input::Echo {
                server,
                generation,
                erle_db,
                residual_likelihood,
            },
            Event::AudioHealth {
                server,
                generation,
                health,
            } => Input::AudioHealth {
                server,
                generation,
                health,
            },
            Event::Rendered {
                server,
                generation,
                key,
                source,
                document,
            } => Input::Rendered {
                server,
                generation,
                key,
                source,
                document,
            },
            Event::Avatar {
                server,
                generation,
                hash,
                document,
            } => Input::Avatar {
                server,
                generation,
                hash,
                document,
            },
            Event::Talking {
                server,
                generation,
                session,
                talking,
            } => Input::Talking {
                server,
                generation,
                session,
                talking,
            },
        })
    }
}

/// What the session says that the client does not model yet.
enum Unmodelled {
    Identity(Box<nuschel_session::identity::IdentityEvent>),
    Mls(Box<nuschel_session::mls::MlsEvent>),
}

/// What the session said, as the client models it; identity and end-to-end
/// events (to the UI for now) and what nobody needs (the driver's own
/// business) are not.
fn server_event(event: SessionEvent) -> Result<ServerEvent, Option<Unmodelled>> {
    Ok(match event {
        SessionEvent::PeerCertificate(pin) => ServerEvent::PeerCertificate(pin),
        SessionEvent::Negotiated(negotiated) => ServerEvent::Negotiated {
            capabilities: negotiated.capabilities,
            policy: negotiated.policy,
        },
        SessionEvent::ChannelInfo { channel, info } => ServerEvent::ChannelInfo {
            channel,
            conversation: info.conversation.as_slice().try_into().ok(),
        },
        SessionEvent::Nuschel(NuschelEvent::Typing(typing)) => ServerEvent::Typing(typing),
        SessionEvent::Nuschel(NuschelEvent::DirectList(list)) => {
            ServerEvent::DirectList(list.directs)
        }
        SessionEvent::Nuschel(NuschelEvent::DirectOpened(opened)) => {
            ServerEvent::DirectOpened(opened)
        }
        SessionEvent::Nuschel(NuschelEvent::Error(error)) => ServerEvent::ExtensionError(error),
        SessionEvent::Nuschel(NuschelEvent::ReadMarker(marker)) => ServerEvent::ReadMarker(marker),
        SessionEvent::Identity(event) => return Err(Some(Unmodelled::Identity(Box::new(event)))),
        SessionEvent::Mls(event) => return Err(Some(Unmodelled::Mls(Box::new(event)))),
        SessionEvent::Nuschel(_)
        | SessionEvent::ChatPending(_)
        | SessionEvent::Text(_)
        | SessionEvent::Audio(_) => return Err(None),
        SessionEvent::ChatAcked { key, seq } => ServerEvent::ChatAcked { key, seq },
        SessionEvent::ChatFailed { key, reason } => ServerEvent::ChatFailed { key, reason },
        SessionEvent::ConversationState {
            channel,
            head,
            read,
        } => ServerEvent::ConversationState {
            channel,
            head,
            read,
        },
        SessionEvent::Channel(channel) => ServerEvent::Channel(channel),
        SessionEvent::ChannelRemoved(id) => ServerEvent::ChannelRemoved(id),
        SessionEvent::User(user) => ServerEvent::User(user),
        SessionEvent::UserRemoved(session) => ServerEvent::UserRemoved(session),
        SessionEvent::Connected { session, .. } => ServerEvent::Connected { session },
        SessionEvent::Welcome(text) => ServerEvent::Welcome(text),
        SessionEvent::PermissionDenied(text) => ServerEvent::PermissionDenied(text),
        SessionEvent::Notice(text) => ServerEvent::Notice(text),
        SessionEvent::Transport(stats) => ServerEvent::Transport(stats),
        SessionEvent::Permissions {
            channel,
            permissions,
            flush,
        } => ServerEvent::Permissions {
            channel,
            permissions,
            flush,
        },
        SessionEvent::Activity(activity) => ServerEvent::Activity(activity),
        SessionEvent::Limits(limits) => ServerEvent::Limits(limits),
        SessionEvent::Stats(stats) => ServerEvent::Stats(stats),
        SessionEvent::Suggest {
            push_to_talk,
            minimum_version,
            ..
        } => ServerEvent::Suggest {
            push_to_talk,
            minimum_version,
        },
        SessionEvent::ContextActions(actions) => ServerEvent::ContextActions(actions),
        SessionEvent::RegisteredUsers(users) => ServerEvent::RegisteredUsers(users),
        SessionEvent::Bans(bans) => ServerEvent::Bans(bans),
        SessionEvent::Acl(acl) => ServerEvent::Acl(acl),
        SessionEvent::UserNames(names) => ServerEvent::UserNames(names),
        SessionEvent::UnsupportedCodec { session } => ServerEvent::UnsupportedCodec {
            session,
            at_ms: connection::now_ms(),
        },
        SessionEvent::Outbox { sent, total } => ServerEvent::Outbox { sent, total },
    })
}

/// Your nicknames and friends as the settings keep them.
impl crate::names::Names for ClientSettings {
    fn nickname(&self, hash: Option<&str>) -> Option<&str> {
        ClientSettings::nickname(self, hash)
    }
    fn friend(&self, hash: Option<&str>) -> Option<&str> {
        ClientSettings::friend(self, hash)
    }
}

/// The client's preferences from the settings.
pub fn preferences(settings: &ClientSettings) -> crate::names::Preferences {
    crate::names::Preferences {
        nicknames: settings.nicknames.clone(),
        friends: settings.friends.clone(),
        notifications: settings.notifications,
        friend_notifications: settings.friend_notifications,
        send_typing: settings.send_typing,
        push_to_talk: settings.audio.transmit == TransmitMode::PushToTalk,
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheChange, CacheKey, Deletion, RecentChanges};
    use nuschel_core::{ChatMessage, MessageDocument, ServerRecord};

    fn record(seq: i64, body: &str) -> ServerRecord {
        ServerRecord {
            message: ChatMessage {
                server: "server".into(),
                channel: "3".into(),
                author: "Ada".into(),
                observed_at_ms: seq,
                body: body.into(),
            },
            seq,
            edited_ms: None,
            deleted_ms: None,
        }
    }

    #[test]
    fn a_page_on_its_way_does_not_bring_back_what_changed_meanwhile() {
        let cache = CacheKey {
            identity: "server".into(),
            channel: "3".into(),
        };
        let mut recent = RecentChanges::default();
        let edit = |body: &str, edited_ms| CacheChange::Edit {
            source: body.into(),
            document: MessageDocument { blocks: Vec::new() },
            edited_ms,
        };
        recent.note(&cache, 1, &edit("newer", 20));
        recent.note(&cache, 1, &edit("older", 10));
        let deletion = Deletion {
            at_ms: 30,
            moderator: false,
        };
        recent.note(&cache, 2, &CacheChange::Delete(deletion));
        let mut page = vec![
            record(1, "original"),
            record(2, "secret"),
            record(3, "other"),
        ];
        recent.patch(&mut page);
        assert_eq!(
            (page[0].message.body.as_str(), page[0].edited_ms),
            ("newer", Some(20))
        );
        assert_eq!(
            (page[1].message.body.as_str(), page[1].deleted_ms),
            ("", Some(30))
        );
        assert_eq!(
            (page[2].message.body.as_str(), page[2].edited_ms),
            ("other", None)
        );
        // A page that already has a newer edit keeps it.
        let mut newer = vec![record(1, "newest")];
        newer[0].edited_ms = Some(25);
        recent.patch(&mut newer);
        assert_eq!(newer[0].message.body, "newest");
    }
}
