// SPDX-License-Identifier: EUPL-1.2
//! The SDK in the browser: a `Platform` whose connections are WebSockets to
//! the web relay, each carrying a [`Session`] (TLS here), and `WebClient`, the
//! wasm-bindgen API the Svelte client uses. Commands are methods; what changed
//! comes back from `poll()` as JSON, after the `on_change` callback says so.
//! A Web Worker can host it unchanged (no DOM is used).

use crate::{
    session::{Event, Login, Session},
    snapshot,
};
use nuschel_client::{
    Client, HistoryTicket, Update,
    input::Input,
    names::Preferences,
    platform::{CacheKey, ChangeRequest, Platform, RenderKey, Request},
    timeline::Page,
};
use nuschel_core::{SearchCursor, audio::Sound, bookmark::ServerBookmark};
use nuschel_protocol::tunnel;
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
        _: String,
        _: String,
        _: String,
        _: HistoryTicket,
        _: Page,
    ) -> Result<(), String> {
        Err(NOT_YET.into())
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
            self.client.connect(index, None, false);
        }
    }

    pub fn disconnect(&mut self, index: usize) {
        if index < self.client.servers.len() {
            self.client.disconnect(index);
        }
    }

    /// Trust the certificate the user saw (its SHA-256) and connect again.
    pub fn accept_certificate(&mut self, index: usize, sha256: String) {
        let Some(server) = self.client.servers.get_mut(index) else {
            return;
        };
        server.bookmark.certificate_sha256 = Some(sha256.to_ascii_lowercase());
        server.certificate = None;
        self.client.connect(index, None, false);
    }

    pub fn join(&mut self, index: usize, channel: u32) {
        if index < self.client.servers.len() {
            self.client.join(index, channel);
        }
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
