// SPDX-License-Identifier: EUPL-1.2
use crate::{
    backend::{Backend, Event, RenderedMessage},
    scrolling::SmoothScroll,
};
use gpui_kit::{
    base::input::{InputEvent, InputState, TextareaState},
    *,
};
use nuschel_client::{Conversation, HistorySource, Navigation, SessionSecurity};
use nuschel_core::{MAX_CONTENT_BYTES, MessageDocument, PreviewPolicy};
use nuschel_session::{
    config::{CachedChannel, Profile, ServerBookmark},
    connection::{Command, SessionEvent, TextTarget, User},
};
use std::collections::{BTreeMap, BTreeSet};

pub struct MessageView {
    pub id: Option<i64>,
    pub author: String,
    pub time: String,
    pub document: MessageDocument,
    pub expanded: bool,
    pub has_more: bool,
}
impl From<RenderedMessage> for MessageView {
    fn from(message: RenderedMessage) -> Self {
        let minutes = message
            .message
            .observed_at_ms
            .div_euclid(60_000)
            .rem_euclid(1440);
        Self {
            id: message.id,
            author: message.message.author,
            time: format!(
                "{:02}:{:02} UTC{}{}",
                minutes / 60,
                minutes % 60,
                if message.outgoing {
                    " · sent, unconfirmed"
                } else {
                    ""
                },
                if message.id.is_none() {
                    " · not saved"
                } else {
                    ""
                }
            ),
            has_more: PreviewPolicy::default()
                .preview(&message.message.body, false)
                .has_more,
            document: message.document,
            expanded: false,
        }
    }
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
}
impl ChannelView {
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
pub struct ServerView {
    pub id: String,
    pub name: String,
    pub initials: String,
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
            })
            .collect::<Vec<_>>();
        if !channels.iter().any(|channel| channel.id == 0) {
            channels.insert(0, ChannelView::root());
        }
        Self {
            id: bookmark.id.clone(),
            name: bookmark.name.clone(),
            initials: bookmark
                .name
                .chars()
                .filter(|c| !c.is_whitespace())
                .take(2)
                .collect(),
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
        }
    }
    pub fn status_label(&self) -> &'static str {
        match self.status {
            Status::Disconnected => "Disconnected",
            Status::Connecting => "Connecting…",
            Status::Connected => "Connected · TCP voice",
        }
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
        handle: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Write a message…")
                .rows(3)
                .submit_on_enter(true)
        });
        let subscription =
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event, window, cx| match event {
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.send_message(window, cx),
                    InputEvent::PressEnter {
                        secondary: false,
                        shift: false,
                    } => {
                        this.composer.update(cx, |input, cx| {
                            input.replace_text_in_range(None, "\n", window, cx)
                        });
                    }
                    _ => {}
                },
            );
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
            backend: Backend::new(handle, profile.clone()),
            profile,
            form: None,
            composer,
            scroll: ScrollHandle::new(),
            channel_scroll: ScrollHandle::new(),
            chat_motion: SmoothScroll::default(),
            channel_motion: SmoothScroll::default(),
            scroll_frame_pending: false,
            error: None,
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
        desktop
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
            })
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
                InputState::new(window, cx)
                    .placeholder("Server name")
                    .default_value(name)
            }),
            host: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Hostname or IP address")
                    .default_value(host)
            }),
            port: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("64738")
                    .default_value(port)
            }),
            username: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Your Mumble username")
                    .default_value(username)
            }),
            password: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Optional; kept only for this connection")
                    .masked(true)
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
            self.error = Some("Message exceeds 64 KiB.".into());
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
                } => {
                    if let Some(index) = self
                        .servers
                        .iter()
                        .position(|entry| entry.id == server && entry.generation == generation)
                    {
                        self.disconnect(index);
                        self.servers[index].notice = Some(reason);
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
                server.own_session = Some(session);
                self.passwords.remove(&server.id);
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
                    channel.name = update.name;
                    channel.parent = update.parent;
                    channel.position = update.position;
                    channel.topic = "Channel chat · select Join voice to move here".into();
                } else {
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
                    });
                }
                server.channels.sort_by(|a, b| {
                    a.position
                        .cmp(&b.position)
                        .then_with(|| a.name.cmp(&b.name))
                });
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
    channel
        .messages
        .sort_by_key(|message| message.id.unwrap_or(i64::MAX));
}
