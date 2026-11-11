// SPDX-License-Identifier: EUPL-1.2
use crate::document::Document;
use crate::{
    attachments::{Attachment, Kind, Source},
    backend::{Backend, Event, RenderKey},
    scrolling::{Anchor, SmoothScroll},
};
use gpui_kit::{
    base::input::{InputEvent, InputState, TextareaState},
    *,
};
use nuschel_client::{
    Conversation, HistorySource, Navigation, SessionSecurity, timeline::MAX_TIMELINE,
};
use nuschel_core::MAX_CONTENT_BYTES;
use nuschel_session::{
    config::{
        CachedChannel, ClientSettings, Loaded, ManagedSettings, MumbleUrl, Profile, ServerBookmark,
        ShortcutAction,
    },
    connection::{Command, NuschelEvent, SessionEvent, TextTarget, User},
    model::RejectKind,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, PoisonError},
};

pub use crate::panels::Panel;

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

/// Milliseconds on a monotonic clock, for the SDK's timers (it reads no
/// clock itself).
pub fn monotonic_ms() -> u64 {
    static START: std::sync::LazyLock<std::time::Instant> =
        std::sync::LazyLock::new(std::time::Instant::now);
    u64::try_from(START.elapsed().as_millis()).unwrap_or(u64::MAX)
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
pub use nuschel_client::{
    conversation::{ActivityLine, ConversationState as ChannelView},
    server::{AudioHealth, CertificatePrompt, ServerState, Status},
    tree::{ChannelRow, TreeRow},
};
/// A server as the desktop shows it: the SDK's state of it, and what only
/// drawing needs (images for the GPU, rendered descriptions and comments,
/// which runs of activity lines are open).
pub struct ServerView {
    /// What the client knows of the server (`nuschel_client::server`).
    pub state: ServerState,
    /// End-to-end state of direct conversations, by the peer's registration number.
    pub e2ee: BTreeMap<u32, nuschel_ipc::crypto::ConversationView>,
    /// Rendered user comments keyed by session, with the raw HTML they came from.
    pub comments: BTreeMap<u32, (String, Document)>,
    /// Decoded avatars by texture hash.
    pub avatars: BTreeMap<String, std::sync::Arc<RenderImage>>,
    /// The server website's icon, from the bookmark.
    pub icon: Option<std::sync::Arc<RenderImage>>,
    /// Channel descriptions as the content worker rendered them, by channel.
    pub descriptions: BTreeMap<u32, Document>,
    /// Runs of activity lines shown expanded, by channel and the time of
    /// the run's first line.
    pub expanded_runs: BTreeMap<u32, BTreeSet<i64>>,
    /// Identities and key transparency on this server.
    pub identity: crate::identity::IdentityState,
    /// The server's minimum-version policy for this client.
    pub version: crate::version_notice::VersionNotice,
}
impl std::ops::Deref for ServerView {
    type Target = ServerState;
    fn deref(&self) -> &ServerState {
        &self.state
    }
}
impl std::ops::DerefMut for ServerView {
    fn deref_mut(&mut self) -> &mut ServerState {
        &mut self.state
    }
}
impl ServerView {
    #[cfg(test)]
    pub(crate) fn new_for_tests() -> Self {
        Self::new(ServerBookmark::new("Test", "localhost", 64738, "andi").expect("valid bookmark"))
    }
    fn new(bookmark: ServerBookmark) -> Self {
        let icon = bookmark
            .icon
            .as_ref()
            .and_then(|icon| crate::document::render_image(icon.width, icon.height, &icon.rgba));
        Self {
            state: ServerState::new(bookmark),
            e2ee: BTreeMap::new(),
            comments: BTreeMap::new(),
            avatars: BTreeMap::new(),
            icon,
            descriptions: BTreeMap::new(),
            expanded_runs: BTreeMap::new(),
            identity: Default::default(),
            version: Default::default(),
        }
    }
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
    /// What drawing derives from the timelines' messages (presentation.rs).
    pub presentation: crate::presentation::Presentation,
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
    /// Which dialogs were open in the last frame (see `keep_dialog_focus`).
    pub dialogs_seen: u8,
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
    /// The Nuschel identity, as the crypto worker last reported it (Settings).
    pub own_identity: crate::identity::OwnIdentity,
    pub search: Option<crate::search::SearchView>,
    /// The message a search result opened, highlighted for a moment.
    pub jumped: Option<crate::search::Jumped>,
    pub finder: crate::finder::Finder,
    pub(crate) pending_browse: Option<(String, u32)>,
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
    /// The message the composer edits instead of sending a new one.
    pub editing: Option<crate::editing::Editing>,
    /// The link the keyboard is on in the timeline (timeline_keys.rs), and
    /// the entry the keyboard is on, as last rendered.
    pub link_focus: Option<(u64, usize)>,
    pub timeline_current: std::cell::Cell<Option<crate::scrolling::Key>>,
    /// Typing indicators (see typing.rs): when we last said we type, where.
    pub typing_sent: Option<((String, u32), std::time::Instant)>,
    /// A timer to clear others' typing indicators is running.
    pub typing_timer: bool,
    /// Direct conversations asked for (`OpenDirect`), by request id: the
    /// server, the peer's session, and whether to open it once known.
    pub direct_requests: BTreeMap<u64, (String, u32, bool)>,
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
                    InputEvent::Change => this.composer_changed(cx),
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
        // Tab in the timeline goes through the current message's links first
        // (timeline_keys.rs); bindings would move focus before it sees the key.
        let desktop = cx.weak_entity();
        let links = cx.intercept_keystrokes({
            move |event, window, cx| {
                let keystroke = &event.keystroke;
                let modifiers = &keystroke.modifiers;
                if keystroke.key != "tab"
                    || modifiers.control
                    || modifiers.alt
                    || modifiers.platform
                {
                    return;
                }
                let _ = desktop.update(cx, |this, cx| {
                    if this.text.focus.is_focused(window) && this.timeline_tab(modifiers.shift) {
                        cx.stop_propagation();
                        cx.notify();
                    }
                });
            }
        });
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
            presentation: Default::default(),
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
            dialogs_seen: 0,
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
            own_identity: Default::default(),
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
            editing: None,
            link_focus: None,
            timeline_current: Default::default(),
            typing_sent: None,
            typing_timer: false,
            direct_requests: BTreeMap::new(),
            acknowledged: Vec::new(),
            pasted_image: None,
            passwords: BTreeMap::new(),
            drafts: BTreeMap::new(),
            attachments: BTreeMap::new(),
            attach_note: None,
            attachment_ids: 0,
            dragged_files: Default::default(),
            outgoing: std::collections::VecDeque::new(),
            _subscriptions: vec![subscription, activation, focus_lost, links],
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
        // An invite's policy=nuschel holds from the very first connection
        // (it only ever raises the server's record).
        if url.require_nuschel
            && let Err(error) = nuschel_session::sticky::PolicyStore::load(&self.profile.config_dir)
                .and_then(|mut store| store.raise(&url.host, url.port, true, &[]))
        {
            self.error = Some(format!(
                "Could not save the invite's security policy: {error}"
            ));
            return;
        }
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
                let mut bookmark = match ServerBookmark::new("", &url.host, url.port, &username) {
                    Ok(bookmark) => bookmark,
                    Err(error) => {
                        self.error = Some(error);
                        return;
                    }
                };
                // An invite's fp= pins the certificate before the first connection.
                bookmark.certificate_sha256 = url.certificate_sha256.clone();
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
        // A link never replaces a pin; it only fills one in.
        if self.servers[index].bookmark.certificate_sha256.is_none()
            && let Some(pin) = url.certificate_sha256
        {
            self.servers[index].bookmark.certificate_sha256 = Some(pin);
            let bookmarks = self
                .servers
                .iter()
                .map(|server| server.bookmark.clone())
                .collect::<Vec<_>>();
            if let Err(error) = self.write_servers(&bookmarks) {
                self.error = Some(error.to_string());
            }
        }
        self.servers[index].pending_channel = url.channel;
        self.browse_server(index, window, cx);
        self.connect(index, url.password);
    }
    /// The lock in the selected conversation's header (docs/e2ee.md §7.4): a direct
    /// conversation with an end-to-end group shows its own state; anything else the
    /// server's transport state. Chat encryption never turns the server badge green.
    pub fn conversation_lock(&self, index: usize) -> (SessionSecurity, String) {
        use nuschel_ipc::crypto::Lock;
        let server = &self.servers[index];
        let peer = self
            .selected_indices()
            .filter(|(selected, _)| *selected == index)
            .and_then(|(_, channel)| server.channels[channel].direct_peer.as_ref())
            .and_then(|peer| peer.user_id);
        match peer.and_then(|peer| server.e2ee.get(&peer)) {
            Some(view) if view.pinned => match &view.lock {
                Lock::Encrypted => (
                    SessionSecurity::MlsEstablished,
                    "End-to-end encrypted; every device verified by key transparency".into(),
                ),
                Lock::Verified => (
                    SessionSecurity::MlsEstablished,
                    "End-to-end encrypted; contact verified".into(),
                ),
                Lock::Attention { reason } => (
                    SessionSecurity::MlsPending,
                    format!("End-to-end encrypted, needs attention: {reason}"),
                ),
                Lock::Pending { reason } => (SessionSecurity::Unknown, reason.clone()),
                Lock::Open => (
                    SessionSecurity::TransportOnly,
                    "No end-to-end encryption; transport encryption only".into(),
                ),
            },
            Some(view) => match &view.lock {
                Lock::Pending { reason } => (SessionSecurity::Unknown, reason.clone()),
                _ => (
                    SessionSecurity::Unknown,
                    "End-to-end encryption is being set up".into(),
                ),
            },
            None if peer.is_some() && server.mls && self.settings.e2ee_direct_messages => (
                SessionSecurity::Unknown,
                "End-to-end encryption is being set up".into(),
            ),
            None => (
                server.security,
                crate::view::security_label(server.security).into(),
            ),
        }
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
        if let Some(selected) = self.navigation.selection().cloned() {
            // An edit in progress ends; what it replaced is the draft.
            let draft = self
                .drop_edit()
                .unwrap_or_else(|| self.composer.read(cx).value().to_string());
            self.drafts
                .insert((selected.server, selected.channel), draft);
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
        if channel.timeline.loaded {
            channel.timeline.trim_oldest(MAX_TIMELINE);
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
    /// Show all of a long message, or collapse it again.
    pub fn toggle_message(&mut self, serial: u64) {
        if let Some(message) = self.presentation.message_mut(serial) {
            message.expanded = !message.expanded;
        }
    }
    /// Present the open conversation's messages for drawing, and now and
    /// then forget the presentations of messages no timeline holds.
    pub fn present_selected(&mut self) {
        if let Some((server, channel)) = self.selected_indices() {
            let view = &mut self.servers[server];
            let channel = &view.state.channels[channel];
            self.presentation
                .present(&channel.timeline.messages, &channel.activity);
            // Runs whose first line is gone are no longer open.
            if let Some(runs) = view.expanded_runs.get_mut(&channel.id) {
                let oldest = channel
                    .activity
                    .front()
                    .map_or(i64::MAX, |line| line.observed_at_ms);
                runs.retain(|first| *first >= oldest);
            }
        }
        if self.presentation.wants_pruning() {
            let live = self.servers.iter().flat_map(|server| {
                server.channels.iter().flat_map(|channel| {
                    let messages = channel.timeline.messages.iter();
                    let lines = channel.activity.iter().map(|line| line.serial);
                    messages.map(|message| message.serial).chain(lines)
                })
            });
            self.presentation.prune(live.collect::<Vec<_>>());
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
    /// The user's explicit reset of a server's sticky security record, then
    /// connect again.
    pub fn reset_security_record(&mut self, index: usize) {
        let Some(server) = self.servers.get_mut(index) else {
            return;
        };
        let (host, port) = (server.bookmark.host.clone(), server.bookmark.port);
        match nuschel_session::sticky::PolicyStore::load(&self.profile.config_dir)
            .and_then(|mut store| store.reset(&host, port))
        {
            Ok(()) => {
                self.servers[index].downgrade = None;
                self.connect(index, self.passwords.get(&self.servers[index].id).cloned());
            }
            Err(error) => {
                self.error = Some(format!("Could not reset the security record: {error}"))
            }
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
        self.backend
            .identity(nuschel_ipc::crypto::CryptoRequest::Identity);
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
        if self.editing.is_some() {
            return self.submit_edit(window, cx);
        }
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
            Ok(()) => {
                self.sending = Some(pending);
                // Sent: we are no longer typing it.
                self.typing_sent = None;
            }
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
        // A direct conversation checks the peer's identity (and its history), and on
        // a `mls/1` server opens its end-to-end group (creating it on first use).
        // End-to-end direct messages are experimental and opt-in until the client
        // sends and shows them (Settings: `e2ee_direct_messages`).
        let mls = server.mls && self.settings.e2ee_direct_messages;
        if let Some(user_id) = peer.user_id {
            crate::identity::check(self, index, user_id, true);
            if mls {
                self.run(
                    index,
                    nuschel_session::connection::Command::Mls(
                        nuschel_session::mls::MlsCommand::OpenDirect { user_id },
                    ),
                );
            }
        }
        // On a Nuschel server, between registered accounts: the server's
        // conversation (it opens once the server names it).
        if self.open_durable_direct(index, &peer) {
            cx.notify();
            return;
        }
        let id = self.stock_direct_view(index, &peer);
        self.browse_channel(index, id, window, cx);
        cx.notify();
    }
    /// The timeline of direct messages with `peer` over the stock path (a
    /// `TextMessage` to their session), created if needed; returns its id.
    pub fn stock_direct_view(&mut self, index: usize, peer: &User) -> u32 {
        let name = crate::names::display_name(peer, &self.settings).to_string();
        self.servers[index].stock_direct_view(peer, &name)
    }
    pub fn load_history(&mut self, before: Option<i64>) {
        let Some((index, channel)) = self.selected_indices() else {
            return;
        };
        let Some(identity) = self.servers[index].history_identity() else {
            return;
        };
        let server = &mut self.servers[index].state;
        // Stock direct messages on a Nuschel server are local observations.
        let archive = server.archived(&server.channels[channel]);
        let cache = Some(crate::backend::CacheKey {
            identity: identity.clone(),
            channel: server.channels[channel].storage_key.clone(),
        });
        let channel = &mut server.channels[channel];
        channel.timeline.loading = true;
        if before.is_none() {
            channel.new_messages = 0;
        }
        let ticket = self.navigation.browse(
            Conversation {
                server: server.id.clone(),
                channel: channel.id,
            },
            server.history_source,
        );
        let requested = match channel.conversation.filter(|_| archive) {
            // A Nuschel server's history: everything this member may read.
            Some(conversation) => self.backend.server_history(
                server.id.clone(),
                conversation,
                (ticket, cache),
                nuschel_client::timeline::Page::Before(before),
            ),
            // Until the server names the conversation, nothing loads.
            None if server.history_source == HistorySource::ServerArchive
                && matches!(channel.target, TextTarget::Channel(_)) =>
            {
                channel.timeline.loading = false;
                return;
            }
            None => self.backend.history(
                server.id.clone(),
                identity,
                channel.storage_key.clone(),
                ticket,
                before,
            ),
        };
        if let Err(error) = requested {
            channel.timeline.loading = false;
            self.older_paused = true;
            self.error = Some(error);
        }
    }
    /// The page before the oldest loaded message, once the reader nears the
    /// top. Returns whether one was requested.
    pub fn load_older(&mut self) -> bool {
        if let Some((server, channel)) = self.selected_indices()
            && !self.older_paused
            && !self.servers[server].channels[channel].timeline.loading
            && let Some(before) = self.servers[server].channels[channel].timeline.next_before
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
            channel.timeline.trim_oldest(MAX_TIMELINE);
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
                Event::Edited {
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
                    nuschel_client::timeline::Change::Edited {
                        source,
                        document,
                        edited_ms,
                    },
                ),
                Event::Deleted {
                    server,
                    conversation,
                    seq,
                    deletion,
                } => self.apply_change(
                    &server,
                    conversation,
                    seq,
                    nuschel_client::timeline::Change::Deleted(deletion),
                ),
                Event::ChangeFailed {
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
                Event::HistoryFailed {
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
                    self.older_paused = true;
                    self.error = Some(format!("Server history: {reason}"));
                }
                Event::Downgrade {
                    server,
                    generation,
                    reason,
                } => {
                    if let Some(index) = self
                        .servers
                        .iter()
                        .position(|entry| entry.id == server && entry.generation == generation)
                    {
                        self.disconnect(index);
                        self.servers[index].downgrade = Some(reason);
                    }
                }
                Event::HistoryError(error) => {
                    self.error = Some(format!("Local history: {error}"));
                    self.older_paused = true;
                    for server in &mut self.servers {
                        for channel in &mut server.channels {
                            channel.timeline.loading = false;
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
                        channel.timeline.loading = false;
                        self.older_paused = false;
                        tracing::debug!(?before, count = messages.len(), "history page");
                        channel.timeline.take_page(before, messages, next_before);
                        if before.is_none() {
                            // A conversation opens at its newest message.
                            self.anchor.reset();
                            self.following = true;
                            self.scroll.scroll_to_bottom();
                        }
                    }
                }
                Event::OwnIdentity(result) => self.own_identity.apply(result),
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
                                if server
                                    .channel_view(id)
                                    .is_some_and(|channel| channel.description_raw == source)
                                {
                                    server.descriptions.insert(id, document.into());
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
                    mut message,
                } => {
                    let Some(server) = self
                        .servers
                        .iter_mut()
                        .find(|entry| entry.id == server && entry.generation == generation)
                        .map(|view| &mut view.state)
                    else {
                        continue;
                    };
                    // Acknowledged before it was shown.
                    if let Some(key) = message.pending
                        && let Some(seq) = server.acked.remove(&key)
                    {
                        message.pending = None;
                        message.id = seq.map(|seq| seq as i64);
                        message.seq = seq;
                    }
                    // A stock direct message from someone we have a durable
                    // direct conversation with (an image, or from a stock
                    // client) shows there; it is not in the server's history.
                    let durable = match (&target, &peer) {
                        (TextTarget::Direct(_), Some(peer))
                            if peer.user_id.is_some_and(|id| id != 0) =>
                        {
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
                    let index = if let Some(index) = durable {
                        index
                    } else if let Some(index) = server.channels.iter().position(|channel| {
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
                        let name = peer
                            .as_ref()
                            .map(|peer| crate::names::display_name(peer, &self.settings))
                            .unwrap_or("Direct message");
                        server.received_direct(
                            name,
                            target.clone(),
                            peer.clone(),
                            storage_key.clone(),
                        )
                    } else if let TextTarget::DirectConversation(id) = target {
                        // A durable direct conversation (directs.rs), perhaps new.
                        let name = match &peer {
                            Some(peer) => {
                                crate::names::display_name(peer, &self.settings).to_string()
                            }
                            None if message.outgoing => server
                                .directs
                                .get(&id)
                                .map(|direct| direct.name.clone())
                                .unwrap_or_else(|| "Direct message".into()),
                            None => message.message.author.clone(),
                        };
                        server.direct_view(id, &name, peer.clone())
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
                        let direct = matches!(
                            target,
                            TextTarget::Direct(_) | TextTarget::DirectConversation(_)
                        );
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
                        let direct = matches!(
                            target,
                            TextTarget::Direct(_) | TextTarget::DirectConversation(_)
                        );
                        let mentioned = crate::notify::mentions(
                            &message.message.body,
                            &server.bookmark.username,
                        );
                        if let Some(sound) = crate::sounds::message(shown, direct, mentioned) {
                            self.backend.play_sound(&server.id, sound);
                        }
                    }
                    let channel = &mut server.channels[index];
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
                    } else if matches!(target, TextTarget::DirectConversation(_)) && peer.is_some()
                    {
                        channel.direct_peer = peer;
                    }
                    let outgoing = message.outgoing;
                    let mention = !outgoing
                        && (matches!(
                            channel.target,
                            TextTarget::Direct(_) | TextTarget::DirectConversation(_)
                        ) || message.document.blocks.iter().any(|block| {
                            matches!(block, nuschel_core::ContentBlock::Text { text, .. }
                                    if crate::notify::mentions(text, &server.bookmark.username))
                        }));
                    // Older history shown apart from the newest gets the
                    // message with the newer pages.
                    let added = channel.timeline.receive(message);
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
                        channel.timeline.trim_oldest(MAX_TIMELINE);
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
        self.expire_typing(cx);
        // Edits and deletions that came before their message's page.
        for server in 0..self.servers.len() {
            if !self.servers[server].waiting_changes.is_empty() {
                self.apply_waiting_changes(server);
            }
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
            // The backend keeps the sticky record. With history, the timeline
            // pages from the server (seqs), not from local observations.
            SessionEvent::Negotiated(negotiated) => {
                server.version.negotiated(&negotiated);
                server.mls = negotiated
                    .capabilities
                    .iter()
                    .any(|capability| capability == "mls/1");
                let history = negotiated
                    .capabilities
                    .iter()
                    .any(|capability| capability == "history/1");
                server.capabilities = negotiated.capabilities.clone();
                server.policy = negotiated.policy;
                if history && server.history_source != HistorySource::ServerArchive {
                    server.history_source = HistorySource::ServerArchive;
                    for channel in &mut server.channels {
                        channel.timeline.reset();
                    }
                }
            }
            SessionEvent::ChannelInfo { channel, info } => {
                let selected = self.navigation.selection().is_some_and(|selected| {
                    selected.server == server.id && selected.channel == channel
                });
                if let Some(view) = server.channel_view(channel) {
                    view.conversation = info.conversation.as_slice().try_into().ok();
                    if selected && !view.timeline.loaded && !view.timeline.loading {
                        self.load_history(None);
                    }
                }
            }
            SessionEvent::ChatAcked { key, seq } => {
                if !settle_pending(server, key, Some(seq)) {
                    server.acked.insert(key, Some(seq));
                }
            }
            SessionEvent::ChatFailed { key, reason } => {
                server.notice = Some(format!("A message was not sent: {reason}"));
                if !settle_pending(server, key, None) {
                    server.acked.insert(key, None);
                }
            }
            SessionEvent::ConversationState {
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
            SessionEvent::Nuschel(NuschelEvent::Typing(typing)) => {
                server.someone_typing(&typing.conversation, typing.session, monotonic_ms());
            }
            SessionEvent::Nuschel(NuschelEvent::DirectList(list)) => {
                self.directs_listed(index, list.directs);
            }
            SessionEvent::Nuschel(NuschelEvent::DirectOpened(opened)) => {
                self.direct_opened(index, opened);
            }
            SessionEvent::Nuschel(NuschelEvent::Error(error)) => {
                self.direct_failed(index, error);
            }
            SessionEvent::Nuschel(NuschelEvent::ReadMarker(marker)) => {
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
            SessionEvent::ChatPending(_) | SessionEvent::Nuschel(_) => {}
            SessionEvent::Mls(event) => match event {
                nuschel_session::mls::MlsEvent::Conversation {
                    view,
                    peer: Some(peer),
                } => {
                    if let Some(notice) = view.notices.last() {
                        server.notice = Some(notice.clone());
                    }
                    server.e2ee.insert(peer, view);
                }
                nuschel_session::mls::MlsEvent::Failed(reason)
                | nuschel_session::mls::MlsEvent::NotSent { reason, .. } => {
                    server.notice = Some(reason);
                }
                _ => {}
            },
            SessionEvent::Identity(event) => {
                if let nuschel_session::identity::IdentityEvent::Own(view) = &event {
                    self.own_identity.view = Some(view.clone());
                }
                let ServerView {
                    state, identity, ..
                } = &mut *server;
                let users = &state.users;
                let name = |user_id: u32| {
                    users
                        .values()
                        .find(|user| user.user_id == Some(user_id))
                        .map(|user| user.name.clone())
                };
                let alarmed = identity.alarm.is_some();
                if let Some(notice) = identity.apply_named(event, name) {
                    state.notice = Some(notice);
                }
                // A new alarm interrupts: it is shown as a dialog until acknowledged.
                if !alarmed && identity.alarm.is_some() && self.panel.is_none() {
                    self.panel = Some(crate::panels::Panel::IdentityAlarm { server: index });
                }
            }
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
                        channel.timeline.loaded = false;
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
                let server = &mut server.state;
                server.channels.retain(|channel| match channel.target {
                    TextTarget::Channel(id) => server.seen_channels.contains(&id),
                    TextTarget::Direct(_) | TextTarget::DirectConversation(_) => true,
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
                        server.descriptions.remove(&update.id);
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
                    server.descriptions.remove(&update.id);
                    server.channels.push(ChannelView {
                        topic: "Channel chat · select Join voice to move here".into(),
                        links: update.links,
                        temporary: update.temporary,
                        max_users: update.max_users,
                        description_raw: update.description,
                        ..ChannelView::channel(
                            update.id,
                            update.parent,
                            update.name,
                            update.position,
                        )
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
                server.descriptions.remove(&id);
                server.expanded_runs.remove(&id);
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
/// Our message on a Nuschel server was acknowledged as `seq` (or refused):
/// returns whether it is shown yet.
fn settle_pending(server: &mut ServerView, key: [u8; 16], seq: Option<u64>) -> bool {
    server
        .channels
        .iter_mut()
        .any(|channel| channel.timeline.settle(key, seq))
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
