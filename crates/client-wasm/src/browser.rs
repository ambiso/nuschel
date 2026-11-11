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
    wake: RefCell<Option<js_sys::Function>>,
    storage: RefCell<Storage>,
    renderer: ContentRenderer,
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
        for event in events {
            let server = server.to_owned();
            inputs.push_back(match event {
                Event::Opened { .. } => continue,
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
                                    "direct:{generation}:{id}:{}",
                                    peer.as_ref().map(|peer| peer.instance).unwrap_or(0)
                                )
                            }),
                        TextTarget::DirectConversation(_) => continue,
                    };
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
        drop(inputs);
        self.wake();
    }
}

struct BrowserPlatform(Rc<Shared>);

const NOT_YET: &str = "Not in the web client yet.";

impl Platform for BrowserPlatform {
    fn connect(
        &self,
        bookmark: ServerBookmark,
        password: Option<String>,
        _deafened: bool,
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

    fn request(&self, server: &str, request: Request) -> Result<(), String> {
        let connections = self.0.connections.borrow();
        let connection = connections.get(server).ok_or("Not connected.")?;
        match request {
            Request::Join(channel) => connection.session.borrow_mut().join(channel),
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
    pub fn new(relay: String) -> WebClient {
        let shared = Rc::new(Shared {
            relay,
            inputs: RefCell::new(VecDeque::new()),
            connections: RefCell::new(HashMap::new()),
            generation: Cell::new(0),
            wake: RefCell::new(None),
            storage: RefCell::new(Storage::default()),
            renderer: ContentRenderer::default(),
        });
        let client = Client::new(
            Box::new(BrowserPlatform(shared.clone())),
            Vec::new(),
            Preferences::default(),
        );
        WebClient { client, shared }
    }

    /// Called whenever something arrived; then call `poll()`.
    pub fn on_change(&self, callback: js_sys::Function) {
        *self.shared.wake.borrow_mut() = Some(callback);
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
        let bookmark = snapshot::bookmark(&name, &host, port, &username, pin)?;
        if let Some(index) = self.client.server_index(&bookmark.id) {
            self.client.replace_server(index, bookmark);
            return Ok(index);
        }
        Ok(self.client.add_server(bookmark))
    }

    pub fn connect(&mut self, index: usize) {
        if index < self.client.servers.len() {
            self.client.connect(index, None, true);
        }
    }

    pub fn disconnect(&mut self, index: usize) {
        if index < self.client.servers.len() {
            self.client.disconnect(index);
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

    pub fn join(&mut self, index: usize, channel: u32) {
        if index < self.client.servers.len() {
            self.client.join(index, channel);
        }
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

    pub fn bookmarks(&self) -> String {
        serde_json::to_string(
            &self
                .client
                .servers
                .iter()
                .map(|server| &server.bookmark)
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
    pub fn tick(&mut self, now_ms: f64) {
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
        self.client.set_time(now_ms.max(0.0) as u64);
    }

    /// Apply what arrived; what changed, as JSON (`[{"kind": …}]`).
    pub fn poll(&mut self) -> String {
        loop {
            let input = self.shared.inputs.borrow_mut().pop_front();
            let Some(input) = input else { break };
            self.client.handle(input);
        }
        self.client.settle();
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
        serde_json::to_string(&snapshot::servers(&self.client.servers))
            .unwrap_or_else(|_| "[]".into())
    }
}
