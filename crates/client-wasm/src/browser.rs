// SPDX-License-Identifier: EUPL-1.2
//! The SDK in the browser: a `Platform` whose connections are WebSockets to
//! the web relay, each carrying a [`Session`] (TLS here), and `WebClient`, the
//! wasm-bindgen API the Svelte client uses. Commands are methods; what changed
//! comes back from `poll()` as JSON, after the `on_change` callback says so.
//! A Web Worker can host it unchanged (no DOM is used).

use crate::{
    session::{Event, Login, Session},
    snapshot,
    storage::Storage,
    voice::{self, Route},
};
use nuschl_client::{
    Client, HistoryTicket, Update,
    input::Input,
    names::Preferences,
    platform::{CacheKey, ChangeRequest, Platform, RenderKey, Request},
    timeline::Page,
};
use nuschl_client::{model::TextTarget, timeline::IncomingMessage};
use nuschl_content::ContentRenderer;
use nuschl_core::{
    ChatMessage, ContentBlock, MessageDocument, SearchCursor, audio::Sound,
    bookmark::ServerBookmark,
};
use nuschl_protocol::tunnel;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    rc::Rc,
};
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{BinaryType, CloseEvent, MessageEvent, WebSocket};

type Handler<T> = Closure<dyn FnMut(T)>;

struct Connection {
    generation: u64,
    socket: WebSocket,
    session: Rc<RefCell<Session>>,
    _handlers: (
        Handler<web_sys::Event>,
        Handler<MessageEvent>,
        Handler<CloseEvent>,
    ),
}

struct Shared {
    relay: String,
    inputs: RefCell<VecDeque<Input>>,
    connections: RefCell<HashMap<String, Connection>>,
    generation: Cell<u64>,
    anonymous_namespace: String,
    wake: RefCell<Option<js_sys::Function>>,
    audio_wake: RefCell<Option<js_sys::Function>>,
    talking: RefCell<HashMap<(String, u64, u32), f64>>,
    storage: RefCell<Storage>,
    renderer: ContentRenderer,
    voice: RefCell<Route>,
    voice_packets: RefCell<Vec<voice::Incoming>>,
}

impl Shared {
    fn wake(&self) {
        if let Some(callback) = self.wake.borrow().as_ref() {
            let _ = callback.call0(&JsValue::NULL);
        }
    }

    /// Send what the session queued, and report what it said.
    fn pump(&self, server: &str, generation: u64, socket: &WebSocket, session: &RefCell<Session>) {
        let (outgoing, events) = {
            let mut session = session.borrow_mut();
            // A timer can run while DNS/TLS/WebSocket setup is still pending.
            // Keep the initial Open (and any later queued records) until the
            // transport is actually writable instead of silently dropping it.
            let outgoing = if socket.ready_state() == WebSocket::OPEN {
                session.take_outgoing()
            } else {
                Vec::new()
            };
            (outgoing, session.take_events())
        };
        if socket.ready_state() == WebSocket::OPEN {
            for message in outgoing {
                let _ = socket.send_with_u8_array(&message);
            }
        }
        if events.is_empty() {
            return;
        }
        let mut inputs = self.inputs.borrow_mut();
        let old_inputs = inputs.len();
        let mut audio_changed = false;
        let mut storage_changed = false;
        for event in events {
            let server = server.to_owned();
            inputs.push_back(match event {
                Event::Opened { .. } => continue,
                Event::Voice(packet) => {
                    if let Some(context) = self.voice.borrow().receive(&server, generation) {
                        let mut packets = self.voice_packets.borrow_mut();
                        if packets.len() < 64 {
                            let key = (server.clone(), generation, packet.sender);
                            let mut talking = self.talking.borrow_mut();
                            let changed = if packet.end {
                                talking.remove(&key).is_some()
                            } else if talking.len() < 64 || talking.contains_key(&key) {
                                talking.insert(key, js_sys::Date::now() + 1000.0).is_none()
                            } else {
                                continue;
                            };
                            if changed {
                                inputs.push_back(Input::Talking {
                                    server: server.clone(),
                                    generation,
                                    session: packet.sender,
                                    talking: !packet.end,
                                });
                            }
                            audio_changed = true;
                            packets.push(voice::Incoming {
                                context,
                                speaker: packet.sender,
                                sequence: packet.sequence.to_string(),
                                opus: packet.opus,
                                end: packet.end,
                                volume: packet.volume,
                            });
                        }
                    }
                    continue;
                }
                Event::Server(event) => Input::Server {
                    server,
                    generation,
                    event,
                },
                Event::Certificate { sha256, changed } => Input::Certificate {
                    server,
                    generation,
                    sha256,
                    changed,
                    reason: "No certificate authority vouches for this server.".into(),
                },
                Event::Text {
                    target,
                    peer,
                    author,
                    body,
                    html,
                    outgoing,
                } => {
                    let Some(identity) = session.borrow().history_identity() else {
                        continue;
                    };
                    let source = if html {
                        self.renderer
                            .legacy_plain_text(&body)
                            .unwrap_or_else(|_| "[Message could not be rendered]".into())
                    } else {
                        body
                    };
                    let document =
                        self.renderer
                            .render(&source)
                            .unwrap_or_else(|_| MessageDocument {
                                blocks: vec![ContentBlock::text("[Message could not be rendered]")],
                            });
                    let storage_key = match &target {
                        TextTarget::Channel(id) => id.to_string(),
                        TextTarget::Direct(id) => peer
                            .as_ref()
                            .and_then(|peer| peer.certificate_hash.as_ref())
                            .filter(|hash| !hash.is_empty())
                            .map(|hash| format!("direct:{hash}"))
                            .unwrap_or_else(|| {
                                format!(
                                    "direct:{}:{generation}:{id}:{}",
                                    self.anonymous_namespace,
                                    peer.as_ref().map(|peer| peer.instance).unwrap_or(0)
                                )
                            }),
                        TextTarget::DirectConversation(_) => continue,
                    };
                    // A successful append queues browser I/O before it can produce
                    // an Input. It still needs a wake to drain storage_requests.
                    storage_changed = true;
                    let message = IncomingMessage::plain(
                        None,
                        ChatMessage {
                            server: identity,
                            channel: storage_key.clone(),
                            author,
                            observed_at_ms: js_sys::Date::now() as i64,
                            body: source,
                        },
                        document,
                        outgoing,
                    );
                    if let Err(reason) = self.storage.borrow_mut().append(
                        server.clone(),
                        generation,
                        target.clone(),
                        peer.clone(),
                        message.clone(),
                    ) {
                        inputs.push_back(Input::HistoryError(reason));
                        inputs.push_back(Input::Message {
                            server,
                            generation,
                            target,
                            storage_key,
                            peer,
                            message,
                        });
                    }
                    continue;
                }
                Event::Sent { target, body } => Input::Sent {
                    server,
                    generation,
                    target,
                    body,
                },
                Event::Closed { reason } => {
                    let _ = socket.close();
                    Input::Disconnected {
                        server,
                        generation,
                        reason,
                        reject: None,
                    }
                }
            });
        }
        let changed = storage_changed || old_inputs != inputs.len();
        drop(inputs);
        if changed {
            self.wake();
        }
        if audio_changed {
            if let Some(callback) = self.audio_wake.borrow().as_ref() {
                let _ = callback.call0(&JsValue::NULL);
            }
        }
    }
}

struct BrowserPlatform(Rc<Shared>);

const NOT_YET: &str = "Not in the web client yet.";

impl Platform for BrowserPlatform {
    fn connect(
        &self,
        bookmark: ServerBookmark,
        password: Option<String>,
        deafened: bool,
    ) -> Result<u64, String> {
        self.disconnect(&bookmark.id);
        let shared = &self.0;
        let generation = shared.generation.get() + 1;
        shared.generation.set(generation);
        let session = Rc::new(RefCell::new(Session::new(Login {
            host: bookmark.host.clone(),
            port: bookmark.port,
            username: bookmark.username.clone(),
            password,
            tokens: bookmark.tokens.clone(),
            pin: bookmark.certificate_sha256.clone(),
        })?));
        session.borrow_mut().set_self(true, deafened);
        let socket = WebSocket::new_with_str(&shared.relay, tunnel::SUBPROTOCOL)
            .map_err(|_| "The relay's address is not valid.".to_string())?;
        socket.set_binary_type(BinaryType::Arraybuffer);
        let server = bookmark.id.clone();
        let on_open = {
            let (shared, server, socket, session) = (
                shared.clone(),
                server.clone(),
                socket.clone(),
                session.clone(),
            );
            Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                shared.pump(&server, generation, &socket, &session);
            })
        };
        let on_message = {
            let (shared, server, socket, session) = (
                shared.clone(),
                server.clone(),
                socket.clone(),
                session.clone(),
            );
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                if let Ok(buffer) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
                    // Check before copying a relay-controlled buffer into WASM.
                    if buffer.byte_length() as usize > tunnel::MAX_FRAME {
                        session.borrow_mut().disconnected();
                    } else {
                        let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                        session.borrow_mut().receive(&bytes);
                    }
                } else {
                    session.borrow_mut().disconnected();
                }
                shared.pump(&server, generation, &socket, &session);
            })
        };
        let on_close = {
            let (shared, server, socket, session) = (
                shared.clone(),
                server.clone(),
                socket.clone(),
                session.clone(),
            );
            Closure::<dyn FnMut(CloseEvent)>::new(move |_| {
                session.borrow_mut().disconnected();
                shared.pump(&server, generation, &socket, &session);
            })
        };
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        shared.connections.borrow_mut().insert(
            server,
            Connection {
                generation,
                socket,
                session,
                _handlers: (on_open, on_message, on_close),
            },
        );
        Ok(generation)
    }

    fn disconnect(&self, server: &str) {
        if let Some(connection) = self.0.connections.borrow_mut().remove(server) {
            connection.socket.set_onopen(None);
            connection.socket.set_onmessage(None);
            connection.socket.set_onclose(None);
            let _ = connection.socket.close();
        }
    }

    fn request_metadata(
        &self,
        server: &str,
        authority: nuschl_core::model::MetadataAuthority,
        request: Request,
    ) -> Result<(), String> {
        // Browser transport keeps its current source-ticket model; this check
        // rejects a replaced connection without claiming native actor parity.
        if self
            .0
            .connections
            .borrow()
            .get(server)
            .is_none_or(|connection| connection.generation != authority.generation)
        {
            return Err("Metadata belongs to an older connection.".into());
        }
        self.request(server, request)
    }
    fn request(&self, server: &str, request: Request) -> Result<(), String> {
        let connections = self.0.connections.borrow();
        let connection = connections.get(server).ok_or("Not connected.")?;
        match request {
            Request::Join(channel) => connection.session.borrow_mut().join(channel),
            Request::SetSelf { muted, deafened } => {
                connection.session.borrow_mut().set_self(muted, deafened)
            }
            Request::SendText {
                target,
                body,
                expected_peer,
            } => connection
                .session
                .borrow_mut()
                .send_text(target, body, expected_peer)?,
            Request::RequestPermissions(channel) => {
                connection.session.borrow_mut().request_permissions(channel)
            }
            _ => return Err(NOT_YET.into()),
        }
        let (shared, generation) = (self.0.clone(), connection.generation);
        let (socket, session) = (connection.socket.clone(), connection.session.clone());
        drop(connections);
        shared.pump(server, generation, &socket, &session);
        Ok(())
    }

    fn local_history(
        &self,
        server: String,
        identity: String,
        channel: String,
        ticket: HistoryTicket,
        page: Page,
    ) -> Result<(), String> {
        let generation = self
            .0
            .connections
            .borrow()
            .get(&server)
            .map(|connection| connection.generation)
            .unwrap_or(0);
        self.0
            .storage
            .borrow_mut()
            .page(server, generation, identity, channel, ticket, page)
    }

    fn server_history(
        &self,
        _: String,
        _: [u8; 16],
        _: HistoryTicket,
        _: Option<CacheKey>,
        _: Page,
    ) -> Result<(), String> {
        Err(NOT_YET.into())
    }

    fn change_message(&self, _: ChangeRequest) -> Result<(), String> {
        Err(NOT_YET.into())
    }

    fn render_html(&self, _: String, _: u64, _: RenderKey, _: String) {}

    fn decode_avatar(&self, _: String, _: u64, _: String, _: Vec<u8>) {}

    fn play_sound(&self, _: &str, _: Sound) {}

    fn chime(&self, _: Sound) {}

    fn search(
        &self,
        _: u64,
        _: (Vec<String>, Option<String>),
        _: String,
        _: Option<SearchCursor>,
    ) -> Result<(), String> {
        Err(NOT_YET.into())
    }
}

/// The client SDK for JavaScript.
#[wasm_bindgen]
pub struct WebClient {
    client: Client,
    shared: Rc<Shared>,
}

#[wasm_bindgen]
impl WebClient {
    /// A client whose connections go through the relay at `relay`
    /// (`wss://<web host>/relay`).
    #[wasm_bindgen(constructor)]
    pub fn new(relay: String) -> Result<WebClient, JsValue> {
        // Anonymous presence counters are process-local. Never attach persisted
        // private history to a new peer just because counters repeat after reload.
        let mut namespace = [0; 16];
        rustls::crypto::ring::default_provider()
            .secure_random
            .fill(&mut namespace)
            .map_err(|_| {
                JsValue::from_str(
                    "Secure randomness is unavailable; the browser client cannot start.",
                )
            })?;
        let shared = Rc::new(Shared {
            relay,
            inputs: RefCell::new(VecDeque::new()),
            connections: RefCell::new(HashMap::new()),
            generation: Cell::new(0),
            anonymous_namespace: crate::session::hex(&namespace),
            wake: RefCell::new(None),
            audio_wake: RefCell::new(None),
            talking: RefCell::new(HashMap::new()),
            storage: RefCell::new(Storage::default()),
            renderer: ContentRenderer::default(),
            voice: RefCell::new(Route::default()),
            voice_packets: RefCell::new(Vec::new()),
        });
        let client = Client::new(
            Box::new(BrowserPlatform(shared.clone())),
            Vec::new(),
            Preferences::default(),
        );
        Ok(WebClient { client, shared })
    }

    /// Called whenever something arrived; then call `poll()`.
    pub fn on_change(&self, callback: js_sys::Function) {
        *self.shared.wake.borrow_mut() = Some(callback);
    }

    /// Encoded packets have their own notification; 20ms media does not redraw timelines.
    pub fn on_audio(&self, callback: js_sys::Function) {
        *self.shared.audio_wake.borrow_mut() = Some(callback);
    }

    /// Add a server; its index.
    pub fn add_server(
        &mut self,
        name: String,
        host: String,
        port: u16,
        username: String,
        pin: Option<String>,
    ) -> Result<usize, String> {
        let mut bookmark = snapshot::bookmark(&name, &host, port, &username, pin)?;
        if let Some(index) = self.client.server_index(&bookmark.id) {
            let saved = &self.client.servers[index].bookmark;
            if bookmark.certificate_sha256.is_none() {
                bookmark
                    .certificate_sha256
                    .clone_from(&saved.certificate_sha256);
            }
            if bookmark.certificate_sha256 == saved.certificate_sha256 {
                bookmark.cached_channels.clone_from(&saved.cached_channels);
            }
            self.client.replace_server(index, bookmark);
            return Ok(index);
        }
        Ok(self.client.add_server(bookmark))
    }

    /// Replace one bookmark explicitly; an address change never leaves a hidden duplicate.
    pub fn edit_server(
        &mut self,
        index: usize,
        name: String,
        host: String,
        port: u16,
        username: String,
        pin: Option<String>,
    ) -> Result<usize, String> {
        if index >= self.client.servers.len() {
            return Err("Unknown server.".into());
        }
        let bookmark = snapshot::bookmark(&name, &host, port, &username, pin)?;
        if self
            .client
            .server_index(&bookmark.id)
            .is_some_and(|other| other != index)
        {
            return Err("This server and username are already saved.".into());
        }
        self.client.replace_server(index, bookmark);
        self.reconcile_voice();
        Ok(index)
    }

    pub fn connect(&mut self, index: usize) {
        if index < self.client.servers.len() {
            self.client.connect(index, None, true);
        }
    }

    pub fn disconnect(&mut self, index: usize) {
        if index < self.client.servers.len() {
            self.client.disconnect(index);
            self.reconcile_voice();
        }
    }

    /// Trust the certificate the user saw (its SHA-256) and connect again.
    pub fn accept_certificate(&mut self, index: usize, sha256: String) {
        if self
            .client
            .servers
            .get(index)
            .and_then(|server| server.certificate.as_ref())
            .is_some_and(|prompt| prompt.sha256.eq_ignore_ascii_case(&sha256))
        {
            self.client.trust_certificate(index, true);
        }
    }

    /// Joining never silently transfers a live microphone to another destination.
    pub fn join(&mut self, index: usize, channel: u32) -> Result<(), String> {
        let server = self.client.servers.get(index).ok_or("Unknown server.")?;
        if server.status != nuschl_client::server::Status::Connected
            || !server.has_mumble_voice()
            || !server
                .channels
                .iter()
                .any(|entry| entry.id == channel && matches!(entry.target, TextTarget::Channel(_)))
        {
            return Err("Connect and choose a voice channel first.".into());
        }
        let (id, generation) = (server.id.clone(), server.generation);
        self.shared
            .voice
            .borrow_mut()
            .select(id, generation, channel);
        self.shared.voice_packets.borrow_mut().clear();
        // Shared SDK owns multi-server consent. Mute before moving its route.
        for candidate in 0..self.client.servers.len() {
            if !self.client.servers[candidate].muted {
                self.client.set_mute(candidate, false);
            }
        }
        self.client.join(index, channel);
        if self.client.servers[index].deafened {
            self.client.set_mute(index, true);
        }
        self.reconcile_voice();
        Ok(())
    }

    pub fn toggle_mute(&mut self, index: usize) -> Result<(), String> {
        self.toggle_audio(index, false)
    }
    pub fn toggle_deaf(&mut self, index: usize) -> Result<(), String> {
        self.toggle_audio(index, true)
    }

    pub fn audio_ready(&mut self, raw: String) -> Result<(), String> {
        if raw.len() > 2048 {
            return Err("Invalid audio readiness.".into());
        }
        self.reconcile_voice();
        let ready = serde_json::from_str(&raw).map_err(|_| "Invalid audio readiness.")?;
        self.shared.voice.borrow_mut().ready(ready)
    }

    pub fn voice_packet(&mut self, raw: String, opus: &[u8]) -> Result<(), String> {
        if raw.len() > 2048 {
            return Err("Invalid voice context.".into());
        }
        self.reconcile_voice();
        let packet: voice::Outgoing =
            serde_json::from_str(&raw).map_err(|_| "Invalid voice context.")?;
        let sequence = self.shared.voice.borrow_mut().admit(&packet, opus.len())?;
        let connections = self.shared.connections.borrow();
        let connection = connections
            .get(&packet.context.server_id)
            .ok_or("Disconnected voice route.")?;
        if connection.generation.to_string() != packet.context.connection_generation {
            return Err("Stale voice connection.".into());
        }
        if connection.socket.ready_state() != WebSocket::OPEN
            || connection.socket.buffered_amount() > 128 * 1024
        {
            return Err("Voice transport is busy.".into());
        }
        connection
            .session
            .borrow_mut()
            .send_voice(sequence, opus.to_vec(), packet.end)?;
        if let Some(own) = connection.session.borrow().own_session() {
            let key = (packet.context.server_id.clone(), connection.generation, own);
            let mut talking = self.shared.talking.borrow_mut();
            let changed = if packet.end {
                talking.remove(&key).is_some()
            } else {
                talking.insert(key, js_sys::Date::now() + 1000.0).is_none()
            };
            if changed {
                self.shared.inputs.borrow_mut().push_back(Input::Talking {
                    server: packet.context.server_id.clone(),
                    generation: connection.generation,
                    session: own,
                    talking: !packet.end,
                });
                self.shared.wake();
            }
        }
        self.shared.pump(
            &packet.context.server_id,
            connection.generation,
            &connection.socket,
            &connection.session,
        );
        Ok(())
    }

    pub fn voice(&mut self) -> String {
        self.reconcile_voice();
        serde_json::to_string(&self.shared.voice.borrow().view()).unwrap_or_else(|_| "null".into())
    }

    pub fn voice_packets(&mut self) -> String {
        self.reconcile_voice();
        let view = self.shared.voice.borrow().view();
        let packets: Vec<_> = self
            .shared
            .voice_packets
            .borrow_mut()
            .drain(..)
            .filter(|packet| view.as_ref().is_some_and(|v| v == &packet.context))
            .collect();
        serde_json::to_string(&packets).unwrap_or_else(|_| "[]".into())
    }

    /// Stop media while retaining the connection and channel presence for chat.
    pub fn stop_audio(&mut self) {
        if let Some(view) = self.shared.voice.borrow().view() {
            if let Some(index) = self.client.server_index(&view.server_id) {
                if !self.client.servers[index].deafened {
                    self.client.set_mute(index, true);
                }
            }
        }
        self.shared.voice.borrow_mut().clear();
        self.shared.voice_packets.borrow_mut().clear();
    }

    pub fn remove_server(&mut self, index: usize) {
        if let Some(server) = self.client.servers.get(index) {
            let id = server.id.clone();
            self.client.remove_server(&id);
            self.reconcile_voice();
        }
    }

    pub fn dismiss_notice(&mut self, index: usize) {
        if let Some(server) = self.client.servers.get_mut(index) {
            server.dismiss_notice();
        }
    }

    pub fn connect_with_password(&mut self, index: usize, password: String) -> Result<(), String> {
        if password.len() > 1024 || index >= self.client.servers.len() {
            return Err("Invalid password or server.".into());
        }
        self.client.connect(index, Some(password), true);
        self.reconcile_voice();
        Ok(())
    }

    /// Single-click navigation uses the shared SDK's selected timeline.
    pub fn open(&mut self, index: usize, channel: u32) -> Result<bool, String> {
        self.client.open(index, channel)
    }

    pub fn browse_server(&mut self, index: usize) -> Result<bool, String> {
        let Some(server) = self.client.servers.get(index) else {
            return Ok(false);
        };
        let channel = self
            .client
            .navigation
            .remembered_channel(&server.id)
            .unwrap_or(0);
        self.client.open(index, channel)
    }

    pub fn open_direct(&mut self, index: usize, session: u32) -> Result<bool, String> {
        if index >= self.client.servers.len() {
            return Ok(false);
        }
        let Some(channel) = self.client.open_direct(index, session) else {
            return Ok(false);
        };
        self.client.open(index, channel)
    }

    pub fn send_text(&mut self, body: String) -> Result<(), String> {
        self.client
            .send_text(body)
            .map_err(|_| "Message was not sent; check the server status and recipient.".into())
    }

    pub fn older(&mut self) -> Result<(), String> {
        let before = self
            .client
            .selected_indices()
            .and_then(|(server, channel)| {
                self.client.servers[server].channels[channel]
                    .timeline
                    .next_before
            });
        if let Some(before) = before {
            self.client.load_history(Some(before))
        } else {
            Ok(())
        }
    }

    /// Storage requests contain opaque ticket IDs, never forgeable HistoryTickets.
    pub fn storage_requests(&mut self) -> String {
        serde_json::to_string(&self.shared.storage.borrow_mut().take())
            .unwrap_or_else(|_| "[]".into())
    }

    pub fn storage_complete(&mut self, ticket: String, response: String) {
        let inputs = self.shared.storage.borrow_mut().complete(
            &ticket,
            &response,
            |id, generation, identity| {
                self.client.servers.iter().any(|server| {
                    server.id == id
                        && (generation == 0 || server.generation == generation)
                        && server.history_identity().as_deref() == Some(identity)
                })
            },
            |source| {
                self.shared
                    .renderer
                    .render(source)
                    .unwrap_or_else(|_| MessageDocument {
                        blocks: vec![ContentBlock::text("[Message could not be rendered]")],
                    })
            },
        );
        self.shared.inputs.borrow_mut().extend(inputs);
    }

    pub fn timeline(&self) -> String {
        serde_json::to_string(&snapshot::timeline(&self.client)).unwrap_or_else(|_| "null".into())
    }

    /// IDs, never display positions: live connection indices stay unchanged.
    pub fn set_server_order(&mut self, raw: String) -> Result<bool, String> {
        if raw.len() > 64 * 1024 {
            return Err("Server order exceeds the browser limit.".into());
        }
        let ids: Vec<String> = serde_json::from_str(&raw)
            .map_err(|_| "Server order must be a list of saved server IDs.".to_owned())?;
        self.client.set_server_order(&ids)
    }

    pub fn bookmarks(&self) -> String {
        serde_json::to_string(
            &self
                .client
                .ordered_servers()
                .map(|(_, server)| &server.bookmark)
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".into())
    }

    pub fn restore_bookmarks(&mut self, raw: String) -> Result<(), String> {
        if raw.len() > 1024 * 1024 {
            return Err("Saved servers exceed the browser limit.".into());
        }
        let saved: Vec<ServerBookmark> =
            serde_json::from_str(&raw).map_err(|_| "Saved servers could not be read.")?;
        if saved.len() > 128 {
            return Err("Too many saved servers.".into());
        }
        for old in saved {
            let mut bookmark = snapshot::bookmark(
                &old.name,
                &old.host,
                old.port,
                &old.username,
                old.certificate_sha256,
            )?;
            bookmark.cached_channels = old.cached_channels.into_iter().take(10_000).collect();
            if self.client.server_index(&bookmark.id).is_none() {
                self.client.add_server(bookmark);
            }
        }
        Ok(())
    }

    /// Keep connections alive; call every few seconds with a monotonic clock.
    pub fn tick(&mut self, now_ms: f64) -> bool {
        let connections: Vec<(String, u64, WebSocket, Rc<RefCell<Session>>)> = self
            .shared
            .connections
            .borrow()
            .iter()
            .map(|(server, connection)| {
                (
                    server.clone(),
                    connection.generation,
                    connection.socket.clone(),
                    connection.session.clone(),
                )
            })
            .collect();
        for (server, generation, socket, session) in connections {
            session.borrow_mut().tick(now_ms);
            self.shared.pump(&server, generation, &socket, &session);
        }
        let changed = self.client.set_time(now_ms.max(0.0) as u64);
        let now = js_sys::Date::now();
        let mut expired = false;
        self.shared
            .talking
            .borrow_mut()
            .retain(|(server, generation, session), until| {
                if *until > now {
                    return true;
                }
                expired = true;
                self.shared.inputs.borrow_mut().push_back(Input::Talking {
                    server: server.clone(),
                    generation: *generation,
                    session: *session,
                    talking: false,
                });
                false
            });
        changed || expired
    }

    /// Apply what arrived; what changed, as JSON (`[{"kind": …}]`).
    pub fn poll(&mut self) -> String {
        loop {
            let input = self.shared.inputs.borrow_mut().pop_front();
            let Some(input) = input else { break };
            self.client.handle(input);
        }
        self.client.settle();
        self.reconcile_voice();
        if self.client.navigation.selection().is_none() {
            if let Some(index) = self
                .client
                .servers
                .iter()
                .position(|server| server.status == nuschl_client::server::Status::Connected)
            {
                let _ = self.client.open(index, 0);
            }
        }
        let updates: Vec<snapshot::UpdateView> = self
            .client
            .take_updates()
            .iter()
            .filter_map(|update: &Update| snapshot::update(update))
            .collect();
        serde_json::to_string(&updates).unwrap_or_else(|_| "[]".into())
    }

    /// Every server with its status, channels and users, as JSON.
    pub fn servers(&self) -> String {
        serde_json::to_string(&snapshot::ordered_servers(&self.client))
            .unwrap_or_else(|_| "[]".into())
    }
}

// Non-exported helpers keep browser devices outside the SDK/session boundary.
impl WebClient {
    fn reconcile_voice(&mut self) {
        let view = self.shared.voice.borrow().view();
        let Some(view) = view else {
            return;
        };
        let Some(server) = self.client.servers.iter().find(|s| {
            s.id == view.server_id
                && s.generation.to_string() == view.connection_generation
                && s.status == nuschl_client::server::Status::Connected
                && s.has_mumble_voice()
                && s.channels
                    .iter()
                    .any(|c| c.id == view.channel_id && matches!(c.target, TextTarget::Channel(_)))
        }) else {
            self.shared.voice.borrow_mut().clear();
            self.shared.voice_packets.borrow_mut().clear();
            return;
        };
        let (muted, deafened) = self
            .shared
            .connections
            .borrow()
            .get(&server.id)
            .map(|c| c.session.borrow().voice_policy())
            .unwrap_or((true, true));
        let bitrate = self
            .shared
            .connections
            .borrow()
            .get(&server.id)
            .map(|connection| connection.session.borrow().voice_bitrate())
            .unwrap_or(0);
        self.shared.voice.borrow_mut().set_bitrate(bitrate);
        self.shared.voice.borrow_mut().policy(
            server.voice_channel() == Some(view.channel_id),
            muted || server.muted,
            deafened || server.deafened,
        );
    }

    fn toggle_audio(&mut self, index: usize, deafen: bool) -> Result<(), String> {
        let server = self.client.servers.get(index).ok_or("Unknown server.")?;
        if !self.shared.voice.borrow().view().is_some_and(|v| {
            v.server_id == server.id
                && v.connection_generation == server.generation.to_string()
                && v.joined
        }) {
            return Err("Join this server's voice channel first.".into());
        }
        // Revoke readiness before any asynchronous acknowledgement can arrive.
        self.shared.voice.borrow_mut().policy(true, true, true);
        self.shared.voice_packets.borrow_mut().clear();
        if self.client.set_mute(index, deafen).is_some() {
            return Err("Move voice to this server before unmuting.".into());
        }
        self.reconcile_voice();
        Ok(())
    }
}
