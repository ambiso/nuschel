// SPDX-License-Identifier: EUPL-1.2
use crate::{
    Error,
    config::{ClientIdentity, ServerBookmark},
    model::{
        Acl, AclEntry, AclGroup, Activity, Ban, ChannelEdit, ContextAction, NameSpan, PacketStats,
        Presence, RegisteredUser, RejectKind, UserAction, UserStats, VoiceTargetSpec,
    },
    udp::{self, PingStats, Received, TransportStats, Udp},
};
use bytes::Bytes;
use nuschel_core::MAX_CONTENT_BYTES;
use nuschel_protocol::{
    ControlFrame, MAX_CONTROL_BYTES, control as proto,
    crypt::CryptState,
    voice::{self, UdpMessage, VoicePacket},
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

pub use crate::model::{Channel, User};
pub use nuschel_legacy_html::{escape_html, legacy_body, text_html};

/// Wire message types (Mumble's TCP control protocol).
mod kind {
    pub const VERSION: u16 = 0;
    pub const UDP_TUNNEL: u16 = 1;
    pub const AUTHENTICATE: u16 = 2;
    pub const PING: u16 = 3;
    pub const REJECT: u16 = 4;
    pub const SERVER_SYNC: u16 = 5;
    pub const CHANNEL_REMOVE: u16 = 6;
    pub const CHANNEL_STATE: u16 = 7;
    pub const USER_REMOVE: u16 = 8;
    pub const USER_STATE: u16 = 9;
    pub const BAN_LIST: u16 = 10;
    pub const TEXT_MESSAGE: u16 = 11;
    pub const PERMISSION_DENIED: u16 = 12;
    pub const ACL: u16 = 13;
    pub const QUERY_USERS: u16 = 14;
    pub const CRYPT_SETUP: u16 = 15;
    pub const CONTEXT_ACTION_MODIFY: u16 = 16;
    pub const CONTEXT_ACTION: u16 = 17;
    pub const USER_LIST: u16 = 18;
    pub const VOICE_TARGET: u16 = 19;
    pub const PERMISSION_QUERY: u16 = 20;
    pub const CODEC_VERSION: u16 = 21;
    pub const USER_STATS: u16 = 22;
    pub const REQUEST_BLOB: u16 = 23;
    pub const SERVER_CONFIG: u16 = 24;
    pub const SUGGEST_CONFIG: u16 = 25;
}

const MAX_BLOB_REQUEST: usize = 64;

/// Budgets against a hostile server. Stock Murmur defaults to 1,000 channels
/// and 100 users, and caps comments and descriptions at 5,000 characters
/// (128 KiB with images) and avatars at 128 KiB.
const MAX_CHANNELS: usize = 4096;
const MAX_USERS: usize = 16_384;
const MAX_LISTENING: usize = 256;
const MAX_AVATAR_BYTES: usize = 1024 * 1024;
/// Comments, channel descriptions and avatars held for one connection.
const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;
const CHANNELS_NOTICE: &str = "This server has more than 4,096 channels; the rest are not shown.";
const USERS_NOTICE: &str = "This server has more than 16,384 users; the rest are not shown.";
const BLOBS_NOTICE: &str = "This server sent more than 64 MiB of comments, descriptions and \
    avatars; further ones are not shown.";

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

#[derive(Clone, Debug, Serialize)]
pub struct ServerLimits {
    pub allow_html: bool,
    pub message_length: usize,
    pub image_message_length: usize,
    pub max_users: Option<u32>,
    pub recording_allowed: bool,
    pub max_bandwidth: Option<u32>,
}

#[derive(Clone, Debug)]
pub enum SessionEvent {
    PeerCertificate(String),
    Channel(Channel),
    ChannelRemoved(u32),
    User(User),
    UserRemoved(u32),
    Connected {
        session: u32,
        max_bandwidth: u32,
    },
    Text(IncomingText),
    Welcome(String),
    PermissionDenied(String),
    Notice(String),
    Audio(VoicePacket),
    /// UDP/TCP state and latency, emitted on changes and every few seconds.
    Transport(TransportStats),
    /// Effective permissions for a channel; `flush` invalidates all cached values first.
    Permissions {
        channel: u32,
        permissions: u32,
        flush: bool,
    },
    Activity(Activity),
    Limits(ServerLimits),
    Stats(UserStats),
    /// The server recommends settings; frontends decide whether to follow them.
    Suggest {
        push_to_talk: Option<bool>,
        positional: Option<bool>,
        minimum_version: Option<String>,
    },
    ContextActions(Vec<ContextAction>),
    RegisteredUsers(Vec<RegisteredUser>),
    Bans(Vec<Ban>),
    Acl(Acl),
    /// Resolved names for registered user ids (ACL editor).
    UserNames(BTreeMap<u32, String>),
    /// A voice packet from a legacy codec Nuschel does not decode.
    UnsupportedCodec {
        session: u32,
    },
    /// Text is waiting for the server's rate limit: `sent` of the `total`
    /// messages queued since the outbox was last empty have left. Reported
    /// once a message has to wait, and again (with `sent == total`) when the
    /// last one has left.
    Outbox {
        sent: usize,
        total: usize,
    },
}

#[derive(Debug)]
pub enum Command {
    Join(u32),
    CreateChannel {
        parent: u32,
        name: String,
        temporary: bool,
    },
    EditChannel {
        channel: u32,
        edit: ChannelEdit,
    },
    RemoveChannel(u32),
    LinkChannels {
        channel: u32,
        add: Vec<u32>,
        remove: Vec<u32>,
    },
    Listen {
        channel: u32,
        listen: bool,
    },
    SendText {
        target: TextTarget,
        body: String,
        expected_peer: Option<u64>,
    },
    /// Send Mumble rich text as-is (e.g. inline images), one message per
    /// entry and in order; only on HTML-enabled servers.
    SendHtml {
        target: TextTarget,
        html: Vec<String>,
        expected_peer: Option<u64>,
    },
    SetSelf {
        muted: bool,
        deafened: bool,
    },
    SetComment(String),
    SetAvatar(Vec<u8>),
    /// Register our own certificate under our current name.
    RegisterSelf,
    SetTokens(Vec<String>),
    User {
        session: u32,
        action: UserAction,
        reason: Option<String>,
    },
    RequestPermissions(u32),
    RequestBlobs {
        textures: Vec<u32>,
        comments: Vec<u32>,
        descriptions: Vec<u32>,
    },
    RequestStats(u32),
    SetVoiceTarget {
        id: u32,
        targets: Vec<VoiceTargetSpec>,
    },
    /// Send subsequent voice to this registered target (0 = normal talking).
    SelectVoiceTarget(u32),
    ContextAction {
        action: String,
        session: Option<u32>,
        channel: Option<u32>,
    },
    QueryRegisteredUsers,
    UpdateRegisteredUsers(Vec<RegisteredUser>),
    QueryBans,
    UpdateBans(Vec<Ban>),
    QueryAcl(u32),
    UpdateAcl(Acl),
    QueryUserNames(Vec<u32>),
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
    /// Never use UDP; voice stays in the TLS tunnel (Mumble's "Force TCP mode").
    pub force_tcp: bool,
    /// Restored after a reconnect; a fresh connection starts muted.
    pub muted: bool,
    pub deafened: bool,
    /// Rejoin this channel after login (reconnect).
    pub channel: Option<u32>,
    /// The server's rate limit for text, which it does not announce.
    pub message_rate: MessageRate,
}

/// Murmur's per-user rate limit (murmur.ini `messageburst` and `messagelimit`):
/// a leaky bucket shared by text, our own user state, channel state, ACL and
/// version messages. The server drops what exceeds it without telling the
/// sender, so text waits in an outbox for its turn instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageRate {
    pub burst: u32,
    pub per_second: u32,
}

impl Default for MessageRate {
    /// Stock Murmur: a burst of five, then one message a second.
    fn default() -> Self {
        Self {
            burst: 5,
            per_second: 1,
        }
    }
}

/// A text message waiting for the rate limit: `wire` is what the server
/// receives, `body` and `html` what our own timeline shows.
struct Outgoing {
    target: TextTarget,
    expected_peer: Option<u64>,
    wire: String,
    body: String,
    html: bool,
}

/// Text waiting for the server's rate limit, and our estimate of its bucket.
/// The estimate leaks a fifth slower than the server's bucket, which drops
/// fractions of a token whenever it drains, and text leaves one message of
/// room for the user state and other limited messages we send meanwhile.
struct Outbox {
    rate: MessageRate,
    level: f64,
    at: Instant,
    waiting: std::collections::VecDeque<Outgoing>,
    /// Messages queued since the outbox was last empty, how many have left,
    /// and whether this run was reported (a message had to wait).
    total: usize,
    sent: usize,
    reported: bool,
}

impl Outbox {
    fn new(rate: MessageRate) -> Self {
        Self {
            rate,
            level: 0.0,
            at: Instant::now(),
            waiting: std::collections::VecDeque::new(),
            total: 0,
            sent: 0,
            reported: false,
        }
    }
    fn leak_rate(&self) -> f64 {
        f64::from(self.rate.per_second.max(1)) * 0.8
    }
    /// Text may fill the bucket up to one message below the server's burst.
    fn capacity(&self) -> f64 {
        f64::from(self.rate.burst.saturating_sub(1).max(1))
    }
    fn leak(&mut self, now: Instant) {
        let leaked = now.saturating_duration_since(self.at).as_secs_f64() * self.leak_rate();
        self.level = (self.level - leaked).max(0.0);
        self.at = now;
    }
    /// Count a limited message that just left.
    fn charge(&mut self, now: Instant) {
        self.leak(now);
        self.level += 1.0;
    }
    fn push(&mut self, message: Outgoing) {
        self.waiting.push_back(message);
        self.total += 1;
    }
    /// The next message, if the bucket has room for it now.
    fn pop(&mut self, now: Instant) -> Option<Outgoing> {
        self.leak(now);
        if self.level + 1.0 > self.capacity() {
            return None;
        }
        let message = self.waiting.pop_front()?;
        self.sent += 1;
        Some(message)
    }
    /// When the next waiting message may leave (a millisecond late, so the
    /// bucket has certainly leaked enough by then).
    fn due(&self) -> Option<Instant> {
        let excess = (self.level + 1.0 - self.capacity()).max(0.0);
        (!self.waiting.is_empty()).then(|| {
            self.at + Duration::from_secs_f64(excess / self.leak_rate()) + Duration::from_millis(1)
        })
    }
    /// The progress to report, if any; an empty outbox starts a new run.
    fn progress(&mut self) -> Option<SessionEvent> {
        let event = SessionEvent::Outbox {
            sent: self.sent,
            total: self.total,
        };
        if self.waiting.is_empty() {
            let reported = std::mem::take(&mut self.reported);
            (self.total, self.sent) = (0, 0);
            reported.then_some(event)
        } else {
            self.reported = true;
            Some(event)
        }
    }
}

pub async fn run(
    options: ConnectOptions,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<SessionEvent>,
) -> Result<(), Error> {
    let (stream, peer, peer_addr) = crate::tls::connect(&options.server, options.identity).await?;
    emit(&events, SessionEvent::PeerCertificate(peer)).await?;
    let udp = Udp::bind(peer_addr, options.force_tcp).await.ok();
    let mut datagram = vec![0; udp::MAX_DATAGRAM];
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
        send(&mut writer, kind::VERSION, &proto::Version {
            version_v1: Some(0x0001_0500),
            version_v2: Some(voice::PROTOBUF_VERSION),
            release: Some(format!("Nuschel {}", env!("CARGO_PKG_VERSION"))),
            os: Some(std::env::consts::OS.into()),
            ..Default::default()
        })
        .await?;
        send(&mut writer, kind::AUTHENTICATE, &proto::Authenticate {
            username: Some(options.server.username.clone()),
            password: options.password,
            tokens: options.server.tokens.clone(),
            opus: Some(true),
            ..Default::default()
        })
        .await?;
        let muted = options.muted || options.deafened;
        send(&mut writer, kind::USER_STATE, &proto::UserState {
            self_mute: Some(muted),
            self_deaf: Some(options.deafened),
            ..Default::default()
        })
        .await?;
        let mut state = State {
            udp,
            local_muted: muted,
            local_deafened: options.deafened,
            rejoin: options.channel,
            outbox: Outbox::new(options.message_rate),
            ..State::default()
        };
        // The version and user state above count against the rate limit.
        state.outbox.level = 2.0;
        let mut ticks = 0_u64;
        let mut reported_udp = false;
        let mut ping = time::interval(Duration::from_secs(1));
        ping.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        let deadline = time::sleep_until(Instant::now() + Duration::from_secs(20));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                frame = frames.recv() => {
                    let frame = frame.ok_or_else(|| Error::Protocol("connection reader stopped".into()))??;
                    state.handle(frame, &events).await?;
                },
                received = async {
                    match &state.udp {
                        Some(udp) if udp.has_key() => udp.recv(&mut datagram).await,
                        _ => std::future::pending().await,
                    }
                } => {
                    let Ok(size) = received else { continue; };
                    let now = now_ms() as u64;
                    let Some(udp) = state.udp.as_mut() else { continue; };
                    match udp.handle(&datagram[..size], now) {
                        Received::Message(UdpMessage::Audio(packet)) => if !state.local_deafened {
                            emit(&events, SessionEvent::Audio(packet)).await?;
                        },
                        Received::Message(UdpMessage::Ping { .. }) | Received::Ignored => {},
                        Received::RequestResync => state.queue(kind::CRYPT_SETUP, &proto::CryptSetup::default()),
                    }
                    let active = state.udp.as_ref().is_some_and(Udp::active);
                    if active != reported_udp {
                        reported_udp = active;
                        emit(&events, SessionEvent::Transport(state.transport())).await?;
                    }
                },
                command = commands.recv() => {
                    let Some(command) = command else { break; };
                    match command {
                        Command::Disconnect => break,
                        Command::SelectVoiceTarget(id) => state.voice_target = id.min(30),
                        Command::Voice { target, packet } => if let Some(packet) = state.outgoing_voice(packet)
                            && let Ok(payload) = voice::encode_voice(state.version, if target == 0 { state.voice_target } else { target }, &packet) {
                            let sent = match state.udp.as_mut() { Some(udp) => udp.send_voice(&payload).await, None => false };
                            if !sent { write_control(&mut writer, kind::UDP_TUNNEL, &payload).await?; }
                        },
                        command => state.command(command, &events).await?,
                    }
                },
                _ = ping.tick() => {
                    ticks += 1;
                    let now = now_ms() as u64;
                    if let Some(udp) = state.udp.as_mut() {
                        udp.maybe_ping(state.version, now).await;
                        if udp.probe_failed() {
                            // The server may now send voice by UDP that never arrives.
                            // A tunneled datagram switches it back to the TCP tunnel.
                            write_control(&mut writer, kind::UDP_TUNNEL, &voice::encode_ping(state.version, now)).await?;
                        }
                    }
                    let active = state.udp.as_ref().is_some_and(Udp::active);
                    if ticks % 5 == 1 || active != reported_udp {
                        let stats = state.transport();
                        state.queue(kind::PING, &proto::Ping {
                            timestamp: Some(now),
                            good: Some(stats.good), late: Some(stats.late), lost: Some(stats.lost), resync: Some(stats.resync),
                            udp_packets: Some(stats.udp.packets), tcp_packets: Some(stats.tcp.packets),
                            udp_ping_avg: Some(stats.udp.average_ms), udp_ping_var: Some(stats.udp.variance_ms),
                            tcp_ping_avg: Some(stats.tcp.average_ms), tcp_ping_var: Some(stats.tcp.variance_ms),
                        });
                        reported_udp = active;
                        if state.own_session.is_some() {
                            emit(&events, SessionEvent::Transport(stats)).await?;
                        }
                    }
                },
                _ = time::sleep_until(state.outbox.due().unwrap_or_else(Instant::now)), if state.outbox.due().is_some() => {
                    state.flush_outbox(&events).await?;
                },
                _ = &mut deadline, if state.own_session.is_none() => return Err(Error::Timeout("waiting for Mumble login")),
            }
            for (kind, payload) in std::mem::take(&mut state.outbound) {
                write_control(&mut writer, kind, &payload).await?;
            }
        }
        let _ = timeout(Duration::from_secs(2), writer.shutdown()).await;
        Ok(())
    }
    .await;
    read_task.abort();
    let _ = read_task.await;
    result
}

struct State {
    udp: Option<Udp>,
    tcp_ping: PingStats,
    /// Control messages produced while handling a frame or command, flushed by the loop.
    outbound: Vec<(u16, Vec<u8>)>,
    user_instance: u64,
    channels: BTreeMap<u32, Channel>,
    users: BTreeMap<u32, User>,
    own_session: Option<u32>,
    version: u64,
    allow_html: bool,
    message_limit: usize,
    image_limit: usize,
    opus: bool,
    local_muted: bool,
    local_deafened: bool,
    rejoin: Option<u32>,
    voice_target: u32,
    permissions: BTreeMap<u32, u32>,
    context_actions: BTreeMap<String, ContextAction>,
    unsupported_codec: BTreeSet<u32>,
    /// Bytes of comments, descriptions and avatars in `users` and `channels`.
    blob_bytes: usize,
    /// Budget notices already shown on this connection.
    notices: BTreeSet<&'static str>,
    outbox: Outbox,
}
impl Default for State {
    fn default() -> Self {
        Self {
            udp: None,
            tcp_ping: PingStats::default(),
            outbound: Vec::new(),
            user_instance: 0,
            channels: BTreeMap::new(),
            users: BTreeMap::new(),
            own_session: None,
            version: 0,
            allow_html: true,
            message_limit: 5000,
            image_limit: 131_072,
            opus: true,
            local_muted: true,
            local_deafened: false,
            rejoin: None,
            voice_target: 0,
            permissions: BTreeMap::new(),
            context_actions: BTreeMap::new(),
            unsupported_codec: BTreeSet::new(),
            blob_bytes: 0,
            notices: BTreeSet::new(),
            outbox: Outbox::new(MessageRate::default()),
        }
    }
}
impl State {
    fn transport(&self) -> TransportStats {
        let crypt = self.udp.as_ref().map(Udp::crypt_stats).unwrap_or_default();
        TransportStats {
            udp_active: self.udp.as_ref().is_some_and(Udp::active),
            forced_tcp: self.udp.as_ref().is_none_or(|udp| udp.forced_tcp),
            udp: self.udp.as_ref().map(|udp| udp.stats).unwrap_or_default(),
            tcp: self.tcp_ping,
            good: crypt.good,
            late: crypt.late,
            lost: crypt.lost,
            resync: crypt.resync,
        }
    }
    fn queue<M: Message>(&mut self, kind: u16, message: &M) {
        // Murmur limits our own user state only; others' are counted too.
        if matches!(
            kind,
            kind::USER_STATE | kind::CHANNEL_STATE | kind::ACL | kind::TEXT_MESSAGE
        ) {
            self.outbox.charge(Instant::now());
        }
        self.outbound.push((kind, message.encode_to_vec()));
    }
    /// What may be sent of a voice packet. While the user may not speak, only
    /// a terminator leaves, and it carries silence instead of captured audio.
    fn outgoing_voice(&self, mut packet: VoicePacket) -> Option<VoicePacket> {
        if self.own_session.is_none() || !self.opus {
            return None;
        }
        if !self.local_muted && !self.local_deafened && self.may_speak() {
            return Some(packet);
        }
        packet.end.then(|| {
            packet.opus = nuschel_core::audio::SILENT_OPUS.to_vec();
            packet
        })
    }
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
    fn user_name(&self, session: Option<u32>) -> String {
        session
            .and_then(|session| self.users.get(&session))
            .map(|user| user.name.clone())
            .unwrap_or_else(|| "the server".into())
    }
    fn channel_name(&self, channel: u32) -> String {
        self.channels
            .get(&channel)
            .map(|channel| channel.name.clone())
            .unwrap_or_else(|| format!("channel {channel}"))
    }
    async fn activity(
        &self,
        events: &mpsc::Sender<SessionEvent>,
        channels: Vec<u32>,
        line: Line,
    ) -> Result<(), Error> {
        if self.own_session.is_none() {
            return Ok(());
        }
        emit(
            events,
            SessionEvent::Activity(Activity {
                channels,
                observed_at_ms: now_ms(),
                ..line.0
            }),
        )
        .await
    }
    /// Tell the user once per connection that a budget dropped server data.
    async fn notice_once(
        &mut self,
        events: &mpsc::Sender<SessionEvent>,
        notice: &'static str,
    ) -> Result<(), Error> {
        if self.notices.insert(notice) {
            emit(events, SessionEvent::Notice(notice.into())).await?;
        }
        Ok(())
    }

    fn logged_in(&self) -> bool {
        self.own_session.is_some()
    }

    async fn command(
        &mut self,
        command: Command,
        events: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), Error> {
        if !self.logged_in() {
            return emit(
                events,
                SessionEvent::Notice("Still connecting; try again in a moment.".into()),
            )
            .await;
        }
        let own = self.own_session.unwrap_or_default();
        match command {
            Command::Disconnect | Command::Voice { .. } | Command::SelectVoiceTarget(_) => {}
            Command::Join(channel) => {
                if self.channels.contains_key(&channel) {
                    self.queue(
                        kind::USER_STATE,
                        &proto::UserState {
                            session: Some(own),
                            channel_id: Some(channel),
                            ..Default::default()
                        },
                    );
                }
            }
            Command::CreateChannel {
                parent,
                name,
                temporary,
            } => {
                if self.channels.contains_key(&parent) && valid_name(&name) {
                    self.queue(
                        kind::CHANNEL_STATE,
                        &proto::ChannelState {
                            parent: Some(parent),
                            name: Some(name),
                            temporary: Some(temporary),
                            ..Default::default()
                        },
                    );
                }
            }
            Command::EditChannel { channel, edit } => {
                if self.channels.contains_key(&channel)
                    && edit.name.as_deref().is_none_or(valid_name)
                {
                    // Stock Mumble renders descriptions as HTML; the editor is plain
                    // text, so what was typed is what others see (as with chat).
                    // Kick and ban reasons stay plain: Mumble escapes those itself.
                    let description = edit.description.map(|text| {
                        let text = if self.allow_html {
                            text_html(&text)
                        } else {
                            text
                        };
                        bounded(text, MAX_CONTENT_BYTES)
                    });
                    self.queue(
                        kind::CHANNEL_STATE,
                        &proto::ChannelState {
                            channel_id: Some(channel),
                            name: edit.name,
                            description,
                            position: edit.position,
                            max_users: edit.max_users,
                            parent: edit.parent.filter(|parent| {
                                self.channels.contains_key(parent) && *parent != channel
                            }),
                            ..Default::default()
                        },
                    );
                }
            }
            Command::RemoveChannel(channel) => {
                if channel != 0 && self.channels.contains_key(&channel) {
                    self.queue(
                        kind::CHANNEL_REMOVE,
                        &proto::ChannelRemove {
                            channel_id: channel,
                        },
                    );
                }
            }
            Command::LinkChannels {
                channel,
                add,
                remove,
            } => {
                let known = |ids: Vec<u32>| {
                    ids.into_iter()
                        .filter(|id| self.channels.contains_key(id))
                        .take(256)
                        .collect::<Vec<_>>()
                };
                let (links_add, links_remove) = (known(add), known(remove));
                self.queue(
                    kind::CHANNEL_STATE,
                    &proto::ChannelState {
                        channel_id: Some(channel),
                        links_add,
                        links_remove,
                        ..Default::default()
                    },
                );
            }
            Command::Listen { channel, listen } => {
                if self.channels.contains_key(&channel) {
                    let mut message = proto::UserState {
                        session: Some(own),
                        ..Default::default()
                    };
                    if listen {
                        message.listening_channel_add.push(channel);
                    } else {
                        message.listening_channel_remove.push(channel);
                    }
                    self.queue(kind::USER_STATE, &message);
                }
            }
            Command::SetSelf { muted, deafened } => {
                self.local_muted = muted || deafened;
                self.local_deafened = deafened;
                self.queue(
                    kind::USER_STATE,
                    &proto::UserState {
                        self_mute: Some(muted || deafened),
                        self_deaf: Some(deafened),
                        ..Default::default()
                    },
                );
            }
            Command::SetComment(comment) => {
                let comment = if self.allow_html {
                    text_html(&comment)
                } else {
                    comment
                };
                if comment.len() > self.message_limit.max(1) {
                    return emit(
                        events,
                        SessionEvent::Notice(format!(
                            "Comments are limited to {} bytes on this server.",
                            self.message_limit
                        )),
                    )
                    .await;
                }
                self.queue(
                    kind::USER_STATE,
                    &proto::UserState {
                        session: Some(own),
                        comment: Some(comment),
                        ..Default::default()
                    },
                );
            }
            Command::SetAvatar(texture) => {
                if texture.len() > self.image_limit {
                    return emit(
                        events,
                        SessionEvent::Notice(format!(
                            "Avatars are limited to {} KiB on this server.",
                            self.image_limit / 1024
                        )),
                    )
                    .await;
                }
                self.queue(
                    kind::USER_STATE,
                    &proto::UserState {
                        session: Some(own),
                        texture: Some(texture),
                        ..Default::default()
                    },
                );
            }
            Command::SendHtml {
                target,
                html,
                expected_peer,
            } => {
                if !self.allow_html {
                    return emit(
                        events,
                        SessionEvent::Notice("This server does not accept rich text.".into()),
                    )
                    .await;
                }
                let limit = self
                    .image_limit
                    .max(self.message_limit)
                    .min(MAX_CONTENT_BYTES);
                if html
                    .iter()
                    .any(|html| html.is_empty() || html.len() > limit)
                {
                    return emit(
                        events,
                        SessionEvent::Notice("Message exceeds this server's limit.".into()),
                    )
                    .await;
                }
                for html in html {
                    self.outbox.push(Outgoing {
                        target: target.clone(),
                        expected_peer,
                        wire: html.clone(),
                        body: html,
                        html: true,
                    });
                }
                self.flush_outbox(events).await?;
            }
            Command::RegisterSelf => {
                self.queue(
                    kind::USER_STATE,
                    &proto::UserState {
                        session: Some(own),
                        user_id: Some(0),
                        ..Default::default()
                    },
                );
            }
            Command::SetTokens(tokens) => {
                let tokens = tokens
                    .into_iter()
                    .filter(|token| !token.is_empty() && token.len() <= 256)
                    .take(64)
                    .collect();
                self.queue(
                    kind::AUTHENTICATE,
                    &proto::Authenticate {
                        tokens,
                        ..Default::default()
                    },
                );
            }
            Command::User {
                session,
                action,
                reason,
            } => {
                if !self.users.contains_key(&session) {
                    return Ok(());
                }
                let reason = reason.map(|text| bounded(text, 1024));
                match action {
                    UserAction::Kick | UserAction::Ban => self.queue(
                        kind::USER_REMOVE,
                        &proto::UserRemove {
                            session,
                            reason,
                            ban: Some(action == UserAction::Ban),
                            ..Default::default()
                        },
                    ),
                    action => {
                        let mut message = proto::UserState {
                            session: Some(session),
                            ..Default::default()
                        };
                        match action {
                            UserAction::Mute(value) => message.mute = Some(value),
                            UserAction::Deafen(value) => message.deaf = Some(value),
                            UserAction::PrioritySpeaker(value) => {
                                message.priority_speaker = Some(value)
                            }
                            UserAction::Move(channel) => message.channel_id = Some(channel),
                            UserAction::Register => message.user_id = Some(0),
                            UserAction::ResetComment => message.comment = Some(String::new()),
                            UserAction::ResetAvatar => message.texture = Some(Vec::new()),
                            UserAction::Kick | UserAction::Ban => {}
                        }
                        self.queue(kind::USER_STATE, &message);
                    }
                }
            }
            Command::RequestPermissions(channel) => {
                if self.channels.contains_key(&channel) {
                    self.queue(
                        kind::PERMISSION_QUERY,
                        &proto::PermissionQuery {
                            channel_id: Some(channel),
                            ..Default::default()
                        },
                    );
                }
            }
            Command::RequestBlobs {
                textures,
                comments,
                descriptions,
            } => {
                let users = |ids: Vec<u32>| {
                    ids.into_iter()
                        .filter(|id| self.users.contains_key(id))
                        .take(MAX_BLOB_REQUEST)
                        .collect::<Vec<_>>()
                };
                let message = proto::RequestBlob {
                    session_texture: users(textures),
                    session_comment: users(comments),
                    channel_description: descriptions
                        .into_iter()
                        .filter(|id| self.channels.contains_key(id))
                        .take(MAX_BLOB_REQUEST)
                        .collect(),
                };
                if !(message.session_texture.is_empty()
                    && message.session_comment.is_empty()
                    && message.channel_description.is_empty())
                {
                    self.queue(kind::REQUEST_BLOB, &message);
                }
            }
            Command::RequestStats(session) => {
                if self.users.contains_key(&session) {
                    self.queue(
                        kind::USER_STATS,
                        &proto::UserStats {
                            session: Some(session),
                            stats_only: Some(false),
                            ..Default::default()
                        },
                    );
                }
            }
            Command::SetVoiceTarget { id, targets } => {
                if (1..=30).contains(&id) {
                    let targets = targets
                        .into_iter()
                        .take(32)
                        .map(|target| match target {
                            VoiceTargetSpec::Users(sessions) => proto::voice_target::Target {
                                session: sessions.into_iter().take(256).collect(),
                                ..Default::default()
                            },
                            VoiceTargetSpec::Channel {
                                channel,
                                links,
                                children,
                                group,
                            } => proto::voice_target::Target {
                                channel_id: Some(channel),
                                links: Some(links),
                                children: Some(children),
                                group,
                                ..Default::default()
                            },
                        })
                        .collect();
                    self.queue(
                        kind::VOICE_TARGET,
                        &proto::VoiceTarget {
                            id: Some(id),
                            targets,
                        },
                    );
                }
            }
            Command::ContextAction {
                action,
                session,
                channel,
            } => {
                if self.context_actions.contains_key(&action) {
                    self.queue(
                        kind::CONTEXT_ACTION,
                        &proto::ContextAction {
                            action,
                            session,
                            channel_id: channel,
                        },
                    );
                }
            }
            Command::QueryRegisteredUsers => {
                self.queue(kind::USER_LIST, &proto::UserList::default())
            }
            Command::UpdateRegisteredUsers(users) => self.queue(
                kind::USER_LIST,
                &proto::UserList {
                    users: users
                        .into_iter()
                        .take(10_000)
                        .map(|user| proto::user_list::User {
                            user_id: user.user_id,
                            name: user.name,
                            ..Default::default()
                        })
                        .collect(),
                },
            ),
            Command::QueryBans => self.queue(
                kind::BAN_LIST,
                &proto::BanList {
                    query: Some(true),
                    ..Default::default()
                },
            ),
            Command::UpdateBans(bans) => self.queue(
                kind::BAN_LIST,
                &proto::BanList {
                    bans: bans
                        .into_iter()
                        .take(10_000)
                        .map(|ban| proto::ban_list::BanEntry {
                            address: ban.address,
                            mask: ban.mask,
                            name: ban.name,
                            hash: ban.hash,
                            reason: ban.reason,
                            start: ban.start,
                            duration: ban.duration,
                        })
                        .collect(),
                    query: Some(false),
                },
            ),
            Command::QueryAcl(channel) => {
                if self.channels.contains_key(&channel) {
                    self.queue(
                        kind::ACL,
                        &proto::Acl {
                            channel_id: channel,
                            query: Some(true),
                            ..Default::default()
                        },
                    );
                }
            }
            Command::UpdateAcl(acl) => self.queue(
                kind::ACL,
                &proto::Acl {
                    channel_id: acl.channel,
                    inherit_acls: Some(acl.inherit_acls),
                    groups: acl
                        .groups
                        .into_iter()
                        .filter(|group| !group.inherited)
                        .map(|group| proto::acl::ChanGroup {
                            name: group.name,
                            inherited: Some(false),
                            inherit: Some(group.inherit),
                            inheritable: Some(group.inheritable),
                            add: group.add,
                            remove: group.remove,
                            inherited_members: Vec::new(),
                        })
                        .collect(),
                    acls: acl
                        .entries
                        .into_iter()
                        .filter(|entry| !entry.inherited)
                        .map(|entry| proto::acl::ChanAcl {
                            apply_here: Some(entry.apply_here),
                            apply_subs: Some(entry.apply_subs),
                            inherited: Some(false),
                            user_id: entry.user_id,
                            group: entry.group,
                            grant: Some(entry.grant),
                            deny: Some(entry.deny),
                        })
                        .collect(),
                    query: Some(false),
                },
            ),
            Command::QueryUserNames(ids) => {
                self.queue(
                    kind::QUERY_USERS,
                    &proto::QueryUsers {
                        ids: ids.into_iter().take(1024).collect(),
                        names: Vec::new(),
                    },
                );
            }
            Command::SendText {
                target,
                body,
                expected_peer,
            } => {
                if body.trim().is_empty() || body.len() > MAX_CONTENT_BYTES {
                    return emit(
                        events,
                        SessionEvent::Notice("Message is empty or exceeds 256 KiB.".into()),
                    )
                    .await;
                }
                let wire_body = legacy_body(&body, self.allow_html);
                if wire_body.len() > self.message_limit {
                    return emit(
                        events,
                        SessionEvent::Notice(format!(
                            "Message exceeds this server's {}-byte limit.",
                            self.message_limit
                        )),
                    )
                    .await;
                }
                self.outbox.push(Outgoing {
                    target,
                    expected_peer,
                    wire: wire_body,
                    body,
                    html: false,
                });
                self.flush_outbox(events).await?;
            }
        }
        Ok(())
    }

    /// Send the text the rate limit has room for, and report on what waits.
    async fn flush_outbox(&mut self, events: &mpsc::Sender<SessionEvent>) -> Result<(), Error> {
        while let Some(message) = self.outbox.pop(Instant::now()) {
            self.send_text(message, events).await?;
        }
        match self.outbox.progress() {
            Some(event) => emit(events, event).await,
            None => Ok(()),
        }
    }

    /// Queue a text message and report it as our own outgoing text.
    async fn send_text(
        &mut self,
        Outgoing {
            target,
            expected_peer,
            wire,
            body,
            html,
        }: Outgoing,
        events: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), Error> {
        let own = self.own_session.unwrap_or_default();
        let mut message = proto::TextMessage {
            message: wire,
            ..Default::default()
        };
        match target {
            TextTarget::Channel(id) => {
                if !self.channels.contains_key(&id) {
                    return emit(
                        events,
                        SessionEvent::Notice("This channel is no longer available.".into()),
                    )
                    .await;
                }
                message.channel_id.push(id);
            }
            TextTarget::Direct(id) => {
                if !self.users.get(&id).is_some_and(|user| {
                    expected_peer.is_none_or(|instance| instance == user.instance)
                }) {
                    return emit(events, SessionEvent::Notice("The recipient disconnected or changed. Open a new private conversation.".into())).await;
                }
                message.session.push(id);
            }
        }
        self.queue(kind::TEXT_MESSAGE, &message);
        let author = self
            .users
            .get(&own)
            .map(|user| user.name.clone())
            .unwrap_or_default();
        let peer = match target {
            TextTarget::Direct(id) => self.users.get(&id).cloned(),
            _ => None,
        };
        emit(
            events,
            SessionEvent::Text(IncomingText {
                target,
                author,
                peer,
                body,
                html,
                outgoing: true,
                observed_at_ms: now_ms(),
            }),
        )
        .await
    }

    async fn handle(
        &mut self,
        frame: ControlFrame,
        events: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), Error> {
        match frame.kind {
            kind::VERSION => {
                let version: proto::Version = decode(&frame)?;
                self.version = version.version_v2.unwrap_or_else(|| {
                    let v = version.version_v1.unwrap_or(0);
                    ((u64::from(v) >> 16) << 48)
                        | (u64::from((v >> 8) & 0xff) << 32)
                        | (u64::from(v & 0xff) << 16)
                });
            }
            kind::UDP_TUNNEL => match voice::decode_udp(&frame.payload) {
                Ok(UdpMessage::Audio(packet)) if !self.local_deafened => {
                    emit(events, SessionEvent::Audio(packet)).await?
                }
                Err(voice::VoiceError::Unsupported) => {
                    // CELT/Speex from a legacy client: tell the frontend once per user.
                    if let Some(sender) = legacy_sender(&frame.payload)
                        && self.unsupported_codec.insert(sender)
                    {
                        emit(events, SessionEvent::UnsupportedCodec { session: sender }).await?;
                    }
                }
                _ => {}
            },
            kind::PING => {
                let ping: proto::Ping = decode(&frame)?;
                let now = now_ms() as u64;
                if let Some(timestamp) = ping.timestamp.filter(|sent| *sent <= now) {
                    self.tcp_ping.record((now - timestamp) as f32);
                }
            }
            kind::REJECT => {
                let rejected: proto::Reject = decode(&frame)?;
                let kind = RejectKind::from_wire(rejected.r#type);
                return Err(Error::Rejected {
                    kind,
                    reason: rejected
                        .reason
                        .map(|reason| bounded(reason, 1024))
                        .unwrap_or_else(|| kind.describe().into()),
                });
            }
            kind::SERVER_SYNC => {
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
                if let Some(permissions) = sync.permissions {
                    self.permissions.insert(0, permissions as u32);
                    emit(
                        events,
                        SessionEvent::Permissions {
                            channel: 0,
                            permissions: permissions as u32,
                            flush: false,
                        },
                    )
                    .await?;
                }
                if let Some(welcome) = sync.welcome_text {
                    emit(
                        events,
                        SessionEvent::Welcome(bounded(welcome, MAX_CONTENT_BYTES)),
                    )
                    .await?;
                }
                if let Some(channel) = self.rejoin.take()
                    && self.channels.contains_key(&channel)
                {
                    self.queue(
                        kind::USER_STATE,
                        &proto::UserState {
                            session: Some(session),
                            channel_id: Some(channel),
                            ..Default::default()
                        },
                    );
                }
            }
            kind::CHANNEL_REMOVE => {
                let channel: proto::ChannelRemove = decode(&frame)?;
                if let Some(removed) = self.channels.remove(&channel.channel_id) {
                    self.blob_bytes -= removed.description.len();
                }
                self.permissions.remove(&channel.channel_id);
                for other in self.channels.values_mut() {
                    other.links.remove(&channel.channel_id);
                }
                emit(events, SessionEvent::ChannelRemoved(channel.channel_id)).await?;
            }
            kind::CHANNEL_STATE => {
                let update: proto::ChannelState = decode(&frame)?;
                let Some(id) = update.channel_id else {
                    return Ok(());
                };
                if !self.channels.contains_key(&id) && self.channels.len() >= MAX_CHANNELS {
                    return self.notice_once(events, CHANNELS_NOTICE).await;
                }
                let channel = self.channels.entry(id).or_insert_with(|| Channel {
                    id,
                    parent: (id != 0).then_some(0),
                    name: format!("Channel {id}"),
                    can_enter: true,
                    ..Default::default()
                });
                let held = self.blob_bytes - channel.description.len();
                let mut over_budget = false;
                if let Some(parent) = update.parent
                    && id != 0
                {
                    channel.parent = Some(parent);
                }
                if let Some(name) = update.name {
                    channel.name = bounded(name, 512);
                }
                if let Some(hash) = &update.description_hash {
                    channel.set_description_hash(hash);
                }
                if let Some(description) = update.description {
                    let description = bounded(description, MAX_CONTENT_BYTES);
                    over_budget = held + description.len() > MAX_BLOB_BYTES;
                    channel.description = if over_budget {
                        String::new()
                    } else {
                        description
                    };
                }
                self.blob_bytes = held + channel.description.len();
                if let Some(position) = update.position {
                    channel.position = position;
                }
                if let Some(temporary) = update.temporary {
                    channel.temporary = temporary;
                }
                if let Some(max_users) = update.max_users {
                    channel.max_users = max_users;
                }
                if let Some(value) = update.is_enter_restricted {
                    channel.enter_restricted = value;
                }
                if let Some(value) = update.can_enter {
                    channel.can_enter = value;
                }
                // `links` replaces the set; add/remove adjust it incrementally.
                if !update.links.is_empty() {
                    channel.links = update.links.iter().copied().collect();
                }
                channel.links.extend(update.links_add.iter().copied());
                for removed in &update.links_remove {
                    channel.links.remove(removed);
                }
                while channel.links.len() > MAX_CHANNELS {
                    channel.links.pop_last();
                }
                let snapshot = channel.clone();
                // Links are symmetric; mirror them so either side shows the link.
                for other in self.channels.values_mut().filter(|other| other.id != id) {
                    if snapshot.links.contains(&other.id) {
                        other.links.insert(id);
                    } else {
                        other.links.remove(&id);
                    }
                }
                emit(events, SessionEvent::Channel(snapshot)).await?;
                if over_budget {
                    self.notice_once(events, BLOBS_NOTICE).await?;
                }
            }
            kind::USER_REMOVE => {
                let remove: proto::UserRemove = decode(&frame)?;
                let actor = remove.actor.map(|actor| self.user_name(Some(actor)));
                let by = remove.actor.map(|actor| self.users.get(&actor).cloned());
                let reason = remove.reason.map(|reason| bounded(reason, 1024));
                let ban = remove.ban.unwrap_or(false);
                if Some(remove.session) == self.own_session {
                    return Err(Error::Removed { actor, reason, ban });
                }
                let Some(user) = self.users.remove(&remove.session) else {
                    return Ok(());
                };
                self.blob_bytes -= user.blob_bytes();
                let line = match by {
                    Some(by) => {
                        let line = Line::default()
                            .name(&user)
                            .text(if ban {
                                " was banned by "
                            } else {
                                " was kicked by "
                            })
                            .actor(by.as_ref());
                        match reason.as_deref() {
                            Some(reason) if !reason.is_empty() => line.text(": ").text(reason),
                            _ => line,
                        }
                    }
                    None => Line::presence(Presence::Disconnected)
                        .name(&user)
                        .text(" disconnected"),
                };
                self.activity(events, vec![user.channel], line).await?;
                emit(events, SessionEvent::UserRemoved(remove.session)).await?;
            }
            kind::USER_STATE => self.user_state(decode(&frame)?, events).await?,
            kind::TEXT_MESSAGE => {
                let message: proto::TextMessage = decode(&frame)?;
                if message.message.len() > MAX_CONTENT_BYTES {
                    emit(
                        events,
                        SessionEvent::Notice(
                            "Received a message exceeding the local 256 KiB limit.".into(),
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
            kind::PERMISSION_DENIED => {
                let denied: proto::PermissionDenied = decode(&frame)?;
                let text = denied
                    .reason
                    .map(|reason| bounded(reason, 1024))
                    .unwrap_or_else(|| match denied.r#type {
                        Some(2) => "Only the SuperUser may do that.".into(),
                        Some(3) => "That channel name is not allowed.".into(),
                        Some(4) => "That message is too long for this server.".into(),
                        Some(6) => {
                            "Temporary channels cannot have children or be edited that way.".into()
                        }
                        Some(7) => "You need a certificate to do that.".into(),
                        Some(8) => format!(
                            "The name \"{}\" is not allowed.",
                            denied.name.unwrap_or_default()
                        ),
                        Some(9) => "That channel is full.".into(),
                        Some(10) => "Channels cannot be nested that deeply.".into(),
                        Some(11) => "This server has reached its channel limit.".into(),
                        Some(12 | 13) => "The listener limit was reached.".into(),
                        _ => {
                            let permission = denied
                                .permission
                                .and_then(|bit| {
                                    crate::model::permission::NAMED
                                        .iter()
                                        .find(|(value, _)| *value == bit)
                                })
                                .map(|(_, name)| *name)
                                .unwrap_or("that action");
                            format!("Permission denied: {permission}")
                        }
                    });
                emit(events, SessionEvent::PermissionDenied(text)).await?;
            }
            kind::CRYPT_SETUP => {
                let setup: proto::CryptSetup = decode(&frame)?;
                match (setup.key, setup.client_nonce, setup.server_nonce) {
                    (Some(key), Some(client_nonce), Some(server_nonce)) => {
                        if let Some(udp) = self.udp.as_mut() {
                            let crypt = CryptState::client(&key, &client_nonce, &server_nonce)
                                .map_err(|_| Error::Protocol("invalid UDP key".into()))?;
                            udp.set_key(crypt);
                        }
                    }
                    (None, None, Some(server_nonce)) => {
                        if let Some(udp) = self.udp.as_mut() {
                            udp.set_decrypt_iv(&server_nonce);
                        }
                    }
                    (None, None, None) => {
                        // The server lost our send nonce and asks for it.
                        if let Some(iv) = self.udp.as_ref().and_then(Udp::encrypt_iv) {
                            self.queue(
                                kind::CRYPT_SETUP,
                                &proto::CryptSetup {
                                    client_nonce: Some(iv.to_vec()),
                                    ..Default::default()
                                },
                            );
                        }
                    }
                    _ => {}
                }
            }
            kind::CODEC_VERSION => {
                let version: proto::CodecVersion = decode(&frame)?;
                self.opus = version.opus.unwrap_or(false);
                if !self.opus {
                    emit(events, SessionEvent::Notice(
                        "This server uses the legacy CELT codec. Nuschel supports Opus only, so voice is unavailable here; text chat still works.".into(),
                    ))
                    .await?;
                }
            }
            kind::SERVER_CONFIG => {
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
                if let Some(limit) = config.image_message_length {
                    self.image_limit = if limit == 0 {
                        1024 * 1024
                    } else {
                        (limit as usize).min(1024 * 1024)
                    };
                }
                emit(
                    events,
                    SessionEvent::Limits(ServerLimits {
                        allow_html: self.allow_html,
                        message_length: self.message_limit,
                        image_message_length: self.image_limit,
                        max_users: config.max_users,
                        recording_allowed: config.recording_allowed.unwrap_or(true),
                        max_bandwidth: config.max_bandwidth,
                    }),
                )
                .await?;
            }
            kind::PERMISSION_QUERY => {
                let query: proto::PermissionQuery = decode(&frame)?;
                let flush = query.flush.unwrap_or(false);
                if flush {
                    self.permissions.clear();
                }
                // Only known channels: the map cannot grow past the channel budget.
                if let (Some(channel), Some(permissions)) = (query.channel_id, query.permissions)
                    && self.channels.contains_key(&channel)
                {
                    self.permissions.insert(channel, permissions);
                    emit(
                        events,
                        SessionEvent::Permissions {
                            channel,
                            permissions,
                            flush,
                        },
                    )
                    .await?;
                } else if flush {
                    emit(
                        events,
                        SessionEvent::Permissions {
                            channel: u32::MAX,
                            permissions: 0,
                            flush,
                        },
                    )
                    .await?;
                }
            }
            kind::USER_STATS => {
                let stats: proto::UserStats = decode(&frame)?;
                let packet = |stats: Option<proto::user_stats::Stats>| {
                    stats.map(|stats| PacketStats {
                        good: stats.good.unwrap_or(0),
                        late: stats.late.unwrap_or(0),
                        lost: stats.lost.unwrap_or(0),
                        resync: stats.resync.unwrap_or(0),
                    })
                };
                let version = stats.version.as_ref();
                emit(
                    events,
                    SessionEvent::Stats(UserStats {
                        session: stats.session.unwrap_or(0),
                        release: version
                            .and_then(|v| v.release.clone())
                            .map(|text| bounded(text, 256)),
                        os: version.and_then(|v| {
                            let os = v.os.clone()?;
                            Some(bounded(
                                format!("{os} {}", v.os_version.clone().unwrap_or_default())
                                    .trim()
                                    .to_string(),
                                256,
                            ))
                        }),
                        version: version.and_then(|v| {
                            v.version_v2.map(format_version).or_else(|| {
                                v.version_v1.map(|v1| format_version(legacy_version(v1)))
                            })
                        }),
                        address: stats.address.as_deref().and_then(format_address),
                        online_seconds: stats.onlinesecs,
                        idle_seconds: stats.idlesecs,
                        bandwidth: stats.bandwidth,
                        udp_ping_ms: stats.udp_ping_avg,
                        tcp_ping_ms: stats.tcp_ping_avg,
                        udp_packets: stats.udp_packets,
                        tcp_packets: stats.tcp_packets,
                        from_client: packet(stats.from_client),
                        from_server: packet(stats.from_server),
                        opus: stats.opus,
                        strong_certificate: stats.strong_certificate.unwrap_or(false),
                        certificate_count: stats.certificates.len(),
                    }),
                )
                .await?;
            }
            kind::SUGGEST_CONFIG => {
                let suggest: proto::SuggestConfig = decode(&frame)?;
                let minimum = suggest
                    .version_v2
                    .or_else(|| suggest.version_v1.map(legacy_version))
                    .filter(|version| *version > voice::PROTOBUF_VERSION)
                    .map(format_version);
                emit(
                    events,
                    SessionEvent::Suggest {
                        push_to_talk: suggest.push_to_talk,
                        positional: suggest.positional,
                        minimum_version: minimum,
                    },
                )
                .await?;
            }
            kind::CONTEXT_ACTION_MODIFY => {
                let modify: proto::ContextActionModify = decode(&frame)?;
                if modify.action.len() > 256 {
                    return Ok(());
                }
                if modify.operation == Some(1) {
                    self.context_actions.remove(&modify.action);
                } else if self.context_actions.len() < 64 {
                    let context = modify.context.unwrap_or(0);
                    self.context_actions.insert(
                        modify.action.clone(),
                        ContextAction {
                            text: bounded(
                                modify.text.unwrap_or_else(|| modify.action.clone()),
                                256,
                            ),
                            action: modify.action,
                            server: context & 1 != 0,
                            channel: context & 2 != 0,
                            user: context & 4 != 0,
                        },
                    );
                }
                emit(
                    events,
                    SessionEvent::ContextActions(self.context_actions.values().cloned().collect()),
                )
                .await?;
            }
            kind::USER_LIST => {
                let list: proto::UserList = decode(&frame)?;
                emit(
                    events,
                    SessionEvent::RegisteredUsers(
                        list.users
                            .into_iter()
                            .take(100_000)
                            .map(|user| RegisteredUser {
                                user_id: user.user_id,
                                name: user.name.map(|name| bounded(name, 512)),
                                last_seen: user.last_seen.map(|text| bounded(text, 64)),
                                last_channel: user.last_channel,
                            })
                            .collect(),
                    ),
                )
                .await?;
            }
            kind::BAN_LIST => {
                let list: proto::BanList = decode(&frame)?;
                emit(
                    events,
                    SessionEvent::Bans(
                        list.bans
                            .into_iter()
                            .take(100_000)
                            .map(|ban| Ban {
                                address: ban.address,
                                mask: ban.mask,
                                name: ban.name.map(|text| bounded(text, 512)),
                                hash: ban.hash.map(|text| bounded(text, 128)),
                                reason: ban.reason.map(|text| bounded(text, 1024)),
                                start: ban.start.map(|text| bounded(text, 64)),
                                duration: ban.duration,
                            })
                            .collect(),
                    ),
                )
                .await?;
            }
            kind::ACL => {
                let acl: proto::Acl = decode(&frame)?;
                emit(
                    events,
                    SessionEvent::Acl(Acl {
                        channel: acl.channel_id,
                        inherit_acls: acl.inherit_acls.unwrap_or(true),
                        groups: acl
                            .groups
                            .into_iter()
                            .take(1024)
                            .map(|group| AclGroup {
                                name: bounded(group.name, 256),
                                inherited: group.inherited.unwrap_or(true),
                                inherit: group.inherit.unwrap_or(true),
                                inheritable: group.inheritable.unwrap_or(true),
                                add: group.add,
                                remove: group.remove,
                                inherited_members: group.inherited_members,
                            })
                            .collect(),
                        entries: acl
                            .acls
                            .into_iter()
                            .take(4096)
                            .map(|entry| AclEntry {
                                apply_here: entry.apply_here.unwrap_or(true),
                                apply_subs: entry.apply_subs.unwrap_or(true),
                                inherited: entry.inherited.unwrap_or(true),
                                user_id: entry.user_id,
                                group: entry.group.map(|group| bounded(group, 256)),
                                grant: entry.grant.unwrap_or(0),
                                deny: entry.deny.unwrap_or(0),
                            })
                            .collect(),
                    }),
                )
                .await?;
            }
            kind::QUERY_USERS => {
                let query: proto::QueryUsers = decode(&frame)?;
                emit(
                    events,
                    SessionEvent::UserNames(
                        query
                            .ids
                            .into_iter()
                            .zip(query.names)
                            .take(4096)
                            .map(|(id, name)| (id, bounded(name, 512)))
                            .collect(),
                    ),
                )
                .await?;
            }
            _ => { /* Optional messages (e.g. plugin data) are framed and ignored. */ }
        }
        Ok(())
    }

    async fn user_state(
        &mut self,
        update: proto::UserState,
        events: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), Error> {
        let Some(session) = update.session else {
            return Ok(());
        };
        let known = self.users.contains_key(&session);
        if !known && self.users.len() >= MAX_USERS {
            return self.notice_once(events, USERS_NOTICE).await;
        }
        if !known {
            self.user_instance += 1;
        }
        let instance = self.user_instance;
        let own = Some(session) == self.own_session;
        let actor = update
            .actor
            .filter(|actor| *actor != session && Some(*actor) != self.own_session.filter(|_| own));
        // Who did it, if someone else; None inside is the server.
        let by = update
            .actor
            .filter(|actor| *actor != session)
            .map(|actor| self.users.get(&actor).cloned());
        let mut log = Vec::new();
        let user = self.users.entry(session).or_insert_with(|| User {
            session,
            instance,
            name: format!("User {session}"),
            ..Default::default()
        });
        let previous = user.clone();
        // Blobs held for everyone else; this user's are counted again below.
        let held = self.blob_bytes - previous.blob_bytes();
        let mut over_budget = false;
        if let Some(name) = update.name {
            user.name = bounded(name, 512);
        }
        if let Some(id) = update.user_id {
            user.user_id = Some(id);
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
        if let Some(value) = update.priority_speaker {
            user.priority_speaker = value;
        }
        if let Some(value) = update.recording {
            user.recording = value;
        }
        if let Some(value) = update.hash {
            user.certificate_hash = Some(bounded(value, 128));
        }
        if let Some(hash) = &update.comment_hash {
            user.set_comment_hash(hash);
        }
        if let Some(comment) = update.comment {
            user.comment = None;
            if comment.is_empty() {
                user.comment_hash = None;
            } else {
                let comment = bounded(comment, MAX_CONTENT_BYTES);
                // Over budget, the hash stays: the comment exists but is not held.
                if held + user.blob_bytes() + comment.len() > MAX_BLOB_BYTES {
                    over_budget = true;
                } else {
                    user.comment = Some(comment);
                }
            }
        }
        if let Some(hash) = &update.texture_hash {
            user.set_texture_hash(hash);
        }
        if let Some(texture) = update.texture {
            if texture.is_empty() {
                user.texture = None;
                user.texture_hash = None;
            } else if texture.len() <= MAX_AVATAR_BYTES {
                user.texture = None;
                if held + user.blob_bytes() + texture.len() > MAX_BLOB_BYTES {
                    over_budget = true;
                } else {
                    user.texture = Some(texture);
                }
            }
        }
        self.blob_bytes = held + user.blob_bytes();
        user.listening
            .extend(update.listening_channel_add.iter().copied());
        for channel in &update.listening_channel_remove {
            user.listening.remove(channel);
        }
        while user.listening.len() > MAX_LISTENING {
            user.listening.pop_last();
        }
        let user = user.clone();
        if self.own_session.is_some() {
            let done = |verb: &str| {
                let line = Line::default().name(&user).text(verb);
                match &by {
                    Some(by) => line.text(" by ").actor(by.as_ref()),
                    None => line,
                }
            };
            if !known {
                log.push((
                    vec![user.channel],
                    Line::presence(Presence::Connected)
                        .name(&user)
                        .text(" connected"),
                ));
            } else if previous.channel != user.channel {
                let destination = self.channel_name(user.channel);
                let line = if own {
                    Line::default().text(if by.is_some() {
                        "You were moved to "
                    } else {
                        "You joined "
                    })
                } else {
                    Line::default().name(&user).text(if by.is_some() {
                        " was moved to "
                    } else {
                        " moved to "
                    })
                }
                .text(&destination);
                let line = match &by {
                    Some(by) => line.text(" by ").actor(by.as_ref()),
                    None => line,
                };
                log.push((vec![previous.channel, user.channel], line));
            }
            if known && !own && previous.name != user.name {
                // The new name is plain text: a nickname stands for the person.
                log.push((
                    vec![user.channel],
                    Line::presence(Presence::Renamed)
                        .name(&previous)
                        .text(" renamed to ")
                        .text(&user.name),
                ));
            }
            if known && previous.muted != user.muted {
                log.push((
                    vec![user.channel],
                    done(if user.muted {
                        " was muted"
                    } else {
                        " was unmuted"
                    }),
                ));
            }
            if known && previous.deafened != user.deafened {
                log.push((
                    vec![user.channel],
                    done(if user.deafened {
                        " was deafened"
                    } else {
                        " was undeafened"
                    }),
                ));
            }
            if known && previous.suppressed != user.suppressed && actor.is_some() {
                log.push((
                    vec![user.channel],
                    done(if user.suppressed {
                        " was suppressed"
                    } else {
                        " was unsuppressed"
                    }),
                ));
            }
            if known && previous.priority_speaker != user.priority_speaker {
                log.push((
                    vec![user.channel],
                    Line::default().name(&user).text(if user.priority_speaker {
                        " is now a priority speaker"
                    } else {
                        " is no longer a priority speaker"
                    }),
                ));
            }
            if previous.recording != user.recording {
                log.push((
                    vec![user.channel],
                    Line::default().name(&user).text(if user.recording {
                        " started recording"
                    } else {
                        " stopped recording"
                    }),
                ));
            }
            if known && previous.user_id.is_none() && user.user_id.is_some() {
                log.push((
                    Vec::new(),
                    Line::default()
                        .name(&user)
                        .text(" is now registered on this server"),
                ));
            }
        }
        emit(events, SessionEvent::User(user)).await?;
        for (channels, line) in log {
            self.activity(events, channels, line).await?;
        }
        if over_budget {
            self.notice_once(events, BLOBS_NOTICE).await?;
        }
        Ok(())
    }
}

/// An activity line being written, recording where it names people.
#[derive(Default)]
struct Line(Activity);

impl Line {
    fn presence(presence: Presence) -> Self {
        Self(Activity {
            presence: Some(presence),
            ..Activity::default()
        })
    }
    fn text(mut self, text: &str) -> Self {
        self.0.text.push_str(text);
        self
    }
    fn name(mut self, user: &User) -> Self {
        let start = self.0.text.len();
        self.0.text.push_str(&user.name);
        self.0.names.push(NameSpan {
            start,
            end: self.0.text.len(),
            certificate_hash: user.certificate_hash.clone(),
        });
        self
    }
    /// Whoever acted: a user, or the server (also for a session not known).
    fn actor(self, actor: Option<&User>) -> Self {
        match actor {
            Some(actor) => self.name(actor),
            None => self.text("the server"),
        }
    }
}

/// The sender session of a legacy (non-Opus) voice packet, for a one-time notice.
fn legacy_sender(payload: &[u8]) -> Option<u32> {
    let (&header, rest) = payload.split_first()?;
    if matches!(header >> 5, 0 | 2 | 3) {
        let first = *rest.first()?;
        // Session ids are small; decode only the 1- and 2-byte varint forms.
        return match first {
            0..=0x7f => Some(u32::from(first)),
            0x80..=0xbf => Some((u32::from(first & 0x3f) << 8) | u32::from(*rest.get(1)?)),
            _ => None,
        };
    }
    None
}

fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 512 && !name.chars().any(char::is_control)
}

fn legacy_version(v1: u32) -> u64 {
    ((u64::from(v1) >> 16) << 48)
        | (u64::from((v1 >> 8) & 0xff) << 32)
        | (u64::from(v1 & 0xff) << 16)
}

fn format_version(version: u64) -> String {
    format!(
        "{}.{}.{}",
        version >> 48,
        (version >> 32) & 0xffff,
        (version >> 16) & 0xffff
    )
}

fn format_address(address: &[u8]) -> Option<String> {
    let octets: [u8; 16] = address.try_into().ok()?;
    let ip = std::net::Ipv6Addr::from(octets);
    Some(match ip.to_ipv4_mapped() {
        Some(v4) => v4.to_string(),
        None => ip.to_string(),
    })
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
    let size = u32::from_be_bytes([header[2], header[3], header[4], header[5]]) as usize;
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

    fn frame<M: Message>(kind: u16, message: M) -> ControlFrame {
        ControlFrame {
            kind,
            payload: message.encode_to_vec().into(),
        }
    }

    async fn drain(rx: &mut mpsc::Receiver<SessionEvent>) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    #[tokio::test]
    async fn partial_user_updates_preserve_name_and_channel() {
        let mut state = State::default();
        let (tx, mut rx) = mpsc::channel(8);
        state
            .handle(
                frame(
                    kind::USER_STATE,
                    proto::UserState {
                        session: Some(42),
                        name: Some("Ada".into()),
                        channel_id: Some(5),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        state
            .handle(
                frame(
                    kind::USER_STATE,
                    proto::UserState {
                        session: Some(42),
                        self_mute: Some(true),
                        ..Default::default()
                    },
                ),
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

    #[tokio::test]
    async fn moderation_and_presence_become_activity_lines() {
        let mut state = State {
            own_session: Some(1),
            ..State::default()
        };
        let (tx, mut rx) = mpsc::channel(64);
        for (session, name) in [(1, "Me"), (2, "Ada"), (3, "Mod")] {
            state
                .handle(
                    frame(
                        kind::USER_STATE,
                        proto::UserState {
                            session: Some(session),
                            name: Some(name.into()),
                            hash: Some(format!("{name}-hash")),
                            ..Default::default()
                        },
                    ),
                    &tx,
                )
                .await
                .unwrap();
        }
        for update in [
            proto::UserState {
                session: Some(2),
                actor: Some(3),
                mute: Some(true),
                ..Default::default()
            },
            proto::UserState {
                session: Some(3),
                name: Some("Moderator".into()),
                ..Default::default()
            },
        ] {
            state
                .handle(frame(kind::USER_STATE, update), &tx)
                .await
                .unwrap();
        }
        state
            .handle(
                frame(
                    kind::USER_REMOVE,
                    proto::UserRemove {
                        session: 2,
                        actor: Some(3),
                        reason: Some("spam".into()),
                        ban: Some(true),
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        let activities: Vec<Activity> = drain(&mut rx)
            .await
            .into_iter()
            .filter_map(|event| match event {
                SessionEvent::Activity(activity) => Some(activity),
                _ => None,
            })
            .collect();
        let lines: Vec<&str> = activities.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(
            lines,
            [
                "Me connected",
                "Ada connected",
                "Mod connected",
                "Ada was muted by Mod",
                "Mod renamed to Moderator",
                "Ada was banned by Moderator: spam"
            ]
        );
        // Where each line names someone, for local nicknames.
        let span = |start, end, name: &str| NameSpan {
            start,
            end,
            certificate_hash: Some(format!("{name}-hash")),
        };
        assert_eq!(
            activities[3].names,
            [span(0, 3, "Ada"), span(17, 20, "Mod")]
        );
        assert_eq!(activities[4].names, [span(0, 3, "Mod")]);
        assert_eq!(activities[5].names[1], span(18, 27, "Mod"));
        let presence: Vec<_> = activities.iter().map(|line| line.presence).collect();
        assert_eq!(
            presence,
            [
                Some(Presence::Connected),
                Some(Presence::Connected),
                Some(Presence::Connected),
                None,
                Some(Presence::Renamed),
                None
            ]
        );
        let error = state
            .handle(
                frame(
                    kind::USER_REMOVE,
                    proto::UserRemove {
                        session: 1,
                        actor: Some(3),
                        reason: Some("bye".into()),
                        ban: Some(false),
                    },
                ),
                &tx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Removed { actor: Some(ref actor), ban: false, .. } if actor == "Moderator")
        );
    }

    #[tokio::test]
    async fn reject_types_are_reported_for_retry_prompts() {
        let mut state = State::default();
        let (tx, _rx) = mpsc::channel(8);
        let error = state
            .handle(
                frame(
                    kind::REJECT,
                    proto::Reject {
                        r#type: Some(4),
                        reason: None,
                    },
                ),
                &tx,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Rejected {
                kind: RejectKind::WrongServerPassword,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_hostile_server_cannot_grow_the_session_without_bound() {
        let mut state = State::default();
        let (tx, mut rx) = mpsc::channel(16);
        let mut notices = Vec::new();
        let mut send = async |state: &mut State, kind: u16, payload: Vec<u8>| {
            let frame = ControlFrame {
                kind,
                payload: payload.into(),
            };
            state.handle(frame, &tx).await.unwrap();
            for event in drain(&mut rx).await {
                if let SessionEvent::Notice(text) = event {
                    notices.push(text);
                }
            }
        };
        let channel = |id, description: Option<String>, links_add: Vec<u32>| {
            proto::ChannelState {
                channel_id: Some(id),
                description,
                links_add,
                ..Default::default()
            }
            .encode_to_vec()
        };
        let user = |session, texture: Option<Vec<u8>>| {
            proto::UserState {
                session: Some(session),
                texture,
                ..Default::default()
            }
            .encode_to_vec()
        };
        for id in 0..MAX_CHANNELS as u32 + 2 {
            send(&mut state, kind::CHANNEL_STATE, channel(id, None, vec![])).await;
        }
        assert_eq!(state.channels.len(), MAX_CHANNELS);
        // Links arrive in many messages; the set stays bounded.
        for _ in 0..2 {
            let links = (0..MAX_CHANNELS as u32 + 100).collect();
            send(&mut state, kind::CHANNEL_STATE, channel(1, None, links)).await;
        }
        assert_eq!(state.channels[&1].links.len(), MAX_CHANNELS);
        for session in 0..MAX_USERS as u32 + 2 {
            send(&mut state, kind::USER_STATE, user(session, None)).await;
        }
        assert_eq!(state.users.len(), MAX_USERS);
        // Avatars up to the total budget; the next is dropped, its hash kept.
        let avatar = vec![7; MAX_AVATAR_BYTES];
        let fit = (MAX_BLOB_BYTES / MAX_AVATAR_BYTES) as u32;
        for session in 0..=fit {
            send(
                &mut state,
                kind::USER_STATE,
                user(session, Some(avatar.clone())),
            )
            .await;
        }
        assert_eq!(state.blob_bytes, MAX_BLOB_BYTES);
        assert!(state.users[&fit].texture.is_none());
        let description = Some("d".repeat(100));
        send(
            &mut state,
            kind::CHANNEL_STATE,
            channel(2, description, vec![]),
        )
        .await;
        assert!(state.channels[&2].description.is_empty());
        // Leaving users free their share.
        let remove = proto::UserRemove {
            session: 0,
            ..Default::default()
        };
        send(&mut state, kind::USER_REMOVE, remove.encode_to_vec()).await;
        send(
            &mut state,
            kind::USER_STATE,
            user(fit, Some(avatar.clone())),
        )
        .await;
        assert!(state.users[&fit].texture.is_some());
        let held: usize = state.users.values().map(User::blob_bytes).sum::<usize>()
            + state
                .channels
                .values()
                .map(|c| c.description.len())
                .sum::<usize>();
        assert_eq!(state.blob_bytes, held);
        assert_eq!(notices, [CHANNELS_NOTICE, USERS_NOTICE, BLOBS_NOTICE]);
    }

    #[tokio::test]
    async fn links_are_mirrored_and_permissions_cached() {
        let mut state = State {
            own_session: Some(1),
            ..State::default()
        };
        let (tx, mut rx) = mpsc::channel(64);
        for id in [0, 1, 2] {
            state
                .handle(
                    frame(
                        kind::CHANNEL_STATE,
                        proto::ChannelState {
                            channel_id: Some(id),
                            ..Default::default()
                        },
                    ),
                    &tx,
                )
                .await
                .unwrap();
        }
        state
            .handle(
                frame(
                    kind::CHANNEL_STATE,
                    proto::ChannelState {
                        channel_id: Some(1),
                        links_add: vec![2],
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        assert!(state.channels[&2].links.contains(&1));
        state
            .handle(
                frame(
                    kind::PERMISSION_QUERY,
                    proto::PermissionQuery {
                        channel_id: Some(2),
                        permissions: Some(0x20c),
                        flush: Some(false),
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        assert_eq!(state.permissions[&2], 0x20c);
        state
            .handle(
                frame(kind::CHANNEL_REMOVE, proto::ChannelRemove { channel_id: 2 }),
                &tx,
            )
            .await
            .unwrap();
        assert!(!state.channels[&1].links.contains(&2));
        drain(&mut rx).await;
    }

    #[tokio::test]
    async fn commands_produce_wire_messages() {
        let mut state = State {
            own_session: Some(7),
            ..State::default()
        };
        let (tx, _rx) = mpsc::channel(64);
        state
            .handle(
                frame(
                    kind::CHANNEL_STATE,
                    proto::ChannelState {
                        channel_id: Some(0),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        state
            .handle(
                frame(
                    kind::USER_STATE,
                    proto::UserState {
                        session: Some(9),
                        name: Some("Bob".into()),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        state
            .command(
                Command::SetVoiceTarget {
                    id: 1,
                    targets: vec![VoiceTargetSpec::Users(vec![9])],
                },
                &tx,
            )
            .await
            .unwrap();
        state
            .command(
                Command::User {
                    session: 9,
                    action: UserAction::Kick,
                    reason: Some("bye".into()),
                },
                &tx,
            )
            .await
            .unwrap();
        state
            .command(
                Command::Listen {
                    channel: 0,
                    listen: true,
                },
                &tx,
            )
            .await
            .unwrap();
        let kinds: Vec<u16> = state.outbound.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(
            kinds,
            [kind::VOICE_TARGET, kind::USER_REMOVE, kind::USER_STATE]
        );
        let target = proto::VoiceTarget::decode(state.outbound[0].1.as_slice()).unwrap();
        assert_eq!(target.targets[0].session, [9]);
        let listen = proto::UserState::decode(state.outbound[2].1.as_slice()).unwrap();
        assert_eq!(listen.listening_channel_add, [0]);
    }

    #[tokio::test]
    async fn images_go_out_as_rich_text_and_into_our_own_timeline() {
        let mut state = State {
            own_session: Some(7),
            ..State::default()
        };
        let (tx, mut rx) = mpsc::channel(64);
        let bob = proto::UserState {
            session: Some(9),
            name: Some("Bob".into()),
            ..Default::default()
        };
        state
            .handle(frame(kind::USER_STATE, bob), &tx)
            .await
            .unwrap();
        while rx.try_recv().is_ok() {}
        let html = "<img src=\"data:image/png;base64,iVBORw0KGgo%3D\" />".to_string();
        let send = |html: String| Command::SendHtml {
            target: TextTarget::Direct(9),
            html: vec![html],
            expected_peer: None,
        };
        state.command(send(html.clone()), &tx).await.unwrap();
        let [(kind::TEXT_MESSAGE, wire)] = state.outbound.as_slice() else {
            panic!("one text message")
        };
        let wire = proto::TextMessage::decode(wire.as_slice()).unwrap();
        assert_eq!(
            (wire.message.as_str(), wire.session.as_slice()),
            (html.as_str(), &[9][..])
        );
        let Ok(SessionEvent::Text(echo)) = rx.try_recv() else {
            panic!("no outgoing echo")
        };
        assert!(echo.outgoing && echo.html && echo.body == html);
        // Above the server's image limit, nothing is sent.
        state.outbound.clear();
        state
            .command(send("x".repeat(state.image_limit + 1)), &tx)
            .await
            .unwrap();
        assert!(state.outbound.is_empty());
        assert!(matches!(rx.try_recv(), Ok(SessionEvent::Notice(_))));
    }

    /// The text messages written since the last call.
    fn sent_texts(state: &mut State) -> Vec<String> {
        state
            .outbound
            .drain(..)
            .filter(|(kind, _)| *kind == kind::TEXT_MESSAGE)
            .map(|(_, wire)| proto::TextMessage::decode(wire.as_slice()).unwrap().message)
            .collect()
    }

    fn progress(events: &[SessionEvent]) -> Vec<(usize, usize)> {
        events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::Outbox { sent, total } => Some((*sent, *total)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn text_waits_for_the_servers_rate_limit_instead_of_being_dropped() {
        let mut state = State {
            own_session: Some(7),
            ..State::default()
        };
        let (tx, mut rx) = mpsc::channel(64);
        let root = proto::ChannelState {
            channel_id: Some(0),
            ..Default::default()
        };
        state
            .handle(frame(kind::CHANNEL_STATE, root), &tx)
            .await
            .unwrap();
        drain(&mut rx).await;
        let images: Vec<String> = (0..7)
            .map(|n| format!("<img src=\"data:image/png;base64,{n}\" />"))
            .collect();
        let send = Command::SendHtml {
            target: TextTarget::Channel(0),
            html: images.clone(),
            expected_peer: None,
        };
        state.command(send, &tx).await.unwrap();
        // Four leave at once, one short of stock Murmur's burst of five.
        assert_eq!(sent_texts(&mut state), images[..4]);
        let events = drain(&mut rx).await;
        let echoes = events
            .iter()
            .filter(|event| matches!(event, SessionEvent::Text(text) if text.outgoing))
            .count();
        assert_eq!((echoes, progress(&events)), (4, vec![(4, 7)]));
        // A message typed meanwhile queues behind the images.
        let text = Command::SendText {
            target: TextTarget::Channel(0),
            body: "and one more thing".into(),
            expected_peer: None,
        };
        state.command(text, &tx).await.unwrap();
        assert!(sent_texts(&mut state).is_empty());
        assert_eq!(progress(&drain(&mut rx).await), [(4, 8)]);
        // Then one every 1.25 s: slower than the server's one a second,
        // which loses fractions of a token whenever it drains.
        let started = Instant::now();
        let mut left = Vec::new();
        while let Some(due) = state.outbox.due() {
            time::sleep_until(due).await;
            state.flush_outbox(&tx).await.unwrap();
            let texts = sent_texts(&mut state);
            assert_eq!(texts.len(), 1);
            left.push((started.elapsed().as_millis(), texts[0].clone()));
        }
        assert_eq!(
            left.iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>(),
            [&images[4], &images[5], &images[6], "and one more thing"]
        );
        for (index, (at, _)) in left.iter().enumerate() {
            let expected = 1250 * (index as u128 + 1);
            assert!(at.abs_diff(expected) <= 2, "{index}: {at} ms");
        }
        assert_eq!(
            progress(&drain(&mut rx).await),
            [(5, 8), (6, 8), (7, 8), (8, 8)]
        );
        // Once the bucket has drained, a message leaves at once, unreported.
        time::advance(Duration::from_secs(5)).await;
        let text = Command::SendText {
            target: TextTarget::Channel(0),
            body: "later".into(),
            expected_peer: None,
        };
        state.command(text, &tx).await.unwrap();
        assert_eq!(sent_texts(&mut state), ["later"]);
        assert!(progress(&drain(&mut rx).await).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn other_limited_messages_count_against_the_rate_limit() {
        let mut state = State {
            own_session: Some(7),
            outbox: Outbox::new(MessageRate {
                burst: 3,
                per_second: 2,
            }),
            ..State::default()
        };
        let (tx, mut rx) = mpsc::channel(64);
        let root = proto::ChannelState {
            channel_id: Some(0),
            ..Default::default()
        };
        state
            .handle(frame(kind::CHANNEL_STATE, root), &tx)
            .await
            .unwrap();
        // Muting ourselves takes one of the two messages text may burst.
        state
            .command(
                Command::SetSelf {
                    muted: true,
                    deafened: false,
                },
                &tx,
            )
            .await
            .unwrap();
        state.outbound.clear();
        let html = vec!["<b>one</b>".to_string(), "<b>two</b>".to_string()];
        let send = Command::SendHtml {
            target: TextTarget::Channel(0),
            html,
            expected_peer: None,
        };
        state.command(send, &tx).await.unwrap();
        assert_eq!(sent_texts(&mut state), ["<b>one</b>"]);
        // At this server's two a second (paced at 1.6), the next follows in 0.625 s.
        let wait = state.outbox.due().unwrap() - Instant::now();
        assert!(wait.as_millis().abs_diff(626) <= 1, "{wait:?}");
        drain(&mut rx).await;
    }

    #[test]
    fn identifies_legacy_codec_senders() {
        assert_eq!(legacy_sender(&[0x40, 0x05, 0x00]), Some(5));
        assert_eq!(legacy_sender(&[0x80, 0x05]), None, "Opus is not legacy");
    }

    #[test]
    fn nothing_captured_leaves_while_muted() {
        let packet = |end| VoicePacket {
            sender: 0,
            sequence: 8,
            opus: vec![1, 2, 3, 4],
            end,
            volume: 1.,
        };
        let mut state = State {
            own_session: Some(7),
            local_muted: false,
            ..State::default()
        };
        state.users.insert(
            7,
            User {
                session: 7,
                ..User::default()
            },
        );
        assert_eq!(state.outgoing_voice(packet(false)), Some(packet(false)));
        assert_eq!(state.outgoing_voice(packet(true)), Some(packet(true)));
        let silent = VoicePacket {
            opus: nuschel_core::audio::SILENT_OPUS.to_vec(),
            ..packet(true)
        };
        state.local_muted = true;
        assert_eq!(state.outgoing_voice(packet(false)), None);
        assert_eq!(state.outgoing_voice(packet(true)), Some(silent.clone()));
        // Muted or suppressed by the server: the same.
        state.local_muted = false;
        state.users.get_mut(&7).unwrap().suppressed = true;
        assert_eq!(state.outgoing_voice(packet(false)), None);
        assert_eq!(state.outgoing_voice(packet(true)), Some(silent));
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
