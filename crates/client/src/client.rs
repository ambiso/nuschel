// SPDX-License-Identifier: EUPL-1.2
//! The client: every server's state, which conversation is open, and the
//! decisions about them. Commands come in as method calls (connect, open,
//! send, mark read, …), the platform's reports as [`Input`]s
//! ([`Client::handle`]); what changed goes out as [`Update`]s
//! ([`Client::take_updates`]) for a UI to react to, and the state stays
//! readable in place (`servers`, `navigation`). Nothing here blocks or reads
//! a clock: slow work is a [`Platform`] call whose answer is a later input,
//! and the platform says what time it is ([`Client::set_time`]).
use crate::{
    Conversation, HistorySource, Navigation, SessionSecurity,
    conversation::ConversationState,
    input::{Input, ServerEvent},
    model::{Presence, RejectKind, TextTarget, User},
    names::{self, Preferences},
    platform::{CacheKey, ChangeRequest, Platform, RenderKey, Request, request_id},
    server::{CertificatePrompt, ServerState, Status},
    timeline::{Change, MAX_TIMELINE, Page},
    voice::{self, Seat, Unmute, Voice},
};
use nuschel_core::{
    ContentBlock, MAX_CONTENT_BYTES, MessageDocument, SearchCursor, SearchPage,
    audio::Sound,
    bookmark::{CachedChannel, ServerBookmark},
};
use nuschel_protocol::nuschel::{self, v1};
use std::collections::BTreeMap;

/// How often we say we are typing: the server's limit per conversation.
pub const TYPING_EVERY_MS: u64 = 3_000;

/// What the UI tells the client about the reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Focus {
    /// The application is in front (its window is focused).
    pub window_active: bool,
    /// The open conversation is shown at its newest message.
    pub following: bool,
}

impl Default for Focus {
    fn default() -> Self {
        Self {
            window_active: true,
            following: true,
        }
    }
}

/// What changed, for a UI to act on; the state itself is read in place.
#[derive(Clone, Debug)]
pub enum Update {
    /// A saved server changed (its cached channels, last channel): save the
    /// servers soon.
    BookmarksChanged,
    /// A saved server's certificate pin was set on first connection: save
    /// the servers now.
    BookmarksChangedNow,
    /// Open this conversation (a direct conversation the server named, a
    /// channel a link pointed to).
    Browse { server: String, channel: u32 },
    /// The login was refused for want of a password or another name.
    LoginRefused {
        server: String,
        kind: RejectKind,
        reason: String,
    },
    /// Our text was sent: a composer still holding it can clear.
    Sent {
        conversation: Conversation,
        body: String,
    },
    /// A page reached the open conversation's timeline (`used`: it fitted).
    /// The latest page opens it at its newest message; a page around a
    /// message (a search result) shows that message.
    PageShown { page: Page, used: bool },
    /// History could not be read or kept: what to tell the user.
    HistoryFailed(String),
    /// History works again.
    HistoryRecovered,
    /// Our own message appeared in the open conversation: it follows the
    /// newest message again.
    OwnMessage,
    /// Something arrived out of sight that deserves a notification.
    Notify(Notification),
    /// The Nuschel extension was negotiated (the server's policy, e.g. its
    /// minimum version, is in the server's state).
    Negotiated { server: String },
    /// The connection was gone between these times; the client logged it
    /// (the UI may word it with local times, see [`Client::log_gap`]).
    Gap {
        server: String,
        from_ms: i64,
        to_ms: i64,
    },
    /// A message was deleted on the server.
    MessageDeleted {
        server: String,
        conversation: [u8; 16],
        seq: u64,
    },
    SearchResults {
        request: u64,
        result: Result<SearchPage, String>,
    },
    /// Rendered HTML (a description or comment) for a UI to show.
    Rendered {
        server: String,
        key: RenderKey,
        source: String,
        document: MessageDocument,
    },
    /// A decoded avatar for a UI to show.
    Avatar {
        server: String,
        hash: String,
        document: MessageDocument,
    },
    /// A share link the server issued for `channel` (asked for with
    /// [`Client::share_link`]), and the web host its operator chose, if any.
    ShareLink {
        server: String,
        channel: u32,
        link: nuschel_core::share::ShareLink,
        web_host: Option<String>,
    },
}

/// A notification for something that arrived out of sight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// One per conversation (server id and channel), or a server's friends.
    pub key: String,
    pub summary: String,
    pub body: String,
    /// The first image of the message: width, height, RGBA (base64).
    pub image: Option<(u32, u32, String)>,
}

/// Why a message was not sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendError {
    /// Nothing to send, or the previous message is still on its way.
    Nothing,
    /// Longer than any server takes.
    TooLong,
    /// Not sent; the server's notice says why.
    Refused,
}

mod share;

/// Unmuting would make a second talk server: the user is asked "Speak on
/// two servers?" first, and answers with [`Client::speak_here`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecondServer {
    /// The server you unmuted on.
    pub server: usize,
    /// Where you speak now.
    pub talking: Vec<usize>,
}

struct PendingSend {
    conversation: Conversation,
    target: TextTarget,
    body: String,
}

/// What the voice policy needs of a server. One reconnecting still counts:
/// it gets its mute state back when it returns.
fn seat(server: &ServerState) -> Seat {
    Seat {
        connected: server.status != Status::Disconnected,
        muted: server.muted,
        deafened: server.deafened,
    }
}

/// Every server's seat, by id.
fn seats(servers: &[ServerState]) -> impl Fn(&str) -> Seat + '_ {
    |id| {
        servers
            .iter()
            .find(|server| server.id == id)
            .map(seat)
            .unwrap_or_default()
    }
}

pub struct Client {
    /// Saved servers in their order, with what is known of each.
    pub servers: Vec<ServerState>,
    /// Which conversation is open, and where our voice is.
    pub navigation: Navigation,
    /// Which servers get the microphone (the talk servers); the others
    /// listen ([`voice`]).
    pub voice: Voice,
    pub focus: Focus,
    pub preferences: Preferences,
    platform: Box<dyn Platform>,
    sending: Option<PendingSend>,
    /// Direct conversations asked for (`OpenDirect`), by request id: the
    /// server, the peer's session, and whether to open it once known.
    direct_requests: BTreeMap<u64, (String, u32, bool)>,
    /// Share links asked for, by request id.
    share_requests: BTreeMap<u64, share::ShareRequest>,
    /// When we last said we type, where (the platform's monotonic ms).
    typing_sent: Option<((String, u32), u64)>,
    /// Passwords of this run's connections, until they log in.
    passwords: BTreeMap<String, String>,
    pub(crate) updates: Vec<Update>,
    now_ms: u64,
}

impl Client {
    pub fn new(
        platform: Box<dyn Platform>,
        bookmarks: Vec<ServerBookmark>,
        preferences: Preferences,
    ) -> Self {
        Self {
            servers: bookmarks.into_iter().map(ServerState::new).collect(),
            navigation: Navigation::default(),
            voice: Voice::default(),
            focus: Focus::default(),
            preferences,
            platform,
            sending: None,
            direct_requests: BTreeMap::new(),
            share_requests: BTreeMap::new(),
            typing_sent: None,
            passwords: BTreeMap::new(),
            updates: Vec::new(),
            now_ms: 0,
        }
    }

    /// The platform's monotonic clock, in milliseconds (typing indicators,
    /// edits waiting for their message).
    pub fn set_time(&mut self, now_ms: u64) -> bool {
        self.now_ms = now_ms;
        let mut expired_any = false;
        for server in &mut self.servers {
            if server
                .notice_expires
                .as_ref()
                .is_some_and(|(_, until)| now_ms >= *until)
                && let Some((expired, _)) = server.notice_expires.take()
                && server.notice.as_ref() == Some(&expired)
            {
                server.notice = None;
                expired_any = true;
            }
        }
        expired_any
    }

    /// What changed since the last call.
    pub fn take_updates(&mut self) -> Vec<Update> {
        std::mem::take(&mut self.updates)
    }

    /// The platform, for requests the client does not model yet.
    pub fn platform(&self) -> &dyn Platform {
        &*self.platform
    }

    pub fn server_index(&self, id: &str) -> Option<usize> {
        self.servers.iter().position(|server| server.id == id)
    }

    /// The open conversation: its server's and its own index.
    pub fn selected_indices(&self) -> Option<(usize, usize)> {
        let selected = self.navigation.selection()?;
        let server = self.server_index(&selected.server)?;
        let channel = self.servers[server]
            .channels
            .iter()
            .position(|channel| channel.id == selected.channel)?;
        Some((server, channel))
    }

    /// Whether a password was given for this server in this run.
    pub fn password(&self, server: &str) -> Option<&String> {
        self.passwords.get(server)
    }

    // Servers.

    /// Add a saved server; returns its index.
    pub fn add_server(&mut self, bookmark: ServerBookmark) -> usize {
        self.servers.push(ServerState::new(bookmark));
        self.servers.len() - 1
    }

    /// Replace a saved server (its address or name changed), dropping its
    /// connection and what was known of it.
    pub fn replace_server(&mut self, index: usize, bookmark: ServerBookmark) {
        self.platform.disconnect(&self.servers[index].id);
        self.voice.disconnected(&self.servers[index].id, None);
        self.servers[index] = ServerState::new(bookmark);
    }

    pub fn remove_server(&mut self, id: &str) {
        self.platform.disconnect(id);
        self.voice.disconnected(id, None);
        self.passwords.remove(id);
        self.servers.retain(|server| server.id != id);
    }

    /// Connect self-muted; `deafened` too (automatic connections at startup,
    /// so nobody hears a session that began without the user present). A
    /// password is remembered for this run until the login succeeds.
    pub fn connect(&mut self, index: usize, password: Option<String>, deafened: bool) {
        let server = &mut self.servers[index];
        let password = password.or_else(|| self.passwords.get(&server.id).cloned());
        if let Some(password) = &password {
            self.passwords.insert(server.id.clone(), password.clone());
        }
        match self
            .platform
            .connect(server.bookmark.clone(), password, deafened)
        {
            Ok(generation) => {
                server.generation = generation;
                server.muted = true;
                server.deafened = deafened;
                server.status = Status::Connecting;
                server.security = SessionSecurity::Unknown;
                server.certificate = None;
                server.notice = None;
                // It joins the channel you were last in there.
                let id = server.id.clone();
                let seats = seats(&self.servers);
                self.voice.connecting(&id, &seats);
            }
            Err(error) => server.notice = Some(error),
        }
    }

    /// Trust the certificate server `index` presented (its
    /// [`ServerState::certificate`] prompt): pin it on the saved server and
    /// connect again. The saved servers change
    /// ([`Update::BookmarksChangedNow`]). Returns false if nothing asked.
    pub fn trust_certificate(&mut self, index: usize, deafened: bool) -> bool {
        let Some(server) = self.servers.get_mut(index) else {
            return false;
        };
        let Some(prompt) = server.certificate.take() else {
            return false;
        };
        server.bookmark.certificate_sha256 = Some(prompt.sha256);
        self.updates.push(Update::BookmarksChangedNow);
        self.connect(index, None, deafened);
        true
    }

    pub fn disconnect(&mut self, index: usize) {
        let Some(server) = self.servers.get_mut(index) else {
            return;
        };
        self.platform.disconnect(&server.id);
        if server.status == Status::Connected && !server.deafened {
            self.platform.chime(Sound::Disconnected);
        }
        self.passwords.remove(&server.id);
        if let Some(lost) = server.lost_messages() {
            server.notice = Some(lost);
        }
        if self
            .sending
            .as_ref()
            .is_some_and(|pending| pending.conversation.server == server.id)
        {
            self.sending = None;
        }
        server.status = Status::Disconnected;
        server.security = SessionSecurity::Unknown;
        server.own_session = None;
        server.capabilities.clear();
        server.policy = None;
        for channel in &mut server.channels {
            channel.typing.clear();
        }
        // Offline, the timeline is local observations again.
        if server.history_source == HistorySource::ServerArchive {
            server.history_source = HistorySource::LocalObserved;
            for channel in &mut server.channels {
                channel.timeline.reset();
            }
        }
        server.users.clear();
        server.talking.clear();
        server.transport = None;
        server.audio_status = "Audio disconnected".into();
        if self
            .navigation
            .voice()
            .is_some_and(|voice| voice.server == server.id)
        {
            self.navigation.set_voice(None);
        }
        let id = server.id.clone();
        // The server in front takes the voice over, else another connected one.
        let fallback = self
            .selected_indices()
            .map(|(selected, _)| selected)
            .into_iter()
            .chain(0..self.servers.len())
            .map(|index| &self.servers[index])
            .find(|server| server.id != id && server.status != Status::Disconnected)
            .map(|server| server.id.clone());
        self.voice.disconnected(&id, fallback.as_deref());
    }

    /// Toggle our mute (or deafen) on server `index`. Unmuting where your
    /// voice does not go yet while it goes elsewhere changes nothing and
    /// returns the question to ask ([`SecondServer`]).
    pub fn set_mute(&mut self, index: usize, deafen: bool) -> Option<SecondServer> {
        let server = &self.servers[index];
        let (id, muted, deafened) = (server.id.clone(), server.muted, server.deafened);
        let changes = if deafen {
            vec![voice::Change {
                server: id,
                muted: !deafened || muted,
                deafened: !deafened,
            }]
        } else if muted || deafened {
            let decision = self.voice.unmute(&id, &seats(&self.servers));
            match decision {
                Unmute::Now(changes) => changes,
                Unmute::Ask => return Some(self.second_server(index)),
            }
        } else {
            vec![voice::Change {
                server: id,
                muted: true,
                deafened: false,
            }]
        };
        self.apply_voice(changes);
        None
    }

    /// The question for unmuting on server `index`.
    fn second_server(&self, index: usize) -> SecondServer {
        let seats = seats(&self.servers);
        SecondServer {
            server: index,
            talking: self
                .voice
                .talk()
                .iter()
                .filter(|talk| seats(talk).speaking())
                .filter_map(|talk| self.server_index(talk))
                .collect(),
        }
    }

    /// The answer to "Speak on two servers?" for server `index`: there as
    /// well (`both`), or only there.
    pub fn speak_here(&mut self, index: usize, both: bool) {
        let id = self.servers[index].id.clone();
        let changes = self.voice.speak_here(&id, both, &seats(&self.servers));
        self.apply_voice(changes);
    }

    /// "Use voice here": server `index` becomes the talk server, undeafened;
    /// a live microphone moves along.
    pub fn use_voice_here(&mut self, index: usize) {
        let id = self.servers[index].id.clone();
        let mut changes = self.voice.move_here(&id, &seats(&self.servers));
        let server = &self.servers[index];
        if server.deafened && !changes.iter().any(|change| change.server == id) {
            changes.push(voice::Change {
                server: id,
                muted: server.muted,
                deafened: false,
            });
        }
        self.apply_voice(changes);
    }

    /// The mute or deafen shortcut: acts on the talk servers (without one,
    /// on the server in front, which may ask first).
    pub fn toggle_voice(&mut self, deafen: bool) -> Option<SecondServer> {
        let fallback = self
            .selected_indices()
            .map(|(server, _)| self.servers[server].id.clone());
        let decision = self
            .voice
            .toggle(deafen, fallback.as_deref(), &seats(&self.servers));
        match decision {
            Unmute::Now(changes) => {
                self.apply_voice(changes);
                None
            }
            Unmute::Ask => fallback
                .and_then(|id| self.server_index(&id))
                .map(|index| self.second_server(index)),
        }
    }

    /// One server's part in your voice.
    pub fn voice_role(&self, index: usize) -> voice::Role {
        let server = &self.servers[index];
        self.voice.role(&server.id, seat(server))
    }

    /// Send what the voice policy decided.
    fn apply_voice(&mut self, changes: Vec<voice::Change>) {
        for change in changes {
            let Some(server) = self
                .servers
                .iter_mut()
                .find(|server| server.id == change.server)
            else {
                continue;
            };
            let muted = change.muted || change.deafened;
            if (server.muted, server.deafened) == (muted, change.deafened) {
                continue;
            }
            server.muted = muted;
            server.deafened = change.deafened;
            let request = Request::SetSelf {
                muted,
                deafened: change.deafened,
            };
            if let Err(error) = self.platform.request(&server.id, request) {
                server.notice = Some(error);
            }
        }
    }

    /// Move our voice to channel `channel` of server `index`: the talk
    /// server moves with you ([`Voice::move_here`]).
    pub fn join(&mut self, index: usize, channel: u32) {
        let server = &mut self.servers[index];
        if let Err(error) = self.platform.request(&server.id, Request::Join(channel)) {
            server.notice = Some(error);
            return;
        }
        let id = server.id.clone();
        let changes = self.voice.move_here(&id, &seats(&self.servers));
        self.apply_voice(changes);
    }

    /// Send a request the client does not model to server `index`.
    pub fn request(&self, index: usize, request: Request) -> Result<(), String> {
        self.platform.request(&self.servers[index].id, request)
    }

    // Conversations.

    /// Open conversation `channel` of server `index` (browsing never moves
    /// voice). Its latest page loads unless it is in memory; a long timeline
    /// is trimmed to its newest messages. Returns false if there is no such
    /// conversation. A history request that fails is returned as `Err`.
    pub fn open(&mut self, index: usize, channel: u32) -> Result<bool, String> {
        let Some(server) = self.servers.get(index) else {
            return Ok(false);
        };
        if !server.channels.iter().any(|entry| entry.id == channel) {
            return Ok(false);
        }
        let conversation = Conversation {
            server: server.id.clone(),
            channel,
        };
        self.navigation
            .browse(conversation, HistorySource::LocalObserved);
        self.focus.following = true;
        let entry = self.servers[index]
            .channels
            .iter_mut()
            .find(|entry| entry.id == channel)
            .expect("checked above");
        let mut result = Ok(true);
        if entry.timeline.loaded {
            entry.timeline.trim_oldest(MAX_TIMELINE);
            entry.new_messages = 0;
        } else if let Err(error) = self.load_history(None) {
            result = Err(error);
        }
        let server = &self.servers[index];
        if server.status == Status::Connected && !server.permissions.contains_key(&channel) {
            let _ = self
                .platform
                .request(&server.id, Request::RequestPermissions(channel));
        }
        result
    }

    /// Load a page of the open conversation's history before `before` (the
    /// latest page for None): from the server on a Nuschel server with
    /// history, else from local history.
    pub fn load_history(&mut self, before: Option<i64>) -> Result<(), String> {
        let Some((index, channel)) = self.selected_indices() else {
            return Ok(());
        };
        self.load_page(index, channel, Page::Before(before))
    }

    /// Load `page` of conversation `position` of server `index`; answers
    /// for an earlier request of it are dropped.
    pub fn load_page(&mut self, index: usize, position: usize, page: Page) -> Result<(), String> {
        let server = &mut self.servers[index];
        let Some(identity) = server.history_identity() else {
            return Ok(());
        };
        // Stock direct messages on a Nuschel server are local observations.
        let archived = server.archived(&server.channels[position]);
        let source = server.history_source;
        let channel = &mut server.channels[position];
        channel.timeline.loading = true;
        if page == Page::Before(None) {
            channel.new_messages = 0;
        }
        let ticket = self.navigation.browse(
            Conversation {
                server: server.id.clone(),
                channel: channel.id,
            },
            source,
        );
        let requested = match channel.conversation.filter(|_| archived) {
            // A Nuschel server's history: everything this member may read.
            Some(conversation) => {
                let cache = CacheKey {
                    identity,
                    channel: channel.storage_key.clone(),
                };
                self.platform.server_history(
                    server.id.clone(),
                    conversation,
                    ticket,
                    Some(cache),
                    page,
                )
            }
            // Until the server names the conversation, its latest pages do
            // not load (a search result's page is from local history).
            None if source == HistorySource::ServerArchive
                && matches!(channel.target, TextTarget::Channel(_))
                && matches!(page, Page::Before(_)) =>
            {
                channel.timeline.loading = false;
                return Ok(());
            }
            None => self.platform.local_history(
                server.id.clone(),
                identity,
                channel.storage_key.clone(),
                ticket,
                page,
            ),
        };
        if requested.is_err() {
            channel.timeline.loading = false;
        }
        requested
    }

    /// Whether our previous message is still on its way.
    pub fn sending(&self) -> bool {
        self.sending.is_some()
    }

    /// Send `body` to the open conversation. One message at a time: the
    /// next waits until the session took this one ([`Update::Sent`]).
    pub fn send_text(&mut self, body: String) -> Result<(), SendError> {
        let Some((server, channel)) = self.selected_indices() else {
            return Err(SendError::Nothing);
        };
        if self.sending.is_some() || body.trim().is_empty() {
            return Err(SendError::Nothing);
        }
        if body.len() > MAX_CONTENT_BYTES {
            return Err(SendError::TooLong);
        }
        let server = &mut self.servers[server];
        if server.status != Status::Connected {
            server.notice = Some("Connect before sending a message.".into());
            return Err(SendError::Refused);
        }
        let (target, expected_peer) = match server.message_target(channel) {
            Ok(target) => target,
            Err(reason) => {
                server.notice = Some(reason.into());
                server.notice_expires = Some((reason.into(), self.now_ms.saturating_add(10_000)));
                return Err(SendError::Refused);
            }
        };
        let channel = &server.channels[channel];
        let pending = PendingSend {
            conversation: Conversation {
                server: server.id.clone(),
                channel: channel.id,
            },
            target: target.clone(),
            body: body.clone(),
        };
        let request = Request::SendText {
            target,
            body,
            expected_peer,
        };
        match self.platform.request(&server.id, request) {
            Ok(()) => {
                self.sending = Some(pending);
                // Sent: we are no longer typing it.
                self.typing_sent = None;
                Ok(())
            }
            Err(error) => {
                server.notice = Some(error);
                Err(SendError::Refused)
            }
        }
    }

    /// The composer of the open conversation changed; `typing` is whether
    /// it holds text being written (not an edit). On Nuschel servers others
    /// learn we type, at most every [`TYPING_EVERY_MS`].
    pub fn composing(&mut self, typing: bool) {
        if !typing || !self.preferences.send_typing {
            self.typing_sent = None;
            return;
        }
        let Some((server, channel)) = self.selected_indices() else {
            return;
        };
        let view = &self.servers[server];
        let channel = &view.channels[channel];
        let Some(conversation) = view.typing_conversation(channel) else {
            return;
        };
        let key = (view.id.clone(), channel.id);
        let now = self.now_ms;
        if self
            .typing_sent
            .as_ref()
            .is_some_and(|(sent, at)| *sent == key && now.saturating_sub(*at) < TYPING_EVERY_MS)
        {
            return;
        }
        if self
            .platform
            .request(&view.id, Request::Typing { conversation })
            .is_ok()
        {
            self.typing_sent = Some((key, now));
        }
    }

    /// Drop typing indicators that are over; returns whether any went, and
    /// whether any are left.
    pub fn expire_typing(&mut self) -> (bool, bool) {
        let now = self.now_ms;
        let mut changed = false;
        let mut left = false;
        for server in &mut self.servers {
            for channel in &mut server.channels {
                changed |= channel.expire_typing(now);
                left |= !channel.typing.is_empty();
            }
        }
        (changed, left)
    }

    /// The open conversation is read when it is in sight: the application in
    /// front and the newest message shown. A Nuschel server keeps the read
    /// marker (synced to the account's other devices; unread counts come
    /// from it).
    pub fn mark_read(&mut self) {
        if !(self.focus.window_active && self.focus.following) {
            return;
        }
        let Some((server, channel)) = self.selected_indices() else {
            return;
        };
        let archive = self.servers[server].history_source == HistorySource::ServerArchive;
        let server = &mut self.servers[server];
        let channel = &mut server.channels[channel];
        channel.unread = 0;
        channel.unread_mention = false;
        let newest = channel
            .timeline
            .newest_seq()
            .and_then(|seq| i64::try_from(seq).ok());
        if archive
            && let Some(newest) = newest
            && newest > channel.read_seq
        {
            let seq = u64::try_from(newest).unwrap_or(0);
            let request = match channel.target.clone() {
                TextTarget::Channel(id) => Some(Request::MarkRead { channel: id, seq }),
                TextTarget::DirectConversation(conversation) => {
                    Some(Request::ReadMarker { conversation, seq })
                }
                TextTarget::Direct(_) => None,
            };
            if let Some(request) = request {
                channel.read_seq = newest;
                let _ = self.platform.request(&server.id, request);
            }
        }
    }

    /// Open the direct conversation with `session` on server `index`. On a
    /// Nuschel server, between registered accounts, it is the server's
    /// conversation, opened once the server names it ([`Update::Browse`]);
    /// otherwise the stock path, whose conversation id is returned to open now.
    pub fn open_direct(&mut self, index: usize, session: u32) -> Option<u32> {
        let server = &self.servers[index];
        if server.own_session == Some(session) {
            return None;
        }
        let peer = server.users.get(&session)?.clone();
        if self.open_durable_direct(index, &peer) {
            return None;
        }
        Some(self.stock_direct(index, &peer))
    }

    fn stock_direct(&mut self, index: usize, peer: &User) -> u32 {
        let name = names::display_name(peer, &self.preferences).to_string();
        self.servers[index].stock_direct_view(peer, &name)
    }

    fn open_durable_direct(&mut self, index: usize, peer: &User) -> bool {
        let server = &mut self.servers[index];
        let Some(user_id) = peer.user_id.filter(|_| server.durable_directs_with(peer)) else {
            return false;
        };
        let name = names::display_name(peer, &self.preferences).to_string();
        if let Some(id) = server.direct_with(user_id) {
            let channel = server.direct_view(id, &name, Some(peer.clone()));
            self.updates.push(Update::Browse {
                server: server.id.clone(),
                channel: server.channels[channel].id,
            });
            return true;
        }
        let request_id = request_id();
        let request = Request::OpenDirect {
            request_id,
            user_id,
        };
        if self.platform.request(&server.id, request).is_err() {
            return false;
        }
        while self.direct_requests.len() >= 16 {
            self.direct_requests.pop_first();
        }
        self.direct_requests
            .insert(request_id, (server.id.clone(), peer.session, true));
        true
    }

    /// Edit or delete a message on a Nuschel server; the answer comes back
    /// as an edit or deletion, or [`Input::ChangeFailed`].
    pub fn change_message(&self, change: ChangeRequest) -> Result<(), String> {
        self.platform.change_message(change)
    }

    /// Edit our message `seq` in conversation `position` of server `index`
    /// to `text`, if the rules allow it (see [`ServerState::may_edit`]).
    pub fn edit(
        &self,
        index: usize,
        position: usize,
        seq: u64,
        text: String,
    ) -> Result<(), String> {
        self.request_change(index, position, seq, Some(text))
    }

    /// Delete message `seq` (ours, or as a moderator where the server allows it).
    pub fn delete(&self, index: usize, position: usize, seq: u64) -> Result<(), String> {
        self.request_change(index, position, seq, None)
    }

    fn request_change(
        &self,
        index: usize,
        position: usize,
        seq: u64,
        content: Option<String>,
    ) -> Result<(), String> {
        let server = &self.servers[index];
        let channel = &server.channels[position];
        let message = channel
            .timeline
            .messages
            .iter()
            .find(|message| message.seq == Some(seq))
            .ok_or("That message is not loaded.")?;
        let allowed = match &content {
            Some(text) => {
                if text.trim().is_empty() || text.len() > nuschel::MAX_CONTENT_BYTES {
                    return Err("An edit needs text, of at most 64 KiB.".into());
                }
                server.may_edit(channel, message)
            }
            None => server.may_delete(channel, message),
        };
        let conversation = channel.conversation.filter(|_| allowed);
        let conversation = conversation.ok_or("This message cannot be changed.")?;
        self.platform.change_message(ChangeRequest {
            server: server.id.clone(),
            conversation,
            seq,
            moderator: content.is_none() && server.moderates(channel),
            content,
            cache: Self::cache_key(server, channel),
        })
    }

    /// Where local history keeps conversation `channel` of `server`.
    pub fn cache_key(server: &ServerState, channel: &ConversationState) -> Option<CacheKey> {
        Some(CacheKey {
            identity: server.history_identity()?,
            channel: channel.storage_key.clone(),
        })
    }

    /// Search the local history of these servers' identities, or of one
    /// conversation (its storage key).
    pub fn search(
        &self,
        request: u64,
        scope: (Vec<String>, Option<String>),
        query: String,
        after: Option<SearchCursor>,
    ) -> Result<(), String> {
        self.platform.search(request, scope, query, after)
    }

    /// Log a line of the connection gap in every conversation of `server`.
    pub fn log_gap(&mut self, server: &str, text: String, to_ms: i64) {
        if let Some(index) = self.server_index(server) {
            for channel in &mut self.servers[index].channels {
                channel.log(text.clone(), to_ms, true);
            }
        }
    }

    // What the platform reports.

    /// Apply what the platform reported; call [`Client::settle`] after a batch.
    pub fn handle(&mut self, input: Input) {
        match input {
            Input::Sent {
                server,
                generation,
                target,
                body,
            } => {
                if self
                    .servers
                    .iter()
                    .any(|entry| entry.id == server && entry.generation == generation)
                    && self.sending.as_ref().is_some_and(|pending| {
                        pending.conversation.server == server
                            && pending.target == target
                            && pending.body == body
                    })
                    && let Some(pending) = self.sending.take()
                {
                    self.updates.push(Update::Sent {
                        conversation: pending.conversation,
                        body: pending.body,
                    });
                }
            }
            Input::SearchResults { request, result } => {
                self.updates.push(Update::SearchResults { request, result });
            }
            Input::HistoryWindow {
                server,
                ticket,
                page,
                messages,
                next_before,
                next_after,
            } => {
                if !self.navigation.accepts(&ticket) {
                    return;
                }
                let Some(channel) = self
                    .servers
                    .iter_mut()
                    .find(|entry| entry.id == server)
                    .and_then(|server| {
                        server
                            .channels
                            .iter_mut()
                            .find(|entry| entry.id == ticket.conversation.channel)
                    })
                else {
                    return;
                };
                channel.timeline.loading = false;
                let used = channel
                    .timeline
                    .take_window(page, messages, (next_before, next_after));
                if used && matches!(page, Page::Around(_)) {
                    channel.new_messages = 0;
                    self.focus.following = false;
                }
                self.updates.push(Update::PageShown { page, used });
            }
            Input::HistoryRecovered => self.updates.push(Update::HistoryRecovered),
            Input::Edited {
                server,
                conversation,
                seq,
                source,
                document,
                edited_ms,
            } => self.apply_change(
                &server,
                conversation,
                seq,
                Change::Edited {
                    source,
                    document,
                    edited_ms,
                },
            ),
            Input::Deleted {
                server,
                conversation,
                seq,
                deletion,
            } => self.apply_change(&server, conversation, seq, Change::Deleted(deletion)),
            Input::ChangeFailed {
                server,
                delete,
                reason,
            } => {
                if let Some(server) = self.servers.iter_mut().find(|entry| entry.id == server) {
                    server.notice = Some(format!(
                        "The message was not {}: {reason}",
                        if delete { "deleted" } else { "edited" }
                    ));
                }
            }
            Input::HistoryFailed {
                server,
                ticket,
                reason,
            } => {
                if let Some(server) = self.servers.iter_mut().find(|entry| entry.id == server)
                    && let Some(channel) = server
                        .channels
                        .iter_mut()
                        .find(|entry| entry.id == ticket.conversation.channel)
                {
                    channel.timeline.loading = false;
                }
                self.updates
                    .push(Update::HistoryFailed(format!("Server history: {reason}")));
            }
            Input::Downgrade {
                server,
                generation,
                reason,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    self.disconnect(index);
                    self.servers[index].downgrade = Some(reason);
                }
            }
            Input::HistoryError(error) => {
                for server in &mut self.servers {
                    for channel in &mut server.channels {
                        channel.timeline.loading = false;
                    }
                }
                self.updates
                    .push(Update::HistoryFailed(format!("Local history: {error}")));
            }
            Input::History {
                server,
                ticket,
                before,
                messages,
                next_before,
            } => {
                if !self.navigation.accepts(&ticket) {
                    return;
                }
                if let Some(server) = self.servers.iter_mut().find(|entry| entry.id == server)
                    && let Some(channel) = server
                        .channels
                        .iter_mut()
                        .find(|entry| entry.id == ticket.conversation.channel)
                {
                    channel.timeline.loading = false;
                    channel.timeline.take_page(before, messages, next_before);
                    if before.is_none() {
                        // A conversation opens at its newest message.
                        self.focus.following = true;
                    }
                    self.updates.push(Update::PageShown {
                        page: Page::Before(before),
                        used: true,
                    });
                }
            }
            Input::Server {
                server,
                generation,
                event,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    self.server_event(index, event);
                }
            }
            Input::Certificate {
                server,
                generation,
                sha256,
                changed,
                reason,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    let server = &mut self.servers[index];
                    server.status = Status::Disconnected;
                    server.security = SessionSecurity::Unknown;
                    server.certificate = Some(CertificatePrompt {
                        sha256,
                        changed,
                        reason,
                    });
                }
            }
            Input::Disconnected {
                server,
                generation,
                reason,
                reject,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    let lost = self.servers[index].lost_messages();
                    self.disconnect(index);
                    self.servers[index].notice = Some(
                        lost.map_or_else(|| reason.clone(), |lost| format!("{reason} {lost}")),
                    );
                    if let Some(kind) =
                        reject.filter(|kind| kind.needs_password() || kind.needs_username())
                    {
                        self.updates.push(Update::LoginRefused {
                            server,
                            kind,
                            reason,
                        });
                    }
                }
            }
            Input::Reconnecting {
                server,
                generation,
                attempt,
                delay_secs,
                reason,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    let server = &mut self.servers[index];
                    if server.status == Status::Connected && !server.deafened {
                        self.platform.chime(Sound::Disconnected);
                    }
                    server.status = Status::Connecting;
                    server.own_session = None;
                    if let Some(lost) = server.lost_messages() {
                        server.notice = Some(lost);
                    }
                    server.users.clear();
                    server.talking.clear();
                    server.transport = None;
                    server.permissions.clear();
                    server.reconnecting = Some(format!(
                        "Connection lost: {reason}. Reconnecting in {delay_secs} s (attempt {attempt})…"
                    ));
                }
            }
            Input::Gap {
                server,
                generation,
                from_ms,
                to_ms,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    self.servers[index].reconnecting = None;
                    self.updates.push(Update::Gap {
                        server,
                        from_ms,
                        to_ms,
                    });
                }
            }
            Input::Rendered {
                server,
                generation,
                key,
                source,
                document,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    let state = &mut self.servers[index];
                    let current = match key {
                        RenderKey::Description(id) => state
                            .channel_view(id)
                            .is_some_and(|channel| channel.description_raw == source),
                        RenderKey::Comment(session) => state
                            .users
                            .get(&session)
                            .is_some_and(|user| user.comment.as_deref() == Some(source.as_str())),
                    };
                    if current {
                        self.updates.push(Update::Rendered {
                            server,
                            key,
                            source,
                            document,
                        });
                    }
                }
            }
            Input::Talking {
                server,
                generation,
                session,
                talking,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    let server = &mut self.servers[index];
                    if talking {
                        server.talking.insert(session);
                    } else {
                        server.talking.remove(&session);
                    }
                }
            }
            Input::Avatar {
                server,
                generation,
                hash,
                document,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    let state = &mut self.servers[index];
                    // A server cycling texture hashes must not grow what is
                    // kept without bound: past 1024, keep only avatars in use.
                    if state.avatars.len() >= 1024 {
                        let used: std::collections::BTreeSet<_> = state
                            .users
                            .values()
                            .filter_map(|user| user.texture_hash.clone())
                            .collect();
                        state.avatars.retain(|hash| used.contains(hash));
                        state.avatar_requests.clear();
                    }
                    state.avatars.insert(hash.clone());
                    self.updates.push(Update::Avatar {
                        server,
                        hash,
                        document,
                    });
                }
            }
            Input::AudioDevices {
                server,
                generation,
                inputs,
                outputs,
                input,
                output,
                echo,
                noise,
                reference,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    let audio = &mut self.servers[index].audio;
                    audio.inputs = inputs;
                    audio.outputs = outputs;
                    audio.input = input;
                    audio.output = output;
                    audio.echo = echo;
                    audio.noise = noise;
                    audio.reference = reference;
                }
            }
            Input::Echo {
                server,
                generation,
                erle_db,
                residual_likelihood,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    let audio = &mut self.servers[index].audio;
                    audio.erle_db = erle_db;
                    audio.residual_echo = residual_likelihood;
                }
            }
            Input::AudioHealth {
                server,
                generation,
                health,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    self.servers[index].audio.health = health;
                }
            }
            Input::AudioStatus {
                server,
                generation,
                message,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    self.servers[index].audio_status = message;
                }
            }
            Input::Message {
                server,
                generation,
                target,
                storage_key,
                peer,
                message,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    self.message(index, target, storage_key, peer, message);
                }
            }
        }
    }

    /// After a batch of inputs: channels in display order again, and edits
    /// and deletions that came before their message's page.
    pub fn settle(&mut self) {
        for server in &mut self.servers {
            if std::mem::take(&mut server.unsorted) {
                server.sort_channels();
            }
        }
        for index in 0..self.servers.len() {
            if !self.servers[index].waiting_changes.is_empty() {
                let mut waiting = std::mem::take(&mut self.servers[index].waiting_changes);
                waiting.retry(self.now_ms, |conversation, seq, change| {
                    self.change(index, conversation, seq, change)
                });
                self.servers[index].waiting_changes = waiting;
            }
        }
    }

    /// The server `id`, if `generation` is its current connection.
    fn current(&self, id: &str, generation: u64) -> Option<usize> {
        self.servers
            .iter()
            .position(|entry| entry.id == id && entry.generation == generation)
    }

    /// A message changed on the server: show its new text, or its tombstone.
    /// A message not shown yet (a page still on its way) gets it when it is.
    fn apply_change(&mut self, server: &str, conversation: [u8; 16], seq: u64, change: Change) {
        let Some(index) = self.server_index(server) else {
            return;
        };
        if let Err(change) = self.change(index, conversation, seq, change) {
            self.servers[index]
                .waiting_changes
                .push(conversation, seq, change, self.now_ms);
        }
    }

    fn change(
        &mut self,
        index: usize,
        conversation: [u8; 16],
        seq: u64,
        change: Change,
    ) -> Result<(), Change> {
        let server = &mut self.servers[index];
        if server.change_message(conversation, seq, change)? {
            self.updates.push(Update::MessageDeleted {
                server: server.id.clone(),
                conversation,
                seq,
            });
        }
        Ok(())
    }

    fn message(
        &mut self,
        index: usize,
        target: TextTarget,
        storage_key: String,
        peer: Option<User>,
        mut message: crate::timeline::IncomingMessage,
    ) {
        let server = &mut self.servers[index];
        // Acknowledged before it was shown.
        if let Some(key) = message.pending
            && let Some(seq) = server.acked.remove(&key)
        {
            message.pending = None;
            message.id = seq.map(|seq| seq as i64);
            message.seq = seq;
        }
        // A stock direct message from someone we have a durable direct
        // conversation with (an image, or from a stock client) shows there;
        // it is not in the server's history.
        let durable = match (&target, &peer) {
            (TextTarget::Direct(_), Some(peer)) if peer.user_id.is_some_and(|id| id != 0) => {
                server.channels.iter().position(|channel| {
                    matches!(channel.target, TextTarget::DirectConversation(_))
                        && channel.direct_peer.as_ref().and_then(|known| known.user_id)
                            == peer.user_id
                })
            }
            _ => None,
        };
        if durable.is_some() {
            message.id = None;
        }
        let position =
            if let Some(position) = durable {
                position
            } else if let Some(position) = server.channels.iter().position(|channel| {
                channel.storage_key == storage_key
                    || (channel.target == target
                        && channel.storage_key.starts_with("direct:pending:")
                        && channel.direct_peer.as_ref().zip(peer.as_ref()).is_some_and(
                            |(old, new)| old.instance == new.instance && old.session == new.session,
                        ))
            }) {
                position
            } else if let TextTarget::Direct(_) = target {
                let name = peer
                    .as_ref()
                    .map(|peer| names::display_name(peer, &self.preferences))
                    .unwrap_or("Direct message");
                server.received_direct(name, target.clone(), peer.clone(), storage_key.clone())
            } else if let TextTarget::DirectConversation(id) = target {
                // A durable direct conversation, perhaps new.
                let name = match &peer {
                    Some(peer) => names::display_name(peer, &self.preferences).to_string(),
                    None if message.outgoing => server
                        .directs
                        .get(&id)
                        .map(|direct| direct.name.clone())
                        .unwrap_or_else(|| "Direct message".into()),
                    None => message.message.author.clone(),
                };
                server.direct_view(id, &name, peer.clone())
            } else {
                return;
            };
        let direct = matches!(
            target,
            TextTarget::Direct(_) | TextTarget::DirectConversation(_)
        );
        let channel_id = server.channels[position].id;
        let shown = self.focus.window_active
            && self.navigation.selection().is_some_and(|selected| {
                selected.server == server.id && selected.channel == channel_id
            });
        // A notification for what arrives out of sight.
        if !message.outgoing && self.preferences.notifications {
            let voice = server.voice_channel();
            let channel = &server.channels[position];
            let in_voice = matches!(channel.target, TextTarget::Channel(id) if Some(id) == voice);
            let mut text = String::new();
            let mut image = None;
            for block in &message.document.blocks {
                match block {
                    ContentBlock::Text { text: part, .. } => text.push_str(part),
                    ContentBlock::Code { runs, .. } => {
                        runs.iter().for_each(|run| text.push_str(&run.text))
                    }
                    ContentBlock::Image {
                        width,
                        height,
                        rgba,
                        ..
                    } if image.is_none() => image = Some((*width, *height, rgba.clone())),
                    _ => {}
                }
            }
            let mentioned = names::mentions(&text, &server.bookmark.username);
            if !shown && (direct || in_voice || mentioned) {
                let name = &message.message.author;
                let author = names::author(name, server.users.values(), peer.as_ref());
                let author = names::author_name(name, author, &self.preferences).to_string();
                let preview = preview(&text);
                self.updates.push(Update::Notify(Notification {
                    key: format!("{}:{}", server.id, channel.id),
                    summary: if direct {
                        author
                    } else {
                        format!("{author} in {}", channel.name)
                    },
                    body: match (preview.is_empty(), image.is_some()) {
                        (true, true) => "Sent an image".into(),
                        _ => preview,
                    },
                    image,
                }));
            }
        }
        if !message.outgoing {
            let mentioned = names::mentions(&message.message.body, &server.bookmark.username);
            if let Some(sound) = crate::sounds::message(shown, direct, mentioned) {
                self.platform.play_sound(&server.id, sound);
            }
        }
        let channel = &mut server.channels[position];
        // Their message is here: they stopped typing it.
        let users = &server.users;
        channel.typing.retain(|session, _| {
            users
                .get(session)
                .is_none_or(|user| user.name != message.message.author)
        });
        if matches!(target, TextTarget::Direct(_))
            && !matches!(channel.target, TextTarget::DirectConversation(_))
        {
            channel.target = target;
            channel.direct_peer = peer;
            channel.storage_key = storage_key;
        } else if matches!(target, TextTarget::DirectConversation(_)) && peer.is_some() {
            channel.direct_peer = peer;
        }
        let outgoing = message.outgoing;
        let mention = !outgoing
            && (channel.is_direct()
                || message.document.blocks.iter().any(|block| {
                    matches!(block, ContentBlock::Text { text, .. }
                        if names::mentions(text, &server.bookmark.username))
                }));
        // Older history shown apart from the newest gets the message with
        // the newer pages.
        let added = channel.timeline.receive(message);
        let selected = self
            .navigation
            .selection()
            .is_some_and(|selected| selected.server == server.id && selected.channel == channel.id);
        let in_sight = self.focus.window_active && self.focus.following && selected;
        if added && !outgoing && !in_sight {
            channel.unread += 1;
            channel.unread_mention |= mention;
        }
        if selected {
            // Following keeps the newest in view; sending something of one's
            // own goes there too.
            if outgoing {
                self.focus.following = true;
                self.updates.push(Update::OwnMessage);
            } else if added && !self.focus.following {
                channel.new_messages += 1;
            }
        } else {
            channel.timeline.trim_oldest(MAX_TIMELINE);
        }
        if direct
            && server.notice.as_deref() == Some(nuschel_core::model::RECIPIENT_UNAVAILABLE_NOTICE)
        {
            server.dismiss_notice();
        }
    }

    fn server_event(&mut self, index: usize, event: ServerEvent) {
        let server = &mut self.servers[index];
        match event {
            // The driver keeps the sticky record. With history, the timeline
            // pages from the server (seqs), not from local observations.
            ServerEvent::Negotiated {
                capabilities,
                policy,
            } => {
                let history = capabilities
                    .iter()
                    .any(|capability| capability == "history/1");
                server.mls = capabilities.iter().any(|capability| capability == "mls/1");
                server.capabilities = capabilities;
                server.policy = policy;
                if history && server.history_source != HistorySource::ServerArchive {
                    server.history_source = HistorySource::ServerArchive;
                    for channel in &mut server.channels {
                        channel.timeline.reset();
                    }
                }
                self.updates.push(Update::Negotiated {
                    server: server.id.clone(),
                });
            }
            ServerEvent::ChannelInfo {
                channel,
                conversation,
            } => {
                let selected = self.navigation.selection().is_some_and(|selected| {
                    selected.server == server.id && selected.channel == channel
                });
                if let Some(view) = server.channel_view(channel) {
                    view.conversation = conversation;
                    if selected
                        && !view.timeline.loaded
                        && !view.timeline.loading
                        && let Err(error) = self.load_history(None)
                    {
                        self.updates.push(Update::HistoryFailed(error));
                    }
                }
            }
            ServerEvent::ChatAcked { key, seq } => {
                if !server.settle_pending(key, Some(seq)) {
                    server.acked.insert(key, Some(seq));
                }
            }
            ServerEvent::ChatFailed { key, reason } => {
                server.notice = Some(format!("A message was not sent: {reason}"));
                if !server.settle_pending(key, None) {
                    server.acked.insert(key, None);
                }
            }
            ServerEvent::ConversationState {
                channel,
                head,
                read,
            } => {
                let selected = self.navigation.selection().is_some_and(|selected| {
                    selected.server == server.id && selected.channel == channel
                });
                if let Some(view) = server.channel_view(channel)
                    && !selected
                {
                    view.unread = usize::try_from(head.saturating_sub(read))
                        .unwrap_or(usize::MAX)
                        .min(999);
                }
            }
            ServerEvent::Typing(typing) => {
                server.someone_typing(&typing.conversation, typing.session, self.now_ms);
            }
            ServerEvent::DirectList(directs) => self.directs_listed(index, directs),
            ServerEvent::DirectOpened(opened) => self.direct_opened(index, opened),
            ServerEvent::ExtensionError(error) => {
                if !self.share_failed(index, &error) {
                    self.direct_failed(index, error);
                }
            }
            ServerEvent::ShareLink(link) => self.share_reply(index, link),
            ServerEvent::ReadMarker(marker) => {
                // Read on our account's other device.
                if let Some(channel) = server.channels.iter_mut().find(|channel| {
                    channel
                        .conversation
                        .is_some_and(|id| id.as_slice() == marker.conversation.as_slice())
                }) {
                    channel.read_seq = channel.read_seq.max(marker.seq as i64);
                    channel.unread = 0;
                    channel.unread_mention = false;
                }
            }
            ServerEvent::PeerCertificate(pin) => {
                let identity = server.history_identity();
                server.peer_sha256 = Some(pin.clone());
                server.security = SessionSecurity::TransportOnly;
                if server.bookmark.certificate_sha256.is_none() {
                    server.bookmark.certificate_sha256 = Some(pin);
                    self.updates.push(Update::BookmarksChangedNow);
                }
                // Reconnecting to the same server leaves the timeline (and
                // where the reader is in it) alone; a new identity has
                // other history.
                if server.history_identity() != identity {
                    for channel in &mut server.channels {
                        channel.timeline.loaded = false;
                    }
                    if self
                        .navigation
                        .selection()
                        .is_some_and(|selected| selected.server == server.id)
                        && let Err(error) = self.load_history(None)
                    {
                        self.updates.push(Update::HistoryFailed(error));
                    }
                }
            }
            ServerEvent::Connected { session } => {
                server.channels.retain(|channel| match channel.target {
                    TextTarget::Channel(id) => server.seen_channels.contains(&id),
                    TextTarget::Direct(_) | TextTarget::DirectConversation(_) => true,
                });
                let cached = server.bookmark.cached_channels.len();
                server
                    .bookmark
                    .cached_channels
                    .retain(|channel| server.seen_channels.contains(&channel.id));
                if server.bookmark.cached_channels.len() != cached {
                    self.updates.push(Update::BookmarksChanged);
                }
                server.status = Status::Connected;
                server.reconnecting = None;
                server.own_session = Some(session);
                server.expand_occupied();
                self.passwords.remove(&server.id);
                // A share link's token decides first; its path is the fallback.
                if server.share_token.is_some() && server.has(nuschel::capability::SHARE) {
                    self.share_connected(index);
                } else {
                    server.share_token = None;
                    self.join_pending(index);
                }
            }
            ServerEvent::Channel(update) => {
                server.seen_channels.insert(update.id);
                let cached = CachedChannel {
                    id: update.id,
                    parent: update.parent,
                    name: update.name.clone(),
                    position: update.position,
                };
                // Most updates (descriptions, links, a resent tree) leave the cache as it is.
                if server.bookmark.cache_channel(cached) {
                    self.updates.push(Update::BookmarksChanged);
                }
                let render =
                    |description: &str| (!description.is_empty()).then(|| description.to_owned());
                let mut rendering = None;
                if let Some(channel) = server
                    .channels
                    .iter_mut()
                    .find(|channel| channel.target == TextTarget::Channel(update.id))
                {
                    channel.name = update.name;
                    channel.parent = update.parent;
                    channel.position = update.position;
                    channel.topic = "Channel chat · select Join voice to move here".into();
                    channel.links = update.links;
                    channel.temporary = update.temporary;
                    channel.max_users = update.max_users;
                    if channel.description_raw != update.description {
                        rendering = render(&update.description);
                        channel.description_raw = update.description;
                    }
                } else {
                    rendering = render(&update.description);
                    server.channels.push(ConversationState {
                        topic: "Channel chat · select Join voice to move here".into(),
                        links: update.links,
                        temporary: update.temporary,
                        max_users: update.max_users,
                        description_raw: update.description,
                        ..ConversationState::channel(
                            update.id,
                            update.parent,
                            update.name,
                            update.position,
                        )
                    });
                }
                if let Some(source) = rendering {
                    self.platform.render_html(
                        server.id.clone(),
                        server.generation,
                        RenderKey::Description(update.id),
                        source,
                    );
                }
                // Sorted once the batch of events is applied, not per channel.
                server.unsorted = true;
            }
            ServerEvent::Permissions {
                channel,
                permissions,
                flush,
            } => {
                if flush {
                    server.permissions.clear();
                }
                if channel != u32::MAX {
                    server.permissions.insert(channel, permissions);
                }
            }
            ServerEvent::Activity(activity) => {
                let mut targets = activity.channels.clone();
                // A friend's arrival, departure or new name shows
                // server-wide, as in Mumble's log.
                let friend = activity.presence.is_some()
                    && names::about_friend(&activity, &self.preferences);
                if targets.is_empty() || friend {
                    targets.push(0);
                    targets.extend(server.voice_channel());
                }
                targets.sort_unstable();
                targets.dedup();
                let text = names::activity_text(&activity, &self.preferences);
                for id in targets {
                    if let Some(channel) = server.channel_view(id) {
                        channel.log(text.clone(), activity.observed_at_ms, false);
                    }
                }
                if friend
                    && activity.presence == Some(Presence::Connected)
                    && self.preferences.friend_notifications
                    && !self.focus.window_active
                {
                    self.updates.push(Update::Notify(Notification {
                        key: format!("{}:friends", server.id),
                        summary: text,
                        body: server.name.clone(),
                        image: None,
                    }));
                }
            }
            ServerEvent::Limits(limits) => server.limits = Some(limits),
            ServerEvent::Stats(stats) => {
                server.stats.insert(stats.session, stats);
            }
            ServerEvent::Suggest {
                push_to_talk,
                minimum_version,
            } => {
                let mut tips = Vec::new();
                if push_to_talk == Some(true) && !self.preferences.push_to_talk {
                    tips.push("This server recommends push-to-talk.".to_string());
                }
                if let Some(version) = minimum_version {
                    tips.push(format!(
                        "This server recommends Mumble {version} or newer; some features may be missing."
                    ));
                }
                server.suggestion = (!tips.is_empty()).then(|| tips.join(" "));
            }
            ServerEvent::ContextActions(actions) => server.context_actions = actions,
            ServerEvent::RegisteredUsers(users) => server.registered_users = Some(users),
            ServerEvent::Bans(bans) => server.bans = Some(bans),
            ServerEvent::Acl(acl) => {
                let unknown: Vec<u32> = acl
                    .entries
                    .iter()
                    .filter_map(|entry| entry.user_id)
                    .chain(
                        acl.groups
                            .iter()
                            .flat_map(|group| group.add.iter().copied()),
                    )
                    .filter(|id| !server.user_names.contains_key(id))
                    .collect();
                if !unknown.is_empty() {
                    let _ = self
                        .platform
                        .request(&server.id, Request::QueryUserNames(unknown));
                }
                server.acl = Some(acl);
            }
            ServerEvent::UserNames(names) => server.user_names.extend(names),
            ServerEvent::Outbox { sent, total } => {
                server.outbox = (sent < total).then_some((sent, total));
            }
            ServerEvent::UnsupportedCodec { session, at_ms } => {
                server.legacy_voice.insert(session);
                if let Some(user) = server.users.get(&session).cloned()
                    && let Some(channel) = server.channel_view(user.channel)
                {
                    channel.log(
                        format!(
                            "{} uses a legacy voice codec (CELT/Speex) that Nuschel cannot play.",
                            names::display_name(&user, &self.preferences)
                        ),
                        at_ms,
                        true,
                    );
                }
            }
            ServerEvent::ChannelRemoved(id) => {
                server.expansion.remove(&id);
                server.permissions.remove(&id);
                let cached = server.bookmark.cached_channels.len();
                server
                    .bookmark
                    .cached_channels
                    .retain(|channel| channel.id != id);
                if server.bookmark.cached_channels.len() != cached {
                    self.updates.push(Update::BookmarksChanged);
                }
                server
                    .channels
                    .retain(|channel| channel.target != TextTarget::Channel(id));
                if server.channels.is_empty() {
                    server.channels.push(ConversationState::root());
                }
            }
            ServerEvent::User(mut user) => {
                user.muted |= user.deafened;
                user.self_muted |= user.self_deafened;
                if let Some(hash) = user.texture_hash.clone()
                    && !server.avatars.contains(&hash)
                {
                    if let Some(texture) = user.texture.clone() {
                        if server.avatar_requests.insert(format!("decode:{hash}")) {
                            self.platform.decode_avatar(
                                server.id.clone(),
                                server.generation,
                                hash,
                                texture,
                            );
                        }
                    } else if server.voice_channel() == Some(user.channel)
                        && server.avatar_requests.insert(hash)
                    {
                        let _ = self.platform.request(
                            &server.id,
                            Request::RequestBlobs {
                                textures: vec![user.session],
                                comments: vec![],
                                descriptions: vec![],
                            },
                        );
                    }
                }
                if server.status == Status::Connected && Some(user.session) != server.own_session {
                    let from = server.users.get(&user.session).map(|old| old.channel);
                    if let Some(sound) =
                        crate::sounds::movement(server.voice_channel(), from, Some(user.channel))
                    {
                        self.platform.play_sound(&server.id, sound);
                    }
                }
                if Some(user.session) == server.own_session {
                    // These controls are local privacy intent, also used by
                    // the native capture gate. Mumble has no request IDs on
                    // self-state echoes: an old echo must not undo a newer
                    // mute/deafen or drive the next rapid toggle from stale
                    // state. Keep the server's observation in `users` below.
                    // Remembered for the next start (the root channel is
                    // where a server puts you anyway).
                    let last = (user.channel != 0).then_some(user.channel);
                    if server.bookmark.last_channel != last {
                        server.bookmark.last_channel = last;
                        self.updates.push(Update::BookmarksChanged);
                    }
                    self.navigation.set_voice(Some(Conversation {
                        server: server.id.clone(),
                        channel: user.channel,
                    }));
                }
                let arrived = user.clone();
                server.users.insert(user.session, user);
                for position in 0..server.channels.len() {
                    let channel = &server.channels[position];
                    let same = channel.direct_peer.as_ref().is_some_and(|old| {
                        (old.instance == arrived.instance && old.session == arrived.session)
                            || old
                                .certificate_hash
                                .as_deref()
                                .filter(|hash| !hash.is_empty())
                                .is_some_and(|hash| {
                                    arrived
                                        .certificate_hash
                                        .as_deref()
                                        .is_some_and(|current| current.eq_ignore_ascii_case(hash))
                                })
                    }) || arrived
                        .certificate_hash
                        .as_deref()
                        .is_some_and(|hash| channel.storage_key == format!("direct:{hash}"));
                    if same {
                        server.refresh_direct(position);
                    }
                }
            }
            ServerEvent::UserRemoved(session) => {
                let from = server.users.get(&session).map(|old| old.channel);
                if server.status == Status::Connected
                    && Some(session) != server.own_session
                    && let Some(sound) = crate::sounds::movement(server.voice_channel(), from, None)
                {
                    self.platform.play_sound(&server.id, sound);
                }
                server.users.remove(&session);
                server.talking.remove(&session);
                server.legacy_voice.remove(&session);
            }
            ServerEvent::Notice(message) | ServerEvent::PermissionDenied(message) => {
                server.notice_expires = Some((message.clone(), self.now_ms.saturating_add(10_000)));
                server.notice = Some(message);
                if self
                    .sending
                    .as_ref()
                    .is_some_and(|pending| pending.conversation.server == server.id)
                {
                    self.sending = None;
                }
            }
            ServerEvent::Welcome(text) => server.welcome = Some(text),
            ServerEvent::Transport(stats) => server.transport = Some(stats),
        }
    }

    /// The server named one of our direct conversations (opened by us).
    fn direct_opened(&mut self, index: usize, opened: v1::DirectOpened) {
        let request = self.direct_requests.remove(&opened.request_id);
        let Some(direct) = opened.direct else {
            return;
        };
        let Some(id) = nuschel::id(&direct.conversation) else {
            return;
        };
        let server = &mut self.servers[index];
        let peer = server.online(direct.user_id).cloned();
        let name = peer
            .as_ref()
            .map(|peer| names::display_name(peer, &self.preferences).to_string())
            .unwrap_or_else(|| direct.name.clone());
        server.directs.insert(id, direct);
        let channel = server.direct_view(id, &name, peer);
        if request.is_some_and(|(_, _, open)| open) {
            self.updates.push(Update::Browse {
                server: server.id.clone(),
                channel: server.channels[channel].id,
            });
        }
    }

    /// Our direct conversations and how far we read them: unread ones get a
    /// place in the list (with their count) even while the other is offline.
    fn directs_listed(&mut self, index: usize, directs: Vec<v1::Direct>) {
        let selected = self.navigation.selection().cloned();
        let server = &mut self.servers[index];
        for direct in directs {
            let Some(id) = nuschel::id(&direct.conversation) else {
                continue;
            };
            let unread = direct.head.saturating_sub(direct.read);
            let shown = server
                .channels
                .iter()
                .any(|channel| channel.conversation == Some(id));
            if shown || unread > 0 {
                let peer = server.online(direct.user_id).cloned();
                let name = peer
                    .as_ref()
                    .map(|peer| names::display_name(peer, &self.preferences).to_string())
                    .unwrap_or_else(|| {
                        if direct.name.is_empty() {
                            "Deleted user".into()
                        } else {
                            direct.name.clone()
                        }
                    });
                let channel = server.direct_view(id, &name, peer);
                let channel = &mut server.channels[channel];
                channel.read_seq = channel.read_seq.max(direct.read as i64);
                let open = selected.as_ref().is_some_and(|selected| {
                    selected.server == server.id && selected.channel == channel.id
                });
                if !open {
                    channel.unread = usize::try_from(unread).unwrap_or(usize::MAX).min(999);
                    // A direct message is meant for us, as a mention is.
                    channel.unread_mention = unread > 0;
                }
            }
            server.directs.insert(id, direct);
        }
    }

    /// The server would not open the conversation (the other person cannot
    /// receive text there, say): fall back to the stock path.
    /// Join the channel a link named by its path, once the tree is known.
    fn join_pending(&mut self, index: usize) {
        let server = &mut self.servers[index];
        let path = std::mem::take(&mut server.pending_channel);
        if path.is_empty() {
            return;
        }
        let mut current = 0;
        let mut found = true;
        for name in &path {
            match server.channels.iter().find(|channel| {
                channel.parent == Some(current)
                    && channel.name == *name
                    && matches!(channel.target, TextTarget::Channel(_))
            }) {
                Some(channel) => current = channel.id,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            let _ = self.platform.request(&server.id, Request::Join(current));
            self.updates.push(Update::Browse {
                server: server.id.clone(),
                channel: current,
            });
        } else {
            server.notice = Some(format!("Channel /{} was not found.", path.join("/")));
        }
    }

    fn direct_failed(&mut self, index: usize, error: v1::Error) {
        let Some((server_id, session, open)) = self.direct_requests.remove(&error.request_id)
        else {
            return;
        };
        let server = &self.servers[index];
        if server.id != server_id || !open {
            return;
        }
        if let Some(peer) = server.users.get(&session).cloned() {
            let channel = self.stock_direct(index, &peer);
            self.updates.push(Update::Browse {
                server: server_id,
                channel,
            });
        }
    }
}

/// A short, single-paragraph preview of a message body (for a notification).
pub fn preview(body: &str) -> String {
    let flat: String = body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| *c != '\u{1e}')
        .collect();
    if flat.chars().count() > 160 {
        format!("{}…", flat.chars().take(160).collect::<String>())
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        HistoryTicket,
        model::{Channel, User},
        timeline::IncomingMessage,
    };
    use nuschel_core::ChatMessage;
    use std::sync::{Arc, Mutex};

    /// A platform that records what the client asked of it.
    #[derive(Default)]
    struct Recorder {
        requests: Mutex<Vec<(String, Request)>>,
        pages: Mutex<Vec<(String, HistoryTicket, Page)>>,
        sounds: Mutex<Vec<Sound>>,
    }

    impl Platform for Recorder {
        fn connect(&self, _: ServerBookmark, _: Option<String>, _: bool) -> Result<u64, String> {
            Ok(1)
        }
        fn disconnect(&self, _: &str) {}
        fn request(&self, server: &str, request: Request) -> Result<(), String> {
            self.requests
                .lock()
                .unwrap()
                .push((server.to_owned(), request));
            Ok(())
        }
        fn local_history(
            &self,
            server: String,
            _: String,
            _: String,
            ticket: HistoryTicket,
            page: Page,
        ) -> Result<(), String> {
            self.pages.lock().unwrap().push((server, ticket, page));
            Ok(())
        }
        fn server_history(
            &self,
            server: String,
            _: [u8; 16],
            ticket: HistoryTicket,
            _: Option<CacheKey>,
            page: Page,
        ) -> Result<(), String> {
            self.pages.lock().unwrap().push((server, ticket, page));
            Ok(())
        }
        fn change_message(&self, _: ChangeRequest) -> Result<(), String> {
            Ok(())
        }
        fn render_html(&self, _: String, _: u64, _: RenderKey, _: String) {}
        fn decode_avatar(&self, _: String, _: u64, _: String, _: Vec<u8>) {}
        fn play_sound(&self, _: &str, sound: Sound) {
            self.sounds.lock().unwrap().push(sound);
        }
        fn chime(&self, _: Sound) {}
        fn search(
            &self,
            _: u64,
            _: (Vec<String>, Option<String>),
            _: String,
            _: Option<SearchCursor>,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    /// A client connected as `andi` (session 1, in channel 2) to a server
    /// with channels 2 and 3 under the root.
    fn connected() -> (Client, Arc<Recorder>, String) {
        let recorder = Arc::new(Recorder::default());
        let bookmark = ServerBookmark::new("Test", "localhost", 64738, "andi").unwrap();
        let id = bookmark.id.clone();
        let mut client = Client::new(
            Box::new(recorder.clone()),
            vec![bookmark],
            Preferences {
                notifications: true,
                ..Default::default()
            },
        );
        client.connect(0, None, false);
        let server = |event| Input::Server {
            server: id.clone(),
            generation: 1,
            event,
        };
        client.handle(server(ServerEvent::PeerCertificate("ab".repeat(32))));
        for (channel, name) in [(2, "Two"), (3, "Three")] {
            client.handle(server(ServerEvent::Channel(Channel {
                id: channel,
                parent: Some(0),
                name: name.into(),
                ..Default::default()
            })));
        }
        client.handle(server(ServerEvent::Connected { session: 1 }));
        client.handle(server(ServerEvent::User(User {
            session: 1,
            name: "andi".into(),
            channel: 2,
            ..Default::default()
        })));
        client.settle();
        client.take_updates();
        (client, recorder, id)
    }

    #[test]
    fn private_sending_refreshes_an_online_recipient_without_reopening_the_view() {
        let (mut client, recorder, _id) = connected();
        let peer = User {
            session: 4,
            instance: 10,
            certificate_hash: Some("abc".into()),
            ..Default::default()
        };
        client.servers[0].users.insert(4, peer.clone());
        let channel = client.open_direct(0, 4).unwrap();
        client.open(0, channel).unwrap();
        client.servers[0].users.remove(&4);
        client.servers[0].users.insert(
            9,
            User {
                session: 9,
                instance: 11,
                ..peer
            },
        );
        client.send_text("still here".into()).unwrap();
        assert!(recorder.requests.lock().unwrap().iter().any(|(_, request)| matches!(request,
            Request::SendText { target: TextTarget::Direct(9), expected_peer: Some(11), body } if body == "still here")));
        assert_eq!(client.navigation.selection().unwrap().channel, channel);
    }

    #[test]
    fn private_send_notices_expire_and_newer_errors_are_preserved() {
        let (mut client, _, id) = connected();
        client.set_time(100);
        client.handle(Input::Server {
            server: id.clone(),
            generation: 1,
            event: ServerEvent::Notice(nuschel_core::model::RECIPIENT_UNAVAILABLE_NOTICE.into()),
        });
        assert!(client.servers[0].has_temporary_notice());
        assert!(!client.set_time(10_099));
        assert!(client.set_time(10_100));
        assert!(client.servers[0].notice.is_none());
        client.handle(Input::Server {
            server: id,
            generation: 1,
            event: ServerEvent::Notice("old transient".into()),
        });
        client.servers[0].notice = Some("a newer problem".into());
        client.set_time(20_100);
        assert_eq!(client.servers[0].notice.as_deref(), Some("a newer problem"));
        client.servers[0].dismiss_notice();
        assert!(client.servers[0].notice.is_none());
        assert!(!client.servers[0].has_temporary_notice());
    }

    #[test]
    fn receiving_refreshes_the_existing_private_view_and_clears_its_failure_notice() {
        let (mut client, _, id) = connected();
        let old = User {
            session: 4,
            instance: 10,
            certificate_hash: Some("abc".into()),
            ..Default::default()
        };
        client.servers[0].users.insert(4, old.clone());
        let channel = client.open_direct(0, 4).unwrap();
        client.open(0, channel).unwrap();
        client.servers[0].notice = Some(nuschel_core::model::RECIPIENT_UNAVAILABLE_NOTICE.into());
        let current = User {
            session: 9,
            instance: 11,
            ..old
        };
        client.servers[0].users.remove(&4);
        client.servers[0].users.insert(9, current.clone());
        let mut incoming = message(&id, 0, "I am here", 1);
        if let Input::Message {
            target,
            storage_key,
            peer,
            ..
        } = &mut incoming
        {
            *target = TextTarget::Direct(9);
            *storage_key = "direct:abc".into();
            *peer = Some(current);
        }
        client.handle(incoming);
        let position = client.servers[0]
            .channels
            .iter()
            .position(|c| c.id == channel)
            .unwrap();
        assert_eq!(
            client.servers[0].message_target(position),
            Ok((TextTarget::Direct(9), Some(11)))
        );
        assert!(client.servers[0].notice.is_none());
        assert_eq!(client.navigation.selection().unwrap().channel, channel);
    }

    fn message(id: &str, channel: u32, body: &str, at: i64) -> Input {
        Input::Message {
            server: id.into(),
            generation: 1,
            target: TextTarget::Channel(channel),
            storage_key: channel.to_string(),
            peer: None,
            message: IncomingMessage::plain(
                Some(at),
                ChatMessage {
                    server: String::new(),
                    channel: channel.to_string(),
                    author: "Ada".into(),
                    observed_at_ms: at,
                    body: body.into(),
                },
                MessageDocument {
                    blocks: vec![ContentBlock::text(body)],
                },
                false,
            ),
        }
    }

    /// What the client told each server about our mute and deafen, in order.
    fn self_states(recorder: &Recorder) -> Vec<(String, bool, bool)> {
        recorder
            .requests
            .lock()
            .unwrap()
            .drain(..)
            .filter_map(|(server, request)| match request {
                Request::SetSelf { muted, deafened } => Some((server, muted, deafened)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn rapid_deafen_toggles_ignore_delayed_and_out_of_order_server_echoes() {
        let (mut client, recorder, id) = connected();
        client.set_mute(0, false);
        assert!(!client.servers[0].muted && !client.servers[0].deafened);
        self_states(&recorder);
        for i in 0..101 {
            client.set_mute(0, true);
            let expected = (true, i % 2 == 0);
            assert_eq!(
                (client.servers[0].muted, client.servers[0].deafened),
                expected
            );
            // Mumble has no acknowledgement request number on these echoes:
            // none may overwrite this client's latest privacy intent.
            for (muted, deafened) in [(false, false), (false, true), (true, false)] {
                client.handle(Input::Server {
                    server: id.clone(),
                    generation: 1,
                    event: ServerEvent::User(User {
                        session: 1,
                        name: "andi".into(),
                        channel: 2,
                        self_muted: muted,
                        self_deafened: deafened,
                        ..Default::default()
                    }),
                });
                assert_eq!(
                    (client.servers[0].muted, client.servers[0].deafened),
                    expected
                );
                let observed = &client.servers[0].users[&1];
                assert!(!observed.self_deafened || observed.self_muted);
            }
        }
        assert_eq!(client.voice_role(0), voice::Role::Deafened);
        assert!(
            self_states(&recorder)
                .iter()
                .all(|(_, muted, deafened)| !deafened || *muted)
        );
    }

    #[test]
    fn your_voice_goes_to_one_server_and_a_second_one_asks_first() {
        let recorder = Arc::new(Recorder::default());
        let bookmarks: Vec<_> = ["alpha", "beta"]
            .into_iter()
            .map(|name| ServerBookmark::new(name, "localhost", 64738, "andi").unwrap())
            .collect();
        let (a, b) = (bookmarks[0].id.clone(), bookmarks[1].id.clone());
        let mut client = Client::new(
            Box::new(recorder.clone()),
            bookmarks,
            Preferences::default(),
        );
        client.connect(0, None, false);
        client.connect(1, None, false);
        for (index, id) in [(0, &a), (1, &b)] {
            client.handle(Input::Server {
                server: id.clone(),
                generation: 1,
                event: ServerEvent::Connected { session: 1 },
            });
            assert_eq!(client.servers[index].status, Status::Connected);
        }
        // The newest connection is the talk server; the first listens.
        assert_eq!(client.voice.talk(), std::slice::from_ref(&b));
        assert_eq!(client.voice_role(0), voice::Role::Listening);
        assert_eq!(client.voice_role(1), voice::Role::Muted);
        // Joining a channel on alpha moves the (muted) voice there.
        client.join(0, 2);
        assert_eq!(client.voice.talk(), std::slice::from_ref(&a));
        assert!(
            self_states(&recorder).is_empty(),
            "a muted voice moved quietly"
        );
        assert_eq!(client.set_mute(0, false), None);
        assert_eq!(self_states(&recorder), [(a.clone(), false, false)]);
        assert_eq!(client.voice_role(0), voice::Role::Talking);
        // Unmuting beta asks first, and sends nothing until answered.
        assert_eq!(
            client.set_mute(1, false),
            Some(SecondServer {
                server: 1,
                talking: vec![0]
            })
        );
        assert!(self_states(&recorder).is_empty());
        assert!(client.servers[1].muted);
        // "Only here": the microphone moves to beta.
        client.speak_here(1, false);
        assert_eq!(
            self_states(&recorder),
            [(a.clone(), true, false), (b.clone(), false, false)]
        );
        // The mute shortcut acts on beta alone; deafening alpha is its own.
        assert_eq!(client.toggle_voice(false), None);
        assert_eq!(self_states(&recorder), [(b.clone(), true, false)]);
        assert_eq!(client.set_mute(0, true), None);
        assert_eq!(self_states(&recorder), [(a.clone(), true, true)]);
        assert_eq!(client.voice_role(0), voice::Role::Deafened);
        // "Use voice here" on alpha: the talk server, undeafened, still muted.
        client.use_voice_here(0);
        assert_eq!(self_states(&recorder), [(a.clone(), true, false)]);
        assert_eq!(client.voice.talk(), std::slice::from_ref(&a));
        // Its disconnection hands the talk role to beta.
        client.disconnect(0);
        assert_eq!(client.voice.talk(), [b]);
    }

    #[test]
    fn what_arrives_out_of_sight_is_unread_and_a_mention_notifies() {
        let (mut client, recorder, id) = connected();
        assert_eq!(client.open(0, 2), Ok(true));
        client.handle(message(&id, 2, "in front of you", 10));
        client.handle(message(&id, 3, "elsewhere", 20));
        client.handle(message(&id, 3, "hey andi", 30));
        let three = &client.servers[0]
            .channels
            .iter()
            .find(|c| c.id == 3)
            .unwrap();
        assert_eq!((three.unread, three.unread_mention), (2, true));
        let two = &client.servers[0]
            .channels
            .iter()
            .find(|c| c.id == 2)
            .unwrap();
        assert_eq!(two.unread, 0, "shown at its newest message");
        let notes: Vec<_> = client
            .take_updates()
            .into_iter()
            .filter_map(|update| match update {
                Update::Notify(note) => Some(note.summary),
                _ => None,
            })
            .collect();
        assert_eq!(notes, ["Ada in Three"], "only the mention notifies");
        assert_eq!(
            *recorder.sounds.lock().unwrap(),
            [Sound::Message, Sound::Mention]
        );
        // Scrolled up: a new message waits below and counts as unread.
        client.focus.following = false;
        client.handle(message(&id, 2, "while you read", 40));
        let two = &client.servers[0]
            .channels
            .iter()
            .find(|c| c.id == 2)
            .unwrap();
        assert_eq!((two.new_messages, two.unread), (1, 1));
        client.focus.following = true;
        client.mark_read();
        let two = &client.servers[0]
            .channels
            .iter()
            .find(|c| c.id == 2)
            .unwrap();
        assert_eq!(two.unread, 0);
    }

    #[test]
    fn one_message_at_a_time_until_the_session_took_it() {
        let (mut client, recorder, id) = connected();
        client.open(0, 3).unwrap();
        assert_eq!(client.send_text("  ".into()), Err(SendError::Nothing));
        assert_eq!(client.send_text("hello".into()), Ok(()));
        assert!(client.sending());
        assert_eq!(client.send_text("again".into()), Err(SendError::Nothing));
        client.handle(Input::Sent {
            server: id.clone(),
            generation: 1,
            target: TextTarget::Channel(3),
            body: "hello".into(),
        });
        assert!(!client.sending());
        assert!(client.take_updates().iter().any(|update| matches!(
            update,
            Update::Sent { conversation, body } if conversation.channel == 3 && body == "hello"
        )));
        let sent = recorder.requests.lock().unwrap().clone();
        assert!(sent.contains(&(
            id,
            Request::SendText {
                target: TextTarget::Channel(3),
                body: "hello".into(),
                expected_peer: None,
            }
        )));
        assert_eq!(
            client.send_text("x".repeat(MAX_CONTENT_BYTES + 1)),
            Err(SendError::TooLong)
        );
    }

    #[test]
    fn a_page_for_a_conversation_left_meanwhile_is_dropped() {
        let (mut client, recorder, id) = connected();
        client.open(0, 2).unwrap();
        client.open(0, 3).unwrap();
        let pages = recorder.pages.lock().unwrap().clone();
        assert_eq!(
            pages.len(),
            2,
            "each opened conversation asked for its latest page"
        );
        let history = |ticket: &HistoryTicket| Input::History {
            server: id.clone(),
            ticket: ticket.clone(),
            before: None,
            messages: Vec::new(),
            next_before: None,
        };
        client.handle(history(&pages[0].1));
        let two = client.servers[0]
            .channels
            .iter()
            .find(|c| c.id == 2)
            .unwrap();
        assert!(!two.timeline.loaded, "late: another conversation is open");
        client.handle(history(&pages[1].1));
        let three = client.servers[0]
            .channels
            .iter()
            .find(|c| c.id == 3)
            .unwrap();
        assert!(three.timeline.loaded && !three.timeline.loading);
        assert!(client.take_updates().iter().any(|update| matches!(
            update,
            Update::PageShown {
                page: Page::Before(None),
                used: true
            }
        )));
    }

    #[test]
    fn a_links_channel_is_joined_once_the_tree_is_known() {
        let recorder = Arc::new(Recorder::default());
        let bookmark = ServerBookmark::new("Test", "localhost", 64738, "andi").unwrap();
        let id = bookmark.id.clone();
        let mut client = Client::new(
            Box::new(recorder.clone()),
            vec![bookmark],
            Preferences::default(),
        );
        client.servers[0].pending_channel = vec!["Two".into()];
        client.connect(0, None, true);
        let server = |event| Input::Server {
            server: id.clone(),
            generation: 1,
            event,
        };
        client.handle(server(ServerEvent::Channel(Channel {
            id: 2,
            parent: Some(0),
            name: "Two".into(),
            ..Default::default()
        })));
        client.handle(server(ServerEvent::Connected { session: 1 }));
        assert!(
            recorder
                .requests
                .lock()
                .unwrap()
                .contains(&(id.clone(), Request::Join(2)))
        );
        assert!(
            client
                .take_updates()
                .iter()
                .any(|update| matches!(update, Update::Browse { channel: 2, .. }))
        );
        // A stale connection's reports are dropped.
        client.handle(Input::Server {
            server: id,
            generation: 0,
            event: ServerEvent::Welcome("old".into()),
        });
        assert_eq!(client.servers[0].welcome, None);
    }

    #[test]
    fn previews_are_short_and_flat() {
        assert_eq!(preview("one\n\ntwo   three"), "one two three");
        assert!(preview(&"x".repeat(500)).ends_with('…'));
    }

    // ---- share links (docs/web.md) ----

    fn share_input(id: &str, event: ServerEvent) -> Input {
        Input::Server {
            server: id.into(),
            generation: 1,
            event,
        }
    }

    #[test]
    fn share_links_name_the_channel_without_a_token() {
        let (mut client, recorder, _) = connected();
        let link = client.share_link(0, 3).unwrap().expect("at once");
        assert_eq!(link.token, None);
        assert_eq!(link.channel, ["Three"]);
        assert_eq!(link.host, "localhost");
        assert_eq!(link.certificate_sha256, Some("ab".repeat(32)));
        assert!(!link.require_nuschel, "a stock server");
        assert!(recorder.requests.lock().unwrap().is_empty());
        assert!(client.share_link(0, 99).is_err());
    }

    #[test]
    fn share_links_come_from_the_server_with_share_1() {
        let (mut client, recorder, id) = connected();
        client.handle(share_input(
            &id,
            ServerEvent::Negotiated {
                capabilities: vec!["history/1".into(), "share/1".into()],
                policy: None,
            },
        ));
        client.handle(share_input(
            &id,
            ServerEvent::User(User {
                session: 1,
                name: "andi".into(),
                channel: 2,
                user_id: Some(5),
                ..Default::default()
            }),
        ));
        assert_eq!(client.share_link(0, 3).unwrap(), None);
        let request_id = match recorder.requests.lock().unwrap().pop() {
            Some((
                _,
                Request::CreateShareLink {
                    request_id,
                    channel: 3,
                    ..
                },
            )) => request_id,
            other => panic!("{other:?}"),
        };
        client.take_updates();
        client.handle(share_input(
            &id,
            ServerEvent::ShareLink(v1::ShareLink {
                request_id,
                token: vec![7; 16],
                channel: 3,
                web_host: "web.example.org".into(),
                ..Default::default()
            }),
        ));
        let update = client
            .take_updates()
            .into_iter()
            .find(|update| matches!(update, Update::ShareLink { .. }));
        let Some(Update::ShareLink {
            channel,
            link,
            web_host,
            ..
        }) = update
        else {
            panic!("no share link");
        };
        assert_eq!(channel, 3);
        assert_eq!(web_host.as_deref(), Some("web.example.org"));
        assert_eq!(
            link.token.as_deref(),
            Some(nuschel_core::share::encode_token(&[7; 16]).as_str())
        );
        assert!(link.require_nuschel);
        // An answer nobody asked for is ignored.
        client.handle(share_input(
            &id,
            ServerEvent::ShareLink(v1::ShareLink {
                request_id,
                token: vec![7; 16],
                ..Default::default()
            }),
        ));
        assert!(client.take_updates().is_empty());
    }

    #[test]
    fn opening_a_share_link_resolves_its_token_then_joins() {
        let recorder = Arc::new(Recorder::default());
        let bookmark = ServerBookmark::new("Test", "localhost", 64738, "andi").unwrap();
        let id = bookmark.id.clone();
        let mut client = Client::new(
            Box::new(recorder.clone()),
            vec![bookmark],
            Preferences::default(),
        );
        let token = nuschel_core::share::encode_token(&[9; 16]);
        client.open_share_token(0, token);
        client.servers[0].pending_channel = vec!["Two".into()];
        client.connect(0, None, false);
        client.handle(share_input(
            &id,
            ServerEvent::Negotiated {
                capabilities: vec!["share/1".into()],
                policy: None,
            },
        ));
        client.handle(share_input(
            &id,
            ServerEvent::Channel(Channel {
                id: 2,
                parent: Some(0),
                name: "Two".into(),
                ..Default::default()
            }),
        ));
        client.handle(share_input(&id, ServerEvent::Connected { session: 1 }));
        // The token first; the path waits as the fallback.
        let request_id = match recorder.requests.lock().unwrap().pop() {
            Some((_, Request::ResolveShareLink { request_id, token })) => {
                assert_eq!(token, [9; 16]);
                request_id
            }
            other => panic!("{other:?}"),
        };
        client.handle(share_input(
            &id,
            ServerEvent::ShareLink(v1::ShareLink {
                request_id,
                token: vec![9; 16],
                channel: 2,
                access_token: "door".into(),
                ..Default::default()
            }),
        ));
        let requests: Vec<Request> = recorder
            .requests
            .lock()
            .unwrap()
            .drain(..)
            .map(|(_, request)| request)
            .collect();
        assert!(
            matches!(&requests[..], [Request::SetTokens(tokens), Request::Join(2)] if tokens == &["door".to_string()]),
            "{requests:?}"
        );
        assert_eq!(client.servers[0].bookmark.tokens, ["door"]);
        assert!(client.servers[0].pending_channel.is_empty());
        let updates = client.take_updates();
        assert!(
            updates
                .iter()
                .any(|update| matches!(update, Update::BookmarksChanged))
        );
        assert!(
            updates
                .iter()
                .any(|update| matches!(update, Update::Browse { channel: 2, .. }))
        );

        // A token the server no longer knows: the path still leads there.
        client.open_share_token(0, nuschel_core::share::encode_token(&[8; 16]));
        client.servers[0].pending_channel = vec!["Two".into()];
        let request_id = match recorder.requests.lock().unwrap().pop() {
            Some((_, Request::ResolveShareLink { request_id, .. })) => request_id,
            other => panic!("{other:?}"),
        };
        client.handle(share_input(
            &id,
            ServerEvent::ExtensionError(v1::Error {
                request_id,
                code: v1::error::Code::NotFound as i32,
                ..Default::default()
            }),
        ));
        let requests: Vec<Request> = recorder
            .requests
            .lock()
            .unwrap()
            .drain(..)
            .map(|(_, request)| request)
            .collect();
        assert!(matches!(&requests[..], [Request::Join(2)]), "{requests:?}");

        // A server without share/1: the path decides.
        let recorder = Arc::new(Recorder::default());
        let bookmark = ServerBookmark::new("Test", "localhost", 64738, "andi").unwrap();
        let id = bookmark.id.clone();
        let mut client = Client::new(
            Box::new(recorder.clone()),
            vec![bookmark],
            Preferences::default(),
        );
        client.open_share_token(0, nuschel_core::share::encode_token(&[1; 16]));
        client.servers[0].pending_channel = vec!["Three".into()];
        client.connect(0, None, false);
        client.handle(share_input(
            &id,
            ServerEvent::Channel(Channel {
                id: 3,
                parent: Some(0),
                name: "Three".into(),
                ..Default::default()
            }),
        ));
        client.handle(share_input(&id, ServerEvent::Connected { session: 1 }));
        assert_eq!(client.servers[0].share_token, None);
        let requests: Vec<Request> = recorder
            .requests
            .lock()
            .unwrap()
            .drain(..)
            .map(|(_, request)| request)
            .collect();
        assert!(matches!(&requests[..], [Request::Join(3)]), "{requests:?}");
    }
}
