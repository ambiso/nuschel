// SPDX-License-Identifier: EUPL-1.2
use crate::document::Document;
use crate::{
    attachments::{Attachment, Kind, Source},
    scrolling::{self, Anchor, Nested, Scroller, SmoothScroll},
    services::{ServiceEvent, Services},
};
use gpui_kit::{
    base::input::{InputEvent, InputState, TextareaState},
    *,
};
use nuschl_client::{
    Client, Conversation, SessionSecurity, Update,
    native::{Driver, Event, Native},
    platform::RenderKey,
    timeline::MAX_TIMELINE,
};
use nuschl_core::MAX_CONTENT_BYTES;
use nuschl_session::{
    config::{
        ClientSettings, Loaded, ManagedSettings, MumbleUrl, Profile, ServerBookmark, ShortcutAction,
    },
    connection::{Command, TextTarget},
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
pub use nuschl_client::{
    conversation::{ActivityLine, ConversationState as ChannelView},
    server::{CertificatePrompt, ServerState, Status},
    tree::{ChannelRow, TreeRow},
};
/// A server as the client knows it (`nuschl_client::server`).
pub type ServerView = ServerState;
/// What only drawing needs of a server, beside the client's state of it:
/// images for the GPU, rendered descriptions and comments, which runs of
/// activity lines are open; its identity and version notices, and the
/// end-to-end state of its direct conversations.
#[derive(Default)]
pub struct ServerUi {
    /// Welcome document converted once, with the original HTML it belongs to.
    pub welcome: Option<(String, Document)>,
    /// End-to-end state of direct conversations, by the peer's registration number.
    pub e2ee: BTreeMap<u32, nuschl_ipc::crypto::ConversationView>,
    /// Rendered user comments keyed by session, with the raw HTML they came from.
    pub comments: BTreeMap<u32, (String, Document)>,
    /// Decoded avatars by texture hash.
    pub avatar_images: BTreeMap<String, std::sync::Arc<RenderImage>>,
    /// The server website's icon, from the bookmark.
    pub icon: Option<std::sync::Arc<RenderImage>>,
    /// Channel descriptions as the content worker rendered them, by channel,
    /// with the raw HTML they came from.
    pub descriptions: BTreeMap<u32, (String, Document)>,
    /// Runs of activity lines shown expanded, by channel and the time of
    /// the run's first line.
    pub expanded_runs: BTreeMap<u32, BTreeSet<i64>>,
    /// Identities and key transparency on this server.
    pub identity: crate::identity::IdentityState,
    /// The server's minimum-version policy for this client.
    pub version: crate::version_notice::VersionNotice,
}
impl ServerUi {
    fn new(bookmark: &ServerBookmark) -> Self {
        Self {
            icon: bookmark.icon.as_ref().and_then(|icon| {
                crate::document::render_image(icon.width, icon.height, &icon.rgba)
            }),
            ..Default::default()
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
    /// The link (an invite or share link without a user name) that opened
    /// the form: its pin, channel and share token apply to the new server.
    pub link: Option<MumbleUrl>,
    _subscriptions: Vec<Subscription>,
}

/// A sent message with attachments, waiting for its images to be prepared.
struct Outgoing {
    /// The conversation, by server id and channel.
    key: (String, u32),
    /// Freeze identity while an image is prepared; the online session may change.
    context: (String, Option<u64>),
    private: bool,
    text: String,
    attachments: Vec<Attachment>,
    /// The server's image and text limits.
    limits: (usize, usize),
}

pub struct Desktop {
    /// The client SDK: servers, conversations and their timelines, and what
    /// to do about what the driver reports.
    pub client: Client,
    /// What only drawing needs of each server, by server id.
    pub ui: BTreeMap<String, ServerUi>,
    /// What drawing derives from the timelines' messages (presentation.rs).
    pub presentation: crate::presentation::Presentation,
    pub history_enabled: bool,
    /// The native driver (the client's platform), for what the client does
    /// not model yet: administration, identities, settings, push-to-talk.
    pub driver: Arc<Driver>,
    /// The desktop's own background work (services.rs).
    pub services: Services,
    pub profile: Profile,
    pub form: Option<ServerForm>,
    pub composer: Entity<TextareaState>,
    pub scroll: ScrollHandle,
    pub channel_scroll: ScrollHandle,
    pub server_scroll: ScrollHandle,
    pub rail: crate::rail::Rail,
    pub unread: crate::unread::Navigation,
    chat_motion: SmoothScroll,
    channel_motion: SmoothScroll,
    server_motion: SmoothScroll,
    /// The open dialog's scrolling (one dialog at a time), and which dialog
    /// this and the previous frame showed, so a newly opened one starts at the top.
    pub modal_scroll: ScrollHandle,
    modal_motion: SmoothScroll,
    /// Lists that scroll inside the open dialog.
    pub nested: Nested,
    pub modal_shown: std::cell::Cell<Option<&'static str>>,
    pub modal_seen: std::cell::Cell<Option<&'static str>>,
    /// Which dialogs were open in the last frame (see `keep_dialog_focus`).
    pub dialogs_seen: u8,
    scroll_frame_pending: bool,
    /// Keeps the timeline still while history is prepended above.
    pub anchor: Anchor,
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
    /// Whether Nuschl starts at login, read from the system when Settings opens.
    pub autostart: Option<bool>,
    pub update: crate::update::UpdateState,
    /// Desktop notifications, and whether the window has focus.
    notes: tokio::sync::mpsc::Sender<crate::notify::Note>,
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
    /// The rail's talk rings and listening badges (voice.rs).
    pub voice_motion: crate::voice::Motion,
    /// "Speak on two servers?" is being asked.
    pub second_server: Option<nuschl_client::SecondServer>,
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
    /// The Nuschl identity, as the crypto worker last reported it (Settings).
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
    acknowledged: Vec<(Conversation, String)>,
    /// The message the composer edits instead of sending a new one.
    pub editing: Option<crate::editing::Editing>,
    /// The link the keyboard is on in the timeline (timeline_keys.rs), and
    /// the entry the keyboard is on, as last rendered.
    pub link_focus: Option<(u64, usize)>,
    pub timeline_current: std::cell::Cell<Option<crate::scrolling::Key>>,
    /// A timer to clear others' typing indicators is running.
    pub typing_timer: bool,
    /// The clipboard image pasted last, and when it was last pasted.
    pasted_image: Option<(u64, std::time::Instant)>,
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
    _serve: Task<()>,
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
            this.client.focus.window_active = window.is_window_active();
            if !this.client.focus.window_active {
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
        let (driver, mut events) = Driver::new(handle.clone(), profile.clone(), settings.clone());
        let driver = Arc::new(driver);
        let ui = bookmarks
            .iter()
            .map(|bookmark| (bookmark.id.clone(), ServerUi::new(bookmark)))
            .collect();
        let client = Client::new(
            Box::new(driver.clone()),
            bookmarks,
            nuschl_client::native::preferences(&settings),
        );
        let (services, mut service_events) = Services::new(handle.clone(), driver.settings());
        // The desktop's own services report apart from the driver.
        let serve = cx.spawn(async move |this, cx| {
            while let Some(event) = service_events.recv().await {
                let applied = this.update(cx, |this, cx| {
                    this.apply_service_event(event, cx);
                    cx.notify();
                });
                if applied.is_err() {
                    break;
                }
            }
        });
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
            client,
            ui,
            presentation: Default::default(),
            history_enabled: true,
            notes: crate::notify::start(&handle),
            driver,
            services,
            profile,
            form: None,
            composer,
            scroll: ScrollHandle::new(),
            channel_scroll: ScrollHandle::new(),
            server_scroll: ScrollHandle::new(),
            rail: Default::default(),
            unread: Default::default(),
            chat_motion: SmoothScroll::default(),
            channel_motion: SmoothScroll::default(),
            server_motion: SmoothScroll::default(),
            modal_scroll: ScrollHandle::new(),
            modal_motion: SmoothScroll::default(),
            nested: Nested::default(),
            modal_shown: Default::default(),
            modal_seen: Default::default(),
            dialogs_seen: 0,
            scroll_frame_pending: false,
            anchor: Anchor::default(),
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
            viewer: None,
            viewer_requests: 0,
            menu_image: None,
            emoji: None,
            emoji_recent: Vec::new(),
            emoji_focus: cx.focus_handle(),
            mute_ease: None,
            modal_focus: cx.focus_handle(),
            deafen_ease: None,
            voice_motion: Default::default(),
            second_server: None,
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
            editing: None,
            link_focus: None,
            timeline_current: Default::default(),
            typing_timer: false,
            acknowledged: Vec::new(),
            pasted_image: None,
            drafts: BTreeMap::new(),
            attachments: BTreeMap::new(),
            attach_note: None,
            attachment_ids: 0,
            dragged_files: Default::default(),
            outgoing: std::collections::VecDeque::new(),
            _subscriptions: vec![subscription, activation, focus_lost, links],
            _receive: receive,
            _serve: serve,
            ptt_wake,
            _ptt_poll: ptt_poll,
        };
        desktop.apply_appearance(window, cx);
        if !desktop.client.servers.is_empty() {
            desktop.browse_server(0, window, cx);
        }
        match startup {
            Some(url) => desktop.open_url(url, window, cx),
            None => {
                for index in 0..desktop.client.servers.len() {
                    if desktop.client.servers[index].bookmark.auto_connect {
                        desktop.connect_as(index, None, true);
                    }
                }
            }
        }
        desktop.sync_global_shortcuts();
        desktop.refresh_icons();
        if crate::update::supported() {
            crate::update::cleanup();
            desktop.services.schedule_update_checks();
        }
        desktop
    }
    /// Check for an update now (Settings) rather than on the schedule.
    pub fn check_update_now(&mut self) {
        self.update = crate::update::UpdateState::Checking;
        self.services.check_update();
    }
    pub fn install_update(&mut self) {
        if let crate::update::UpdateState::Available(update) = &self.update {
            self.services.install_update(update.clone());
            self.update = crate::update::UpdateState::Installing { done: 0, total: 0 };
        }
    }
    /// Keep the system-wide shortcuts in step with the settings.
    pub fn sync_global_shortcuts(&mut self) {
        let wanted = crate::shortcuts::active(&self.settings);
        if let Some(released) = self.shortcuts.rebind(wanted, self.driver.runtime()) {
            for effect in released {
                self.apply_shortcut(effect);
            }
            self.ptt_wake.notify_one();
        }
    }
    /// A share link as text: on the server's web host, else the one in the
    /// settings, else the project's; with none, the app's own `nuschl://` link.
    pub fn share_url(
        &self,
        link: &nuschl_core::share::ShareLink,
        server_host: Option<&str>,
    ) -> String {
        let configured = Some(self.settings.share_web_host.trim()).filter(|host| !host.is_empty());
        match server_host
            .or(configured)
            .or(nuschl_core::share::PROJECT_WEB_HOST)
        {
            Some(host) => link.web_url(host),
            None => link.app_url(),
        }
    }
    /// Copy a channel's share link: at once, or when the server's token comes
    /// (`Update::ShareLink`).
    pub fn copy_share_link(&mut self, index: usize, channel: u32, cx: &mut Context<Self>) {
        match self.client.share_link(index, channel) {
            Ok(Some(link)) => {
                let url = self.share_url(&link, None);
                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(url));
            }
            Ok(None) => {}
            Err(error) => self.client.servers[index].set_notice(format!("No share link: {error}")),
        }
        cx.notify();
    }
    /// Connect to a `mumble://` or share link, reusing a matching saved server
    /// (the SDK decides); a link naming nobody for a new server opens the form,
    /// and the link's pin, policy, channel and token apply to the server saved
    /// from it.
    pub fn open_url(&mut self, url: MumbleUrl, window: &mut Window, cx: &mut Context<Self>) {
        let (host, port) = (url.host.clone(), url.port);
        let link = url.clone();
        let index = match self.client.open_url(url, &self.profile, false) {
            Ok(Some(index)) => index,
            Ok(None) => {
                self.open_form(None, window, cx);
                if let Some(form) = &mut self.form {
                    form.link = Some(link);
                    let port = port.to_string();
                    form.host
                        .update(cx, |input, cx| input.set_value(host, window, cx));
                    form.port
                        .update(cx, |input, cx| input.set_value(port, window, cx));
                    form.username
                        .update(cx, |input, cx| input.focus(window, cx));
                }
                return;
            }
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let bookmark = &self.client.servers[index].bookmark;
        if !self.ui.contains_key(&bookmark.id) {
            self.ui.insert(bookmark.id.clone(), ServerUi::new(bookmark));
            self.refresh_icons();
        }
        // A new server, or a pin from an invite, is saved now.
        self.apply_updates(cx);
        self.browse_server(index, window, cx);
    }
    /// The lock in the selected conversation's header (docs/e2ee.md §7.4): a direct
    /// conversation with an end-to-end group shows its own state; anything else the
    /// server's transport state. Chat encryption never turns the server badge green.
    pub fn conversation_lock(&self, index: usize) -> (SessionSecurity, String) {
        use nuschl_ipc::crypto::Lock;
        let server = &self.client.servers[index];
        if let Some((selected, channel)) = self.selected_indices()
            && selected == index
            && let Some(state) = &server.channels[channel].encryption
        {
            return (state.security, state.description.clone());
        }
        if server.mls {
            return (
                SessionSecurity::MlsPending,
                "End-to-end encryption is being set up".into(),
            );
        }
        let peer = self
            .selected_indices()
            .filter(|(selected, _)| *selected == index)
            .and_then(|(_, channel)| server.channels[channel].direct_peer.as_ref())
            .and_then(|peer| peer.user_id);
        let e2ee = self.ui.get(&server.id).map(|ui| &ui.e2ee);
        match peer.and_then(|peer| e2ee?.get(&peer)) {
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
            None if peer.is_some() && server.mls => (
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
        let selected = self.client.navigation.selection()?;
        let server = self
            .client
            .servers
            .iter()
            .position(|s| s.id == selected.server)?;
        let channel = self.client.servers[server]
            .channels
            .iter()
            .position(|c| c.id == selected.channel)?;
        Some((server, channel))
    }
    pub fn browse_server(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(server) = self.client.servers.get(index) else {
            return;
        };
        if self
            .client
            .navigation
            .selection()
            .is_none_or(|selection| selection.server != server.id)
        {
            self.unread.reset_channels();
            self.channel_motion.cancel();
            self.channel_scroll.set_offset(point(px(0.), px(0.)));
        }
        let channel = self
            .client
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
        if let Some(selected) = self.client.navigation.selection().cloned() {
            // An edit in progress ends; what it replaced is the draft.
            let draft = self
                .drop_edit()
                .unwrap_or_else(|| self.composer.read(cx).value().to_string());
            self.drafts
                .insert((selected.server, selected.channel), draft);
        }
        let Some(server) = self.client.servers.get(index) else {
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
        self.chat_motion.cancel();
        self.anchor.reset();
        self.older_paused = false;
        if let Err(error) = self.client.open(index, id) {
            self.older_paused = true;
            self.error = Some(error);
        }
        self.scroll.scroll_to_bottom();
    }
    /// What only drawing needs of the server at `index`.
    pub fn ui_of(&self, index: usize) -> Option<&ServerUi> {
        self.ui.get(&self.client.servers.get(index)?.id)
    }
    pub fn ui_of_mut(&mut self, index: usize) -> Option<&mut ServerUi> {
        self.ui.get_mut(&self.client.servers.get(index)?.id)
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
            let state = &self.client.servers[server];
            let channel = &state.channels[channel];
            self.presentation
                .present(&channel.timeline.messages, &channel.activity);
            // Runs whose first line is gone are no longer open.
            if let Some(runs) = self
                .ui
                .get_mut(&state.id)
                .and_then(|ui| ui.expanded_runs.get_mut(&channel.id))
            {
                let oldest = channel
                    .activity
                    .front()
                    .map_or(i64::MAX, |line| line.observed_at_ms);
                runs.retain(|first| *first >= oldest);
            }
        }
        if self.presentation.wants_pruning() {
            let live = self.client.servers.iter().flat_map(|server| {
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

    pub fn smooth_server_scroll(
        &mut self,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.server_motion.wheel(
            &self.server_scroll,
            event,
            window.line_height(),
            cx.reduce_motion(),
        ) {
            cx.stop_propagation();
            cx.notify();
        }
        self.schedule_scroll_frame(window, cx);
    }

    pub fn cancel_channel_scroll(&mut self) {
        self.channel_motion.cancel();
    }

    pub fn cancel_server_scroll(&mut self) {
        self.server_motion.cancel();
    }

    pub fn reveal_server(&mut self, slot: usize) {
        self.server_motion.cancel();
        self.server_scroll.scroll_to_item(slot);
    }
    /// A wheel step or touchpad motion in the open dialog, over `list` (one
    /// of the lists that scroll inside it) or elsewhere. It moves either the
    /// list or the dialog, never both (see `scrolling::WheelRun`).
    pub fn scroll_modal(
        &mut self,
        list: Option<&'static str>,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = window.line_height();
        let delta = scrolling::vertical(event, line_height);
        if delta == px(0.) || !f32::from(delta).is_finite() {
            return;
        }
        let reduce_motion = cx.reduce_motion();
        match self.nested.route(list, delta, std::time::Instant::now()) {
            Scroller::List(id) => self.nested.scroll(id, event, line_height, reduce_motion),
            Scroller::Dialog => scrolling::scroll_by(
                &mut self.modal_motion,
                &self.modal_scroll,
                event,
                line_height,
                reduce_motion,
            ),
        }
        cx.stop_propagation();
        cx.notify();
        self.schedule_scroll_frame(window, cx);
    }
    fn schedule_scroll_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scroll_frame_pending
            || !(self.chat_motion.active()
                || self.channel_motion.active()
                || self.server_motion.active()
                || self.modal_motion.active()
                || self.nested.active())
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
                        | this.server_motion.advance(&this.server_scroll)
                        | this.modal_motion.advance(&this.modal_scroll)
                        | this.nested.advance();
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
        let server = editing.and_then(|index| self.client.servers.get(index));
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
            link: None,
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
        // The link that opened the form, if the address is still its own.
        let link = form.link.clone().filter(|link| {
            editing.is_none() && link.host == bookmark.host && link.port == bookmark.port
        });
        if let Some(link) = &link {
            bookmark.certificate_sha256 = link.certificate_sha256.clone();
            if link.require_nuschl
                && let Err(error) = self
                    .profile
                    .policy()
                    .and_then(|mut store| store.raise(&link.host, link.port, true, &[]))
            {
                self.error = Some(format!(
                    "Could not save the invite's security policy: {error}"
                ));
                return;
            }
        }
        // A new or offline server connects on save; an online one reconnects
        // only when its address, name or password changed.
        let mut connect = editing.is_none();
        if let Some(previous) = editing
            .as_ref()
            .and_then(|id| self.client.servers.iter().find(|server| &server.id == id))
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
                    .client
                    .servers
                    .iter()
                    .position(|server| server.id == bookmark.id)
                    .unwrap_or_default();
                let server = &mut self.client.servers[index];
                server.name = bookmark.name.clone();
                server.bookmark = bookmark.clone();
                let bookmarks = self
                    .client
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
            .client
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
        self.ui
            .insert(bookmark.id.clone(), ServerUi::new(&bookmark));
        if index < self.client.servers.len() {
            self.client.replace_server(index, bookmark);
        } else {
            self.client.add_server(bookmark);
        }
        if let Some(link) = link {
            self.client.servers[index].pending_channel = link.channel;
            if let Some(token) = link.share_token {
                self.client.open_share_token(index, token);
            }
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
        self.client.connect(index, password, deafened);
    }
    /// The user's explicit reset of a server's sticky security record, then
    /// connect again.
    pub fn reset_security_record(&mut self, index: usize) {
        let Some(server) = self.client.servers.get_mut(index) else {
            return;
        };
        let (host, port) = (server.bookmark.host.clone(), server.bookmark.port);
        match self
            .profile
            .policy()
            .and_then(|mut store| store.reset(&host, port))
        {
            Ok(()) => {
                self.client.servers[index].downgrade = None;
                self.connect(index, None);
            }
            Err(error) => {
                self.error = Some(format!("Could not reset the security record: {error}"))
            }
        }
    }
    pub fn trust_certificate(&mut self, index: usize) {
        let Some(server) = self.client.servers.get(index) else {
            return;
        };
        let Some(prompt) = server.certificate.as_ref() else {
            return;
        };
        let mut bookmarks = self
            .client
            .servers
            .iter()
            .map(|server| server.bookmark.clone())
            .collect::<Vec<_>>();
        bookmarks[index].certificate_sha256 = Some(prompt.sha256.clone());
        if let Err(error) = self.write_servers(&bookmarks) {
            self.error = Some(error.to_string());
            return;
        }
        self.client.trust_certificate(index, false);
    }
    pub fn disconnect(&mut self, index: usize) {
        self.client.disconnect(index);
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
            .client
            .servers
            .iter()
            .filter(|server| server.id != id)
            .map(|server| server.bookmark.clone())
            .collect::<Vec<_>>();
        if let Err(error) = self.write_servers(&bookmarks) {
            self.error = Some(error.to_string());
            return;
        }
        self.client.remove_server(&id);
        self.reconcile_delayed_actions(cx);
        self.ui.remove(&id);
        self.form = None;
        if !self.client.servers.is_empty() {
            self.browse_server(0, window, cx);
        }
        cx.notify();
    }
    pub fn join_selected(&mut self) {
        let Some((server, channel)) = self.selected_indices() else {
            return;
        };
        if let TextTarget::Channel(id) = self.client.servers[server].channels[channel].target {
            self.client.join(server, id);
        }
    }
    /// Change preferences, persist them and apply them to live connections.
    pub fn sidebar_width(&self) -> f32 {
        self.sidebar_drag
            .map_or(self.settings.sidebar_width, |(_, _, width)| width)
    }
    pub fn drag_sidebar(&mut self, x: f32) {
        if let Some((start_x, start_width, width)) = &mut self.sidebar_drag {
            let range = nuschl_session::config::SIDEBAR_WIDTH;
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
        self.driver
            .identity(nuschl_ipc::crypto::CryptoRequest::Identity);
        if self.preview.is_none() {
            self.preview = Some(Default::default());
            self.services.start_preview();
        }
    }
    pub fn close_settings(&mut self) {
        self.settings_open = false;
        self.appearance.close_picker();
        self.shortcuts.recording = None;
        self.preview = None;
        self.services.stop_preview();
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
            self.services.preview(crate::preview::Command::Set {
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
        let now = nuschl_session::connection::now_ms();
        for server in &self.client.servers {
            let bookmark = &server.bookmark;
            let wait = if bookmark.icon.is_some() { 30 } else { 7 } * DAY_MS;
            if now - bookmark.icon_checked_ms >= wait {
                self.services
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
            self.services.preview(crate::preview::Command::EchoTest);
        }
    }
    pub fn stop_echo_test(&mut self) {
        if let Some(preview) = &mut self.preview {
            preview.echo_test = crate::preview::EchoTestState::Idle;
            if std::mem::take(&mut preview.test_opened_capture) {
                preview.close_capture();
            }
            self.services.preview(crate::preview::Command::StopEchoTest);
        }
    }
    pub fn update_settings(&mut self, change: impl FnOnce(&mut ClientSettings)) {
        let mut settings = self.settings.clone();
        change(&mut settings);
        let settings = self.managed.apply(settings.sanitized());
        if settings == self.settings {
            return;
        }
        if settings.relay_only {
            self.transfers.require_relay();
        }
        if let Err(error) = self.profile.save_settings(&settings, &self.managed) {
            self.error = Some(format!("Could not save settings: {error}"));
        }
        self.driver.apply_settings(settings.clone());
        self.client.preferences = nuschl_client::native::preferences(&settings);
        self.settings = settings;
        self.sync_global_shortcuts();
    }
    pub fn set_push_to_talk(&mut self, active: bool) {
        if self.ptt_active != active {
            self.ptt_active = active;
            self.driver.set_push_to_talk(active);
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
                || self.second_server.is_some()
                || self.panel.is_some())
        {
            if self.menu.take().is_none()
                && self.prompt.take().is_none()
                && self.second_server.take().is_none()
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
    /// What a shortcut does matches the footer's buttons and push to talk,
    /// on the talk servers (voice.rs).
    fn apply_shortcut(&mut self, effect: crate::shortcuts::Effect) {
        use crate::shortcuts::Effect;
        match effect {
            Effect::PushToTalk(active) => self.set_push_to_talk(active),
            Effect::PushToMute(active) => {
                self.push_muted = active;
                self.driver.set_push_to_mute(active);
            }
            Effect::ToggleMute => self.toggle_voice(false),
            Effect::ToggleDeafen => self.toggle_voice(true),
        }
    }
    pub fn sending(&self) -> bool {
        self.client.sending()
    }
    pub fn apply_composer_acks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (conversation, body) in self.acknowledged.drain(..) {
            if self.client.navigation.selection() == Some(&conversation)
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
        let Some((server, _)) = self.selected_indices() else {
            return;
        };
        // Text waits behind attachments still being prepared for this server.
        let server_id = &self.client.servers[server].id;
        if self.client.sending() || self.outgoing.iter().any(|batch| &batch.key.0 == server_id) {
            return;
        }
        if !self.attachments().is_empty() {
            return self.send_attachments(window, cx);
        }
        let body = self.composer.read(cx).value().to_string();
        if let Err(nuschl_client::SendError::TooLong) = self.client.send_text(body) {
            self.error = Some("Message exceeds 256 KiB.".into());
        }
        self.expire_typing(cx);
        cx.notify();
    }
    /// What the open conversation's composer has attached.
    pub fn attachments(&self) -> &[Attachment] {
        self.client
            .navigation
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
        match self.client.servers[server].outbox {
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
        let selected = self.client.navigation.selection()?;
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
        let server = self.client.servers.iter().find(|server| server.id == key.0);
        let limits = server
            .and_then(|server| server.limits.clone())
            .unwrap_or_default();
        let allow_html = limits.allow_html;
        let limit = limits.inline_image_byte_budget();
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
            self.services.prepare_image(id, source, budget);
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
        result: Result<nuschl_ipc::EncodedImage, String>,
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
            nuschl_core::ContentBlock::Image {
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
        let server = &mut self.client.servers[server];
        if server.mls {
            self.attach_note = Some((
                key,
                "Attachments and file offers are not supported on encrypted servers yet.".into(),
            ));
            cx.notify();
            return;
        }
        let limits = server.limits.clone().unwrap_or_default();
        let images = self.attachments.get(&key).is_some_and(|tray| {
            tray.iter()
                .any(|attachment| matches!(attachment, Attachment::Image { .. }))
        });
        let wire = nuschl_session::connection::legacy_body(&text, limits.allow_html);
        let note = if server.status != Status::Connected {
            Some("Connect before sending.".to_owned())
        } else if images && !limits.allow_html {
            Some("This server does not accept images.".to_owned())
        } else if text.len() > MAX_CONTENT_BYTES {
            Some(format!(
                "Message exceeds the {} KiB client limit.",
                MAX_CONTENT_BYTES / 1024
            ))
        } else {
            limits
                .check_text_wire(&wire)
                .err()
                .map(|reason| reason.to_string())
        };
        if note.is_some() {
            self.attach_note = note.map(|note| (key, note));
            cx.notify();
            return;
        }
        if let Err(reason) = server.message_target(channel) {
            self.attach_note = Some((key, reason.into()));
            cx.notify();
            return;
        }
        self.outgoing.push_back(Outgoing {
            key: key.clone(),
            context: (
                server.channels[channel].storage_key.clone(),
                server.channels[channel]
                    .direct_peer
                    .as_ref()
                    .map(|peer| peer.instance),
            ),
            private: server.channels[channel].is_direct(),
            text,
            attachments: self.attachments.remove(&key).unwrap_or_default(),
            limits: (
                limits.inline_image_byte_budget(),
                if limits.message_length == 0 {
                    MAX_CONTENT_BYTES
                } else {
                    limits.message_length
                },
            ),
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
        let mut remaining = self.outgoing.len();
        while remaining > 0
            && self
                .outgoing
                .front()
                .is_some_and(|batch| batch.attachments.iter().all(Attachment::ready))
        {
            remaining -= 1;
            let Some(batch) = self.outgoing.pop_front() else {
                break;
            };
            if self
                .client
                .servers
                .iter()
                .any(|server| server.id == batch.key.0 && server.mls)
            {
                self.attach_note = Some((
                    batch.key.clone(),
                    "Queued attachments cannot be sent to an encrypted server.".into(),
                ));
                self.outgoing.push_back(batch);
                continue;
            }
            let resolved = self
                .client
                .servers
                .iter_mut()
                .find(|server| server.id == batch.key.0)
                .filter(|server| server.status == Status::Connected)
                .and_then(|server| {
                    server
                        .channels
                        .iter()
                        .position(|channel| channel.id == batch.key.1)
                        .map(|position| {
                            let target = server.message_target(position)?;
                            let channel = &server.channels[position];
                            if channel.is_direct() != batch.private
                                || (batch.private
                                    && channel.storage_key != batch.context.0
                                    && channel.direct_peer.as_ref().map(|peer| peer.instance)
                                        != batch.context.1)
                            {
                                return Err(nuschl_core::model::RECIPIENT_UNAVAILABLE_NOTICE);
                            }
                            Ok(target)
                        })
                });
            let (target, expected_peer) = match resolved {
                Some(Ok(target)) => target,
                Some(Err(reason)) => {
                    self.attach_note = Some((batch.key.clone(), reason.into()));
                    self.outgoing.push_back(batch);
                    continue;
                }
                None => {
                    self.attach_note = Some((
                        batch.key.clone(),
                        "Waiting for this conversation to reconnect.".into(),
                    ));
                    self.outgoing.push_back(batch);
                    continue;
                }
            };
            if self
                .attach_note
                .as_ref()
                .is_some_and(|(key, _)| *key == batch.key)
            {
                self.attach_note = None;
            }
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
            let text = (!batch.text.trim().is_empty()).then_some(batch.text);
            let command = if images.is_empty() {
                text.map(|body| Command::SendText {
                    target,
                    body,
                    expected_peer,
                })
            } else {
                let (image_limit, message_limit) = batch.limits;
                let caption = text.map(|text| nuschl_session::connection::legacy_body(&text, true));
                let html =
                    crate::attachments::messages(caption, images, image_limit, message_limit);
                Some(Command::SendHtml {
                    target,
                    html,
                    expected_peer,
                })
            };
            if let Some(command) = command
                && let Err(error) = self.driver.command(&batch.key.0, command)
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
        let server = &self.client.servers[index];
        if server.own_session == Some(session) {
            return;
        }
        let Some(user_id) = server.users.get(&session).map(|peer| peer.user_id) else {
            return;
        };
        // A direct conversation checks the peer's identity (and its history), and on
        // an end-to-end server (`e2ee/1`) opens its group (creating it on first use).
        let mls = server.mls;
        if let Some(user_id) = user_id {
            crate::identity::check(self, index, user_id, true);
            if mls {
                self.run(
                    index,
                    nuschl_session::connection::Command::Mls(
                        nuschl_session::mls::MlsCommand::OpenDirect { user_id },
                    ),
                );
            }
        }
        // On a Nuschl server, between registered accounts, the server's
        // conversation opens once the server names it; otherwise the stock
        // path's opens now.
        if let Some(id) = self.client.open_direct(index, session) {
            self.browse_channel(index, id, window, cx);
        }
        cx.notify();
    }
    /// Load a page of the open conversation's history before `before` (the
    /// latest for None); a failure waits for the reader before a retry.
    pub fn load_history(&mut self, before: Option<i64>) {
        if let Err(error) = self.client.load_history(before) {
            self.older_paused = true;
            self.error = Some(error);
        }
    }
    /// The page before the oldest loaded message, once the reader nears the
    /// top. Returns whether one was requested.
    pub fn load_older(&mut self) -> bool {
        if let Some((server, channel)) = self.selected_indices()
            && !self.older_paused
            && !self.client.servers[server].channels[channel]
                .timeline
                .loading
            && let Some(before) = self.client.servers[server].channels[channel]
                .timeline
                .next_before
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
            self.client.focus.following =
                crate::scrolling::follows(self.client.focus.following, painted, now, max);
        }
        if self.client.focus.following {
            let channel = &mut self.client.servers[server].channels[channel];
            channel.new_messages = 0;
            // Out of sight above while at the bottom; they load again on scrolling up.
            channel.timeline.trim_oldest(MAX_TIMELINE);
            self.scroll.scroll_to_bottom();
        }
    }
    pub fn jump_to_latest(&mut self) {
        self.chat_motion.cancel();
        self.client.focus.following = true;
        self.scroll.scroll_to_bottom();
        if let Some((server, channel)) = self.selected_indices() {
            self.client.servers[server].channels[channel].new_messages = 0;
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
    /// Apply what the driver reported: the client decides, the desktop
    /// follows its updates.
    fn apply_events(&mut self, events: impl IntoIterator<Item = Event>, cx: &mut Context<Self>) {
        // A closed/replaced profile cannot keep requesting or rendering comment
        // updates. Only the explicit profile-open action creates SDK interest.
        self.client.retain_comment_interest(match self.panel {
            Some(Panel::UserInfo {
                server, session, ..
            }) => Some((server, session)),
            _ => None,
        });
        self.client.set_time(monotonic_ms());
        for event in events {
            match event.into_input() {
                Ok(input) => {
                    #[cfg(feature = "frame-timing")]
                    {
                        use nuschl_client::input::{Input, ServerEvent};
                        let kind = match &input {
                            Input::Server {
                                event: ServerEvent::Transport(_),
                                ..
                            } => Some("input_transport"),
                            Input::AudioStatus { .. } => Some("input_audio_status"),
                            Input::AudioDevices { .. } => Some("input_audio_devices"),
                            Input::AudioHealth { .. } => Some("input_audio_health"),
                            _ => None,
                        };
                        if let Some(kind) = kind {
                            crate::frame_timing::input(kind);
                        }
                    }
                    self.client.handle(input);
                    self.reconcile_delayed_actions(cx);
                    crate::acl_editor::sync(self, cx);
                }
                Err(Native::OwnIdentity(result)) => self.own_identity.apply(*result),
                Err(Native::Identity {
                    server,
                    generation,
                    event,
                }) => self.identity_event(&server, generation, *event),
                Err(Native::Mls {
                    server,
                    generation,
                    event,
                }) => self.mls_event(&server, generation, *event),
                Err(Native::Ignored) => {}
            }
        }
        // Channels sorted once a batch is applied, and edits and deletions
        // that came before their message's page.
        self.client.settle();
        self.apply_updates(cx);
        self.expire_typing(cx);
        self.flush_outgoing(cx);
    }
    /// Follow what the client says changed.
    pub fn apply_updates(&mut self, cx: &mut Context<Self>) {
        for update in self.client.take_updates() {
            match update {
                Update::BookmarksChanged => self.cache_dirty = true,
                Update::BookmarksChangedNow => {
                    let bookmarks = self
                        .client
                        .servers
                        .iter()
                        .map(|server| server.bookmark.clone())
                        .collect::<Vec<_>>();
                    if let Err(error) = self.write_servers(&bookmarks) {
                        self.error = Some(error.to_string());
                    }
                }
                Update::Browse { server, channel } => self.pending_browse = Some((server, channel)),
                Update::LoginRefused {
                    server,
                    kind,
                    reason,
                } => {
                    if let Some(index) = self.client.server_index(&server) {
                        self.pending_retry = Some((index, kind, reason));
                    }
                }
                Update::Sent { conversation, body } => {
                    self.acknowledged.push((conversation, body));
                }
                Update::PageShown { page, used } => {
                    self.older_paused = false;
                    match page {
                        // A conversation opens at its newest message.
                        nuschl_client::timeline::Page::Before(None) => {
                            self.anchor.reset();
                            self.scroll.scroll_to_bottom();
                        }
                        // A search result's message, highlighted.
                        nuschl_client::timeline::Page::Around(_) if used => {
                            self.anchor.reset();
                            self.reveal_jumped(cx);
                        }
                        _ => {}
                    }
                }
                Update::HistoryFailed(error) => {
                    self.older_paused = true;
                    self.error = Some(error);
                }
                Update::HistoryRecovered => {
                    self.older_paused = false;
                    if self
                        .error
                        .as_deref()
                        .is_some_and(|error| error.starts_with("Local history"))
                    {
                        self.error = None;
                    }
                }
                Update::OwnMessage => self.chat_motion.cancel(),
                Update::Notify(note) => {
                    let image = note.image.and_then(|(width, height, rgba)| {
                        crate::notify::thumbnail(width, height, &rgba)
                    });
                    let _ = self.notes.try_send(crate::notify::Note {
                        key: note.key,
                        summary: note.summary,
                        body: note.body,
                        image,
                    });
                }
                Update::Negotiated { server } => {
                    if let Some(index) = self.client.server_index(&server)
                        && let Some(ui) = self.ui.get_mut(&server)
                    {
                        ui.version
                            .negotiated(self.client.servers[index].policy.as_ref());
                    }
                }
                Update::Gap {
                    server,
                    from_ms,
                    to_ms,
                } => self
                    .client
                    .log_gap(&server, gap_text(from_ms, to_ms), to_ms),
                Update::MessageDeleted {
                    server,
                    conversation,
                    seq,
                } => {
                    if self.editing.as_ref().is_some_and(|editing| {
                        editing.server == server
                            && editing.seq == seq
                            && editing.channel_conversation == Some(conversation)
                    }) {
                        // The composer keeps the text; there is nothing left to edit.
                        self.editing = None;
                    }
                }
                Update::SearchResults { request, result } => {
                    self.apply_search_results(request, result, cx);
                }
                Update::Rendered {
                    server,
                    key,
                    source,
                    document,
                } => {
                    let Some(ui) = self.ui.get_mut(&server) else {
                        continue;
                    };
                    match key {
                        RenderKey::Welcome => {
                            ui.welcome = (!source.is_empty()).then(|| (source, document.into()));
                        }
                        RenderKey::Description(id) => {
                            ui.descriptions.insert(id, (source, document.into()));
                        }
                        RenderKey::Comment(session) => {
                            ui.comments.insert(session, (source, document.into()));
                        }
                    }
                }
                Update::ShareLink {
                    server,
                    link,
                    web_host,
                    ..
                } => {
                    if self.client.server_index(&server).is_none() {
                        continue;
                    }
                    // Copied as quietly as "Copy link"; only failures are shown.
                    let url = self.share_url(&link, web_host.as_deref());
                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(url));
                }
                Update::Avatar {
                    server,
                    hash,
                    document,
                } => {
                    let (Some(index), Some(image)) = (
                        self.client.server_index(&server),
                        Document::from(document).first_image(),
                    ) else {
                        continue;
                    };
                    let Some(ui) = self.ui.get_mut(&server) else {
                        continue;
                    };
                    // A server cycling texture hashes must not grow the cache
                    // without bound: past 1024, keep only avatars in use.
                    if ui.avatar_images.len() >= 1024 {
                        let used: BTreeSet<_> = self.client.servers[index]
                            .users
                            .values()
                            .filter_map(|user| user.texture_hash.clone())
                            .collect();
                        ui.avatar_images.retain(|hash, _| used.contains(hash));
                    }
                    ui.avatar_images.insert(hash, image);
                }
            }
        }
        if std::mem::take(&mut self.cache_dirty) {
            self.save_servers_soon(cx);
        }
    }
    /// End-to-end conversations on a server, as the session reports them
    /// (docs/e2ee.md §7.4): their lock state, and what went wrong.
    fn mls_event(&mut self, server: &str, generation: u64, event: nuschl_session::mls::MlsEvent) {
        let Some(index) = self
            .client
            .servers
            .iter()
            .position(|entry| entry.id == server && entry.generation == generation)
        else {
            return;
        };
        match event {
            nuschl_session::mls::MlsEvent::Conversation {
                view,
                peer: Some(peer),
            } => {
                if let Some(notice) = view.notices.last() {
                    self.client.servers[index].set_notice(notice.clone());
                }
                if let Some(ui) = self.ui.get_mut(server) {
                    ui.e2ee.insert(peer, view);
                }
            }
            nuschl_session::mls::MlsEvent::Failed(reason)
            | nuschl_session::mls::MlsEvent::NotSent { reason, .. } => {
                self.client.servers[index].set_notice(reason);
            }
            _ => {}
        }
    }
    /// Identity and key transparency on a server, as the crypto worker
    /// reports them (identity.rs).
    fn identity_event(
        &mut self,
        server: &str,
        generation: u64,
        event: nuschl_session::identity::IdentityEvent,
    ) {
        let Some(index) = self
            .client
            .servers
            .iter()
            .position(|entry| entry.id == server && entry.generation == generation)
        else {
            return;
        };
        if let nuschl_session::identity::IdentityEvent::Own(view) = &event {
            self.own_identity.view = Some(view.clone());
        }
        let Some(ui) = self.ui.get_mut(server) else {
            return;
        };
        let state = &mut self.client.servers[index];
        let users = &state.users;
        let name = |user_id: u32| {
            users
                .values()
                .find(|user| user.user_id == Some(user_id))
                .map(|user| user.name.clone())
        };
        let alarmed = ui.identity.alarm.is_some();
        if let Some(notice) = ui.identity.apply_named(event, name) {
            state.set_notice(notice);
        }
        // A new alarm interrupts: it is shown as a dialog until acknowledged.
        if !alarmed && ui.identity.alarm.is_some() && self.panel.is_none() {
            self.panel = Some(crate::panels::Panel::IdentityAlarm { server: index });
        }
    }
    /// What the desktop's own services report (services.rs).
    fn apply_service_event(&mut self, event: ServiceEvent, cx: &mut Context<Self>) {
        match event {
            ServiceEvent::Prepared { id, result } => self.prepared(id, result, cx),
            ServiceEvent::ServerIcon { server, icon } => {
                if let Some(server) = self
                    .client
                    .servers
                    .iter_mut()
                    .find(|entry| entry.id == server)
                {
                    server.bookmark.icon_checked_ms = nuschl_session::connection::now_ms();
                    if let Some(nuschl_core::ContentBlock::Image {
                        width,
                        height,
                        rgba,
                        ..
                    }) = icon
                    {
                        if let Some(ui) = self.ui.get_mut(&server.id) {
                            ui.icon = crate::document::render_image(width, height, &rgba);
                        }
                        server.bookmark.icon = Some(nuschl_session::config::ServerIcon {
                            width,
                            height,
                            rgba,
                        });
                    }
                    self.cache_dirty = true;
                }
            }
            ServiceEvent::Viewed { request, document } => {
                let image = document.and_then(|document| {
                    document.blocks.into_iter().find_map(|block| match block {
                        nuschl_core::ContentBlock::Image {
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
            ServiceEvent::Update(event) => {
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
                    UpdateEvent::Installed(Err(error)) => self.update = UpdateState::Failed(error),
                }
            }
            ServiceEvent::Preview(event) => {
                if let Some(preview) = &mut self.preview {
                    preview.apply(event);
                }
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
        let servers = self.client.order_bookmarks(servers);
        self.server_writes
            .write(&self.profile, self.servers_generation, &servers)
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
        let servers = self.client.ordered_bookmarks();
        let (profile, writes) = (self.profile.clone(), self.server_writes.clone());
        move || writes.write(&profile, generation, &servers)
    }
    pub fn reconcile_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((index, kind, reason)) = self.pending_retry.take()
            && index < self.client.servers.len()
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
            && let Some(index) = self
                .client
                .servers
                .iter()
                .position(|entry| entry.id == server)
        {
            self.browse_channel(index, channel, window, cx);
        }
        if self.selected_indices().is_none() && !self.client.servers.is_empty() {
            let index = self
                .client
                .navigation
                .selection()
                .and_then(|selection| {
                    self.client
                        .servers
                        .iter()
                        .position(|server| server.id == selection.server)
                })
                .unwrap_or(0);
            self.browse_server(index, window, cx);
        }
    }
}

#[cfg(test)]
mod server_writes_tests {
    use super::ServerWrites;
    use nuschl_session::config::{Profile, ServerBookmark};

    #[test]
    fn an_older_snapshot_never_overwrites_a_newer_one() {
        let root = std::env::temp_dir().join(format!("nuschl-writes-{}", std::process::id()));
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
