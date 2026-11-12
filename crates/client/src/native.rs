// SPDX-License-Identifier: EUPL-1.2
//! The native driver: the SDK's platform services on Linux, Windows and
//! macOS, on tokio. It supervises one connection per server (`nuschl-session`)
//! with reconnects and backoff, runs the one media worker they share
//! ([`media`]: every server's voice mixed, the microphone to the talk
//! servers), renders
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
use nuschl_core::audio::{
    AudioDevice, DEFAULT_SERVER_BANDWIDTH, DEFAULT_VOICE_BITRATE, EchoSource, Sound, TransmitMode,
};
use nuschl_core::bitrate::{VoiceBitrate, voice_frame_fits};
use nuschl_core::{ChatMessage, ContentBlock, HistoryPage, MessageDocument};
use nuschl_ipc::{LiveMediaEvent, LiveMediaRequest};
use nuschl_protocol::nuschl::v1;
use nuschl_session::{
    Error,
    config::{ClientSettings, Profile, ServerBookmark},
    connection::{
        self, Command, ConnectOptions, IncomingText, NuschlEvent, NuschlRequest, PendingChat,
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

mod media;

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

/// Render a Nuschl server's history page like a local one: text in one
/// content worker, stock clients' HTML made inert first. Ids are seqs;
/// messages come in their current state (edited, or a tombstone); edit,
/// deletion and void rows only move the cursor. Also returns the records
/// that keep the page in local history (`cache`).
async fn render_server_page(
    page: v1::HistoryPage,
    cache: Option<&CacheKey>,
    unauthenticated: &[u64],
) -> (
    Vec<IncomingMessage>,
    (Option<i64>, Option<i64>),
    Vec<nuschl_core::ServerRecord>,
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
                && !nuschl_protocol::nuschl::is_mls_format(message.format)
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
        if let Some(cache) = cache.filter(|_| !unauthenticated.contains(&message.seq)) {
            records.push(nuschl_core::ServerRecord {
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
    Input(Input),
    /// The crypto worker's answer to [`Driver::identity`].
    OwnIdentity(Result<nuschl_ipc::crypto::CryptoResponse, String>),
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
    /// A message on a Nuschl server was edited (by its author, perhaps us).
    Edited {
        server: String,
        conversation: [u8; 16],
        seq: u64,
        source: String,
        document: MessageDocument,
        edited_ms: i64,
    },
    /// A message on a Nuschl server became a tombstone.
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
        result: Result<nuschl_core::SearchPage, String>,
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
    generation: u64,
    voice_route: std::sync::Arc<nuschl_session::connection::VoiceRoute>,
    commands: mpsc::Sender<Command>,
    sounds: mpsc::Sender<Sound>,
    task: tokio::task::JoinHandle<()>,
}
/// Synchronous UI callback scope only. No async task inherits this thread-local
/// value. The guard restores the prior scope even when a callback unwinds.
#[derive(Clone)]
struct ActionScope {
    id: u64,
    driver: usize,
    server: String,
    generation: u64,
    authority: nuschl_session::connection::CommandAuthority,
}
#[derive(Default)]
struct ActionScopes {
    entries: Vec<ActionScope>,
    blocked: usize,
    next_id: u64,
}
thread_local! {
    static ACTION_SCOPES: std::cell::RefCell<ActionScopes> = const { std::cell::RefCell::new(ActionScopes {
        entries: Vec::new(), blocked: 0, next_id: 0,
    }) };
}
#[must_use]
struct ActionScopeGuard {
    id: Option<u64>,
    not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl Drop for ActionScopeGuard {
    fn drop(&mut self) {
        ACTION_SCOPES.with(|scopes| {
            let mut scopes = scopes.borrow_mut();
            match self.id {
                Some(id) => scopes.entries.retain(|scope| scope.id != id),
                None => scopes.blocked = scopes.blocked.saturating_sub(1),
            }
        });
    }
}
fn action_scope(
    driver: usize,
    server: String,
    generation: u64,
    authority: nuschl_session::connection::CommandAuthority,
) -> ActionScopeGuard {
    let id = ACTION_SCOPES.with(|scopes| {
        let mut scopes = scopes.borrow_mut();
        if scopes.entries.len() >= 32 {
            scopes.blocked = scopes.blocked.saturating_add(1);
            return None;
        }
        scopes.next_id = scopes
            .next_id
            .checked_add(1)
            .expect("action scope IDs exhausted");
        let id = scopes.next_id;
        scopes.entries.push(ActionScope {
            id,
            driver,
            server,
            generation,
            authority,
        });
        Some(id)
    });
    ActionScopeGuard {
        id,
        not_send: std::marker::PhantomData,
    }
}
fn scoped_command(
    driver: usize,
    server: &str,
    command: &mut Option<Command>,
) -> Result<Option<(u64, Command)>, String> {
    ACTION_SCOPES.with(|scopes| {
        let scopes = scopes.borrow();
        if scopes.blocked != 0 {
            return Err("Too many nested actions.".into());
        }
        let mut matching = scopes.entries.iter().filter(|scope| scope.driver == driver);
        let Some(scope) = matching.next() else {
            return Ok(None);
        };
        // Reentrant SDK/UI callbacks may only retain the exact same origin.
        // Differing nested scopes fail closed rather than widening either one.
        if scope.server != server
            || matching.any(|other| {
                other.server != scope.server
                    || other.generation != scope.generation
                    || other.authority != scope.authority
            })
            || command.as_ref().and_then(Command::targets).is_none()
        {
            return Err("This action does not authorize that request.".into());
        }
        let authority = scope
            .authority
            .for_command(command.as_ref().expect("command present"))
            .ok_or("This action does not cover the requested targets.")?;
        Ok(Some((
            scope.generation,
            Command::Scoped {
                authority,
                command: Box::new(command.take().expect("command present")),
            },
        )))
    })
}

fn metadata_command(
    driver: usize,
    server: &str,
    metadata: nuschl_core::model::MetadataAuthority,
    request: Request,
) -> Result<(u64, Command), String> {
    use nuschl_session::connection::{CommandAuthority, MAX_ACTION_TARGETS};
    if !matches!(request, Request::RequestBlobs { .. })
        || metadata.users.len().saturating_add(metadata.channels.len()) > MAX_ACTION_TARGETS
    {
        return Err("Invalid metadata authority.".into());
    }
    let authority = CommandAuthority {
        own: metadata.own,
        subject: None,
        users: metadata.users,
        channels: metadata.channels,
        conversations: vec![],
        accounts: vec![],
    };
    let mut command = Some(request_command(request));
    let minimal = authority
        .for_command(command.as_ref().unwrap())
        .ok_or("Incomplete metadata authority.")?;
    let mut authority = minimal;
    if let Some((generation, scoped)) = scoped_command(driver, server, &mut command)? {
        let Command::Scoped {
            authority: callback,
            command: inner,
        } = scoped
        else {
            return Err("Missing callback authority.".into());
        };
        if generation != metadata.generation
            || callback.own != authority.own
            || callback.users != authority.users
            || callback.channels != authority.channels
        {
            return Err("Metadata and callback authority disagree.".into());
        }
        authority.subject = callback.subject;
        command = Some(*inner);
    }
    Ok((
        metadata.generation,
        Command::Scoped {
            authority,
            command: Box::new(command.unwrap()),
        },
    ))
}

fn with_action_scope<R>(
    driver: usize,
    server: String,
    generation: u64,
    authority: nuschl_session::connection::CommandAuthority,
    callback: impl FnOnce() -> R,
) -> R {
    let _scope = action_scope(driver, server, generation, authority);
    callback()
}
fn voice_intent(
    connections: &BTreeMap<String, Connection>,
    server: &str,
    generation: u64,
    command: Command,
) -> Result<Command, String> {
    let route = &connections
        .get(server)
        .filter(|connection| connection.generation == generation)
        .ok_or("Voice target belongs to an older connection.")?
        .voice_route;
    match command {
        Command::Whisper { id, targets, .. } => Ok(Command::Whisper {
            id,
            targets,
            revision: route.suspend(),
        }),
        Command::SelectVoiceTarget(id) => Ok(Command::SelectVoiceTargetAt {
            id,
            revision: route.suspend(),
        }),
        _ => Err("Expected an explicit voice selection.".into()),
    }
}
fn enqueue_scoped(
    connections: &BTreeMap<String, Connection>,
    server: &str,
    generation: u64,
    command: Command,
) -> Result<(), String> {
    let connection = connections
        .get(server)
        .filter(|connection| connection.generation == generation)
        .ok_or("This action belongs to an older connection.")?;
    if !matches!(command, Command::Scoped { .. }) {
        return Err("Missing action authority.".into());
    }
    if matches!(&command,Command::Scoped { command,.. } if matches!(command.as_ref(),
        Command::SetVoiceTarget { .. } | Command::SelectVoiceTarget(_) | Command::Whisper { revision:0,.. }))
    {
        return Err("Voice selection requires a current local intent revision.".into());
    }

    connection
        .commands
        .try_send(command)
        .map_err(|_| "Connection is busy or has ended; reconnect to try again.".into())
}

pub struct Driver {
    handle: Handle,
    profile: Profile,
    /// The crypto worker, shared by every connection (identity and key transparency).
    crypto: nuschl_session::identity::CryptoHandle,
    sender: mpsc::Sender<Event>,
    storage: mpsc::Sender<StorageCommand>,
    content: mpsc::Sender<ContentWork>,
    connections: std::sync::Mutex<BTreeMap<String, Connection>>,
    generation: std::sync::atomic::AtomicU64,
    settings: watch::Sender<ClientSettings>,
    voice_keys: watch::Sender<VoiceKeys>,
    /// The media worker every connection shares.
    media: media::Hub,
    render_permits: std::sync::Arc<tokio::sync::Semaphore>,
    storage_task: tokio::task::JoinHandle<()>,
    content_task: tokio::task::JoinHandle<()>,
    media_task: tokio::task::JoinHandle<()>,
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
    /// A Nuschl server's messages by seq (a history page): kept, or
    /// replaced where already kept.
    StoreServer {
        records: Vec<nuschl_core::ServerRecord>,
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
        after: Option<nuschl_core::SearchCursor>,
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
            nuschl_session::identity::CryptoHandle::start(
                nuschl_session::workers::sibling("nuschl-crypto-worker")
                    .unwrap_or_else(|_| "nuschl-crypto-worker".into()),
                profile.keys_dir().to_owned(),
            )
        };
        let settings = watch::Sender::new(settings);
        let voice_keys = watch::Sender::new(VoiceKeys::default());
        let (media, media_task) = media::start(
            &handle,
            sender.clone(),
            (settings.subscribe(), voice_keys.subscribe()),
        );
        let backend = Self {
            handle,
            crypto,
            profile,
            sender,
            storage,
            content,
            connections: Default::default(),
            generation: Default::default(),
            settings,
            voice_keys,
            media,
            render_permits: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
            storage_task,
            content_task,
            media_task,
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
            self.media.send(media::Control::Detach {
                server: server.id.clone(),
                generation: old.generation,
            });
        }
        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let (commands, receiver) = mpsc::channel(32);
        let (sounds, sound_requests) = mpsc::channel(8);
        let id = server.id.clone();
        let voice_route = std::sync::Arc::new(nuschl_session::connection::VoiceRoute::default());
        let task = self.handle.spawn(connection_loop(
            (self.profile.clone(), self.crypto.clone()),
            server,
            (password, deafened),
            generation,
            voice_route.clone(),
            (receiver, sound_requests),
            (self.sender.clone(), self.media.clone()),
            (self.content.clone(), self.storage.clone()),
            (self.settings.subscribe(), self.voice_keys.subscribe()),
        ));
        connections.insert(
            id,
            Connection {
                generation,
                voice_route,
                commands,
                sounds,
                task,
            },
        );
        Ok(generation)
    }
    /// A page of a Nuschl server's history: before a seq (or the latest),
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
            Command::Nuschl(NuschlRequest::History(v1::HistoryRequest {
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
    /// Edit (`content`) or delete a message on a Nuschl server. The answer
    /// arrives as `Event::Edited`/`Event::Deleted` or `Event::ChangeFailed`.
    pub fn change_message(&self, change: ChangeRequest) -> Result<(), String> {
        let request_id = request_id();
        let conversation = change.conversation.to_vec();
        let request = match &change.content {
            Some(content) => NuschlRequest::Edit(v1::ChatEdit {
                request_id,
                conversation,
                target: change.seq,
                format: v1::Format::Text as i32,
                content: content.clone(),
            }),
            None => NuschlRequest::Delete(v1::ChatDelete {
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
        let result = self.command(&server, Command::Nuschl(request));
        if result.is_err() {
            changes().remove(&request_id);
        }
        result
    }
    /// A request to the crypto worker that concerns no server (Settings → Identity).
    pub fn identity(&self, request: nuschl_ipc::crypto::CryptoRequest) {
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
        let mut command = Some(command);
        if let Some((generation, command)) =
            scoped_command(self as *const Self as usize, server, &mut command)?
        {
            return self.command_scoped(server, generation, command);
        }
        let command = command.expect("unscoped command retained");
        if matches!(
            command,
            Command::Whisper { .. }
                | Command::SetVoiceTarget { .. }
                | Command::SelectVoiceTarget(_)
                | Command::SelectVoiceTargetAt { .. }
        ) {
            return Err("Use an explicit generation-bound voice selection.".into());
        }

        self.connections()
            .get(server)
            .ok_or("Server is not connected.")?
            .commands
            .try_send(command)
            .map_err(|_| "Connection is busy or has ended; reconnect to try again.".into())
    }
    /// A new explicit local voice intent suspends before trying the bounded
    /// queue. Its revision prevents an older queued action from resuming audio.
    pub fn voice_command(
        &self,
        server: &str,
        generation: u64,
        command: Command,
    ) -> Result<(), String> {
        let command = voice_intent(&self.connections(), server, generation, command)?;
        let mut command = Some(command);
        if let Some((expected, command)) =
            scoped_command(self as *const Self as usize, server, &mut command)?
        {
            if expected != generation {
                return Err("Voice action belongs to an older connection.".into());
            }
            return self.command_scoped(server, generation, command);
        }
        let command = command.unwrap();
        if !matches!(command, Command::SelectVoiceTargetAt { id: 0, .. }) {
            return Err("Whisper requires original target authority.".into());
        }
        let connections = self.connections();
        connections
            .get(server)
            .filter(|connection| connection.generation == generation)
            .ok_or("Voice connection changed.")?
            .commands
            .try_send(command)
            .map_err(|_| "Connection is busy; voice is suspended. Select a target again.".into())
    }
    /// Restrict synchronous SDK/platform requests made by one UI callback.
    /// The envelope is copied before enqueue, never retained as ambient worker authority.
    pub fn with_action_scope<R>(
        &self,
        server: String,
        generation: u64,
        authority: nuschl_session::connection::CommandAuthority,
        callback: impl FnOnce() -> R,
    ) -> R {
        with_action_scope(
            self as *const Self as usize,
            server,
            generation,
            authority,
            callback,
        )
    }
    /// Enqueue only on the original explicit driver connection. The actor also
    /// checks observed presence lifetimes across automatic reconnects.
    pub fn command_scoped(
        &self,
        server: &str,
        generation: u64,
        command: Command,
    ) -> Result<(), String> {
        enqueue_scoped(&self.connections(), server, generation, command)
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
        after: Option<nuschl_core::SearchCursor>,
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
            if let Ok(document) = nuschl_ipc::host::decode_avatar(worker, &encoded).await {
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
    /// Apply new preferences to every live connection and the media worker.
    pub fn apply_settings(&self, settings: ClientSettings) {
        self.settings.send_replace(settings);
    }
    /// Play a sound effect for this server, unless it is deafened (see `connect_once`).
    pub fn play_sound(&self, server: &str, sound: Sound) {
        if let Some(connection) = self.connections().get(server) {
            let _ = connection.sounds.try_send(sound);
        }
    }
    /// Play a sound effect that no connection asks for, as after a
    /// disconnect: on the media worker, or one that opens only the output.
    pub fn chime(&self, sound: Sound) {
        self.media.send(media::Control::Chime(sound));
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
            // Before a chime for it, so the worker knows whether it still runs.
            self.media.send(media::Control::Detach {
                server: server.to_string(),
                generation: connection.generation,
            });
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
fn request_command(request: Request) -> Command {
    match request {
        Request::Join(channel) => Command::Join(channel),
        Request::OpenEncrypted {
            conversation,
            channel,
            peer,
        } => Command::Mls(match (channel, peer) {
            (true, _) => nuschl_session::mls::MlsCommand::OpenChannel { conversation },
            (false, Some(user_id)) => nuschl_session::mls::MlsCommand::OpenDirect { user_id },
            _ => nuschl_session::mls::MlsCommand::Sync { conversation },
        }),
        Request::SendEncrypted { conversation, text } => {
            Command::Mls(nuschl_session::mls::MlsCommand::SendText { conversation, text })
        }
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
            Command::Nuschl(NuschlRequest::ReadMarker(v1::ReadMarker {
                conversation: conversation.to_vec(),
                seq,
            }))
        }
        Request::Typing { conversation } => Command::Nuschl(NuschlRequest::Typing(v1::Typing {
            conversation: conversation.to_vec(),
            session: 0,
        })),
        Request::OpenDirect {
            request_id,
            user_id,
        } => Command::Nuschl(NuschlRequest::OpenDirect(v1::OpenDirect {
            request_id,
            user_id,
        })),
        Request::SetTokens(tokens) => Command::SetTokens(tokens),
        Request::CreateShareLink {
            request_id,
            channel,
            expires_at,
            access_token,
        } => Command::Nuschl(NuschlRequest::ShareLinkCreate(v1::ShareLinkCreate {
            request_id,
            channel,
            expires_at,
            access_token,
        })),
        Request::ResolveShareLink { request_id, token } => {
            Command::Nuschl(NuschlRequest::ShareLinkResolve(v1::ShareLinkResolve {
                request_id,
                token: token.to_vec(),
            }))
        }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        for connection in self.connections().values() {
            connection.task.abort();
        }
        self.storage_task.abort();
        self.content_task.abort();
        self.media_task.abort();
    }
}

/// What to restore when a dropped connection is re-established.
#[derive(Default)]
struct Restore {
    voice_route: std::sync::Arc<nuschl_session::connection::VoiceRoute>,
    channel: Option<u32>,
    muted: bool,
    deafened: bool,
    connected: bool,
    disconnected_at_ms: Option<i64>,
    /// Our messages a Nuschl server has not acknowledged yet; sent again
    /// with their keys after a reconnect.
    unacked: Vec<PendingChat>,
}

enum Outcome {
    Ended(Result<Result<(), Error>, tokio::task::JoinError>),
    /// The client dropped this connection.
    Closed,
    /// Apply a changed address-privacy policy in Hello before new presence.
    PrivacyReconnect,
}

/// Supervise one server connection: reconnect with backoff after transient
/// failures, restoring channel and mute/deafen, and report gaps in history.
#[allow(clippy::too_many_arguments)]
async fn connection_loop(
    (profile, crypto): (Profile, nuschl_session::identity::CryptoHandle),
    server: ServerBookmark,
    (password, deafened): (Option<String>, bool),
    generation: u64,
    voice_route: std::sync::Arc<nuschl_session::connection::VoiceRoute>,
    (mut commands, mut sounds): (mpsc::Receiver<Command>, mpsc::Receiver<Sound>),
    (events, hub): (mpsc::Sender<Event>, media::Hub),
    (content, storage): (mpsc::Sender<ContentWork>, mpsc::Sender<StorageCommand>),
    (mut settings, mut voice_keys): (watch::Receiver<ClientSettings>, watch::Receiver<VoiceKeys>),
) {
    let id = server.id.clone();
    // A fresh connection rejoins the channel you were last in, like a reconnect.
    let mut restore = Restore {
        voice_route,
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
            (&events, &hub),
            (&content, &storage),
            (&mut settings, &mut voice_keys),
            &mut restore,
        )
        .await;
        let result = match outcome {
            Outcome::Closed => return,
            Outcome::PrivacyReconnect => {
                restore
                    .disconnected_at_ms
                    .get_or_insert_with(connection::now_ms);
                let _ = events
                    .send(Event::Reconnecting {
                        server: id.clone(),
                        generation,
                        attempt: 0,
                        delay_secs: 0,
                        reason: "Applying relay-only connection privacy".into(),
                    })
                    .await;
                continue;
            }
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
        Some(host) => format!("Nuschl on {host}"),
        None => format!("Nuschl ({})", std::env::consts::OS),
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_once(
    (profile, crypto): (&Profile, &nuschl_session::identity::CryptoHandle),
    server: &ServerBookmark,
    password: Option<String>,
    generation: u64,
    (commands, sounds): (&mut mpsc::Receiver<Command>, &mut mpsc::Receiver<Sound>),
    (events, hub): (&mpsc::Sender<Event>, &media::Hub),
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
        .map_err(|error| format!("Nuschl's server security records could not be read: {error}"));
    let options = match profile
        .identity()
        .map_err(|error| error.to_string())
        .and_then(|identity| Ok((identity, required?)))
    {
        Ok((identity, required)) => ConnectOptions {
            voice_route: restore.voice_route.clone(),
            server: server.clone(),
            identity,
            password,
            force_tcp: settings.borrow().force_tcp,
            relay_only: settings.borrow().relay_only,
            muted: restore.muted,
            deafened: restore.deafened,
            channel: restore.channel,
            resend: restore.unacked.clone(),
            message_rate: Default::default(),
            required,
            crypto: Some(nuschl_session::identity::IdentitySetup {
                crypto: crypto.clone(),
                device_name: device_name(),
                recovery: settings.borrow().recovery_key,
                witnessing: settings.borrow().key_transparency.clone(),
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
    let relay_only = options.relay_only;
    let mut session = tokio::spawn(connection::run(options, receiver, session_events));
    let mut peer: Option<String> = None;
    let nonce = format!("{}:{generation}:{}", id, connection::now_ms());
    let mut muted = restore.muted;
    let mut deafened = restore.deafened;
    let mut voice_revision = 0_u64;
    let mut voice_allowed = true;
    let mut sequence = 0;
    let mut content_gap = false;
    // The budget this server advertised (ServerSync, then ServerConfig) and
    // the requested target: this connection's share of the microphone is
    // encoded and admitted at their effective rate (nuschl_core::bitrate).
    let mut budget = DEFAULT_SERVER_BANDWIDTH;
    let mut requested = settings.borrow().voice_bitrate;
    let mut bitrate = VoiceBitrate::resolve(requested, budget).effective;
    // Our share of the app's microphone, once connected; detached however
    // this ends.
    let mut voice: Option<(mpsc::Receiver<media::Outgoing>, media::Attached)> = None;
    let mut finished = false;
    let mut outcome = None;
    let mut own_session = None;
    let mut own_registration = None;
    // Where each channel's conversation is kept in local history.
    let mut conversation_keys = BTreeMap::<[u8; 16], String>::new();
    // Timelines here are the server's history (the extension's history/1).
    let mut server_history = false;
    let mut encrypted_server = false;
    // A channel can be opened before registration/key publication completes.
    // Retry the latest visible conversation when the worker uploads its keys.
    let mut pending_encrypted_open = None;
    let mut mls_keys_ready = false;
    let mut hashes = BTreeMap::<u32, Option<String>>::new();
    let mut loss_percent = 5_u8;
    // Our own voice is on the air (no terminator sent yet).
    let mut transmitting = false;
    let mut speaking = BTreeMap::<u32, tokio::time::Instant>::new();
    let mut talk_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    talk_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Requests from before a reconnection are stale.
    while sounds.try_recv().is_ok() {}
    loop {
        tokio::select! {
            changed = settings.changed() => {
                // Volumes apply to the next packet; the worker follows audio
                // settings itself.
                if changed.is_err() { break; }
                let (relay, wanted) = {
                    let current = settings.borrow_and_update();
                    (current.relay_only, current.voice_bitrate)
                };
                if relay != relay_only {
                    // A full command queue must not silently lose an opt-in.
                    // Reconnect even when ordinary auto-reconnect is disabled.
                    outcome = Some(Outcome::PrivacyReconnect);
                    break;
                }
                if wanted != requested {
                    // Only the encoder follows: a request (64k or more) cannot
                    // open or close capture, so the revision and its privacy
                    // epochs stay. Frames queued at the old rate that exceed
                    // the new allowance are refused below, never resent.
                    requested = wanted;
                    let next = VoiceBitrate::resolve(requested, budget).effective;
                    if (next == 0) != (bitrate == 0) { voice_revision = voice_revision.wrapping_add(1); }
                    if next != bitrate {
                        bitrate = next;
                        if voice.is_some() {
                            hub.send(media::Control::Update { server: id.clone(), generation,
                                link: media::Link { revision: voice_revision, voice_allowed, muted, deafened, bitrate, loss_percent }, cue: None });
                        }
                    }
                }
            },
            changed = voice_keys.changed() => {
                if changed.is_err() { break; }
                let keys = *voice_keys.borrow_and_update();
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
                hub.send(media::Control::Sound(sound));
            },
            Some(command) = commands.recv() => {
                if let Command::Mls(open @ (nuschl_session::mls::MlsCommand::OpenChannel { .. } | nuschl_session::mls::MlsCommand::OpenDirect { .. })) = match &command { Command::Scoped { command, .. } => command.as_ref(), command => command } {
                    let authority = match &command { Command::Scoped { authority, .. } => Some(authority.clone()), _ => None };
                    pending_encrypted_open = Some((open.clone(), authority));
                    if !mls_keys_ready { continue; }
                }
                if let Command::SetSelf { muted: mute, deafened: deaf } = &command {
                    voice_revision = voice_revision.wrapping_add(1);
                    let cue = crate::sounds::self_state((muted, deafened), (*mute || *deaf, *deaf));
                    muted = *mute || *deaf; deafened = *deaf;
                    if muted && std::mem::take(&mut transmitting) {
                        let _ = session_commands.try_send(silent_terminator(&mut sequence));
                        if let Some(session) = own_session { mark_talking(events, &id, generation, &mut speaking, session, false).await; }
                    }
                    // The worker closes the microphone (a new process) once no
                    // server gets it, and stops once every server is deafened.
                    let link = media::Link { revision: voice_revision, voice_allowed, muted, deafened, bitrate, loss_percent };
                    if voice.is_some() {
                        hub.send(media::Control::Update { server: id.clone(), generation, link, cue });
                    } else if let Some(sound) = cue {
                        hub.send(media::Control::Chime(sound));
                    }
                }
                if session_commands.send(command).await.is_err() { break; }
            },
            outgoing = async { voice.as_mut().unwrap().0.recv().await }, if voice.is_some() => {
                let Some(media::Outgoing { revision, opus, end }) = outgoing else { voice = None; continue };
                // Only unmuted, as the worker's supervisor routes it; nothing
                // captured before a mute leaves.
                if !voice_allowed || revision != voice_revision || muted || deafened || voice_keys.borrow().mute || !voice_frame_fits(bitrate, opus.len()) { continue; }
                let packet = nuschl_protocol::voice::VoicePacket { sender: 0, sequence, opus, end, volume: 1. };
                sequence += 2;
                transmitting = !end;
                let _ = session_commands.try_send(Command::Voice { target: 0, packet });
                if let Some(session) = own_session { mark_talking(events, &id, generation, &mut speaking, session, !end).await; }
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
                SessionEvent::Nuschl(page_event @ (NuschlEvent::Page(_) | NuschlEvent::DecryptedPage { .. })) => {
                    let (page, decrypted, unauthenticated) = match page_event {
                        NuschlEvent::Page(page) => (page, false, Vec::new()),
                        NuschlEvent::DecryptedPage { page, unauthenticated } => (page, true, unauthenticated),
                        _ => unreachable!(),
                    };
                    if encrypted_server && !decrypted {
                        let _ = session_commands.send(Command::Mls(nuschl_session::mls::MlsCommand::DecryptPage { page })).await;
                        continue;
                    }
                    let asked = server_pages().remove(&page.request_id);
                    if let Some((server, ticket, asked, cache)) = asked {
                        let (events, storage) = (events.clone(), storage.clone());
                        tokio::spawn(async move {
                            let (messages, (next_before, next_after), records) = render_server_page(page, cache.as_ref(), &unauthenticated).await;
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
                SessionEvent::Nuschl(NuschlEvent::Ack(ack)) if changes().contains_key(&ack.request_id) => {
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
                SessionEvent::Nuschl(NuschlEvent::Error(error)) => {
                    let asked = server_pages().remove(&error.request_id);
                    let change = changes().remove(&error.request_id);
                    match (asked, change) {
                        (Some((server, ticket, _, _)), _) => { let _ = events.send(Event::HistoryFailed { server, ticket, reason: error.detail }).await; },
                        (None, Some(change)) => { let _ = events.send(Event::ChangeFailed { server: change.server, delete: change.content.is_none(), reason: error.detail }).await; },
                        (None, None) => { let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::Nuschl(NuschlEvent::Error(error)) }).await; },
                    }
                },
                SessionEvent::Nuschl(NuschlEvent::Chat(chat)) => {
                    // New messages arrive as `Text`; edits and deletions here.
                    let Some(message) = chat.message else { continue };
                    let Some(conversation) = nuschl_protocol::nuschl::id(&chat.conversation) else { continue };
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
                    if let Some(conversation) = nuschl_protocol::nuschl::id(&info.conversation)
                        && conversation_keys.len() < 4096
                    {
                        conversation_keys.insert(conversation, channel.to_string());
                    }
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::ChannelInfo { channel, info } }).await;
                },
                SessionEvent::Negotiated(negotiated) => {
                    voice_allowed = !negotiated.capabilities.iter().any(|capability| capability == "e2ee/1");
                    if !voice_allowed {
                        voice_revision = voice_revision.wrapping_add(1);
                        if voice.is_some() {
                            hub.send(media::Control::Update { server: id.clone(), generation,
                                link: media::Link { revision: voice_revision, voice_allowed, muted, deafened, bitrate, loss_percent }, cue: None });
                        }
                        let _ = events.send(Event::AudioStatus { server: id.clone(), generation,
                            message: "End-to-end voice is not available yet on this server.".into() }).await;
                    }
                    server_history = negotiated.capabilities.iter().any(|capability| capability == "history/1");
                    encrypted_server = negotiated.capabilities.iter().any(|capability| capability == "e2ee/1");
                    // Remember what this server offers: a weaker answer later fails.
                    let raised = profile.policy()
                        .and_then(|mut store| store.raise(&server.host, server.port, negotiated.nuschl, &negotiated.capabilities));
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
                    budget = max_bandwidth;
                    bitrate = VoiceBitrate::resolve(requested, budget).effective;
                    let (sender, receiver) = mpsc::channel(64);
                    let link = media::Link { revision: voice_revision, voice_allowed, muted, deafened, bitrate, loss_percent };
                    hub.send(media::Control::Attach { server: id.clone(), generation, link, voice: sender });
                    voice = Some((receiver, media::Attached { hub: hub.clone(), server: id.clone(), generation }));
                    if !deafened { hub.send(media::Control::Sound(Sound::Connected)); }
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::Connected { session: own_session, max_bandwidth } }).await;
                },
                SessionEvent::Limits(limits) => {
                    if let Some(max_bandwidth) = limits.max_bandwidth {
                        budget = max_bandwidth;
                        let next = VoiceBitrate::resolve(requested, budget).effective;
                        if (next == 0) != (bitrate == 0) { voice_revision = voice_revision.wrapping_add(1); }
                        bitrate = next;
                        if voice.is_some() {
                            hub.send(media::Control::Update { server: id.clone(), generation,
                                link: media::Link { revision: voice_revision, voice_allowed, muted, deafened, bitrate, loss_percent }, cue: None });
                        }
                    }
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::Limits(limits) }).await;
                },
                SessionEvent::Audio(packet) => {
                    if !voice_allowed { continue; }
                    let hash = hashes.get(&packet.sender).cloned().flatten();
                    let (muted_locally, volume) = {
                        let current = settings.borrow();
                        (current.locally_muted(hash.as_deref()), current.volume_for(hash.as_deref()) * current.server_volume(&id))
                    };
                    if muted_locally { continue; }
                    mark_talking(events, &id, generation, &mut speaking, packet.sender, !packet.end).await;
                    if !deafened {
                        hub.play(media::Playback { server: id.clone(), generation, sender: packet.sender, sequence: packet.sequence, opus: packet.opus, end: packet.end, volume: packet.volume * volume });
                    }
                },
                SessionEvent::User(user) => {
                    hashes.insert(user.session, user.certificate_hash.clone());
                    if Some(user.session) == own_session {
                        restore.channel = Some(user.channel);
                        if own_registration != user.user_id && user.user_id.is_some() {
                            own_registration = user.user_id;
                            let _ = session_commands.send(Command::Identity(nuschl_session::identity::IdentityCommand::Refresh)).await;
                        }
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
                        if voice.is_some() {
                            let link = media::Link { revision: voice_revision, voice_allowed, muted, deafened, bitrate, loss_percent };
                            hub.send(media::Control::Update { server: id.clone(), generation, link, cue: None });
                        }
                    }
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::Transport(stats) }).await;
                },
                SessionEvent::Mls(event) => {
                    use nuschl_session::mls::MlsEvent;
                    use nuschl_ipc::crypto::Received;
                    match &event {
                        MlsEvent::KeyPackages { .. } => {
                            mls_keys_ready = true;
                            if let Some((open, authority)) = pending_encrypted_open.clone() {
                                let command = Command::Mls(open);
                                let command = match authority { Some(authority) => Command::Scoped { authority, command: Box::new(command) }, None => command };
                                let _ = session_commands.send(command).await;
                            }
                        }
                        MlsEvent::Conversation { view, peer } => {
                            if view.pinned && view.active && pending_encrypted_open.as_ref().is_some_and(|(pending, _)| match pending {
                                nuschl_session::mls::MlsCommand::OpenChannel { conversation } => view.conversation.as_slice() == conversation,
                                nuschl_session::mls::MlsCommand::OpenDirect { user_id } => *peer == Some(*user_id),
                                _ => false,
                            }) { pending_encrypted_open = None; }

                            if let Some(conversation) = nuschl_protocol::nuschl::id(&view.conversation) {
                                let state = encryption_state(view);
                                let _ = events.send(Event::Input(Input::Server { server: id.clone(), generation, event: ServerEvent::Encryption { conversation, state } })).await;
                            }
                        }
                        MlsEvent::PlaintextRefused { conversation, .. } => {
                            let _ = events.send(Event::Input(Input::Server { server: id.clone(), generation, event: ServerEvent::EncryptionGap { conversation: *conversation, reason: "The server supplied unencrypted content in an encrypted conversation; it was refused.".into(), at_ms: connection::now_ms() } })).await;
                        }
                        MlsEvent::Sent { conversation, seq, key, text } => {
                            let target = conversation_keys.get(conversation).and_then(|key| key.parse().ok()).map(TextTarget::Channel).unwrap_or(TextTarget::DirectConversation(*conversation));
                            let _ = storage.send(StorageCommand::Acked { key: *key, seq: *seq }).await;
                            let _ = events.send(Event::Sent { server: id.clone(), generation, target, body: text.clone() }).await;
                        }
                        MlsEvent::NotSent { reason, .. } | MlsEvent::Failed(reason) => {
                            let _ = events.send(Event::Input(Input::SendFailed { server: id.clone(), generation, reason: reason.clone() })).await;
                        }
                        MlsEvent::Received { conversation, received, author: _ } => {
                            let visible = match received {
                                Received::Message { seq, text, sent_at, .. } => Some((*seq, text.clone(), (*sent_at).min(i64::MAX as u64) as i64, false)),
                                Received::Own { seq, text, .. } => Some((*seq, text.clone(), connection::now_ms(), true)),
                                Received::Undecryptable { reason, .. } | Received::Refused { reason, .. } => {
                                    let _ = events.send(Event::Input(Input::Server { server: id.clone(), generation, event: ServerEvent::EncryptionGap { conversation: *conversation, reason: format!("Unable to decrypt a message: {reason}"), at_ms: connection::now_ms() } })).await;
                                    None
                                },
                                _ => None,
                            };
                            if let (Some((seq, body, observed_at_ms, outgoing)), Some(peer)) = (visible, peer.as_ref()) {
                                let target = conversation_keys.get(conversation).and_then(|key| key.parse().ok()).map(TextTarget::Channel).unwrap_or(TextTarget::DirectConversation(*conversation));
                                let author = if outgoing { server.username.clone() } else if let Received::Message { sender, .. } = received { nuschl_session::mls::authenticated_sender_label(sender) } else { "Encrypted member".into() };
                                let text = IncomingText { target, author, peer: None, body, html: false, outgoing, observed_at_ms, seq: Some(seq), pending: None };
                                let job = ContentJob { server: id.clone(), generation, identity: server.history_identity(peer), nonce: nonce.clone(), text, archived: true };
                                if content.send(ContentWork::Text(Box::new(job))).await.is_err() { break; }
                            }
                        }
                        _ => {}
                    }
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::Mls(event) }).await;
                },
                SessionEvent::Text(text) => if let Some(peer) = &peer {
                    if text.outgoing { let _ = events.send(Event::Sent { server: id.clone(), generation, target: text.target.clone(), body: text.body.clone() }).await; }
                    let archived = server_history && matches!(text.target, TextTarget::Channel(_) | TextTarget::DirectConversation(_));
                    let job = ContentJob { server: id.clone(), generation, identity: server.history_identity(peer), nonce: nonce.clone(), text, archived };
                    if content.try_send(ContentWork::Text(Box::new(job))).is_err() {
                        if !content_gap { let _ = events.send(Event::HistoryError("Chat processing is overloaded; some received messages could not be saved.".into())).await; content_gap = true; }
                    } else { content_gap = false; }
                },
                SessionEvent::Welcome(body) => {
                    let (_, document) = prepare(&body, true).await;
                    let _ = events.send(Event::Input(Input::Server {
                        server: id.clone(), generation,
                        event: ServerEvent::Welcome { source: body, document },
                    })).await;
                },
                event => { let _ = events.send(Event::Protocol { server: id.clone(), generation, event }).await; },
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
    let packet = nuschl_protocol::voice::VoicePacket {
        sender: 0,
        sequence: *sequence,
        opus: nuschl_core::audio::SILENT_OPUS.to_vec(),
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
        std::sync::LazyLock::new(|| workers::sibling("nuschl-content-worker").ok());
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
        Some(worker) => nuschl_ipc::host::prepare_content(worker, source, html)
            .await
            .ok(),
        None => None,
    };
    match prepared {
        Some(prepared) => (prepared.source, prepared.document),
        None => (source.into(), plain(source)),
    }
}
/// Many messages, prepared in one content worker (see [`content_loop`]).
async fn prepare_many(messages: Vec<(String, bool)>) -> Vec<(String, MessageDocument)> {
    let prepared = match content_worker() {
        Some(worker) if !messages.is_empty() => {
            nuschl_ipc::host::prepare_contents(worker, &messages).await
        }
        _ => Vec::new(),
    };
    messages
        .into_iter()
        .zip(prepared.into_iter().chain(std::iter::repeat_with(|| None)))
        .map(|((source, _), prepared)| match prepared {
            Some(prepared) => (prepared.source, prepared.document),
            None => {
                let document = plain(&source);
                (source, document)
            }
        })
        .collect()
}

/// The most received messages and changes prepared in one content worker.
const CONTENT_BATCH: usize = 64;

/// Received messages and edits, rendered by the sandboxed content worker and
/// handed to storage in order. Whatever has queued up meanwhile is prepared
/// together: a burst costs one worker start instead of one per message, so a
/// slow machine keeps up with it rather than dropping what overflows the
/// queue in front of this loop.
async fn content_loop(
    mut jobs: mpsc::Receiver<ContentWork>,
    storage: mpsc::Sender<StorageCommand>,
    events: mpsc::Sender<Event>,
) {
    let mut batch = Vec::with_capacity(CONTENT_BATCH);
    while jobs.recv_many(&mut batch, CONTENT_BATCH).await > 0 {
        let texts: Vec<_> = batch
            .iter()
            .filter_map(|work| match work {
                ContentWork::Text(job) => Some((job.text.body.clone(), job.text.html)),
                ContentWork::Change(job) => match &job.change {
                    ChangeKind::Edit { content, .. } => Some((content.clone(), false)),
                    ChangeKind::Delete(_) => None,
                },
            })
            .collect();
        let mut prepared = prepare_many(texts).await.into_iter();
        for work in batch.drain(..) {
            let command = match work {
                ContentWork::Text(job) => {
                    let (source, document) = prepared
                        .next()
                        .unwrap_or_else(|| (job.text.body.clone(), plain(&job.text.body)));
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
                            let (source, document) = prepared
                                .next()
                                .unwrap_or_else(|| (content.clone(), plain(&content)));
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
                return;
            }
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
    fn patch(&self, records: &mut [nuschl_core::ServerRecord]) {
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
    // Our own messages on Nuschl servers, kept before the server stored
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
                // A Nuschl server's message is kept once by its seq (a page
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
                            let mut records = vec![nuschl_core::ServerRecord {
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
                // On a Nuschl server the timeline counts in the server's seqs;
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
            Some(worker) => nuschl_ipc::host::render_many(worker, &sources).await,
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
    audio: &nuschl_core::audio::AudioSettings,
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
    audio: nuschl_core::audio::AudioSettings,
    volume: f32,
    host: Option<String>,
    sound: Sound,
) {
    let Ok(mut worker) = Media::start() else {
        return;
    };
    configure(&worker, &audio, (DEFAULT_VOICE_BITRATE, 5), host);
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
        self.command(server, request_command(request))
    }
    fn request_metadata(
        &self,
        server: &str,
        authority: nuschl_core::model::MetadataAuthority,
        request: Request,
    ) -> Result<(), String> {
        let (generation, command) =
            metadata_command(self as *const Self as usize, server, authority, request)?;
        self.command_scoped(server, generation, command)
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
        after: Option<nuschl_core::SearchCursor>,
    ) -> Result<(), String> {
        Driver::search(self, request, scope, query, after)
    }
}

/// What the driver reports that the client does not model (yet): the
/// crypto worker's identity and key-transparency state.
pub enum Native {
    /// The crypto worker's answer to [`Driver::identity`].
    OwnIdentity(Box<Result<nuschl_ipc::crypto::CryptoResponse, String>>),
    /// Identity and key transparency on one connection.
    Identity {
        server: String,
        generation: u64,
        event: Box<nuschl_session::identity::IdentityEvent>,
    },
    /// End-to-end conversations on one connection (`mls/1`).
    Mls {
        server: String,
        generation: u64,
        event: Box<nuschl_session::mls::MlsEvent>,
    },
    /// Nothing anyone needs.
    Ignored,
}

impl Event {
    /// The client's input for this event, or what the client does not model.
    pub fn into_input(self) -> Result<Input, Native> {
        Ok(match self {
            Event::Input(input) => input,
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
    Identity(Box<nuschl_session::identity::IdentityEvent>),
    Mls(Box<nuschl_session::mls::MlsEvent>),
}

/// What the session said, as the client models it; identity and end-to-end
/// events (to the UI for now) and what nobody needs (the driver's own
/// business) are not.
fn encryption_state(
    view: &nuschl_ipc::crypto::ConversationView,
) -> crate::conversation::EncryptionState {
    use crate::SessionSecurity;
    use nuschl_ipc::crypto::Lock;
    let (security, description) = match &view.lock {
        Lock::Encrypted | Lock::Verified => (
            SessionSecurity::MlsEstablished,
            if view.history_pending {
                "End-to-end encrypted; waiting for history keys".into()
            } else {
                "End-to-end encrypted; every device confirmed by key transparency".into()
            },
        ),
        Lock::Attention { reason } => (
            SessionSecurity::MlsPending,
            format!("End-to-end encrypted; needs attention: {reason}"),
        ),
        Lock::Pending { reason } => (SessionSecurity::MlsPending, reason.clone()),
        Lock::Open => (
            SessionSecurity::TransportOnly,
            "Not end-to-end encrypted".into(),
        ),
    };
    crate::conversation::EncryptionState {
        security,
        description,
        blocked: view.blocked.clone(),
        epoch: view.epoch,
    }
}

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
        SessionEvent::Nuschl(NuschlEvent::Typing(typing)) => ServerEvent::Typing(typing),
        SessionEvent::Nuschl(NuschlEvent::DirectList(list)) => {
            ServerEvent::DirectList(list.directs)
        }
        SessionEvent::Nuschl(NuschlEvent::DirectOpened(opened)) => {
            ServerEvent::DirectOpened(opened)
        }
        SessionEvent::Nuschl(NuschlEvent::Error(error)) => ServerEvent::ExtensionError(error),
        SessionEvent::Nuschl(NuschlEvent::ShareLink(link)) => ServerEvent::ShareLink(link),
        SessionEvent::Nuschl(NuschlEvent::ReadMarker(marker)) => ServerEvent::ReadMarker(marker),
        SessionEvent::Identity(event) => return Err(Some(Unmodelled::Identity(Box::new(event)))),
        SessionEvent::Mls(event) => return Err(Some(Unmodelled::Mls(Box::new(event)))),
        SessionEvent::Nuschl(_)
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
        SessionEvent::OpusAvailable(available) => ServerEvent::OpusAvailable(available),
        // The connection loop prepares welcome HTML in the content worker
        // and delivers its document directly; never print its marker source.
        SessionEvent::Welcome(_) => return Err(None),
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
        SessionEvent::AclReply(reply) => ServerEvent::AclReply(reply),
        SessionEvent::AccountReply(reply) => ServerEvent::AccountReply(reply),
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
    use nuschl_core::{ChatMessage, MessageDocument, ServerRecord};

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

#[cfg(test)]
mod encrypted_history_tests {
    use super::*;
    #[tokio::test]
    async fn authenticated_archive_text_cannot_be_erased_by_unsigned_mutation_metadata() {
        let forged = v1::Message {
            seq: 7,
            kind: v1::message::Kind::Delete as i32,
            target: 99,
            deleted_ms: 1,
            edited_ms: 2,
            edits: 3,
            delete_reason: 1,
            reply_to: 99,
            ..Default::default()
        };
        let authenticated = nuschl_session::mls::reconstructed_archive_chat(
            forged,
            "Authenticated original text".into(),
            123,
        );
        assert_eq!(
            (
                authenticated.kind,
                authenticated.target,
                authenticated.reply_to
            ),
            (0, 0, 0)
        );
        let page = v1::HistoryPage {
            messages: vec![authenticated],
            ..Default::default()
        };
        let cache = CacheKey {
            identity: "server".into(),
            channel: "channel".into(),
        };
        let (messages, _, records) = render_server_page(page, Some(&cache), &[]).await;
        assert_eq!(messages[0].message.body, "Authenticated original text");
        assert!(messages[0].deleted.is_none() && messages[0].edited_ms.is_none());
        assert_eq!(records[0].message.body, "Authenticated original text");
        assert!(records[0].deleted_ms.is_none() && records[0].edited_ms.is_none());
    }

    #[tokio::test]
    async fn unauthenticated_remote_gaps_never_replace_stored_plaintext() {
        let page = v1::HistoryPage {
            messages: vec![v1::Message {
                seq: 7,
                content: "Unable to decrypt this archived message.".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let cache = CacheKey {
            identity: "server".into(),
            channel: "channel".into(),
        };
        let (messages, _, records) = render_server_page(page, Some(&cache), &[7]).await;
        assert_eq!(messages.len(), 1, "the gap remains visible");
        assert!(
            records.is_empty(),
            "local verified plaintext must not be overwritten by a remote failure"
        );
    }
}

#[cfg(test)]
mod action_scope_tests {
    use super::*;
    use nuschl_session::connection::{ActionSubject, CommandAuthority};

    fn lease() -> CommandAuthority {
        CommandAuthority {
            own: (1, 11),
            subject: Some(ActionSubject::User(2, 22)),
            users: vec![(2, 22)],
            channels: vec![(5, 45)],
            conversations: vec![[5; 16]],
            accounts: vec![],
        }
    }
    fn wrap(
        driver: usize,
        server: &str,
        command: Command,
    ) -> Result<Option<(u64, Command)>, String> {
        scoped_command(driver, server, &mut Some(command))
    }
    #[test]
    fn distinct_drivers_nested_origins_and_explicit_guard_drop_never_widen_scope() {
        let a = action_scope(10, "a".into(), 7, lease());
        let b = action_scope(20, "b".into(), 8, lease());
        assert!(wrap(10, "b", Command::RequestStats(2)).is_err());
        assert!(wrap(10, "a", Command::RequestStats(2)).unwrap().is_some());
        assert!(wrap(20, "b", Command::RequestStats(2)).unwrap().is_some());
        let nested = action_scope(10, "changed".into(), 7, lease());
        assert!(wrap(10, "changed", Command::RequestStats(2)).is_err());
        assert!(wrap(10, "a", Command::RequestStats(2)).is_err());
        drop(a); // Out-of-order release removes this exact guard, not the innermost.
        assert!(
            wrap(10, "changed", Command::RequestStats(2))
                .unwrap()
                .is_some()
        );
        drop(nested);
        assert!(wrap(10, "a", Command::RequestStats(2)).unwrap().is_none());
        assert!(wrap(20, "b", Command::RequestStats(2)).unwrap().is_some());
        drop(b);
        assert!(wrap(20, "b", Command::RequestStats(2)).unwrap().is_none());
    }
    #[test]
    fn scope_restores_after_early_return_unwind_and_future_creation() {
        let result = with_action_scope(10, "a".into(), 7, lease(), || {
            assert!(wrap(10, "a", Command::RequestStats(2)).unwrap().is_some());
            9
        });
        assert_eq!(result, 9);
        assert!(wrap(10, "a", Command::RequestStats(2)).unwrap().is_none());
        assert!(
            std::panic::catch_unwind(|| with_action_scope(10, "a".into(), 7, lease(), || panic!(
                "expected unwind"
            )))
            .is_err()
        );
        assert!(wrap(10, "a", Command::RequestStats(2)).unwrap().is_none());
        let future = with_action_scope(10, "a".into(), 7, lease(), || async { 9 });
        assert!(wrap(10, "a", Command::RequestStats(2)).unwrap().is_none());
        drop(future);
    }
    #[test]
    fn actual_sdk_mapping_inherits_only_covered_concrete_and_semantic_targets() {
        with_action_scope(10, "a".into(), 7, lease(), || {
            for request in [
                Request::RequestBlobs {
                    textures: vec![],
                    comments: vec![2],
                    descriptions: vec![],
                },
                Request::ReadMarker {
                    conversation: [5; 16],
                    seq: 4,
                },
                Request::OpenDirect {
                    request_id: 1,
                    user_id: 202,
                },
                Request::CreateShareLink {
                    request_id: 2,
                    channel: 5,
                    expires_at: 0,
                    access_token: String::new(),
                },
            ] {
                assert!(wrap(10, "a", request_command(request)).unwrap().is_some());
            }
            assert!(
                wrap(
                    10,
                    "a",
                    request_command(Request::CreateShareLink {
                        request_id: 3,
                        channel: 6,
                        expires_at: 0,
                        access_token: String::new()
                    })
                )
                .is_err()
            );
            assert!(
                wrap(
                    10,
                    "a",
                    request_command(Request::ReadMarker {
                        conversation: [6; 16],
                        seq: 4
                    })
                )
                .is_err()
            );
            assert!(wrap(10, "a", Command::SetTokens(vec!["local config".into()])).is_err());
            for command in [
                Command::Nuschl(NuschlRequest::History(v1::HistoryRequest {
                    conversation: vec![5; 16],
                    ..Default::default()
                })),
                Command::Nuschl(NuschlRequest::Delete(v1::ChatDelete {
                    conversation: vec![5; 16],
                    ..Default::default()
                })),
            ] {
                assert!(wrap(10, "a", command).unwrap().is_some());
            }
        });
        assert!(wrap(10, "a", Command::SetTokens(vec![])).unwrap().is_none());
    }
    #[tokio::test]
    async fn native_enqueue_rejects_replaced_generation_full_closed_and_removed_connections() {
        let (commands, mut queued) = mpsc::channel(1);
        let (sounds, _) = mpsc::channel(1);
        let connection = Connection {
            generation: 7,
            voice_route: Default::default(),
            commands,
            sounds,
            task: tokio::spawn(async {}),
        };
        let mut connections = BTreeMap::from([("a".into(), connection)]);
        let command = || Command::Scoped {
            authority: lease(),
            command: Box::new(Command::RequestStats(2)),
        };
        assert!(enqueue_scoped(&connections, "a", 6, command()).is_err());
        assert!(enqueue_scoped(&connections, "a", 7, command()).is_ok());
        assert!(enqueue_scoped(&connections, "a", 7, command()).is_err());
        assert!(matches!(queued.recv().await, Some(Command::Scoped { .. })));
        connections.get_mut("a").unwrap().generation = 8;
        assert!(enqueue_scoped(&connections, "a", 7, command()).is_err());
        drop(queued);
        assert!(enqueue_scoped(&connections, "a", 8, command()).is_err());
        connections.clear();
        assert!(enqueue_scoped(&connections, "a", 8, command()).is_err());
    }
    #[test]
    fn automatic_metadata_keeps_original_tokens_and_intersects_callback_restrictions() {
        let original = nuschl_core::model::MetadataAuthority {
            generation: 7,
            own: (1, 11),
            users: vec![(2, 22)],
            channels: vec![],
        };
        let request = || Request::RequestBlobs {
            textures: vec![],
            comments: vec![2],
            descriptions: vec![],
        };
        let (generation, command) = metadata_command(10, "a", original.clone(), request()).unwrap();
        assert_eq!(generation, 7);
        let Command::Scoped { authority, .. } = command else {
            panic!("metadata must be scoped");
        };
        assert_eq!(authority.own, (1, 11));
        assert_eq!(authority.users, vec![(2, 22)]);
        with_action_scope(10, "a".into(), 7, lease(), || {
            assert!(metadata_command(10, "a", original.clone(), request()).is_ok());
            let mut changed = original.clone();
            changed.users[0].1 = 222;
            assert!(metadata_command(10, "a", changed, request()).is_err());
            let mut changed = original.clone();
            changed.own.1 = 111;
            assert!(metadata_command(10, "a", changed, request()).is_err());
            let mut changed = original.clone();
            changed.generation = 8;
            assert!(metadata_command(10, "a", changed, request()).is_err());
            assert!(metadata_command(10, "b", original.clone(), request()).is_err());
        });
        let mut missing = original;
        missing.users.clear();
        assert!(metadata_command(10, "a", missing, request()).is_err());
    }
    #[tokio::test]
    async fn queue_full_new_voice_intent_stays_suspended_with_its_new_revision() {
        let (commands, mut queued) = mpsc::channel(1);
        let (sounds, _) = mpsc::channel(1);
        let route = std::sync::Arc::new(nuschl_session::connection::VoiceRoute::default());
        let connections = BTreeMap::from([(
            "a".into(),
            Connection {
                generation: 7,
                voice_route: route.clone(),
                commands,
                sounds,
                task: tokio::spawn(async {}),
            },
        )]);
        let before = route.revision();
        assert!(voice_intent(&connections, "a", 6, Command::SelectVoiceTarget(0)).is_err());
        assert_eq!(
            route.revision(),
            before,
            "stale generation cannot suspend a replacement connection"
        );
        let b = voice_intent(
            &connections,
            "a",
            7,
            Command::Whisper {
                revision: 0,
                id: 1,
                targets: vec![nuschl_session::model::VoiceTargetSpec::Users(vec![2])],
            },
        )
        .unwrap();
        let revision_b = route.revision();
        enqueue_scoped(
            &connections,
            "a",
            7,
            Command::Scoped {
                authority: lease(),
                command: Box::new(b),
            },
        )
        .unwrap();
        let c = voice_intent(
            &connections,
            "a",
            7,
            Command::Whisper {
                revision: 0,
                id: 1,
                targets: vec![nuschl_session::model::VoiceTargetSpec::Users(vec![2])],
            },
        )
        .unwrap();
        let revision_c = route.revision();
        assert!(revision_c > revision_b);
        assert!(
            enqueue_scoped(
                &connections,
                "a",
                7,
                Command::Scoped {
                    authority: lease(),
                    command: Box::new(c)
                }
            )
            .is_err()
        );
        assert_eq!(route.target(), 31);
        assert_eq!(route.revision(), revision_c);
        let Some(Command::Scoped { command, .. }) = queued.recv().await else {
            panic!("queued B missing")
        };
        assert!(matches!(*command,Command::Whisper {revision,..} if revision==revision_b));
        // A later explicit Stop gets another revision; it cannot be confused
        // with B or with the failed C enqueue.
        assert!(
            matches!(voice_intent(&connections,"a",7,Command::SelectVoiceTarget(0)).unwrap(),Command::SelectVoiceTargetAt {revision,id:0} if revision>revision_c)
        );
    }
}
