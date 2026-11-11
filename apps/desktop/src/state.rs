// SPDX-License-Identifier: EUPL-1.2
use crate::document::Document;
use crate::{
    attachments::{Attachment, Kind, Source},
    backend::{Backend, Event, RenderKey, RenderedMessage},
    scrolling::{Anchor, SmoothScroll},
};
use gpui_kit::{
    base::input::{InputEvent, InputState, TextareaState},
    *,
};
use nuschel_client::{Conversation, HistorySource, Navigation, SessionSecurity};
use nuschel_core::MAX_CONTENT_BYTES;
use nuschel_session::{
    config::{
        CachedChannel, ClientSettings, Loaded, ManagedSettings, MumbleUrl, Profile, ServerBookmark,
        ShortcutAction,
    },
    connection::{Command, ServerLimits, SessionEvent, TextTarget, User},
    model::{Acl, Ban, ContextAction, RegisteredUser, RejectKind, UserStats},
    udp::TransportStats,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::{Arc, Mutex, PoisonError},
};

pub use crate::panels::Panel;

pub struct MessageView {
    pub id: Option<i64>,
    /// Identifies the message to the UI (its text selection) while it lives.
    pub serial: u64,
    pub author: String,
    pub observed_at_ms: i64,
    pub time: String,
    pub status: Option<&'static str>,
    pub document: Document,
    pub expanded: bool,
    /// Laid-out content height from the last frame, so "Show more" follows
    /// the real wrapped height (window width, images, code) rather than a
    /// character count.
    pub measured: std::rc::Rc<std::cell::Cell<f32>>,
    /// A file offered with this message (transfer.rs shows it as a card).
    pub offer: Option<nuschel_core::transfer::Offer>,
    /// The row's height when last laid out, for the room it keeps while
    /// scrolled out of view.
    pub height: crate::scrolling::Height,
}
/// Name a text field for screen readers. GPUI-kit inputs expose no role or
/// name to AT-SPI/UIA/AX unless the state is told what it is.
pub fn accessible<M: gpui_kit::base::input::InputModeKind>(
    mut state: gpui_kit::base::input::InputBaseState<M>,
    label: &str,
    multiline: bool,
    secret: bool,
) -> gpui_kit::base::input::InputBaseState<M> {
    let role = if multiline {
        Role::MultilineTextInput
    } else if secret {
        Role::PasswordInput
    } else {
        Role::TextInput
    };
    state.set_accessibility(
        gpui_kit::base::input::InputAccessibility::new(Some(role))
            .with_label(Some(label.to_string().into()))
            .with_value_exposed(!secret),
    );
    state
}
/// Messages taller than this are collapsed until expanded.
pub const COLLAPSED_HEIGHT: f32 = 300.;
/// Messages a timeline keeps while its reader is at the bottom; older ones
/// are dropped and load again from local history on scrolling up.
pub const MAX_TIMELINE: usize = 1000;
impl MessageView {
    pub fn has_more(&self) -> bool {
        self.measured.get() > COLLAPSED_HEIGHT + 1.
    }
}
impl From<RenderedMessage> for MessageView {
    fn from(message: RenderedMessage) -> Self {
        static SERIALS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let offer = nuschel_core::transfer::parse_offer(&message.message.body);
        Self {
            id: message.id,
            serial: SERIALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            author: message.message.author,
            observed_at_ms: message.message.observed_at_ms,
            time: local_time(message.message.observed_at_ms),
            status: if message.id.is_none() {
                Some("not saved")
            } else if message.outgoing {
                Some("sent")
            } else {
                None
            },
            measured: Default::default(),
            height: Default::default(),
            document: message.document.into(),
            expanded: false,
            offer,
        }
    }
}
/// "Disconnected from 14:51 to 14:52 (40 s)…", with dates only when they differ from today.
fn gap_text(from_ms: i64, to_ms: i64) -> String {
    let seconds = (to_ms - from_ms).max(0) / 1000;
    let duration = if seconds < 120 {
        format!("{seconds} s")
    } else {
        format!("{} min", seconds / 60)
    };
    let strip = |text: String| text.trim_start_matches("Today at ").to_string();
    format!(
        "Disconnected from {} to {} ({duration}). Messages sent meanwhile were not received.",
        strip(local_time(from_ms)),
        strip(local_time(to_ms))
    )
}

/// Local observation time, labelled relative to today for recent messages.
pub fn local_time(milliseconds: i64) -> String {
    let zone = jiff::tz::TimeZone::system();
    let Ok(stamp) = jiff::Timestamp::from_millisecond(milliseconds) else {
        return String::new();
    };
    let when = stamp.to_zoned(zone.clone());
    let today = jiff::Timestamp::now().to_zoned(zone).date();
    let time = when.strftime("%H:%M").to_string();
    if when.date() == today {
        format!("Today at {time}")
    } else if today
        .yesterday()
        .is_ok_and(|yesterday| yesterday == when.date())
    {
        format!("Yesterday at {time}")
    } else {
        format!("{} {time}", when.strftime("%Y-%m-%d"))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeRow {
    Channel(ChannelRow),
    User {
        session: u32,
        depth: usize,
    },
    /// Someone listening to (not in) this channel, Mumble 1.4+.
    Listener {
        session: u32,
        depth: usize,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelRow {
    /// Into `ServerView::channels`.
    pub index: usize,
    pub depth: usize,
    pub has_children: bool,
    pub expanded: bool,
    /// Users hidden inside a collapsed branch, shown as a count.
    pub hidden_users: usize,
    /// Someone in that collapsed branch is talking.
    pub hidden_talking: bool,
}
pub struct ChannelView {
    pub id: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub topic: String,
    pub position: i32,
    pub target: TextTarget,
    pub direct_peer: Option<User>,
    pub storage_key: String,
    /// One timeline: the latest page, older pages prepended as the reader
    /// scrolls up, and live messages appended.
    pub messages: Vec<MessageView>,
    /// Cursor for the next older page, if local history has more.
    pub next_before: Option<i64>,
    /// Set while the timeline shows older history apart from the newest
    /// (after a search result opened a message): the next newer page.
    pub next_after: Option<i64>,
    /// A live message was left out of such a timeline since the newer page
    /// was asked for.
    pub missed_live: bool,
    /// The oldest stored message is loaded: there is nothing further up.
    pub beginning: bool,
    /// The latest page is in memory; live messages keep it current, so
    /// opening the conversation again needs no reload.
    pub loaded: bool,
    pub loading: bool,
    /// Messages that arrived while the reader was scrolled up.
    pub new_messages: usize,
    /// Messages that arrived out of sight, and whether one mentions you or
    /// is a direct message (shown in the channel list and server rail).
    pub unread: usize,
    pub unread_mention: bool,
    /// Joins, moves, moderation and connection gaps; in memory only.
    pub activity: std::collections::VecDeque<ActivityLine>,
    /// Runs of activity lines shown expanded, by their first line's time.
    pub expanded_runs: BTreeSet<i64>,
    pub links: BTreeSet<u32>,
    pub temporary: bool,
    pub max_users: u32,
    /// Raw Mumble HTML description and its inert rendering from the content worker.
    pub description_raw: String,
    pub description: Option<Document>,
}
#[derive(Clone)]
pub struct ActivityLine {
    pub observed_at_ms: i64,
    pub text: String,
    pub warning: bool,
    pub height: crate::scrolling::Height,
    /// The height of the collapsed (or header) row of a run starting here.
    pub run_height: crate::scrolling::Height,
}
pub const MAX_ACTIVITY: usize = 200;
impl Default for ChannelView {
    fn default() -> Self {
        Self::root()
    }
}
impl ChannelView {
    pub fn log(&mut self, text: String, observed_at_ms: i64, warning: bool) {
        self.activity.push_back(ActivityLine {
            observed_at_ms,
            text,
            warning,
            height: Default::default(),
            run_height: Default::default(),
        });
        while self.activity.len() > MAX_ACTIVITY {
            if let Some(gone) = self.activity.pop_front() {
                self.expanded_runs
                    .retain(|first| *first > gone.observed_at_ms);
            }
        }
    }
    fn root() -> Self {
        Self {
            id: 0,
            parent: None,
            name: "Root".into(),
            topic: "Connect to load channels and users".into(),
            position: 0,
            target: TextTarget::Channel(0),
            direct_peer: None,
            storage_key: "0".into(),
            messages: Vec::new(),
            next_before: None,
            next_after: None,
            missed_live: false,
            beginning: false,
            loaded: false,
            loading: false,
            new_messages: 0,
            unread: 0,
            unread_mention: false,
            activity: Default::default(),
            expanded_runs: BTreeSet::new(),
            links: BTreeSet::new(),
            temporary: false,
            max_users: 0,
            description_raw: String::new(),
            description: None,
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Disconnected,
    Connecting,
    Connected,
}
pub struct CertificatePrompt {
    pub sha256: String,
    pub changed: bool,
    pub reason: String,
}
/// What the media worker reports about the active audio route.
#[derive(Default)]
pub struct AudioInfo {
    pub inputs: Vec<nuschel_core::audio::AudioDevice>,
    pub outputs: Vec<nuschel_core::audio::AudioDevice>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub echo: nuschel_core::audio::EchoSource,
    /// A noise suppressor upstream of the microphone (Nuschel's own is off).
    pub noise: Option<String>,
    /// The output Nuschel's canceller records (`None`: its own playback only).
    pub reference: Option<String>,
    pub erle_db: Option<f32>,
    pub residual_echo: Option<f32>,
    pub health: AudioHealth,
}
/// The media worker's adaptive controller: what it measured and what it did
/// with Nuschel's own playback buffer and transmit gate.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioHealth {
    pub clipping: bool,
    pub echo_guard: bool,
    pub double_talk: f32,
    pub underruns: u32,
    pub playback_ms: u32,
}
/// The smallest playback buffer the controller uses.
const NORMAL_PLAYBACK_MS: u32 = 40;
impl AudioHealth {
    /// (short label, advice) for each condition the user may want to act on.
    pub fn issues(&self) -> Vec<(&'static str, String)> {
        let mut issues = Vec::new();
        if self.clipping {
            issues.push((
                "Microphone is clipping",
                "Your microphone reaches its maximum level before Nuschel receives it, which distorts \
                 your voice. Lower the microphone's input level in the system sound settings."
                    .to_string(),
            ));
        }
        if self.echo_guard {
            issues.push((
                "Echo detected",
                "Some of what you hear keeps reaching your microphone. While others talk, Nuschel now \
                 needs clearer speech before it transmits. Headphones avoid echo entirely."
                    .to_string(),
            ));
        }
        if self.playback_ms > NORMAL_PLAYBACK_MS {
            issues.push((
                "Playback stuttered",
                format!(
                    "Audio output ran dry, so Nuschel now buffers {} ms of playback. It returns to \
                     normal after a minute without stutters.",
                    self.playback_ms
                ),
            ));
        }
        issues
    }
}
pub struct ServerView {
    pub id: String,
    pub name: String,
    pub bookmark: ServerBookmark,
    pub history_source: HistorySource,
    pub security: SessionSecurity,
    pub channels: Vec<ChannelView>,
    pub status: Status,
    pub generation: u64,
    pub peer_sha256: Option<String>,
    pub own_session: Option<u32>,
    pub users: BTreeMap<u32, User>,
    pub seen_channels: BTreeSet<u32>,
    pub muted: bool,
    pub deafened: bool,
    pub notice: Option<String>,
    pub audio_status: String,
    pub welcome: Option<String>,
    pub certificate: Option<CertificatePrompt>,
    pub talking: BTreeSet<u32>,
    pub transport: Option<TransportStats>,
    pub audio: AudioInfo,
    pub permissions: BTreeMap<u32, u32>,
    pub limits: Option<ServerLimits>,
    pub context_actions: Vec<ContextAction>,
    /// A server recommendation the user can act on (e.g. push-to-talk).
    pub suggestion: Option<String>,
    pub stats: BTreeMap<u32, UserStats>,
    /// Users heard speaking with CELT/Speex, which Nuschel cannot play.
    pub legacy_voice: BTreeSet<u32>,
    pub registered_users: Option<Vec<RegisteredUser>>,
    pub bans: Option<Vec<Ban>>,
    pub acl: Option<Acl>,
    pub user_names: BTreeMap<u32, String>,
    pub reconnecting: Option<String>,
    /// Sent and total messages of the text waiting for the server's rate limit.
    pub outbox: Option<(usize, usize)>,
    /// Rendered user comments keyed by session, with the raw HTML they came from.
    pub comments: BTreeMap<u32, (String, Document)>,
    /// Decoded avatars by texture hash, and hashes already requested.
    pub avatars: BTreeMap<String, std::sync::Arc<RenderImage>>,
    /// The server website's icon, from the bookmark.
    pub icon: Option<std::sync::Arc<RenderImage>>,
    pub avatar_requests: BTreeSet<String>,
    /// Active whisper/shout target label while one is selected.
    pub whisper: Option<String>,
    /// Channel path from a `mumble://` link, joined once the tree has loaded.
    pub pending_channel: Vec<String>,
    /// Branches opened or closed by hand this session, by channel id.
    pub expansion: BTreeMap<u32, bool>,
    /// Channel updates arrived since `channels` was last sorted.
    unsorted: bool,
}
impl ServerView {
    fn new(bookmark: ServerBookmark) -> Self {
        let icon = bookmark
            .icon
            .as_ref()
            .and_then(|icon| crate::document::render_image(icon.width, icon.height, &icon.rgba));
        let mut channels = bookmark
            .cached_channels
            .iter()
            .map(|channel| ChannelView {
                id: channel.id,
                parent: channel.parent,
                name: channel.name.clone(),
                topic: "Saved channel history".into(),
                position: channel.position,
                target: TextTarget::Channel(channel.id),
                direct_peer: None,
                storage_key: channel.id.to_string(),
                messages: Vec::new(),
                next_before: None,
                loading: false,
                new_messages: 0,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        if !channels.iter().any(|channel| channel.id == 0) {
            channels.insert(0, ChannelView::root());
        }
        Self {
            id: bookmark.id.clone(),
            name: bookmark.name.clone(),
            bookmark,
            history_source: HistorySource::LocalObserved,
            security: SessionSecurity::Unknown,
            channels,
            status: Status::Disconnected,
            generation: 0,
            peer_sha256: None,
            own_session: None,
            users: BTreeMap::new(),
            seen_channels: BTreeSet::new(),
            muted: true,
            deafened: false,
            notice: None,
            audio_status: "Audio disconnected".into(),
            welcome: None,
            certificate: None,
            talking: BTreeSet::new(),
            transport: None,
            audio: AudioInfo::default(),
            permissions: BTreeMap::new(),
            limits: None,
            context_actions: Vec::new(),
            suggestion: None,
            stats: BTreeMap::new(),
            legacy_voice: BTreeSet::new(),
            registered_users: None,
            bans: None,
            acl: None,
            user_names: BTreeMap::new(),
            reconnecting: None,
            outbox: None,
            comments: BTreeMap::new(),
            avatars: BTreeMap::new(),
            icon,
            avatar_requests: BTreeSet::new(),
            whisper: None,
            pending_channel: Vec::new(),
            expansion: BTreeMap::new(),
            unsorted: false,
        }
    }
    pub fn status_label(&self) -> String {
        match self.status {
            Status::Disconnected => "Disconnected".into(),
            Status::Connecting => "Connecting…".into(),
            Status::Connected => match &self.transport {
                Some(stats) if stats.udp_active => {
                    format!("Connected · UDP · {:.0} ms", stats.udp.average_ms)
                }
                Some(stats) if stats.tcp.packets > 0 => format!(
                    "Connected · TCP{} · {:.0} ms",
                    if stats.forced_tcp { " (forced)" } else { "" },
                    stats.tcp.average_ms
                ),
                _ => "Connected".into(),
            },
        }
    }
    /// Effective permission bits for a channel, if the server has told us.
    pub fn may(&self, channel: u32, bits: u32) -> bool {
        self.permissions
            .get(&channel)
            .is_some_and(|permissions| permissions & bits == bits)
    }
    /// Like `may`, but optimistic while permissions are still unknown.
    pub fn may_try(&self, channel: u32, bits: u32) -> bool {
        self.permissions
            .get(&channel)
            .is_none_or(|permissions| permissions & bits == bits)
    }
    pub fn channel_view(&mut self, id: u32) -> Option<&mut ChannelView> {
        self.channels
            .iter_mut()
            .find(|channel| channel.target == TextTarget::Channel(id))
    }
    pub fn voice_channel(&self) -> Option<u32> {
        self.own_session
            .and_then(|session| self.users.get(&session))
            .map(|user| user.channel)
    }
    pub fn history_identity(&self) -> Option<String> {
        self.peer_sha256
            .as_ref()
            .or(self.bookmark.certificate_sha256.as_ref())
            .map(|pin| self.bookmark.history_identity(pin))
    }
    /// Channels in tree order, each followed by its people and listeners;
    /// a closed branch counts the people inside it. One pass over channels
    /// and users, so it is cheap enough to run every frame.
    pub fn tree_rows(&self) -> Vec<TreeRow> {
        // One entry per channel id; direct conversations are listed apart.
        let mut index_of = HashMap::with_capacity(self.channels.len());
        for (index, channel) in self.channels.iter().enumerate() {
            if let TextTarget::Channel(id) = channel.target {
                index_of.entry(id).or_insert(index);
            }
        }
        // Families by parent (roots under `None`), each in display order.
        let mut children = index_of
            .values()
            .map(|&index| (self.channels[index].parent, index))
            .collect::<Vec<_>>();
        children.sort_unstable();
        let family = |parent| run(&children, parent, |&(parent, _)| parent);
        let mut people = self.users.values().collect::<Vec<_>>();
        people.sort_by_cached_key(|user| (user.channel, user.name.to_lowercase(), user.session));
        let mut listeners = self
            .users
            .values()
            .flat_map(|user| user.listening.iter().map(|&id| (id, user.session)))
            .collect::<Vec<_>>();
        listeners.sort_unstable();
        // The branch holding our voice channel starts open.
        let parent_of = |index: usize| {
            let parent = self.channels[index].parent?;
            index_of.get(&parent).copied()
        };
        let mut holds_voice = vec![false; self.channels.len()];
        let mut voice = self
            .voice_channel()
            .and_then(|id| index_of.get(&id).copied());
        while let Some(parent) = voice.and_then(parent_of)
            && !holds_voice[parent]
        {
            holds_voice[parent] = true;
            voice = Some(parent);
        }

        // Depth first through every branch, closed ones too: they count
        // their people. A child always follows its parent. With one entry
        // per id each channel is in one family, so a cycle is unreachable.
        struct Node<'a> {
            row: ChannelRow,
            parent: Option<usize>,
            visible: bool,
            people: &'a [&'a User],
        }
        let mut nodes = Vec::<Node>::with_capacity(index_of.len());
        let mut pending = family(None)
            .iter()
            .rev()
            .map(|&(_, index)| (index, 0, None, true))
            .collect::<Vec<_>>();
        while let Some((index, depth, parent, visible)) = pending.pop() {
            let channel = &self.channels[index];
            let expanded = self
                .expansion
                .get(&channel.id)
                .copied()
                .unwrap_or(channel.parent.is_none() || holds_voice[index]);
            let children = family(Some(channel.id));
            let slot = nodes.len();
            pending.extend(
                children
                    .iter()
                    .rev()
                    .map(|&(_, child)| (child, depth + 1, Some(slot), visible && expanded)),
            );
            nodes.push(Node {
                row: ChannelRow {
                    index,
                    depth,
                    has_children: !children.is_empty(),
                    expanded,
                    hidden_users: 0,
                    hidden_talking: false,
                },
                parent,
                visible,
                people: run(&people, channel.id, |user| user.channel),
            });
        }
        // Children before parents: everyone inside adds up towards the roots.
        for slot in (0..nodes.len()).rev() {
            let node = &nodes[slot];
            let Some(parent) = node.parent else {
                continue;
            };
            let inside = node.row.hidden_users + node.people.len();
            let talking = node.row.hidden_talking
                || node
                    .people
                    .iter()
                    .any(|user| self.talking.contains(&user.session));
            let parent = &mut nodes[parent].row;
            parent.hidden_users += inside;
            parent.hidden_talking |= talking;
        }

        let mut rows = Vec::new();
        for Node {
            mut row, people, ..
        } in nodes.into_iter().filter(|node| node.visible)
        {
            if row.expanded {
                (row.hidden_users, row.hidden_talking) = (0, false);
            }
            let id = self.channels[row.index].id;
            let depth = row.depth + 1;
            rows.push(TreeRow::Channel(row));
            rows.extend(people.iter().map(|user| TreeRow::User {
                session: user.session,
                depth,
            }));
            rows.extend(
                run(&listeners, id, |&(channel, _)| channel)
                    .iter()
                    .map(|&(_, session)| TreeRow::Listener { session, depth }),
            );
        }
        rows
    }
    /// Open or close a branch by hand; the choice outlasts the defaults.
    pub fn set_expanded(&mut self, id: u32, expanded: bool) {
        self.expansion.insert(id, expanded);
    }
    /// Branches where people are open on every connect; anyone who arrives
    /// later in a collapsed branch shows as a count on it.
    fn expand_occupied(&mut self) {
        let mut parents = HashMap::with_capacity(self.channels.len());
        for channel in &self.channels {
            if let TextTarget::Channel(id) = channel.target {
                parents.entry(id).or_insert(channel.parent);
            }
        }
        // Each channel's ancestors are walked once.
        let mut walked = HashSet::new();
        for user in self.users.values() {
            let mut current = user.channel;
            while walked.insert(current)
                && let Some(&Some(parent)) = parents.get(&current)
            {
                self.expansion.insert(parent, true);
                current = parent;
            }
        }
    }
    /// Channels in display order: by position, then name.
    fn sort_channels(&mut self) {
        self.channels.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| a.name.cmp(&b.name))
        });
    }
}

/// The run of `sorted` (ascending by `key`) whose key is `wanted`.
fn run<T, K: Ord>(sorted: &[T], wanted: K, key: impl Fn(&T) -> K) -> &[T] {
    let start = sorted.partition_point(|item| key(item) < wanted);
    let len = sorted[start..].partition_point(|item| key(item) == wanted);
    &sorted[start..start + len]
}

/// How long changes to the saved servers gather before they are written.
const SAVE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
/// Serializes writes of the saved servers and remembers the newest one on
/// disk, so a snapshot taken earlier never overwrites a later one.
#[derive(Clone, Default)]
struct ServerWrites(Arc<Mutex<u64>>);
impl ServerWrites {
    fn write(
        &self,
        profile: &Profile,
        generation: u64,
        servers: &[ServerBookmark],
    ) -> std::io::Result<()> {
        let mut written = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if *written < generation {
            profile.save_servers(servers)?;
            *written = generation;
        }
        Ok(())
    }
    fn written(&self) -> u64 {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct ServerForm {
    pub editing: Option<String>,
    pub name: Entity<InputState>,
    pub host: Entity<InputState>,
    pub port: Entity<InputState>,
    pub username: Entity<InputState>,
    pub password: Entity<InputState>,
    pub auto_connect: bool,
    /// managed.json declares the server: only the password can be entered.
    pub managed: bool,
    pub error: Option<String>,
    /// The address the name last followed: the name tracks the address
    /// until it is changed to something else.
    last_host: String,
    _subscriptions: Vec<Subscription>,
}

struct PendingSend {
    conversation: Conversation,
    target: TextTarget,
    body: String,
}

/// A sent message with attachments, waiting for its images to be prepared.
struct Outgoing {
    /// The conversation, by server id and channel.
    key: (String, u32),
    target: (TextTarget, Option<u64>),
    text: String,
    attachments: Vec<Attachment>,
    /// The server's image and text limits.
    limits: (usize, usize),
}

pub struct Desktop {
    pub servers: Vec<ServerView>,
    pub navigation: Navigation,
    pub history_enabled: bool,
    pub backend: Backend,
    pub profile: Profile,
    pub form: Option<ServerForm>,
    pub composer: Entity<TextareaState>,
    pub scroll: ScrollHandle,
    pub channel_scroll: ScrollHandle,
    chat_motion: SmoothScroll,
    channel_motion: SmoothScroll,
    /// The open dialog's scrolling (one dialog at a time), and which dialog
    /// this and the previous frame showed, so a newly opened one starts at the top.
    pub modal_scroll: ScrollHandle,
    modal_motion: SmoothScroll,
    pub modal_shown: std::cell::Cell<Option<&'static str>>,
    pub modal_seen: std::cell::Cell<Option<&'static str>>,
    scroll_frame_pending: bool,
    /// Keeps the timeline still while history is prepended above.
    pub anchor: Anchor,
    /// The timeline stays at its newest message; off once the reader scrolls up.
    pub following: bool,
    /// Loading older history failed: wait for the reader before trying again.
    pub older_paused: bool,
    pub error: Option<String>,
    pub title_drag: bool,
    /// Pointer x and sidebar width where a sidebar resize started, and the live width.
    pub sidebar_drag: Option<(f32, f32, f32)>,
    pub settings: ClientSettings,
    /// What the system configuration (managed.json) sets; read-only here.
    pub managed: ManagedSettings,
    pub settings_open: bool,
    /// Theme, fonts and zoom (see crate::appearance).
    pub appearance: crate::appearance::Appearance,
    /// The local audio check, alive while Settings is open.
    pub preview: Option<crate::preview::PreviewView>,
    /// Whether Nuschel starts at login, read from the system when Settings opens.
    pub autostart: Option<bool>,
    pub update: crate::update::UpdateState,
    /// Desktop notifications, and whether the window has focus.
    notes: tokio::sync::mpsc::Sender<crate::notify::Note>,
    pub window_active: bool,
    /// The image viewer, its request counter, and the image a context menu is for.
    pub viewer: Option<crate::viewer::Viewer>,
    pub viewer_requests: u64,
    pub menu_image: Option<crate::viewer::ViewerImage>,
    /// The emoji picker's search while it is open, recent picks, and its focus.
    pub emoji: Option<crate::emoji::Picker>,
    pub emoji_recent: Vec<&'static str>,
    pub emoji_focus: FocusHandle,
    /// The footer's mute and deafen animations (created on first render).
    pub mute_ease: Option<crate::motion::Ease>,
    /// Focus stays inside an open dialog (see `keep_dialog_focus`).
    pub modal_focus: FocusHandle,
    pub deafen_ease: Option<crate::motion::Ease>,
    pub menu: Option<crate::menus::Menu>,
    /// Message text: its selection, the message a menu is for, copy feedback.
    pub text: crate::selection::TextState,
    pub prompt: Option<crate::menus::Prompt>,
    /// File offers and downloads (transfer.rs).
    pub transfers: crate::transfer::Transfers,
    pub panel: Option<Panel>,
    pub ptt_active: bool,
    /// Push to mute is held: nothing is sent.
    pub push_muted: bool,
    pub shortcuts: crate::shortcuts::Shortcuts,
    /// Focused whenever nothing else is: keys reach the window's shortcuts
    /// (and Escape) only through a focused element.
    pub window_focus: FocusHandle,
    pub identity_status: Option<String>,
    pub search: Option<crate::search::SearchView>,
    /// The message a search result opened, highlighted for a moment.
    pub jumped: Option<crate::search::Jumped>,
    pub finder: crate::finder::Finder,
    pending_browse: Option<(String, u32)>,
    /// A rejected login waiting to reopen the server form for a retry.
    pending_retry: Option<(usize, RejectKind, String)>,
    /// A server's cached channels changed while applying events.
    cache_dirty: bool,
    /// A delayed write of the saved servers is due.
    servers_unsaved: bool,
    servers_generation: u64,
    server_writes: ServerWrites,
    sending: Option<PendingSend>,
    acknowledged: Vec<(Conversation, String)>,
    /// The clipboard image pasted last, and when it was last pasted.
    pasted_image: Option<(u64, std::time::Instant)>,
    passwords: BTreeMap<String, String>,
    drafts: BTreeMap<(String, u32), String>,
    /// What each conversation's composer has attached, and a line about it.
    attachments: BTreeMap<(String, u32), Vec<Attachment>>,
    pub attach_note: Option<((String, u32), String)>,
    attachment_ids: u64,
    /// Files being dragged over the conversation (see view::file_drop).
    pub dragged_files: std::rc::Rc<std::cell::RefCell<Option<Vec<std::path::PathBuf>>>>,
    /// Sent messages, in order, waiting for their images to be prepared.
    outgoing: std::collections::VecDeque<Outgoing>,
    _subscriptions: Vec<Subscription>,
    _receive: Task<()>,
    /// Wakes the shortcut poll when system-wide shortcuts are bound.
    ptt_wake: Arc<tokio::sync::Notify>,
    _ptt_poll: Task<()>,
}
impl Drop for Desktop {
    /// The latest saved servers reach disk even when a delayed write is
    /// pending, still running, or failed.
    fn drop(&mut self) {
        if (self.servers_unsaved || self.server_writes.written() < self.servers_generation)
            && let Err(error) = self.servers_snapshot()()
        {
            tracing::warn!(%error, "could not save the servers on exit");
        }
    }
}
impl Desktop {
    pub fn new(
        profile: Profile,
        loaded: Loaded,
        startup: Option<MumbleUrl>,
        handle: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            accessible(
                TextareaState::new(window, cx)
                    .placeholder("Write a message…")
                    .auto_grow(1, 10)
                    .submit_on_enter(true),
                "Message",
                true,
                false,
            )
        });
        let subscription =
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event, window, cx| match event {
                    // Shift+Enter has already inserted a newline.
                    InputEvent::PressEnter { shift: true, .. } => {}
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.send_message(window, cx),
                    InputEvent::PressEnter { .. } if this.settings.enter_sends => {
                        this.send_message(window, cx)
                    }
                    InputEvent::PressEnter { .. } => {
                        this.composer.update(cx, |input, cx| {
                            input.replace_text_in_range(None, "\n", window, cx)
                        });
                    }
                    _ => {}
                },
            );
        let Loaded {
            settings,
            managed,
            servers: bookmarks,
            problems,
        } = loaded;
        let activation = cx.observe_window_activation(window, |this: &mut Self, window, _| {
            this.window_active = window.is_window_active();
            if !this.window_active {
                // Its key releases go elsewhere now.
                for effect in this
                    .shortcuts
                    .held
                    .release(crate::shortcuts::Source::Window)
                {
                    this.apply_shortcut(effect);
                }
            }
        });
        let window_focus = cx.focus_handle();
        window.focus(&window_focus, cx);
        // A closed dialog or menu leaves nothing focused.
        let focus_lost = cx.on_focus_lost(window, |this: &mut Self, window, cx| {
            let target = window
                .focus_lost_restore_target(cx)
                .unwrap_or_else(|| this.window_focus.clone());
            window.focus(&target, cx);
        });
        let (backend, mut events) = Backend::new(handle.clone(), profile.clone(), settings.clone());
        // Events apply as they arrive, a bounded batch per update.
        let receive = cx.spawn(async move |this, cx| {
            let mut batch = Vec::new();
            while let Some(event) = events.recv().await {
                batch.push(event);
                while batch.len() < 256
                    && let Ok(event) = events.try_recv()
                {
                    batch.push(event);
                }
                let applied = this.update(cx, |this, cx| {
                    this.apply_events(batch.drain(..), cx);
                    cx.notify();
                });
                if applied.is_err() {
                    break;
                }
                // Let a frame render between the batches of a burst.
                tokio::task::yield_now().await;
            }
        });
        // System-wide shortcuts report through a queue that has to be polled.
        let ptt_wake = Arc::new(tokio::sync::Notify::new());
        let ptt_poll = cx.spawn({
            let wake = ptt_wake.clone();
            async move |this, cx| {
                loop {
                    match this.update(cx, |this, cx| this.poll_global_shortcuts(cx)) {
                        Ok(true) => {
                            cx.background_executor()
                                .timer(std::time::Duration::from_millis(20))
                                .await
                        }
                        Ok(false) => wake.notified().await,
                        Err(_) => break,
                    }
                }
            }
        });
        let mut desktop = Self {
            servers: bookmarks.into_iter().map(ServerView::new).collect(),
            navigation: Navigation::default(),
            history_enabled: true,
            notes: crate::notify::start(&handle),
            backend,
            profile,
            form: None,
            composer,
            scroll: ScrollHandle::new(),
            channel_scroll: ScrollHandle::new(),
            chat_motion: SmoothScroll::default(),
            channel_motion: SmoothScroll::default(),
            modal_scroll: ScrollHandle::new(),
            modal_motion: SmoothScroll::default(),
            modal_shown: Default::default(),
            modal_seen: Default::default(),
            scroll_frame_pending: false,
            anchor: Anchor::default(),
            following: true,
            older_paused: false,
            error: (!problems.is_empty()).then(|| problems.join("\n")),
            title_drag: false,
            sidebar_drag: None,
            settings,
            managed,
            settings_open: false,
            appearance: crate::appearance::Appearance::new(window, cx),
            preview: None,
            autostart: None,
            update: Default::default(),
            window_active: true,
            viewer: None,
            viewer_requests: 0,
            menu_image: None,
            emoji: None,
            emoji_recent: Vec::new(),
            emoji_focus: cx.focus_handle(),
            mute_ease: None,
            modal_focus: cx.focus_handle(),
            deafen_ease: None,
            menu: None,
            text: crate::selection::TextState::new(cx),
            prompt: None,
            transfers: Default::default(),
            panel: None,
            ptt_active: false,
            push_muted: false,
            shortcuts: Default::default(),
            window_focus,
            identity_status: None,
            search: None,
            jumped: None,
            finder: Default::default(),
            pending_browse: None,
            pending_retry: None,
            cache_dirty: false,
            servers_unsaved: false,
            servers_generation: 0,
            server_writes: ServerWrites::default(),
            sending: None,
            acknowledged: Vec::new(),
            pasted_image: None,
            passwords: BTreeMap::new(),
            drafts: BTreeMap::new(),
            attachments: BTreeMap::new(),
            attach_note: None,
            attachment_ids: 0,
            dragged_files: Default::default(),
            outgoing: std::collections::VecDeque::new(),
            _subscriptions: vec![subscription, activation, focus_lost],
            _receive: receive,
            ptt_wake,
            _ptt_poll: ptt_poll,
        };
        desktop.apply_appearance(window, cx);
        if !desktop.servers.is_empty() {
            desktop.browse_server(0, window, cx);
        }
        match startup {
            Some(url) => desktop.open_url(url, window, cx),
            None => {
                for index in 0..desktop.servers.len() {
                    if desktop.servers[index].bookmark.auto_connect {
                        desktop.connect_as(index, None, true);
                    }
                }
            }
        }
        desktop.sync_global_shortcuts();
        desktop.refresh_icons();
        if crate::update::supported() {
            crate::update::cleanup();
            desktop.backend.schedule_update_checks();
        }
        desktop
    }
    /// Check for an update now (Settings) rather than on the schedule.
    pub fn check_update_now(&mut self) {
        self.update = crate::update::UpdateState::Checking;
        self.backend.check_update();
    }
    pub fn install_update(&mut self) {
        if let crate::update::UpdateState::Available(update) = &self.update {
            self.backend.install_update(update.clone());
            self.update = crate::update::UpdateState::Installing { done: 0, total: 0 };
        }
    }
    /// Keep the system-wide shortcuts in step with the settings.
    pub fn sync_global_shortcuts(&mut self) {
        let wanted = crate::shortcuts::active(&self.settings);
        if let Some(released) = self.shortcuts.rebind(wanted, self.backend.runtime()) {
            for effect in released {
                self.apply_shortcut(effect);
            }
            self.ptt_wake.notify_one();
        }
    }
    /// Connect to a `mumble://` link, reusing a matching saved server.
    pub fn open_url(&mut self, url: MumbleUrl, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self.servers.iter().position(|server| {
            server.bookmark.host == url.host
                && server.bookmark.port == url.port
                && url
                    .username
                    .as_ref()
                    .is_none_or(|name| *name == server.bookmark.username)
        });
        let index = match existing {
            Some(index) => index,
            None => {
                let Some(username) = url.username.clone() else {
                    self.open_form(None, window, cx);
                    if let Some(form) = &self.form {
                        let host = url.host.clone();
                        let port = url.port.to_string();
                        form.host
                            .update(cx, |input, cx| input.set_value(host, window, cx));
                        form.port
                            .update(cx, |input, cx| input.set_value(port, window, cx));
                        form.username
                            .update(cx, |input, cx| input.focus(window, cx));
                    }
                    return;
                };
                let bookmark = match ServerBookmark::new("", &url.host, url.port, &username) {
                    Ok(bookmark) => bookmark,
                    Err(error) => {
                        self.error = Some(error);
                        return;
                    }
                };
                let mut bookmarks = self
                    .servers
                    .iter()
                    .map(|server| server.bookmark.clone())
                    .collect::<Vec<_>>();
                bookmarks.push(bookmark.clone());
                if let Err(error) = self.write_servers(&bookmarks) {
                    self.error = Some(error.to_string());
                    return;
                }
                self.servers.push(ServerView::new(bookmark));
                self.refresh_icons();
                self.servers.len() - 1
            }
        };
        self.servers[index].pending_channel = url.channel;
        self.browse_server(index, window, cx);
        self.connect(index, url.password);
    }
    pub fn selected_indices(&self) -> Option<(usize, usize)> {
        let selected = self.navigation.selection()?;
        let server = self.servers.iter().position(|s| s.id == selected.server)?;
        let channel = self.servers[server]
            .channels
            .iter()
            .position(|c| c.id == selected.channel)?;
        Some((server, channel))
    }
    pub fn browse_server(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(server) = self.servers.get(index) else {
            return;
        };
        if self
            .navigation
            .selection()
            .is_none_or(|selection| selection.server != server.id)
        {
            self.channel_motion.cancel();
            self.channel_scroll.set_offset(point(px(0.), px(0.)));
        }
        let channel = self
            .navigation
            .remembered_channel(&server.id)
            .filter(|id| server.channels.iter().any(|channel| channel.id == *id))
            .unwrap_or(0);
        self.browse_channel(index, channel, window, cx);
    }
    pub fn browse_channel(
        &mut self,
        index: usize,
        id: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(selected) = self.navigation.selection() {
            self.drafts.insert(
                (selected.server.clone(), selected.channel),
                self.composer.read(cx).value().to_string(),
            );
        }
        let Some(server) = self.servers.get(index) else {
            return;
        };
        if !server.channels.iter().any(|channel| channel.id == id) {
            return;
        }
        let draft = self
            .drafts
            .get(&(server.id.clone(), id))
            .cloned()
            .unwrap_or_default();
        self.composer
            .update(cx, |input, cx| input.set_value(draft, window, cx));
        let conversation = Conversation {
            server: server.id.clone(),
            channel: id,
        };
        self.browsed(&conversation);
        self.navigation
            .browse(conversation, HistorySource::LocalObserved);
        self.chat_motion.cancel();
        self.anchor.reset();
        self.following = true;
        self.older_paused = false;
        let channel = self.servers[index]
            .channels
            .iter_mut()
            .find(|channel| channel.id == id)
            .expect("checked above");
        if channel.loaded {
            channel.trim_oldest(MAX_TIMELINE);
            channel.new_messages = 0;
        } else {
            self.load_history(None);
        }
        self.scroll.scroll_to_bottom();
        let server = &self.servers[index];
        if server.status == Status::Connected && !server.permissions.contains_key(&id) {
            let _ = self
                .backend
                .command(&server.id, Command::RequestPermissions(id));
        }
    }
    pub fn toggle_message(&mut self, index: usize) {
        if let Some((server, channel)) = self.selected_indices()
            && let Some(message) = self.servers[server].channels[channel]
                .messages
                .get_mut(index)
        {
            message.expanded = !message.expanded;
        }
    }
    pub fn smooth_scroll(
        &mut self,
        channels: bool,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let consumed = if channels {
            self.channel_motion.wheel(
                &self.channel_scroll,
                event,
                window.line_height(),
                cx.reduce_motion(),
            )
        } else {
            self.chat_motion.shift(self.anchor.take_shift());
            self.chat_motion.wheel(
                &self.scroll,
                event,
                window.line_height(),
                cx.reduce_motion(),
            )
        };
        if consumed {
            cx.stop_propagation();
            cx.notify();
            self.schedule_scroll_frame(window, cx);
        }
    }
    pub fn smooth_scroll_modal(
        &mut self,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_motion.wheel(
            &self.modal_scroll,
            event,
            window.line_height(),
            cx.reduce_motion(),
        ) {
            cx.stop_propagation();
            cx.notify();
            self.schedule_scroll_frame(window, cx);
        }
    }
    fn schedule_scroll_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scroll_frame_pending
            || !(self.chat_motion.active()
                || self.channel_motion.active()
                || self.modal_motion.active())
        {
            return;
        }
        self.scroll_frame_pending = true;
        let entity = cx.entity().downgrade();
        window.on_next_frame(move |window, cx| {
            if let Some(entity) = entity.upgrade() {
                entity.update(cx, |this, cx| {
                    this.scroll_frame_pending = false;
                    this.chat_motion.shift(this.anchor.take_shift());
                    let changed = this.chat_motion.advance(&this.scroll)
                        | this.channel_motion.advance(&this.channel_scroll)
                        | this.modal_motion.advance(&this.modal_scroll);
                    if changed {
                        cx.notify();
                    }
                    this.schedule_scroll_frame(window, cx);
                });
            }
        });
    }
    pub fn open_form(
        &mut self,
        editing: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let server = editing.and_then(|index| self.servers.get(index));
        let name = server.map(|server| server.name.clone()).unwrap_or_default();
        let host = server
            .map(|server| server.bookmark.host.clone())
            .unwrap_or_default();
        let port = server
            .map(|server| server.bookmark.port)
            .unwrap_or(64738)
            .to_string();
        let username = server
            .map(|server| server.bookmark.username.clone())
            .unwrap_or_default();
        let auto_connect = server.is_none_or(|server| server.bookmark.auto_connect);
        let managed = server.is_some_and(|server| server.bookmark.managed);
        let last_host = host.clone();
        self.form = Some(ServerForm {
            last_host,
            _subscriptions: Vec::new(),
            auto_connect,
            managed,
            editing: server.map(|server| server.id.clone()),
            name: cx.new(|cx| {
                accessible(
                    InputState::new(window, cx)
                        .placeholder("Server name")
                        .default_value(name),
                    "Server name",
                    false,
                    false,
                )
            }),
            host: cx.new(|cx| {
                accessible(
                    InputState::new(window, cx)
                        .placeholder("Hostname or IP address")
                        .default_value(host),
                    "Server address",
                    false,
                    false,
                )
            }),
            port: cx.new(|cx| {
                accessible(
                    InputState::new(window, cx)
                        .placeholder("64738")
                        .default_value(port),
                    "Port",
                    false,
                    false,
                )
            }),
            username: cx.new(|cx| {
                accessible(
                    InputState::new(window, cx)
                        .placeholder("Your Mumble username")
                        .default_value(username),
                    "Username",
                    false,
                    false,
                )
            }),
            password: cx.new(|cx| {
                accessible(
                    InputState::new(window, cx)
                        .placeholder("Optional; kept only for this connection")
                        .masked(true),
                    "Server password",
                    false,
                    true,
                )
            }),
            error: None,
        });
        if let Some(form) = &mut self.form {
            // Enter in any field adds (and connects) or saves.
            let mut subscriptions: Vec<Subscription> = [
                &form.name,
                &form.host,
                &form.port,
                &form.username,
                &form.password,
            ]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, event, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.submit_form(window, cx);
                    }
                })
            })
            .collect();
            subscriptions.push(cx.subscribe_in(
                &form.host,
                window,
                |this, _, event, window, cx| {
                    if let InputEvent::Change = event {
                        this.follow_host(window, cx);
                    }
                },
            ));
            form._subscriptions = subscriptions;
            if managed {
                for input in [&form.name, &form.host, &form.port, &form.username] {
                    input.update(cx, |input, cx| input.set_disabled(true, cx));
                }
            }
            let first = if managed { &form.password } else { &form.host };
            first.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }
    /// The name shows the address while it is empty or still equal to the
    /// address it followed.
    fn follow_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        let host = form.host.read(cx).value().trim().to_string();
        let name = form.name.read(cx).value().to_string();
        if name.trim().is_empty() || name == form.last_host {
            let value = host.clone();
            form.name
                .update(cx, |input, cx| input.set_value(value, window, cx));
        }
        form.last_host = host;
    }
    pub fn submit_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        let result = form
            .port
            .read(cx)
            .value()
            .parse::<u16>()
            .map_err(|_| "Enter a port from 1 to 65535.".to_string())
            .and_then(|port| {
                ServerBookmark::new(
                    form.name.read(cx).value().as_ref(),
                    form.host.read(cx).value().as_ref(),
                    port,
                    form.username.read(cx).value().as_ref(),
                )
            });
        let mut bookmark = match result {
            Ok(bookmark) => bookmark,
            Err(error) => {
                if let Some(form) = &mut self.form {
                    form.error = Some(error);
                }
                cx.notify();
                return;
            }
        };
        let editing = form.editing.clone();
        let password = form.password.read(cx).value().to_string();
        bookmark.auto_connect = form.auto_connect;
        // A new or offline server connects on save; an online one reconnects
        // only when its address, name or password changed.
        let mut connect = editing.is_none();
        if let Some(previous) = editing
            .as_ref()
            .and_then(|id| self.servers.iter().find(|server| &server.id == id))
        {
            if previous.bookmark.managed {
                // Declared in managed.json: only the password changes.
                bookmark = previous.bookmark.clone();
            }
            bookmark.id = previous.id.clone();
            bookmark.tokens = previous.bookmark.tokens.clone();
            let same_address =
                bookmark.host == previous.bookmark.host && bookmark.port == previous.bookmark.port;
            if same_address {
                bookmark.certificate_sha256 = previous.bookmark.certificate_sha256.clone();
                bookmark.icon = previous.bookmark.icon.clone();
                bookmark.icon_checked_ms = previous.bookmark.icon_checked_ms;
                bookmark.last_channel = previous.bookmark.last_channel;
                bookmark.cached_channels = previous.bookmark.cached_channels.clone();
            }
            let online = previous.status != Status::Disconnected;
            let changed = !same_address
                || bookmark.username != previous.bookmark.username
                || !password.is_empty();
            connect = !online || changed;
            if !connect {
                // Keep the live view (users, history, state); only the saved details change.
                let index = self
                    .servers
                    .iter()
                    .position(|server| server.id == bookmark.id)
                    .unwrap_or_default();
                let server = &mut self.servers[index];
                server.name = bookmark.name.clone();
                server.bookmark = bookmark.clone();
                let bookmarks = self
                    .servers
                    .iter()
                    .map(|server| server.bookmark.clone())
                    .collect::<Vec<_>>();
                if let Err(error) = self.write_servers(&bookmarks) {
                    if let Some(form) = &mut self.form {
                        form.error = Some(error.to_string());
                    }
                    cx.notify();
                    return;
                }
                self.form = None;
                cx.notify();
                return;
            }
        }
        let mut bookmarks = self
            .servers
            .iter()
            .map(|server| server.bookmark.clone())
            .collect::<Vec<_>>();
        let index =
            if let Some(index) = bookmarks.iter().position(|server| server.id == bookmark.id) {
                bookmarks[index] = bookmark.clone();
                index
            } else {
                bookmarks.push(bookmark.clone());
                bookmarks.len() - 1
            };
        if let Err(error) = self.write_servers(&bookmarks) {
            if let Some(form) = &mut self.form {
                form.error = Some(error.to_string());
            }
            cx.notify();
            return;
        }
        if index < self.servers.len() {
            self.backend.disconnect(&bookmark.id);
            self.servers[index] = ServerView::new(bookmark);
        } else {
            self.servers.push(ServerView::new(bookmark));
        }
        self.refresh_icons();
        self.form = None;
        self.browse_server(index, window, cx);
        if connect {
            self.connect(
                index,
                if password.is_empty() {
                    None
                } else {
                    Some(password)
                },
            );
        }
        cx.notify();
    }
    pub fn connect(&mut self, index: usize, password: Option<String>) {
        self.connect_as(index, password, false);
    }
    /// Connect self-muted; automatic connections at startup are deafened too,
    /// so nobody hears a session that began without the user present.
    fn connect_as(&mut self, index: usize, password: Option<String>, deafened: bool) {
        let server = &mut self.servers[index];
        let password = password.or_else(|| self.passwords.get(&server.id).cloned());
        if let Some(password) = &password {
            self.passwords.insert(server.id.clone(), password.clone());
        }
        match self
            .backend
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
            }
            Err(error) => server.notice = Some(error),
        }
    }
    pub fn trust_certificate(&mut self, index: usize) {
        let Some(server) = self.servers.get_mut(index) else {
            return;
        };
        let Some(prompt) = server.certificate.take() else {
            return;
        };
        let old = server.bookmark.certificate_sha256.replace(prompt.sha256);
        if let Err(error) = self.write_servers(
            &self
                .servers
                .iter()
                .map(|server| server.bookmark.clone())
                .collect::<Vec<_>>(),
        ) {
            self.servers[index].bookmark.certificate_sha256 = old;
            self.error = Some(error.to_string());
            return;
        }
        self.connect(index, self.passwords.get(&self.servers[index].id).cloned());
    }
    pub fn disconnect(&mut self, index: usize) {
        let Some(server) = self.servers.get_mut(index) else {
            return;
        };
        self.backend.disconnect(&server.id);
        if server.status == Status::Connected && !server.deafened {
            self.backend.chime(nuschel_core::audio::Sound::Disconnected);
        }
        self.passwords.remove(&server.id);
        if let Some(lost) = lost_messages(server) {
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
    }
    pub fn remove_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self
            .form
            .as_ref()
            .filter(|form| !form.managed)
            .and_then(|form| form.editing.clone())
        else {
            return;
        };
        let bookmarks = self
            .servers
            .iter()
            .filter(|server| server.id != id)
            .map(|server| server.bookmark.clone())
            .collect::<Vec<_>>();
        if let Err(error) = self.write_servers(&bookmarks) {
            self.error = Some(error.to_string());
            return;
        }
        self.backend.disconnect(&id);
        self.passwords.remove(&id);
        self.servers.retain(|server| server.id != id);
        self.form = None;
        if !self.servers.is_empty() {
            self.browse_server(0, window, cx);
        }
        cx.notify();
    }
    pub fn join_selected(&mut self) {
        let Some((server, channel)) = self.selected_indices() else {
            return;
        };
        if let TextTarget::Channel(id) = self.servers[server].channels[channel].target
            && let Err(error) = self
                .backend
                .command(&self.servers[server].id, Command::Join(id))
        {
            self.servers[server].notice = Some(error);
        }
    }
    /// Change preferences, persist them and apply them to live connections.
    pub fn sidebar_width(&self) -> f32 {
        self.sidebar_drag
            .map_or(self.settings.sidebar_width, |(_, _, width)| width)
    }
    pub fn drag_sidebar(&mut self, x: f32) {
        if let Some((start_x, start_width, width)) = &mut self.sidebar_drag {
            let range = nuschel_session::config::SIDEBAR_WIDTH;
            *width = (*start_width + x - *start_x).clamp(*range.start(), *range.end());
        }
    }
    pub fn finish_sidebar_drag(&mut self) {
        if let Some((_, _, width)) = self.sidebar_drag.take() {
            self.update_settings(|settings| settings.sidebar_width = width);
        }
    }
    pub fn open_settings(&mut self) {
        self.settings_open = true;
        self.autostart = Some(crate::autostart::enabled());
        if self.preview.is_none() {
            self.preview = Some(Default::default());
            self.backend.start_preview();
        }
    }
    pub fn close_settings(&mut self) {
        self.settings_open = false;
        self.appearance.close_picker();
        self.shortcuts.recording = None;
        self.preview = None;
        self.backend.stop_preview();
    }
    /// Open or close the microphone for the Settings check, or toggle hearing yourself.
    pub fn set_preview(&mut self, capture: bool, loopback: bool) {
        if let Some(preview) = &mut self.preview {
            preview.capture = capture;
            preview.loopback = loopback && capture;
            preview.test_opened_capture = false;
            if !capture {
                preview.close_capture();
                // Stopping the microphone ends an echo test that needs it.
                if preview.echo_test == crate::preview::EchoTestState::Running {
                    preview.echo_test = crate::preview::EchoTestState::Idle;
                }
            }
            self.backend.preview(crate::preview::Command::Set {
                capture,
                loopback: loopback && capture,
            });
        }
    }
    /// Look for website icons that are missing (again after a week) or old
    /// (after a month), while the setting is on.
    pub fn refresh_icons(&self) {
        const DAY_MS: i64 = 24 * 60 * 60 * 1000;
        if !self.settings.server_icons {
            return;
        }
        let now = nuschel_session::connection::now_ms();
        for server in &self.servers {
            let bookmark = &server.bookmark;
            let wait = if bookmark.icon.is_some() { 30 } else { 7 } * DAY_MS;
            if now - bookmark.icon_checked_ms >= wait {
                self.backend
                    .fetch_icon(bookmark.id.clone(), bookmark.host.clone());
            }
        }
    }
    pub fn set_autostart(&mut self, enable: bool) {
        match crate::autostart::set(enable) {
            Ok(()) => self.autostart = Some(crate::autostart::enabled()),
            Err(error) => self.error = Some(format!("Could not change starting at login: {error}")),
        }
    }
    pub fn start_echo_test(&mut self) {
        if let Some(preview) = &mut self.preview {
            preview.test_opened_capture = !preview.capture;
            preview.capture = true;
            preview.echo_test = crate::preview::EchoTestState::Running;
            self.backend.preview(crate::preview::Command::EchoTest);
        }
    }
    pub fn stop_echo_test(&mut self) {
        if let Some(preview) = &mut self.preview {
            preview.echo_test = crate::preview::EchoTestState::Idle;
            if std::mem::take(&mut preview.test_opened_capture) {
                preview.close_capture();
            }
            self.backend.preview(crate::preview::Command::StopEchoTest);
        }
    }
    pub fn update_settings(&mut self, change: impl FnOnce(&mut ClientSettings)) {
        let mut settings = self.settings.clone();
        change(&mut settings);
        let settings = self.managed.apply(settings.sanitized());
        if settings == self.settings {
            return;
        }
        if let Err(error) = self.profile.save_settings(&settings, &self.managed) {
            self.error = Some(format!("Could not save settings: {error}"));
        }
        self.backend.apply_settings(settings.clone());
        self.settings = settings;
        self.sync_global_shortcuts();
    }
    pub fn set_push_to_talk(&mut self, active: bool) {
        if self.ptt_active != active {
            self.ptt_active = active;
            self.backend.set_push_to_talk(active);
        }
    }
    /// Returns true when the key was consumed by a shortcut or key recording.
    pub fn key_down(&mut self, keystroke: &Keystroke, repeat: bool) -> bool {
        if repeat {
            // Key repeat never acts, but a shortcut's key is not typed either.
            return !self
                .shortcuts
                .window_actions(&self.settings, keystroke, true)
                .is_empty();
        }
        if keystroke.key == "escape" && self.viewer.is_some() && self.menu.is_none() {
            self.viewer = None;
            return true;
        }
        if keystroke.key == "escape"
            && (self.form.is_some()
                || self.settings_open
                || self.menu.is_some()
                || self.prompt.is_some()
                || self.panel.is_some())
        {
            if self.menu.take().is_none()
                && self.prompt.take().is_none()
                && self.panel.take().is_none()
            {
                self.form = None;
                if self.settings_open {
                    self.close_settings();
                }
            }
            return true;
        }
        self.shortcut_key(keystroke, true)
    }
    pub fn key_up(&mut self, keystroke: &Keystroke) -> bool {
        self.shortcut_key(keystroke, false)
    }
    /// Modifier-only shortcuts (e.g. "control", from older settings) arrive
    /// as modifier changes.
    pub fn modifiers_changed(&mut self, modifiers: &Modifiers) {
        if self.shortcuts.recording.is_some() {
            return;
        }
        for effect in self.shortcuts.window_modifiers(&self.settings, modifiers) {
            self.apply_shortcut(effect);
        }
    }
    /// While a shortcut is being recorded, every key press is for it (Escape
    /// cancels), ahead of focused controls. True when the key was taken.
    pub fn record_key(&mut self, keystroke: &Keystroke) -> bool {
        let Some(action) = self.shortcuts.recording else {
            return false;
        };
        if keystroke.is_modifier_key() {
            return false;
        }
        self.shortcuts.recording = None;
        if keystroke.key != "escape" {
            self.bind_shortcut(action, crate::shortcuts::notation(keystroke));
        }
        true
    }
    /// A key does one thing: binding it to an action unbinds it elsewhere.
    pub fn bind_shortcut(&mut self, action: ShortcutAction, key: String) {
        self.update_settings(|settings| {
            settings.shortcuts.retain(|_, bound| *bound != key);
            // An in-app action whose default is this key loses it too.
            for other in ShortcutAction::ALL {
                if other != action
                    && crate::shortcuts::binding(settings, other) == Some(key.as_str())
                {
                    settings
                        .shortcuts
                        .insert(other, crate::shortcuts::UNSET.into());
                }
            }
            settings.shortcuts.insert(action, key);
        });
    }
    fn shortcut_key(&mut self, keystroke: &Keystroke, down: bool) -> bool {
        use crate::shortcuts::Source;
        let actions = self
            .shortcuts
            .window_actions(&self.settings, keystroke, down);
        for &action in &actions {
            if let Some(effect) = self.shortcuts.held.set(action, Source::Window, down) {
                self.apply_shortcut(effect);
            }
        }
        !actions.is_empty()
    }
    /// What a shortcut does matches the footer's buttons and push to talk.
    fn apply_shortcut(&mut self, effect: crate::shortcuts::Effect) {
        use crate::shortcuts::Effect;
        let connected = self
            .selected_indices()
            .is_some_and(|(server, _)| self.servers[server].status == Status::Connected);
        match effect {
            Effect::PushToTalk(active) => self.set_push_to_talk(active),
            Effect::PushToMute(active) => {
                self.push_muted = active;
                self.backend.set_push_to_mute(active);
            }
            Effect::ToggleMute if connected => self.set_mute(false),
            Effect::ToggleDeafen if connected => self.set_mute(true),
            Effect::ToggleMute | Effect::ToggleDeafen => {}
        }
    }
    pub fn set_mute(&mut self, deafen: bool) {
        let Some((index, _)) = self.selected_indices() else {
            return;
        };
        let server = &mut self.servers[index];
        if deafen {
            server.deafened = !server.deafened;
            if server.deafened {
                server.muted = true;
            }
        } else {
            server.muted = !server.muted;
            if !server.muted {
                server.deafened = false;
            }
        }
        if let Err(error) = self.backend.command(
            &server.id,
            Command::SetSelf {
                muted: server.muted,
                deafened: server.deafened,
            },
        ) {
            server.notice = Some(error);
        }
    }
    pub fn sending(&self) -> bool {
        self.sending.is_some()
    }
    pub fn apply_composer_acks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (conversation, body) in self.acknowledged.drain(..) {
            if self.navigation.selection() == Some(&conversation)
                && self.composer.read(cx).value().as_ref() == body
            {
                self.composer
                    .update(cx, |input, cx| input.set_value("", window, cx));
            }
            let key = (conversation.server, conversation.channel);
            if self.drafts.get(&key).is_some_and(|draft| draft == &body) {
                self.drafts.remove(&key);
            }
        }
    }
    pub fn send_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((server, channel)) = self.selected_indices() else {
            return;
        };
        // Text waits behind attachments still being prepared for this server.
        let server_id = &self.servers[server].id;
        if self.sending.is_some() || self.outgoing.iter().any(|batch| &batch.key.0 == server_id) {
            return;
        }
        if !self.attachments().is_empty() {
            return self.send_attachments(window, cx);
        }
        let body = self.composer.read(cx).value().to_string();
        if body.trim().is_empty() {
            return;
        }
        if body.len() > MAX_CONTENT_BYTES {
            self.error = Some("Message exceeds 256 KiB.".into());
            return;
        }
        let server = &mut self.servers[server];
        if server.status != Status::Connected {
            server.notice = Some("Connect before sending a message.".into());
            return;
        }
        let channel = &server.channels[channel];
        let expected_peer = channel.direct_peer.as_ref().map(|peer| peer.instance);
        let target = channel.target.clone();
        let pending = PendingSend {
            conversation: Conversation {
                server: server.id.clone(),
                channel: channel.id,
            },
            target: target.clone(),
            body: body.clone(),
        };
        match self.backend.command(
            &server.id,
            Command::SendText {
                target,
                body,
                expected_peer,
            },
        ) {
            Ok(()) => self.sending = Some(pending),
            Err(error) => server.notice = Some(error),
        }
        cx.notify();
    }
    /// What the open conversation's composer has attached.
    pub fn attachments(&self) -> &[Attachment] {
        self.navigation
            .selection()
            .and_then(|selected| {
                self.attachments
                    .get(&(selected.server.clone(), selected.channel))
            })
            .map_or(&[], Vec::as_slice)
    }
    /// What the open conversation's composer is waiting for, if anything.
    pub fn sending_status(&self) -> Option<String> {
        let (server, _) = self.selected_indices()?;
        let key = self.selected_key()?;
        let preparing = self
            .outgoing
            .iter()
            .filter(|batch| batch.key == key)
            .flat_map(|batch| &batch.attachments)
            .filter(|attachment| !attachment.ready())
            .count();
        match self.servers[server].outbox {
            Some((sent, total)) => Some(format!("Sending {} of {total}…", sent + 1)),
            None if preparing == 1 => Some("Preparing the image…".into()),
            None if preparing > 1 => Some(format!("Preparing {preparing} images…")),
            None => None,
        }
    }
    /// Ctrl+V with only an image on the clipboard attaches it. Holding the
    /// keys repeats the paste: the same image again within two seconds is ignored.
    pub fn paste_image(&mut self, id: u64, image: Vec<u8>, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        let last = self.pasted_image.replace((id, now));
        if last.is_some_and(|(last, at)| last == id && now - at < std::time::Duration::from_secs(2))
        {
            return;
        }
        let Some(key) = self.selected_key() else {
            return;
        };
        self.attach_note = None;
        let name =
            crate::attachments::pasted_name(self.attachments.get(&key).map_or(&[], Vec::as_slice));
        if let Err(note) = self.attach_image(&key, name, Source::Bytes(image), None) {
            self.attach_note = Some((key, note));
        }
        cx.notify();
    }
    /// Ask for files to attach.
    pub fn choose_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let paths = match paths.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(key) = this.selected_key() {
                            this.attach_note =
                                Some((key, format!("Could not open the file chooser: {error}")));
                        }
                        cx.notify();
                    });
                    return;
                }
            };
            let _ = this.update(cx, |this, cx| this.attach_files(paths, cx));
        })
        .detach();
    }
    /// Attach chosen or dropped files to the open conversation: images to
    /// send in messages, anything else to offer for transfer. They are looked
    /// at off the UI thread.
    pub fn attach_files(&mut self, paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        let Some(key) = self.selected_key() else {
            return;
        };
        let checked = cx.background_executor().spawn(async move {
            paths
                .into_iter()
                .map(|path| {
                    let checked = crate::attachments::check(&path);
                    (path, checked)
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let checked = checked.await;
            let _ = this.update(cx, |this, cx| {
                this.attach_note = None;
                let mut refused = Vec::new();
                let mut notes = Vec::new();
                for (path, checked) in checked {
                    let name = path.file_name().map_or_else(
                        || path.display().to_string(),
                        |name| name.to_string_lossy().into_owned(),
                    );
                    match checked {
                        // An image this server or the tray cannot take goes as a file.
                        Ok(Kind::Image { size }) => {
                            let source = Source::File(path.clone());
                            let file = Some((path, size));
                            if let Err(note) = this.attach_image(&key, name.clone(), source, file) {
                                notes.push(note);
                            }
                        }
                        Ok(Kind::File { size }) => this.attach_file(&key, name, path, size),
                        Err(refusal) => refused.push((name, refusal)),
                    }
                }
                notes.dedup();
                notes.extend(crate::attachments::refusal(&refused));
                if !notes.is_empty() {
                    this.attach_note = Some((key, notes.join(" ")));
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn selected_key(&self) -> Option<(String, u32)> {
        let selected = self.navigation.selection()?;
        Some((selected.server.clone(), selected.channel))
    }
    fn next_attachment(&mut self) -> u64 {
        self.attachment_ids += 1;
        self.attachment_ids
    }
    /// Add an image to a conversation's tray and have the worker prepare it
    /// for that server's image limit, keeping room for the text. One from a
    /// file goes as a file where it cannot go as an image.
    fn attach_image(
        &mut self,
        key: &(String, u32),
        name: String,
        source: Source,
        file: Option<(std::path::PathBuf, u64)>,
    ) -> Result<(), String> {
        let server = self.servers.iter().find(|server| server.id == key.0);
        let (allow_html, limit) = server
            .and_then(|server| server.limits.as_ref())
            .map_or((true, 131_072), |limits| {
                (limits.allow_html, limits.image_message_length)
            });
        let images = self.attachments.get(key).map_or(0, |tray| {
            tray.iter()
                .filter(|attachment| matches!(attachment, Attachment::Image { .. }))
                .count()
        });
        let refusal = if !allow_html {
            "This server does not accept images.".to_owned()
        } else if images >= crate::attachments::MAX_IMAGES {
            format!(
                "A message can carry up to {} images; send these first.",
                crate::attachments::MAX_IMAGES
            )
        } else {
            let id = self.next_attachment();
            self.attachments
                .entry(key.clone())
                .or_default()
                .push(Attachment::Image {
                    id,
                    name,
                    file,
                    prepared: None,
                });
            let limit = limit.min(MAX_CONTENT_BYTES);
            let budget = limit
                .saturating_sub(crate::attachments::CAPTION_ROOM)
                .max(limit / 2);
            self.backend.prepare_image(id, source, budget);
            return Ok(());
        };
        match file {
            Some((path, size)) => {
                self.attach_file(key, name, path, size);
                Ok(())
            }
            None => Err(refusal),
        }
    }
    fn attach_file(
        &mut self,
        key: &(String, u32),
        name: String,
        path: std::path::PathBuf,
        size: u64,
    ) {
        let id = self.next_attachment();
        self.attachments
            .entry(key.clone())
            .or_default()
            .push(Attachment::File {
                id,
                name,
                path,
                size,
            });
    }
    pub fn remove_attachment(&mut self, id: u64) {
        for tray in self.attachments.values_mut() {
            tray.retain(|attachment| attachment.id() != id);
        }
        self.attachments.retain(|_, tray| !tray.is_empty());
        self.attach_note = None;
    }
    /// Esc in an empty composer: the open conversation's attachments go.
    pub fn clear_attachments(&mut self) -> bool {
        let Some(key) = self.selected_key() else {
            return false;
        };
        self.attach_note = None;
        self.attachments.remove(&key).is_some()
    }
    /// The content worker prepared an attached image, or could not.
    fn prepared(
        &mut self,
        id: u64,
        result: Result<nuschel_ipc::EncodedImage, String>,
        cx: &mut Context<Self>,
    ) {
        // In a composer's tray, or in a sent message waiting for it.
        let trays = self.attachments.iter_mut();
        let batches = self
            .outgoing
            .iter_mut()
            .map(|batch| (&batch.key, &mut batch.attachments));
        let Some((key, attachment)) = trays.chain(batches).find_map(|(key, tray)| {
            let attachment = tray.iter_mut().find(|attachment| attachment.id() == id)?;
            Some((key.clone(), attachment))
        }) else {
            return;
        };
        let Attachment::Image {
            name,
            file,
            prepared,
            ..
        } = attachment
        else {
            return;
        };
        let thumbnail = result.and_then(|encoded| match encoded.thumbnail {
            nuschel_core::ContentBlock::Image {
                width,
                height,
                rgba,
                ..
            } => crate::document::render_image(width, height, &rgba)
                .map(|thumbnail| (encoded.html, thumbnail))
                .ok_or_else(|| "the thumbnail is invalid".to_owned()),
            _ => Err("the thumbnail is missing".to_owned()),
        });
        let note = match (thumbnail, file.take()) {
            (Ok(thumbnail), _) => {
                *prepared = Some(thumbnail);
                None
            }
            // A file the worker cannot encode is offered as a file instead.
            (Err(reason), Some((path, size))) => {
                let note = format!("{name} goes as a file: {reason}.");
                *attachment = Attachment::File {
                    id,
                    name: std::mem::take(name),
                    path,
                    size,
                };
                Some(note)
            }
            (Err(reason), None) => {
                let note = format!("Could not attach {name}: {reason}.");
                let id = attachment.id();
                for tray in self.attachments.values_mut() {
                    tray.retain(|attachment| attachment.id() != id);
                }
                for batch in &mut self.outgoing {
                    batch.attachments.retain(|attachment| attachment.id() != id);
                }
                Some(note)
            }
        };
        if let Some(note) = note {
            self.attach_note = Some((key, note));
        }
        self.attachments.retain(|_, tray| !tray.is_empty());
        self.flush_outgoing(cx);
    }
    /// Send the open conversation's text and attachments once its images are
    /// prepared: text first, then the images one per message (the text with
    /// the first), then the files offered for transfer.
    fn send_attachments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some((server, channel)), Some(key)) = (self.selected_indices(), self.selected_key())
        else {
            return;
        };
        let text = self.composer.read(cx).value().to_string();
        let server = &mut self.servers[server];
        let limits = server
            .limits
            .as_ref()
            .map_or((true, 131_072, 5000), |limits| {
                (
                    limits.allow_html,
                    limits.image_message_length,
                    limits.message_length,
                )
            });
        let images = self.attachments.get(&key).is_some_and(|tray| {
            tray.iter()
                .any(|attachment| matches!(attachment, Attachment::Image { .. }))
        });
        let text_length = nuschel_session::connection::legacy_body(&text, limits.0).len();
        let note = if server.status != Status::Connected {
            Some("Connect before sending.".to_owned())
        } else if images && !limits.0 {
            Some("This server does not accept images.".to_owned())
        } else if text_length > limits.2 || text.len() > MAX_CONTENT_BYTES {
            Some(format!(
                "The text exceeds this server's {}-byte limit.",
                limits.2
            ))
        } else {
            None
        };
        if note.is_some() {
            self.attach_note = note.map(|note| (key, note));
            cx.notify();
            return;
        }
        let channel = &server.channels[channel];
        self.outgoing.push_back(Outgoing {
            key: key.clone(),
            target: (
                channel.target.clone(),
                channel.direct_peer.as_ref().map(|peer| peer.instance),
            ),
            text,
            attachments: self.attachments.remove(&key).unwrap_or_default(),
            limits: (limits.1.min(MAX_CONTENT_BYTES), limits.2),
        });
        self.attach_note = None;
        self.drafts.remove(&key);
        self.composer
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.flush_outgoing(cx);
        cx.notify();
    }
    /// Hand sent messages whose images are all prepared to the connection,
    /// in order, then offer their files.
    fn flush_outgoing(&mut self, cx: &mut Context<Self>) {
        while self
            .outgoing
            .front()
            .is_some_and(|batch| batch.attachments.iter().all(Attachment::ready))
        {
            let Some(batch) = self.outgoing.pop_front() else {
                break;
            };
            let mut images = Vec::new();
            let mut files = Vec::new();
            for attachment in batch.attachments {
                match attachment {
                    Attachment::Image {
                        prepared: Some((html, _)),
                        ..
                    } => images.push(html),
                    Attachment::Image { .. } => {}
                    Attachment::File { path, .. } => files.push(path),
                }
            }
            let (target, expected_peer) = batch.target;
            let text = (!batch.text.trim().is_empty()).then_some(batch.text);
            let command = if images.is_empty() {
                text.map(|body| Command::SendText {
                    target,
                    body,
                    expected_peer,
                })
            } else {
                let (image_limit, message_limit) = batch.limits;
                let caption =
                    text.map(|text| nuschel_session::connection::legacy_body(&text, true));
                let html =
                    crate::attachments::messages(caption, images, image_limit, message_limit);
                Some(Command::SendHtml {
                    target,
                    html,
                    expected_peer,
                })
            };
            if let Some(command) = command
                && let Err(error) = self.backend.command(&batch.key.0, command)
            {
                self.attach_note = Some((batch.key, format!("Not sent: {error}")));
                continue;
            }
            if !files.is_empty() {
                self.offer_files(files, cx);
            }
        }
    }
    pub fn open_direct(
        &mut self,
        index: usize,
        session: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let server = &mut self.servers[index];
        if server.own_session == Some(session) {
            return;
        }
        let Some(peer) = server.users.get(&session).cloned() else {
            return;
        };
        let key = peer
            .certificate_hash
            .as_ref()
            .map(|hash| format!("direct:{hash}"))
            .unwrap_or_else(|| {
                format!(
                    "direct:pending:{}:{session}:{}",
                    server.generation, peer.instance
                )
            });
        let id = if let Some(channel) = server
            .channels
            .iter_mut()
            .find(|channel| channel.storage_key == key)
        {
            channel.target = TextTarget::Direct(session);
            channel.direct_peer = Some(peer);
            channel.id
        } else {
            let mut id = u32::MAX;
            while server.channels.iter().any(|channel| channel.id == id) {
                id -= 1;
            }
            server.channels.push(ChannelView {
                id,
                parent: None,
                name: format!("@ {}", crate::names::display_name(&peer, &self.settings)),
                topic: "Private conversation".into(),
                position: i32::MAX,
                target: TextTarget::Direct(session),
                direct_peer: Some(peer),
                storage_key: key,
                messages: Vec::new(),
                next_before: None,
                loading: false,
                new_messages: 0,
                ..Default::default()
            });
            id
        };
        self.browse_channel(index, id, window, cx);
        cx.notify();
    }
    pub fn load_history(&mut self, before: Option<i64>) {
        let Some((index, channel)) = self.selected_indices() else {
            return;
        };
        let Some(identity) = self.servers[index].history_identity() else {
            return;
        };
        let server = &mut self.servers[index];
        let channel = &mut server.channels[channel];
        channel.loading = true;
        if before.is_none() {
            channel.new_messages = 0;
        }
        let ticket = self.navigation.browse(
            Conversation {
                server: server.id.clone(),
                channel: channel.id,
            },
            HistorySource::LocalObserved,
        );
        if let Err(error) = self.backend.history(
            server.id.clone(),
            identity,
            channel.storage_key.clone(),
            ticket,
            before,
        ) {
            channel.loading = false;
            self.older_paused = true;
            self.error = Some(error);
        }
    }
    /// The page before the oldest loaded message, once the reader nears the
    /// top. Returns whether one was requested.
    pub fn load_older(&mut self) -> bool {
        if let Some((server, channel)) = self.selected_indices()
            && !self.older_paused
            && !self.servers[server].channels[channel].loading
            && let Some(before) = self.servers[server].channels[channel].next_before
        {
            self.load_history(Some(before));
            return true;
        }
        false
    }
    /// Before each frame: follow the newest message, or hold still where the
    /// reader scrolled to, judged by how the offset moved since the last frame.
    pub fn settle_timeline(&mut self) {
        if self.settle_detached() {
            return;
        }
        let Some((server, channel)) = self.selected_indices() else {
            return;
        };
        if let Some((painted, max)) = self.anchor.painted() {
            let now = self.scroll.offset().y;
            if now != painted {
                // The reader is scrolling again: a failed page may be retried.
                self.older_paused = false;
            }
            self.following = crate::scrolling::follows(self.following, painted, now, max);
        }
        if self.following {
            let channel = &mut self.servers[server].channels[channel];
            channel.new_messages = 0;
            // Out of sight above while at the bottom; they load again on scrolling up.
            channel.trim_oldest(MAX_TIMELINE);
            self.scroll.scroll_to_bottom();
        }
    }
    pub fn jump_to_latest(&mut self) {
        self.chat_motion.cancel();
        self.following = true;
        self.scroll.scroll_to_bottom();
        if let Some((server, channel)) = self.selected_indices() {
            self.servers[server].channels[channel].new_messages = 0;
        }
    }
    /// Apply system-wide shortcut events; false while none are wanted.
    fn poll_global_shortcuts(&mut self, cx: &mut Context<Self>) -> bool {
        while let Some(effects) = self.shortcuts.poll_global() {
            cx.notify();
            for effect in effects {
                self.apply_shortcut(effect);
            }
        }
        self.shortcuts.polling()
    }
    fn apply_events(&mut self, events: impl IntoIterator<Item = Event>, cx: &mut Context<Self>) {
        for event in events {
            match event {
                Event::Sent {
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
                        self.acknowledged.push((pending.conversation, pending.body));
                    }
                }
                Event::SearchResults { request, result } => {
                    self.apply_search_results(request, result, cx);
                }
                Event::HistoryWindow {
                    server,
                    ticket,
                    page,
                    messages,
                    next_before,
                    next_after,
                } => self.apply_history_window(
                    (server, ticket, page),
                    messages,
                    (next_before, next_after),
                    cx,
                ),
                Event::HistoryRecovered => {
                    self.older_paused = false;
                    if self
                        .error
                        .as_deref()
                        .is_some_and(|error| error.starts_with("Local history"))
                    {
                        self.error = None;
                    }
                }
                Event::HistoryError(error) => {
                    self.error = Some(format!("Local history: {error}"));
                    self.older_paused = true;
                    for server in &mut self.servers {
                        for channel in &mut server.channels {
                            channel.loading = false;
                        }
                    }
                }
                Event::History {
                    server,
                    ticket,
                    before,
                    messages,
                    next_before,
                } => {
                    if !self.navigation.accepts(&ticket) {
                        continue;
                    }
                    if let Some(server) = self.servers.iter_mut().find(|entry| entry.id == server)
                        && let Some(channel) = server
                            .channels
                            .iter_mut()
                            .find(|entry| entry.id == ticket.conversation.channel)
                    {
                        channel.loading = false;
                        self.older_paused = false;
                        tracing::debug!(?before, count = messages.len(), "history page");
                        channel.take_page(before, messages, next_before);
                        if before.is_none() {
                            // A conversation opens at its newest message.
                            self.anchor.reset();
                            self.following = true;
                            self.scroll.scroll_to_bottom();
                        }
                    }
                }
                Event::Protocol {
                    server,
                    generation,
                    event,
                } => {
                    let Some(index) = self
                        .servers
                        .iter()
                        .position(|entry| entry.id == server && entry.generation == generation)
                    else {
                        continue;
                    };
                    self.apply_protocol(index, event);
                }
                Event::Certificate {
                    server,
                    generation,
                    sha256,
                    changed,
                    reason,
                } => {
                    if let Some(server) = self
                        .servers
                        .iter_mut()
                        .find(|entry| entry.id == server && entry.generation == generation)
                    {
                        server.status = Status::Disconnected;
                        server.security = SessionSecurity::Unknown;
                        server.certificate = Some(CertificatePrompt {
                            sha256,
                            changed,
                            reason,
                        });
                    }
                }
                Event::Disconnected {
                    server,
                    generation,
                    reason,
                    reject,
                } => {
                    if let Some(index) = self
                        .servers
                        .iter()
                        .position(|entry| entry.id == server && entry.generation == generation)
                    {
                        let lost = lost_messages(&mut self.servers[index]);
                        self.disconnect(index);
                        self.servers[index].notice = Some(
                            lost.map_or_else(|| reason.clone(), |lost| format!("{reason} {lost}")),
                        );
                        if let Some(kind) =
                            reject.filter(|kind| kind.needs_password() || kind.needs_username())
                        {
                            self.pending_retry = Some((index, kind, reason));
                        }
                    }
                }
                Event::Reconnecting {
                    server,
                    generation,
                    attempt,
                    delay_secs,
                    reason,
                } => {
                    if let Some(server) = self
                        .servers
                        .iter_mut()
                        .find(|entry| entry.id == server && entry.generation == generation)
                    {
                        if server.status == Status::Connected && !server.deafened {
                            self.backend.chime(nuschel_core::audio::Sound::Disconnected);
                        }
                        server.status = Status::Connecting;
                        server.own_session = None;
                        if let Some(lost) = lost_messages(server) {
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
                Event::Gap {
                    server,
                    generation,
                    from_ms,
                    to_ms,
                } => {
                    if let Some(server) = self
                        .servers
                        .iter_mut()
                        .find(|entry| entry.id == server && entry.generation == generation)
                    {
                        server.reconnecting = None;
                        let text = gap_text(from_ms, to_ms);
                        for channel in &mut server.channels {
                            channel.log(text.clone(), to_ms, true);
                        }
                    }
                }
                Event::Rendered {
                    server,
                    generation,
                    key,
                    source,
                    document,
                } => {
                    if let Some(server) = self
                        .servers
                        .iter_mut()
                        .find(|entry| entry.id == server && entry.generation == generation)
                    {
                        match key {
                            RenderKey::Description(id) => {
                                if let Some(channel) = server.channel_view(id)
                                    && channel.description_raw == source
                                {
                                    channel.description = Some(document.into());
                                }
                            }
                            RenderKey::Comment(session) => {
                                if server.users.get(&session).is_some_and(|user| {
                                    user.comment.as_deref() == Some(source.as_str())
                                }) {
                                    server.comments.insert(session, (source, document.into()));
                                }
                            }
                        }
                    }
                }
                Event::Talking {
                    server,
                    generation,
                    session,
                    talking,
                } => {
                    if let Some(server) = self
                        .servers
                        .iter_mut()
                        .find(|entry| entry.id == server && entry.generation == generation)
                    {
                        if talking {
                            server.talking.insert(session);
                        } else {
                            server.talking.remove(&session);
                        }
                    }
                }
                Event::Prepared { id, result } => self.prepared(id, result, cx),
                Event::Avatar {
                    server,
                    generation,
                    hash,
                    document,
                } => {
                    if let Some(server) = self
                        .servers
                        .iter_mut()
                        .find(|entry| entry.id == server && entry.generation == generation)
                        && let Some(image) = Document::from(document).first_image()
                    {
                        // A server cycling texture hashes must not grow the cache
                        // without bound: past 1024, keep only avatars in use.
                        if server.avatars.len() >= 1024 {
                            let used: BTreeSet<_> = server
                                .users
                                .values()
                                .filter_map(|user| user.texture_hash.clone())
                                .collect();
                            server.avatars.retain(|hash, _| used.contains(hash));
                            server.avatar_requests.clear();
                        }
                        server.avatars.insert(hash, image);
                    }
                }
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
                } => {
                    if let Some(server) = self
                        .servers
                        .iter_mut()
                        .find(|entry| entry.id == server && entry.generation == generation)
                    {
                        server.audio.inputs = inputs;
                        server.audio.outputs = outputs;
                        server.audio.input = input;
                        server.audio.output = output;
                        server.audio.echo = echo;
                        server.audio.noise = noise;
                        server.audio.reference = reference;
                    }
                }
                Event::ServerIcon { server, icon } => {
                    if let Some(server) = self.servers.iter_mut().find(|entry| entry.id == server) {
                        server.bookmark.icon_checked_ms = nuschel_session::connection::now_ms();
                        if let Some(nuschel_core::ContentBlock::Image {
                            width,
                            height,
                            rgba,
                            ..
                        }) = icon
                        {
                            server.icon = crate::document::render_image(width, height, &rgba);
                            server.bookmark.icon = Some(nuschel_session::config::ServerIcon {
                                width,
                                height,
                                rgba,
                            });
                        }
                        self.cache_dirty = true;
                    }
                }
                Event::Viewed { request, document } => {
                    let image = document.and_then(|document| {
                        document.blocks.into_iter().find_map(|block| match block {
                            nuschel_core::ContentBlock::Image {
                                width,
                                height,
                                rgba,
                                ..
                            } => crate::document::render_image(width, height, &rgba)
                                .map(|image| (image, width, height)),
                            _ => None,
                        })
                    });
                    self.viewed(request, image);
                }
                Event::Update(event) => {
                    use crate::update::{UpdateEvent, UpdateState};
                    match event {
                        UpdateEvent::Checked(result) => {
                            // Installing or installed wins over a late check.
                            if !matches!(
                                self.update,
                                UpdateState::Installing { .. } | UpdateState::Installed
                            ) {
                                self.update = match result {
                                    Ok(Some(update)) => UpdateState::Available(update),
                                    Ok(None) => UpdateState::UpToDate,
                                    Err(error) => UpdateState::Failed(error),
                                };
                            }
                        }
                        UpdateEvent::Progress { done, total } => {
                            self.update = UpdateState::Installing { done, total };
                        }
                        UpdateEvent::Installed(Ok(())) => self.update = UpdateState::Installed,
                        UpdateEvent::Installed(Err(error)) => {
                            self.update = UpdateState::Failed(error)
                        }
                    }
                }
                Event::Preview(event) => {
                    if let Some(preview) = &mut self.preview {
                        preview.apply(event);
                    }
                }
                Event::Echo {
                    server,
                    generation,
                    erle_db,
                    residual_likelihood,
                } => {
                    if let Some(server) = self
                        .servers
                        .iter_mut()
                        .find(|entry| entry.id == server && entry.generation == generation)
                    {
                        server.audio.erle_db = erle_db;
                        server.audio.residual_echo = residual_likelihood;
                    }
                }
                Event::AudioHealth {
                    server,
                    generation,
                    health,
                } => {
                    if let Some(server) = self
                        .servers
                        .iter_mut()
                        .find(|entry| entry.id == server && entry.generation == generation)
                    {
                        server.audio.health = health;
                    }
                }
                Event::AudioStatus {
                    server,
                    generation,
                    message,
                } => {
                    if let Some(server) = self
                        .servers
                        .iter_mut()
                        .find(|entry| entry.id == server && entry.generation == generation)
                    {
                        server.audio_status = message;
                    }
                }
                Event::Message {
                    server,
                    generation,
                    target,
                    storage_key,
                    peer,
                    message,
                } => {
                    let Some(server) = self
                        .servers
                        .iter_mut()
                        .find(|entry| entry.id == server && entry.generation == generation)
                    else {
                        continue;
                    };
                    let index = if let Some(index) = server.channels.iter().position(|channel| {
                        channel.storage_key == storage_key
                            || (channel.target == target
                                && channel.storage_key.starts_with("direct:pending:")
                                && channel.direct_peer.as_ref().zip(peer.as_ref()).is_some_and(
                                    |(old, new)| {
                                        old.instance == new.instance && old.session == new.session
                                    },
                                ))
                    }) {
                        index
                    } else if let TextTarget::Direct(_) = target {
                        let mut id = u32::MAX;
                        while server.channels.iter().any(|channel| channel.id == id) {
                            id -= 1;
                        }
                        server.channels.push(ChannelView {
                            id,
                            parent: None,
                            name: format!(
                                "@ {}",
                                peer.as_ref()
                                    .map(|peer| crate::names::display_name(peer, &self.settings))
                                    .unwrap_or("Direct message")
                            ),
                            topic: "Private conversation".into(),
                            position: i32::MAX,
                            target: target.clone(),
                            direct_peer: peer.clone(),
                            storage_key: storage_key.clone(),
                            messages: Vec::new(),
                            next_before: None,
                            loading: false,
                            new_messages: 0,
                            ..Default::default()
                        });
                        server.channels.len() - 1
                    } else {
                        continue;
                    };
                    // A desktop notification for what arrives out of sight.
                    if !message.outgoing && self.settings.notifications {
                        let voice = server.voice_channel();
                        let channel = &server.channels[index];
                        let shown = self.window_active
                            && self.navigation.selection().is_some_and(|selected| {
                                selected.server == server.id && selected.channel == channel.id
                            });
                        let direct = matches!(target, TextTarget::Direct(_));
                        let in_voice =
                            matches!(channel.target, TextTarget::Channel(id) if Some(id) == voice);
                        let mut text = String::new();
                        let mut image = None;
                        for block in &message.document.blocks {
                            match block {
                                nuschel_core::ContentBlock::Text { text: part, .. } => {
                                    text.push_str(part)
                                }
                                nuschel_core::ContentBlock::Code { runs, .. } => {
                                    runs.iter().for_each(|run| text.push_str(&run.text))
                                }
                                nuschel_core::ContentBlock::Image {
                                    width,
                                    height,
                                    rgba,
                                    ..
                                } if image.is_none() => {
                                    image = crate::notify::thumbnail(*width, *height, rgba);
                                }
                                _ => {}
                            }
                        }
                        let mentioned = crate::notify::mentions(&text, &server.bookmark.username);
                        tracing::debug!(
                            shown,
                            direct,
                            in_voice,
                            mentioned,
                            "message notification check"
                        );
                        if !shown && (direct || in_voice || mentioned) {
                            let name = &message.message.author;
                            let author =
                                crate::names::author(name, server.users.values(), peer.as_ref());
                            let author =
                                crate::names::author_name(name, author, &self.settings).to_string();
                            let preview = crate::notify::preview(&text);
                            let _ = self.notes.try_send(crate::notify::Note {
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
                            });
                        }
                    }
                    if !message.outgoing {
                        let shown = self.window_active
                            && self.navigation.selection().is_some_and(|selected| {
                                selected.server == server.id
                                    && selected.channel == server.channels[index].id
                            });
                        let direct = matches!(target, TextTarget::Direct(_));
                        let mentioned = crate::notify::mentions(
                            &message.message.body,
                            &server.bookmark.username,
                        );
                        if let Some(sound) = crate::sounds::message(shown, direct, mentioned) {
                            self.backend.play_sound(&server.id, sound);
                        }
                    }
                    let channel = &mut server.channels[index];
                    if matches!(target, TextTarget::Direct(_)) {
                        channel.target = target;
                        channel.direct_peer = peer;
                        channel.storage_key = storage_key;
                    }
                    let outgoing = message.outgoing;
                    let mention = !outgoing
                        && (matches!(channel.target, TextTarget::Direct(_))
                            || message.document.blocks.iter().any(|block| {
                                matches!(block, nuschel_core::ContentBlock::Text { text, .. }
                                    if crate::notify::mentions(text, &server.bookmark.username))
                            }));
                    // Older history is shown apart from the newest: the
                    // message arrives with the newer pages.
                    let added = if channel.next_after.is_some() && message.id.is_some() {
                        channel.missed_live = true;
                        true
                    } else {
                        merge_message(channel, message)
                    };
                    let in_sight = self.window_active
                        && self.following
                        && self.navigation.selection().is_some_and(|selected| {
                            selected.server == server.id && selected.channel == channel.id
                        });
                    if added && !outgoing && !in_sight {
                        channel.unread += 1;
                        channel.unread_mention |= mention;
                    }
                    if self.navigation.selection().is_some_and(|selected| {
                        selected.server == server.id && selected.channel == channel.id
                    }) {
                        // Following keeps the newest in view (see settle_timeline);
                        // sending something of one's own jumps there too.
                        if outgoing {
                            self.chat_motion.cancel();
                            self.following = true;
                        } else if added && !self.following {
                            channel.new_messages += 1;
                        }
                    } else {
                        channel.trim_oldest(MAX_TIMELINE);
                    }
                }
            }
        }
        for server in &mut self.servers {
            if std::mem::take(&mut server.unsorted) {
                server.sort_channels();
            }
        }
        if std::mem::take(&mut self.cache_dirty) {
            self.save_servers_soon(cx);
        }
    }
    /// Write `servers` now, for an action the user waits on; a delayed write
    /// of an older state never lands after it.
    pub fn write_servers(&mut self, servers: &[ServerBookmark]) -> std::io::Result<()> {
        self.servers_generation += 1;
        self.server_writes
            .write(&self.profile, self.servers_generation, servers)
    }
    /// Save the servers shortly, off the UI thread: a burst of channel
    /// updates (joining a server) becomes one write.
    fn save_servers_soon(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.servers_unsaved, true) {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let Ok(Some(write)) = this.update(cx, |this, _| {
                this.servers_unsaved.then(|| this.servers_snapshot())
            }) else {
                return;
            };
            if let Err(error) = cx.background_spawn(async move { write() }).await {
                this.update(cx, |this, cx| {
                    this.error = Some(error.to_string());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }
    /// The saved servers as they are now, written by the returned closure on any thread.
    fn servers_snapshot(&mut self) -> impl FnOnce() -> std::io::Result<()> + Send + use<> {
        self.servers_unsaved = false;
        self.servers_generation += 1;
        let generation = self.servers_generation;
        let servers = self
            .servers
            .iter()
            .map(|server| server.bookmark.clone())
            .collect::<Vec<_>>();
        let (profile, writes) = (self.profile.clone(), self.server_writes.clone());
        move || writes.write(&profile, generation, &servers)
    }
    pub fn reconcile_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((index, kind, reason)) = self.pending_retry.take()
            && index < self.servers.len()
        {
            self.open_form(Some(index), window, cx);
            if let Some(form) = &mut self.form {
                form.error = Some(format!(
                    "{reason} {}",
                    if kind.needs_password() {
                        "Enter the password and try again."
                    } else {
                        "Choose another username."
                    }
                ));
                let field = if kind.needs_password() {
                    form.password.clone()
                } else {
                    form.username.clone()
                };
                field.update(cx, |input, cx| input.focus(window, cx));
            }
        }
        if let Some((server, channel)) = self.pending_browse.take()
            && let Some(index) = self.servers.iter().position(|entry| entry.id == server)
        {
            self.browse_channel(index, channel, window, cx);
        }
        if self.selected_indices().is_none() && !self.servers.is_empty() {
            let index = self
                .navigation
                .selection()
                .and_then(|selection| {
                    self.servers
                        .iter()
                        .position(|server| server.id == selection.server)
                })
                .unwrap_or(0);
            self.browse_server(index, window, cx);
        }
    }
    fn apply_protocol(&mut self, index: usize, event: SessionEvent) {
        let server = &mut self.servers[index];
        match event {
            SessionEvent::PeerCertificate(pin) => {
                let identity = server.history_identity();
                server.peer_sha256 = Some(pin.clone());
                server.security = SessionSecurity::TransportOnly;
                if server.bookmark.certificate_sha256.is_none() {
                    server.bookmark.certificate_sha256 = Some(pin);
                    if let Err(error) = self.write_servers(
                        &self
                            .servers
                            .iter()
                            .map(|server| server.bookmark.clone())
                            .collect::<Vec<_>>(),
                    ) {
                        self.error = Some(error.to_string());
                    }
                }
                // Reconnecting to the same server leaves the timeline (and where
                // the reader is in it) alone; a new identity has other history.
                let server = &mut self.servers[index];
                if server.history_identity() != identity {
                    for channel in &mut server.channels {
                        channel.loaded = false;
                    }
                    if self
                        .navigation
                        .selection()
                        .is_some_and(|selected| selected.server == server.id)
                    {
                        self.load_history(None);
                    }
                }
            }
            SessionEvent::Connected { session, .. } => {
                server.channels.retain(|channel| match channel.target {
                    TextTarget::Channel(id) => server.seen_channels.contains(&id),
                    TextTarget::Direct(_) => true,
                });
                let cached = server.bookmark.cached_channels.len();
                server
                    .bookmark
                    .cached_channels
                    .retain(|channel| server.seen_channels.contains(&channel.id));
                self.cache_dirty |= server.bookmark.cached_channels.len() != cached;
                server.status = Status::Connected;
                server.reconnecting = None;
                server.own_session = Some(session);
                server.expand_occupied();
                self.passwords.remove(&server.id);
                let path = std::mem::take(&mut server.pending_channel);
                if !path.is_empty() {
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
                        let _ = self.backend.command(&server.id, Command::Join(current));
                        self.pending_browse = Some((server.id.clone(), current));
                    } else {
                        server.notice = Some(format!("Channel /{} was not found.", path.join("/")));
                    }
                }
            }
            SessionEvent::Channel(update) => {
                server.seen_channels.insert(update.id);
                let previous = server
                    .bookmark
                    .cached_channels
                    .iter_mut()
                    .find(|channel| channel.id == update.id);
                // Most updates (descriptions, links, a resent tree) leave the cache as it is.
                if !previous.as_ref().is_some_and(|cached| {
                    cached.parent == update.parent
                        && cached.position == update.position
                        && cached.name == update.name
                }) {
                    let cached = CachedChannel {
                        id: update.id,
                        parent: update.parent,
                        name: update.name.clone(),
                        position: update.position,
                    };
                    match previous {
                        Some(previous) => *previous = cached,
                        None => server.bookmark.cached_channels.push(cached),
                    }
                    self.cache_dirty = true;
                }
                if let Some(channel) = server
                    .channels
                    .iter_mut()
                    .find(|channel| channel.target == TextTarget::Channel(update.id))
                {
                    channel.name = update.name.clone();
                    channel.parent = update.parent;
                    channel.position = update.position;
                    channel.topic = "Channel chat · select Join voice to move here".into();
                    channel.links = update.links.clone();
                    channel.temporary = update.temporary;
                    channel.max_users = update.max_users;
                    if channel.description_raw != update.description {
                        channel.description_raw = update.description.clone();
                        channel.description = None;
                        if !update.description.is_empty() {
                            self.backend.render_html(
                                server.id.clone(),
                                server.generation,
                                RenderKey::Description(update.id),
                                update.description.clone(),
                            );
                        }
                    }
                } else {
                    if !update.description.is_empty() {
                        self.backend.render_html(
                            server.id.clone(),
                            server.generation,
                            RenderKey::Description(update.id),
                            update.description.clone(),
                        );
                    }
                    server.channels.push(ChannelView {
                        id: update.id,
                        parent: update.parent,
                        name: update.name,
                        topic: "Channel chat · select Join voice to move here".into(),
                        position: update.position,
                        target: TextTarget::Channel(update.id),
                        direct_peer: None,
                        storage_key: update.id.to_string(),
                        messages: Vec::new(),
                        next_before: None,
                        loading: false,
                        new_messages: 0,
                        links: update.links,
                        temporary: update.temporary,
                        max_users: update.max_users,
                        description_raw: update.description,
                        ..Default::default()
                    });
                }
                // Sorted once the batch of events is applied, not per channel.
                server.unsorted = true;
            }
            SessionEvent::Permissions {
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
            SessionEvent::Activity(activity) => {
                let mut targets = activity.channels.clone();
                // A friend's arrival, departure or new name shows server-wide,
                // as in Mumble's log.
                let friend = activity.presence.is_some()
                    && crate::names::about_friend(&activity, &self.settings);
                if targets.is_empty() || friend {
                    targets.push(0);
                    targets.extend(server.voice_channel());
                }
                targets.sort_unstable();
                targets.dedup();
                let text = crate::names::activity_text(&activity, &self.settings);
                for id in targets {
                    if let Some(channel) = server.channel_view(id) {
                        channel.log(text.clone(), activity.observed_at_ms, false);
                    }
                }
                if friend
                    && activity.presence == Some(nuschel_session::model::Presence::Connected)
                    && self.settings.friend_notifications
                    && !self.window_active
                {
                    let _ = self.notes.try_send(crate::notify::Note {
                        key: format!("{}:friends", server.id),
                        summary: text,
                        body: server.name.clone(),
                        image: None,
                    });
                }
            }
            SessionEvent::Limits(limits) => server.limits = Some(limits),
            SessionEvent::Stats(stats) => {
                server.stats.insert(stats.session, stats);
            }
            SessionEvent::Suggest {
                push_to_talk,
                minimum_version,
                ..
            } => {
                let mut tips = Vec::new();
                if push_to_talk == Some(true)
                    && self.settings.audio.transmit != nuschel_core::audio::TransmitMode::PushToTalk
                {
                    tips.push("This server recommends push-to-talk.".to_string());
                }
                if let Some(version) = minimum_version {
                    tips.push(format!("This server recommends Mumble {version} or newer; some features may be missing."));
                }
                server.suggestion = (!tips.is_empty()).then(|| tips.join(" "));
            }
            SessionEvent::ContextActions(actions) => server.context_actions = actions,
            SessionEvent::RegisteredUsers(users) => server.registered_users = Some(users),
            SessionEvent::Bans(bans) => server.bans = Some(bans),
            SessionEvent::Acl(acl) => {
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
                        .backend
                        .command(&server.id, Command::QueryUserNames(unknown));
                }
                server.acl = Some(acl);
            }
            SessionEvent::UserNames(names) => server.user_names.extend(names),
            SessionEvent::Outbox { sent, total } => {
                server.outbox = (sent < total).then_some((sent, total));
            }
            SessionEvent::UnsupportedCodec { session } => {
                server.legacy_voice.insert(session);
                if let Some(user) = server.users.get(&session).cloned()
                    && let Some(channel) = server.channel_view(user.channel)
                {
                    channel.log(
                        format!(
                            "{} uses a legacy voice codec (CELT/Speex) that Nuschel cannot play.",
                            crate::names::display_name(&user, &self.settings)
                        ),
                        nuschel_session::connection::now_ms(),
                        true,
                    );
                }
            }
            SessionEvent::ChannelRemoved(id) => {
                server.expansion.remove(&id);
                server.permissions.remove(&id);
                let cached = server.bookmark.cached_channels.len();
                server
                    .bookmark
                    .cached_channels
                    .retain(|channel| channel.id != id);
                self.cache_dirty |= server.bookmark.cached_channels.len() != cached;
                server
                    .channels
                    .retain(|channel| channel.target != TextTarget::Channel(id));
                if server.channels.is_empty() {
                    server.channels.push(ChannelView::root());
                }
            }
            SessionEvent::User(user) => {
                if let Some(hash) = user.texture_hash.clone()
                    && !server.avatars.contains_key(&hash)
                {
                    if let Some(texture) = user.texture.clone() {
                        if server.avatar_requests.insert(format!("decode:{hash}")) {
                            self.backend.decode_avatar(
                                server.id.clone(),
                                server.generation,
                                hash,
                                texture,
                            );
                        }
                    } else if server.voice_channel() == Some(user.channel)
                        && server.avatar_requests.insert(hash)
                    {
                        let _ = self.backend.command(
                            &server.id,
                            Command::RequestBlobs {
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
                        self.backend.play_sound(&server.id, sound);
                    }
                }
                if Some(user.session) == server.own_session {
                    server.muted = user.self_muted;
                    server.deafened = user.self_deafened;
                    // Remembered for the next start (the root channel is where
                    // a server puts you anyway).
                    let last = (user.channel != 0).then_some(user.channel);
                    if server.bookmark.last_channel != last {
                        server.bookmark.last_channel = last;
                        self.cache_dirty = true;
                    }
                    self.navigation.set_voice(Some(Conversation {
                        server: server.id.clone(),
                        channel: user.channel,
                    }));
                }
                server.users.insert(user.session, user);
            }
            SessionEvent::UserRemoved(session) => {
                let from = server.users.get(&session).map(|old| old.channel);
                if server.status == Status::Connected
                    && Some(session) != server.own_session
                    && let Some(sound) = crate::sounds::movement(server.voice_channel(), from, None)
                {
                    self.backend.play_sound(&server.id, sound);
                }
                server.users.remove(&session);
                server.talking.remove(&session);
                server.legacy_voice.remove(&session);
            }
            SessionEvent::Notice(message) | SessionEvent::PermissionDenied(message) => {
                server.notice = Some(message);
                if self
                    .sending
                    .as_ref()
                    .is_some_and(|pending| pending.conversation.server == server.id)
                {
                    self.sending = None;
                }
            }
            SessionEvent::Welcome(text) => server.welcome = Some(text),
            SessionEvent::Transport(stats) => server.transport = Some(stats),
            SessionEvent::Text(_) | SessionEvent::Audio(_) => {}
        }
    }
}
/// Text still waiting for the rate limit when the connection ends is lost.
fn lost_messages(server: &mut ServerView) -> Option<String> {
    let (sent, total) = server.outbox.take()?;
    Some(match total - sent {
        1 => "1 message waiting to be sent was not sent.".into(),
        lost => format!("{lost} messages waiting to be sent were not sent."),
    })
}
/// Returns whether the message is new to this timeline.
pub(crate) fn merge_message(channel: &mut ChannelView, message: RenderedMessage) -> bool {
    if message
        .id
        .is_some_and(|id| channel.messages.iter().any(|entry| entry.id == Some(id)))
    {
        return false;
    }
    channel.messages.push(message.into());
    // Observation time first: an unsaved message still appears where it arrived.
    channel
        .messages
        .sort_by_key(|message| (message.observed_at_ms, message.id.unwrap_or(i64::MAX)));
    true
}
impl ChannelView {
    /// Fold a page of local history into the timeline. The latest page
    /// (`before` is None) starts it over, keeping only what arrived since;
    /// an older page is prepended if it continues from the oldest loaded
    /// message. Returns whether the page was used.
    fn take_page(
        &mut self,
        before: Option<i64>,
        messages: Vec<RenderedMessage>,
        next_before: Option<i64>,
    ) -> bool {
        match before {
            None => {
                self.loaded = true;
                self.next_after = None;
                self.missed_live = false;
                let newest = messages
                    .iter()
                    .filter_map(|message| message.id)
                    .max()
                    .unwrap_or(0);
                self.messages
                    .retain(|message| message.id.is_none_or(|id| id > newest));
            }
            // Trimmed while the page was on its way: the next request
            // starts from the new oldest message.
            Some(before) if self.next_before != Some(before) => return false,
            Some(_) => {}
        }
        for message in messages {
            merge_message(self, message);
        }
        self.next_before = next_before;
        self.beginning = next_before.is_none();
        true
    }
    /// Drop the oldest messages beyond `cap`; they load again from local history.
    fn trim_oldest(&mut self, cap: usize) {
        if self.messages.len() <= cap {
            return;
        }
        self.messages.drain(..self.messages.len() - cap);
        if let Some(oldest) = self.messages.iter().filter_map(|message| message.id).min() {
            self.next_before = Some(oldest);
            self.beginning = false;
        }
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that glob includes GPUI's `test` macro, which shadows `#[test]`.
    use super::{ChannelView, RenderedMessage, merge_message};

    fn message(id: Option<i64>, observed_at_ms: i64) -> RenderedMessage {
        RenderedMessage {
            id,
            message: nuschel_core::ChatMessage {
                server: "server".into(),
                channel: "2".into(),
                author: "Ada".into(),
                observed_at_ms,
                body: format!("message {id:?}"),
            },
            document: nuschel_core::MessageDocument { blocks: Vec::new() },
            outgoing: false,
        }
    }

    fn page(ids: std::ops::RangeInclusive<i64>) -> Vec<RenderedMessage> {
        ids.map(|id| message(Some(id), id * 1000)).collect()
    }

    fn ids(channel: &ChannelView) -> Vec<Option<i64>> {
        channel.messages.iter().map(|message| message.id).collect()
    }

    #[test]
    fn older_pages_prepend_to_one_timeline_until_the_beginning() {
        let mut channel = ChannelView::default();
        assert!(channel.take_page(None, page(81..=120), Some(81)));
        assert!(!channel.beginning);
        // A live message joins the same timeline; its echo from history does not repeat it.
        assert!(merge_message(&mut channel, message(Some(121), 121_000)));
        assert!(!merge_message(&mut channel, message(Some(121), 121_000)));
        assert!(channel.take_page(Some(81), page(41..=80), Some(41)));
        assert!(channel.take_page(Some(41), page(1..=40), None));
        assert!(channel.beginning);
        assert_eq!(channel.next_before, None);
        assert_eq!(ids(&channel), (1..=121).map(Some).collect::<Vec<_>>());
    }

    #[test]
    fn a_page_for_a_stale_cursor_is_dropped() {
        let mut channel = ChannelView::default();
        channel.take_page(None, page(81..=120), Some(81));
        channel.trim_oldest(20);
        assert_eq!(channel.next_before, Some(101));
        // Asked for before the trim: it no longer continues the timeline.
        assert!(!channel.take_page(Some(81), page(41..=80), Some(41)));
        assert_eq!(ids(&channel), (101..=120).map(Some).collect::<Vec<_>>());
        assert!(channel.take_page(Some(101), page(61..=100), Some(61)));
        assert_eq!(channel.messages.len(), 60);
    }

    #[test]
    fn the_latest_page_starts_over_but_keeps_what_arrived_since() {
        let mut channel = ChannelView::default();
        channel.take_page(None, page(81..=120), Some(81));
        channel.take_page(Some(81), page(41..=80), Some(41));
        merge_message(&mut channel, message(Some(121), 121_000));
        merge_message(&mut channel, message(None, 122_000));
        // Reopened: the latest page again, with the newer and unsaved messages.
        channel.take_page(None, page(82..=120), Some(82));
        assert_eq!(
            ids(&channel),
            (82..=121)
                .map(Some)
                .chain(std::iter::once(None))
                .collect::<Vec<_>>()
        );
        assert_eq!(channel.next_before, Some(82));
    }

    #[test]
    fn trimming_drops_only_the_oldest_and_rewinds_the_cursor() {
        let mut channel = ChannelView::default();
        channel.take_page(None, page(1..=40), None);
        assert!(channel.beginning);
        channel.trim_oldest(40);
        assert!(channel.beginning, "nothing beyond the cap, nothing dropped");
        channel.trim_oldest(30);
        assert_eq!(ids(&channel).first(), Some(&Some(11)));
        assert_eq!(channel.messages.len(), 30);
        assert_eq!(channel.next_before, Some(11));
        assert!(!channel.beginning);
    }
}

#[cfg(test)]
mod tree_tests {
    // Not `super::*`: that glob includes GPUI's `test` macro, which shadows `#[test]`.
    use super::{ChannelRow, ChannelView, ServerView, TreeRow};
    use nuschel_session::{
        config::ServerBookmark,
        connection::{TextTarget, User},
    };

    fn channel(id: u32, parent: Option<u32>, name: &str, position: i32) -> ChannelView {
        ChannelView {
            id,
            parent,
            name: name.into(),
            position,
            target: TextTarget::Channel(id),
            storage_key: id.to_string(),
            ..Default::default()
        }
    }

    fn user(session: u32, name: &str, channel: u32) -> User {
        User {
            session,
            name: name.into(),
            channel,
            ..Default::default()
        }
    }

    /// Root holds AFK, Lobby (Development; Gaming with Shooters and Strategy)
    /// and Music (Listening party), which its position sorts first. We are in
    /// Development; Margaret talks in Listening party; bob listens to
    /// Development from AFK.
    fn sample() -> ServerView {
        let mut server =
            ServerView::new(ServerBookmark::new("Test", "localhost", 64738, "andi").unwrap());
        server.channels = vec![
            channel(0, None, "Root", 0),
            channel(1, Some(0), "AFK", 0),
            channel(2, Some(0), "Lobby", 0),
            channel(3, Some(2), "Development", 0),
            channel(4, Some(2), "Gaming", 0),
            channel(5, Some(4), "Shooters", 0),
            channel(6, Some(4), "Strategy", 0),
            channel(7, Some(0), "Music", -1),
            channel(8, Some(7), "Listening party", 0),
            // Its parent is unknown: never shown.
            channel(9, Some(98), "Orphan", 0),
        ];
        server.sort_channels();
        // A direct conversation shares the list but is not part of the tree.
        server.channels.push(ChannelView {
            id: u32::MAX,
            name: "@ Zed".into(),
            position: i32::MAX,
            target: TextTarget::Direct(7),
            ..Default::default()
        });
        let mut bob = user(6, "bob", 1);
        bob.listening.insert(3);
        for user in [
            user(1, "andi", 3),
            user(2, "Ada Lovelace", 3),
            user(3, "grace", 3),
            user(4, "Linus", 5),
            user(5, "Margaret", 8),
            bob,
            user(7, "Zed", 0),
            user(8, "Lost", 99),
            user(9, "Orphaned", 9),
            user(10, "ada lovelace", 3),
        ] {
            server.users.insert(user.session, user);
        }
        server.own_session = Some(1);
        server.talking.insert(5);
        server
    }

    /// One line per row: `v`/`>` for an open/closed branch, `@` a person,
    /// `~` a listener, `[n]` people inside a closed branch.
    fn describe(server: &ServerView) -> Vec<String> {
        server
            .tree_rows()
            .into_iter()
            .map(|row| match row {
                TreeRow::Channel(ChannelRow {
                    index,
                    depth,
                    has_children,
                    expanded,
                    hidden_users,
                    hidden_talking,
                }) => {
                    let channel = &server.channels[index];
                    let branch = match (has_children, expanded) {
                        (false, _) => " ",
                        (true, true) => "v",
                        (true, false) => ">",
                    };
                    let hidden = match hidden_users {
                        0 => String::new(),
                        count if hidden_talking => format!(" [{count} talking]"),
                        count => format!(" [{count}]"),
                    };
                    format!("{}{branch} {}{hidden}", "  ".repeat(depth), channel.name)
                }
                TreeRow::User { session, depth } => {
                    format!("{}@{}", "  ".repeat(depth), server.users[&session].name)
                }
                TreeRow::Listener { session, depth } => {
                    format!("{}~{}", "  ".repeat(depth), server.users[&session].name)
                }
            })
            .collect()
    }

    #[test]
    fn roots_and_the_voice_branch_start_open() {
        assert_eq!(
            describe(&sample()),
            [
                "v Root",
                "  @Zed",
                "  > Music [1 talking]",
                "    AFK",
                "    @bob",
                "  v Lobby",
                "      Development",
                "      @Ada Lovelace",
                "      @ada lovelace",
                "      @andi",
                "      @grace",
                "      ~bob",
                "    > Gaming [1]",
            ]
        );
    }

    #[test]
    fn closed_branches_count_who_is_inside_and_keep_their_own_people() {
        let mut server = sample();
        server.set_expanded(4, true);
        assert_eq!(
            describe(&server)[12..],
            [
                "    v Gaming",
                "        Shooters",
                "        @Linus",
                "        Strategy",
            ]
        );
        server.talking.insert(4);
        server.set_expanded(2, false);
        assert_eq!(describe(&server)[5..], ["  > Lobby [5 talking]"]);
        server.set_expanded(0, false);
        assert_eq!(describe(&server), ["> Root [7 talking]", "  @Zed"]);
    }

    #[test]
    fn the_branch_follows_our_voice_channel_unless_chosen_by_hand() {
        let mut server = sample();
        server.users.get_mut(&1).unwrap().channel = 6;
        let rows = describe(&server);
        assert!(rows.contains(&"    v Gaming".to_string()), "{rows:#?}");
        assert!(rows.contains(&"        @andi".to_string()), "{rows:#?}");
        server.set_expanded(2, false);
        assert_eq!(describe(&server)[5..], ["  > Lobby [5]"]);
        server.own_session = None;
        server.expansion.clear();
        assert_eq!(describe(&server)[5..], ["  > Lobby [5]"]);
    }

    #[test]
    fn connecting_opens_every_branch_with_people() {
        let mut server = sample();
        server.set_expanded(2, false);
        server.expand_occupied();
        assert_eq!(
            describe(&server),
            [
                "v Root",
                "  @Zed",
                "  v Music",
                "      Listening party",
                "      @Margaret",
                "    AFK",
                "    @bob",
                "  v Lobby",
                "      Development",
                "      @Ada Lovelace",
                "      @ada lovelace",
                "      @andi",
                "      @grace",
                "      ~bob",
                "    v Gaming",
                "        Shooters",
                "        @Linus",
                "        Strategy",
            ]
        );
    }

    #[test]
    fn a_parent_cycle_or_duplicate_never_hangs() {
        let mut server = sample();
        server.channels.push(channel(20, Some(21), "Ping", 0));
        server.channels.push(channel(21, Some(20), "Pong", 0));
        server.channels.push(channel(22, Some(22), "Self", 0));
        server.users.insert(11, user(11, "Loop", 20));
        server.own_session = Some(11);
        server.expand_occupied();
        assert_eq!(describe(&server).len(), 18);
        // A second Lobby beneath one of Lobby's own descendants.
        server.channels.push(channel(2, Some(5), "Lobby again", 0));
        server.expand_occupied();
        describe(&server);
    }

    /// 2,000 channels eight to a parent and 5,000 people:
    /// `cargo test --release -p nuschel large_tree -- --ignored --nocapture`
    #[test]
    #[ignore = "timing; run in release"]
    fn large_tree_timing() {
        use std::{hint::black_box, time::Instant};
        let channels = || {
            (0..2000u32).map(|id| {
                let name = format!("Channel {}", id * 7919 % 2000);
                channel(
                    id,
                    id.checked_sub(1).map(|id| id / 8),
                    &name,
                    (id % 3) as i32,
                )
            })
        };
        let mut server =
            ServerView::new(ServerBookmark::new("Test", "localhost", 64738, "andi").unwrap());
        server.channels = channels().collect();
        server.sort_channels();
        for session in 0..5000u32 {
            let name = ["ada", "Bob", "carol", "Dave"][session as usize % 4];
            let mut user = user(session, &format!("{name} {session}"), session * 31 % 2000);
            if session % 50 == 0 {
                user.listening
                    .extend([session * 7 % 2000, session * 13 % 2000]);
            }
            server.users.insert(session, user);
        }
        server.own_session = Some(4999);
        server.talking.extend((0..5000).step_by(100));
        let rows = |label: &str, server: &ServerView| {
            let rounds = 20;
            let start = Instant::now();
            let mut rows = 0;
            for _ in 0..rounds {
                rows = black_box(server.tree_rows()).len();
            }
            println!(
                "tree_rows, {label}: {rows} rows, {:?}",
                start.elapsed() / rounds
            );
        };
        rows("default expansion", &server);
        let start = Instant::now();
        server.expand_occupied();
        println!("expand_occupied: {:?}", start.elapsed());
        rows("occupied branches open", &server);

        // Joining: every channel arrives, sorted after each or once at the end.
        let start = Instant::now();
        server.channels.clear();
        for channel in channels() {
            server.channels.push(channel);
            server.sort_channels();
        }
        println!("2,000 arrivals, sorting after each: {:?}", start.elapsed());
        let start = Instant::now();
        server.channels.clear();
        server.channels.extend(channels());
        server.sort_channels();
        println!("2,000 arrivals, sorting once: {:?}", start.elapsed());
    }
}

#[cfg(test)]
mod server_writes_tests {
    use super::ServerWrites;
    use nuschel_session::config::{Profile, ServerBookmark};

    #[test]
    fn an_older_snapshot_never_overwrites_a_newer_one() {
        let root = std::env::temp_dir().join(format!("nuschel-writes-{}", std::process::id()));
        let profile = Profile::open(Some(root.clone())).unwrap();
        let server = |name| ServerBookmark::new(name, "localhost", 64738, "andi").unwrap();
        let writes = ServerWrites::default();
        writes
            .write(&profile, 2, &[server("a"), server("b")])
            .unwrap();
        writes.write(&profile, 1, &[server("a")]).unwrap();
        assert_eq!(profile.load_servers(None).unwrap().len(), 2);
        assert_eq!(writes.written(), 2);
        writes.write(&profile, 3, &[]).unwrap();
        assert!(profile.load_servers(None).unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
