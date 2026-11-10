// SPDX-License-Identifier: EUPL-1.2
use crate::document::Document;
use crate::{
    backend::{Backend, Event, RenderKey, RenderedMessage},
    scrolling::SmoothScroll,
};
use gpui_kit::{
    base::input::{InputEvent, InputState, TextareaState},
    *,
};
use nuschel_client::{Conversation, HistorySource, Navigation, SessionSecurity};
use nuschel_core::MAX_CONTENT_BYTES;
use nuschel_session::{
    config::{CachedChannel, ClientSettings, MumbleUrl, Profile, ServerBookmark},
    connection::{Command, ServerLimits, SessionEvent, TextTarget, User},
    model::{Acl, Ban, ContextAction, RegisteredUser, RejectKind, UserStats},
    udp::TransportStats,
};
use std::collections::{BTreeMap, BTreeSet};

pub use crate::panels::Panel;

pub struct MessageView {
    pub id: Option<i64>,
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
impl MessageView {
    pub fn has_more(&self) -> bool {
        self.measured.get() > COLLAPSED_HEIGHT + 1.
    }
}
impl From<RenderedMessage> for MessageView {
    fn from(message: RenderedMessage) -> Self {
        Self {
            id: message.id,
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
            document: message.document.into(),
            expanded: false,
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
    Channel {
        index: usize,
        depth: usize,
        has_children: bool,
        /// Users hidden inside a collapsed branch, shown as a count.
        hidden_users: usize,
    },
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
pub struct ChannelView {
    pub id: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub topic: String,
    pub position: i32,
    pub target: TextTarget,
    pub direct_peer: Option<User>,
    pub storage_key: String,
    pub messages: Vec<MessageView>,
    pub next_before: Option<i64>,
    pub loading: bool,
    pub viewing_older: bool,
    pub new_messages: usize,
    /// Joins, moves, moderation and connection gaps; in memory only.
    pub activity: std::collections::VecDeque<ActivityLine>,
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
        });
        while self.activity.len() > MAX_ACTIVITY {
            self.activity.pop_front();
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
            loading: false,
            viewing_older: false,
            new_messages: 0,
            activity: Default::default(),
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
    pub level_db: f32,
    pub voice: f32,
    pub transmitting: bool,
    pub inputs: Vec<nuschel_core::audio::AudioDevice>,
    pub outputs: Vec<nuschel_core::audio::AudioDevice>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub echo: nuschel_core::audio::EchoSource,
    /// A noise suppressor upstream of the microphone (Nuschel's own is off).
    pub noise: Option<String>,
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
    pub registered_users: Option<Vec<RegisteredUser>>,
    pub bans: Option<Vec<Ban>>,
    pub acl: Option<Acl>,
    pub user_names: BTreeMap<u32, String>,
    pub reconnecting: Option<String>,
    /// Rendered user comments keyed by session, with the raw HTML they came from.
    pub comments: BTreeMap<u32, (String, Document)>,
    /// Decoded avatars by texture hash, and hashes already requested.
    pub avatars: BTreeMap<String, std::sync::Arc<RenderImage>>,
    pub avatar_requests: BTreeSet<String>,
    /// Active whisper/shout target label while one is selected.
    pub whisper: Option<String>,
    /// Channel path from a `mumble://` link, joined once the tree has loaded.
    pub pending_channel: Vec<String>,
}
impl ServerView {
    fn new(bookmark: ServerBookmark) -> Self {
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
                viewing_older: false,
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
            registered_users: None,
            bans: None,
            acl: None,
            user_names: BTreeMap::new(),
            reconnecting: None,
            comments: BTreeMap::new(),
            avatars: BTreeMap::new(),
            avatar_requests: BTreeSet::new(),
            whisper: None,
            pending_channel: Vec::new(),
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
}

pub struct SearchView {
    pub server: String,
    pub input: Entity<InputState>,
    pub query: String,
    pub results: Vec<SearchHit>,
    pub loading: bool,
    _changes: Subscription,
}
pub struct SearchHit {
    pub channel: u32,
    pub place: String,
    pub author: String,
    pub time: String,
    pub snippet: String,
}

pub struct ServerForm {
    pub editing: Option<String>,
    pub name: Entity<InputState>,
    pub host: Entity<InputState>,
    pub port: Entity<InputState>,
    pub username: Entity<InputState>,
    pub password: Entity<InputState>,
    pub error: Option<String>,
}

struct PendingSend {
    conversation: Conversation,
    target: TextTarget,
    body: String,
}

pub struct Desktop {
    pub servers: Vec<ServerView>,
    pub navigation: Navigation,
    expansion: BTreeMap<(String, u32), bool>,
    pub history_enabled: bool,
    pub backend: Backend,
    pub profile: Profile,
    pub form: Option<ServerForm>,
    pub composer: Entity<TextareaState>,
    pub scroll: ScrollHandle,
    pub channel_scroll: ScrollHandle,
    chat_motion: SmoothScroll,
    channel_motion: SmoothScroll,
    scroll_frame_pending: bool,
    pub error: Option<String>,
    pub title_drag: bool,
    pub settings: ClientSettings,
    pub settings_open: bool,
    /// The local audio check, alive while Settings is open.
    pub preview: Option<crate::preview::PreviewView>,
    pub menu: Option<crate::menus::Menu>,
    pub prompt: Option<crate::menus::Prompt>,
    pub panel: Option<Panel>,
    pub recording_ptt: bool,
    pub ptt_active: bool,
    pub identity_status: Option<String>,
    pub search: Option<SearchView>,
    global_ptt: Option<(String, crate::global_ptt::GlobalPtt)>,
    pub global_ptt_status: Option<String>,
    pending_browse: Option<(String, u32)>,
    /// A rejected login waiting to reopen the server form for a retry.
    pending_retry: Option<(usize, RejectKind, String)>,
    cache_dirty: bool,
    sending: Option<PendingSend>,
    acknowledged: Vec<(Conversation, String)>,
    passwords: BTreeMap<String, String>,
    drafts: BTreeMap<(String, u32), String>,
    _subscriptions: Vec<Subscription>,
    _poll: Task<()>,
}
impl Desktop {
    pub fn new(
        profile: Profile,
        bookmarks: Vec<ServerBookmark>,
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
        let settings = profile.load_settings().unwrap_or_default();
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(20))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.drain_events() {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut desktop = Self {
            servers: bookmarks.into_iter().map(ServerView::new).collect(),
            navigation: Navigation::default(),
            expansion: BTreeMap::new(),
            history_enabled: true,
            backend: Backend::new(handle, profile.clone(), settings.clone()),
            profile,
            form: None,
            composer,
            scroll: ScrollHandle::new(),
            channel_scroll: ScrollHandle::new(),
            chat_motion: SmoothScroll::default(),
            channel_motion: SmoothScroll::default(),
            scroll_frame_pending: false,
            error: None,
            title_drag: false,
            settings,
            settings_open: false,
            preview: None,
            menu: None,
            prompt: None,
            panel: None,
            recording_ptt: false,
            ptt_active: false,
            identity_status: None,
            search: None,
            global_ptt: None,
            global_ptt_status: None,
            pending_browse: None,
            pending_retry: None,
            cache_dirty: false,
            sending: None,
            acknowledged: Vec::new(),
            passwords: BTreeMap::new(),
            drafts: BTreeMap::new(),
            _subscriptions: vec![subscription],
            _poll: poll,
        };
        if !desktop.servers.is_empty() {
            desktop.browse_server(0, window, cx);
        }
        if let Some(url) = startup {
            desktop.open_url(url, window, cx);
        }
        desktop.sync_global_ptt();
        desktop
    }
    /// Keep the system-wide push-to-talk binding in step with the settings.
    pub fn sync_global_ptt(&mut self) {
        let wanted = (self.settings.audio.transmit
            == nuschel_core::audio::TransmitMode::PushToTalk)
            .then(|| self.settings.push_to_talk_key.clone())
            .flatten();
        match (&wanted, &self.global_ptt) {
            (Some(key), Some((bound, _))) if key == bound => {}
            (Some(key), _) => {
                self.global_ptt = Some((
                    key.clone(),
                    crate::global_ptt::GlobalPtt::start(self.backend.runtime(), key.clone()),
                ));
                self.global_ptt_status = Some("Requesting a system-wide shortcut…".into());
            }
            (None, _) => {
                self.global_ptt = None;
                self.global_ptt_status = None;
            }
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
                if let Err(error) = self.profile.save_servers(&bookmarks) {
                    self.error = Some(error.to_string());
                    return;
                }
                self.servers.push(ServerView::new(bookmark));
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
        self.navigation.browse(
            Conversation {
                server: server.id.clone(),
                channel: id,
            },
            HistorySource::LocalObserved,
        );
        self.chat_motion.cancel();
        self.load_history(None);
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
    pub fn channel_rows(&self, server_index: usize) -> Vec<(usize, usize, bool)> {
        let Some(server) = self.servers.get(server_index) else {
            return Vec::new();
        };
        let mut pending = server
            .channels
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, channel)| channel.parent.is_none())
            .map(|(index, _)| (index, 0))
            .collect::<Vec<_>>();
        let mut visited = BTreeSet::new();
        let mut rows = Vec::new();
        while let Some((index, depth)) = pending.pop() {
            let channel = &server.channels[index];
            if !visited.insert(channel.id) {
                continue;
            }
            let children = server
                .channels
                .iter()
                .enumerate()
                .filter(|(_, child)| child.parent == Some(channel.id))
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            rows.push((index, depth, !children.is_empty()));
            if self.channel_expanded(server_index, channel.id) {
                pending.extend(children.into_iter().rev().map(|index| (index, depth + 1)));
            }
        }
        rows
    }
    /// Channels in tree order with each expanded channel's users beneath it.
    pub fn tree_rows(&self, server_index: usize) -> Vec<TreeRow> {
        let Some(server) = self.servers.get(server_index) else {
            return Vec::new();
        };
        let mut users_by_channel = BTreeMap::<u32, Vec<&User>>::new();
        for user in server.users.values() {
            users_by_channel.entry(user.channel).or_default().push(user);
        }
        let mut rows = Vec::new();
        for (index, depth, has_children) in self.channel_rows(server_index) {
            let channel = &server.channels[index];
            let TextTarget::Channel(id) = channel.target else {
                continue;
            };
            let expanded = self.channel_expanded(server_index, id);
            let hidden_users = if has_children && !expanded {
                self.subtree(server_index, id)
                    .into_iter()
                    .filter(|child| *child != id)
                    .map(|child| users_by_channel.get(&child).map_or(0, Vec::len))
                    .sum()
            } else {
                0
            };
            rows.push(TreeRow::Channel {
                index,
                depth,
                has_children,
                hidden_users,
            });
            if let Some(users) = users_by_channel.get_mut(&id) {
                users.sort_by(|a, b| {
                    a.name
                        .to_lowercase()
                        .cmp(&b.name.to_lowercase())
                        .then(a.session.cmp(&b.session))
                });
                rows.extend(users.iter().map(|user| TreeRow::User {
                    session: user.session,
                    depth: depth + 1,
                }));
            }
            rows.extend(
                server
                    .users
                    .values()
                    .filter(|user| user.listening.contains(&id))
                    .map(|user| TreeRow::Listener {
                        session: user.session,
                        depth: depth + 1,
                    }),
            );
        }
        rows
    }
    fn subtree(&self, server_index: usize, root: u32) -> Vec<u32> {
        let server = &self.servers[server_index];
        let mut found = vec![root];
        let mut index = 0;
        while index < found.len() && found.len() <= server.channels.len() {
            let parent = found[index];
            found.extend(
                server
                    .channels
                    .iter()
                    .filter(|channel| channel.parent == Some(parent) && channel.id != parent)
                    .filter_map(|channel| match channel.target {
                        TextTarget::Channel(id) if !found.contains(&id) => Some(id),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            );
            index += 1;
        }
        found
    }
    /// Roots and the branch holding our own voice channel start expanded;
    /// explicit choices made during the session take precedence.
    pub fn channel_expanded(&self, server_index: usize, id: u32) -> bool {
        let server = &self.servers[server_index];
        self.expansion
            .get(&(server.id.clone(), id))
            .copied()
            .unwrap_or_else(|| {
                server
                    .channels
                    .iter()
                    .any(|channel| channel.id == id && channel.parent.is_none())
                    || self.holds_voice(server_index, id)
            })
    }
    fn holds_voice(&self, server_index: usize, id: u32) -> bool {
        let server = &self.servers[server_index];
        let Some(mut current) = server.voice_channel() else {
            return false;
        };
        for _ in 0..server.channels.len() {
            let Some(parent) = server
                .channels
                .iter()
                .find(|channel| channel.target == TextTarget::Channel(current))
                .and_then(|channel| channel.parent)
            else {
                return false;
            };
            if parent == id {
                return true;
            }
            current = parent;
        }
        false
    }
    pub fn toggle_channel(&mut self, server_index: usize, id: u32) {
        let expanded = self.channel_expanded(server_index, id);
        self.expansion
            .insert((self.servers[server_index].id.clone(), id), !expanded);
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
    fn schedule_scroll_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scroll_frame_pending || !(self.chat_motion.active() || self.channel_motion.active())
        {
            return;
        }
        self.scroll_frame_pending = true;
        let entity = cx.entity().downgrade();
        window.on_next_frame(move |window, cx| {
            if let Some(entity) = entity.upgrade() {
                entity.update(cx, |this, cx| {
                    this.scroll_frame_pending = false;
                    let changed = this.chat_motion.advance(&this.scroll)
                        | this.channel_motion.advance(&this.channel_scroll);
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
        self.form = Some(ServerForm {
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
        if let Some(form) = &self.form {
            form.host.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
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
        if let Some(previous) = editing
            .as_ref()
            .and_then(|id| self.servers.iter().find(|server| &server.id == id))
        {
            bookmark.id = previous.id.clone();
            if bookmark.host == previous.bookmark.host && bookmark.port == previous.bookmark.port {
                bookmark.certificate_sha256 = previous.bookmark.certificate_sha256.clone();
                bookmark.cached_channels = previous.bookmark.cached_channels.clone();
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
        if let Err(error) = self.profile.save_servers(&bookmarks) {
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
        self.form = None;
        self.browse_server(index, window, cx);
        self.connect(
            index,
            if password.is_empty() {
                None
            } else {
                Some(password)
            },
        );
        cx.notify();
    }
    fn connect(&mut self, index: usize, password: Option<String>) {
        let server = &mut self.servers[index];
        if let Some(password) = &password {
            self.passwords.insert(server.id.clone(), password.clone());
        }
        match self.backend.connect(server.bookmark.clone(), password) {
            Ok(generation) => {
                server.generation = generation;
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
        if let Err(error) = self.profile.save_servers(
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
        self.passwords.remove(&server.id);
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
        let Some(id) = self.form.as_ref().and_then(|form| form.editing.clone()) else {
            return;
        };
        let bookmarks = self
            .servers
            .iter()
            .filter(|server| server.id != id)
            .map(|server| server.bookmark.clone())
            .collect::<Vec<_>>();
        if let Err(error) = self.profile.save_servers(&bookmarks) {
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
    pub fn open_settings(&mut self) {
        self.settings_open = true;
        if self.preview.is_none() {
            self.preview = Some(Default::default());
            self.backend.start_preview();
        }
    }
    pub fn close_settings(&mut self) {
        self.settings_open = false;
        self.recording_ptt = false;
        self.preview = None;
        self.backend.stop_preview();
    }
    /// Open or close the microphone for the Settings check, or toggle hearing yourself.
    pub fn set_preview(&mut self, capture: bool, loopback: bool) {
        if let Some(preview) = &mut self.preview {
            preview.capture = capture;
            preview.loopback = loopback && capture;
            if !capture {
                preview.level_db = -100.;
                preview.voice = 0.;
                preview.transmitting = false;
            }
            self.backend.preview(crate::preview::Command::Set {
                capture,
                loopback: loopback && capture,
            });
        }
    }
    pub fn start_echo_test(&mut self) {
        if let Some(preview) = &mut self.preview {
            preview.capture = true;
            preview.echo_test = crate::preview::EchoTestState::Running;
            self.backend.preview(crate::preview::Command::EchoTest);
        }
    }
    pub fn update_settings(&mut self, change: impl FnOnce(&mut ClientSettings)) {
        let mut settings = self.settings.clone();
        change(&mut settings);
        let settings = settings.sanitized();
        if settings == self.settings {
            return;
        }
        if let Err(error) = self.profile.save_settings(&settings) {
            self.error = Some(format!("Could not save settings: {error}"));
        }
        self.backend.apply_settings(settings.clone());
        self.settings = settings;
        self.sync_global_ptt();
    }
    fn ptt_key_matches(&self, key: &str) -> bool {
        self.settings
            .push_to_talk_key
            .as_deref()
            .is_some_and(|wanted| wanted.eq_ignore_ascii_case(key))
    }
    pub fn set_push_to_talk(&mut self, active: bool) {
        if self.ptt_active != active {
            self.ptt_active = active;
            self.backend.set_push_to_talk(active);
        }
    }
    /// Returns true when the key was consumed by push-to-talk or key recording.
    pub fn key_down(&mut self, keystroke: &Keystroke) -> bool {
        if keystroke.key == "escape"
            && !self.recording_ptt
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
        if self.recording_ptt {
            if keystroke.key != "escape" {
                let key = keystroke.key.clone();
                self.update_settings(|settings| settings.push_to_talk_key = Some(key));
            }
            self.recording_ptt = false;
            return true;
        }
        if self.ptt_key_matches(&keystroke.key) {
            self.set_push_to_talk(true);
            return true;
        }
        false
    }
    pub fn key_up(&mut self, keystroke: &Keystroke) -> bool {
        if self.ptt_key_matches(&keystroke.key) {
            self.set_push_to_talk(false);
            return true;
        }
        false
    }
    /// Modifier-only push-to-talk keys (e.g. "control") arrive as modifier changes.
    pub fn modifiers_changed(&mut self, modifiers: &Modifiers) {
        let Some(key) = self.settings.push_to_talk_key.as_deref() else {
            return;
        };
        let held = match key {
            "control" | "ctrl" => modifiers.control,
            "alt" => modifiers.alt,
            "shift" => modifiers.shift,
            "platform" | "super" | "cmd" => modifiers.platform,
            _ => return,
        };
        if self.recording_ptt {
            return;
        }
        self.set_push_to_talk(held);
    }
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((index, _)) = self.selected_indices() else {
            return;
        };
        let input = cx.new(|cx| {
            accessible(
                InputState::new(window, cx).placeholder("Search local history"),
                "Search local history",
                false,
                false,
            )
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        // Search as you type; replies for older queries are dropped on arrival.
        let changes = cx.subscribe(&input, |this, _, event, cx| {
            if matches!(event, InputEvent::Change | InputEvent::PressEnter { .. }) {
                this.run_search(cx);
                cx.notify();
            }
        });
        self.search = Some(SearchView {
            server: self.servers[index].id.clone(),
            input,
            query: String::new(),
            results: Vec::new(),
            loading: false,
            _changes: changes,
        });
    }
    pub fn run_search(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let query = search.input.read(cx).value().trim().to_string();
        let Some(server) = self
            .servers
            .iter()
            .find(|server| server.id == search.server)
        else {
            return;
        };
        let Some(identity) = server.history_identity() else {
            return;
        };
        if query == search.query {
            return;
        }
        if query.is_empty() {
            search.query.clear();
            search.results.clear();
            search.loading = false;
            return;
        }
        search.query = query.clone();
        search.loading = true;
        if let Err(error) = self.backend.search(server.id.clone(), identity, query) {
            self.error = Some(error);
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
    pub fn send_message(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.sending.is_some() {
            return;
        }
        let Some((server, channel)) = self.selected_indices() else {
            return;
        };
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
                name: format!("@ {}", peer.name),
                topic: "Private conversation".into(),
                position: i32::MAX,
                target: TextTarget::Direct(session),
                direct_peer: Some(peer),
                storage_key: key,
                messages: Vec::new(),
                next_before: None,
                loading: false,
                viewing_older: false,
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
        channel.viewing_older = before.is_some();
        channel.new_messages = 0;
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
            self.error = Some(error);
        }
    }
    pub fn load_older(&mut self) {
        if let Some((server, channel)) = self.selected_indices()
            && !self.servers[server].channels[channel].loading
            && let Some(before) = self.servers[server].channels[channel].next_before
        {
            self.load_history(Some(before));
        }
    }
    fn drain_events(&mut self) -> bool {
        let mut changed = false;
        while let Some(event) = self
            .global_ptt
            .as_mut()
            .and_then(|(_, global)| global.try_next())
        {
            changed = true;
            match event {
                crate::global_ptt::GlobalEvent::Pressed => self.set_push_to_talk(true),
                crate::global_ptt::GlobalEvent::Released => self.set_push_to_talk(false),
                crate::global_ptt::GlobalEvent::Bound(trigger) => {
                    self.global_ptt_status =
                        Some(format!("System-wide shortcut active: {trigger}"));
                }
                crate::global_ptt::GlobalEvent::Unavailable(reason) => {
                    self.global_ptt_status =
                        Some(format!("Only while Nuschel is focused: {reason}"));
                }
            }
        }
        for _ in 0..256 {
            let Ok(event) = self.backend.events.try_recv() else {
                break;
            };
            changed = true;
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
                Event::SearchResults {
                    server,
                    query,
                    results,
                } => {
                    let Some(search) = self
                        .search
                        .as_mut()
                        .filter(|search| search.server == server && search.query == query)
                    else {
                        continue;
                    };
                    let Some(view) = self.servers.iter().find(|entry| entry.id == server) else {
                        continue;
                    };
                    search.loading = false;
                    search.results = results
                        .into_iter()
                        .map(|entry| {
                            let channel = view
                                .channels
                                .iter()
                                .find(|channel| channel.storage_key == entry.message.channel);
                            let visible: String = entry
                                .message
                                .body
                                .split(nuschel_core::EMBED)
                                .step_by(2)
                                .collect::<String>()
                                .split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ");
                            let mut snippet: String = visible.chars().take(240).collect();
                            if visible.chars().count() > 240 {
                                snippet.push('…');
                            }
                            SearchHit {
                                channel: channel.map(|channel| channel.id).unwrap_or(u32::MAX),
                                place: channel
                                    .map(|channel| channel.name.clone())
                                    .unwrap_or_else(|| "Earlier conversation".into()),
                                author: entry.message.author,
                                time: local_time(entry.message.observed_at_ms),
                                snippet,
                            }
                        })
                        .collect();
                }
                Event::HistoryRecovered => {
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
                    for server in &mut self.servers {
                        for channel in &mut server.channels {
                            channel.loading = false;
                        }
                    }
                }
                Event::History {
                    server,
                    ticket,
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
                        channel.next_before = next_before;
                        if !channel.viewing_older {
                            let newest = messages
                                .iter()
                                .filter_map(|message| message.id)
                                .max()
                                .unwrap_or(0);
                            channel
                                .messages
                                .retain(|message| message.id.is_none_or(|id| id > newest));
                        }
                        for message in messages {
                            merge_message(channel, message);
                        }
                        if channel.messages.len() > 500 {
                            channel.messages.truncate(500);
                        }
                        if !channel.viewing_older {
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
                        self.disconnect(index);
                        self.servers[index].notice = Some(reason.clone());
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
                        server.status = Status::Connecting;
                        server.own_session = None;
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
                        server.avatars.insert(hash, image);
                    }
                }
                Event::AudioLevel {
                    server,
                    generation,
                    level_db,
                    voice,
                    transmitting,
                } => {
                    if let Some(server) = self
                        .servers
                        .iter_mut()
                        .find(|entry| entry.id == server && entry.generation == generation)
                    {
                        server.audio.level_db = level_db;
                        server.audio.voice = voice;
                        server.audio.transmitting = transmitting;
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
                                    .map(|peer| peer.name.clone())
                                    .unwrap_or_else(|| "Direct message".into())
                            ),
                            topic: "Private conversation".into(),
                            position: i32::MAX,
                            target: target.clone(),
                            direct_peer: peer.clone(),
                            storage_key: storage_key.clone(),
                            messages: Vec::new(),
                            next_before: None,
                            loading: false,
                            viewing_older: false,
                            new_messages: 0,
                            ..Default::default()
                        });
                        server.channels.len() - 1
                    } else {
                        continue;
                    };
                    let channel = &mut server.channels[index];
                    if matches!(target, TextTarget::Direct(_)) {
                        channel.target = target;
                        channel.direct_peer = peer;
                        channel.storage_key = storage_key;
                    }
                    if channel.viewing_older {
                        channel.new_messages += 1;
                    } else {
                        let at_bottom = !self.chat_motion.active()
                            && self.scroll.max_offset().y + self.scroll.offset().y <= px(32.);
                        merge_message(channel, message);
                        if at_bottom
                            && self.navigation.selection().is_some_and(|selected| {
                                selected.server == server.id && selected.channel == channel.id
                            })
                        {
                            self.scroll.scroll_to_bottom();
                        }
                        if channel.messages.len() > 500 {
                            channel.messages.drain(..channel.messages.len() - 500);
                        }
                    }
                }
            }
        }
        if self.cache_dirty {
            self.cache_dirty = false;
            if let Err(error) = self.profile.save_servers(
                &self
                    .servers
                    .iter()
                    .map(|server| server.bookmark.clone())
                    .collect::<Vec<_>>(),
            ) {
                self.error = Some(error.to_string());
            }
        }
        changed
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
                server.peer_sha256 = Some(pin.clone());
                server.security = SessionSecurity::TransportOnly;
                if server.bookmark.certificate_sha256.is_none() {
                    server.bookmark.certificate_sha256 = Some(pin);
                    if let Err(error) = self.profile.save_servers(
                        &self
                            .servers
                            .iter()
                            .map(|server| server.bookmark.clone())
                            .collect::<Vec<_>>(),
                    ) {
                        self.error = Some(error.to_string());
                    }
                }
                self.load_history(None);
            }
            SessionEvent::Connected { session, .. } => {
                server.channels.retain(|channel| match channel.target {
                    TextTarget::Channel(id) => server.seen_channels.contains(&id),
                    TextTarget::Direct(_) => true,
                });
                server
                    .bookmark
                    .cached_channels
                    .retain(|channel| server.seen_channels.contains(&channel.id));
                self.cache_dirty = true;
                server.status = Status::Connected;
                server.reconnecting = None;
                server.own_session = Some(session);
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
                let cached = CachedChannel {
                    id: update.id,
                    parent: update.parent,
                    name: update.name.clone(),
                    position: update.position,
                };
                if let Some(previous) = server
                    .bookmark
                    .cached_channels
                    .iter_mut()
                    .find(|channel| channel.id == update.id)
                {
                    *previous = cached;
                } else {
                    server.bookmark.cached_channels.push(cached);
                }
                self.cache_dirty = true;
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
                        viewing_older: false,
                        new_messages: 0,
                        links: update.links,
                        temporary: update.temporary,
                        max_users: update.max_users,
                        description_raw: update.description,
                        ..Default::default()
                    });
                }
                server.channels.sort_by(|a, b| {
                    a.position
                        .cmp(&b.position)
                        .then_with(|| a.name.cmp(&b.name))
                });
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
                if targets.is_empty() {
                    targets.push(0);
                    targets.extend(server.voice_channel());
                }
                targets.sort_unstable();
                targets.dedup();
                for id in targets {
                    if let Some(channel) = server.channel_view(id) {
                        channel.log(activity.text.clone(), activity.observed_at_ms, false);
                    }
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
            SessionEvent::UnsupportedCodec { session } => {
                if let Some(user) = server.users.get(&session).cloned()
                    && let Some(channel) = server.channel_view(user.channel)
                {
                    channel.log(
                        format!(
                            "{} uses a legacy voice codec (CELT/Speex) that Nuschel cannot play.",
                            user.name
                        ),
                        nuschel_session::connection::now_ms(),
                        true,
                    );
                }
            }
            SessionEvent::ChannelRemoved(id) => {
                self.expansion.remove(&(server.id.clone(), id));
                server
                    .bookmark
                    .cached_channels
                    .retain(|channel| channel.id != id);
                self.cache_dirty = true;
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
                if Some(user.session) == server.own_session {
                    server.muted = user.self_muted;
                    server.deafened = user.self_deafened;
                    self.navigation.set_voice(Some(Conversation {
                        server: server.id.clone(),
                        channel: user.channel,
                    }));
                }
                server.users.insert(user.session, user);
            }
            SessionEvent::UserRemoved(session) => {
                server.users.remove(&session);
                server.talking.remove(&session);
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
fn merge_message(channel: &mut ChannelView, message: RenderedMessage) {
    if message
        .id
        .is_some_and(|id| channel.messages.iter().any(|entry| entry.id == Some(id)))
    {
        return;
    }
    channel.messages.push(message.into());
    // Observation time first: an unsaved message still appears where it arrived.
    channel
        .messages
        .sort_by_key(|message| (message.observed_at_ms, message.id.unwrap_or(i64::MAX)));
}
