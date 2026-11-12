// SPDX-License-Identifier: EUPL-1.2
//! The terminal client's state: the SDK's `Client`, and what only this view
//! keeps (which pane has the keyboard, the composer, the open overlay, where
//! the timeline is scrolled, notes for the status line). Commands typed in
//! the composer (`/join`, `/edit`, …) and the keys of the full-screen view
//! end up in the same methods, so the plain-line mode and the full screen
//! behave alike. Everything that is not drawing is the SDK's.
use crate::{document, input::Input};
use nuschl_client::{
    Client, Conversation, Update,
    conversation::ConversationState,
    finder::{self, Item, Target},
    model::TextTarget,
    platform::Request,
    search::{Hit, Located, OPENS_ONLINE, Scope},
    server::{ServerState, Status},
    timeline::{Message, Page},
    tree::{ChannelRow, TreeRow},
};
use nuschl_core::{SearchCursor, SearchPage, model::RejectKind, search::Query};
use nuschl_session::config::{MumbleUrl, Profile};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Notes kept for the status line and the plain-line mode.
const NOTES: usize = 64;

/// Milliseconds on a monotonic clock, for the SDK's timers.
pub fn monotonic_ms() -> u64 {
    static START: std::sync::LazyLock<std::time::Instant> =
        std::sync::LazyLock::new(std::time::Instant::now);
    u64::try_from(START.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// How the client was started.
#[derive(Clone, Debug)]
pub struct Options {
    /// Voice may open audio devices. Without (`--no-audio`, over SSH),
    /// every connection stays deafened and muted.
    pub audio: bool,
    /// The program that opens web links (`xdg-open`, `open`); `None` only
    /// shows them.
    pub opener: Option<String>,
    /// Plain lines instead of the full screen: prompts are commands.
    pub plain: bool,
    /// Never read the remote host's OS clipboard during an SSH session.
    pub ssh: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Tree,
    Timeline,
    Composer,
}

/// What the full screen shows over everything else.
pub enum Overlay {
    Help {
        scroll: usize,
    },
    Finder {
        input: Input,
        /// The highlighted result, kept while the list changes.
        selected: Option<(String, Target)>,
    },
    Search(Search),
    /// Delete message `seq` of conversation `position` on server `server`?
    Delete {
        server: usize,
        position: usize,
        seq: u64,
        ours: bool,
    },
    /// A password or another name for server `server`.
    Prompt {
        kind: RejectKind,
        server: usize,
        input: Input,
        reason: String,
    },
    /// The links of a message, to open one.
    Links {
        links: Vec<String>,
        selected: usize,
    },
    /// The certificate of server `server` needs a decision.
    Certificate(usize),
}

/// A message search (Ctrl+F) and its results.
pub struct Search {
    pub input: Input,
    pub scope: Scope,
    /// Where it was started: what the conversation and server scopes cover.
    pub origin: Conversation,
    pub request: u64,
    pub results: Vec<Hit>,
    pub next: Option<SearchCursor>,
    pub loading: bool,
    pub indexing: bool,
    pub error: Option<String>,
    pub selected: usize,
    searched: Option<(String, Scope)>,
}

/// Our message being edited in the composer, and what the composer held.
pub struct Editing {
    pub server: String,
    pub channel: u32,
    pub seq: u64,
    pub draft: String,
}

/// A row of the channel pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Server(usize),
    Channel {
        server: usize,
        row: ChannelRow,
    },
    User {
        server: usize,
        session: u32,
        depth: usize,
        listener: bool,
    },
    /// A direct conversation, by its index in the server's `channels`.
    Direct {
        server: usize,
        index: usize,
    },
}

/// What a row is, kept while rows come and go.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowKey {
    Server(String),
    Channel(String, u32),
    User(String, u32, bool),
    Direct(String, u32),
}

/// Where the timeline is.
#[derive(Clone, Debug, Default)]
pub struct TimelineView {
    /// Lines scrolled up from the newest; 0 follows the newest message.
    pub scroll: usize,
    /// The message the keyboard is on (its serial), in the timeline pane.
    pub selected: Option<u64>,
    /// The most the view could scroll up, and its height, as last drawn.
    pub max: usize,
    pub height: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub text: String,
    pub warning: bool,
    pub at_ms: u64,
}

pub struct App {
    pub client: Client,
    pub graphics: std::cell::RefCell<crate::graphics::Graphics>,
    pub clipboard: crate::clipboard::Clipboard,
    profile: Option<Profile>,
    pub options: Options,
    pub pane: Pane,
    pub overlay: Option<Overlay>,
    pub composer: Input,
    pub editing: Option<Editing>,
    pub timeline: TimelineView,
    /// The message a search result opened, until the reader moves on.
    pub jumped: Option<(Conversation, i64)>,
    pub notes: VecDeque<Note>,
    /// Notes the plain-line mode has not printed yet.
    pub unprinted: usize,
    pub quit: bool,
    pub tz: jiff::tz::TimeZone,
    tree_cursor: (Option<RowKey>, usize),
    recent: finder::Recent,
    drafts: BTreeMap<(String, u32), String>,
    /// The saved servers changed and are written on the next tick.
    servers_dirty: bool,
    /// Servers whose certificate prompt was announced.
    prompted: BTreeSet<String>,
    /// Servers where we were registered when last looked at.
    registered: BTreeSet<String>,
    /// Local history failed and has not said it works again.
    local_history_failed: bool,
    requests: u64,
}

impl App {
    pub fn new(client: Client, profile: Option<Profile>, options: Options) -> Self {
        Self {
            client,
            graphics: Default::default(),
            clipboard: Default::default(),
            profile,
            options,
            pane: Pane::Composer,
            overlay: None,
            composer: Input::default(),
            editing: None,
            timeline: TimelineView::default(),
            jumped: None,
            notes: VecDeque::new(),
            unprinted: 0,
            quit: false,
            tz: jiff::tz::TimeZone::try_system().unwrap_or(jiff::tz::TimeZone::UTC),
            tree_cursor: (None, 0),
            recent: finder::Recent::default(),
            drafts: BTreeMap::new(),
            servers_dirty: false,
            prompted: BTreeSet::new(),
            registered: BTreeSet::new(),
            local_history_failed: false,
            requests: 0,
        }
    }

    // Notes.

    pub fn say(&mut self, text: impl Into<String>) {
        self.note(text.into(), false);
    }

    pub fn warn(&mut self, text: impl Into<String>) {
        self.note(text.into(), true);
    }

    fn note(&mut self, text: String, warning: bool) {
        self.notes.push_back(Note {
            text: document::clean_with(&text, true),
            warning,
            at_ms: monotonic_ms(),
        });
        self.unprinted += 1;
        while self.notes.len() > NOTES {
            self.notes.pop_front();
        }
        self.unprinted = self.unprinted.min(self.notes.len());
    }

    // Starting.

    /// Open `url` (a `mumble://` link), or connect the saved servers that
    /// connect at start (deafened: nobody hears a session that began
    /// without the user present).
    pub fn start(&mut self, url: Option<&str>) {
        if !self.client.servers.is_empty() {
            self.browse_server(0);
        }
        match url {
            Some(url) => self.open_url(url),
            None => {
                for index in 0..self.client.servers.len() {
                    if self.client.servers[index].bookmark.auto_connect {
                        self.client.connect(index, None, true);
                    }
                }
                if self.client.servers.is_empty() {
                    self.say(
                        "No saved servers. Connect with /url mumble://name@host[:port], \
                         or start nuschl-tui with such a link.",
                    );
                }
            }
        }
        self.apply_updates();
    }

    pub fn open_url(&mut self, url: &str) {
        let parsed = match MumbleUrl::parse(url.trim()) {
            Ok(url) => url,
            Err(error) => return self.warn(error),
        };
        let Some(profile) = self.profile.clone() else {
            return self.warn("No profile to save servers in.");
        };
        let (host, port) = (parsed.host.clone(), parsed.port);
        match self.client.open_url(parsed, &profile, !self.options.audio) {
            Ok(Some(index)) => {
                self.browse_server(index);
                self.say(format!(
                    "Connecting to {}…",
                    self.client.servers[index].name
                ));
            }
            Ok(None) => self.warn(format!(
                "Which name on {host}:{port}? Open mumble://NAME@{host}:{port} instead."
            )),
            Err(error) => self.warn(error),
        }
        self.apply_updates();
    }

    // What the platform reports.

    /// Apply a batch of the driver's events, then what changed.
    pub fn events(&mut self, events: impl IntoIterator<Item = nuschl_client::native::Event>) {
        self.client.set_time(monotonic_ms());
        for event in events {
            match event.into_input() {
                Ok(input) => {
                    self.clipboard_input(&input);
                    self.client.handle(input);
                }
                Err(nuschl_client::native::Native::Identity { event, .. }) => self.identity(*event),
                // End-to-end conversations are not shown here yet.
                Err(_) => {}
            }
        }
        self.settle();
    }

    /// What the identity worker says that must not go unseen; the rest of
    /// identity and key transparency is the desktop's for now.
    fn identity(&mut self, event: nuschl_session::identity::IdentityEvent) {
        use nuschl_session::identity::IdentityEvent;
        match event {
            IdentityEvent::RecoveryKey(key) => self.warn(format!(
                "Your Nuschl identity was made with this recovery key. Write it down: it is shown \
                 once here (the desktop's Settings → Identity shows it again until you confirm). \
                 Without it, losing this device means a new identity: {key}"
            )),
            IdentityEvent::Failed(reason) => self.warn(format!("Identity: {reason}")),
            _ => {}
        }
    }

    /// After inputs: the SDK settles, then the view follows its updates.
    pub fn settle(&mut self) {
        self.client.settle();
        self.apply_updates();
        self.client.expire_typing();
        let prompts: Vec<usize> = (0..self.client.servers.len())
            .filter(|&index| self.client.servers[index].certificate.is_some())
            .collect();
        for index in prompts {
            let id = self.client.servers[index].id.clone();
            if self.prompted.insert(id) {
                let server = &self.client.servers[index];
                let prompt = server.certificate.as_ref().expect("filtered");
                let text = if prompt.changed {
                    format!(
                        "{}: the server's certificate CHANGED ({}). New SHA-256: {}. \
                         Trust it only if the server's operator told you so: /trust, else /disconnect.",
                        server.name, prompt.reason, prompt.sha256
                    )
                } else {
                    format!(
                        "{}: first connection; the server's certificate has SHA-256 {}. \
                         Trust it with /trust (or y), or leave with /disconnect.",
                        server.name, prompt.sha256
                    )
                };
                self.warn(text);
                if !self.options.plain && self.overlay.is_none() {
                    self.overlay = Some(Overlay::Certificate(index));
                }
            }
        }
        for index in 0..self.client.servers.len() {
            let server = &self.client.servers[index];
            let registered = server.own_user_id().is_some();
            let id = server.id.clone();
            if registered && self.registered.insert(id.clone()) {
                let text = format!(
                    "You are registered on {} as {}.",
                    server.name, server.bookmark.username
                );
                if server.status == Status::Connected {
                    self.say(text);
                }
            } else if !registered && server.status == Status::Connected {
                self.registered.remove(&id);
            }
        }
        self.prompted.retain(|id| {
            self.client
                .servers
                .iter()
                .any(|server| &server.id == id && server.certificate.is_some())
        });
        self.follow();
    }

    /// Once a second: the clock, typing indicators, saved servers.
    pub fn tick(&mut self) {
        self.poll_media();
        self.client.set_time(monotonic_ms());
        self.client.expire_typing();
        if std::mem::take(&mut self.servers_dirty) {
            self.save_servers();
        }
    }

    pub fn poll_media(&mut self) {
        self.graphics.borrow_mut().poll();
        self.poll_clipboard();
    }

    /// The reader is at the newest message: it is read.
    fn follow(&mut self) {
        let detached = self.selected().is_some_and(|(server, channel)| {
            self.client.servers[server].channels[channel]
                .timeline
                .next_after
                .is_some()
        });
        self.client.focus.following =
            self.options.plain || (self.timeline.scroll == 0 && !detached);
        if self.client.focus.following
            && let Some((server, channel)) = self.selected()
        {
            self.client.servers[server].channels[channel].new_messages = 0;
        }
        self.client.mark_read();
    }

    /// The terminal gained or lost focus (where the terminal says so).
    pub fn focus(&mut self, active: bool) {
        self.client.focus.window_active = active;
        if active {
            self.follow();
        }
    }

    pub fn apply_updates(&mut self) {
        for update in self.client.take_updates() {
            match update {
                Update::BookmarksChanged => self.servers_dirty = true,
                Update::BookmarksChangedNow => self.save_servers(),
                Update::Browse { server, channel } => {
                    if let Some(index) = self.client.server_index(&server) {
                        self.browse(index, channel);
                    }
                }
                Update::LoginRefused {
                    server,
                    kind,
                    reason,
                } => {
                    let Some(index) = self.client.server_index(&server) else {
                        continue;
                    };
                    let ask = if kind.needs_password() {
                        "Enter the password (/password SECRET)."
                    } else {
                        "Choose another name (/username NAME)."
                    };
                    self.warn(format!(
                        "{}: {reason} {ask}",
                        self.client.servers[index].name
                    ));
                    if !self.options.plain {
                        self.overlay = Some(Overlay::Prompt {
                            kind,
                            server: index,
                            input: Input::default(),
                            reason,
                        });
                    }
                }
                Update::Sent { conversation, body } => {
                    self.acknowledge_composer(&conversation, &body);
                }
                Update::PageShown { page, used } => match page {
                    Page::Before(None) => self.timeline.scroll = 0,
                    Page::Around(id) if used => self.select_message_id(id),
                    _ => {}
                },
                Update::HistoryFailed(error) => {
                    let guest = self.selected().is_some_and(|(server, _)| {
                        let server = &self.client.servers[server];
                        server.status == Status::Connected && server.own_user_id().is_none()
                    });
                    if error.starts_with("Server history") && guest {
                        // nuschld keeps its history for registered people
                        // unless it lets guests read it.
                        self.warn(format!(
                            "{error} This server may show its history to registered people \
                             only: /register registers your name."
                        ));
                    } else {
                        self.local_history_failed |= error.starts_with("Local history");
                        self.warn(error);
                    }
                }
                Update::HistoryRecovered => {
                    if std::mem::take(&mut self.local_history_failed) {
                        self.say("Local history works again.");
                    }
                }
                Update::OwnMessage => self.timeline.scroll = 0,
                Update::Notify(note) => {
                    self.say(format!("{}: {}", note.summary, note.body));
                }
                Update::Negotiated { .. } => {}
                Update::Gap {
                    server,
                    from_ms,
                    to_ms,
                } => {
                    let text = self.gap_text(from_ms, to_ms);
                    self.client.log_gap(&server, text, to_ms);
                }
                Update::MessageDeleted {
                    server,
                    conversation,
                    seq,
                } => {
                    let editing_it = self.editing.as_ref().is_some_and(|editing| {
                        editing.server == server
                            && editing.seq == seq
                            && self.client.server_index(&server).is_some_and(|index| {
                                self.client.servers[index].channels.iter().any(|channel| {
                                    channel.id == editing.channel
                                        && channel.conversation == Some(conversation)
                                })
                            })
                    });
                    if editing_it {
                        // The composer keeps the text; nothing is left to edit.
                        self.editing = None;
                        self.warn("The message you were editing was deleted.");
                    }
                }
                Update::SearchResults { request, result } => self.search_results(request, result),
                // Share links: the terminal has no menu for them yet.
                Update::Rendered { .. } | Update::Avatar { .. } | Update::ShareLink { .. } => {}
            }
        }
    }

    // Time.

    /// "14:02" today, else with the date.
    pub fn time(&self, ms: i64) -> String {
        let Ok(stamp) = jiff::Timestamp::from_millisecond(ms) else {
            return String::new();
        };
        let when = stamp.to_zoned(self.tz.clone());
        let today = jiff::Timestamp::now().to_zoned(self.tz.clone()).date();
        if when.date() == today {
            when.strftime("%H:%M").to_string()
        } else {
            when.strftime("%Y-%m-%d %H:%M").to_string()
        }
    }

    fn gap_text(&self, from_ms: i64, to_ms: i64) -> String {
        let seconds = (to_ms - from_ms).max(0) / 1000;
        let duration = if seconds < 120 {
            format!("{seconds} s")
        } else {
            format!("{} min", seconds / 60)
        };
        format!(
            "Disconnected from {} to {} ({duration}). Messages sent meanwhile were not received.",
            self.time(from_ms),
            self.time(to_ms)
        )
    }

    // Servers.

    fn save_servers(&mut self) {
        self.servers_dirty = false;
        let Some(profile) = &self.profile else {
            return;
        };
        let bookmarks = self.client.ordered_bookmarks();
        if let Err(error) = profile.save_servers(&bookmarks) {
            self.warn(format!("Could not save the servers: {error}"));
        }
    }

    /// The server commands act on: the open conversation's, else the one
    /// the channel pane is on, else the first.
    pub fn current_server(&self) -> Option<usize> {
        self.selected()
            .map(|(server, _)| server)
            .or_else(|| match self.rows().get(self.tree_index()) {
                Some(Row::Server(server))
                | Some(Row::Channel { server, .. })
                | Some(Row::User { server, .. })
                | Some(Row::Direct { server, .. }) => Some(*server),
                None => None,
            })
            .or((!self.client.servers.is_empty()).then_some(0))
    }

    pub fn connect(&mut self, index: usize) {
        if index >= self.client.servers.len() {
            return;
        }
        if self.client.servers[index].certificate.is_some() {
            // Connecting never trusts a certificate by itself: it asks again.
            self.prompted.remove(&self.client.servers[index].id);
            return self.settle();
        }
        self.client.connect(index, None, !self.options.audio);
        self.say(format!(
            "Connecting to {}…",
            self.client.servers[index].name
        ));
    }

    pub fn disconnect(&mut self, index: usize) {
        if index >= self.client.servers.len() {
            return;
        }
        self.client.servers[index].certificate = None;
        self.client.disconnect(index);
        self.say(format!(
            "Disconnected from {}.",
            self.client.servers[index].name
        ));
    }

    /// Trust the certificate server `index` showed, and connect.
    pub fn trust(&mut self, index: usize) {
        if matches!(self.overlay, Some(Overlay::Certificate(_))) {
            self.overlay = None;
        }
        if self.client.trust_certificate(index, !self.options.audio) {
            self.say(format!(
                "Trusted the certificate of {}; connecting…",
                self.client.servers[index].name
            ));
            self.apply_updates();
        } else {
            self.warn("No certificate is waiting for a decision.");
        }
    }

    /// Toggle mute (or deafen) on the current server.
    pub fn set_mute(&mut self, deafen: bool) {
        let Some(index) = self.current_server() else {
            return;
        };
        let server = &self.client.servers[index];
        if !self.options.audio && (server.muted || server.deafened) {
            return self
                .warn("Voice is off (--no-audio): this session neither speaks nor listens.");
        }
        if let Some(ask) = self.client.set_mute(index, deafen) {
            // Your voice goes to another server already (docs/clients.md,
            // "Voice on several servers").
            let elsewhere: Vec<&str> = ask
                .talking
                .iter()
                .map(|other| self.client.servers[*other].name.as_str())
                .collect();
            return self.say(format!(
                "You are talking on {}. Speak on both with /speak both, or only here with /speak here.",
                elsewhere.join(" and ")
            ));
        }
        let server = &self.client.servers[index];
        self.say(match (server.muted, server.deafened) {
            (_, true) => "Deafened (and muted).",
            (true, false) => "Muted.",
            (false, false) => "Unmuted: others hear you.",
        });
    }

    // Conversations.

    pub fn selected(&self) -> Option<(usize, usize)> {
        self.client.selected_indices()
    }

    /// The open conversation and its server.
    pub fn open_conversation(&self) -> Option<(&ServerState, &ConversationState)> {
        let (server, channel) = self.selected()?;
        let server = &self.client.servers[server];
        Some((server, &server.channels[channel]))
    }

    pub fn browse_server(&mut self, index: usize) {
        let Some(server) = self.client.servers.get(index) else {
            return;
        };
        let channel = self
            .client
            .navigation
            .remembered_channel(&server.id)
            .filter(|id| server.channels.iter().any(|channel| channel.id == *id))
            .unwrap_or(0);
        self.browse(index, channel);
    }

    /// Open conversation `channel` of server `index`: the composer keeps a
    /// draft per conversation, an edit in progress ends.
    pub fn browse(&mut self, index: usize, channel: u32) {
        if let Some(selected) = self.client.navigation.selection().cloned() {
            let draft = match self.editing.take() {
                Some(editing) => editing.draft,
                None => self.composer.text().to_string(),
            };
            self.drafts
                .insert((selected.server, selected.channel), draft);
        }
        let Some(server) = self.client.servers.get(index) else {
            return;
        };
        if !server.channels.iter().any(|entry| entry.id == channel) {
            return;
        }
        let conversation = Conversation {
            server: server.id.clone(),
            channel,
        };
        let draft = self
            .drafts
            .remove(&(conversation.server.clone(), channel))
            .unwrap_or_default();
        self.composer.set(draft);
        self.recent.visited(&conversation);
        self.timeline = TimelineView::default();
        self.jumped = None;
        if let Err(error) = self.client.open(index, channel) {
            self.warn(error);
        }
        self.follow();
    }

    /// Join the voice of channel `channel` on server `index`, and open it.
    pub fn join(&mut self, index: usize, channel: u32) {
        if self.client.servers[index].status != Status::Connected {
            return self.warn("Connect before joining a channel.");
        }
        self.client.join(index, channel);
        let name = self.client.servers[index]
            .channels
            .iter()
            .find(|entry| entry.target == TextTarget::Channel(channel))
            .map(|entry| entry.name.clone())
            .unwrap_or_default();
        self.say(format!("Joining {name}…"));
        if self.client.navigation.selection()
            != Some(&Conversation {
                server: self.client.servers[index].id.clone(),
                channel,
            })
        {
            self.browse(index, channel);
        }
    }

    /// Direct messages with `session` on server `index`.
    pub fn open_direct(&mut self, index: usize, session: u32) {
        if let Some(channel) = self.client.open_direct(index, session) {
            self.browse(index, channel);
        }
        self.apply_updates();
    }

    /// The composer changed: say we are typing, now and then.
    pub fn composer_changed(&mut self) {
        self.client.set_time(monotonic_ms());
        self.client
            .composing(!self.composer.text().trim().is_empty() && self.editing.is_none());
    }

    /// Enter in the composer: a command, an edit, or a message.
    pub fn submit(&mut self) {
        let text = self.composer.text().to_string();
        if let Some(command) = text.strip_prefix('/')
            && !command.starts_with('/')
        {
            self.composer.take();
            self.command(command);
            return;
        }
        if self.editing.is_some() {
            return self.submit_edit();
        }
        // "//text" sends "/text".
        let body = text.strip_prefix('/').map(str::to_string).unwrap_or(text);
        self.send(body);
    }

    pub fn send(&mut self, body: String) {
        if self.clipboard.attached() {
            return self.send_image(&body);
        }
        if self.client.sending() {
            return self.warn("The previous message is still on its way.");
        }
        match self.client.send_text(body) {
            Ok(()) => {}
            Err(nuschl_client::SendError::TooLong) => self.warn("The message is too long."),
            Err(nuschl_client::SendError::Nothing) => {
                if self.selected().is_none() {
                    self.warn("Open a conversation first (Ctrl+K, or /go NAME).");
                }
            }
            Err(nuschl_client::SendError::Refused(reason)) => self.warn(reason),
        }
    }

    pub fn acknowledge_composer(&mut self, conversation: &Conversation, body: &str) {
        if self.client.navigation.selection() == Some(conversation)
            && self.editing.is_none()
            && self.composer.text() == body
        {
            self.composer.take();
        }
        let key = (conversation.server.clone(), conversation.channel);
        if self.drafts.get(&key).is_some_and(|draft| draft == body) {
            self.drafts.remove(&key);
        }
    }

    // Editing and deleting (Nuschl servers).

    /// Our newest message in the open conversation that may be edited.
    fn last_own(&self, edit: bool) -> Option<&Message> {
        let (server, channel) = self.open_conversation()?;
        channel.timeline.messages.iter().rev().find(|message| {
            if edit {
                server.may_edit(channel, message)
            } else {
                server.may_delete(channel, message) && message.ours(server.own_user_id())
            }
        })
    }

    fn message(&self, serial: u64) -> Option<&Message> {
        let (_, channel) = self.open_conversation()?;
        channel
            .timeline
            .messages
            .iter()
            .find(|message| message.serial == serial)
    }

    /// Edit message `serial` (our newest for None) in the composer.
    pub fn start_edit(&mut self, serial: Option<u64>) -> bool {
        let message = match serial {
            Some(serial) => self.message(serial),
            None => self.last_own(true),
        };
        let Some((message, (server, channel))) = message.zip(self.open_conversation()) else {
            self.warn("There is no message of yours here to edit.");
            return false;
        };
        if !server.may_edit(channel, message) {
            self.warn("Only your own stored messages on Nuschl servers can be edited (registered accounts).");
            return false;
        }
        let (source, seq) = (message.source.clone(), message.seq);
        let (server_id, channel_id) = (server.id.clone(), channel.id);
        let Some(seq) = seq else {
            return false;
        };
        let draft = match self.editing.take() {
            Some(editing) => editing.draft,
            None => self.composer.text().to_string(),
        };
        self.editing = Some(Editing {
            server: server_id,
            channel: channel_id,
            seq,
            draft,
        });
        self.composer.set(source);
        self.pane = Pane::Composer;
        true
    }

    /// Escape while editing: the composer gets back what it held.
    pub fn cancel_edit(&mut self) -> bool {
        let Some(editing) = self.editing.take() else {
            return false;
        };
        self.composer.set(editing.draft);
        true
    }

    fn submit_edit(&mut self) {
        let Some(editing) = &self.editing else {
            return;
        };
        let Some((index, position)) = self.selected().filter(|&(server, channel)| {
            self.client.servers[server].id == editing.server
                && self.client.servers[server].channels[channel].id == editing.channel
        }) else {
            return;
        };
        let text = self.composer.text().to_string();
        let seq = editing.seq;
        let unchanged = self.client.servers[index].channels[position]
            .timeline
            .messages
            .iter()
            .any(|message| message.seq == Some(seq) && message.source == text);
        if unchanged {
            self.cancel_edit();
            return;
        }
        if text.trim().is_empty() {
            self.cancel_edit();
            return self.ask_delete(index, position, seq);
        }
        match self.client.edit(index, position, seq, text) {
            Ok(()) => {
                self.cancel_edit();
            }
            Err(error) => self.warn(error),
        }
    }

    /// Ask before deleting message `serial` (our newest for None).
    pub fn delete(&mut self, serial: Option<u64>) {
        let message = match serial {
            Some(serial) => self.message(serial),
            None => self.last_own(false),
        };
        let Some(seq) = message.and_then(|message| message.seq) else {
            return self.warn("There is no message here you may delete.");
        };
        if let Some((index, position)) = self.selected() {
            self.ask_delete(index, position, seq);
        }
    }

    fn ask_delete(&mut self, index: usize, position: usize, seq: u64) {
        let server = &self.client.servers[index];
        let channel = &server.channels[position];
        let Some(message) = channel
            .timeline
            .messages
            .iter()
            .find(|message| message.seq == Some(seq))
        else {
            return;
        };
        if !server.may_delete(channel, message) {
            return self.warn("You may not delete this message.");
        }
        let ours = message.ours(server.own_user_id());
        if self.options.plain {
            // Commands are deliberate: no second question.
            return self.confirm_delete(index, position, seq);
        }
        self.overlay = Some(Overlay::Delete {
            server: index,
            position,
            seq,
            ours,
        });
    }

    pub fn confirm_delete(&mut self, index: usize, position: usize, seq: u64) {
        self.overlay = None;
        if let Err(error) = self.client.delete(index, position, seq) {
            self.warn(error);
        }
    }

    // History.

    /// The page before the oldest loaded message, if there is one.
    pub fn older(&mut self) -> bool {
        let Some((server, channel)) = self.selected() else {
            return false;
        };
        let timeline = &self.client.servers[server].channels[channel].timeline;
        if timeline.loading {
            return false;
        }
        let Some(before) = timeline.next_before else {
            return false;
        };
        if let Err(error) = self.client.load_history(Some(before)) {
            self.warn(error);
        }
        true
    }

    /// The page after the newest loaded message, while the timeline shows
    /// older history apart from the newest (after a search result).
    pub fn newer(&mut self) -> bool {
        let Some((server, channel)) = self.selected() else {
            return false;
        };
        let timeline = &self.client.servers[server].channels[channel].timeline;
        let Some(after) = timeline.next_after.filter(|_| !timeline.loading) else {
            return false;
        };
        if let Err(error) = self.client.load_page(server, channel, Page::After(after)) {
            self.warn(error);
        }
        true
    }

    /// Back to the newest message (End).
    pub fn latest(&mut self) {
        self.timeline.scroll = 0;
        self.timeline.selected = None;
        self.jumped = None;
        if let Some((server, channel)) = self.selected()
            && self.client.servers[server].channels[channel]
                .timeline
                .next_after
                .is_some()
            && let Err(error) = self.client.load_history(None)
        {
            self.warn(error);
        }
        self.follow();
    }

    fn select_message_id(&mut self, id: i64) {
        if let Some((_, channel)) = self.open_conversation()
            && let Some(message) = channel
                .timeline
                .messages
                .iter()
                .find(|message| message.id == Some(id))
        {
            self.timeline.selected = Some(message.serial);
            self.pane = Pane::Timeline;
        }
    }

    // The channel pane.

    /// Every server with its channel tree, people and direct conversations.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (index, server) in self.client.servers.iter().enumerate() {
            rows.push(Row::Server(index));
            for row in server.tree_rows() {
                rows.push(match row {
                    TreeRow::Channel(row) => Row::Channel { server: index, row },
                    TreeRow::User { session, depth } => Row::User {
                        server: index,
                        session,
                        depth,
                        listener: false,
                    },
                    TreeRow::Listener { session, depth } => Row::User {
                        server: index,
                        session,
                        depth,
                        listener: true,
                    },
                });
            }
            for (position, channel) in server.channels.iter().enumerate() {
                if channel.is_direct() {
                    rows.push(Row::Direct {
                        server: index,
                        index: position,
                    });
                }
            }
        }
        rows
    }

    fn row_key(&self, row: &Row) -> RowKey {
        let id = |server: usize| self.client.servers[server].id.clone();
        match *row {
            Row::Server(server) => RowKey::Server(id(server)),
            Row::Channel { server, row } => RowKey::Channel(
                id(server),
                self.client.servers[server].channels[row.index].id,
            ),
            Row::User {
                server,
                session,
                listener,
                ..
            } => RowKey::User(id(server), session, listener),
            Row::Direct { server, index } => {
                RowKey::Direct(id(server), self.client.servers[server].channels[index].id)
            }
        }
    }

    /// Where the channel pane's cursor is among `rows()`.
    pub fn tree_index(&self) -> usize {
        let rows = self.rows();
        let (key, index) = &self.tree_cursor;
        key.as_ref()
            .and_then(|key| rows.iter().position(|row| &self.row_key(row) == key))
            .unwrap_or_else(|| (*index).min(rows.len().saturating_sub(1)))
    }

    pub fn move_tree(&mut self, step: isize) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let next = self
            .tree_index()
            .saturating_add_signed(step)
            .min(rows.len() - 1);
        self.tree_cursor = (Some(self.row_key(&rows[next])), next);
    }

    pub fn tree_home(&mut self, end: bool) {
        let rows = self.rows();
        let next = if end { rows.len().saturating_sub(1) } else { 0 };
        self.tree_cursor = (rows.get(next).map(|row| self.row_key(row)), next);
    }

    /// Enter on a row: connect a server, open a channel or conversation,
    /// direct messages with a person. `join`: join a channel's voice, or a
    /// person's channel.
    pub fn activate(&mut self, join: bool) {
        let Some(row) = self.rows().get(self.tree_index()).copied() else {
            return;
        };
        match row {
            Row::Server(index) => match self.client.servers[index].status {
                Status::Disconnected => self.connect(index),
                _ if join => self.disconnect(index),
                _ => self.browse_server(index),
            },
            Row::Channel { server, row } => {
                let id = self.client.servers[server].channels[row.index].id;
                if join {
                    self.join(server, id);
                } else {
                    self.browse(server, id);
                    self.pane = Pane::Composer;
                }
            }
            Row::User {
                server, session, ..
            } => {
                if join {
                    if let Some(channel) = self.client.servers[server]
                        .users
                        .get(&session)
                        .map(|user| user.channel)
                    {
                        self.join(server, channel);
                    }
                } else if self.client.servers[server].own_session == Some(session) {
                    self.say("That is you.");
                } else {
                    self.open_direct(server, session);
                    self.pane = Pane::Composer;
                }
            }
            Row::Direct { server, index } => {
                let id = self.client.servers[server].channels[index].id;
                self.browse(server, id);
                self.pane = Pane::Composer;
            }
        }
    }

    /// Open or close the branch under the cursor (`None` toggles).
    pub fn expand(&mut self, open: Option<bool>) {
        if let Some(Row::Channel { server, row }) = self.rows().get(self.tree_index()).copied()
            && row.has_children
        {
            let id = self.client.servers[server].channels[row.index].id;
            self.client.servers[server].set_expanded(id, open.unwrap_or(!row.expanded));
        }
    }

    // The switcher (Ctrl+K).

    pub fn finder_items(&self, query: &str) -> Vec<Item> {
        finder::items(&self.client, &self.recent, query)
    }

    /// Go to a switcher result: a channel or conversation opens, a person's
    /// direct conversation opens; `join` joins a channel's voice or a
    /// person's channel.
    pub fn choose(&mut self, item: &Item, join: bool) {
        self.overlay = None;
        let Some(index) = self.client.server_index(&item.server) else {
            return;
        };
        match item.target {
            Target::Channel(id) if join => self.join(index, id),
            Target::Channel(id) | Target::Direct(id) => self.browse(index, id),
            Target::Person(session) if join => {
                if let Some(channel) = self.client.servers[index]
                    .users
                    .get(&session)
                    .map(|user| user.channel)
                {
                    self.join(index, channel);
                }
            }
            Target::Person(session) => self.open_direct(index, session),
        }
        self.pane = Pane::Composer;
    }

    // Search (Ctrl+F).

    pub fn open_search(&mut self, scope: Scope, query: Option<&str>) {
        let Some(origin) = self.client.navigation.selection().cloned() else {
            return self.warn("Open a conversation to search from.");
        };
        let mut input = Input::default();
        if let Some(query) = query {
            input.set(query);
        }
        self.overlay = Some(Overlay::Search(Search {
            input,
            scope,
            origin,
            request: 0,
            results: Vec::new(),
            next: None,
            loading: false,
            indexing: false,
            error: None,
            selected: 0,
            searched: None,
        }));
        self.run_search();
    }

    /// Search again if the query or scope changed.
    pub fn run_search(&mut self) {
        self.requests += 1;
        let request = self.requests;
        let Some(Overlay::Search(search)) = &self.overlay else {
            return;
        };
        let query = search.input.text().to_string();
        let key = (query.clone(), search.scope);
        if search.searched.as_ref() == Some(&key) {
            return;
        }
        let scope = self.client.search_scope(&search.origin, search.scope);
        let outcome = match scope {
            Ok(scope) if !Query::parse(&query).is_empty() => {
                Some(self.client.search(request, scope, query, None))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error.to_string())),
        };
        let Some(Overlay::Search(search)) = &mut self.overlay else {
            return;
        };
        search.searched = Some(key);
        search.results.clear();
        search.next = None;
        search.selected = 0;
        search.indexing = false;
        search.request = request;
        search.loading = matches!(outcome, Some(Ok(())));
        search.error = outcome.and_then(Result::err);
    }

    /// The next page of results.
    pub fn more_results(&mut self) {
        let Some(Overlay::Search(search)) = &self.overlay else {
            return;
        };
        let (Some(next), false) = (search.next, search.loading) else {
            return;
        };
        let (request, query) = (search.request, search.input.text().to_string());
        let outcome = self
            .client
            .search_scope(&search.origin, search.scope)
            .map_err(str::to_string)
            .and_then(|scope| self.client.search(request, scope, query, Some(next)));
        if let Some(Overlay::Search(search)) = &mut self.overlay {
            match outcome {
                Ok(()) => search.loading = true,
                Err(error) => search.error = Some(error),
            }
        }
    }

    fn search_results(&mut self, request: u64, result: Result<SearchPage, String>) {
        let Some(Overlay::Search(search)) = &self.overlay else {
            return;
        };
        if search.request != request {
            return;
        }
        let query = search.input.text().to_string();
        let page = result.map(|page| {
            let next = page.next;
            let indexing = page.indexing;
            (self.client.search_hits(page.hits, &query), next, indexing)
        });
        let Some(Overlay::Search(search)) = &mut self.overlay else {
            return;
        };
        search.loading = false;
        match page {
            Ok((hits, next, indexing)) => {
                search.results.extend(hits);
                search.next = next;
                search.indexing = indexing;
            }
            Err(error) => search.error = Some(error),
        }
        if self.options.plain {
            self.print_results();
        }
    }

    /// The results so far as notes (the plain-line mode).
    fn print_results(&mut self) {
        let Some(Overlay::Search(search)) = &self.overlay else {
            return;
        };
        let mut lines = Vec::new();
        if let Some(error) = &search.error {
            lines.push(format!("Search: {error}"));
        } else if search.results.is_empty() {
            lines.push("Search: nothing found.".to_string());
        }
        for (number, hit) in search.results.iter().enumerate() {
            lines.push(format!(
                "{}. {} {} in {}: {}",
                number + 1,
                self.time(hit.observed_at_ms),
                hit.author,
                hit.place,
                hit.snippet
            ));
        }
        if search.next.is_some() {
            lines.push("More with /more.".into());
        }
        for line in lines {
            self.say(line);
        }
    }

    /// Open result `number` (from 0): its message in its conversation.
    pub fn open_result(&mut self, number: usize) {
        let Some(Overlay::Search(search)) = &self.overlay else {
            return self.warn("Search first (/search WORDS).");
        };
        let Some(hit) = search.results.get(number).cloned() else {
            return self.warn("There is no such result.");
        };
        if !self.options.plain {
            self.overlay = None;
        }
        let Some(index) = self.client.server_index(&hit.server) else {
            return;
        };
        let ids = (hit.id, hit.seq);
        let mut located = self.client.locate(index, &hit.storage_key, ids);
        if let Ok(Located::Reopen { session }) = located {
            self.open_direct(index, session);
            located = self.client.locate(index, &hit.storage_key, ids);
        }
        let (position, id) = match located {
            Ok(Located::Found { position, id }) => (position, id),
            Ok(Located::Reopen { .. }) => return self.warn(OPENS_ONLINE),
            Err(notice) => return self.warn(notice),
        };
        let channel = self.client.servers[index].channels[position].id;
        let conversation = Conversation {
            server: hit.server.clone(),
            channel,
        };
        if self.client.navigation.selection() != Some(&conversation) {
            self.browse(index, channel);
        }
        self.jumped = Some((conversation, id));
        match self.client.show_message(index, position, id) {
            Ok(true) => self.select_message_id(id),
            Ok(false) => {}
            Err(error) => self.warn(error),
        }
    }

    // Links.

    /// The links of message `serial` in the open conversation.
    pub fn links_of(&self, serial: u64) -> Vec<String> {
        let Some(message) = self.message(serial) else {
            return Vec::new();
        };
        let mut links = Vec::new();
        document::lines(&message.document, &message.source, &mut links);
        links
    }

    /// The links of the open conversation's newest messages, newest last.
    pub fn recent_links(&self) -> Vec<String> {
        let Some((_, channel)) = self.open_conversation() else {
            return Vec::new();
        };
        let mut links = Vec::new();
        let start = channel.timeline.messages.len().saturating_sub(30);
        for message in &channel.timeline.messages[start..] {
            document::lines(&message.document, &message.source, &mut links);
        }
        links
    }

    /// Open a link: a `mumble://` link here, a web link with the system's
    /// opener (or only shown, without one).
    pub fn open_link(&mut self, url: &str) {
        self.overlay = None;
        let url = document::clean(url);
        if url.len() > 4096 {
            return self.warn("That link is too long to open.");
        }
        let lower = url.to_ascii_lowercase();
        if lower.starts_with("mumble://") {
            return self.open_url(&url);
        }
        if !(lower.starts_with("https://") || lower.starts_with("http://"))
            || url.contains(char::is_whitespace)
        {
            return self.warn(format!("Not opened (only web and mumble:// links): {url}"));
        }
        let Some(opener) = self.options.opener.clone() else {
            return self.say(format!("Link: {url}"));
        };
        match std::process::Command::new(&opener)
            .arg(&url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => self.say(format!("Opened {url}")),
            Err(error) => self.warn(format!("Could not run {opener}: {error}. The link: {url}")),
        }
    }

    // Prompts.

    /// A password or another name for a refused login.
    pub fn login(&mut self, index: usize, kind: RejectKind, value: String) {
        self.overlay = None;
        if index >= self.client.servers.len() || value.trim().is_empty() {
            return;
        }
        if kind.needs_password() {
            self.client.connect(index, Some(value), !self.options.audio);
        } else {
            let mut bookmark = self.client.servers[index].bookmark.clone();
            bookmark.username = value.trim().to_string();
            self.client.replace_server(index, bookmark);
            self.save_servers();
            self.client.connect(index, None, !self.options.audio);
        }
        self.say(format!(
            "Connecting to {}…",
            self.client.servers[index].name
        ));
    }

    // Commands.

    /// A command typed in the composer, without its slash.
    pub fn command(&mut self, line: &str) {
        let (name, argument) = line
            .trim()
            .split_once(char::is_whitespace)
            .map(|(name, rest)| (name, rest.trim()))
            .unwrap_or((line.trim(), ""));
        let server = self.current_server();
        match name.to_ascii_lowercase().as_str() {
            "paste-image" => self.paste_image(),
            "send-image" => {
                let caption = self.composer.text().to_string();
                self.send_image(&caption);
            }
            "discard-image" => {
                self.clipboard.discard();
                self.say("Image attachment discarded.");
            }
            "help" | "h" | "?" => {
                for line in crate::keys::COMMANDS {
                    self.say(*line);
                }
                if !self.options.plain {
                    self.overlay = Some(Overlay::Help { scroll: 0 });
                }
            }
            "quit" | "q" | "exit" => self.quit = true,
            "servers" => {
                let lines: Vec<String> = self
                    .client
                    .servers
                    .iter()
                    .enumerate()
                    .map(|(index, server)| {
                        format!(
                            "{}. {} ({}@{}:{}): {}",
                            index + 1,
                            server.name,
                            server.bookmark.username,
                            server.bookmark.host,
                            server.bookmark.port,
                            server.status_label()
                        )
                    })
                    .collect();
                if lines.is_empty() {
                    self.say("No saved servers.");
                }
                for line in lines {
                    self.say(line);
                }
            }
            "connect" => match self.server_argument(argument).or(server) {
                Some(index) => {
                    self.connect(index);
                    self.browse_server(index);
                }
                None => self.warn("Which server? /servers lists them."),
            },
            "disconnect" => match self.server_argument(argument).or(server) {
                Some(index) => self.disconnect(index),
                None => self.warn("Not connected."),
            },
            "url" | "open-url" => self.open_url(argument),
            "trust" => match self.server_argument(argument).or_else(|| {
                self.client
                    .servers
                    .iter()
                    .position(|server| server.certificate.is_some())
            }) {
                Some(index) => self.trust(index),
                None => self.warn("No certificate is waiting for a decision."),
            },
            "password" | "username" => match server {
                Some(index) => {
                    let kind = if name.eq_ignore_ascii_case("password") {
                        RejectKind::WrongServerPassword
                    } else {
                        RejectKind::UsernameInUse
                    };
                    self.login(index, kind, argument.to_string());
                }
                None => self.warn("No server."),
            },
            "register" => match server {
                Some(index) => match self.client.request(index, Request::RegisterSelf) {
                    Ok(()) => self.say("Asked the server to register your name."),
                    Err(error) => self.warn(error),
                },
                None => self.warn("No server."),
            },
            "go" | "g" | "find" => self.go(argument, false),
            "join" | "j" => {
                if argument.is_empty() {
                    match self.selected() {
                        Some((index, position)) => {
                            match self.client.servers[index].channels[position].target {
                                TextTarget::Channel(id) => self.join(index, id),
                                _ => self.warn("A direct conversation has no voice to join."),
                            }
                        }
                        None => self.warn("Join which channel? /join NAME"),
                    }
                } else {
                    self.go(argument, true);
                }
            }
            "dm" | "msg" => {
                let item = self
                    .finder_items(argument)
                    .into_iter()
                    .find(|item| matches!(item.target, Target::Person(_) | Target::Direct(_)));
                match item {
                    Some(item) => self.choose(&item, false),
                    None => self.warn(format!("Nobody called {argument} is here.")),
                }
            }
            "who" => self.who(),
            "channels" | "list" => self.list_channels(),
            "mute" => self.set_mute(false),
            "deafen" | "deaf" => self.set_mute(true),
            "speak" => match (self.current_server(), argument) {
                (Some(_), "both" | "here") if !self.options.audio => {
                    self.warn(
                        "Voice is off (--no-audio): this session neither speaks nor listens.",
                    );
                }
                (Some(index), "both" | "here") => {
                    self.client.speak_here(index, argument == "both");
                    self.say(if argument == "both" {
                        "Unmuted: this server also receives your voice."
                    } else {
                        "Unmuted: you speak only here now."
                    });
                }
                _ => self.warn("Speak where? /speak both, or /speak here"),
            },
            "edit" => {
                if self.start_edit(None) && !argument.is_empty() {
                    self.composer.set(argument);
                    self.submit_edit();
                } else if self.options.plain && self.editing.is_some() {
                    // No composer to edit in: say what it was.
                    let text = self.composer.take();
                    self.editing = None;
                    self.say(format!(
                        "Your message: {text}  (replace it with /edit NEW TEXT)"
                    ));
                }
            }
            "delete" => self.delete(None),
            "search" | "searchall" | "searchserver" => {
                let scope = match name {
                    "searchall" => Scope::All,
                    "searchserver" => Scope::Server,
                    _ => Scope::Conversation,
                };
                self.open_search(scope, Some(argument));
            }
            "more" => self.more_results(),
            "result" => match argument.parse::<usize>() {
                Ok(number) if number > 0 => self.open_result(number - 1),
                _ => self.warn("Which result? /result NUMBER"),
            },
            "older" => {
                if !self.older() {
                    self.say("No older messages.");
                }
            }
            "links" => {
                let links = self.recent_links();
                if links.is_empty() {
                    self.say("No links in the newest messages.");
                }
                for (number, link) in links.iter().enumerate() {
                    self.say(format!("[{}] {link}", number + 1));
                }
            }
            "open" => {
                let links = self.recent_links();
                match argument
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| links.get(n.checked_sub(1)?))
                {
                    Some(link) => {
                        let link = link.clone();
                        self.open_link(&link);
                    }
                    None => self.warn("Which link? /links numbers them."),
                }
            }
            "status" => self.status(),
            other => self.warn(format!("Unknown command /{other}; /help lists them.")),
        }
        self.apply_updates();
    }

    /// A saved server by number (from 1) or name.
    fn server_argument(&self, argument: &str) -> Option<usize> {
        if argument.is_empty() {
            return None;
        }
        if let Ok(number) = argument.parse::<usize>() {
            return number
                .checked_sub(1)
                .filter(|&index| index < self.client.servers.len());
        }
        self.client
            .servers
            .iter()
            .position(|server| server.name.eq_ignore_ascii_case(argument))
    }

    /// The switcher's best result for `query`: open it, or join it.
    fn go(&mut self, query: &str, join: bool) {
        let items = self.finder_items(query);
        let item = items
            .iter()
            .find(|item| !join || matches!(item.target, Target::Channel(_)))
            .or(items.first())
            .cloned();
        match item {
            Some(item) => self.choose(&item, join),
            None => self.warn(format!("Nothing called {query}.")),
        }
    }

    fn who(&mut self) {
        let Some((server, channel)) = self.open_conversation() else {
            return self.warn("Open a channel first.");
        };
        let TextTarget::Channel(id) = channel.target else {
            return self.say(format!("Direct conversation: {}", channel.name));
        };
        let mut people: Vec<String> = server
            .users
            .values()
            .filter(|user| user.channel == id)
            .map(|user| crate::view::person_label(&self.client, server, user))
            .collect();
        people.sort();
        let line = if people.is_empty() {
            format!("Nobody is in {}.", channel.name)
        } else {
            format!("In {}: {}.", channel.name, people.join("; "))
        };
        self.say(line);
    }

    fn list_channels(&mut self) {
        let lines: Vec<String> = self
            .rows()
            .iter()
            .map(|row| crate::view::row_text(self, row))
            .collect();
        for line in lines {
            self.say(line);
        }
    }

    fn status(&mut self) {
        let Some(index) = self.current_server() else {
            return self.say("No server.");
        };
        let server = &self.client.servers[index];
        let mut line = format!("{}: {}", server.name, server.status_label());
        if server.status == Status::Connected {
            line.push_str(match (server.muted, server.deafened) {
                (_, true) => ", deafened",
                (true, false) => ", muted",
                (false, false) => ", unmuted",
            });
            if self.options.audio {
                line.push_str(&format!(", audio: {}", server.audio_status));
            } else {
                line.push_str(", voice off (--no-audio)");
            }
        }
        if let Some(voice) = self.client.navigation.voice()
            && voice.server == server.id
            && let Some(channel) = server
                .channels
                .iter()
                .find(|channel| channel.target == TextTarget::Channel(voice.channel))
        {
            line.push_str(&format!(", in {}", channel.name));
        }
        self.say(line);
    }
}
