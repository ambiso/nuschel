// SPDX-License-Identifier: EUPL-1.2
//! The desktop sees typed events; transport, parsing, audio and history run off its thread.
use nuschel_client::HistoryTicket;
use nuschel_core::{ChatMessage, ContentBlock, MessageDocument};
use nuschel_ipc::{LiveMediaEvent, LiveMediaRequest};
use nuschel_session::{
    Error,
    config::{Profile, ServerBookmark},
    connection::{self, Command, ConnectOptions, IncomingText, SessionEvent, TextTarget},
    workers::{self, Media, Storage},
};
use std::{collections::BTreeMap, path::PathBuf};
use tokio::{runtime::Handle, sync::mpsc};

pub struct RenderedMessage {
    pub id: Option<i64>,
    pub message: ChatMessage,
    pub document: MessageDocument,
    pub outgoing: bool,
}
pub enum Event {
    Protocol {
        server: String,
        generation: u64,
        event: SessionEvent,
    },
    Certificate {
        server: String,
        generation: u64,
        sha256: String,
        changed: bool,
        reason: String,
    },
    Disconnected {
        server: String,
        generation: u64,
        reason: String,
    },
    AudioStatus {
        server: String,
        generation: u64,
        message: String,
    },
    Message {
        server: String,
        generation: u64,
        target: TextTarget,
        storage_key: String,
        peer: Option<connection::User>,
        message: RenderedMessage,
    },
    History {
        server: String,
        ticket: HistoryTicket,
        messages: Vec<RenderedMessage>,
        next_before: Option<i64>,
    },
    Sent {
        server: String,
        generation: u64,
        target: TextTarget,
        body: String,
    },
    HistoryError(String),
    /// Voice activity for a session (including our own) on one connection generation.
    Talking {
        server: String,
        generation: u64,
        session: u32,
        talking: bool,
    },
}
struct Connection {
    commands: mpsc::Sender<Command>,
    task: tokio::task::JoinHandle<()>,
}
pub struct Backend {
    handle: Handle,
    profile: Profile,
    pub events: mpsc::Receiver<Event>,
    sender: mpsc::Sender<Event>,
    storage: mpsc::Sender<StorageCommand>,
    content: mpsc::Sender<ContentJob>,
    connections: BTreeMap<String, Connection>,
    generation: u64,
    storage_task: tokio::task::JoinHandle<()>,
    content_task: tokio::task::JoinHandle<()>,
}
struct ContentJob {
    server: String,
    generation: u64,
    identity: String,
    nonce: String,
    text: IncomingText,
}
enum StorageCommand {
    Append {
        job: ContentJob,
        source: String,
        document: MessageDocument,
    },
    Load {
        server: String,
        identity: String,
        channel: String,
        ticket: HistoryTicket,
        before: Option<i64>,
    },
}
impl Backend {
    pub fn new(handle: Handle, profile: Profile) -> Self {
        let (sender, events) = mpsc::channel(512);
        let (storage, storage_rx) = mpsc::channel(128);
        let (content, content_rx) = mpsc::channel(64);
        let storage_task = handle.spawn(storage_loop(
            profile.data_dir.clone(),
            storage_rx,
            sender.clone(),
        ));
        let content_task = handle.spawn(content_loop(content_rx, storage.clone(), sender.clone()));
        Self {
            handle,
            profile,
            events,
            sender,
            storage,
            content,
            connections: BTreeMap::new(),
            generation: 0,
            storage_task,
            content_task,
        }
    }
    pub fn connect(
        &mut self,
        server: ServerBookmark,
        password: Option<String>,
    ) -> Result<u64, String> {
        if self.connections.len() >= 8 && !self.connections.contains_key(&server.id) {
            return Err("Disconnect a server before opening another connection (limit: 8).".into());
        }
        if let Some(old) = self.connections.remove(&server.id) {
            old.task.abort();
        }
        self.generation += 1;
        let generation = self.generation;
        let (commands, receiver) = mpsc::channel(32);
        let id = server.id.clone();
        let task = self.handle.spawn(connection_loop(
            self.profile.clone(),
            server,
            password,
            generation,
            receiver,
            self.sender.clone(),
            self.content.clone(),
        ));
        self.connections.insert(id, Connection { commands, task });
        Ok(generation)
    }
    pub fn command(&self, server: &str, command: Command) -> Result<(), String> {
        self.connections
            .get(server)
            .ok_or("Server is not connected.")?
            .commands
            .try_send(command)
            .map_err(|_| "Connection is busy or has ended; reconnect to try again.".into())
    }
    pub fn disconnect(&mut self, server: &str) {
        if let Some(connection) = self.connections.remove(server) {
            connection.task.abort();
        }
    }
    pub fn history(
        &self,
        server: String,
        identity: String,
        channel: String,
        ticket: HistoryTicket,
        before: Option<i64>,
    ) -> Result<(), String> {
        self.storage
            .try_send(StorageCommand::Load {
                server,
                identity,
                channel,
                ticket,
                before,
            })
            .map_err(|_| "History is busy; try again shortly.".into())
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        for connection in self.connections.values() {
            connection.task.abort();
        }
        self.storage_task.abort();
        self.content_task.abort();
    }
}

async fn connection_loop(
    profile: Profile,
    server: ServerBookmark,
    password: Option<String>,
    generation: u64,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    content: mpsc::Sender<ContentJob>,
) {
    let id = server.id.clone();
    let options = match profile.identity() {
        Ok(identity) => ConnectOptions {
            server: server.clone(),
            identity,
            password,
            force_tcp: false,
        },
        Err(error) => {
            let _ = events
                .send(Event::Disconnected {
                    server: id,
                    generation,
                    reason: error.to_string(),
                })
                .await;
            return;
        }
    };
    let (session_commands, receiver) = mpsc::channel(32);
    let (session_events, mut incoming) = mpsc::channel(128);
    let mut session = tokio::spawn(connection::run(options, receiver, session_events));
    let mut media: Option<Media> = None;
    let mut peer = None;
    let nonce = format!("{}:{generation}:{}", id, connection::now_ms());
    let mut muted = true;
    let mut deafened = false;
    let mut sequence = 0;
    let mut content_gap = false;
    let mut capture_epoch = 0;
    let mut audio_enabled = false;
    let mut bitrate = 48_000;
    let mut retry_audio = tokio::time::Instant::now();
    let mut finished = false;
    let mut own_session = None;
    let mut speaking = BTreeMap::<u32, tokio::time::Instant>::new();
    let mut talk_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    talk_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = talk_tick.tick(), if !speaking.is_empty() => {
                let now = tokio::time::Instant::now();
                let silent = speaking.iter().filter(|(_, last)| now.duration_since(**last) > TALK_HOLD).map(|(session, _)| *session).collect::<Vec<_>>();
                for session in silent {
                    speaking.remove(&session);
                    let _ = events.send(Event::Talking { server: id.clone(), generation, session, talking: false }).await;
                }
            },
            result = &mut session => {
                finished = true;
                match result {
                    Ok(Err(Error::Certificate { sha256, changed, reason })) => { let _ = events.send(Event::Certificate { server: id.clone(), generation, sha256, changed, reason }).await; },
                    result => {
                        let reason = match result { Ok(Ok(())) => "Disconnected".into(), Ok(Err(error)) => error.to_string(), Err(error) => format!("Connection task failed: {error}") };
                        let _ = events.send(Event::Disconnected { server: id.clone(), generation, reason }).await;
                    },
                }
                break;
            },
            Some(command) = commands.recv() => {
                if let Command::SetSelf { muted: mute, deafened: deaf } = &command {
                    muted = *mute || *deaf; deafened = *deaf; capture_epoch += 1;
                    // Native stream teardown can block inside a backend (including Pulse).
                    // Replacing the isolated worker closes its audio capability even then;
                    // no old callback or queued capture can survive a mute transition.
                    media.take();
                    retry_audio = tokio::time::Instant::now();
                }
                if session_commands.send(command).await.is_err() { break; }
            },
            Some(event) = incoming.recv() => match event {
                SessionEvent::PeerCertificate(sha256) => { peer = Some(sha256.clone()); let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::PeerCertificate(sha256) }).await; },
                SessionEvent::Connected { session: connected_session, max_bandwidth } => {
                    own_session = Some(connected_session);
                    let own_session = connected_session;
                    audio_enabled = true;
                    bitrate = max_bandwidth.saturating_sub(24_000).clamp(6000, 48_000);
                    retry_audio = tokio::time::Instant::now();
                    let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::Connected { session: own_session, max_bandwidth } }).await;
                },
                SessionEvent::Audio(packet) => {
                    mark_talking(&events, &id, generation, &mut speaking, packet.sender, !packet.end).await;
                    if let Some(media) = &media {
                        let _ = media.commands.try_send(LiveMediaRequest::Playback { sender: packet.sender, sequence: packet.sequence, opus: packet.opus, end: packet.end, volume: packet.volume });
                    }
                },
                SessionEvent::Text(text) => if let Some(peer) = &peer {
                    if text.outgoing { let _ = events.send(Event::Sent { server: id.clone(), generation, target: text.target.clone(), body: text.body.clone() }).await; }
                    let job = ContentJob { server: id.clone(), generation, identity: server.history_identity(peer), nonce: nonce.clone(), text };
                    if content.try_send(job).is_err() {
                        if !content_gap { let _ = events.send(Event::HistoryError("Chat processing is overloaded; some received messages could not be saved.".into())).await; content_gap = true; }
                    } else { content_gap = false; }
                },
                SessionEvent::Welcome(body) => { let (text, _) = prepare(&body, true).await; let _ = events.send(Event::Protocol { server: id.clone(), generation, event: SessionEvent::Welcome(text) }).await; },
                event => { let _ = events.send(Event::Protocol { server: id.clone(), generation, event }).await; },
            },
            _ = tokio::time::sleep_until(retry_audio), if audio_enabled && !deafened && media.is_none() => {
                capture_epoch += 1;
                match Media::start() {
                    Ok(worker) => {
                        let _ = worker.commands.try_send(LiveMediaRequest::Configure { bitrate, noise_suppression: false });
                        let _ = worker.commands.try_send(LiveMediaRequest::SetState { epoch: capture_epoch, muted, deafened });
                        media = Some(worker);
                    },
                    Err(error) => { retry_audio = tokio::time::Instant::now() + std::time::Duration::from_secs(3); let _ = events.send(Event::AudioStatus { server: id.clone(), generation, message: error.to_string() }).await; },
                }
            },
            event = async { media.as_mut().unwrap().events.recv().await }, if media.is_some() => match event {
                Some(LiveMediaEvent::Packet { epoch, opus, end, .. }) => {
                    if epoch != capture_epoch { continue; }
                    let packet = nuschel_protocol::voice::VoicePacket { sender: 0, sequence, opus, end, volume: 1. };
                    sequence += 2;
                    if !muted || end { let _ = session_commands.try_send(Command::Voice { target: 0, packet }); }
                    if let Some(session) = own_session { mark_talking(&events, &id, generation, &mut speaking, session, !end && !muted).await; }
                },
                Some(LiveMediaEvent::Status { ready, message }) => {
                    if !ready { media.take(); retry_audio = tokio::time::Instant::now() + std::time::Duration::from_secs(2); }
                    let _ = events.send(Event::AudioStatus { server: id.clone(), generation, message }).await;
                },
                None => { media.take(); retry_audio = tokio::time::Instant::now() + std::time::Duration::from_secs(2); let _ = events.send(Event::AudioStatus { server: id.clone(), generation, message: "Audio worker restarting…".into() }).await; },
            },
        }
    }
    if !finished {
        session.abort();
        let _ = session.await;
    }
}

const TALK_HOLD: std::time::Duration = std::time::Duration::from_millis(280);

async fn mark_talking(
    events: &mpsc::Sender<Event>,
    server: &str,
    generation: u64,
    speaking: &mut BTreeMap<u32, tokio::time::Instant>,
    session: u32,
    talking: bool,
) {
    let changed = if talking {
        speaking
            .insert(session, tokio::time::Instant::now())
            .is_none()
    } else {
        speaking.remove(&session).is_some()
    };
    if changed {
        let _ = events
            .send(Event::Talking {
                server: server.into(),
                generation,
                session,
                talking,
            })
            .await;
    }
}

async fn prepare(source: &str, html: bool) -> (String, MessageDocument) {
    let prepared = match workers::sibling("nuschel-content-worker") {
        Ok(worker) => nuschel_ipc::host::prepare_content(&worker, source, html)
            .await
            .ok(),
        Err(_) => None,
    };
    if let Some(prepared) = prepared {
        (prepared.source, prepared.document)
    } else {
        (
            source.into(),
            MessageDocument {
                blocks: vec![ContentBlock::Text {
                    text: source.into(),
                }],
            },
        )
    }
}
async fn content_loop(
    mut jobs: mpsc::Receiver<ContentJob>,
    storage: mpsc::Sender<StorageCommand>,
    events: mpsc::Sender<Event>,
) {
    while let Some(job) = jobs.recv().await {
        let (source, document) = prepare(&job.text.body, job.text.html).await;
        if storage
            .send(StorageCommand::Append {
                job,
                source,
                document,
            })
            .await
            .is_err()
        {
            let _ = events
                .send(Event::HistoryError(
                    "Local history worker has stopped.".into(),
                ))
                .await;
            break;
        }
    }
}
async fn storage_loop(
    directory: PathBuf,
    mut jobs: mpsc::Receiver<StorageCommand>,
    events: mpsc::Sender<Event>,
) {
    let mut storage: Option<Storage> = None;
    while let Some(job) = jobs.recv().await {
        if storage.is_none() {
            match Storage::start(&directory) {
                Ok(worker) => storage = Some(worker),
                Err(error) => {
                    let _ = events.send(Event::HistoryError(error.to_string())).await;
                }
            }
        }
        match job {
            StorageCommand::Append {
                job,
                source,
                document,
            } => {
                let storage_key = match &job.text.target {
                    TextTarget::Channel(id) => id.to_string(),
                    TextTarget::Direct(id) => job
                        .text
                        .peer
                        .as_ref()
                        .and_then(|peer| peer.certificate_hash.as_ref())
                        .map(|hash| format!("direct:{hash}"))
                        .unwrap_or_else(|| {
                            format!(
                                "direct:{}:{id}:{}",
                                job.nonce,
                                job.text
                                    .peer
                                    .as_ref()
                                    .map(|peer| peer.instance)
                                    .unwrap_or(0)
                            )
                        }),
                };
                let message = ChatMessage {
                    server: job.identity,
                    channel: storage_key.clone(),
                    author: job.text.author,
                    observed_at_ms: job.text.observed_at_ms,
                    body: source,
                };
                let id = if let Some(worker) = &mut storage {
                    match worker.append(message.clone()).await {
                        Ok(id) => Some(id),
                        Err(error) => {
                            let _ = events.send(Event::HistoryError(error.to_string())).await;
                            storage.take();
                            None
                        }
                    }
                } else {
                    None
                };
                let _ = events
                    .send(Event::Message {
                        server: job.server,
                        generation: job.generation,
                        target: job.text.target,
                        storage_key,
                        peer: job.text.peer,
                        message: RenderedMessage {
                            id,
                            message,
                            document,
                            outgoing: job.text.outgoing,
                        },
                    })
                    .await;
            }
            StorageCommand::Load {
                server,
                identity,
                channel,
                ticket,
                before,
            } => {
                let result = if let Some(worker) = &mut storage {
                    worker.page(identity, channel, before).await
                } else {
                    Err(Error::Worker("Local history is unavailable.".into()))
                };
                match result {
                    Ok(page) => {
                        let mut messages = Vec::new();
                        for entry in page.messages {
                            let (_, document) = prepare(&entry.message.body, false).await;
                            messages.push(RenderedMessage {
                                id: Some(entry.id),
                                message: entry.message,
                                document,
                                outgoing: false,
                            });
                        }
                        let _ = events
                            .send(Event::History {
                                server,
                                ticket,
                                messages,
                                next_before: page.next_before,
                            })
                            .await;
                    }
                    Err(error) => {
                        storage.take();
                        let _ = events.send(Event::HistoryError(error.to_string())).await;
                    }
                }
            }
        }
    }
    if let Some(worker) = storage {
        worker.stop().await;
    }
}
