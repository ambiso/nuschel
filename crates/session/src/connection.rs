// SPDX-License-Identifier: EUPL-1.2
use crate::{
    Error,
    config::{ClientIdentity, ServerBookmark},
};
use bytes::Bytes;
use nuschel_core::MAX_CONTENT_BYTES;
use nuschel_protocol::{
    ControlFrame, MAX_CONTROL_BYTES, control as proto,
    voice::{self, VoicePacket},
};
use prost::Message;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
    time::{self, Duration, Instant, timeout},
};

#[derive(Clone, Debug, Serialize)]
pub struct Channel {
    pub id: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub description: String,
    pub position: i32,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct User {
    pub instance: u64,
    pub session: u32,
    pub name: String,
    pub channel: u32,
    pub certificate_hash: Option<String>,
    pub muted: bool,
    pub deafened: bool,
    pub self_muted: bool,
    pub self_deafened: bool,
    pub suppressed: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum TextTarget {
    Channel(u32),
    Direct(u32),
}

#[derive(Clone, Debug)]
pub struct IncomingText {
    pub target: TextTarget,
    pub author: String,
    pub peer: Option<User>,
    pub body: String,
    pub html: bool,
    pub outgoing: bool,
    pub observed_at_ms: i64,
}
#[derive(Clone, Debug)]
pub enum SessionEvent {
    PeerCertificate(String),
    Channel(Channel),
    ChannelRemoved(u32),
    User(User),
    UserRemoved(u32),
    Connected { session: u32, max_bandwidth: u32 },
    Text(IncomingText),
    Welcome(String),
    PermissionDenied(String),
    Notice(String),
    Audio(VoicePacket),
}
#[derive(Debug)]
pub enum Command {
    Join(u32),
    CreateChannel {
        parent: u32,
        name: String,
    },
    SendText {
        target: TextTarget,
        body: String,
        expected_peer: Option<u64>,
    },
    SetSelf {
        muted: bool,
        deafened: bool,
    },
    Voice {
        target: u32,
        packet: VoicePacket,
    },
    Disconnect,
}
/// Passwords exist only in memory and are never part of a saved bookmark or Debug output.
pub struct ConnectOptions {
    pub server: ServerBookmark,
    pub identity: ClientIdentity,
    pub password: Option<String>,
}

pub async fn run(
    options: ConnectOptions,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<SessionEvent>,
) -> Result<(), Error> {
    let (stream, peer) = crate::tls::connect(&options.server, options.identity).await?;
    emit(&events, SessionEvent::PeerCertificate(peer)).await?;
    let (reader, mut writer) = tokio::io::split(stream);
    let (frames_tx, mut frames) = mpsc::channel(32);
    let read_task = tokio::spawn(async move {
        let mut reader = reader;
        loop {
            let frame = read_control(&mut reader).await;
            let failed = frame.is_err();
            if frames_tx.send(frame).await.is_err() || failed {
                break;
            }
        }
    });
    let result = async {
        send(&mut writer, 0, &proto::Version {
            version_v1: Some(0x0001_0500), version_v2: Some(voice::PROTOBUF_VERSION),
            release: Some(format!("Nuschel {}", env!("CARGO_PKG_VERSION"))),
            os: Some(std::env::consts::OS.into()), ..Default::default()
        }).await?;
        send(&mut writer, 2, &proto::Authenticate { username: Some(options.server.username), password: options.password,
            opus: Some(true), ..Default::default() }).await?;
        send(&mut writer, 9, &proto::UserState { self_mute: Some(true), self_deaf: Some(false), ..Default::default() }).await?;
        let mut state = State::default();
        let mut ping = time::interval(Duration::from_secs(5));
        ping.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        let deadline = time::sleep_until(Instant::now() + Duration::from_secs(20));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                frame = frames.recv() => {
                    let frame = frame.ok_or_else(|| Error::Protocol("connection reader stopped".into()))??;
                    state.handle(frame, &events).await?;
                },
                command = commands.recv() => {
                    let Some(command) = command else { break; };
                    match command {
                        Command::Disconnect => break,
                        Command::CreateChannel { parent, name } => if state.own_session.is_some() && state.channels.contains_key(&parent) && !name.is_empty() && name.len() <= 128 {
                            send(&mut writer, 7, &proto::ChannelState { parent: Some(parent), name: Some(name), temporary: Some(true), ..Default::default() }).await?;
                        },
                        Command::Join(channel) => if state.own_session.is_some() && state.channels.contains_key(&channel) {
                            send(&mut writer, 9, &proto::UserState { channel_id: Some(channel), ..Default::default() }).await?;
                        },
                        Command::SetSelf { muted, deafened } => {
                            state.local_muted = muted || deafened; state.local_deafened = deafened;
                            send(&mut writer, 9, &proto::UserState { self_mute: Some(muted || deafened), self_deaf: Some(deafened), ..Default::default() }).await?;
                        },
                        Command::SendText { target, body, expected_peer } => {
                            if state.own_session.is_none() { emit(&events, SessionEvent::Notice("Connect before sending a message.".into())).await?; continue; }
                            if body.trim().is_empty() || body.len() > MAX_CONTENT_BYTES {
                                emit(&events, SessionEvent::Notice("Message is empty or exceeds 64 KiB.".into())).await?; continue;
                            }
                            let wire_body = if state.allow_html { escape_html(&body) } else { body.clone() };
                            if wire_body.len() > state.message_limit {
                                emit(&events, SessionEvent::Notice(format!("Message exceeds this server's {}-byte limit.", state.message_limit))).await?; continue;
                            }
                            let mut message = proto::TextMessage { message: wire_body, ..Default::default() };
                            match target { TextTarget::Channel(id) => {
                                if !state.channels.contains_key(&id) { emit(&events, SessionEvent::Notice("This channel is no longer available.".into())).await?; continue; } message.channel_id.push(id);
                            }, TextTarget::Direct(id) => { if !state.users.get(&id).is_some_and(|user| expected_peer.is_none_or(|instance| instance == user.instance)) { emit(&events, SessionEvent::Notice("The recipient disconnected or changed. Open a new private conversation.".into())).await?; continue; } message.session.push(id); } }
                            send(&mut writer, 11, &message).await?;
                            let own = state.users.get(&state.own_session.unwrap()).cloned().unwrap_or_default();
                            let peer = match target { TextTarget::Direct(id) => state.users.get(&id).cloned(), _ => None };
                            emit(&events, SessionEvent::Text(IncomingText { target, author: own.name, peer,
                                body, html: false, outgoing: true, observed_at_ms: now_ms() })).await?;
                        },
                        Command::Voice { target, packet } => if state.own_session.is_some() && state.opus
                            && ((!state.local_muted && !state.local_deafened && state.may_speak()) || packet.end)
                            && let Ok(payload) = voice::encode_voice(state.version, target, &packet) { write_control(&mut writer, 1, &payload).await?; },
                    }
                },
                _ = ping.tick() => send(&mut writer, 3, &proto::Ping { timestamp: Some(now_ms() as u64), ..Default::default() }).await?,
                _ = &mut deadline, if state.own_session.is_none() => return Err(Error::Timeout("waiting for Mumble login")),
            }
        }
        let _ = timeout(Duration::from_secs(2), writer.shutdown()).await;
        Ok(())
    }.await;
    read_task.abort();
    let _ = read_task.await;
    result
}

struct State {
    user_instance: u64,
    channels: BTreeMap<u32, Channel>,
    users: BTreeMap<u32, User>,
    own_session: Option<u32>,
    version: u64,
    allow_html: bool,
    message_limit: usize,
    opus: bool,
    local_muted: bool,
    local_deafened: bool,
}
impl Default for State {
    fn default() -> Self {
        Self {
            user_instance: 0,
            channels: BTreeMap::new(),
            users: BTreeMap::new(),
            own_session: None,
            version: 0,
            allow_html: true,
            message_limit: 5000,
            opus: true,
            local_muted: true,
            local_deafened: false,
        }
    }
}
impl State {
    fn may_speak(&self) -> bool {
        self.own_session
            .and_then(|session| self.users.get(&session))
            .is_some_and(|user| {
                !user.muted
                    && !user.deafened
                    && !user.suppressed
                    && !user.self_muted
                    && !user.self_deafened
            })
    }
    async fn handle(
        &mut self,
        frame: ControlFrame,
        events: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), Error> {
        match frame.kind {
            0 => {
                let version: proto::Version = decode(&frame)?;
                self.version = version.version_v2.unwrap_or_else(|| {
                    let v = version.version_v1.unwrap_or(0);
                    ((u64::from(v) >> 16) << 48)
                        | (u64::from((v >> 8) & 0xff) << 32)
                        | (u64::from(v & 0xff) << 16)
                });
            }
            1 => {
                if !self.local_deafened
                    && let Ok(packet) = voice::decode_voice(&frame.payload)
                {
                    emit(events, SessionEvent::Audio(packet)).await?;
                }
            }
            3 => {
                let _: proto::Ping = decode(&frame)?;
            }
            4 => {
                let rejected: proto::Reject = decode(&frame)?;
                return Err(Error::Rejected(
                    rejected
                        .reason
                        .unwrap_or_else(|| "login was rejected".into()),
                ));
            }
            5 => {
                let sync: proto::ServerSync = decode(&frame)?;
                let session = sync
                    .session
                    .ok_or_else(|| Error::Protocol("ServerSync omitted session".into()))?;
                self.own_session = Some(session);
                emit(
                    events,
                    SessionEvent::Connected {
                        session,
                        max_bandwidth: sync.max_bandwidth.unwrap_or(72_000),
                    },
                )
                .await?;
                if let Some(welcome) = sync.welcome_text {
                    emit(
                        events,
                        SessionEvent::Welcome(bounded(welcome, MAX_CONTENT_BYTES)),
                    )
                    .await?;
                }
            }
            6 => {
                let channel: proto::ChannelRemove = decode(&frame)?;
                self.channels.remove(&channel.channel_id);
                emit(events, SessionEvent::ChannelRemoved(channel.channel_id)).await?;
            }
            7 => {
                let update: proto::ChannelState = decode(&frame)?;
                if let Some(id) = update.channel_id {
                    if !self.channels.contains_key(&id) && self.channels.len() >= 4096 {
                        return Err(Error::Protocol("channel limit exceeded".into()));
                    }
                    let channel = self.channels.entry(id).or_insert_with(|| Channel {
                        id,
                        parent: (id != 0).then_some(0),
                        name: format!("Channel {id}"),
                        description: String::new(),
                        position: 0,
                    });
                    if let Some(parent) = update.parent
                        && id != 0
                    {
                        channel.parent = Some(parent);
                    }
                    if let Some(name) = update.name {
                        channel.name = bounded(name, 512);
                    }
                    if let Some(description) = update.description {
                        channel.description = bounded(description, 4096);
                    }
                    if let Some(position) = update.position {
                        channel.position = position;
                    }
                    emit(events, SessionEvent::Channel(channel.clone())).await?;
                }
            }
            8 => {
                let remove: proto::UserRemove = decode(&frame)?;
                if Some(remove.session) == self.own_session {
                    return Err(Error::Removed(
                        remove.reason.unwrap_or_else(|| "session removed".into()),
                    ));
                }
                self.users.remove(&remove.session);
                emit(events, SessionEvent::UserRemoved(remove.session)).await?;
            }
            9 => {
                let update: proto::UserState = decode(&frame)?;
                if let Some(session) = update.session {
                    if !self.users.contains_key(&session) && self.users.len() >= 16384 {
                        return Err(Error::Protocol("user limit exceeded".into()));
                    }
                    if !self.users.contains_key(&session) {
                        self.user_instance += 1;
                    }
                    let instance = self.user_instance;
                    let user = self.users.entry(session).or_insert_with(|| User {
                        session,
                        instance,
                        name: format!("User {session}"),
                        ..Default::default()
                    });
                    if let Some(name) = update.name {
                        user.name = bounded(name, 512);
                    }
                    if let Some(channel) = update.channel_id {
                        user.channel = channel;
                    }
                    if let Some(value) = update.mute {
                        user.muted = value;
                    }
                    if let Some(value) = update.deaf {
                        user.deafened = value;
                    }
                    if let Some(value) = update.self_mute {
                        user.self_muted = value;
                    }
                    if let Some(value) = update.self_deaf {
                        user.self_deafened = value;
                    }
                    if let Some(value) = update.suppress {
                        user.suppressed = value;
                    }
                    if let Some(value) = update.hash {
                        user.certificate_hash = Some(bounded(value, 128));
                    }
                    emit(events, SessionEvent::User(user.clone())).await?;
                }
            }
            11 => {
                let message: proto::TextMessage = decode(&frame)?;
                if message.message.len() > MAX_CONTENT_BYTES {
                    emit(
                        events,
                        SessionEvent::Notice(
                            "Received a message exceeding the local 64 KiB limit.".into(),
                        ),
                    )
                    .await?;
                    return Ok(());
                }
                let actor = message.actor.and_then(|id| self.users.get(&id)).cloned();
                let author = actor
                    .as_ref()
                    .map(|user| user.name.clone())
                    .unwrap_or_else(|| "Server".into());
                let mut targets = message
                    .channel_id
                    .iter()
                    .filter(|id| self.channels.contains_key(id))
                    .copied()
                    .collect::<BTreeSet<_>>();
                if let Some(own) = self.own_session.and_then(|id| self.users.get(&id)) {
                    for root in message.tree_id {
                        let mut current = Some(own.channel);
                        let mut seen = BTreeSet::new();
                        while let Some(id) = current {
                            if !seen.insert(id) {
                                break;
                            }
                            if id == root {
                                targets.insert(own.channel);
                                break;
                            }
                            current = self.channels.get(&id).and_then(|channel| channel.parent);
                        }
                    }
                }
                let mut targets = targets
                    .into_iter()
                    .map(TextTarget::Channel)
                    .collect::<Vec<_>>();
                if targets.is_empty() {
                    targets.push(
                        actor
                            .as_ref()
                            .map(|user| TextTarget::Direct(user.session))
                            .unwrap_or(TextTarget::Channel(0)),
                    );
                }
                for target in targets {
                    emit(
                        events,
                        SessionEvent::Text(IncomingText {
                            target,
                            author: author.clone(),
                            peer: actor.clone(),
                            body: message.message.clone(),
                            html: self.allow_html,
                            outgoing: false,
                            observed_at_ms: now_ms(),
                        }),
                    )
                    .await?;
                }
            }
            12 => {
                let denied: proto::PermissionDenied = decode(&frame)?;
                emit(
                    events,
                    SessionEvent::PermissionDenied(
                        denied
                            .reason
                            .unwrap_or_else(|| format!("Permission denied ({:?})", denied.r#type)),
                    ),
                )
                .await?;
            }
            15 => { /* CryptSetup is unused while voice is tunneled through TLS. */ }
            21 => {
                let version: proto::CodecVersion = decode(&frame)?;
                self.opus = version.opus.unwrap_or(false);
                if !self.opus {
                    emit(
                        events,
                        SessionEvent::Notice(
                            "This server has disabled Opus; voice is unavailable.".into(),
                        ),
                    )
                    .await?;
                }
            }
            24 => {
                let config: proto::ServerConfig = decode(&frame)?;
                if let Some(html) = config.allow_html {
                    self.allow_html = html;
                }
                if let Some(limit) = config.message_length {
                    self.message_limit = if limit == 0 {
                        MAX_CONTENT_BYTES
                    } else {
                        (limit as usize).min(MAX_CONTENT_BYTES)
                    };
                }
            }
            _ => { /* Optional messages are framed and ignored without unsafe processing. */ }
        }
        Ok(())
    }
}
fn bounded(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}
fn decode<M: Message + Default>(frame: &ControlFrame) -> Result<M, Error> {
    M::decode(frame.payload.clone()).map_err(|error| Error::Protocol(error.to_string()))
}
async fn emit(events: &mpsc::Sender<SessionEvent>, event: SessionEvent) -> Result<(), Error> {
    events
        .send(event)
        .await
        .map_err(|_| Error::Protocol("client event receiver closed".into()))
}
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
pub fn escape_html(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for c in body.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("<br/>"),
            _ => out.push(c),
        }
    }
    out
}
async fn send<M: Message>(
    writer: &mut (impl AsyncWrite + Unpin),
    kind: u16,
    message: &M,
) -> Result<(), Error> {
    write_control(writer, kind, &message.encode_to_vec()).await
}
pub async fn write_control(
    writer: &mut (impl AsyncWrite + Unpin),
    kind: u16,
    payload: &[u8],
) -> Result<(), Error> {
    if payload.len() > MAX_CONTROL_BYTES {
        return Err(Error::Protocol("outgoing control frame too large".into()));
    }
    timeout(Duration::from_secs(10), async {
        writer.write_all(&kind.to_be_bytes()).await?;
        writer
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await?;
        writer.write_all(payload).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| Error::Timeout("writing control frame"))??;
    Ok(())
}
async fn read_control(reader: &mut (impl AsyncRead + Unpin)) -> Result<ControlFrame, Error> {
    let mut header = [0; 6];
    timeout(Duration::from_secs(30), reader.read_exact(&mut header))
        .await
        .map_err(|_| Error::Timeout("waiting for server traffic"))??;
    let size = u32::from_be_bytes(header[2..6].try_into().unwrap()) as usize;
    if size > MAX_CONTROL_BYTES {
        return Err(Error::Protocol("oversized control frame".into()));
    }
    let mut payload = vec![0; size];
    timeout(Duration::from_secs(10), reader.read_exact(&mut payload))
        .await
        .map_err(|_| Error::Timeout("reading control frame"))??;
    Ok(ControlFrame {
        kind: u16::from_be_bytes([header[0], header[1]]),
        payload: Bytes::from(payload),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn partial_user_updates_preserve_name_and_channel() {
        let mut state = State::default();
        let (tx, mut rx) = mpsc::channel(8);
        state
            .handle(
                ControlFrame {
                    kind: 9,
                    payload: proto::UserState {
                        session: Some(42),
                        name: Some("Ada".into()),
                        channel_id: Some(5),
                        ..Default::default()
                    }
                    .encode_to_vec()
                    .into(),
                },
                &tx,
            )
            .await
            .unwrap();
        state
            .handle(
                ControlFrame {
                    kind: 9,
                    payload: proto::UserState {
                        session: Some(42),
                        self_mute: Some(true),
                        ..Default::default()
                    }
                    .encode_to_vec()
                    .into(),
                },
                &tx,
            )
            .await
            .unwrap();
        rx.recv().await;
        let Some(SessionEvent::User(user)) = rx.recv().await else {
            panic!()
        };
        assert_eq!(user.name, "Ada");
        assert_eq!(user.channel, 5);
        assert!(user.self_muted);
    }
    #[test]
    fn literal_chat_survives_html_enabled_servers() {
        assert_eq!(
            escape_html("<script>&\n\n```rust\nfn main() {}\n```"),
            "&lt;script&gt;&amp;<br/><br/>```rust<br/>fn main() {}<br/>```"
        );
    }
    #[tokio::test]
    async fn rejects_oversized_frame_before_body_allocation() {
        let mut input = &[0_u8, 1, 255, 255, 255, 255][..];
        assert!(matches!(
            read_control(&mut input).await,
            Err(Error::Protocol(_))
        ));
    }
}
