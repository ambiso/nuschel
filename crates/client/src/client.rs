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
use nuschl_core::{
    ContentBlock, MAX_CONTENT_BYTES, MessageDocument, SearchCursor, SearchPage,
    audio::Sound,
    bookmark::{CachedChannel, ServerBookmark},
};
use nuschl_protocol::nuschl::{self, v1};
use std::collections::{BTreeMap, BTreeSet};

/// Cosmetic labels never change account IDs or ACL completeness. Bound scan,
/// deduplication, command count and attempts for both legacy and draft replies.
fn fetch_acl_names(
    platform: &dyn Platform,
    server: &mut ServerState,
    acl: &nuschl_core::model::Acl,
) {
    if server.acl_name_budget == 0 {
        return;
    }
    let mut ids = BTreeSet::new();
    let limit = server.acl_name_budget.min(128);
    for id in acl
        .entries
        .iter()
        .filter_map(|entry| entry.user_id)
        .chain(acl.groups.iter().flat_map(|group| {
            group
                .add
                .iter()
                .chain(&group.remove)
                .chain(&group.inherited_members)
                .copied()
        }))
        .take(4096)
    {
        if !server.user_names.contains_key(&id) && !server.acl_name_requests.contains(&id) {
            ids.insert(id);
            if ids.len() == limit {
                break;
            }
        }
    }
    if ids.is_empty() {
        return;
    }
    // Failed enqueues consume attempt budget too: repeated remote snapshots
    // cannot continuously hammer a full command queue. Explicit lookup remains.
    server.acl_name_budget -= ids.len();
    let ids: Vec<_> = ids.into_iter().collect();
    if platform
        .request(&server.id, Request::QueryUserNames(ids.clone()))
        .is_ok()
    {
        server.acl_name_requests.extend(ids);
    }
}

fn metadata_authority(
    server: &ServerState,
    users: Vec<(u32, u64)>,
    channels: Vec<(u32, u64)>,
) -> nuschl_core::model::MetadataAuthority {
    let own = server
        .own_session
        .and_then(|id| server.users.get(&id))
        .map(|user| (user.session, user.instance))
        .unwrap_or((0, 0));
    nuschl_core::model::MetadataAuthority {
        generation: server.generation,
        own,
        users,
        channels,
    }
}

/// One login-time pass for hashes received before our own presence was known.
/// Ordinary subsequent User events keep their existing incremental path.
fn fetch_pending_avatars(platform: &dyn Platform, server: &mut ServerState) {
    let Some(own) = server
        .own_session
        .and_then(|id| server.users.get(&id))
        .filter(|own| own.instance != 0)
        .map(|own| (own.session, own.instance))
    else {
        return;
    };
    let channel = server.voice_channel();
    for user in server.users.values() {
        if channel != Some(user.channel) || user.texture.is_some() {
            continue;
        }
        let Some(hash) = user
            .texture_hash
            .as_ref()
            .filter(|hash| !server.avatars.contains(*hash))
        else {
            continue;
        };
        if !server.avatar_requests.insert(hash.clone()) {
            continue;
        }
        let result = platform.request_metadata(
            &server.id,
            nuschl_core::model::MetadataAuthority {
                generation: server.generation,
                own,
                users: vec![(user.session, user.instance)],
                channels: vec![],
            },
            Request::RequestBlobs {
                textures: vec![user.session],
                comments: vec![],
                descriptions: vec![],
            },
        );
        if result.is_err() {
            server.avatar_requests.remove(hash);
        }
    }
}

mod order;

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
    /// The Nuschl extension was negotiated (the server's policy, e.g. its
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
    /// Rendered HTML for a UI to show; an empty welcome revokes its cache.
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
        link: nuschl_core::share::ShareLink,
        web_host: Option<String>,
    },
}

/// Model correlation after admission, never permission or network authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageReceipt {
    pub server: String,
    pub channel: u32,
    pub channel_instance: u64,
    pub serial: u64,
    pub revision: u32,
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
    /// Longer than the local raw-body byte ceiling.
    TooLong,
    /// Not sent; the precise reason is also shown as a temporary notice.
    Refused(String),
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
    notice_lease: Option<u64>,
    conversation: Conversation,
    target: TextTarget,
    body: String,
    /// The outgoing body a legacy HTML send reports; never the logical draft.
    wire: Option<String>,
    preview: Option<(crate::preview::Context, crate::preview::SubmissionToken)>,
}

#[derive(Default)]
struct DescriptionRequests {
    generation: u64,
    channels: BTreeMap<u32, DescriptionRequest>,
}

struct DescriptionRequest {
    instance: u64,
    hash: String,
    sent: bool,
}

/// Only an explicit profile-open action creates interest. A UI may retain or
/// revoke it while applying events; that cannot re-authorize a reused session.
struct CommentInterest {
    server: String,
    generation: u64,
    session: u32,
    instance: u64,
    hash: Option<String>,
    sent: bool,
}

fn blob_hash(hash: &Option<String>) -> Option<&String> {
    hash.as_ref()
        .filter(|hash| hash.len() == 40 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// Welcome content belongs only to the connection that supplied it.
fn clear_welcome(server: &mut ServerState, updates: &mut Vec<Update>) {
    if server.welcome.take().is_some() {
        updates.push(Update::Rendered {
            server: server.id.clone(),
            key: RenderKey::Welcome,
            source: String::new(),
            document: MessageDocument { blocks: Vec::new() },
        });
    }
}

/// What the voice policy needs of a server. One reconnecting still counts:
/// it gets its mute state back when it returns.
fn seat(server: &ServerState) -> Seat {
    Seat {
        connected: server.status != Status::Disconnected && server.has_mumble_voice(),
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
    /// Live servers in stable runtime-index order; use ordered_servers() for display.
    pub servers: Vec<ServerState>,
    catalog: order::Catalog,
    /// Which conversation is open, and where our voice is.
    pub navigation: Navigation,
    /// Which servers get the microphone (the talk servers); the others
    /// listen ([`voice`]).
    pub voice: Voice,
    pub focus: Focus,
    pub preferences: Preferences,
    platform: Box<dyn Platform>,
    sending: Option<PendingSend>,
    pub(crate) previews: crate::preview::Previews,
    /// Direct conversations asked for (`OpenDirect`), by request id: the
    /// server, the peer's session, and whether to open it once known.
    direct_requests: BTreeMap<u64, (String, u32, bool)>,
    /// Share links asked for, by request id.
    share_requests: BTreeMap<u64, share::ShareRequest>,
    /// When we last said we type, where (the platform's monotonic ms).
    typing_sent: Option<((String, u32), u64)>,
    /// Passwords of this run's connections, until they log in.
    passwords: BTreeMap<String, String>,
    description_requests: BTreeMap<String, DescriptionRequests>,
    comment_interest: Option<CommentInterest>,
    pub(crate) updates: Vec<Update>,
    now_ms: u64,
}

impl Client {
    pub fn new(
        platform: Box<dyn Platform>,
        bookmarks: Vec<ServerBookmark>,
        preferences: Preferences,
    ) -> Self {
        let catalog = order::Catalog::new(&bookmarks);
        Self {
            catalog,
            servers: bookmarks.into_iter().map(ServerState::new).collect(),
            navigation: Navigation::default(),
            voice: Voice::default(),
            focus: Focus::default(),
            preferences,
            platform,
            sending: None,
            previews: crate::preview::Previews::default(),
            direct_requests: BTreeMap::new(),
            share_requests: BTreeMap::new(),
            typing_sent: None,
            passwords: BTreeMap::new(),
            description_requests: BTreeMap::new(),
            comment_interest: None,
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
                server.dismiss_notice();
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
        self.catalog.synchronize(&self.servers);
        self.servers.len() - 1
    }

    /// Replace a saved server (its address or name changed), dropping its
    /// connection and what was known of it.
    pub fn replace_server(&mut self, index: usize, bookmark: ServerBookmark) {
        self.forget_document_requests(index);
        clear_welcome(&mut self.servers[index], &mut self.updates);
        self.platform.disconnect(&self.servers[index].id);
        self.voice.disconnected(&self.servers[index].id, None);
        self.catalog.replaced(&self.servers[index].id, &bookmark.id);
        self.servers[index] = ServerState::new(bookmark);
        self.catalog.synchronize(&self.servers);
    }

    pub fn remove_server(&mut self, id: &str) {
        if let Some(index) = self.server_index(id) {
            self.forget_document_requests(index);
        }
        if let Some(server) = self.servers.iter_mut().find(|server| server.id == id) {
            clear_welcome(server, &mut self.updates);
        }
        self.platform.disconnect(id);
        self.voice.disconnected(id, None);
        self.passwords.remove(id);
        self.servers.retain(|server| server.id != id);
        self.catalog.synchronize(&self.servers);
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
                self.description_requests.remove(&server.id);
                if self
                    .comment_interest
                    .as_ref()
                    .is_some_and(|interest| interest.server == server.id)
                {
                    self.comment_interest = None;
                }
                clear_welcome(server, &mut self.updates);
                server.clear_acl_cache();
                server.limits = None;
                server.opus_available = None;
                server.generation = generation;
                server.muted = true;
                server.deafened = deafened;
                server.status = Status::Connecting;
                server.security = SessionSecurity::Unknown;
                server.certificate = None;
                server.dismiss_notice();
                // It joins the channel you were last in there.
                let id = server.id.clone();
                let seats = seats(&self.servers);
                self.voice.connecting(&id, &seats);
            }
            Err(error) => server.set_notice(error),
        }
        self.validate_link_preview(self.now_ms);
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
        let identity = server.history_identity();
        server.bookmark.certificate_sha256 = Some(prompt.sha256);
        // No previously observed peer can stand in for this new explicit pin.
        server.peer_sha256 = None;
        let changed = identity != server.history_identity();
        self.updates.push(Update::BookmarksChangedNow);
        if changed {
            self.forget_server_history(index);
        }
        self.connect(index, None, deafened);
        true
    }

    /// Cached rows and pending pages belong to the previously authenticated
    /// identity. Forget the view, never the persisted history in its old scope.
    fn forget_server_history(&mut self, index: usize) {
        let server = &mut self.servers[index];
        for channel in &mut server.channels {
            channel.timeline = Default::default();
            channel.activity.clear();
            channel.new_messages = 0;
            channel.unread = 0;
            channel.unread_mention = false;
            channel.read_seq = 0;
        }
        if let Some(selection) = self
            .navigation
            .selection()
            .filter(|selection| selection.server == server.id)
            .cloned()
        {
            self.navigation.browse(selection, server.history_source);
        }
    }

    pub fn disconnect(&mut self, index: usize) {
        self.forget_document_requests(index);
        let Some(server) = self.servers.get_mut(index) else {
            return;
        };
        self.platform.disconnect(&server.id);
        clear_welcome(server, &mut self.updates);
        if server.status == Status::Connected && !server.deafened {
            self.platform.chime(Sound::Disconnected);
        }
        self.passwords.remove(&server.id);
        if let Some(lost) = server.lost_messages() {
            server.set_notice(lost);
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
        self.validate_link_preview(self.now_ms);
    }

    /// Toggle our mute (or deafen) on server `index`. Unmuting where your
    /// voice does not go yet while it goes elsewhere changes nothing and
    /// returns the question to ask ([`SecondServer`]).
    pub fn set_mute(&mut self, index: usize, deafen: bool) -> Option<SecondServer> {
        if !self.servers[index].has_mumble_voice() {
            self.servers[index]
                .set_notice("End-to-end voice is not available yet on this server.".into());
            return None;
        }
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
        if !self.servers[index].has_mumble_voice() {
            self.servers[index]
                .set_notice("End-to-end voice is not available yet on this server.".into());
            return;
        }
        let id = self.servers[index].id.clone();
        let changes = self.voice.speak_here(&id, both, &seats(&self.servers));
        self.apply_voice(changes);
    }

    /// "Use voice here": server `index` becomes the talk server, undeafened;
    /// a live microphone moves along.
    pub fn use_voice_here(&mut self, index: usize) {
        if !self.servers[index].has_mumble_voice() {
            self.servers[index]
                .set_notice("End-to-end voice is not available yet on this server.".into());
            return;
        }
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
                server.set_notice(error);
            }
        }
    }

    /// Move our voice to channel `channel` of server `index`: the talk
    /// server moves with you ([`Voice::move_here`]).
    pub fn join(&mut self, index: usize, channel: u32) {
        let server = &mut self.servers[index];
        if let Err(error) = self.platform.request(&server.id, Request::Join(channel)) {
            server.set_notice(error);
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
        if server.mls
            && let Some(entry) = server.channels.iter().find(|entry| entry.id == channel)
            && let Some(conversation) = entry.conversation
        {
            let _ = self.platform.request(
                &server.id,
                Request::OpenEncrypted {
                    conversation,
                    channel: !entry.is_direct(),
                    peer: entry.direct_peer.as_ref().and_then(|peer| peer.user_id),
                },
            );
        }
        self.request_selected_description(index);
        self.validate_link_preview(self.now_ms);
        result
    }

    fn forget_document_requests(&mut self, index: usize) {
        let Some(server) = self.servers.get(index) else {
            return;
        };
        self.description_requests.remove(&server.id);
        if self
            .comment_interest
            .as_ref()
            .is_some_and(|interest| interest.server == server.id)
        {
            self.comment_interest = None;
        }
    }

    /// Fetch a hash-only description only when its real channel is selected.
    /// One request per advertised hash/connection; no per-frame polling or timer.
    fn request_selected_description(&mut self, index: usize) {
        let Some(server) = self.servers.get(index) else {
            return;
        };
        let Some(selected) = self
            .navigation
            .selection()
            .filter(|selected| selected.server == server.id)
        else {
            return;
        };
        let Some(channel) = server
            .channels
            .iter()
            .find(|channel| channel.id == selected.channel)
        else {
            return;
        };
        let TextTarget::Channel(id) = channel.target else {
            return;
        };
        if server.status != Status::Connected || !channel.description_raw.is_empty() {
            return;
        }
        let Some(requests) = self
            .description_requests
            .get_mut(&server.id)
            .filter(|requests| requests.generation == server.generation)
        else {
            return;
        };
        let Some(request) = requests
            .channels
            .get_mut(&id)
            .filter(|request| !request.sent && request.instance == channel.instance)
        else {
            return;
        };
        if self
            .platform
            .request_metadata(
                &server.id,
                metadata_authority(server, vec![], vec![(id, request.instance)]),
                Request::RequestBlobs {
                    textures: vec![],
                    comments: vec![],
                    descriptions: vec![id],
                },
            )
            .is_ok()
        {
            request.sent = true;
        }
    }

    /// Open one profile comment. The content worker renders a body already held
    /// or the body that later answers this hash-only request, without reopening.
    /// A matching UI cache source avoids re-parsing an already prepared body.
    pub fn open_comment(&mut self, index: usize, session: u32, cached_source: Option<&str>) {
        let Some(server) = self
            .servers
            .get(index)
            .filter(|server| server.status == Status::Connected)
        else {
            return;
        };
        let Some(user) = server.users.get(&session) else {
            return;
        };
        let same = self.comment_interest.as_ref().is_some_and(|interest| {
            interest.server == server.id
                && interest.generation == server.generation
                && interest.session == session
                && interest.instance == user.instance
        });
        let render_existing = !same && user.comment.as_deref() != cached_source;
        if !same {
            self.comment_interest = Some(CommentInterest {
                server: server.id.clone(),
                generation: server.generation,
                session,
                instance: user.instance,
                hash: blob_hash(&user.comment_hash).cloned(),
                sent: false,
            });
        }
        self.refresh_comment_interest(render_existing);
    }

    /// A native UI calls this before applying a batch with its still-visible
    /// profile. It may revoke interest, but never create it after a reconnect.
    pub fn retain_comment_interest(&mut self, shown: Option<(usize, u32)>) {
        let keep = shown.is_some_and(|(index, session)| {
            self.servers.get(index).is_some_and(|server| {
                self.comment_interest.as_ref().is_some_and(|interest| {
                    interest.server == server.id
                        && interest.generation == server.generation
                        && interest.session == session
                        && server.status == Status::Connected
                        && server
                            .users
                            .get(&session)
                            .is_some_and(|user| user.instance == interest.instance)
                })
            })
        });
        if !keep {
            self.comment_interest = None;
        }
    }

    fn refresh_comment_interest(&mut self, render_body: bool) {
        let Some(mut interest) = self.comment_interest.take() else {
            return;
        };
        let Some(server) = self.servers.iter().find(|server| {
            server.id == interest.server
                && server.generation == interest.generation
                && server.status == Status::Connected
        }) else {
            return;
        };
        let Some(user) = server
            .users
            .get(&interest.session)
            .filter(|user| user.instance == interest.instance)
        else {
            return;
        };
        if blob_hash(&user.comment_hash) != interest.hash.as_ref() {
            interest.hash = blob_hash(&user.comment_hash).cloned();
            interest.sent = false;
        }
        if let Some(source) = &user.comment {
            if render_body {
                self.platform.render_html(
                    server.id.clone(),
                    server.generation,
                    RenderKey::Comment(user.session),
                    source.clone(),
                );
            }
        } else if interest.hash.is_some()
            && !interest.sent
            && self
                .platform
                .request_metadata(
                    &server.id,
                    metadata_authority(server, vec![(interest.session, interest.instance)], vec![]),
                    Request::RequestBlobs {
                        textures: vec![],
                        comments: vec![user.session],
                        descriptions: vec![],
                    },
                )
                .is_ok()
        {
            interest.sent = true;
        }
        self.comment_interest = Some(interest);
    }

    /// Load a page of the open conversation's history before `before` (the
    /// latest page for None): from the server on a Nuschl server with
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
        // Stock direct messages on a Nuschl server are local observations.
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
            // A Nuschl server's history: everything this member may read.
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
        let preview_wire = self.link_preview_wire(&body);
        let preview = self.link_preview_submission(&body);
        let server = &mut self.servers[server];
        if server.status != Status::Connected {
            return Err(SendError::Refused(server.refuse_send(
                "Connect before sending a message.".into(),
                self.now_ms,
            )));
        }
        let (target, expected_peer) = match server.message_target(channel) {
            Ok(target) => target,
            Err(reason) => {
                return Err(SendError::Refused(
                    server.refuse_send(reason.into(), self.now_ms),
                ));
            }
        };
        if !server.mls {
            let defaults = nuschl_core::model::ServerLimits::default();
            let limits = server.limits.as_ref().unwrap_or(&defaults);
            let wire = nuschl_legacy_html::legacy_body(&body, limits.allow_html);
            if let Err(reason) = limits.check_text_wire(&wire) {
                return Err(SendError::Refused(
                    server.refuse_send(reason.to_string(), self.now_ms),
                ));
            }
        }
        let channel = &server.channels[channel];
        let pending = PendingSend {
            notice_lease: server.send_refusal_lease(),
            conversation: Conversation {
                server: server.id.clone(),
                channel: channel.id,
            },
            target: target.clone(),
            body: body.clone(),
            wire: preview_wire.as_ref().map(|(_, _, html)| html.clone()),
            preview,
        };
        let scoped_preview =
            preview_wire.map(|(context, connection, html)| crate::preview::PreviewSend {
                context,
                connection,
                html,
                target: target.clone(),
                expected_peer,
            });
        let request = if server.mls {
            let Some(conversation) = channel.conversation else {
                return Err(SendError::Refused(server.refuse_send(
                    "End-to-end encryption is being set up.".into(),
                    self.now_ms,
                )));
            };
            if let Some(reason) = channel
                .encryption
                .as_ref()
                .and_then(|state| state.blocked.clone())
            {
                return Err(SendError::Refused(server.refuse_send(reason, self.now_ms)));
            }
            Request::SendEncrypted {
                conversation,
                text: body,
            }
        } else {
            Request::SendText {
                target,
                body,
                expected_peer,
            }
        };
        let submitted = match scoped_preview {
            Some(preview) => self.platform.send_link_preview(preview),
            None => self.platform.request(&server.id, request),
        };
        match submitted {
            Ok(()) => {
                self.sending = Some(pending);
                // Sent: we are no longer typing it.
                self.typing_sent = None;
                self.pause_submitted_link_preview(self.now_ms);
                Ok(())
            }
            Err(error) => Err(SendError::Refused(server.refuse_send(error, self.now_ms))),
        }
    }

    /// The composer of the open conversation changed; `typing` is whether
    /// it holds text being written (not an edit). On Nuschl servers others
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
    /// front and the newest message shown. A Nuschl server keeps the read
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
    /// Nuschl server, between registered accounts, it is the server's
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

    /// Edit or delete a message on a Nuschl server; the answer comes back
    /// as an edit or deletion, or [`Input::ChangeFailed`].
    pub fn change_message(&self, change: ChangeRequest) -> Result<(), String> {
        if self
            .servers
            .iter()
            .any(|server| server.id == change.server && server.mls)
        {
            return Err("Encrypted edits and deletions are not implemented yet.".into());
        }
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
                if text.trim().is_empty() || text.len() > nuschl::MAX_CONTENT_BYTES {
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
            Input::LinkPreview { id, result } => {
                self.receive_link_preview(id, result, self.now_ms);
            }
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
                            && pending.wire.as_ref().unwrap_or(&pending.body) == &body
                    })
                    && let Some(pending) = self.sending.take()
                {
                    if let Some(index) = self.current(&server, generation) {
                        self.servers[index].clear_sent_refusal(pending.notice_lease);
                    }
                    if let Some((context, token)) = pending.preview {
                        self.acknowledge_link_preview(&context, token, self.now_ms);
                    }
                    self.updates.push(Update::Sent {
                        conversation: pending.conversation,
                        body: pending.body,
                    });
                }
            }
            Input::SendFailed {
                server,
                generation,
                reason,
            } => {
                if let Some(index) = self.current(&server, generation) {
                    self.sending = None;
                    self.servers[index].refuse_send(reason, self.now_ms);
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
                    server.set_notice(format!(
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
                    self.forget_document_requests(index);
                    let server = &mut self.servers[index];
                    server.status = Status::Disconnected;
                    clear_welcome(server, &mut self.updates);
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
                    self.servers[index].set_notice(
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
                    self.forget_document_requests(index);
                    let server = &mut self.servers[index];
                    if server.status == Status::Connected && !server.deafened {
                        self.platform.chime(Sound::Disconnected);
                    }
                    server.clear_acl_cache();
                    server.limits = None;
                    server.opus_available = None;
                    server.status = Status::Connecting;
                    clear_welcome(server, &mut self.updates);
                    server.own_session = None;
                    if let Some(lost) = server.lost_messages() {
                        server.set_notice(lost);
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
                        RenderKey::Welcome => state.welcome.as_deref() == Some(source.as_str()),
                        RenderKey::Description(id) => {
                            state.status == Status::Connected
                                && state
                                    .channel_view(id)
                                    .is_some_and(|channel| channel.description_raw == source)
                        }
                        RenderKey::Comment(session) => {
                            state.status == Status::Connected
                                && state.users.get(&session).is_some_and(|user| {
                                    user.comment.as_deref() == Some(source.as_str())
                                        && self.comment_interest.as_ref().is_some_and(|interest| {
                                            interest.server == state.id
                                                && interest.generation == state.generation
                                                && interest.session == session
                                                && interest.instance == user.instance
                                        })
                                })
                        }
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
                self.receive_message(&server, generation, target, storage_key, peer, message);
            }
        }
    }

    /// After a batch of inputs: channels in display order again, and edits
    /// and deletions that came before their message's page.
    pub fn settle(&mut self) {
        self.validate_link_preview(self.now_ms);
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

    /// The common live-message admission path, also used by asynchronous
    /// platform adapters that need the exact allocated row's identity.
    pub fn receive_message(
        &mut self,
        server: &str,
        generation: u64,
        target: TextTarget,
        storage_key: String,
        peer: Option<User>,
        message: crate::timeline::IncomingMessage,
    ) -> Option<MessageReceipt> {
        let index = self.current(server, generation)?;
        self.message(index, target, storage_key, peer, message)
    }

    fn message(
        &mut self,
        index: usize,
        target: TextTarget,
        storage_key: String,
        peer: Option<User>,
        mut message: crate::timeline::IncomingMessage,
    ) -> Option<MessageReceipt> {
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
                return None;
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
        let received = channel.timeline.receive_with_receipt(message);
        let added = received.added;
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
            && server.notice.as_deref() == Some(nuschl_core::model::RECIPIENT_UNAVAILABLE_NOTICE)
        {
            server.dismiss_notice();
        }
        received.serial.and_then(|serial| {
            let channel = &server.channels[position];
            let message = channel
                .timeline
                .messages
                .iter()
                .find(|message| message.serial == serial)?;
            Some(MessageReceipt {
                server: server.id.clone(),
                channel: channel.id,
                channel_instance: channel.instance,
                serial,
                revision: message.revision,
            })
        })
    }

    fn server_event(&mut self, index: usize, event: ServerEvent) {
        let fetch_description = matches!(
            &event,
            ServerEvent::Connected { .. } | ServerEvent::Channel(_)
        );
        let refresh_comment = matches!(&event, ServerEvent::User(_) | ServerEvent::UserRemoved(_));
        let render_comment = match &event {
            ServerEvent::User(user)
                if self.comment_interest.as_ref().is_some_and(|interest| {
                    interest.session == user.session && interest.server == self.servers[index].id
                }) =>
            {
                self.servers[index]
                    .users
                    .get(&user.session)
                    .is_none_or(|old| old.comment != user.comment)
            }
            _ => false,
        };
        let server = &mut self.servers[index];
        match event {
            ServerEvent::EncryptionGap {
                conversation,
                reason,
                at_ms,
            } => {
                if let Some(channel) = server
                    .channels
                    .iter_mut()
                    .find(|channel| channel.conversation == Some(conversation))
                {
                    channel.log(reason, at_ms, true);
                }
            }
            ServerEvent::Encryption {
                conversation,
                state,
            } => {
                for channel in &mut server.channels {
                    if channel.conversation == Some(conversation) {
                        channel.encryption = Some(state.clone());
                    }
                }
                if server.encryption.len() < 4096 || server.encryption.contains_key(&conversation) {
                    server.encryption.insert(conversation, state);
                }
            }
            // The driver keeps the sticky record. With history, the timeline
            // pages from the server (seqs), not from local observations.
            ServerEvent::Negotiated {
                capabilities,
                policy,
            } => {
                let history = capabilities
                    .iter()
                    .any(|capability| capability == "history/1");
                server.mls = capabilities.iter().any(|capability| capability == "e2ee/1");
                server.capabilities = capabilities;
                if !server.has_mumble_voice() {
                    server.audio_status =
                        "End-to-end voice is not available yet on this server.".into();
                }
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
                if selected
                    && server.mls
                    && let Some(conversation) = conversation
                {
                    let _ = self.platform.request(
                        &server.id,
                        Request::OpenEncrypted {
                            conversation,
                            channel: true,
                            peer: None,
                        },
                    );
                }
                let encryption = conversation.and_then(|id| server.encryption.get(&id).cloned());
                if let Some(view) = server.channel_view(channel) {
                    view.conversation = conversation;
                    view.encryption = encryption;
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
                server.set_notice(format!("A message was not sent: {reason}"));
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
                server.peer_sha256 = Some(pin);
                server.security = SessionSecurity::TransportOnly;
                // No configured pin means this successful session already
                // passed WebPKI. Do not turn normal CA renewal into a pin change.
                // Existing pins, including older pins of unknown origin, stay strict.
                // Reconnecting to the same server leaves the timeline (and
                // where the reader is in it) alone; a new identity has
                // other history.
                if server.history_identity() != identity {
                    let selected = self
                        .navigation
                        .selection()
                        .is_some_and(|selected| selected.server == server.id);
                    if identity.is_some() {
                        self.forget_server_history(index);
                    } else {
                        for channel in &mut server.channels {
                            channel.timeline.loaded = false;
                        }
                    }
                    if selected && let Err(error) = self.load_history(None) {
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
                fetch_pending_avatars(self.platform.as_ref(), server);
                server.expand_occupied();
                self.passwords.remove(&server.id);
                // A share link's token decides first; its path is the fallback.
                if server.share_token.is_some() && server.has(nuschl::capability::SHARE) {
                    self.share_connected(index);
                } else {
                    server.share_token = None;
                    self.join_pending(index);
                }
            }
            ServerEvent::Channel(update) => {
                let requests = self
                    .description_requests
                    .entry(server.id.clone())
                    .or_default();
                if requests.generation != server.generation {
                    *requests = DescriptionRequests {
                        generation: server.generation,
                        ..Default::default()
                    };
                }
                if let Some(hash) = blob_hash(&update.description_hash) {
                    if requests.channels.get(&update.id).is_none_or(|request| {
                        request.hash != *hash || request.instance != update.instance
                    }) {
                        requests.channels.insert(
                            update.id,
                            DescriptionRequest {
                                instance: update.instance,
                                hash: hash.clone(),
                                sent: false,
                            },
                        );
                    }
                } else {
                    requests.channels.remove(&update.id);
                }
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
                    channel.instance = update.instance;
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
                        instance: update.instance,
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
                    server.permission_queries.clear();
                }
                if channel != u32::MAX {
                    server.permission_queries.remove(&channel);
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
            ServerEvent::OpusAvailable(available) => server.opus_available = Some(available),
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
            ServerEvent::AccountReply(reply) => server.account_reply = Some(reply),
            ServerEvent::AclReply(reply) => {
                if let Ok(acl) = &reply.result {
                    fetch_acl_names(self.platform.as_ref(), server, acl);
                    server.acl = Some(acl.clone());
                }
                server.acl_reply = Some(reply);
            }
            ServerEvent::Acl(acl) => {
                fetch_acl_names(self.platform.as_ref(), server, &acl);
                server.acl = Some(acl);
            }
            ServerEvent::UserNames(names) => {
                for (id, name) in names {
                    if !server.user_names.contains_key(&id) && server.user_names.len() >= 4096 {
                        server.user_names.pop_first();
                    }
                    server.user_names.insert(id, name);
                }
            }
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
                            "{} uses a legacy voice codec (CELT/Speex) that Nuschl cannot play.",
                            names::display_name(&user, &self.preferences)
                        ),
                        at_ms,
                        true,
                    );
                }
            }
            ServerEvent::ChannelRemoved(id) => {
                if let Some(requests) = self.description_requests.get_mut(&server.id) {
                    requests.channels.remove(&id);
                }
                server.expansion.remove(&id);
                server.permissions.remove(&id);
                server.permission_queries.remove(&id);
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
                        && server
                            .own_session
                            .and_then(|id| server.users.get(&id))
                            .is_some_and(|own| own.instance != 0)
                        && server.avatar_requests.insert(hash)
                    {
                        let _ = self.platform.request_metadata(
                            &server.id,
                            metadata_authority(server, vec![(user.session, user.instance)], vec![]),
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
                let own_arrived = Some(user.session) == server.own_session
                    && server
                        .users
                        .get(&user.session)
                        .is_none_or(|own| own.instance == 0);
                let arrived = user.clone();
                server.users.insert(user.session, user);
                if own_arrived {
                    fetch_pending_avatars(self.platform.as_ref(), server);
                }
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
                server.set_notice(message.clone());
                server.notice_expires = Some((message, self.now_ms.saturating_add(10_000)));
                if self
                    .sending
                    .as_ref()
                    .is_some_and(|pending| pending.conversation.server == server.id)
                {
                    self.sending = None;
                }
            }
            ServerEvent::Welcome { source, document } if server.status == Status::Connected => {
                server.welcome = Some(source.clone());
                self.updates.push(Update::Rendered {
                    server: server.id.clone(),
                    key: RenderKey::Welcome,
                    source,
                    document,
                });
            }
            ServerEvent::Welcome { .. } => {}
            ServerEvent::Transport(stats) => server.transport = Some(stats),
        }
        if fetch_description {
            self.request_selected_description(index);
        }
        if refresh_comment {
            self.refresh_comment_interest(render_comment);
        }
    }

    /// The server named one of our direct conversations (opened by us).
    fn direct_opened(&mut self, index: usize, opened: v1::DirectOpened) {
        let request = self.direct_requests.remove(&opened.request_id);
        let Some(direct) = opened.direct else {
            return;
        };
        let Some(id) = nuschl::id(&direct.conversation) else {
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
            let Some(id) = nuschl::id(&direct.conversation) else {
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
            server.set_notice(format!("Channel /{} was not found.", path.join("/")));
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
    use nuschl_core::ChatMessage;
    use std::sync::{Arc, Mutex};

    /// A platform that records what the client asked of it.
    #[derive(Default)]
    struct Recorder {
        preview_sends: Mutex<Vec<crate::preview::PreviewSend>>,
        previews: Mutex<Vec<crate::preview::FetchTicket>>,
        cancelled_previews: Mutex<Vec<crate::preview::RequestId>>,
        reject_requests: std::sync::atomic::AtomicBool,
        metadata: Mutex<Vec<nuschl_core::model::MetadataAuthority>>,
        requests: Mutex<Vec<(String, Request)>>,
        pages: Mutex<Vec<(String, HistoryTicket, Page)>>,
        sounds: Mutex<Vec<Sound>>,
        rendered: Mutex<Vec<(String, u64, RenderKey, String)>>,
    }

    impl Platform for Recorder {
        fn send_link_preview(&self, request: crate::preview::PreviewSend) -> Result<(), String> {
            self.preview_sends.lock().unwrap().push(request.clone());
            self.request(
                &request.context.server,
                Request::SendHtml {
                    target: request.target,
                    html: vec![request.html],
                    expected_peer: request.expected_peer,
                },
            )
        }
        fn link_preview(
            &self,
            _: crate::preview::Settings,
            ticket: crate::preview::FetchTicket,
        ) -> Result<(), crate::preview::Unavailable> {
            self.previews.lock().unwrap().push(ticket);
            Ok(())
        }
        fn cancel_link_preview(&self, id: crate::preview::RequestId) {
            self.cancelled_previews.lock().unwrap().push(id);
        }
        fn connect(&self, _: ServerBookmark, _: Option<String>, _: bool) -> Result<u64, String> {
            Ok(1)
        }
        fn disconnect(&self, _: &str) {}
        fn request(&self, server: &str, request: Request) -> Result<(), String> {
            self.requests
                .lock()
                .unwrap()
                .push((server.to_owned(), request));
            if self
                .reject_requests
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                Err("test command queue full".into())
            } else {
                Ok(())
            }
        }
        fn request_metadata(
            &self,
            server: &str,
            authority: nuschl_core::model::MetadataAuthority,
            request: Request,
        ) -> Result<(), String> {
            self.metadata.lock().unwrap().push(authority);
            self.request(server, request)
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
        fn render_html(&self, server: String, generation: u64, key: RenderKey, source: String) {
            self.rendered
                .lock()
                .unwrap()
                .push((server, generation, key, source));
        }
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

    #[test]
    fn shared_send_policy_refuses_html_total_and_wire_bytes_but_accepts_utf16_unicode() {
        for (html, text, image, body, accepted, reason) in [
            (
                true,
                0,
                128,
                "x".repeat(129),
                false,
                "128-character total HTML limit",
            ),
            (true, 0, 128, "x".repeat(128), true, ""),
            (
                true,
                0,
                0,
                "&".repeat(60_000),
                false,
                "256 KiB client limit",
            ),
            (true, 1, 0, "漢".into(), true, ""),
            (false, 1, 0, "é".into(), true, ""),
            (true, 1, 0, "🙂".into(), false, "1-character text limit"),
            (true, 2, 0, "🙂".into(), true, ""),
            (
                true,
                50,
                0,
                "see https://example.test/".into(),
                false,
                "50-character text limit",
            ),
        ] {
            let (mut client, recorder, _) = connected();
            client.open(0, 2).unwrap();
            client.servers[0].limits = Some(nuschl_core::model::ServerLimits {
                allow_html: html,
                message_length: text,
                image_message_length: image,
                ..Default::default()
            });
            recorder.requests.lock().unwrap().clear();
            let result = client.send_text(body);
            assert_eq!(result.is_ok(), accepted);
            if !accepted {
                assert!(matches!(result,Err(SendError::Refused(value)) if value.contains(reason)));
                assert!(recorder.requests.lock().unwrap().is_empty());
                assert!(client.servers[0].has_temporary_notice());
                assert!(!client.sending());
            }
        }
    }

    #[test]
    fn synchronous_refusals_return_their_exact_reason_and_expire_without_scraping_notices() {
        for case in 0..5 {
            let (mut client, recorder, _) = connected();
            client.open(0, 2).unwrap();
            client.set_time(500);
            client.servers[0].set_notice("unrelated sentinel".into());
            let (_, channel) = client.selected_indices().unwrap();
            let expected = match case {
                0 => {
                    client.servers[0].status = Status::Disconnected;
                    "Connect before sending a message."
                }
                1 => {
                    client.servers[0].channels[channel].target = TextTarget::Direct(99);
                    nuschl_core::model::RECIPIENT_UNAVAILABLE_NOTICE
                }
                2 => {
                    client.servers[0].mls = true;
                    "End-to-end encryption is being set up."
                }
                3 => {
                    client.servers[0].mls = true;
                    client.servers[0].channels[channel].conversation = Some([1; 16]);
                    client.servers[0].channels[channel].encryption =
                        Some(crate::conversation::EncryptionState {
                            security: SessionSecurity::Unknown,
                            description: "test".into(),
                            blocked: Some("New key requires verification.".into()),
                            epoch: 3,
                        });
                    "New key requires verification."
                }
                _ => {
                    recorder
                        .reject_requests
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    "test command queue full"
                }
            };
            assert_eq!(
                client.send_text("hello".into()),
                Err(SendError::Refused(expected.into()))
            );
            assert_eq!(client.servers[0].notice.as_deref(), Some(expected));
            assert!(client.servers[0].send_refusal_lease().is_some());
            assert!(!client.set_time(10_499));
            assert!(client.set_time(10_500));
            assert!(client.servers[0].notice.is_none());
        }
        let (mut client, _, _) = connected();
        client.open(0, 2).unwrap();
        client.servers[0].set_notice("sentinel".into());
        assert_eq!(client.send_text("".into()), Err(SendError::Nothing));
        assert_eq!(
            client.send_text("x".repeat(MAX_CONTENT_BYTES + 1)),
            Err(SendError::TooLong)
        );
        assert_eq!(client.servers[0].notice.as_deref(), Some("sentinel"));
    }

    #[test]
    fn only_matching_sent_clears_its_old_refusal_and_never_a_newer_identical_notice() {
        for replacement in 0..4 {
            let (mut client, recorder, id) = connected();
            client.open(0, 2).unwrap();
            recorder
                .reject_requests
                .store(true, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(
                client.send_text("hello".into()),
                Err(SendError::Refused("test command queue full".into()))
            );
            recorder
                .reject_requests
                .store(false, std::sync::atomic::Ordering::Relaxed);
            client.send_text("hello".into()).unwrap();
            match replacement {
                1 => client.servers[0].set_notice("test command queue full".into()),
                2 => client.servers[0].dismiss_notice(),
                3 => {
                    assert!(client.set_time(10_000));
                }
                _ => {}
            }
            let sent = |generation, body: &str| Input::Sent {
                server: id.clone(),
                generation,
                target: TextTarget::Channel(2),
                body: body.into(),
            };
            client.handle(sent(0, "hello"));
            client.handle(sent(1, "other"));
            assert!(
                client.sending(),
                "stale/unmatched send must not release pending state"
            );
            client.handle(sent(1, "hello"));
            assert!(!client.sending());
            assert_eq!(
                client.servers[0].notice.as_deref(),
                (replacement == 1).then_some("test command queue full")
            );
        }
        let (mut client, _, id) = connected();
        client.open(0, 2).unwrap();
        client.handle(Input::SendFailed {
            server: id.clone(),
            generation: 0,
            reason: "stale".into(),
        });
        assert!(client.servers[0].notice.is_none());
        client.handle(Input::SendFailed {
            server: id.clone(),
            generation: 1,
            reason: "refused".into(),
        });
        assert_eq!(client.servers[0].notice.as_deref(), Some("refused"));
        client.send_text("next".into()).unwrap();
        client.handle(Input::Sent {
            server: id,
            generation: 1,
            target: TextTarget::Channel(2),
            body: "next".into(),
        });
        assert!(client.servers[0].notice.is_none());
    }

    #[test]
    fn limits_and_negotiated_codec_reset_only_for_the_current_connection_attempt() {
        let (mut client, _, id) = connected();
        let limits = || nuschl_core::model::ServerLimits {
            allow_html: false,
            message_length: 17,
            image_message_length: 32,
            max_users: Some(2),
            recording_allowed: false,
            max_bandwidth: Some(24_000),
        };
        let input = |generation, event| Input::Server {
            server: id.clone(),
            generation,
            event,
        };
        assert_eq!(client.servers[0].opus_available, None);
        client.handle(input(1, ServerEvent::Limits(limits())));
        client.handle(input(1, ServerEvent::OpusAvailable(false)));
        client.handle(input(1, ServerEvent::Connected { session: 1 }));
        assert_eq!(
            client.servers[0].opus_available,
            Some(false),
            "Sync must retain actual earlier CodecVersion"
        );
        client.servers[0].generation = 77;
        client.connect(0, None, false);
        assert!(client.servers[0].limits.is_none());
        assert_eq!(client.servers[0].opus_available, None);
        client.handle(input(77, ServerEvent::Limits(limits())));
        client.handle(input(77, ServerEvent::OpusAvailable(true)));
        assert!(client.servers[0].limits.is_none());
        assert_eq!(client.servers[0].opus_available, None);
        client.handle(input(1, ServerEvent::Limits(limits())));
        client.handle(input(1, ServerEvent::OpusAvailable(false)));
        let reconnect = |generation| Input::Reconnecting {
            server: id.clone(),
            generation,
            attempt: 1,
            delay_secs: 1,
            reason: "owned test".into(),
        };
        client.handle(reconnect(77));
        assert!(client.servers[0].limits.is_some());
        assert_eq!(client.servers[0].opus_available, Some(false));
        client.handle(reconnect(1));
        assert!(client.servers[0].limits.is_none());
        assert_eq!(client.servers[0].opus_available, None);
        client.handle(input(77, ServerEvent::OpusAvailable(false)));
        assert_eq!(client.servers[0].opus_available, None);
        client.handle(input(1, ServerEvent::Limits(limits())));
        client.handle(input(1, ServerEvent::OpusAvailable(true)));
        assert_eq!(
            client.servers[0].limits.as_ref().unwrap().max_bandwidth,
            Some(24_000)
        );
        assert_eq!(client.servers[0].opus_available, Some(true));
    }

    #[test]
    fn acl_label_resolution_is_bounded_deduplicated_and_preserves_full_membership() {
        use nuschl_core::{
            admin::AclReply,
            model::{Acl, AclGroup},
        };
        let (mut client, recorder, id) = connected();
        recorder.requests.lock().unwrap().clear();
        let acl = Acl {
            channel: 0,
            inherit_acls: true,
            entries: vec![],
            groups: vec![AclGroup {
                name: "large".into(),
                inherited: true,
                inherit: true,
                inheritable: true,
                add: (0..8192).collect(),
                remove: vec![1; 8192],
                inherited_members: (10000..18192).collect(),
            }],
        };
        for request in 1..=32 {
            let event = if request % 2 == 0 {
                ServerEvent::Acl(acl.clone())
            } else {
                ServerEvent::AclReply(AclReply {
                    request,
                    result: Ok(acl.clone()),
                })
            };
            client.handle(Input::Server {
                server: id.clone(),
                generation: 1,
                event,
            });
        }
        let requests = recorder.requests.lock().unwrap();
        let batches: Vec<_> = requests
            .iter()
            .filter_map(|(_, request)| {
                if let Request::QueryUserNames(ids) = request {
                    Some(ids)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(batches.len(), 8);
        assert!(batches.iter().all(|batch| batch.len() == 128));
        let all: Vec<_> = batches
            .iter()
            .flat_map(|batch| batch.iter().copied())
            .collect();
        assert_eq!(all.len(), 1024);
        assert_eq!(all.iter().copied().collect::<BTreeSet<_>>().len(), 1024);
        assert_eq!(
            client.servers[0].acl.as_ref(),
            Some(&acl),
            "labels must not truncate group members"
        );
        assert_eq!(client.servers[0].acl_name_requests.len(), 1024);
        drop(requests);
        client.handle(Input::Server {
            server: id.clone(),
            generation: 1,
            event: ServerEvent::UserNames((0..5000).map(|id| (id, format!("user {id}"))).collect()),
        });
        assert_eq!(client.servers[0].user_names.len(), 4096);
    }

    #[test]
    fn duplicate_acl_labels_and_full_command_queue_do_not_create_a_retry_flood() {
        use nuschl_core::model::{Acl, AclGroup};
        let (mut client, recorder, id) = connected();
        recorder.requests.lock().unwrap().clear();
        let mut acl = Acl {
            channel: 0,
            inherit_acls: true,
            entries: vec![],
            groups: vec![AclGroup {
                name: "duplicates".into(),
                inherited: false,
                inherit: true,
                inheritable: true,
                add: vec![77; 8192],
                remove: vec![77; 8192],
                inherited_members: vec![77; 8192],
            }],
        };
        for _ in 0..8 {
            client.handle(Input::Server {
                server: id.clone(),
                generation: 1,
                event: ServerEvent::Acl(acl.clone()),
            });
        }
        let queries = || {
            recorder
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, request)| matches!(request, Request::QueryUserNames(_)))
                .count()
        };
        assert_eq!(queries(), 1, "successful outstanding IDs are deduplicated");
        recorder
            .reject_requests
            .store(true, std::sync::atomic::Ordering::Relaxed);
        acl.groups[0].add = (1000..9192).collect();
        for _ in 0..32 {
            client.handle(Input::Server {
                server: id.clone(),
                generation: 1,
                event: ServerEvent::Acl(acl.clone()),
            });
        }
        assert_eq!(
            queries(),
            9,
            "failed enqueues also consume the bounded per-connection attempt budget"
        );
        assert_eq!(
            client.servers[0].acl_name_requests.len(),
            1,
            "failed IDs remain available to explicit lookup"
        );
        assert_eq!(client.servers[0].acl_name_budget, 0);
        client.handle(Input::Reconnecting {
            server: id,
            generation: 1,
            attempt: 1,
            delay_secs: 1,
            reason: "owned test disconnect".into(),
        });
        assert!(client.servers[0].acl_name_requests.is_empty());
        assert_eq!(client.servers[0].acl_name_budget, 1024);
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

    fn prepare_link(client: &mut Client, recorder: &Recorder) -> crate::preview::FetchTicket {
        client.open(0, 2).unwrap();
        client.set_link_preview_settings(
            crate::preview::Settings {
                mode: crate::preview::Mode::Direct,
                ..Default::default()
            },
            0,
        );
        client.composing_link_preview("https://example.org/ ", false, false, false, 0);
        client.poll_link_preview(crate::preview::DEBOUNCE_MS - 1);
        assert!(recorder.previews.lock().unwrap().is_empty());
        client.poll_link_preview(crate::preview::DEBOUNCE_MS);
        recorder.previews.lock().unwrap().last().unwrap().clone()
    }

    #[test]
    fn preview_arc_hooks_cancel_and_discard_a_departed_conversation_result() {
        let (mut client, recorder, _) = connected();
        let ticket = prepare_link(&mut client, &recorder);
        client.open(0, 3).unwrap();
        assert_eq!(
            *recorder.cancelled_previews.lock().unwrap(),
            vec![ticket.id]
        );
        client.handle(Input::LinkPreview {
            id: ticket.id,
            result: Err(crate::preview::Unavailable::Network),
        });
        assert_eq!(client.link_preview_status(), crate::preview::Status::None);
    }

    #[test]
    fn preview_never_fetches_ineligible_or_disabled_composer_sources() {
        for case in 0..6 {
            let (mut client, recorder, _) = connected();
            client.open(0, 2).unwrap();
            if case != 0 {
                client.set_link_preview_settings(
                    crate::preview::Settings {
                        mode: crate::preview::Mode::Direct,
                        ..Default::default()
                    },
                    0,
                );
            }
            match case {
                3 => client.servers[0].mls = true,
                4 => {
                    client.servers[0].limits = Some(nuschl_core::model::ServerLimits {
                        allow_html: false,
                        ..Default::default()
                    })
                }
                5 => {
                    client.servers[0]
                        .channels
                        .iter_mut()
                        .find(|channel| channel.id == 2)
                        .unwrap()
                        .target = TextTarget::DirectConversation([1; 16])
                }
                _ => {}
            }
            client.composing_link_preview("https://example.org/ ", false, case == 1, case == 2, 0);
            client.poll_link_preview(10_000);
            assert!(recorder.previews.lock().unwrap().is_empty(), "case {case}");
        }
    }

    #[test]
    fn preview_rejects_results_when_certificate_or_own_session_instance_changes() {
        for change_certificate in [false, true] {
            let (mut client, recorder, _) = connected();
            let ticket = prepare_link(&mut client, &recorder);
            if change_certificate {
                client.servers[0].peer_sha256 = Some("cd".repeat(32));
            } else {
                client.servers[0].users.get_mut(&1).unwrap().instance += 1;
            }
            client.handle(Input::LinkPreview {
                id: ticket.id,
                result: Err(crate::preview::Unavailable::Network),
            });
            assert_eq!(
                *recorder.cancelled_previews.lock().unwrap(),
                vec![ticket.id]
            );
            assert_eq!(client.link_preview_status(), crate::preview::Status::None);
        }
    }

    #[test]
    fn preview_sends_one_html_entry_and_acknowledges_exact_original_logical_text() {
        let (mut client, recorder, id) = connected();
        let ticket = prepare_link(&mut client, &recorder);
        let card = crate::preview::Card::new(
            &ticket.target,
            &nuschl_link_preview::metadata::ExtractedMetadata {
                title: "Fixture title".into(),
                description: "Fixture description".into(),
                image: None,
                image_alt: String::new(),
            },
            None,
        )
        .unwrap();
        client.handle(Input::LinkPreview {
            id: ticket.id,
            result: Ok(card),
        });
        recorder.requests.lock().unwrap().clear();
        let original = "https://example.org/ ";
        recorder
            .reject_requests
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            client.send_text(original.into()),
            Err(SendError::Refused(_))
        ));
        assert!(!client.sending());
        assert_eq!(
            recorder.requests.lock().unwrap().len(),
            1,
            "refused scoped preview must not fall back to a second text request"
        );
        assert!(matches!(
            client.link_preview_status(),
            crate::preview::Status::Ready(_)
        ));
        recorder
            .reject_requests
            .store(false, std::sync::atomic::Ordering::Relaxed);
        recorder.requests.lock().unwrap().clear();
        client.send_text(original.into()).unwrap();
        let scoped = recorder.preview_sends.lock().unwrap();
        let frozen = scoped.last().unwrap();
        let own = client.servers[0].users.get(&1).unwrap();
        assert_eq!(frozen.context.own, (own.session, own.instance));
        assert_eq!(frozen.connection, 1);
        assert_eq!(
            frozen.context.instance,
            client.servers[0]
                .channels
                .iter()
                .find(|channel| channel.id == 2)
                .unwrap()
                .instance
        );
        drop(scoped);
        let requests = recorder.requests.lock().unwrap();
        let [(server, Request::SendHtml { target, html, .. })] = requests.as_slice() else {
            panic!("one atomic HTML request required")
        };
        assert_eq!(server, &id);
        assert_eq!(html.len(), 1);
        assert!(html[0].contains("Fixture title"));
        let target = target.clone();
        let wire = html[0].clone();
        drop(requests);
        client.handle(Input::Sent {
            server: id.clone(),
            generation: 1,
            target: target.clone(),
            body: original.into(),
        });
        assert!(
            client.sending(),
            "logical text cannot fake the actual wire ACK"
        );
        client.handle(Input::Sent {
            server: id,
            generation: 1,
            target,
            body: wire,
        });
        assert!(!client.sending());
        assert!(
            client
                .take_updates()
                .iter()
                .any(|update| matches!(update, Update::Sent { body, .. } if body == original))
        );
    }

    #[test]
    fn observed_receipts_use_exact_serials_and_keep_generation_and_detached_semantics() {
        let (mut client, _, id) = connected();
        let Input::Message { mut message, .. } = message(&id, 2, "same", 5) else {
            unreachable!()
        };
        message.id = None;
        let receive = |client: &mut Client, generation, message| {
            client.receive_message(
                &id,
                generation,
                TextTarget::Channel(2),
                "2".into(),
                None,
                message,
            )
        };
        assert_eq!(receive(&mut client, 77, message.clone()), None);
        let first = receive(&mut client, 1, message.clone()).unwrap();
        let second = receive(&mut client, 1, message.clone()).unwrap();
        assert_ne!(first.serial, second.serial);
        assert_eq!(
            (first.server.as_str(), first.channel, first.revision),
            (id.as_str(), 2, 0)
        );
        let channel = client.servers[0]
            .channels
            .iter_mut()
            .find(|channel| channel.id == 2)
            .unwrap();
        assert_eq!(channel.timeline.messages.len(), 2);
        channel.timeline.next_after = Some(7);
        message.id = Some(8);
        assert_eq!(receive(&mut client, 1, message), None);
        let channel = client.servers[0]
            .channels
            .iter()
            .find(|channel| channel.id == 2)
            .unwrap();
        assert!(channel.timeline.missed_live);
        assert_eq!(channel.timeline.messages.len(), 2);
    }

    #[test]
    fn permission_reads_deduplicate_bound_retry_and_drop_stale_cache_on_flush_or_channel_reuse() {
        let (mut client, _, id) = connected();
        for channel in &mut client.servers[0].channels {
            channel.instance = u64::from(channel.id) + 10;
        }
        client.servers[0].permissions.clear();
        assert!(client.servers[0].begin_permission_query(2, 0));
        assert!(!client.servers[0].begin_permission_query(2, 1));
        assert!(!client.servers[0].begin_permission_query(2, 1999));
        assert!(client.servers[0].begin_permission_query(2, 2000));
        assert!(
            !client.servers[0].begin_permission_query(2, 100000),
            "no per-frame or unlimited timeout retries"
        );
        client.servers[0].permissions.insert(0, 1); // previously known root Write
        client.handle(Input::Server {
            server: id.clone(),
            generation: 1,
            event: ServerEvent::Permissions {
                channel: 2,
                permissions: 0,
                flush: true,
            },
        });
        assert_eq!(client.servers[0].permissions.get(&2), Some(&0));
        assert!(
            !client.servers[0].permissions.contains_key(&0),
            "root is unknown after a child-only flush"
        );
        assert!(
            !client.servers[0].begin_permission_query(2, 100001),
            "a known denial is not polled repeatedly"
        );
        assert!(client.servers[0].begin_permission_query(3, 100001));
        client.handle(Input::Server {
            server: id.clone(),
            generation: 1,
            event: ServerEvent::ChannelRemoved(3),
        });
        assert!(!client.servers[0].permission_queries.contains_key(&3));
        client.handle(Input::Server {
            server: id.clone(),
            generation: 1,
            event: ServerEvent::Channel(Channel {
                id: 3,
                instance: 99,
                parent: Some(0),
                name: "Replacement".into(),
                ..Default::default()
            }),
        });
        assert!(client.servers[0].begin_permission_query(3, 100002));
        client.handle(Input::Reconnecting {
            server: id,
            generation: 1,
            attempt: 1,
            delay_secs: 1,
            reason: "owned permission fixture".into(),
        });
        assert!(client.servers[0].permission_queries.is_empty());
    }

    #[test]
    fn a_new_certificate_cannot_keep_old_rows_or_accept_an_old_history_page() {
        let (mut client, recorder, id) = connected();
        client.servers[0].bookmark.certificate_sha256 = Some("ab".repeat(32));
        client.open(0, 2).unwrap();
        let old_ticket = recorder.pages.lock().unwrap().last().unwrap().1.clone();
        client.handle(message(&id, 2, "old certificate's private history", 1));
        client.handle(message(&id, 3, "another cached conversation", 2));
        client.handle(Input::Server {
            server: id.clone(),
            generation: 1,
            event: ServerEvent::PeerCertificate("cd".repeat(32)),
        });
        assert!(
            client.servers[0]
                .channels
                .iter()
                .all(|channel| channel.timeline.messages.is_empty())
        );
        assert!(!client.navigation.accepts(&old_ticket));
        // An answer already in flight from the old scope must not restore rows.
        let Input::Message { message, .. } = message(&id, 2, "late old row", 3) else {
            unreachable!()
        };
        client.handle(Input::History {
            server: id.clone(),
            ticket: old_ticket,
            before: None,
            messages: vec![message],
            next_before: None,
        });
        let current = recorder.pages.lock().unwrap().last().unwrap().1.clone();
        client.handle(Input::History {
            server: id,
            ticket: current,
            before: None,
            messages: vec![],
            next_before: None,
        });
        assert!(
            client.servers[0]
                .channels
                .iter()
                .all(|channel| channel.timeline.messages.is_empty())
        );
    }

    #[test]
    fn the_same_certificate_preserves_loaded_rows_and_pending_pages() {
        let (mut client, recorder, id) = connected();
        client.open(0, 2).unwrap();
        let ticket = recorder.pages.lock().unwrap().last().unwrap().1.clone();
        client.handle(message(&id, 2, "keep this conversation", 1));
        client.handle(Input::Server {
            server: id,
            generation: 1,
            event: ServerEvent::PeerCertificate("ab".repeat(32)),
        });
        assert!(client.navigation.accepts(&ticket));
        assert_eq!(
            client.servers[0]
                .channels
                .iter()
                .find(|channel| channel.id == 2)
                .unwrap()
                .timeline
                .messages
                .len(),
            1
        );
    }

    #[test]
    fn webpki_leaf_renewal_keeps_origin_history_without_creating_a_pin() {
        let (mut client, recorder, id) = connected();
        assert!(client.servers[0].bookmark.certificate_sha256.is_none());
        client.open(0, 2).unwrap();
        let identity = client.servers[0].history_identity();
        let ticket = recorder.pages.lock().unwrap().last().unwrap().1.clone();
        client.handle(message(&id, 2, "verified origin history", 1));
        client.handle(Input::Server {
            server: id,
            generation: 1,
            event: ServerEvent::PeerCertificate("cd".repeat(32)),
        });
        assert_eq!(client.servers[0].history_identity(), identity);
        assert!(client.navigation.accepts(&ticket));
        assert!(client.servers[0].bookmark.certificate_sha256.is_none());
        assert_eq!(
            client.servers[0]
                .channels
                .iter()
                .find(|c| c.id == 2)
                .unwrap()
                .timeline
                .messages
                .len(),
            1
        );
    }

    #[test]
    fn accepting_a_changed_certificate_invalidates_the_old_view_before_reconnecting() {
        let (mut client, recorder, id) = connected();
        client.open(0, 2).unwrap();
        let ticket = recorder.pages.lock().unwrap().last().unwrap().1.clone();
        client.handle(message(&id, 2, "old scoped row", 1));
        client.servers[0].certificate = Some(CertificatePrompt {
            sha256: "cd".repeat(32),
            changed: true,
            reason: "changed".into(),
        });
        assert!(client.trust_certificate(0, false));
        assert!(!client.navigation.accepts(&ticket));
        assert!(
            client.servers[0]
                .channels
                .iter()
                .all(|channel| channel.timeline.messages.is_empty())
        );
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
            event: ServerEvent::Notice(nuschl_core::model::RECIPIENT_UNAVAILABLE_NOTICE.into()),
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
        client.servers[0].set_notice("a newer problem".into());
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
        client.servers[0].set_notice(nuschl_core::model::RECIPIENT_UNAVAILABLE_NOTICE.into());
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
    fn e2ee_only_sessions_cannot_enable_unprotected_voice_through_the_sdk() {
        let (mut client, recorder, id) = connected();
        client.handle(Input::Server {
            server: id,
            generation: 1,
            event: ServerEvent::Negotiated {
                capabilities: vec!["e2ee/1".into(), "mls/1".into()],
                policy: None,
            },
        });
        self_states(&recorder);
        client.set_mute(0, false);
        client.set_mute(0, true);
        client.speak_here(0, true);
        client.use_voice_here(0);
        client.toggle_voice(false);
        assert!(self_states(&recorder).is_empty());
        assert!(client.servers[0].muted);
        assert_eq!(client.voice_role(0), voice::Role::Offline);
        assert!(client.servers[0].audio_status.contains("not available yet"));
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
    fn encrypted_conversations_use_only_the_crypto_request_path() {
        let (mut client, recorder, id) = connected();
        client.servers[0].mls = true;
        let conversation = [7; 16];
        let channel = client.servers[0]
            .channels
            .iter_mut()
            .find(|channel| channel.id == 2)
            .unwrap();
        channel.conversation = Some(conversation);
        client.open(0, 2).unwrap();
        assert!(recorder.requests.lock().unwrap().iter().any(|(_, request)| matches!(request, Request::OpenEncrypted { conversation: id, channel: true, .. } if *id == conversation)));
        client.handle(Input::Server {
            server: id.clone(),
            generation: 1,
            event: ServerEvent::Encryption {
                conversation,
                state: crate::conversation::EncryptionState {
                    security: SessionSecurity::MlsEstablished,
                    description: "Encrypted".into(),
                    blocked: None,
                    epoch: 3,
                },
            },
        });
        let state = client.servers[0]
            .channels
            .iter()
            .find(|channel| channel.id == 2)
            .unwrap()
            .encryption
            .as_ref()
            .unwrap();
        assert_eq!(state.security, SessionSecurity::MlsEstablished);
        assert_eq!(client.send_text("private text".into()), Ok(()));
        assert!(
            matches!(recorder.requests.lock().unwrap().last(), Some((_, Request::SendEncrypted { conversation: id, text })) if *id == conversation && text == "private text")
        );
        client.handle(Input::SendFailed {
            server: id,
            generation: 1,
            reason: "Identity verification needed".into(),
        });
        assert!(!client.sending());
        assert!(
            client.servers[0]
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("verification"))
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
            event: ServerEvent::Welcome {
                source: "old".into(),
                document: MessageDocument { blocks: Vec::new() },
            },
        });
        assert_eq!(client.servers[0].welcome, None);
    }

    #[test]
    fn welcome_document_is_delivered_without_turning_worker_markers_into_text() {
        let (mut client, _, id) = connected();
        let source = "Welcome to <b>Nuschl</b><br>Second line".to_string();
        let document = MessageDocument {
            blocks: vec![ContentBlock::text("Welcome to Nuschl\nSecond line")],
        };
        client.handle(Input::Server {
            server: id.clone(),
            generation: 1,
            event: ServerEvent::Welcome {
                source: source.clone(),
                document: document.clone(),
            },
        });
        assert_eq!(client.servers[0].welcome.as_deref(), Some(source.as_str()));
        assert!(client.take_updates().iter().any(|update| matches!(update,
            Update::Rendered { server, key: RenderKey::Welcome, source: actual, document: rendered }
                if server == &id && actual == &source && rendered == &document)));

        client.handle(Input::Rendered {
            server: id,
            generation: 0,
            key: RenderKey::Welcome,
            source,
            document: MessageDocument {
                blocks: vec![ContentBlock::text("stale")],
            },
        });
        assert!(client.take_updates().is_empty());
    }

    #[test]
    fn welcome_is_revoked_on_disconnect_and_reconnect_and_late_work_is_ignored() {
        let (mut client, _, id) = connected();
        let source = "<b>Old welcome</b>".to_string();
        let document = MessageDocument {
            blocks: vec![ContentBlock::text("Old welcome")],
        };
        let welcome = || Input::Server {
            server: id.clone(),
            generation: 1,
            event: ServerEvent::Welcome {
                source: source.clone(),
                document: document.clone(),
            },
        };
        client.handle(welcome());
        client.take_updates();
        client.disconnect(0);
        assert!(client.servers[0].welcome.is_none());
        assert!(client.take_updates().iter().any(|update| matches!(update,
            Update::Rendered { key: RenderKey::Welcome, source, document, .. }
                if source.is_empty() && document.blocks.is_empty())));
        client.handle(welcome());
        client.handle(Input::Rendered {
            server: id.clone(),
            generation: 1,
            key: RenderKey::Welcome,
            source: source.clone(),
            document: document.clone(),
        });
        assert!(client.take_updates().is_empty());
        assert!(client.servers[0].welcome.is_none());

        client.connect(0, None, false);
        client.handle(Input::Server {
            server: id.clone(),
            generation: 1,
            event: ServerEvent::Connected { session: 1 },
        });
        client.handle(welcome());
        client.take_updates();
        client.connect(0, None, false);
        assert!(client.servers[0].welcome.is_none());
        assert!(client.take_updates().iter().any(|update| matches!(update,
            Update::Rendered { key: RenderKey::Welcome, source, document, .. }
                if source.is_empty() && document.blocks.is_empty())));
    }

    fn document_event(id: &str, event: ServerEvent) -> Input {
        Input::Server {
            server: id.into(),
            generation: 1,
            event,
        }
    }

    fn description(id: u32, hash: &str, body: &str) -> Channel {
        Channel {
            id,
            parent: Some(0),
            name: format!("Channel {id}"),
            description_hash: Some(hash.into()),
            description: body.into(),
            ..Default::default()
        }
    }

    fn comment_user(session: u32, instance: u64, hash: &str, body: Option<&str>) -> User {
        User {
            session,
            instance,
            name: format!("User {session}"),
            channel: 2,
            comment_hash: Some(hash.into()),
            comment: body.map(str::to_owned),
            ..Default::default()
        }
    }

    fn blob_requests(recorder: &Recorder) -> Vec<Request> {
        recorder
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|(_, request)| request.clone())
            .filter(|request| matches!(request, Request::RequestBlobs { .. }))
            .collect()
    }

    fn rendered_comment(id: &str, source: &str) -> Input {
        Input::Rendered {
            server: id.into(),
            generation: 1,
            key: RenderKey::Comment(7),
            source: source.into(),
            document: MessageDocument {
                blocks: vec![ContentBlock::text("prepared comment")],
            },
        }
    }

    #[test]
    fn hash_only_descriptions_fetch_selected_channel_once_and_render_arrival() {
        let (mut client, recorder, id) = connected();
        let first = "ab".repeat(20);
        let second = "cd".repeat(20);
        client.handle(document_event(
            &id,
            ServerEvent::Channel(description(2, &first, "")),
        ));
        assert!(
            blob_requests(&recorder).is_empty(),
            "unselected descriptions must not fetch"
        );
        client.open(0, 2).unwrap();
        client.open(0, 2).unwrap();
        client.handle(document_event(
            &id,
            ServerEvent::Channel(description(2, &first, "")),
        ));
        assert_eq!(
            blob_requests(&recorder),
            vec![Request::RequestBlobs {
                textures: vec![],
                comments: vec![],
                descriptions: vec![2]
            }]
        );
        let body = format!("<b>Long description</b><br>{}", "x".repeat(160));
        client.handle(document_event(
            &id,
            ServerEvent::Channel(description(2, &first, &body)),
        ));
        assert_eq!(
            recorder.rendered.lock().unwrap().as_slice(),
            &[(id.clone(), 1, RenderKey::Description(2), body.clone())]
        );
        client.handle(document_event(
            &id,
            ServerEvent::Channel(description(2, &second, "")),
        ));
        assert_eq!(
            blob_requests(&recorder).len(),
            2,
            "new selected hash must fetch once"
        );
        client.open(0, 3).unwrap();
        client.handle(document_event(
            &id,
            ServerEvent::Channel(description(2, &first, "")),
        ));
        assert_eq!(blob_requests(&recorder).len(), 2);
        client.open(0, 2).unwrap();
        assert_eq!(blob_requests(&recorder).len(), 3);
        client.handle(document_event(&id, ServerEvent::ChannelRemoved(2)));
        assert!(!client.description_requests[&id].channels.contains_key(&2));
        client.disconnect(0);
        assert!(!client.description_requests.contains_key(&id));
    }

    #[test]
    fn selected_cached_description_waits_until_connected_and_invalid_hashes_are_not_copied() {
        let (mut client, recorder, id) = connected();
        client.open(0, 2).unwrap();
        client.servers[0].status = Status::Connecting;
        client.handle(document_event(
            &id,
            ServerEvent::Channel(description(2, &"ab".repeat(20), "")),
        ));
        assert!(blob_requests(&recorder).is_empty());
        client.handle(document_event(&id, ServerEvent::Connected { session: 1 }));
        assert_eq!(blob_requests(&recorder).len(), 1);
        client.handle(document_event(
            &id,
            ServerEvent::Channel(description(2, &"x".repeat(4096), "")),
        ));
        assert!(!client.description_requests[&id].channels.contains_key(&2));
        assert_eq!(blob_requests(&recorder).len(), 1);
    }

    #[test]
    fn requested_comment_renders_when_body_arrives_without_reopening_and_deduplicates() {
        let (mut client, recorder, id) = connected();
        let hash = "ab".repeat(20);
        let user = comment_user(7, 70, &hash, None);
        client.handle(document_event(&id, ServerEvent::User(user.clone())));
        assert!(blob_requests(&recorder).is_empty());
        client.open_comment(0, 7, None);
        client.open_comment(0, 7, None);
        client.retain_comment_interest(Some((0, 7)));
        assert_eq!(
            blob_requests(&recorder),
            vec![Request::RequestBlobs {
                textures: vec![],
                comments: vec![7],
                descriptions: vec![]
            }]
        );
        let body = format!("<b>Long comment</b><br>{}", "y".repeat(160));
        let with_body = comment_user(7, 70, &hash, Some(&body));
        client.handle(document_event(&id, ServerEvent::User(with_body.clone())));
        client.handle(document_event(&id, ServerEvent::User(with_body)));
        client.handle(document_event(
            &id,
            ServerEvent::User(comment_user(8, 80, &hash, Some("unrelated"))),
        ));
        assert_eq!(
            recorder.rendered.lock().unwrap().as_slice(),
            &[(id.clone(), 1, RenderKey::Comment(7), body.clone())]
        );
        client.handle(rendered_comment(&id, &body));
        assert!(client.take_updates().iter().any(|update| matches!(
            update,
            Update::Rendered {
                key: RenderKey::Comment(7),
                ..
            }
        )));
        client.handle(document_event(
            &id,
            ServerEvent::User(comment_user(7, 70, &"cd".repeat(20), None)),
        ));
        assert_eq!(
            blob_requests(&recorder).len(),
            2,
            "changed hash while open fetches new body"
        );
        client.handle(rendered_comment(&id, &body));
        assert!(
            !client
                .take_updates()
                .iter()
                .any(|update| matches!(update, Update::Rendered { .. }))
        );
    }

    #[test]
    fn prepared_comment_source_is_reused_and_uncached_pending_body_is_not_resubmitted() {
        let (mut client, recorder, id) = connected();
        let body = "<b>Existing complete profile comment</b>";
        client.handle(document_event(
            &id,
            ServerEvent::User(comment_user(7, 70, &"ab".repeat(20), Some(body))),
        ));
        assert_eq!(client.servers[0].users[&7].comment.as_deref(), Some(body));
        client.open_comment(0, 7, Some(body));
        assert!(
            recorder.rendered.lock().unwrap().is_empty(),
            "matching prepared source must skip parsing a present body"
        );
        client.retain_comment_interest(None);
        client.open_comment(0, 7, Some("outdated cached source"));
        assert_eq!(
            recorder.rendered.lock().unwrap().as_slice(),
            &[(id, 1, RenderKey::Comment(7), body.into())],
            "a wrong cached source must render the present current body"
        );
        client.open_comment(0, 7, None);
        client.open_comment(0, 7, Some("outdated cached source"));
        assert_eq!(
            recorder.rendered.lock().unwrap().len(),
            1,
            "repeated opens must not resubmit the same pending body"
        );
    }

    #[test]
    fn hidden_or_replaced_comment_interest_cannot_schedule_or_accept_old_work() {
        let (mut client, recorder, id) = connected();
        let hash = "ab".repeat(20);
        client.handle(document_event(
            &id,
            ServerEvent::User(comment_user(7, 70, &hash, None)),
        ));
        client.open_comment(0, 7, None);
        client.retain_comment_interest(None);
        client.handle(document_event(
            &id,
            ServerEvent::User(comment_user(7, 70, &hash, Some("hidden body"))),
        ));
        client.handle(rendered_comment(&id, "hidden body"));
        assert!(recorder.rendered.lock().unwrap().is_empty());
        assert!(
            !client
                .take_updates()
                .iter()
                .any(|update| matches!(update, Update::Rendered { .. }))
        );
        client.open_comment(0, 7, None);
        recorder.rendered.lock().unwrap().clear();
        client.handle(document_event(
            &id,
            ServerEvent::User(comment_user(8, 80, &hash, None)),
        ));
        client.open_comment(0, 8, None);
        client.handle(document_event(
            &id,
            ServerEvent::User(comment_user(7, 70, &hash, Some("late previous profile"))),
        ));
        client.handle(rendered_comment(&id, "late previous profile"));
        assert!(recorder.rendered.lock().unwrap().is_empty());
        assert!(
            !client
                .take_updates()
                .iter()
                .any(|update| matches!(update, Update::Rendered { .. }))
        );
        client.retain_comment_interest(Some((0, 7)));
        assert!(
            client.comment_interest.is_none(),
            "retaining cannot open a different profile"
        );
    }

    #[test]
    fn reused_sessions_and_reconnections_revoke_comment_interest_permanently() {
        let (mut client, recorder, id) = connected();
        let hash = "ab".repeat(20);
        client.handle(document_event(
            &id,
            ServerEvent::User(comment_user(7, 70, &hash, Some("old body"))),
        ));
        client.open_comment(0, 7, None);
        recorder.rendered.lock().unwrap().clear();
        client.handle(document_event(
            &id,
            ServerEvent::User(comment_user(7, 71, &hash, Some("old body"))),
        ));
        client.handle(rendered_comment(&id, "old body"));
        assert!(client.comment_interest.is_none());
        assert!(recorder.rendered.lock().unwrap().is_empty());
        assert!(
            !client
                .take_updates()
                .iter()
                .any(|update| matches!(update, Update::Rendered { .. }))
        );
        client.open_comment(0, 7, None);
        recorder.rendered.lock().unwrap().clear();
        client.disconnect(0);
        client.retain_comment_interest(Some((0, 7)));
        client.servers[0].generation = 2;
        client.servers[0].status = Status::Connected;
        client.handle(document_event(
            &id,
            ServerEvent::User(comment_user(7, 71, &hash, Some("old body"))),
        ));
        client.handle(rendered_comment(&id, "old body"));
        assert!(client.servers[0].users.is_empty());
        assert!(recorder.rendered.lock().unwrap().is_empty());
        assert!(client.comment_interest.is_none());
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
        assert!(!link.require_nuschl, "a stock server");
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
            Some(nuschl_core::share::encode_token(&[7; 16]).as_str())
        );
        assert!(link.require_nuschl);
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
        let token = nuschl_core::share::encode_token(&[9; 16]);
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
        client.open_share_token(0, nuschl_core::share::encode_token(&[8; 16]));
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
        client.open_share_token(0, nuschl_core::share::encode_token(&[1; 16]));
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
    #[test]
    fn automatic_hash_refresh_carries_original_comment_interest_and_channel_lifetimes() {
        let (mut client, recorder, id) = connected();
        client.servers[0].users.get_mut(&1).unwrap().instance = 11;
        client.handle(document_event(
            &id,
            ServerEvent::User(comment_user(7, 70, &"ab".repeat(20), None)),
        ));
        client.open_comment(0, 7, None);
        let first = recorder.metadata.lock().unwrap().last().unwrap().clone();
        assert_eq!(first.own, (1, 11));
        assert_eq!(first.users, vec![(7, 70)]);
        client.handle(document_event(
            &id,
            ServerEvent::User(comment_user(7, 70, &"cd".repeat(20), None)),
        ));
        assert_eq!(
            recorder.metadata.lock().unwrap().last().unwrap().users,
            vec![(7, 70)]
        );
        let count = recorder.metadata.lock().unwrap().len();
        client.handle(document_event(
            &id,
            ServerEvent::User(comment_user(7, 700, &"ef".repeat(20), None)),
        ));
        assert_eq!(
            recorder.metadata.lock().unwrap().len(),
            count,
            "replacement presence cannot inherit profile interest"
        );
        client.open(0, 2).unwrap();
        let mut channel = description(2, &"aa".repeat(20), "");
        channel.instance = 25;
        client.handle(document_event(&id, ServerEvent::Channel(channel)));
        assert_eq!(
            recorder.metadata.lock().unwrap().last().unwrap().channels,
            vec![(2, 25)]
        );
        assert_eq!(
            client.servers[0]
                .channels
                .iter()
                .find(|channel| channel.id == 2)
                .unwrap()
                .instance,
            25
        );
    }
    #[test]
    fn prelogin_hash_only_avatars_wait_for_own_presence_then_fetch_once() {
        let (mut client, recorder, id) = connected();
        client.servers[0].own_session = None;
        client.servers[0].users.remove(&1);
        client.handle(document_event(
            &id,
            ServerEvent::User(User {
                session: 7,
                instance: 70,
                channel: 2,
                texture_hash: Some("cd".repeat(20)),
                ..Default::default()
            }),
        ));
        assert!(blob_requests(&recorder).is_empty());
        client.handle(document_event(&id, ServerEvent::Connected { session: 1 }));
        assert!(blob_requests(&recorder).is_empty());
        client.handle(document_event(
            &id,
            ServerEvent::User(User {
                session: 1,
                instance: 11,
                channel: 2,
                ..Default::default()
            }),
        ));
        assert_eq!(
            blob_requests(&recorder),
            vec![Request::RequestBlobs {
                textures: vec![7],
                comments: vec![],
                descriptions: vec![]
            }]
        );
        assert_eq!(
            recorder.metadata.lock().unwrap().last().unwrap().own,
            (1, 11)
        );
        client.handle(document_event(
            &id,
            ServerEvent::User(User {
                session: 1,
                instance: 11,
                channel: 2,
                ..Default::default()
            }),
        ));
        assert_eq!(blob_requests(&recorder).len(), 1);
    }
}
