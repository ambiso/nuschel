// SPDX-License-Identifier: EUPL-1.2
//! The SDK in the browser: a `Platform` whose connections are WebSockets to
//! the web relay, each carrying a [`Session`] (TLS here), and `WebClient`, the
//! wasm-bindgen API the Svelte client uses. Commands are methods; what changed
//! comes back from `poll()` as JSON, after the `on_change` callback says so.
//! A Web Worker can host it unchanged (no DOM is used).

use crate::{
    actions::{Catalog, ServerRef},
    content_broker::{ImageAdmission, ImageAdmissionState, ImageBegin, ImageSend, SourceMode},
    content_jobs::{Jobs, Origin, RequestError},
    image::{Destination, Images},
    session::{Event, Login, Session},
    snapshot,
    storage::{
        HealthEvent, HistoryHealth, RawCompletion, SAVE_FAILED, SaveOutcome, SaveResult,
        SourceProvenance, Storage, pending_document,
    },
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
    deliveries: RefCell<VecDeque<RawCompletion>>,
    /// History-save status of each server's live connection.
    history: RefCell<HistoryHealth>,
    jobs: RefCell<Jobs>,
    images: RefCell<Images>,
    welcomes: RefCell<crate::welcome::Welcomes>,
    voice: RefCell<Route>,
    voice_packets: RefCell<Vec<voice::Incoming>>,
}

impl Shared {
    fn wake(&self) {
        if let Some(callback) = self.wake.borrow().as_ref() {
            let _ = callback.call0(&JsValue::NULL);
        }
    }

    /// Whether `generation` is still this server's live transport connection.
    fn live(&self, server: &str, generation: u64) -> bool {
        self.connections
            .borrow()
            .get(server)
            .is_some_and(|connection| connection.generation == generation)
    }

    /// Send what the session queued, and report what it said.
    fn pump(
        &self,
        server: &str,
        generation: u64,
        socket: &WebSocket,
        session: &RefCell<Session>,
    ) -> Result<(), String> {
        let (before, outgoing) = if socket.ready_state() == WebSocket::OPEN {
            session.borrow_mut().take_outgoing_batch()
        } else {
            (0, Vec::new())
        };
        let mut admitted = before;
        let mut failure = None;
        if socket.ready_state() == WebSocket::OPEN {
            (admitted, failure) = crate::session::enqueue_outgoing(
                before,
                &outgoing,
                socket.buffered_amount() as usize,
                |message| socket.send_with_u8_array(message).map_err(|_| ()),
            );
        } else if matches!(socket.ready_state(), WebSocket::CLOSING | WebSocket::CLOSED) {
            failure = Some("The browser transport is closed. Keep your draft and reconnect.");
        }
        if failure.is_some() {
            session.borrow_mut().disconnected();
            let _ = socket.close();
        }
        let events = session.borrow_mut().take_events();
        if events.is_empty() {
            return failure.map_or(Ok(()), |reason| Err(reason.into()));
        }
        let mut inputs = self.inputs.borrow_mut();
        let old_inputs = inputs.len();
        let mut audio_changed = false;
        let mut storage_changed = false;
        for event in events {
            if !event.transport_admitted(admitted) {
                continue;
            }
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
                Event::Welcome(source) => {
                    // Old sockets must not replace a newly connected server's cache.
                    if self
                        .connections
                        .borrow()
                        .get(&server)
                        .is_none_or(|c| c.generation != generation)
                    {
                        continue;
                    }
                    let mut document = self
                        .welcomes
                        .borrow_mut()
                        .begin(&server, generation, &source);
                    let revision = self
                        .welcomes
                        .borrow()
                        .view(&server, generation, Some(&source), true)
                        .map(|view| view.token.clone());
                    if let Some(revision) = revision {
                        let refused = self.jobs.borrow_mut().request(
                            Origin::Welcome {
                                server: server.clone(),
                                generation,
                                revision: revision.clone(),
                            },
                            &source,
                            SourceMode::Legacy,
                            now_ms(),
                        );
                        if let Err(error) = refused {
                            self.welcomes
                                .borrow_mut()
                                .fail(&server, generation, &revision, &source);
                            document = error.terminal().unwrap_or_else(|| MessageDocument {
                                blocks: vec![ContentBlock::text(
                                    "[Server welcome processing is busy. Reconnect to retry.]",
                                )],
                            });
                        }
                    }
                    Input::Server {
                        server,
                        generation,
                        event: nuschl_client::input::ServerEvent::Welcome { source, document },
                    }
                }
                Event::Certificate { sha256, changed } => Input::Certificate {
                    server,
                    generation,
                    sha256,
                    changed,
                    reason: "No certificate authority vouches for this server.".into(),
                },
                Event::Text {
                    target,
                    channel_instance,
                    peer,
                    author,
                    body,
                    html,
                    outgoing,
                    transport: _,
                } => {
                    let Some(identity) = session.borrow().history_identity() else {
                        continue;
                    };
                    let mode = if html {
                        SourceMode::Legacy
                    } else {
                        SourceMode::Plain
                    };
                    let source = body;
                    let document = pending_document();
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
                    if let Err(reason) = self.storage.borrow_mut().append_raw(
                        server.clone(),
                        generation,
                        target.clone(),
                        peer.clone(),
                        message.clone(),
                        SourceProvenance {
                            mode,
                            channel_instance,
                        },
                    ) {
                        // Only this live connection's first refusal is a notice,
                        // in its original place; later ones are counted status.
                        if self.live(&server, generation)
                            && self.history.borrow_mut().observe(
                                &server,
                                generation,
                                HealthEvent::Busy,
                            )
                        {
                            inputs.push_back(Input::HistoryError(reason));
                        }
                        self.deliveries.borrow_mut().push_back(RawCompletion {
                            inputs: vec![Input::Message {
                                server,
                                generation,
                                target,
                                storage_key,
                                peer,
                                message,
                            }],
                            live_mode: Some(mode),
                            channel_instance,
                            stored_modes: Default::default(),
                            save: None,
                        });
                    }
                    continue;
                }
                Event::Sent {
                    target,
                    body,
                    transport: _,
                } => Input::Sent {
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
        if audio_changed && let Some(callback) = self.audio_wake.borrow().as_ref() {
            let _ = callback.call0(&JsValue::NULL);
        }
        failure.map_or(Ok(()), |reason| Err(reason.into()))
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
        let shared = &self.0;
        let generation = shared
            .generation
            .get()
            .checked_add(1)
            .ok_or("Browser connection counter exhausted.")?;
        self.disconnect(&bookmark.id);
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
                let _ = shared.pump(&server, generation, &socket, &session);
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
                let _ = shared.pump(&server, generation, &socket, &session);
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
                let _ = shared.pump(&server, generation, &socket, &session);
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
        self.0.welcomes.borrow_mut().remove(server);
        // Reconnect, disconnect, replacement and removal end this connection's
        // status; a late result for it then fails the liveness check.
        self.0.history.borrow_mut().forget(server);
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
        shared.pump(server, generation, &socket, &session)
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
    recovery: VecDeque<(Origin, SourceMode)>,
    recovery_selection: Option<(String, u64, u32, u64)>,
    recovery_until: u64,
    /// The selected lifetime whose rows were last recovered and rebound.
    rekeyed: Option<(String, u64, u32, u64)>,
    catalog: Catalog,
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
            deliveries: RefCell::new(VecDeque::new()),
            history: RefCell::new(HistoryHealth::default()),
            jobs: RefCell::new(Jobs::default()),
            images: RefCell::new(Images::default()),
            welcomes: RefCell::new(crate::welcome::Welcomes::default()),
            voice: RefCell::new(Route::default()),
            voice_packets: RefCell::new(Vec::new()),
        });
        let client = Client::new(
            Box::new(BrowserPlatform(shared.clone())),
            Vec::new(),
            Preferences::default(),
        );
        Ok(WebClient {
            client,
            shared,
            recovery: VecDeque::new(),
            recovery_selection: None,
            recovery_until: 0,
            rekeyed: None,
            catalog: Catalog::default(),
        })
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
            let stamp = self
                .catalog
                .reserve(self.client.servers.len(), Some(index))?;
            self.client.replace_server(index, bookmark);
            self.catalog.commit(index, stamp);
            return Ok(index);
        }
        let stamp = self.catalog.reserve(self.client.servers.len(), None)?;
        let index = self.client.add_server(bookmark);
        self.catalog.commit(index, stamp);
        Ok(index)
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
        let stamp = self
            .catalog
            .reserve(self.client.servers.len(), Some(index))?;
        self.client.replace_server(index, bookmark);
        self.catalog.commit(index, stamp);
        self.reconcile_voice();
        Ok(index)
    }

    pub fn connect(&mut self, index: usize) {
        self.shared.images.borrow_mut().invalidate();
        if index < self.client.servers.len() {
            self.client.connect(index, None, true);
        }
    }

    pub fn disconnect(&mut self, index: usize) {
        self.shared.images.borrow_mut().invalidate();
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

    /// The user's requested voice bitrate (bits per second; the page keeps
    /// it in local storage). Each server's budget can lower it; the voice
    /// view reports requested, effective and why.
    pub fn set_voice_bitrate(&mut self, requested: u32) -> Result<(), String> {
        if !(nuschl_core::bitrate::MIN_REQUESTED_VOICE_BITRATE
            ..=nuschl_core::bitrate::MAX_VOICE_BITRATE)
            .contains(&requested)
        {
            return Err("Unsupported voice bitrate.".into());
        }
        self.shared.voice.borrow_mut().set_requested(requested);
        self.reconcile_voice();
        Ok(())
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
        )?;
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
        if let Some(view) = self.shared.voice.borrow().view()
            && let Some(index) = self.client.server_index(&view.server_id)
            && !self.client.servers[index].deafened
        {
            self.client.set_mute(index, true);
        }
        self.shared.voice.borrow_mut().clear();
        self.shared.voice_packets.borrow_mut().clear();
    }

    pub fn remove_server(&mut self, index: usize) -> Result<(), String> {
        let server = self.client.servers.get(index).ok_or("Unknown server.")?;
        let id = server.id.clone();
        self.catalog.remove(index, self.client.servers.len())?;
        self.shared.images.borrow_mut().invalidate();
        self.client.remove_server(&id);
        self.reconcile_voice();
        Ok(())
    }

    pub fn dismiss_notice(&mut self, index: usize) {
        if let Some(server) = self.client.servers.get_mut(index) {
            server.dismiss_notice();
        }
    }

    /// Dismiss only the displayed welcome from this exact live source/connection.
    pub fn dismiss_welcome(&mut self, index: usize, token: String) -> Result<(), String> {
        let server = self.client.servers.get(index).ok_or("Unknown server.")?;
        if server.status != nuschl_client::server::Status::Connected
            || !self.shared.welcomes.borrow_mut().dismiss(
                &server.id,
                server.generation,
                server.welcome.as_deref(),
                &token,
            )
        {
            return Err("This server welcome has changed.".into());
        }
        Ok(())
    }

    pub fn connect_with_password(&mut self, index: usize, password: String) -> Result<(), String> {
        self.shared.images.borrow_mut().invalidate();
        if password.len() > 1024 || index >= self.client.servers.len() {
            return Err("Invalid password or server.".into());
        }
        self.client.connect(index, Some(password), true);
        self.reconcile_voice();
        Ok(())
    }

    /// Single-click navigation uses the shared SDK's selected timeline.
    pub fn open(&mut self, index: usize, channel: u32) -> Result<bool, String> {
        self.shared.images.borrow_mut().invalidate();
        let result = self.client.open(index, channel);
        if matches!(result, Ok(true)) {
            self.recover_selected_content();
        }
        result
    }

    pub fn browse_server(&mut self, index: usize) -> Result<bool, String> {
        self.shared.images.borrow_mut().invalidate();
        let Some(server) = self.client.servers.get(index) else {
            return Ok(false);
        };
        let channel = self
            .client
            .navigation
            .remembered_channel(&server.id)
            .unwrap_or(0);
        let result = self.client.open(index, channel);
        if matches!(result, Ok(true)) {
            self.recover_selected_content();
        }
        result
    }

    pub fn open_direct(&mut self, index: usize, session: u32) -> Result<bool, String> {
        self.shared.images.borrow_mut().invalidate();
        if index >= self.client.servers.len() {
            return Ok(false);
        }
        let Some(channel) = self.client.open_direct(index, session) else {
            return Ok(false);
        };
        let result = self.client.open(index, channel);
        if matches!(result, Ok(true)) {
            self.recover_selected_content();
        }
        result
    }

    /// Capture original destination and caption; selected file bytes stay in
    /// the UI/codec realm. Context is mandatory and issued by `timeline()`.
    pub fn image_begin(&mut self, raw: String) -> Result<String, String> {
        if raw.len() > nuschl_core::MAX_CONTENT_BYTES + 4096 || self.client.sending() {
            return Err("This image draft is too large or another send is pending.".into());
        }
        let request: ImageBegin = serde_json::from_str(&raw).map_err(|_| "Invalid image draft.")?;
        let destination = image_destination(&self.client)
            .ok_or("Choose an available conversation before attaching an image.")?;
        let limits = self
            .client
            .servers
            .iter()
            .find(|server| server.id == destination.server)
            .and_then(|server| server.limits.clone())
            .unwrap_or_default();
        let budget = self
            .shared
            .images
            .borrow_mut()
            .begin(request, destination, &limits)?;
        serde_json::to_string(&budget).map_err(|_| "Image budget could not be serialized.".into())
    }

    pub fn image_cancel(&mut self, ticket: String) {
        self.shared.images.borrow_mut().cancel(&ticket);
    }

    pub fn image_send(&mut self, raw: String) -> Result<String, String> {
        if raw.len() > crate::content_broker::MAX_REPLY_BYTES || self.client.sending() {
            return Err("The image is too large or another send is pending.".into());
        }
        let request: ImageSend =
            serde_json::from_str(&raw).map_err(|_| "Invalid encoded image.")?;
        let destination = image_destination(&self.client)
            .ok_or("This image destination is no longer available.")?;
        let limits = self
            .client
            .servers
            .iter()
            .find(|server| server.id == destination.server)
            .and_then(|server| server.limits.clone())
            .unwrap_or_default();
        let captured = self
            .shared
            .images
            .borrow()
            .validate(&request, &destination, &limits)?;
        let connections = self.shared.connections.borrow();
        let connection = connections
            .get(&destination.server)
            .filter(|connection| connection.generation == destination.generation)
            .ok_or("This image belongs to an older connection.")?;
        connection.session.borrow_mut().send_inline_image(
            destination.target.clone(),
            &request.html,
            &captured.caption,
            destination.peer,
            matches!(destination.target, TextTarget::Channel(_)).then_some(destination.instance),
            destination.own,
        )?;
        self.shared.pump(
            &destination.server,
            destination.generation,
            &connection.socket,
            &connection.session,
        )?;
        drop(connections);
        self.shared.images.borrow_mut().cancel(&captured.ticket);
        serde_json::to_string(&ImageAdmission {
            ticket: captured.ticket,
            attachment_revision: captured.attachment_revision,
            draft_revision: captured.draft_revision,
            state: ImageAdmissionState::SentToTransport,
        })
        .map_err(|_| "Image admission could not be serialized.".into())
    }

    pub fn send_text(&mut self, body: String) -> Result<(), String> {
        self.client.send_text(body).map_err(|reason| match reason {
            nuschl_client::SendError::Nothing => {
                "Choose a conversation, enter a message, and wait for any previous send.".into()
            }
            nuschl_client::SendError::TooLong => "Message exceeds the 256 KiB client limit.".into(),
            nuschl_client::SendError::Refused(reason) => reason,
        })
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
        let completed = self.shared.storage.borrow_mut().complete_raw(
            &ticket,
            &response,
            |id, generation, identity| {
                self.client.servers.iter().any(|server| {
                    server.id == id
                        && (generation == 0 || server.generation == generation)
                        && server.history_identity().as_deref() == Some(identity)
                })
            },
        );
        self.shared.deliveries.borrow_mut().push_back(completed);
    }

    /// Sources leave this credential-bearing realm only under SDK-owned leases.
    pub fn content_requests(&mut self) -> String {
        self.drain_recovery();
        let batch = self
            .shared
            .jobs
            .borrow_mut()
            .take_with_failures(now_ms(), |origin, source| {
                content_current(&self.client, &self.shared, origin, Some(source))
            });
        let changed = !batch.failures.is_empty();
        for failure in batch.failures {
            self.clear_evicted(&failure.evicted);
            if let Some(message) = content_message_mut(&mut self.client, &failure.origin) {
                message.document = failure.terminal.and_then(RequestError::terminal).unwrap_or_else(|| MessageDocument {
                    blocks: vec![ContentBlock::text(
                        "[Content processing exceeded its deadline. Retry this message preview or reload.]",
                    )],
                });
            }
            if let Origin::Welcome {
                server,
                generation,
                revision,
            } = &failure.origin
                && let Some(source) = content_source(&self.client, &self.shared, &failure.origin)
                && self
                    .shared
                    .welcomes
                    .borrow_mut()
                    .fail(server, *generation, revision, &source)
            {
                self.shared.inputs.borrow_mut().push_back(Input::Server {
                    server: server.clone(),
                    generation: *generation,
                    event: nuschl_client::input::ServerEvent::Welcome {
                        source,
                        document: pending_document(),
                    },
                });
            }
        }
        if changed {
            self.shared.wake();
        }
        serde_json::to_string(&batch.operations).unwrap_or_else(|_| "[]".into())
    }

    /// Acknowledges success, failure or cancellation. Only this exact original
    /// source/context may install a new opaque UI derivative handle.
    pub fn content_complete(
        &mut self,
        request: String,
        response: String,
    ) -> Result<String, String> {
        if request.len() > 20 {
            return Err("Invalid content request.".into());
        }
        let completed = self.shared.jobs.borrow_mut().complete(
            &request,
            (!response.is_empty()).then_some(response.as_str()),
            now_ms(),
            |origin, source| content_current(&self.client, &self.shared, origin, Some(source)),
        )?;
        self.drain_recovery();
        let Some(completed) = completed else {
            return Ok("null".into());
        };
        self.clear_evicted(&completed.evicted);
        let token = completed.token;
        match &completed.origin {
            Origin::Message {
                server,
                generation,
                channel,
                instance,
                serial,
                revision,
            } => {
                if let Some(message) = self
                    .client
                    .servers
                    .iter_mut()
                    .find(|state| &state.id == server && state.generation == *generation)
                    .and_then(|state| {
                        state
                            .channels
                            .iter_mut()
                            .find(|state| state.id == *channel && state.instance == *instance)
                    })
                    .and_then(|state| {
                        state.timeline.messages.iter_mut().find(|message| {
                            message.serial == *serial && message.revision == *revision
                        })
                    })
                {
                    message.document = MessageDocument {
                        blocks: vec![ContentBlock::text(&completed.rendered.plain_text)],
                    };
                } else {
                    return Ok("null".into());
                }
            }
            Origin::Welcome {
                server,
                generation,
                revision,
            } => {
                if !self.shared.welcomes.borrow_mut().complete(
                    server,
                    *generation,
                    revision,
                    &completed.source,
                    token.clone(),
                ) {
                    return Ok("null".into());
                }
                self.shared.inputs.borrow_mut().push_back(Input::Server {
                    server: server.clone(),
                    generation: *generation,
                    event: nuschl_client::input::ServerEvent::Welcome {
                        source: completed.source,
                        document: pending_document(),
                    },
                });
            }
        }
        serde_json::to_string(&serde_json::json!({"token":token,"charge":completed.rendered.retained_bytes,"document":completed.rendered.document,"failed":completed.failed})).map_err(|_| "Content result could not be serialized.".into())
    }

    pub fn content_request(&mut self, token: String) -> Result<bool, String> {
        let (origin, mode, digest) = self
            .shared
            .jobs
            .borrow()
            .origin(&token)
            .ok_or("Content is no longer available.")?;
        let source = content_source(&self.client, &self.shared, &origin)
            .ok_or("Content is no longer available.")?;
        use sha2::Digest;
        if <[u8; 32]>::from(sha2::Sha256::digest(source.as_bytes())) != digest {
            return Err("This content source changed.".into());
        }
        let result = self
            .shared
            .jobs
            .borrow_mut()
            .request(origin.clone(), &source, mode, now_ms());
        match result {
            Ok(()) => Ok(true),
            Err(RequestError::Overloaded) => Ok(false),
            Err(error) => {
                self.source_refused(&origin, error);
                Err(error.message().into())
            }
        }
    }

    /// The UI's terminal retirement of its preview lane, with a fixed reason.
    /// Posted credit stays held until each real ACK; nothing is replaced.
    pub fn content_retire(&mut self, reason: String) -> Result<bool, String> {
        reason.parse::<crate::content_jobs::RetireReason>()?;
        let retired = self.shared.jobs.borrow_mut().retire_lane();
        self.drain_recovery();
        Ok(retired)
    }

    pub fn timeline(&self) -> String {
        let mut view = snapshot::timeline(&self.client);
        let context = self
            .shared
            .images
            .borrow_mut()
            .context(image_destination(&self.client));
        if let Some(view) = &mut view {
            view.content_context = context;
        }
        if let Some(view) = &mut view
            && let Some(server) = self
                .client
                .servers
                .iter()
                .find(|server| server.id == view.server)
            && let Some(channel) = server
                .channels
                .iter()
                .find(|channel| channel.id == view.channel)
        {
            for message in &mut view.messages {
                let origin = Origin::Message {
                    server: server.id.clone(),
                    generation: server.generation,
                    channel: channel.id,
                    instance: channel.instance,
                    serial: message.serial.parse().unwrap_or(u64::MAX),
                    revision: message.revision,
                };
                let jobs = self.shared.jobs.borrow();
                message.content_token = jobs.token(&origin).map(str::to_owned);
                message.content_failed = jobs.is_failed(&origin);
                if message.content_failed
                    && crate::content_jobs::is_terminal_document(&message.document)
                {
                    message.content_token = None;
                    message.content_failed = false;
                }
            }
        }
        serde_json::to_string(&view).unwrap_or_else(|_| "null".into())
    }

    /// Original visible UI provenance. The fixed adapter validates this and
    /// invokes its concrete existing method synchronously, without yielding.
    pub fn validate_action(&self, call: String, proof: String) -> Result<(), String> {
        let voice = self.shared.voice.borrow().view();
        crate::actions::validate(
            &self.client.servers,
            &self.catalog,
            self.client.selected_indices(),
            voice.as_ref(),
            &call,
            &proof,
        )
    }

    /// Explicit result authority for add/edit -> connect -> browse form chains;
    /// normal old UI clicks must never substitute a newer reference by index.
    pub fn server_reference(&self, index: usize) -> Result<String, String> {
        let server = self.client.servers.get(index).ok_or("Unknown server.")?;
        let catalog = self.catalog.get(index).ok_or("Unknown catalog entry.")?;
        serde_json::to_string(&ServerRef::capture(server, catalog))
            .map_err(|_| "Server reference could not be read.".into())
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
                let stamp = self.catalog.reserve(self.client.servers.len(), None)?;
                let index = self.client.add_server(bookmark);
                self.catalog.commit(index, stamp);
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
            let _ = self.shared.pump(&server, generation, &socket, &session);
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
        // Content deadlines (including a cancelled job's ACK bound) must not
        // wait for unrelated network traffic to be observed.
        let due = self.shared.jobs.borrow().due(now_ms.max(0.0) as u64);
        changed || expired || due
    }

    /// Apply what arrived; what changed, as JSON (`[{"kind": …}]`).
    pub fn poll(&mut self) -> String {
        let previous_lifetime = selected_lifetime(&self.client);
        loop {
            let input = self.shared.inputs.borrow_mut().pop_front();
            let Some(input) = input else { break };
            self.client.handle(input);
        }
        if let (Some(previous), Some(current)) =
            (previous_lifetime, selected_lifetime(&self.client))
            && previous.0 == current.0
            && previous.2 == current.2
            && previous != current
        {
            // Initial cached/offline channels and reconnects get a NEW
            // storage request/ticket once the real wire lifetime is known.
            // Never retarget an old reply or its already-posted render.
            if let Err(error) = self.client.load_history(None) {
                self.client.handle(Input::HistoryError(error));
            }
        }
        loop {
            let Some(delivery) = self.shared.deliveries.borrow_mut().pop_front() else {
                break;
            };
            self.admit_delivery(delivery);
        }
        self.client.settle();
        let selected = selected_lifetime(&self.client);
        self.shared
            .jobs
            .borrow_mut()
            .select(selected.map(|(server, _, channel, _)| (server, channel)));
        self.shared
            .jobs
            .borrow_mut()
            .retain(|origin| content_resident(&self.client, &self.shared, origin));
        // Rebind verified derivatives of the visible conversation before the
        // next snapshot whenever its lifetime differs from the one last
        // recovered. A reconnect can change the generation outside poll
        // (connect), and a direct conversation keeps instance 0.
        let visible = selected_lifetime(&self.client);
        if visible.is_some() && visible != self.rekeyed {
            self.recover_selected_content();
        }
        self.reconcile_voice();
        if self.client.navigation.selection().is_none()
            && let Some(index) = self
                .client
                .servers
                .iter()
                .position(|server| server.status == nuschl_client::server::Status::Connected)
        {
            let _ = self.client.open(index, 0);
        }
        let updates: Vec<snapshot::UpdateView> = self
            .client
            .take_updates()
            .iter()
            .filter_map(|update| {
                if let Update::Rendered {
                    server,
                    key: RenderKey::Welcome,
                    ..
                } = update
                {
                    let state = self.client.servers.iter().find(|s| &s.id == server)?;
                    return self
                        .shared
                        .welcomes
                        .borrow_mut()
                        .take_view(
                            server,
                            state.generation,
                            state.welcome.as_deref(),
                            state.status == nuschl_client::server::Status::Connected,
                        )
                        .map(|view| snapshot::UpdateView::Welcome {
                            server: server.clone(),
                            token: view.token,
                            document: view.document,
                            content_token: view.content_token,
                        });
                }
                snapshot::update(update)
            })
            .collect();
        serde_json::to_string(&updates).unwrap_or_else(|_| "[]".into())
    }

    /// Every server with its status, channels and users, as JSON.
    pub fn servers(&self) -> String {
        let mut views = snapshot::ordered_servers(&self.client);
        let welcomes = self.shared.welcomes.borrow();
        for view in &mut views {
            let state = &self.client.servers[view.runtime_index];
            view.catalog_instance = self
                .catalog
                .get(view.runtime_index)
                .unwrap_or(0)
                .to_string();
            view.welcome_token = welcomes
                .view(
                    &state.id,
                    state.generation,
                    state.welcome.as_deref(),
                    state.status == nuschl_client::server::Status::Connected,
                )
                .map(|v| v.token.clone());
            view.history = self
                .shared
                .history
                .borrow()
                .view(&state.id, state.generation);
        }
        serde_json::to_string(&views).unwrap_or_else(|_| "[]".into())
    }
}

// Non-exported helpers keep browser devices outside the SDK/session boundary.
impl WebClient {
    fn recover_selected_content(&mut self) {
        self.recovery.clear();
        self.recovery_selection = selected_lifetime(&self.client);
        self.rekeyed = self.recovery_selection.clone();
        self.recovery_until = now_ms().saturating_add(120_000);
        let Some((server, channel)) = self.client.selected_indices() else {
            return;
        };
        let server = &self.client.servers[server];
        let channel = &server.channels[channel];
        let mut jobs = self.shared.jobs.borrow_mut();
        jobs.select(Some((server.id.clone(), channel.id)));
        self.recovery = jobs
            .recover_rows(&server.id, server.generation, channel)
            .into();
        drop(jobs);
        self.drain_recovery();
    }

    fn drain_recovery(&mut self) {
        let selected = selected_lifetime(&self.client);
        if self.recovery_selection != selected {
            self.recovery.clear();
            self.recovery_selection = selected.clone();
            self.shared
                .jobs
                .borrow_mut()
                .select(selected.map(|(server, _, channel, _)| (server, channel)));
            return;
        }
        if now_ms() >= self.recovery_until {
            self.recovery.clear();
            return;
        }
        if self.shared.jobs.borrow().retired() {
            // A retired lane admits nothing; tell each waiting row why.
            while let Some((origin, _)) = self.recovery.pop_front() {
                self.source_refused(&origin, RequestError::Retired);
            }
            return;
        }
        while self.shared.jobs.borrow().recovery_credit() {
            let Some((origin, mode)) = self.recovery.front().cloned() else {
                break;
            };
            // Cursor carries identities/modes only. Original authority and raw
            // source are rechecked at admission; navigation never rebinds work.
            let Some(source) = content_source(&self.client, &self.shared, &origin) else {
                self.recovery.pop_front();
                continue;
            };
            let result =
                self.shared
                    .jobs
                    .borrow_mut()
                    .request(origin.clone(), &source, mode, now_ms());
            match result {
                Ok(()) => {}
                Err(RequestError::Overloaded) => break,
                Err(error) => self.source_refused(&origin, error),
            }
            self.recovery.pop_front();
        }
    }

    fn clear_evicted(&mut self, origins: &[Origin]) {
        self.shared
            .jobs
            .borrow()
            .clear_evicted(&mut self.client.servers, origins);
    }

    fn schedule_source(&mut self, origin: Origin, source: &str, mode: SourceMode) {
        let Some(message) = content_message_mut(&mut self.client, &origin)
            .filter(|message| message.source == source)
        else {
            return;
        };
        // Source equality verifies the already identified receipt, never finds a
        // different row. Existing representation cannot be overwritten by a page.
        let mode = *message.source_format.get_or_insert(mode);
        let selected = self.recovery_selection.as_ref().is_some_and(|(server, generation, channel, instance)| matches!(&origin, Origin::Message { server: id, generation: epoch, channel: target, instance: lifetime, .. } if id == server && epoch == generation && target == channel && lifetime == instance))
            && self.recovery_selection == selected_lifetime(&self.client);
        if self
            .shared
            .jobs
            .borrow()
            .pending_source(&origin, source, mode)
        {
            return;
        }
        let deferred = selected && !self.shared.jobs.borrow().recovery_credit();
        let permanent = self.shared.jobs.borrow().permanent_refusal(source);
        let result = if let Some(error) = permanent {
            Err(error)
        } else if deferred {
            Err(RequestError::Overloaded)
        } else {
            self.shared
                .jobs
                .borrow_mut()
                .request(origin.clone(), source, mode, now_ms())
        };
        if let Err(error) = result {
            self.source_refused(&origin, error);
            if error == RequestError::Overloaded
                && selected
                && self.recovery.len() < nuschl_client::timeline::MAX_TIMELINE
                && !self
                    .recovery
                    .iter()
                    .any(|(candidate, _)| candidate == &origin)
                && !self.shared.jobs.borrow().successful_row(&origin)
            {
                if self.recovery.is_empty() {
                    self.recovery_until = now_ms().saturating_add(120_000);
                }
                self.recovery.push_back((origin, mode));
            }
        }
    }

    fn source_refused(&mut self, origin: &Origin, error: RequestError) {
        // A rehydration failure cannot destroy a valid charged derivative.
        if self.shared.jobs.borrow().successful_row(origin) {
            return;
        }
        if let Some(message) = content_message_mut(&mut self.client, origin) {
            message.document = error.terminal().unwrap_or_else(|| MessageDocument {
                blocks: vec![ContentBlock::text(
                    "[Content processing is busy; reopen this conversation to prepare it again.]",
                )],
            });
        }
    }

    /// A validated Append changes only its own connection's status, and only
    /// while that connection is still live. Its first failure is the notice,
    /// at the position its per-row history input used to occupy.
    fn admit_save(&mut self, save: &SaveResult) {
        if save.outcome == SaveOutcome::Saved {
            self.client.handle(Input::HistoryRecovered);
        }
        if self.shared.live(&save.server, save.generation)
            && self.shared.history.borrow_mut().observe(
                &save.server,
                save.generation,
                HealthEvent::Outcome(save.outcome),
            )
        {
            self.client.handle(Input::HistoryError(SAVE_FAILED.into()));
        }
    }

    fn admit_delivery(&mut self, delivery: RawCompletion) {
        if let Some(save) = &delivery.save {
            self.admit_save(save);
        }
        for input in delivery.inputs {
            match input {
                Input::Message {
                    server,
                    generation,
                    target,
                    storage_key,
                    peer,
                    message,
                } => {
                    if let TextTarget::Channel(id) = &target
                        && delivery.channel_instance.is_some_and(|instance| {
                            self.client
                                .servers
                                .iter()
                                .find(|state| state.id == server && state.generation == generation)
                                .and_then(|state| {
                                    state.channels.iter().find(|channel| channel.id == *id)
                                })
                                .is_none_or(|channel| channel.instance != instance)
                        })
                    {
                        continue;
                    }
                    let source = message.message.body.clone();
                    if let Some(receipt) = self.client.receive_message(
                        &server,
                        generation,
                        target,
                        storage_key,
                        peer,
                        message,
                    ) {
                        let origin = Origin::Message {
                            server: receipt.server,
                            generation,
                            channel: receipt.channel,
                            instance: receipt.channel_instance,
                            serial: receipt.serial,
                            revision: receipt.revision,
                        };
                        self.schedule_source(
                            origin,
                            &source,
                            delivery.live_mode.unwrap_or(SourceMode::Marked),
                        );
                    }
                }
                Input::History {
                    server,
                    ticket,
                    before,
                    messages,
                    next_before,
                } => {
                    if !self.client.navigation.accepts(&ticket) {
                        continue;
                    }
                    use sha2::Digest;
                    let originals: std::collections::BTreeMap<_, _> = messages
                        .iter()
                        .filter_map(|message| {
                            Some((
                                message.id?,
                                <[u8; 32]>::from(sha2::Sha256::digest(
                                    message.message.body.as_bytes(),
                                )),
                            ))
                        })
                        .collect();
                    let channel_id = ticket.conversation.channel;
                    self.client.handle(Input::History {
                        server: server.clone(),
                        ticket,
                        before,
                        messages,
                        next_before,
                    });
                    let mut sources = Vec::new();
                    if let Some(server) =
                        self.client.servers.iter().find(|state| state.id == server)
                        && let Some(channel) = server
                            .channels
                            .iter()
                            .find(|channel| channel.id == channel_id)
                    {
                        for message in &channel.timeline.messages {
                            if message
                                .id
                                .and_then(|id| originals.get(&id))
                                .is_some_and(|digest| {
                                    message.source_format.is_some()
                                        || *digest
                                            == <[u8; 32]>::from(sha2::Sha256::digest(
                                                message.source.as_bytes(),
                                            ))
                                })
                            {
                                let mode = message
                                    .id
                                    .and_then(|id| delivery.stored_modes.get(&id))
                                    .copied()
                                    .unwrap_or(SourceMode::Marked);
                                let origin = Origin::Message {
                                    server: server.id.clone(),
                                    generation: server.generation,
                                    channel: channel.id,
                                    instance: channel.instance,
                                    serial: message.serial,
                                    revision: message.revision,
                                };
                                sources.push((origin, mode));
                            }
                        }
                    }
                    for (origin, mode) in sources {
                        if let Some(source) = content_source(&self.client, &self.shared, &origin) {
                            self.schedule_source(origin, &source, mode);
                        }
                    }
                }
                input => self.client.handle(input),
            }
        }
    }

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
        let requested = self.shared.voice.borrow().requested();
        let target = self
            .shared
            .connections
            .borrow()
            .get(&server.id)
            .map(|connection| connection.session.borrow().voice_target(requested))
            .unwrap_or_else(|| nuschl_core::bitrate::VoiceBitrate::resolve(requested, 0));
        self.shared.voice.borrow_mut().set_target(target);
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

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = performance, js_name = now)]
    fn content_monotonic_now() -> f64;
}
fn now_ms() -> u64 {
    content_monotonic_now().max(0.0) as u64
}
fn content_source(client: &Client, shared: &Shared, origin: &Origin) -> Option<String> {
    match origin {
        Origin::Message {
            server,
            generation,
            channel,
            instance,
            serial,
            revision,
        } => {
            let server = client
                .servers
                .iter()
                .find(|state| &state.id == server && state.generation == *generation)?;
            let channel = server
                .channels
                .iter()
                .find(|state| state.id == *channel && state.instance == *instance)?;
            channel
                .timeline
                .messages
                .iter()
                .find(|message| message.serial == *serial && message.revision == *revision)
                .map(|message| message.source.clone())
        }
        Origin::Welcome {
            server,
            generation,
            revision,
        } => {
            let server = client.servers.iter().find(|state| {
                &state.id == server
                    && state.generation == *generation
                    && state.status == nuschl_client::server::Status::Connected
            })?;
            let source = server.welcome.as_deref()?;
            shared
                .welcomes
                .borrow()
                .view(&server.id, *generation, Some(source), true)
                .filter(|view| &view.token == revision)?;
            Some(source.into())
        }
    }
}
fn content_current(
    client: &Client,
    shared: &Shared,
    origin: &Origin,
    expected: Option<&str>,
) -> bool {
    match origin {
        Origin::Message {
            server,
            generation,
            channel,
            instance,
            serial,
            revision,
        } => client
            .servers
            .iter()
            .find(|state| &state.id == server && state.generation == *generation)
            .and_then(|state| {
                state
                    .channels
                    .iter()
                    .find(|state| state.id == *channel && state.instance == *instance)
            })
            .and_then(|state| {
                state
                    .timeline
                    .messages
                    .iter()
                    .find(|message| message.serial == *serial && message.revision == *revision)
            })
            .is_some_and(|message| expected.is_none_or(|source| source == message.source)),
        Origin::Welcome {
            server,
            generation,
            revision,
        } => client
            .servers
            .iter()
            .find(|state| {
                &state.id == server
                    && state.generation == *generation
                    && state.status == nuschl_client::server::Status::Connected
            })
            .is_some_and(|state| {
                state.welcome.as_deref().is_some_and(|source| {
                    expected.is_none_or(|expected| expected == source)
                        && shared
                            .welcomes
                            .borrow()
                            .view(server, *generation, Some(source), true)
                            .is_some_and(|view| &view.token == revision)
                })
            }),
    }
}

fn image_destination(client: &Client) -> Option<Destination> {
    let (index, position) = client.selected_indices()?;
    let server = &client.servers[index];
    if server.status != nuschl_client::server::Status::Connected || server.mls {
        return None;
    }
    let channel = &server.channels[position];
    let own = server
        .own_session
        .and_then(|id| server.users.get(&id))?
        .instance;
    let peer = match channel.target {
        TextTarget::Channel(_) => None,
        TextTarget::Direct(id) => Some(
            server
                .users
                .get(&id)
                .filter(|user| {
                    channel
                        .direct_peer
                        .as_ref()
                        .is_some_and(|peer| peer.instance == user.instance)
                })?
                .instance,
        ),
        _ => return None,
    };
    Some(Destination {
        server: server.id.clone(),
        generation: server.generation,
        channel: channel.id,
        instance: channel.instance,
        target: channel.target.clone(),
        peer,
        own,
    })
}

fn selected_lifetime(client: &Client) -> Option<(String, u64, u32, u64)> {
    let (server, channel) = client.selected_indices()?;
    let server = &client.servers[server];
    let channel = &server.channels[channel];
    Some((
        server.id.clone(),
        server.generation,
        channel.id,
        channel.instance,
    ))
}

fn content_message_mut<'a>(
    client: &'a mut Client,
    origin: &Origin,
) -> Option<&'a mut nuschl_client::timeline::Message> {
    let Origin::Message {
        server,
        generation,
        channel,
        instance,
        serial,
        revision,
    } = origin
    else {
        return None;
    };
    client
        .servers
        .iter_mut()
        .find(|state| &state.id == server && state.generation == *generation)
        .and_then(|state| {
            state
                .channels
                .iter_mut()
                .find(|state| state.id == *channel && state.instance == *instance)
        })
        .and_then(|state| {
            state
                .timeline
                .messages
                .iter_mut()
                .find(|message| message.serial == *serial && message.revision == *revision)
        })
}

fn content_resident(client: &Client, shared: &Shared, origin: &Origin) -> bool {
    match origin {
        Origin::Message { .. } => origin.resident(&client.servers),
        _ => content_current(client, shared, origin, None),
    }
}
