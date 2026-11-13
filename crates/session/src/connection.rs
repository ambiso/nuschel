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
use nuschl_core::MAX_CONTENT_BYTES;
use nuschl_core::admin::{AccountReply, AclReply, RegisteredAccount, group_has_local_definition};
use nuschl_protocol::{
    ControlFrame, MAX_CONTROL_BYTES, control as proto,
    crypt::CryptState,
    nuschl::{
        self, v1,
        version::{Standing, Version},
    },
    plugin,
    voice::{self, UdpMessage, VoicePacket},
};
use prost::Message;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
    time::{self, Duration, Instant, timeout},
};

pub use crate::model::{Channel, ServerLimits, TextTarget, User};
pub use nuschl_legacy_html::{escape_html, legacy_body, text_html};

fn next_user_instance() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

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
    pub const PLUGIN_DATA_TRANSMISSION: u16 = 26;
}

const MAX_BLOB_REQUEST: usize = 64;
/// Delayed UI actions are small; bulk targets need explicit fresh actions.
pub const MAX_ACTION_TARGETS: usize = 128;

// Known protocol types are not necessarily implemented by this client. In
// particular knock/1 needs a native consumer before we negotiate typed notices;
// stock-compatible admission notices remain available without that capability.
const CLIENT_CAPABILITIES: &[&str] = &[
    nuschl::capability::HISTORY,
    nuschl::capability::SUBSCRIBE,
    nuschl::capability::TYPING,
    nuschl::capability::KT,
    nuschl::capability::MLS,
    nuschl::capability::E2EE,
    nuschl::capability::KT_WITNESS,
    nuschl::capability::SHARE,
    nuschl::capability::PRIVACY,
];

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

#[derive(Clone, Debug)]
pub struct IncomingText {
    pub target: TextTarget,
    pub author: String,
    pub peer: Option<User>,
    pub body: String,
    pub html: bool,
    pub outgoing: bool,
    pub observed_at_ms: i64,
    /// The server's sequence number (Nuschl servers: from history).
    pub seq: Option<u64>,
    /// Our own message on a Nuschl server, until `ChatAcked`: its key.
    pub pending: Option<[u8; 16]>,
}

/// Our message to a Nuschl conversation until the server acknowledged it.
/// Retries (also after a reconnect) carry the same idempotency key, so the
/// server stores it once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingChat {
    /// A channel, or a direct conversation (`TextTarget::DirectConversation`).
    pub target: TextTarget,
    pub conversation: [u8; 16],
    pub key: [u8; 16],
    pub content: String,
}

/// Plugin data another client sent us (`PluginDataTransmission`): opaque
/// bytes, never interpreted here. The sender's identity is what this
/// connection knew of it when the packet arrived, so a later occupant of the
/// same session number never inherits it. The certificate hash is the
/// server's claim, not proof: a higher layer authenticates content itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingPluginData {
    pub sender: u32,
    pub sender_instance: u64,
    pub sender_user_id: Option<u32>,
    pub sender_certificate_hash: Option<String>,
    pub payload: plugin::Payload,
}

/// Receivers tolerated in plugin data from the server, which stock servers
/// clear: this bounds what is read for it ([`plugin::max_body`], 4,951 bytes).
const MAX_RECEIVED_RECEIVERS: usize = 512;

/// Our plugin data's local admission bucket: two a second, a burst of ten.
/// Murmur's default bucket (4 a second, burst 15) drains whole tokens and
/// restarts its timer only when one drained, so it is guaranteed to drain
/// only about two a second; this stays within that. It is best effort: a
/// server configured lower (the shipped `mumble-server.ini` suggests 1 a
/// second, burst 5) still drops what this admits, and nothing acknowledges
/// delivery.
struct PluginRate {
    level: f64,
    at: Instant,
}

impl PluginRate {
    const BURST: f64 = 10.0;
    const PER_SECOND: f64 = 2.0;
    fn new() -> Self {
        Self {
            level: 0.0,
            at: Instant::now(),
        }
    }
    /// Admit one message now, or say in how many milliseconds one fits.
    fn admit(&mut self, now: Instant) -> Result<(), u64> {
        let leaked = now.saturating_duration_since(self.at).as_secs_f64() * Self::PER_SECOND;
        self.level = (self.level - leaked).max(0.0);
        self.at = now;
        let excess = self.level + 1.0 - Self::BURST;
        if excess > 0.0 {
            return Err((excess / Self::PER_SECOND * 1000.0).ceil().max(1.0) as u64);
        }
        self.level += 1.0;
        Ok(())
    }
}

/// What plugin data one connection may surface: a second's worth of items
/// and bytes, refilled continuously. A hostile server cannot make plugin
/// data crowd out the connection's other events beyond this.
struct PluginReceive {
    items: f64,
    bytes: f64,
    at: Instant,
}

impl PluginReceive {
    const ITEMS: f64 = 64.0;
    const BYTES: f64 = 64.0 * 1024.0;
    fn new() -> Self {
        Self {
            items: Self::ITEMS,
            bytes: Self::BYTES,
            at: Instant::now(),
        }
    }
    fn admit(&mut self, size: usize, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.at = now;
        self.items = (self.items + elapsed * Self::ITEMS).min(Self::ITEMS);
        self.bytes = (self.bytes + elapsed * Self::BYTES).min(Self::BYTES);
        let size = size as f64;
        if self.items < 1.0 || self.bytes < size {
            return false;
        }
        self.items -= 1.0;
        self.bytes -= size;
        true
    }
}

/// Request ids the session uses itself; frontends use smaller ones.
const INTERNAL: u64 = 1 << 63;
/// Busy or rate-limited sends are retried this often before they fail.
const MAX_SEND_ATTEMPTS: u32 = 5;

#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// Actual CodecVersion knowledge, independent of the private routing default.
    OpusAvailable(bool),
    PeerCertificate(String),
    /// The extension negotiation succeeded (only for `nuschl/1` servers);
    /// frontends raise the server's sticky record to it.
    Negotiated(Negotiated),
    /// A channel's conversation id and history properties (Nuschl servers).
    ChannelInfo {
        channel: u32,
        info: v1::ChannelInfo,
    },
    Nuschl(NuschlEvent),
    /// Our message went to a Nuschl server and waits for its acknowledgement.
    ChatPending(PendingChat),
    /// Identity and key transparency on this server (`kt/1`, with a crypto worker).
    Identity(crate::identity::IdentityEvent),
    /// End-to-end-encrypted conversations (`mls/1`, with a crypto worker).
    Mls(crate::mls::MlsEvent),
    /// The server stored our message as `seq`.
    ChatAcked {
        key: [u8; 16],
        seq: u64,
    },
    /// The server refused our message; it was not stored.
    ChatFailed {
        key: [u8; 16],
        reason: String,
    },
    /// A subscribed channel's newest seq and our read marker (Nuschl servers).
    ConversationState {
        channel: u32,
        head: u64,
        read: u64,
    },
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
    AclReply(AclReply),
    AccountReply(AccountReply),
    /// Resolved names for registered user ids (ACL editor).
    UserNames(BTreeMap<u32, String>),
    /// A voice packet from a legacy codec Nuschl does not decode.
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
    /// Plugin data from another client (never in the `nuschl/` namespace).
    PluginData(IncomingPluginData),
    /// Whether our plugin data `request` was admitted and handed to the
    /// connection. `Ok` is not delivery: there is no acknowledgement.
    PluginDataAdmission {
        request: u64,
        result: Result<(), plugin::Refusal>,
    },
}

const RELAY_PRIVACY_UNAVAILABLE: &str = "Relay-only is enabled locally, but this server cannot promise to hide your IP address from other clients.";

#[derive(Debug)]
pub enum Command {
    /// Native delayed action, checked again by the single session actor.
    Scoped {
        authority: CommandAuthority,
        command: Box<Command>,
    },
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
    /// Skip peer discovery locally and ask privacy/1 servers not to disclose
    /// our address to other clients. Configured server connections stay allowed.
    RelayOnly(bool),
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
    /// Install and select one explicit route atomically (native UI whisper).
    Whisper {
        revision: u64,
        id: u32,
        targets: Vec<VoiceTargetSpec>,
    },
    /// Send subsequent voice to this registered target (0 = normal talking).
    SelectVoiceTarget(u32),
    /// Native explicit intent, revision allocated before attempting enqueue.
    SelectVoiceTargetAt {
        id: u32,
        revision: u64,
    },
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
    /// One correlated query per channel, retained until a reply or reconnect.
    QueryAclDraft {
        channel: u32,
        request: u64,
    },
    UpdateAcl(Acl),
    QueryUserNames(Vec<u32>),
    QueryUserIds(Vec<String>),
    LookupAccount {
        channel: u32,
        request: u64,
        name: String,
    },
    /// Mark a channel read up to `seq` (Nuschl servers; synced to the
    /// account's other devices).
    MarkRead {
        channel: u32,
        seq: u64,
    },
    /// A Nuschl extension request; only sent when the extension (and the
    /// capability it needs) was negotiated, otherwise answered locally with
    /// `UNSUPPORTED`.
    Nuschl(NuschlRequest),
    /// Identity and key transparency (`kt/1` servers with a crypto worker);
    /// ignored elsewhere.
    Identity(crate::identity::IdentityCommand),
    /// End-to-end-encrypted conversations (`mls/1` servers with a crypto worker);
    /// ignored elsewhere.
    Mls(crate::mls::MlsCommand),
    Voice {
        target: u32,
        packet: VoicePacket,
    },
    /// Plugin data to up to [`MAX_ACTION_TARGETS`] receivers, each named by
    /// session and presence instance, answered by `PluginDataAdmission`. The
    /// whole send is refused if any receiver left or was replaced; frontends
    /// also send it scoped, so the actor checks the authority too.
    PluginData {
        request: u64,
        receivers: Vec<(u32, u64)>,
        payload: plugin::Payload,
    },
    /// Whether plugin data from the server is decoded and surfaced at all
    /// (off by default; while off it is skipped undecoded).
    AcceptPluginData(bool),
    Disconnect,
}

/// Shared local voice intent. Target 31 means suspended. A queued older action
/// cannot admit audio after a newer intent, including when its enqueue fails.
#[derive(Debug, Default)]
pub struct VoiceRoute(std::sync::atomic::AtomicU64);
impl VoiceRoute {
    pub fn suspend(&self) -> u64 {
        let previous = self
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |state| {
                (state & !31).checked_add(32).map(|revision| revision | 31)
            })
            .expect("voice intent revisions exhausted");
        (previous >> 5) + 1
    }
    pub fn target(&self) -> u32 {
        (self.0.load(Ordering::Relaxed) & 31) as u32
    }
    pub fn revision(&self) -> u64 {
        self.0.load(Ordering::Relaxed) >> 5
    }
    fn allows(&self, revision: u64) -> bool {
        let state = self.0.load(Ordering::Relaxed);
        state & 31 != 31 && state >> 5 == revision
    }
    fn current(&self, revision: u64) -> bool {
        self.revision() == revision
    }
    fn suspend_revision(&self, revision: u64) {
        let _ = self
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |state| {
                (state >> 5 == revision).then_some((revision << 5) | 31)
            });
    }
    fn admit(&self, revision: u64, target: u32) -> bool {
        target <= 30
            && self
                .0
                .compare_exchange(
                    (revision << 5) | 31,
                    (revision << 5) | target as u64,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
    }
}

/// A bounded observed-model lease. IDs alone are reusable on Mumble servers.
/// This is local metadata; no presence tokens are sent on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionSubject {
    User(u32, u64),
    Channel(u32, u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandAuthority {
    pub subject: Option<ActionSubject>,
    pub own: (u32, u64),
    pub users: Vec<(u32, u64)>,
    pub channels: Vec<(u32, u64)>,
    /// Explicit immutable Nuschl conversation IDs, scoped to this connection.
    pub conversations: Vec<[u8; 16]>,
    /// Immutable crypto account identities; never inferred from an arbitrary ID.
    pub accounts: Vec<[u8; 16]>,
}

impl CommandAuthority {
    /// Keep only concrete command refs plus the original primary subject. The
    /// UI snapshot can include candidate link/move destinations, but unrelated
    /// candidates do not become dependencies of an action.
    pub fn for_command(&self, command: &Command) -> Option<Self> {
        if self.users.len() > MAX_ACTION_TARGETS
            || self.channels.len() > MAX_ACTION_TARGETS
            || self.conversations.len() > MAX_ACTION_TARGETS
            || self.accounts.len() > MAX_ACTION_TARGETS
        {
            return None;
        }
        let (users, channels) = command.targets()?;
        if !users
            .iter()
            .all(|id| self.users.iter().any(|(held, _)| id == held))
            || !channels
                .iter()
                .all(|id| self.channels.iter().any(|(held, _)| id == held))
        {
            return None;
        }
        let conversations = command.conversation_targets()?;
        let accounts = command.account_targets();
        if !accounts
            .iter()
            .all(|account| self.accounts.contains(account))
        {
            return None;
        }
        if !conversations
            .iter()
            .all(|id| self.conversations.contains(id))
        {
            return None;
        }
        let selecting = matches!(command, Command::SelectVoiceTarget(id) | Command::SelectVoiceTargetAt { id, .. } if *id != 0);
        let selected_user = selecting
            .then_some(self.subject)
            .flatten()
            .and_then(|subject| match subject {
                ActionSubject::User(id, instance) => Some((id, instance)),
                _ => None,
            });
        let selected_channel = selecting
            .then_some(self.subject)
            .flatten()
            .and_then(|subject| match subject {
                ActionSubject::Channel(id, instance) => Some((id, instance)),
                _ => None,
            });
        Some(Self {
            subject: self.subject,
            own: self.own,
            users: self
                .users
                .iter()
                .filter(|(id, _)| {
                    users.contains(id) || selected_user.is_some_and(|(selected, _)| selected == *id)
                })
                .copied()
                .collect(),
            channels: self
                .channels
                .iter()
                .filter(|(id, _)| {
                    channels.contains(id)
                        || selected_channel.is_some_and(|(selected, _)| selected == *id)
                })
                .copied()
                .collect(),
            conversations,
            accounts,
        })
    }
}

impl Command {
    fn account_targets(&self) -> Vec<[u8; 16]> {
        match self {
            Self::Identity(
                crate::identity::IdentityCommand::Check {
                    account: Some(account),
                    ..
                }
                | crate::identity::IdentityCommand::MarkVerified { account }
                | crate::identity::IdentityCommand::UnmarkVerified { account }
                | crate::identity::IdentityCommand::AcceptKeyChange { account },
            ) => vec![*account],
            _ => Vec::new(),
        }
    }
    fn conversation_targets(&self) -> Option<Vec<[u8; 16]>> {
        let bytes: Option<&[u8]> = match self {
            Self::Nuschl(NuschlRequest::History(request)) => Some(&request.conversation),
            Self::Nuschl(NuschlRequest::ReadMarker(request)) => Some(&request.conversation),
            Self::Nuschl(NuschlRequest::Edit(request)) => Some(&request.conversation),
            Self::Nuschl(NuschlRequest::Delete(request)) => Some(&request.conversation),
            Self::Mls(
                crate::mls::MlsCommand::OpenChannel { conversation }
                | crate::mls::MlsCommand::Status { conversation }
                | crate::mls::MlsCommand::Sync { conversation }
                | crate::mls::MlsCommand::History { conversation, .. },
            ) => Some(conversation),
            _ => None,
        };
        match bytes {
            Some(bytes) => Some(vec![nuschl::id(bytes)?]),
            None => Some(Vec::new()),
        }
    }

    /// Every explicit wire target must be covered by the envelope. A caller
    /// cannot authorize a different target by supplying an unrelated valid lease.
    /// The bound also limits allocation/work before reaching the actor.
    pub fn targets(&self) -> Option<(BTreeSet<u32>, BTreeSet<u32>)> {
        let within = match self {
            Self::LinkChannels { add, remove, .. } => {
                add.len().saturating_add(remove.len()).saturating_add(1) <= MAX_ACTION_TARGETS
            }
            Self::RequestBlobs {
                textures,
                comments,
                descriptions,
            } => {
                textures
                    .len()
                    .saturating_add(comments.len())
                    .saturating_add(descriptions.len())
                    <= MAX_ACTION_TARGETS
            }
            Self::PluginData { receivers, .. } => {
                !receivers.is_empty() && receivers.len() <= MAX_ACTION_TARGETS
            }
            Self::SetVoiceTarget { targets, .. } | Self::Whisper { targets, .. } => {
                targets.len() <= 32
                    && targets
                        .iter()
                        .try_fold(0usize, |count, target| {
                            let count = count.saturating_add(match target {
                                VoiceTargetSpec::Users(ids) => ids.len(),
                                _ => 1,
                            });
                            (count <= MAX_ACTION_TARGETS).then_some(count)
                        })
                        .is_some()
            }
            _ => true,
        };
        if !within {
            return None;
        }
        let mut users = BTreeSet::new();
        let mut channels = BTreeSet::new();
        match self {
            Self::Join(id)
            | Self::RemoveChannel(id)
            | Self::RequestPermissions(id)
            | Self::QueryAcl(id)
            | Self::QueryAclDraft { channel: id, .. }
            | Self::LookupAccount { channel: id, .. } => {
                channels.insert(*id);
            }
            Self::CreateChannel { parent, .. } => {
                channels.insert(*parent);
            }
            Self::EditChannel { channel, edit } => {
                channels.insert(*channel);
                channels.extend(edit.parent);
            }
            Self::LinkChannels {
                channel,
                add,
                remove,
            } => {
                channels.insert(*channel);
                channels.extend(add.iter().chain(remove).copied());
            }
            Self::Listen { channel, .. } | Self::MarkRead { channel, .. } => {
                channels.insert(*channel);
            }
            Self::User {
                session, action, ..
            } => {
                users.insert(*session);
                if let UserAction::Move(channel) = action {
                    channels.insert(*channel);
                }
            }
            Self::RequestStats(session) => {
                users.insert(*session);
            }
            Self::PluginData { receivers, .. } => {
                users.extend(receivers.iter().map(|(session, _)| *session));
            }
            Self::RequestBlobs {
                textures,
                comments,
                descriptions,
            } => {
                users.extend(textures.iter().chain(comments).copied());
                channels.extend(descriptions.iter().copied());
            }
            Self::SetVoiceTarget { targets, .. } | Self::Whisper { targets, .. } => {
                for target in targets {
                    match target {
                        VoiceTargetSpec::Users(ids) => users.extend(ids.iter().copied()),
                        VoiceTargetSpec::Channel { channel, .. } => {
                            channels.insert(*channel);
                        }
                    }
                }
            }
            Self::ContextAction {
                session, channel, ..
            } => {
                users.extend(session);
                channels.extend(channel);
            }
            Self::UpdateAcl(acl) => {
                channels.insert(acl.channel);
            }
            Self::SetComment(_)
            | Self::SetAvatar(_)
            | Self::RegisterSelf
            | Self::QueryRegisteredUsers
            | Self::UpdateRegisteredUsers(_)
            | Self::QueryBans
            | Self::QueryUserNames(_)
            | Self::QueryUserIds(_)
            | Self::UpdateBans(_)
            | Self::SelectVoiceTarget(_)
            | Self::SelectVoiceTargetAt { .. }
            | Self::Identity(_)
            | Self::Mls(
                crate::mls::MlsCommand::OpenDirect { .. }
                | crate::mls::MlsCommand::OpenChannel { .. }
                | crate::mls::MlsCommand::Status { .. }
                | crate::mls::MlsCommand::Sync { .. }
                | crate::mls::MlsCommand::History { .. },
            ) => {}
            Self::Nuschl(NuschlRequest::ShareLinkCreate(request)) => {
                channels.insert(request.channel);
            }
            Self::Nuschl(
                NuschlRequest::History(_)
                | NuschlRequest::ReadMarker(_)
                | NuschlRequest::OpenDirect(_)
                | NuschlRequest::Edit(_)
                | NuschlRequest::Delete(_),
            ) => {}
            // Persistent configuration, chat/crypto and connection management
            // have their own lifecycles; this envelope is not a generic bypass.
            _ => return None,
        }
        (users.len() <= MAX_ACTION_TARGETS && channels.len() <= MAX_ACTION_TARGETS)
            .then_some((users, channels))
    }
}

/// Passwords exist only in memory and are never part of a saved bookmark or Debug output.
pub struct ConnectOptions {
    pub server: ServerBookmark,
    /// Accepted voice route, retained by native reconnect supervision. Nonzero
    /// reconnects suspended until an explicit new target/normal selection.
    pub voice_route: std::sync::Arc<VoiceRoute>,
    pub identity: ClientIdentity,
    pub password: Option<String>,
    /// Never use UDP; voice stays in the TLS tunnel (Mumble's "Force TCP mode").
    pub force_tcp: bool,
    pub relay_only: bool,
    /// Restored after a reconnect; a fresh connection starts muted.
    pub muted: bool,
    pub deafened: bool,
    /// Rejoin this channel after login (reconnect).
    pub channel: Option<u32>,
    /// Messages not acknowledged on an earlier connection, sent again with
    /// their keys (Nuschl servers).
    pub resend: Vec<PendingChat>,
    /// The server's rate limit for text, which it does not announce.
    pub message_rate: MessageRate,
    /// What this server negotiated before (the sticky policy, or an invite):
    /// a weaker answer fails the connection before any credential is sent.
    pub required: crate::sticky::Requirement,
    /// The crypto worker, for key transparency on `kt/1` servers (none: the
    /// session neither publishes nor checks identities).
    pub crypto: Option<crate::identity::IdentitySetup>,
}

/// Requests of the Nuschl extension (docs/extension.md), as on the wire.
#[derive(Debug, Clone)]
pub enum NuschlRequest {
    Send(v1::ChatSend),
    Edit(v1::ChatEdit),
    Delete(v1::ChatDelete),
    History(v1::HistoryRequest),
    Subscribe(v1::Subscribe),
    Unsubscribe(v1::Unsubscribe),
    ReadMarker(v1::ReadMarker),
    Typing(v1::Typing),
    OpenDirect(v1::OpenDirect),
    ListDirect(v1::ListDirect),
    ChannelSettings(v1::ChannelSettings),
    /// Share links (`share/1`, docs/web.md).
    ShareLinkCreate(v1::ShareLinkCreate),
    ShareLinkRevoke(v1::ShareLinkRevoke),
    ShareLinkResolve(v1::ShareLinkResolve),
}

impl NuschlRequest {
    fn frame(&self) -> (u16, Vec<u8>) {
        use nuschl::kind;
        match self {
            Self::Send(message) => (kind::CHAT_SEND, message.encode_to_vec()),
            Self::Edit(message) => (kind::CHAT_EDIT, message.encode_to_vec()),
            Self::Delete(message) => (kind::CHAT_DELETE, message.encode_to_vec()),
            Self::History(message) => (kind::HISTORY_REQUEST, message.encode_to_vec()),
            Self::Subscribe(message) => (kind::SUBSCRIBE, message.encode_to_vec()),
            Self::Unsubscribe(message) => (kind::UNSUBSCRIBE, message.encode_to_vec()),
            Self::ReadMarker(message) => (kind::READ_MARKER, message.encode_to_vec()),
            Self::Typing(message) => (kind::TYPING, message.encode_to_vec()),
            Self::OpenDirect(message) => (kind::OPEN_DIRECT, message.encode_to_vec()),
            Self::ListDirect(message) => (kind::LIST_DIRECT, message.encode_to_vec()),
            Self::ChannelSettings(message) => (kind::CHANNEL_SETTINGS, message.encode_to_vec()),
            Self::ShareLinkCreate(message) => (kind::SHARE_LINK_CREATE, message.encode_to_vec()),
            Self::ShareLinkRevoke(message) => (kind::SHARE_LINK_REVOKE, message.encode_to_vec()),
            Self::ShareLinkResolve(message) => (kind::SHARE_LINK_RESOLVE, message.encode_to_vec()),
        }
    }

    fn request_id(&self) -> u64 {
        match self {
            Self::Send(message) => message.request_id,
            Self::Edit(message) => message.request_id,
            Self::Delete(message) => message.request_id,
            Self::History(message) => message.request_id,
            Self::Subscribe(message) => message.request_id,
            Self::OpenDirect(message) => message.request_id,
            Self::ListDirect(message) => message.request_id,
            Self::ChannelSettings(message) => message.request_id,
            Self::ShareLinkCreate(message) => message.request_id,
            Self::ShareLinkRevoke(message) => message.request_id,
            Self::ShareLinkResolve(message) => message.request_id,
            Self::Unsubscribe(_) | Self::ReadMarker(_) | Self::Typing(_) => 0,
        }
    }
}

/// A random 128-bit idempotency key (or request id material) for a send.
pub fn random_key() -> [u8; 16] {
    uuid::Uuid::new_v4().into_bytes()
}

/// What a Nuschl server sends, checked against the extension's bounds.
#[derive(Debug, Clone)]
pub enum NuschlEvent {
    Error(v1::Error),
    Ack(v1::ChatAck),
    Chat(v1::ChatEvent),
    Page(v1::HistoryPage),
    /// Local crypto-worker output, never decoded from a server frame.
    DecryptedPage {
        page: v1::HistoryPage,
        unauthenticated: Vec<u64>,
    },
    Subscribed(v1::Subscribed),
    Unsubscribed(v1::Unsubscribed),
    ReadMarker(v1::ReadMarker),
    Typing(v1::Typing),
    DirectOpened(v1::DirectOpened),
    DirectList(v1::DirectList),
    Done(v1::Done),
    /// A share link created or resolved (`share/1`).
    ShareLink(v1::ShareLink),
}

/// Decode and bound an extension frame from the server (`None`: ignored).
fn nuschl_event(frame: &ControlFrame) -> Result<Option<NuschlEvent>, Error> {
    use nuschl::kind;
    if frame.payload.len() > nuschl::server_limit(frame.kind) {
        return Err(Error::Protocol("oversized Nuschl message".into()));
    }
    let invalid = || Error::Protocol("invalid Nuschl message".into());
    let id = |bytes: &[u8]| bytes.is_empty() || nuschl::id(bytes).is_some();
    let event = match frame.kind {
        kind::ERROR => {
            let mut error: v1::Error = decode(frame)?;
            // Shown as plain text, so bounded and never more than a line.
            error.detail = bounded(error.detail.replace(['\n', '\r'], " "), 256);
            NuschlEvent::Error(error)
        }
        kind::CHAT_ACK => NuschlEvent::Ack(decode(frame)?),
        kind::CHAT_EVENT => {
            let event: v1::ChatEvent = decode(frame)?;
            if !id(&event.conversation)
                || !event.message.as_ref().is_some_and(nuschl::valid_message)
            {
                return Err(invalid());
            }
            NuschlEvent::Chat(event)
        }
        kind::HISTORY_PAGE => {
            let page: v1::HistoryPage = decode(frame)?;
            if !id(&page.conversation)
                || page.messages.len() > nuschl::MAX_PAGE_MESSAGES
                || !page.messages.iter().all(nuschl::valid_message)
            {
                return Err(invalid());
            }
            NuschlEvent::Page(page)
        }
        kind::SUBSCRIBED => {
            let subscribed: v1::Subscribed = decode(frame)?;
            if subscribed.conversations.len() > nuschl::MAX_SUBSCRIBE_PER_MESSAGE
                || !subscribed
                    .conversations
                    .iter()
                    .all(|state| id(&state.conversation))
            {
                return Err(invalid());
            }
            NuschlEvent::Subscribed(subscribed)
        }
        kind::UNSUBSCRIBED => NuschlEvent::Unsubscribed(decode(frame)?),
        kind::READ_MARKER => NuschlEvent::ReadMarker(decode(frame)?),
        kind::TYPING => NuschlEvent::Typing(decode(frame)?),
        kind::DIRECT_OPENED => {
            let mut opened: v1::DirectOpened = decode(frame)?;
            if let Some(direct) = &mut opened.direct {
                direct.name = bounded(std::mem::take(&mut direct.name), nuschl::MAX_NAME_BYTES);
            }
            NuschlEvent::DirectOpened(opened)
        }
        kind::DIRECT_LIST => {
            let mut list: v1::DirectList = decode(frame)?;
            list.directs.truncate(nuschl::MAX_DIRECTS);
            for direct in &mut list.directs {
                direct.name = bounded(std::mem::take(&mut direct.name), nuschl::MAX_NAME_BYTES);
            }
            NuschlEvent::DirectList(list)
        }
        kind::DONE => NuschlEvent::Done(decode(frame)?),
        kind::SHARE_LINK => {
            let link: v1::ShareLink = decode(frame)?;
            if link.token.len() != 16
                || link.access_token.len() > nuschl::MAX_ACCESS_TOKEN_BYTES
                || link.access_token.contains(char::is_control)
                || link.web_host.len() > nuschl::MAX_WEB_HOST_BYTES
                || !link
                    .web_host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte))
            {
                return Err(invalid());
            }
            NuschlEvent::ShareLink(link)
        }
        _ => return Ok(None),
    };
    Ok(Some(event))
}

/// What the Nuschl extension negotiation settled, before `Authenticate`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Negotiated {
    pub nuschl: bool,
    /// The capabilities both sides offered.
    pub capabilities: Vec<String>,
    pub policy: Option<v1::Policy>,
    /// With `kt/1`: the server's key-transparency log descriptor (nuschl-kt
    /// encoding), which the crypto worker pins.
    pub kt_log: Vec<u8>,
}

impl Negotiated {
    /// Where this client stands with the server's minimum-version policy
    /// (docs/extension.md, "Minimum versions") at `now_ms`.
    pub fn standing(&self, now_ms: i64) -> Standing {
        self.policy.as_ref().map_or(Standing::Current, |policy| {
            Standing::of(policy, Some(&client_version()), now_ms.div_euclid(1000))
        })
    }
}

/// This client's version, as it reports it in the hello.
pub fn client_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0))
}

/// The minimum-version policy as the server's hello stated it, when this
/// client is below the minimum.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Below {
    minimum: Version,
    /// The hello's date has passed (by our clock): the server will refuse us.
    refused: bool,
}

impl Below {
    fn of(policy: &v1::Policy, now_ms: i64) -> Option<Self> {
        match Standing::of(policy, Some(&client_version()), now_ms.div_euclid(1000)) {
            Standing::Required { version, .. } => Some(Self {
                minimum: version,
                refused: false,
            }),
            Standing::Refused { version } => Some(Self {
                minimum: version,
                refused: true,
            }),
            Standing::Current | Standing::Recommended { .. } => None,
        }
    }
}

/// Before login, below the server's minimum: its `WrongVersion` rejection
/// (whatever our clock says), or any failure once the hello said we are
/// refused (the server may close before its `Reject` is read), is the
/// minimum-version policy, said with the version to update to. Anything else
/// stays as it is.
fn outdated(error: Error, below: Option<&Below>) -> Error {
    let Some(below) = below else {
        return error;
    };
    let policy = match &error {
        Error::Rejected { kind, .. } => *kind == RejectKind::WrongVersion,
        Error::Io(_) | Error::Protocol(_) | Error::Timeout(_) => below.refused,
        _ => false,
    };
    if policy {
        Error::Outdated {
            minimum: below.minimum.to_string(),
            current: client_version().to_string(),
        }
    } else {
        error
    }
}

fn rejection_error(frame: &ControlFrame) -> Result<Error, Error> {
    let rejected: proto::Reject = decode(frame)?;
    let kind = RejectKind::from_wire(rejected.r#type);
    Ok(Error::Rejected {
        kind,
        reason: rejected
            .reason
            .map(|reason| bounded(reason, 1024))
            .unwrap_or_else(|| kind.describe().into()),
    })
}

/// A server may reject Authenticate and close while our initial UserState is
/// still being written. Keep the SAME verified reader alive briefly on this
/// error path, so that a real Reject wins over the resulting broken pipe.
/// Never infer a rejection from EOF or retry any credential. The absolute time
/// and frame budgets also bound a peer that sends unrelated traffic forever.
async fn initial_login_error(
    error: Error,
    logged_in: bool,
    frames: &mut mpsc::Receiver<Result<ControlFrame, Error>>,
) -> Error {
    if logged_in || !matches!(error, Error::Io(_)) {
        return error;
    }
    let until = Instant::now() + Duration::from_millis(100);
    for _ in 0..32 {
        match time::timeout_at(until, frames.recv()).await {
            Ok(Some(Ok(frame))) if frame.kind == kind::REJECT => {
                return rejection_error(&frame).unwrap_or(error);
            }
            // A completed login is not a failed initial authentication, even
            // if its queued Sync had not been handled when the writer failed.
            Ok(Some(Ok(frame))) if frame.kind != kind::SERVER_SYNC => {}
            _ => break,
        }
    }
    error
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
    /// A Nuschl conversation's message (sent as `ChatSend`), and whether it
    /// was already shown (a retry or resend).
    chat: Option<(PendingChat, bool)>,
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
    fn slot_due(&self) -> Instant {
        let excess = (self.level + 1.0 - self.capacity()).max(0.0);
        self.at + Duration::from_secs_f64(excess / self.leak_rate()) + Duration::from_millis(1)
    }
    fn due(&self) -> Option<Instant> {
        (!self.waiting.is_empty()).then(|| self.slot_due())
    }
    fn ready(&mut self, now: Instant) -> bool {
        self.leak(now);
        self.level + 1.0 <= self.capacity()
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
    let nuschl = stream.get_ref().1.alpn_protocol() == Some(nuschl::ALPN);
    // Nothing at all is sent to a server that answers weaker than required.
    if !nuschl {
        options
            .required
            .check(false, &[])
            .map_err(Error::Downgrade)?;
    }
    emit(&events, SessionEvent::PeerCertificate(peer)).await?;
    let udp = Udp::bind(peer_addr, options.force_tcp).await.ok();
    let mut datagram = vec![0; udp::MAX_DATAGRAM];
    let (reader, mut writer) = tokio::io::split(stream);
    let (frames_tx, mut frames) = mpsc::channel(32);
    let plugin_dropped = std::sync::Arc::new(AtomicU64::new(0));
    let skipped = plugin_dropped.clone();
    let read_task = tokio::spawn(async move {
        let mut reader = reader;
        loop {
            let frame = read_control(&mut reader, &skipped).await;
            let failed = frame.is_err();
            if frames_tx.send(frame).await.is_err() || failed {
                break;
            }
        }
    });
    // The server's minimum, until login, if this client is below it (see
    // `outdated`).
    let mut below = None;
    let mut logged_in = false;
    let result = async {
        let version = proto::Version {
            version_v1: Some(0x0001_0500),
            version_v2: Some(voice::PROTOBUF_VERSION),
            release: Some(format!("Nuschl {}", env!("CARGO_PKG_VERSION"))),
            os: Some(std::env::consts::OS.into()),
            ..Default::default()
        };
        let hello = nuschl.then(|| v1::Hello {
            protocol: nuschl::PROTOCOL,
            capabilities: CLIENT_CAPABILITIES.iter().map(|c| (*c).to_owned()).collect(),
            policy: None,
            kt_log: Vec::new(),
            client_version: client_version().to_string(),
            relay_only: options.relay_only,
        });
        let frame = nuschl::version_frame(&version, hello.as_ref())
            .ok_or_else(|| Error::Protocol("outgoing version too large".into()))?;
        write_control(&mut writer, kind::VERSION, &frame[nuschl_protocol::HEADER_BYTES..]).await?;
        let muted = options.muted || options.deafened;
        let mut state = State {
            udp,
            local_muted: muted,
            local_deafened: options.deafened,
            rejoin: options.channel,
            outbox: Outbox::new(options.message_rate),
            resend: options.resend,
            plugin_dropped,
            ..State::default()
        };
        state.restore_voice_route(options.voice_route.clone());
        // With the extension (or a sticky record) the server's hello decides
        // before any credential is sent.
        let mut kt_descriptor = None;
        if nuschl || !options.required.is_empty() {
            let frame = loop {
                let frame = timeout(Duration::from_secs(10), frames.recv())
                    .await
                    .map_err(|_| Error::Timeout("waiting for the server's version"))?
                    .ok_or_else(|| Error::Protocol("connection reader stopped".into()))??;
                if frame.kind == kind::VERSION {
                    break frame;
                }
                state.handle(frame, &events).await?;
                logged_in |= state.logged_in();
            };
            let negotiated = negotiate(nuschl, &frame.payload)?;
            options
                .required
                .check(negotiated.nuschl, &negotiated.capabilities)
                .map_err(Error::Downgrade)?;
            state.handle(frame, &events).await?;
            if negotiated.nuschl {
                below = negotiated
                    .policy
                    .as_ref()
                    .and_then(|policy| Below::of(policy, now_ms()));
                state.extension = Some(negotiated.capabilities.clone());
                if !negotiated.kt_log.is_empty() {
                    kt_descriptor = Some(negotiated.kt_log.clone());
                }
                emit(&events, SessionEvent::Negotiated(negotiated)).await?;
            }
        }
        if options.relay_only && !state.privacy_supported() {
            emit(&events, SessionEvent::Notice(RELAY_PRIVACY_UNAVAILABLE.into())).await?;
        }
        send(&mut writer, kind::AUTHENTICATE, &proto::Authenticate {
            username: Some(options.server.username.clone()),
            password: options.password,
            tokens: options.server.tokens.clone(),
            opus: Some(true),
            ..Default::default()
        })
        .await?;
        send(&mut writer, kind::USER_STATE, &proto::UserState {
            self_mute: Some(muted),
            self_deaf: Some(options.deafened),
            ..Default::default()
        })
        .await?;
        // The version and user state above count against the rate limit.
        state.outbox.level = 2.0;
        let mut ticks = 0_u64;
        let mut reported_udp = false;
        let mut ping = time::interval(Duration::from_secs(1));
        ping.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        let deadline = time::sleep_until(Instant::now() + Duration::from_secs(20));
        tokio::pin!(deadline);
        // Key transparency: the driver starts once we are logged in (see identity.rs).
        let mut kt = kt_descriptor.zip(options.crypto).map(|(descriptor, setup)| KtLink {
            descriptor,
            setup,
            // End-to-end is a server-level switch (`e2ee/1`): on a compatible server
            // there is none, even if it offers `mls/1`.
            mls: state
                .extension
                .as_ref()
                .is_some_and(|capabilities| capabilities.iter().any(|c| c == nuschl::capability::E2EE)),
            witness: state.extension.as_ref().is_some_and(|capabilities| {
                capabilities
                    .iter()
                    .any(|c| c == nuschl::capability::KT_WITNESS)
            }),
            server: nuschl_ipc::crypto::CryptoServer {
                key: crate::sticky::key(&options.server.host, options.server.port),
                label: if options.server.name.is_empty() {
                    options.server.host.clone()
                } else {
                    options.server.name.clone()
                },
            },
            running: None,
        });
        loop {
            if state.logged_in() {
                below = None;
            }
            if let Some(link) = kt.as_mut()
                && link.running.is_none()
                && state.logged_in()
            {
                link.start(&events);
            }
            tokio::select! {
                frame = frames.recv() => {
                    let frame = frame.ok_or_else(|| Error::Protocol("connection reader stopped".into()))??;
                    if let Some(frame) = kt_reply(kt.as_mut(), state.extension.is_some(), frame)? {
                        state.handle(frame, &events).await?;
                        // Record this before any outbound flush can fail.
                        logged_in |= state.logged_in();
                        if state.voice_target != 0 && state.voice_target != 31 && !state.voice_target_valid(state.voice_target) {
                            state.invalidate_voice_target();
                            emit(&events, SessionEvent::Notice("The whisper target changed or became unavailable. Microphone audio is suspended; select a new target or normal talking.".into())).await?;
                        }
                    }
                },
                outgoing = async {
                    match kt.as_mut().and_then(|link| link.running.as_mut()) {
                        Some(running) => running.frames.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some((kind, payload)) = outgoing {
                        write_control(&mut writer, kind, &payload).await?;
                    }
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
                        Received::Message(UdpMessage::Audio(packet)) => if !state.local_deafened && !state.encrypted_server() {
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
                    if acl_control(&command).is_some() {
                        state.enqueue_admin(command,&events).await?;
                        continue;
                    }
                    let request_id = match &command {
                        Command::Scoped { command, .. } => match command.as_ref() {
                            Command::Nuschl(request) => Some(request.request_id()),
                            _ => None,
                        },
                        _ => None,
                    };
                    let plugin_request = match &command {
                        Command::Scoped { command, .. } => match command.as_ref() {
                            Command::PluginData { request, .. } => Some(*request),
                            _ => None,
                        },
                        _ => None,
                    };
                    let admin_refusal = match &command {
                        Command::Scoped {authority,command} => {
                            let request=match command.as_ref() {Command::QueryAclDraft {request,channel}=>Some((*request,*channel,false)),Command::LookupAccount {request,channel,..}=>Some((*request,*channel,true)),_=>None};
                            request.map(|(request,channel,account)| {
                                let reason=if state.own_session!=Some(authority.own.0) {"The original connection's user session changed."}
                                else if state.users.get(&authority.own.0).is_none_or(|own|own.instance!=authority.own.1) {"The original connection's user presence changed."}
                                else if !authority.channels.iter().any(|(id,instance)|*id==channel && state.channels.get(id).is_some_and(|current|current.instance==*instance)) {"The original channel was replaced or was not covered by this action."}
                                else if !state.command_permitted(command) {"You no longer have permission to edit this channel."}
                                else {"The original action authority changed. Reopen permissions on the intended channel."};
                                (request,account,reason)
                            })
                        },_=>None,
                    };
                    let Some(command) = state.authorize(command) else {
                        // A receiver left (or its number now belongs to someone else)
                        // while the send was queued: refused, never re-addressed.
                        if let Some(request) = plugin_request {
                            state.plugin_admission(request, Err(plugin::Refusal::ReceiverChanged), &events);
                            continue;
                        }
                        if let Some((request,account,reason))=admin_refusal {
                            if account {emit(&events,SessionEvent::AccountReply(AccountReply {request,result:Err(reason.into())})).await?;}
                            else {emit(&events,SessionEvent::AclReply(AclReply {request,result:Err(reason.into())})).await?;}
                        }
                        if let Some(request_id) = request_id {
                            emit(&events, SessionEvent::Nuschl(NuschlEvent::Error(nuschl::error(request_id, v1::error::Code::NotFound, 0)))).await?;
                        }
                        let message = if state.voice_target == 31 {
                            "This action expired. Microphone audio is suspended; select a new voice target or normal talking."
                        } else { "This action expired; open it again for the current connection and target." };
                        emit(&events, SessionEvent::Notice(message.into())).await?;
                        continue;
                    };
                    match command {
                        Command::Disconnect => break,
                        Command::Identity(command) => {
                            if let Some(running) = kt.as_ref().and_then(|link| link.running.as_ref()) {
                                let _ = running.commands.try_send(command);
                            }
                        }
                        Command::Mls(command) => {
                            match kt.as_ref().and_then(|link| link.running.as_ref()).and_then(|running| running.mls_commands.as_ref()) {
                                Some(commands) => { let _ = commands.try_send(command); }
                                None => emit(&events, SessionEvent::Mls(crate::mls::MlsEvent::Failed(
                                    "This server is not end-to-end encrypted (it is a compatible server).".into()))).await?,
                            }
                        }
                        Command::SelectVoiceTargetAt { id, revision } => {
                            if id == 0 || state.voice_target_valid(id) { state.admit_voice_target(revision,id); }
                        },
                        Command::Voice { target, packet } => if state.voice_target_valid(if target == 0 { state.voice_target } else { target })
                            && let Some(packet) = state.outgoing_voice(packet)
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
                _ = time::sleep_until(state.admin_due().unwrap_or_else(Instant::now)), if state.admin_due().is_some() => {
                    state.dispatch_admin(&events).await?;
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
        if let Some(running) = kt.and_then(|link| link.running) {
            running.task.abort();
        }
        let _ = timeout(Duration::from_secs(2), writer.shutdown()).await;
        Ok(())
    }
    .await;
    let result = match result {
        Err(error) => Err(initial_login_error(error, logged_in, &mut frames).await),
        Ok(()) => Ok(()),
    };
    read_task.abort();
    let _ = read_task.await;
    result.map_err(|error| outdated(error, below.as_ref()))
}

/// The key-transparency driver of a `kt/1` connection with a crypto worker.
struct KtLink {
    descriptor: Vec<u8>,
    setup: crate::identity::IdentitySetup,
    /// `mls/1` was negotiated: the driver also runs end-to-end conversations.
    mls: bool,
    /// `kt-witness/1` was negotiated (docs/e2ee.md §5.9).
    witness: bool,
    server: nuschl_ipc::crypto::CryptoServer,
    running: Option<KtRunning>,
}

struct KtRunning {
    frames: mpsc::Receiver<(u16, Vec<u8>)>,
    replies: mpsc::Sender<(u16, bytes::Bytes)>,
    commands: mpsc::Sender<crate::identity::IdentityCommand>,
    mls_commands: Option<mpsc::Sender<crate::mls::MlsCommand>>,
    /// Welcomes and rows of end-to-end conversations; a full queue drops them
    /// (the driver catches up from the stream).
    pushes: Option<mpsc::Sender<(u16, bytes::Bytes)>>,
    task: tokio::task::JoinHandle<()>,
}

impl KtLink {
    fn start(&mut self, events: &mpsc::Sender<SessionEvent>) {
        let (frames_tx, frames) = mpsc::channel(8);
        let (replies, replies_rx) = mpsc::channel(8);
        let (commands, commands_rx) = mpsc::channel(32);
        let (mls_commands, pushes, mls) = if self.mls {
            let (commands, commands_rx) = mpsc::channel(32);
            let (pushes, pushes_rx) = mpsc::channel(64);
            (
                Some(commands),
                Some(pushes),
                Some(crate::identity::MlsLink {
                    commands: commands_rx,
                    pushes: pushes_rx,
                }),
            )
        } else {
            (None, None, None)
        };
        let task = tokio::spawn(crate::identity::drive(crate::identity::Driver {
            crypto: self.setup.crypto.clone(),
            server: self.server.clone(),
            descriptor: self.descriptor.clone(),
            device_name: self.setup.device_name.clone(),
            recovery: self.setup.recovery,
            frames: frames_tx,
            replies: replies_rx,
            commands: commands_rx,
            events: events.clone(),
            mls,
            witness: self.witness,
            witnessing: self.setup.witnessing.clone(),
        }));
        self.running = Some(KtRunning {
            frames,
            replies,
            commands,
            mls_commands,
            pushes,
            task,
        });
    }
}

/// Hands the driver its replies (and errors for its requests); everything else
/// goes on to the session.
fn kt_reply(
    link: Option<&mut KtLink>,
    extension: bool,
    frame: ControlFrame,
) -> Result<Option<ControlFrame>, Error> {
    use nuschl::kind;
    let Some(running) = link.and_then(|link| link.running.as_mut()) else {
        return Ok(Some(frame));
    };
    if !extension {
        return Ok(Some(frame));
    }
    let reply = matches!(
        frame.kind,
        kind::KT_STATUS_REPLY
            | kind::KT_PUBLISHED
            | kind::KT_LOOKUP_REPLY
            | kind::KT_HISTORY_REPLY
            | kind::KT_AUDIT_REPLY
            | kind::KT_HEAD_REPLY
            | kind::MLS_KEY_PACKAGE_STATUS
            | kind::MLS_KEY_PACKAGES
            | kind::MLS_ACK
            | kind::MLS_WELCOMES
            | kind::MLS_JOIN_INFO
    );
    // Replies to the driver's own requests of other kinds (history pages, direct
    // conversations), by its request ids.
    let by_id = matches!(
        frame.kind,
        kind::ERROR | kind::HISTORY_PAGE | kind::DIRECT_LIST | kind::DIRECT_OPENED | kind::DONE
    ) && v1::Done::decode(frame.payload.clone()).is_ok_and(|done| {
        done.request_id & crate::identity::KT_REQUEST != 0 && done.request_id & INTERNAL == 0
    });
    if frame.payload.len() > nuschl::server_limit(frame.kind) {
        return Err(Error::Protocol("oversized Nuschl message".into()));
    }
    if reply || by_id {
        let _ = running.replies.try_send((frame.kind, frame.payload));
        return Ok(None);
    }
    // Pushes for end-to-end conversations: Welcomes, and rows the frontend never
    // sees as text.
    let push = frame.kind == kind::MLS_WELCOME_PUSH
        || frame.kind == kind::MLS_TASK
        || (frame.kind == kind::CHAT_EVENT && running.mls_commands.is_some());
    if push {
        if let Some(pushes) = &running.pushes {
            let _ = pushes.try_send((frame.kind, frame.payload));
        }
        return Ok(None);
    }
    Ok(Some(frame))
}

fn resolved_account(
    name: &str,
    query: &proto::QueryUsers,
) -> Result<RegisteredAccount, &'static str> {
    // A one-name query has zero or one canonical pair. Do not accept unrelated
    // bulk/partial data as the fresh result of this explicit lookup.
    if query.ids.len() != query.names.len() || query.ids.len() > 1 {
        return Err("The server returned an invalid registered-account lookup response.");
    }
    match query.ids.first().zip(query.names.first()) {
        Some((&id, canonical))
            if id != u32::MAX
                && canonical.len() <= 512
                && canonical.to_lowercase() == name.to_lowercase() =>
        {
            Ok(RegisteredAccount {
                id,
                name: canonical.clone(),
            })
        }
        _ => Err("No registered account with that name was found."),
    }
}

struct AccountLookup {
    request: u64,
    name: String,
    channel: (u32, u64),
    own: (u32, u64),
    barrier: u64,
    waiting_reply: bool,
}

struct PendingAdmin {
    command: Command,
    bytes: usize,
}

fn acl_control(command: &Command) -> Option<&Command> {
    let command = match command {
        Command::Scoped { command, .. } => command.as_ref(),
        command => command,
    };
    matches!(
        command,
        Command::QueryAcl(_) | Command::QueryAclDraft { .. } | Command::UpdateAcl(_)
    )
    .then_some(command)
}
fn admin_ticket(command: &Command) -> Option<(u64, bool)> {
    let command = match command {
        Command::Scoped { command, .. } => command.as_ref(),
        command => command,
    };
    match command {
        Command::QueryAclDraft { request, .. } => Some((*request, false)),
        Command::LookupAccount { request, .. } => Some((*request, true)),
        _ => None,
    }
}
async fn refuse_admin(
    ticket: Option<(u64, bool)>,
    message: &str,
    events: &mpsc::Sender<SessionEvent>,
) -> Result<(), Error> {
    match ticket {
        Some((request, false)) => {
            emit(
                events,
                SessionEvent::AclReply(AclReply {
                    request,
                    result: Err(message.into()),
                }),
            )
            .await
        }
        Some((request, true)) => {
            emit(
                events,
                SessionEvent::AccountReply(AccountReply {
                    request,
                    result: Err(message.into()),
                }),
            )
            .await
        }
        None => emit(events, SessionEvent::Notice(message.into())).await,
    }
}

struct State {
    udp: Option<Udp>,
    tcp_ping: PingStats,
    /// Control messages produced while handling a frame or command, flushed by the loop.
    outbound: Vec<(u16, Vec<u8>)>,
    channels: BTreeMap<u32, Channel>,
    users: BTreeMap<u32, User>,
    own_session: Option<u32>,
    /// Exact bounded source last emitted on this connection; duplicates stay dismissed.
    welcome_source: Option<String>,
    version: u64,
    limits: nuschl_core::model::ServerLimits,
    opus: bool,
    local_muted: bool,
    local_deafened: bool,
    rejoin: Option<u32>,
    voice_target: u32,
    voice_revision: u64,
    voice_route: std::sync::Arc<VoiceRoute>,
    /// Installed explicit targets retain authority through later voice packets.
    voice_authorities: BTreeMap<u32, CommandAuthority>,
    permissions: BTreeMap<u32, u32>,
    /// Never expire/delete an unresolved query: a late wire reply has no ticket
    /// and must not be attributed to a replacement draft or reused channel ID.
    acl_queries: BTreeMap<u32, (u64, Option<u64>)>,
    admin_pending: std::collections::VecDeque<PendingAdmin>,
    admin_bytes: usize,
    account_lookup: Option<AccountLookup>,
    deferred_name_queries: Vec<proto::QueryUsers>,
    context_actions: BTreeMap<String, ContextAction>,
    unsupported_codec: BTreeSet<u32>,
    /// Bytes of comments, descriptions and avatars in `users` and `channels`.
    blob_bytes: usize,
    /// Budget notices already shown on this connection.
    notices: BTreeSet<&'static str>,
    outbox: Outbox,
    /// The Nuschl extension's capabilities, when negotiated.
    extension: Option<Vec<String>>,
    /// Channel → conversation id (Nuschl servers).
    conversations: BTreeMap<u32, [u8; 16]>,
    /// Our direct conversations (Nuschl servers), from `DirectList`,
    /// `DirectOpened` and messages in conversations not known before.
    directs: BTreeMap<[u8; 16], v1::Direct>,
    /// Our sends in flight by request id, with their attempts.
    sending: BTreeMap<u64, (PendingChat, u32)>,
    subscribed: BTreeSet<u32>,
    next_request: u64,
    resend: Vec<PendingChat>,
    plugin_rate: PluginRate,
    accept_plugin_data: bool,
    plugin_receive: PluginReceive,
    /// Plugin data dropped on this connection: oversized frames (counted by
    /// the reader), undecodable or unknown-sender messages, over the receive
    /// budget, or with no room in the event channel.
    plugin_dropped: std::sync::Arc<AtomicU64>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            udp: None,
            tcp_ping: PingStats::default(),
            outbound: Vec::new(),
            channels: BTreeMap::new(),
            users: BTreeMap::new(),
            own_session: None,
            welcome_source: None,
            version: 0,
            limits: nuschl_core::model::ServerLimits::default(),
            opus: true,
            local_muted: true,
            local_deafened: false,
            rejoin: None,
            voice_target: 0,
            voice_revision: 0,
            voice_route: Default::default(),
            voice_authorities: BTreeMap::new(),
            permissions: BTreeMap::new(),
            acl_queries: BTreeMap::new(),
            admin_pending: std::collections::VecDeque::new(),
            admin_bytes: 0,
            account_lookup: None,
            deferred_name_queries: Vec::new(),
            context_actions: BTreeMap::new(),
            unsupported_codec: BTreeSet::new(),
            blob_bytes: 0,
            notices: BTreeSet::new(),
            outbox: Outbox::new(MessageRate::default()),
            extension: None,
            conversations: BTreeMap::new(),
            directs: BTreeMap::new(),
            sending: BTreeMap::new(),
            subscribed: BTreeSet::new(),
            next_request: 0,
            resend: Vec::new(),
            plugin_rate: PluginRate::new(),
            accept_plugin_data: false,
            plugin_receive: PluginReceive::new(),
            plugin_dropped: Default::default(),
        }
    }
}
impl State {
    async fn welcome(
        &mut self,
        source: String,
        events: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), Error> {
        if self.own_session.is_none() {
            return Ok(());
        }
        let source = bounded(source, MAX_CONTENT_BYTES);
        if self.welcome_source.as_deref() == Some(source.as_str()) {
            return Ok(());
        }
        self.welcome_source = Some(source.clone());
        emit(events, SessionEvent::Welcome(source)).await
    }

    fn admin_due(&self) -> Option<Instant> {
        (!self.admin_pending.is_empty()).then(|| self.outbox.slot_due())
    }
    /// Keep semantic commands and the ORIGINAL authority until actual dispatch.
    /// No pre-authorized/serialized frames wait behind the shared rate bucket.
    async fn enqueue_admin(
        &mut self,
        command: Command,
        events: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), Error> {
        let ticket = admin_ticket(&command);
        let Some(inner) = acl_control(&command) else {
            return refuse_admin(ticket, "Unsupported ACL command.", events).await;
        };
        let channel = match inner {
            Command::QueryAcl(channel) | Command::QueryAclDraft { channel, .. } => *channel,
            Command::UpdateAcl(acl) => acl.channel,
            _ => unreachable!(),
        };
        let bytes = match inner {
            Command::UpdateAcl(acl) => {
                if nuschl_core::admin::validate_snapshot(acl).is_err() {
                    return refuse_admin(
                        ticket,
                        "This ACL exceeds the safe send limits; no rules were sent.",
                        events,
                    )
                    .await;
                }
                acl.entries
                    .iter()
                    .fold(4096usize, |bytes, entry| {
                        bytes
                            .saturating_add(64)
                            .saturating_add(entry.group.as_ref().map_or(0, String::len))
                    })
                    .saturating_add(acl.groups.iter().fold(0usize, |bytes, group| {
                        bytes
                            .saturating_add(128)
                            .saturating_add(group.name.len())
                            .saturating_add(
                                6usize.saturating_mul(
                                    group
                                        .add
                                        .len()
                                        .saturating_add(group.remove.len())
                                        .saturating_add(group.inherited_members.len()),
                                ),
                            )
                    }))
            }
            _ => 4096,
        };
        if bytes > MAX_CONTROL_BYTES
            || self.admin_pending.len() >= 16
            || self.admin_bytes.saturating_add(bytes) > 16 * 1024 * 1024
        {
            return refuse_admin(
                ticket,
                "The bounded ACL send queue is full. Your draft is retained; wait before retrying.",
                events,
            )
            .await;
        }
        let command = match command {
            command @ Command::Scoped { .. } => command,
            command => {
                // Diagnostic/legacy callers select the current actor lifetime.
                // Never synthesize replacement authority for a supplied envelope.
                let own = self
                    .own_session
                    .and_then(|id| self.users.get(&id).map(|own| (id, own.instance)));
                let instance = self.channels.get(&channel).map(|channel| channel.instance);
                let (Some(own), Some(instance)) = (own, instance) else {
                    return refuse_admin(
                        ticket,
                        "The original connection or channel is unavailable.",
                        events,
                    )
                    .await;
                };
                Command::Scoped {
                    authority: CommandAuthority {
                        own,
                        subject: Some(ActionSubject::Channel(channel, instance)),
                        users: vec![own],
                        channels: vec![(channel, instance)],
                        conversations: Vec::new(),
                        accounts: Vec::new(),
                    },
                    command: Box::new(command),
                }
            }
        };
        if let Command::Scoped { authority, .. } = &command
            && authority
                .users
                .len()
                .saturating_add(authority.channels.len())
                .saturating_add(authority.conversations.len())
                .saturating_add(authority.accounts.len())
                > MAX_ACTION_TARGETS
        {
            return refuse_admin(
                ticket,
                "The ACL action authority exceeds its bounded target limit.",
                events,
            )
            .await;
        }
        self.admin_pending
            .push_back(PendingAdmin { command, bytes });
        self.admin_bytes += bytes;
        Ok(())
    }
    async fn dispatch_admin(&mut self, events: &mpsc::Sender<SessionEvent>) -> Result<(), Error> {
        if self.admin_pending.is_empty() || !self.outbox.ready(Instant::now()) {
            return Ok(());
        }
        let pending = self.admin_pending.pop_front().unwrap();
        self.admin_bytes -= pending.bytes;
        let ticket = admin_ticket(&pending.command);
        let Some(command) = self.authorize(pending.command) else {
            return refuse_admin(ticket,"The original ACL action expired or its permissions changed. Reopen permissions on the intended channel.",events).await;
        };
        self.command(command, events).await
    }

    async fn query_acl(
        &mut self,
        channel: u32,
        request: Option<u64>,
        events: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), Error> {
        let error = if self.account_lookup.is_some() {
            Some(
                "A registered-account lookup is unresolved. Wait for its reply or reconnect before querying ACLs.",
            )
        } else if self.acl_queries.contains_key(&channel) {
            Some(
                "An earlier ACL query for this channel is still unresolved. Wait for its reply or reconnect before reloading.",
            )
        } else if self.acl_queries.len() >= 128 {
            Some("Too many unresolved ACL queries. Reconnect before opening another editor.")
        } else if !self.channels.contains_key(&channel) {
            Some("The original channel no longer exists.")
        } else {
            None
        };
        if let Some(error) = error {
            if let Some(request) = request {
                emit(
                    events,
                    SessionEvent::AclReply(AclReply {
                        request,
                        result: Err(error.into()),
                    }),
                )
                .await?;
            } else {
                emit(events, SessionEvent::Notice(error.into())).await?;
            }
            return Ok(());
        }
        let instance = self.channels[&channel].instance;
        self.acl_queries.insert(channel, (instance, request));
        self.queue(
            kind::ACL,
            &proto::Acl {
                channel_id: channel,
                query: Some(true),
                ..Default::default()
            },
        );
        Ok(())
    }

    fn ping_message(&self, timestamp: u64) -> proto::Ping {
        let stats = self.transport();
        proto::Ping {
            timestamp: Some(timestamp),
            good: Some(stats.good),
            late: Some(stats.late),
            lost: Some(stats.lost),
            resync: Some(stats.resync),
            udp_packets: Some(stats.udp.packets),
            tcp_packets: Some(stats.tcp.packets),
            udp_ping_avg: Some(stats.udp.average_ms),
            udp_ping_var: Some(stats.udp.variance_ms),
            tcp_ping_avg: Some(stats.tcp.average_ms),
            tcp_ping_var: Some(stats.tcp.variance_ms),
        }
    }
    fn lookup_valid(&self, lookup: &AccountLookup) -> bool {
        self.own_session == Some(lookup.own.0)
            && self
                .users
                .get(&lookup.own.0)
                .is_some_and(|own| own.instance == lookup.own.1)
            && self
                .channels
                .get(&lookup.channel.0)
                .is_some_and(|channel| channel.instance == lookup.channel.1)
            && (self
                .permissions
                .get(&lookup.channel.0)
                .is_none_or(|bits| bits & crate::model::permission::WRITE != 0)
                || self
                    .permissions
                    .get(&0)
                    .is_none_or(|bits| bits & crate::model::permission::WRITE != 0))
    }
    fn queue_names(&mut self, query: proto::QueryUsers) {
        if self.account_lookup.is_some() {
            if self.deferred_name_queries.len() < 16 {
                self.deferred_name_queries.push(query);
            }
        } else {
            self.queue(kind::QUERY_USERS, &query);
        }
    }
    fn release_name_queries(&mut self) {
        let queries = std::mem::take(&mut self.deferred_name_queries);
        for query in queries {
            self.queue(kind::QUERY_USERS, &query);
        }
    }
    async fn lookup_account(
        &mut self,
        channel: u32,
        request: u64,
        name: String,
        events: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), Error> {
        let own = self
            .own_session
            .and_then(|id| self.users.get(&id).map(|user| (id, user.instance)));
        let instance = self.channels.get(&channel).map(|channel| channel.instance);
        if self.account_lookup.is_some()
            || !self.acl_queries.is_empty()
            || own.is_none()
            || instance.is_none()
            || name.trim().is_empty()
            || name.len() > 512
        {
            return emit(events,SessionEvent::AccountReply(AccountReply {request,result:Err("The original channel is unavailable or an earlier administration lookup is unresolved. Wait for its response or reconnect.".into())})).await;
        }
        let barrier = (1_u64 << 63) | next_user_instance();
        self.account_lookup = Some(AccountLookup {
            request,
            name: name.trim().into(),
            channel: (channel, instance.unwrap()),
            own: own.unwrap(),
            barrier,
            waiting_reply: false,
        });
        // The high timestamp namespace is never used for RTT. Preserve ALL
        // transport counters: Mumble servers overwrite missing fields with zero.
        self.queue(kind::PING, &self.ping_message(barrier));
        Ok(())
    }

    fn encrypted_server(&self) -> bool {
        self.extension
            .as_ref()
            .is_some_and(|caps| caps.iter().any(|cap| cap == nuschl::capability::E2EE))
    }

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
    /// Our plugin data, checked as stock Mumble checks it (a server that
    /// relays it, receivers it knows) plus this connection's policy and each
    /// receiver's presence, then admitted by the local bucket: charged once,
    /// only when it is queued.
    fn send_plugin_data(
        &mut self,
        receivers: &[(u32, u64)],
        payload: plugin::Payload,
    ) -> Result<(), plugin::Refusal> {
        use plugin::Refusal;
        // An end-to-end server carries nothing in plaintext between clients
        // (docs/e2ee.md §10, answer 7).
        if self.encrypted_server() {
            return Err(Refusal::EndToEnd);
        }
        if payload.is_reserved() {
            return Err(Refusal::Reserved);
        }
        // Servers before Mumble 1.4 do not relay it (API_v_1_x_x.cpp).
        if self.version < (1 << 48) | (4 << 32) {
            return Err(Refusal::ServerTooOld);
        }
        if receivers.is_empty() || receivers.len() > MAX_ACTION_TARGETS {
            return Err(Refusal::Receivers(MAX_ACTION_TARGETS));
        }
        let mut sessions = BTreeSet::new();
        for &(session, instance) in receivers {
            if instance == 0 || self.users.get(&session).map(|user| user.instance) != Some(instance)
            {
                return Err(Refusal::ReceiverChanged);
            }
            sessions.insert(session);
        }
        self.plugin_rate
            .admit(Instant::now())
            .map_err(|retry_after_ms| Refusal::RateLimited { retry_after_ms })?;
        let message = plugin::PluginData {
            sender: self.own_session,
            receivers: sessions.into_iter().collect(),
            payload,
        }
        .into_message();
        self.queue(kind::PLUGIN_DATA_TRANSMISSION, &message);
        Ok(())
    }

    /// Report a send's admission without ever waiting for the event channel.
    fn plugin_admission(
        &self,
        request: u64,
        result: Result<(), plugin::Refusal>,
        events: &mpsc::Sender<SessionEvent>,
    ) {
        if events
            .try_send(SessionEvent::PluginDataAdmission { request, result })
            .is_err()
        {
            self.plugin_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Plugin data from the server. Skipped undecoded unless someone asked
    /// for it, we are logged in and the server is not end to end; then
    /// budgeted, bounded before it is decoded, and surfaced with its sender's
    /// identity as known right now, without waiting for the event channel.
    /// Nothing here interprets the bytes, and the reserved namespace has no
    /// generic consumer. Nothing in it can end the connection.
    fn plugin_data(&mut self, frame: &ControlFrame, events: &mpsc::Sender<SessionEvent>) {
        if !self.accept_plugin_data || self.own_session.is_none() || self.encrypted_server() {
            return;
        }
        let counter = std::sync::Arc::clone(&self.plugin_dropped);
        let dropped = || {
            counter.fetch_add(1, Ordering::Relaxed);
        };
        if !self
            .plugin_receive
            .admit(frame.payload.len(), Instant::now())
        {
            return dropped();
        }
        let Ok(message) = plugin::PluginData::decode(&frame.payload, MAX_RECEIVED_RECEIVERS) else {
            return dropped();
        };
        if message.payload.is_reserved() {
            return;
        }
        // Stock clients ignore data without a sender they know; one sent to
        // ourselves is ours.
        let Some(sender) = message
            .sender
            .and_then(|session| self.users.get(&session))
            .filter(|user| user.instance != 0)
        else {
            return dropped();
        };
        let incoming = IncomingPluginData {
            sender: sender.session,
            sender_instance: sender.instance,
            sender_user_id: sender.user_id,
            sender_certificate_hash: sender.certificate_hash.clone(),
            payload: message.payload,
        };
        if events.try_send(SessionEvent::PluginData(incoming)).is_err() {
            dropped();
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
        if !self.voice_route.allows(self.voice_revision)
            || self.encrypted_server()
            || self.own_session.is_none()
            || !self.opus
        {
            return None;
        }
        if !self.local_muted && !self.local_deafened && self.may_speak() {
            return Some(packet);
        }
        packet.end.then(|| {
            packet.opus = nuschl_core::audio::SILENT_OPUS.to_vec();
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
    fn privacy_supported(&self) -> bool {
        self.extension
            .as_ref()
            .is_some_and(|caps| caps.iter().any(|cap| cap == nuschl::capability::PRIVACY))
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

    fn authority_valid(&self, authority: &CommandAuthority) -> bool {
        if authority
            .users
            .len()
            .saturating_add(authority.channels.len())
            .saturating_add(authority.conversations.len())
            .saturating_add(authority.accounts.len())
            > MAX_ACTION_TARGETS
            || authority.users.len() > MAX_ACTION_TARGETS
            || authority.channels.len() > MAX_ACTION_TARGETS
            || authority.conversations.len() > MAX_ACTION_TARGETS
            || authority.accounts.len() > MAX_ACTION_TARGETS
            || self.own_session != Some(authority.own.0)
            || self
                .users
                .get(&authority.own.0)
                .is_none_or(|u| u.instance == 0 || u.instance != authority.own.1)
        {
            return false;
        }
        let subject_valid = match authority.subject {
            Some(ActionSubject::User(id, instance)) => {
                instance != 0
                    && self
                        .users
                        .get(&id)
                        .is_some_and(|user| user.instance == instance)
            }
            Some(ActionSubject::Channel(id, instance)) => {
                instance != 0
                    && self
                        .channels
                        .get(&id)
                        .is_some_and(|channel| channel.instance == instance)
            }
            None => true,
        };
        subject_valid
            && authority.users.iter().all(|(id, instance)| {
                *instance != 0 && self.users.get(id).is_some_and(|u| u.instance == *instance)
            })
            && authority.channels.iter().all(|(id, instance)| {
                *instance != 0
                    && self
                        .channels
                        .get(id)
                        .is_some_and(|c| c.instance == *instance)
            })
    }

    /// Unknown permissions are left to the server; a known revocation must
    /// take effect even while a UI prompt or native command is queued.
    fn known_permission(&self, channel: u32, permission: u32) -> bool {
        use crate::model::permission::WRITE;
        self.permissions
            .get(&channel)
            .is_none_or(|bits| bits & (permission | WRITE) != 0)
    }

    fn command_permitted(&self, command: &Command) -> bool {
        use crate::model::permission as p;
        let may = |channel, permission| self.known_permission(channel, permission);
        match command {
            Command::Join(id) => may(*id, p::ENTER),
            Command::CreateChannel {
                parent, temporary, ..
            } => may(
                *parent,
                if *temporary {
                    p::MAKE_TEMP_CHANNEL
                } else {
                    p::MAKE_CHANNEL
                },
            ),
            Command::EditChannel { channel, .. } | Command::RemoveChannel(channel) => {
                may(*channel, p::WRITE)
            }
            Command::QueryAcl(channel)
            | Command::QueryAclDraft { channel, .. }
            | Command::LookupAccount { channel, .. } => may(*channel, p::WRITE) || may(0, p::WRITE),
            Command::UpdateAcl(acl) => may(acl.channel, p::WRITE) || may(0, p::WRITE),
            Command::LinkChannels {
                channel,
                add,
                remove,
            } => {
                may(*channel, p::LINK_CHANNEL)
                    && add.iter().chain(remove).all(|id| may(*id, p::LINK_CHANNEL))
            }
            Command::Listen { channel, .. } => may(*channel, p::LISTEN),
            Command::User {
                session, action, ..
            } => match action {
                UserAction::Kick => may(0, p::KICK),
                UserAction::Ban => may(0, p::BAN),
                UserAction::Register => may(0, p::REGISTER),
                UserAction::ResetComment | UserAction::ResetAvatar => may(0, p::RESET_USER_CONTENT),
                UserAction::Move(channel) => may(*channel, p::MOVE),
                _ => self
                    .users
                    .get(session)
                    .is_some_and(|user| may(user.channel, p::MUTE_DEAFEN)),
            },
            Command::RegisterSelf => may(0, p::SELF_REGISTER),
            Command::QueryRegisteredUsers | Command::UpdateRegisteredUsers(_) => {
                may(0, p::REGISTER)
            }
            Command::QueryBans | Command::UpdateBans(_) => may(0, p::BAN),
            Command::SetVoiceTarget { targets, .. } | Command::Whisper { targets, .. } => {
                targets.iter().all(|target| match target {
                    VoiceTargetSpec::Users(ids) => ids.iter().all(|id| {
                        self.users
                            .get(id)
                            .is_some_and(|user| may(user.channel, p::WHISPER))
                    }),
                    VoiceTargetSpec::Channel { channel, .. } => may(*channel, p::WHISPER),
                })
            }
            Command::ContextAction {
                action,
                session,
                channel,
            } => self.context_actions.get(action).is_some_and(|action| {
                if session.is_some() {
                    action.user
                } else if channel.is_some() {
                    action.channel
                } else {
                    action.server
                }
            }),
            _ => true,
        }
    }

    fn semantic_targets_covered(&self, command: &Command, authority: &CommandAuthority) -> bool {
        let user_id = |wanted: u32| {
            wanted != 0
                && authority
                    .users
                    .iter()
                    .copied()
                    .chain(match authority.subject {
                        Some(ActionSubject::User(id, instance)) => Some((id, instance)),
                        _ => None,
                    })
                    .any(|(session, instance)| {
                        self.users.get(&session).is_some_and(|user| {
                            user.instance == instance && user.user_id == Some(wanted)
                        })
                    })
        };
        let conversation = |bytes: &[u8]| {
            let Some(id) = nuschl::id(bytes) else {
                return false;
            };
            if !authority.conversations.contains(&id) {
                return false;
            }
            if let Some((channel, _)) = self.conversations.iter().find(|(_, held)| **held == id) {
                return authority
                    .channels
                    .iter()
                    .copied()
                    .chain(match authority.subject {
                        Some(ActionSubject::Channel(id, instance)) => Some((id, instance)),
                        _ => None,
                    })
                    .any(|(held, instance)| {
                        held == *channel
                            && self
                                .channels
                                .get(&held)
                                .is_some_and(|current| current.instance == instance)
                    });
            }
            // Direct IDs are immutable server-side conversation identities;
            // the explicit lease remains usable if the peer goes offline.
            self.directs.contains_key(&id)
        };
        match command {
            Command::Nuschl(request) => match request {
                NuschlRequest::OpenDirect(request) => user_id(request.user_id),
                NuschlRequest::History(request) => conversation(&request.conversation),
                NuschlRequest::ReadMarker(request) => conversation(&request.conversation),
                NuschlRequest::Edit(request) => conversation(&request.conversation),
                NuschlRequest::Delete(request) => conversation(&request.conversation),
                NuschlRequest::ShareLinkCreate(_) => true, // numeric coverage checked centrally
                _ => false,
            },
            Command::Mls(command) => match command {
                crate::mls::MlsCommand::OpenDirect { user_id: id } => user_id(*id),
                crate::mls::MlsCommand::OpenChannel { conversation: id }
                | crate::mls::MlsCommand::Status { conversation: id }
                | crate::mls::MlsCommand::Sync { conversation: id }
                | crate::mls::MlsCommand::History {
                    conversation: id, ..
                } => conversation(id),
                _ => false,
            },
            Command::Identity(command) => match command {
                crate::identity::IdentityCommand::Check {
                    user_id: user,
                    account,
                    ..
                } => {
                    (user.is_some() || account.is_some())
                        && user.is_none_or(user_id)
                        && account.is_none_or(|account| authority.accounts.contains(&account))
                }
                crate::identity::IdentityCommand::MarkVerified { account }
                | crate::identity::IdentityCommand::UnmarkVerified { account }
                | crate::identity::IdentityCommand::AcceptKeyChange { account } => {
                    authority.accounts.contains(account)
                }
                _ => false,
            },
            _ => true,
        }
    }

    fn voice_target_valid(&self, id: u32) -> bool {
        use crate::model::permission::WHISPER;
        id == 0
            || self.voice_authorities.get(&id).is_some_and(|authority| {
                self.authority_valid(authority)
                    && authority.users.iter().all(|(id, _)| {
                        self.users
                            .get(id)
                            .is_some_and(|user| self.known_permission(user.channel, WHISPER))
                    })
                    && authority
                        .channels
                        .iter()
                        .all(|(id, _)| self.known_permission(*id, WHISPER))
            })
    }

    fn restore_voice_route(&mut self, route: std::sync::Arc<VoiceRoute>) {
        self.voice_revision = route.revision();
        self.voice_target = if route.target() == 0 { 0 } else { 31 };
        if self.voice_target == 31 {
            route.suspend_revision(self.voice_revision);
        }
        self.voice_route = route;
    }
    fn invalidate_voice_target(&mut self) {
        self.voice_target = 31;
        self.voice_route.suspend_revision(self.voice_revision);
    }
    fn admit_voice_target(&mut self, revision: u64, id: u32) -> bool {
        if !self.voice_route.admit(revision, id) {
            return false;
        }
        self.voice_revision = revision;
        self.voice_target = id;
        true
    }
    #[cfg(test)]
    fn select_voice_target(&mut self, id: u32) {
        let revision = self.voice_route.suspend();
        self.voice_revision = revision;
        self.voice_target = 31;
        if id <= 30 {
            self.admit_voice_target(revision, id);
        }
    }

    fn authorize(&mut self, command: Command) -> Option<Command> {
        let (mut command, mut authority) = match command {
            Command::Scoped { authority, command } => (*command, Some(authority)),
            command => (command, None),
        };
        if matches!(command, Command::Scoped { .. }) {
            return None;
        }
        if authority.is_some()
            && matches!(
                command,
                Command::SetVoiceTarget { .. }
                    | Command::SelectVoiceTarget(_)
                    | Command::Whisper { revision: 0, .. }
            )
        {
            return None;
        }

        // Direct session/CLI installations get observed lifetime descriptors;
        // native delayed actions already captured theirs before enqueue.
        if authority.is_none()
            && matches!(
                command,
                Command::SetVoiceTarget { .. } | Command::Whisper { .. }
            )
        {
            let (users, channels) = command.targets()?;
            let own = self.own_session.and_then(|id| self.users.get(&id))?;
            authority = Some(CommandAuthority {
                subject: None,
                own: (own.session, own.instance),
                users: users
                    .iter()
                    .map(|id| self.users.get(id).map(|user| (*id, user.instance)))
                    .collect::<Option<_>>()?,
                channels: channels
                    .iter()
                    .map(|id| self.channels.get(id).map(|channel| (*id, channel.instance)))
                    .collect::<Option<_>>()?,
                conversations: vec![],
                accounts: vec![],
            });
        }
        match &mut command {
            Command::Whisper { revision, id, .. } => {
                if *revision == 0 {
                    *revision = self.voice_route.suspend();
                }
                if !self.voice_route.current(*revision) {
                    return None;
                }
                self.voice_target = 31;
                self.voice_revision = *revision;
                self.voice_authorities.remove(id);
            }
            Command::SelectVoiceTarget(id) => {
                command = Command::SelectVoiceTargetAt {
                    id: *id,
                    revision: self.voice_route.suspend(),
                };
            }
            Command::SetVoiceTarget { id, .. } => {
                self.voice_authorities.remove(id);
                if self.voice_target == *id {
                    self.voice_revision = self.voice_route.suspend();
                    self.voice_target = 31;
                }
            }
            _ => {}
        }
        if let Command::SelectVoiceTargetAt { revision, .. } = &command {
            if !self.voice_route.current(*revision) {
                return None;
            }
            self.voice_target = 31;
            self.voice_revision = *revision;
        }
        let Some(authority) = authority else {
            return Some(command);
        };
        let (users, channels) = command.targets()?;
        if !self.authority_valid(&authority)
            || !users
                .iter()
                .all(|id| authority.users.iter().any(|(held, _)| id == held))
            || !channels
                .iter()
                .all(|id| authority.channels.iter().any(|(held, _)| id == held))
            || !self.semantic_targets_covered(&command, &authority)
            || !self.command_permitted(&command)
        {
            return None;
        }
        if let Command::SelectVoiceTargetAt { id, .. } = &command
            && *id != 0
            && self.voice_authorities.get(id).is_none_or(|installed| {
                installed.own != authority.own
                    || !installed
                        .users
                        .iter()
                        .all(|target| authority.users.contains(target))
                    || !installed
                        .channels
                        .iter()
                        .all(|target| authority.channels.contains(target))
            })
        {
            return None;
        }
        if let Command::SetVoiceTarget { id, .. } | Command::Whisper { id, .. } = &command {
            if !(1..=30).contains(id) {
                return None;
            }
            let mut authority = authority;
            authority.users.retain(|(id, _)| users.contains(id));
            authority.channels.retain(|(id, _)| channels.contains(id));
            authority.subject = None;
            authority.conversations.clear();
            authority.accounts.clear();
            self.voice_authorities.insert(*id, authority);
        }
        Some(command)
    }

    async fn command(
        &mut self,
        command: Command,
        events: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), Error> {
        if let Command::AcceptPluginData(accept) = command {
            self.accept_plugin_data = accept;
            return Ok(());
        }
        if !self.logged_in() {
            if let Command::PluginData { request, .. } = command {
                self.plugin_admission(request, Err(plugin::Refusal::NotConnected), events);
                return Ok(());
            }
            return emit(
                events,
                SessionEvent::Notice("Still connecting; try again in a moment.".into()),
            )
            .await;
        }
        if self.encrypted_server()
            && matches!(command, Command::SendText { .. } | Command::SendHtml { .. })
        {
            return emit(events, SessionEvent::Notice("Plaintext chat and attachments are disabled on an encrypted server; send text through MLS.".into())).await;
        }
        let own = self.own_session.unwrap_or_default();
        let whisper_revision = match &command {
            Command::Whisper { revision, .. } => Some(*revision),
            _ => None,
        };
        match command {
            Command::RelayOnly(relay_only) => {
                if self.privacy_supported() {
                    self.queue(nuschl::kind::PRIVACY, &v1::Privacy { relay_only });
                } else if relay_only {
                    emit(
                        events,
                        SessionEvent::Notice(RELAY_PRIVACY_UNAVAILABLE.into()),
                    )
                    .await?;
                }
            }
            Command::PluginData {
                request,
                receivers,
                payload,
            } => {
                let result = self.send_plugin_data(&receivers, payload);
                self.plugin_admission(request, result, events);
            }
            Command::Scoped { .. }
            | Command::AcceptPluginData(_)
            | Command::Disconnect
            | Command::Voice { .. }
            | Command::SelectVoiceTarget(_)
            | Command::SelectVoiceTargetAt { .. }
            | Command::Identity(_)
            | Command::Mls(_) => {}
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
                        let text = if self.limits.allow_html {
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
                if comment.len() > MAX_CONTENT_BYTES {
                    return emit(
                        events,
                        SessionEvent::Notice("Comment exceeds the 256 KiB client limit.".into()),
                    )
                    .await;
                }
                let comment = if self.limits.allow_html {
                    text_html(&comment)
                } else {
                    comment
                };
                if let Err(reason) = self.limits.check_text_wire(&comment) {
                    return emit(
                        events,
                        SessionEvent::Notice(format!("Comment refused: {reason}")),
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
                let server_limit = self.limits.image_message_length;
                let limit = if server_limit == 0 {
                    MAX_AVATAR_BYTES
                } else {
                    server_limit.min(MAX_AVATAR_BYTES)
                };
                if texture.len() > limit {
                    let reason = if server_limit == 0 || server_limit > MAX_AVATAR_BYTES {
                        format!(
                            "Avatar exceeds the {} KiB client limit.",
                            MAX_AVATAR_BYTES / 1024
                        )
                    } else {
                        format!("Avatar exceeds this server's {server_limit}-byte limit.")
                    };
                    return emit(events, SessionEvent::Notice(reason)).await;
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
                if !self.limits.allow_html {
                    return emit(
                        events,
                        SessionEvent::Notice("This server does not accept rich text.".into()),
                    )
                    .await;
                }
                if let Some(reason) = html.iter().find_map(|wire| {
                    if wire.is_empty() {
                        Some("Message is empty.".to_owned())
                    } else {
                        self.limits
                            .check_html_total(wire)
                            .err()
                            .map(|reason| reason.to_string())
                    }
                }) {
                    return emit(events, SessionEvent::Notice(reason)).await;
                }
                for html in html {
                    self.outbox.push(Outgoing {
                        target: target.clone(),
                        expected_peer,
                        wire: html.clone(),
                        body: html,
                        html: true,
                        chat: None,
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
            Command::SetVoiceTarget { id, targets } | Command::Whisper { id, targets, .. } => {
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
                    if let Some(revision) = whisper_revision {
                        self.admit_voice_target(revision, id);
                    }
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
            Command::QueryAcl(channel) => self.query_acl(channel, None, events).await?,
            Command::QueryAclDraft { channel, request } => {
                self.query_acl(channel, Some(request), events).await?
            }
            Command::UpdateAcl(acl) => self.queue(
                kind::ACL,
                &proto::Acl {
                    channel_id: acl.channel,
                    inherit_acls: Some(acl.inherit_acls),
                    groups: acl
                        .groups
                        .into_iter()
                        .filter(group_has_local_definition)
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
            Command::Nuschl(request) => {
                let protected = matches!(
                    request,
                    NuschlRequest::Send(_)
                        | NuschlRequest::Edit(_)
                        | NuschlRequest::Delete(_)
                        | NuschlRequest::Typing(_)
                );
                let negotiated = !(self.encrypted_server() && protected)
                    && self.extension.as_ref().is_some_and(|capabilities| {
                        nuschl::required_capability(request.frame().0)
                            .is_some_and(|needed| capabilities.iter().any(|have| have == needed))
                    });
                if !negotiated {
                    return emit(
                        events,
                        SessionEvent::Nuschl(NuschlEvent::Error(nuschl::error(
                            request.request_id(),
                            v1::error::Code::Unsupported,
                            0,
                        ))),
                    )
                    .await;
                }
                // Sends, edits and deletions share the server's text bucket.
                if matches!(
                    request,
                    NuschlRequest::Send(_) | NuschlRequest::Edit(_) | NuschlRequest::Delete(_)
                ) {
                    self.outbox.charge(Instant::now());
                }
                self.outbound.push(request.frame());
            }
            Command::QueryUserNames(ids) => self.queue_names(proto::QueryUsers {
                ids: ids.into_iter().take(1024).collect(),
                names: Vec::new(),
            }),
            Command::QueryUserIds(names) => self.queue_names(proto::QueryUsers {
                ids: Vec::new(),
                names: names
                    .into_iter()
                    .take(128)
                    .filter(|name| !name.is_empty() && name.len() <= 512)
                    .collect(),
            }),
            Command::LookupAccount {
                channel,
                request,
                name,
            } => self.lookup_account(channel, request, name, events).await?,
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
                let wire_body = legacy_body(&body, self.limits.allow_html);
                if let Err(reason) = self.limits.check_text_wire(&wire_body) {
                    return emit(events, SessionEvent::Notice(reason.to_string())).await;
                }
                let conversation = match target {
                    TextTarget::Channel(channel) if self.history() => {
                        self.conversations.get(&channel).copied()
                    }
                    // The server decides whether it is ours (right after a
                    // reconnect the list may not be back yet).
                    TextTarget::DirectConversation(id) if self.history() => Some(id),
                    _ => None,
                };
                let chat = conversation
                    .filter(|_| body.len() <= nuschl::MAX_CONTENT_BYTES)
                    .map(|conversation| {
                        (
                            PendingChat {
                                target: target.clone(),
                                conversation,
                                key: random_key(),
                                content: body.clone(),
                            },
                            false,
                        )
                    });
                self.outbox.push(Outgoing {
                    target,
                    expected_peer,
                    wire: wire_body,
                    body,
                    html: false,
                    chat,
                });
                self.flush_outbox(events).await?;
            }
            Command::MarkRead { channel, seq } => {
                if self.history()
                    && let Some(conversation) = self.conversations.get(&channel)
                {
                    self.outbound.push((
                        nuschl::kind::READ_MARKER,
                        v1::ReadMarker {
                            conversation: conversation.to_vec(),
                            seq,
                        }
                        .encode_to_vec(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Whether the Nuschl extension's history was negotiated.
    fn history(&self) -> bool {
        self.extension.as_ref().is_some_and(|capabilities| {
            capabilities
                .iter()
                .any(|c| c == nuschl::capability::HISTORY)
        })
    }

    /// Send a Nuschl conversation's message (paced like text by the outbox).
    async fn send_chat(
        &mut self,
        (pending, shown): (PendingChat, bool),
        events: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), Error> {
        if self.encrypted_server() {
            return emit(
                events,
                SessionEvent::ChatFailed {
                    key: pending.key,
                    reason: "A queued plaintext message was refused on an encrypted server.".into(),
                },
            )
            .await;
        }
        self.next_request += 1;
        let request_id = INTERNAL | self.next_request;
        self.queue(
            nuschl::kind::CHAT_SEND,
            &v1::ChatSend {
                request_id,
                conversation: pending.conversation.to_vec(),
                idempotency_key: pending.key.to_vec(),
                format: v1::Format::Text as i32,
                content: pending.content.clone(),
                reply_to: 0,
            },
        );
        let attempts = self
            .sending
            .values()
            .find(|(sent, _)| sent.key == pending.key)
            .map_or(0, |(_, attempts)| *attempts);
        self.sending.retain(|_, (sent, _)| sent.key != pending.key);
        self.sending
            .insert(request_id, (pending.clone(), attempts + 1));
        if shown {
            return Ok(());
        }
        let author = self
            .own_session
            .and_then(|own| self.users.get(&own))
            .map(|user| user.name.clone())
            .unwrap_or_default();
        emit(events, SessionEvent::ChatPending(pending.clone())).await?;
        let peer = match pending.target {
            TextTarget::DirectConversation(id) => self.direct_peer(&id),
            _ => None,
        };
        emit(
            events,
            SessionEvent::Text(IncomingText {
                target: pending.target.clone(),
                author,
                peer,
                body: pending.content,
                html: false,
                outgoing: true,
                observed_at_ms: now_ms(),
                seq: None,
                pending: Some(pending.key),
            }),
        )
        .await
    }

    /// What the session handles of the extension itself: its own sends'
    /// answers, subscriptions, and live messages (as `Text`). The rest goes
    /// to the frontend.
    async fn nuschl(
        &mut self,
        event: NuschlEvent,
        events: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), Error> {
        match event {
            NuschlEvent::Ack(ack) if ack.request_id & INTERNAL != 0 => {
                if let Some((pending, _)) = self.sending.remove(&ack.request_id) {
                    emit(
                        events,
                        SessionEvent::ChatAcked {
                            key: pending.key,
                            seq: ack.seq,
                        },
                    )
                    .await?;
                }
            }
            NuschlEvent::Error(error) if error.request_id & INTERNAL != 0 => {
                let Some((pending, attempts)) = self.sending.remove(&error.request_id) else {
                    return Ok(());
                };
                let code =
                    v1::error::Code::try_from(error.code).unwrap_or(v1::error::Code::Unspecified);
                let transient =
                    matches!(code, v1::error::Code::Busy | v1::error::Code::RateLimited);
                if transient && attempts < MAX_SEND_ATTEMPTS {
                    // The outbox paces the retry; the key makes it idempotent.
                    self.sending
                        .insert(error.request_id, (pending.clone(), attempts));
                    self.outbox.charge(Instant::now());
                    self.outbox.push(Outgoing {
                        target: pending.target.clone(),
                        expected_peer: None,
                        wire: String::new(),
                        body: String::new(),
                        html: false,
                        chat: Some((pending, true)),
                    });
                } else {
                    emit(
                        events,
                        SessionEvent::ChatFailed {
                            key: pending.key,
                            reason: error.detail,
                        },
                    )
                    .await?;
                }
            }
            NuschlEvent::Subscribed(subscribed) if subscribed.request_id & INTERNAL != 0 => {
                for state in subscribed.conversations {
                    if let Some(channel) = self.channel_of(&state.conversation) {
                        emit(
                            events,
                            SessionEvent::ConversationState {
                                channel,
                                head: state.head,
                                read: state.read,
                            },
                        )
                        .await?;
                    }
                }
            }
            NuschlEvent::Chat(chat) => {
                if self
                    .extension
                    .as_ref()
                    .is_some_and(|caps| caps.iter().any(|cap| cap == nuschl::capability::E2EE))
                {
                    emit(
                        events,
                        SessionEvent::Notice(
                            "Unencrypted chat on an end-to-end server was refused.".into(),
                        ),
                    )
                    .await?;
                    return Ok(());
                }
                // Frontends with a timeline use the `Text` below; the event
                // itself also carries edits and deletions.
                emit(
                    events,
                    SessionEvent::Nuschl(NuschlEvent::Chat(chat.clone())),
                )
                .await?;
                let Some(message) = chat.message else {
                    return Ok(());
                };
                // MLS rows are ciphertext: the end-to-end driver reads them (and
                // without a crypto worker nothing can).
                if message.kind != v1::message::Kind::Message as i32
                    || nuschl::is_mls_format(message.format)
                {
                    return Ok(());
                }
                let target = match self.channel_of(&chat.conversation) {
                    Some(channel) => TextTarget::Channel(channel),
                    None => {
                        let Some(id) = nuschl::id(&chat.conversation) else {
                            return Ok(());
                        };
                        // Not a channel's: one of our direct conversations,
                        // perhaps new (the server delivers those only to
                        // its two accounts). Ask for the list to be sure.
                        if !self.directs.contains_key(&id) {
                            let author = message.author.clone().unwrap_or_default();
                            let own = self
                                .own_session
                                .and_then(|own| self.users.get(&own))
                                .and_then(|user| user.user_id);
                            if own.is_some_and(|own| own != author.user_id)
                                && self.directs.len() < nuschl::MAX_DIRECTS
                            {
                                self.directs.insert(
                                    id,
                                    v1::Direct {
                                        conversation: id.to_vec(),
                                        user_id: author.user_id,
                                        name: author.name,
                                        account: author.account,
                                        ..Default::default()
                                    },
                                );
                            }
                            self.next_request += 1;
                            self.outbound.push((
                                nuschl::kind::LIST_DIRECT,
                                v1::ListDirect {
                                    request_id: INTERNAL | self.next_request,
                                }
                                .encode_to_vec(),
                            ));
                        }
                        TextTarget::DirectConversation(id)
                    }
                };
                let author = message.author.unwrap_or_default();
                let own = self
                    .own_session
                    .and_then(|own| self.users.get(&own))
                    .and_then(|user| user.user_id);
                // From our own account on another device.
                let outgoing = author.user_id != 0 && own == Some(author.user_id);
                let peer = match target {
                    TextTarget::DirectConversation(id) if outgoing => self.direct_peer(&id),
                    _ => self
                        .users
                        .values()
                        .find(|user| author.user_id != 0 && user.user_id == Some(author.user_id))
                        .cloned(),
                };
                emit(
                    events,
                    SessionEvent::Text(IncomingText {
                        target,
                        author: if author.name.is_empty() {
                            "Deleted user".into()
                        } else {
                            author.name
                        },
                        peer,
                        body: message.content,
                        html: message.format == v1::Format::LegacyHtml as i32,
                        outgoing,
                        observed_at_ms: message.sent_ms,
                        seq: Some(message.seq),
                        pending: None,
                    }),
                )
                .await?;
            }
            NuschlEvent::DirectList(list) => {
                for direct in &list.directs {
                    if let Some(id) = nuschl::id(&direct.conversation) {
                        self.directs.insert(id, direct.clone());
                    }
                }
                emit(events, SessionEvent::Nuschl(NuschlEvent::DirectList(list))).await?;
            }
            NuschlEvent::DirectOpened(opened) => {
                if let Some(direct) = &opened.direct
                    && let Some(id) = nuschl::id(&direct.conversation)
                    && (self.directs.len() < nuschl::MAX_DIRECTS || self.directs.contains_key(&id))
                {
                    self.directs.insert(id, direct.clone());
                }
                emit(
                    events,
                    SessionEvent::Nuschl(NuschlEvent::DirectOpened(opened)),
                )
                .await?;
            }
            other => emit(events, SessionEvent::Nuschl(other)).await?,
        }
        Ok(())
    }

    /// The other account of a direct conversation, if it is online.
    fn direct_peer(&self, id: &[u8; 16]) -> Option<User> {
        let user_id = self.directs.get(id)?.user_id;
        self.users
            .values()
            .find(|user| user_id != 0 && user.user_id == Some(user_id))
            .cloned()
    }

    fn channel_of(&self, conversation: &[u8]) -> Option<u32> {
        self.conversations
            .iter()
            .find(|(_, id)| id.as_slice() == conversation)
            .map(|(channel, _)| *channel)
    }

    /// Subscribe to channels' conversations (all channels the sidebar shows,
    /// up to the server's limit), so their messages arrive anywhere.
    fn subscribe(&mut self, channels: Vec<u32>) {
        if !self.extension.as_ref().is_some_and(|capabilities| {
            capabilities
                .iter()
                .any(|c| c == nuschl::capability::SUBSCRIBE)
        }) {
            return;
        }
        let ids: Vec<Vec<u8>> = channels
            .into_iter()
            .filter(|channel| {
                self.subscribed.len() < nuschl::MAX_SUBSCRIPTIONS
                    && self.subscribed.insert(*channel)
            })
            .filter_map(|channel| self.conversations.get(&channel))
            .map(|id| id.to_vec())
            .collect();
        for chunk in ids.chunks(nuschl::MAX_SUBSCRIBE_PER_MESSAGE) {
            self.next_request += 1;
            self.outbound.push((
                nuschl::kind::SUBSCRIBE,
                v1::Subscribe {
                    request_id: INTERNAL | self.next_request,
                    conversations: chunk.to_vec(),
                }
                .encode_to_vec(),
            ));
        }
    }

    /// Send the text the rate limit has room for, and report on what waits.
    async fn flush_outbox(&mut self, events: &mpsc::Sender<SessionEvent>) -> Result<(), Error> {
        while let Some(mut message) = self.outbox.pop(Instant::now()) {
            match message.chat.take() {
                Some(chat) => self.send_chat(chat, events).await?,
                None => self.send_text(message, events).await?,
            }
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
            chat: _,
        }: Outgoing,
        events: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), Error> {
        if self.encrypted_server() {
            return emit(
                events,
                SessionEvent::Notice(
                    "A queued plaintext message was refused on an encrypted server.".into(),
                ),
            )
            .await;
        }
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
                    return emit(
                        events,
                        SessionEvent::Notice(
                            nuschl_core::model::RECIPIENT_UNAVAILABLE_NOTICE.into(),
                        ),
                    )
                    .await;
                }
                message.session.push(id);
            }
            // What the extension does not carry (images, more than 64 KiB)
            // goes the stock way, to the other account's session while it
            // is online; our timeline shows it in the conversation.
            TextTarget::DirectConversation(id) => match self.direct_peer(&id) {
                Some(peer) => message.session.push(peer.session),
                None => {
                    return emit(
                        events,
                        SessionEvent::Notice(
                            "Images and messages over 64 KiB reach someone in a direct \
                             conversation only while they are online."
                                .into(),
                        ),
                    )
                    .await;
                }
            },
        }
        self.queue(kind::TEXT_MESSAGE, &message);
        let author = self
            .users
            .get(&own)
            .map(|user| user.name.clone())
            .unwrap_or_default();
        let peer = match target {
            TextTarget::Direct(id) => self.users.get(&id).cloned(),
            TextTarget::DirectConversation(id) => self.direct_peer(&id),
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
                seq: None,
                pending: None,
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
                Ok(UdpMessage::Audio(packet))
                    if !self.local_deafened && !self.encrypted_server() =>
                {
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
                if self.account_lookup.as_ref().is_some_and(|lookup| {
                    !lookup.waiting_reply && ping.timestamp == Some(lookup.barrier)
                }) {
                    let valid = self
                        .account_lookup
                        .as_ref()
                        .is_some_and(|lookup| self.lookup_valid(lookup));
                    if valid {
                        let lookup = self.account_lookup.as_mut().unwrap();
                        lookup.waiting_reply = true;
                        let name = lookup.name.clone();
                        self.queue(
                            kind::QUERY_USERS,
                            &proto::QueryUsers {
                                ids: Vec::new(),
                                names: vec![name],
                            },
                        );
                    } else {
                        let lookup = self.account_lookup.take().unwrap();
                        emit(events,SessionEvent::AccountReply(AccountReply {request:lookup.request,result:Err("The original connection or channel changed during account lookup.".into())})).await?;
                        self.release_name_queries();
                    }
                    return Ok(());
                }
                let now = now_ms() as u64;
                if let Some(timestamp) = ping.timestamp.filter(|sent| *sent <= now) {
                    self.tcp_ping.record((now - timestamp) as f32);
                }
            }
            kind::REJECT => {
                return Err(rejection_error(&frame)?);
            }
            kind::SERVER_SYNC => {
                let sync: proto::ServerSync = decode(&frame)?;
                let session = sync
                    .session
                    .ok_or_else(|| Error::Protocol("ServerSync omitted session".into()))?;
                self.own_session = Some(session);
                self.limits.max_bandwidth = Some(
                    sync.max_bandwidth
                        .unwrap_or(nuschl_core::audio::DEFAULT_SERVER_BANDWIDTH),
                );
                if self.voice_target == 31 {
                    emit(events, SessionEvent::Notice("The previous whisper is suspended after reconnect. Select a new target or normal talking before speaking.".into())).await?;
                }
                emit(
                    events,
                    SessionEvent::Connected {
                        session,
                        max_bandwidth: self.limits.bandwidth(),
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
                    self.welcome(welcome, events).await?;
                }
                emit(events, SessionEvent::Limits(self.limits.clone())).await?;
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
                if self.history() {
                    let channels = self.conversations.keys().copied().collect();
                    self.subscribe(channels);
                    // What an earlier connection could not get acknowledged,
                    // with the same keys: the server stores each once.
                    // Our direct conversations (unread counts, and where
                    // messages from others belong).
                    self.next_request += 1;
                    self.outbound.push((
                        nuschl::kind::LIST_DIRECT,
                        v1::ListDirect {
                            request_id: INTERNAL | self.next_request,
                        }
                        .encode_to_vec(),
                    ));
                    for pending in std::mem::take(&mut self.resend) {
                        // A direct conversation outlives connections; the
                        // server refuses it if it went.
                        let known = match pending.target {
                            TextTarget::Channel(channel) => {
                                self.conversations.get(&channel) == Some(&pending.conversation)
                            }
                            TextTarget::DirectConversation(id) => id == pending.conversation,
                            TextTarget::Direct(_) => false,
                        };
                        if known {
                            if let TextTarget::DirectConversation(id) = pending.target {
                                self.directs.entry(id).or_insert_with(|| v1::Direct {
                                    conversation: id.to_vec(),
                                    ..Default::default()
                                });
                            }
                            self.outbox.push(Outgoing {
                                target: pending.target.clone(),
                                expected_peer: None,
                                wire: String::new(),
                                body: String::new(),
                                html: false,
                                chat: Some((pending, true)),
                            });
                        } else {
                            emit(
                                events,
                                SessionEvent::ChatFailed {
                                    key: pending.key,
                                    reason: "The conversation is no longer available.".into(),
                                },
                            )
                            .await?;
                        }
                    }
                    self.flush_outbox(events).await?;
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
                    instance: next_user_instance(),
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
                if self.extension.is_some()
                    && let Some(info) = nuschl::channel_info(&frame.payload)
                {
                    if let Some(conversation) = nuschl::id(&info.conversation) {
                        self.conversations.insert(id, conversation);
                        if self.logged_in() {
                            self.subscribe(vec![id]);
                        }
                    }
                    emit(events, SessionEvent::ChannelInfo { channel: id, info }).await?;
                }
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
                if self
                    .extension
                    .as_ref()
                    .is_some_and(|caps| caps.iter().any(|cap| cap == nuschl::capability::E2EE))
                {
                    emit(
                        events,
                        SessionEvent::Notice(
                            "Unencrypted chat on an end-to-end server was refused.".into(),
                        ),
                    )
                    .await?;
                    return Ok(());
                }
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
                            html: self.limits.allow_html,
                            outgoing: false,
                            observed_at_ms: now_ms(),
                            seq: None,
                            pending: None,
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
                emit(events, SessionEvent::OpusAvailable(self.opus)).await?;
                if !self.opus {
                    emit(events, SessionEvent::Notice(
                        "This server uses the legacy CELT codec. Nuschl supports Opus only, so voice is unavailable here; text chat still works.".into(),
                    ))
                    .await?;
                }
            }
            kind::SERVER_CONFIG => {
                let config: proto::ServerConfig = decode(&frame)?;
                if let Some(source) = config.welcome_text {
                    self.welcome(source, events).await?;
                }
                let supplied = self.limits.apply(nuschl_core::model::LimitsUpdate {
                    allow_html: config.allow_html,
                    message_length: config.message_length,
                    image_message_length: config.image_message_length,
                    max_users: config.max_users,
                    recording_allowed: config.recording_allowed,
                    max_bandwidth: config.max_bandwidth,
                });
                if supplied {
                    emit(events, SessionEvent::Limits(self.limits.clone())).await?;
                }
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
                let pending = self.acl_queries.remove(&acl.channel_id);
                let complete = acl.groups.len() <= 1024
                    && acl.acls.len() <= 4096
                    && acl.groups.iter().all(|group| {
                        group.name.len() <= 256
                            && group
                                .add
                                .len()
                                .saturating_add(group.remove.len())
                                .saturating_add(group.inherited_members.len())
                                <= 8192
                    })
                    && acl
                        .acls
                        .iter()
                        .all(|entry| entry.group.as_ref().is_none_or(|group| group.len() <= 256));
                let current_instance = self
                    .channels
                    .get(&acl.channel_id)
                    .map(|channel| channel.instance);
                let value = Acl {
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
                };
                match pending {
                    Some((instance, Some(request))) => {
                        let result = if current_instance != Some(instance) {
                            Err("The original channel no longer exists. Reopen its editor.".into())
                        } else if !complete {
                            Err("This ACL exceeds the safe editor limits. It was not loaded or made editable.".into())
                        } else {
                            Ok(value)
                        };
                        emit(events, SessionEvent::AclReply(AclReply { request, result })).await?;
                    }
                    Some((instance, None)) if current_instance != Some(instance) => {}
                    _ => emit(events, SessionEvent::Acl(value)).await?,
                }
            }
            kind::QUERY_USERS => {
                let query: proto::QueryUsers = decode(&frame)?;
                if self
                    .account_lookup
                    .as_ref()
                    .is_some_and(|lookup| lookup.waiting_reply)
                {
                    let lookup = self.account_lookup.take().unwrap();
                    let result = if !self.lookup_valid(&lookup) {
                        Err(
                            "The original connection or channel changed during account lookup."
                                .into(),
                        )
                    } else {
                        resolved_account(&lookup.name, &query).map_err(str::to_owned)
                    };
                    emit(
                        events,
                        SessionEvent::AccountReply(AccountReply {
                            request: lookup.request,
                            result,
                        }),
                    )
                    .await?;
                    self.release_name_queries();
                }
                emit(
                    events,
                    SessionEvent::UserNames(
                        query
                            .ids
                            .into_iter()
                            .zip(query.names)
                            .filter(|(id, _)| *id != u32::MAX)
                            .take(4096)
                            .map(|(id, name)| (id, bounded(name, 512)))
                            .collect(),
                    ),
                )
                .await?;
            }
            kind if nuschl_protocol::kind::NUSCHL_EXTENSION.contains(&kind) => {
                // Ignored unless ALPN selected `nuschl/1` (docs/extension.md §1).
                if self.extension.is_some()
                    && let Some(event) = nuschl_event(&frame)?
                {
                    self.nuschl(event, events).await?;
                }
            }
            kind::PLUGIN_DATA_TRANSMISSION => self.plugin_data(&frame, events),
            _ => { /* Optional messages are framed and ignored. */ }
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
            // A new transport may reuse both Mumble's session number and the
            // order in which users arrive. Old queued private sends must still
            // fail their presence check after that reconnect.
            instance: next_user_instance(),
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
        // A UserState is sparse. Both Murmur and Nuschl can announce
        // deafness without a separate mute bit; deafness always implies mute.
        user.muted |= user.deafened;
        user.self_muted |= user.self_deafened;
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
            if user
                .certificate_hash
                .as_deref()
                .filter(|old| !old.is_empty())
                .is_some_and(|old| !old.eq_ignore_ascii_case(&value))
            {
                user.instance = next_user_instance();
            }
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
/// Settle the extension from the server's `Version`: on if and only if ALPN
/// selected `nuschl/1` and the server sent a hello (ours went out with our
/// `Version`). Without ALPN the field is ignored; ALPN without a valid hello is
/// a protocol error, never plain Mumble.
fn negotiate(nuschl: bool, version: &[u8]) -> Result<Negotiated, Error> {
    if !nuschl {
        return Ok(Negotiated {
            nuschl: false,
            capabilities: Vec::new(),
            policy: None,
            kt_log: Vec::new(),
        });
    }
    let hello = nuschl::hello(version)
        .map_err(|error| Error::Protocol(error.to_string()))?
        .ok_or_else(|| {
            Error::Protocol("the server selected the Nuschl protocol but sent no hello".into())
        })?;
    let capabilities: Vec<String> = nuschl::intersect(CLIENT_CAPABILITIES, &hello.capabilities)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let kt = capabilities.iter().any(|c| c == nuschl::capability::KT);
    // Key transparency without a log to pin fails closed, like a hello without ALPN.
    if kt && hello.kt_log.is_empty() {
        return Err(Error::Protocol(
            "the server offers key transparency without naming its log".into(),
        ));
    }
    // MLS credentials are checked against key transparency: never one without it.
    let mls = capabilities.iter().any(|c| c == nuschl::capability::MLS);
    if mls && !kt {
        return Err(Error::Protocol(
            "the server offers end-to-end encryption without key transparency".into(),
        ));
    }
    // An end-to-end server without MLS could carry nothing: fail closed.
    if capabilities.iter().any(|c| c == nuschl::capability::E2EE) && !mls {
        return Err(Error::Protocol(
            "the server says it encrypts end to end but offers no MLS".into(),
        ));
    }
    Ok(Negotiated {
        nuschl: true,
        capabilities,
        policy: hello.policy,
        kt_log: if kt { hello.kt_log } else { Vec::new() },
    })
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
/// The next frame. Plugin data larger than any message accepted from the
/// server ([`plugin::max_body`] for [`MAX_RECEIVED_RECEIVERS`]) is read past
/// through a fixed buffer and counted in `skipped`: a stock server forwards
/// the unknown fields a sender adds, so one peer must not be able to end our
/// connection with them.
async fn read_control(
    reader: &mut (impl AsyncRead + Unpin),
    skipped: &AtomicU64,
) -> Result<ControlFrame, Error> {
    loop {
        let mut header = [0; 6];
        timeout(Duration::from_secs(30), reader.read_exact(&mut header))
            .await
            .map_err(|_| Error::Timeout("waiting for server traffic"))??;
        let kind = u16::from_be_bytes([header[0], header[1]]);
        let size = u32::from_be_bytes([header[2], header[3], header[4], header[5]]) as usize;
        if size > MAX_CONTROL_BYTES {
            return Err(Error::Protocol("oversized control frame".into()));
        }
        if kind == kind::PLUGIN_DATA_TRANSMISSION && size > plugin::max_body(MAX_RECEIVED_RECEIVERS)
        {
            timeout(Duration::from_secs(10), async {
                let mut scratch = [0_u8; 4096];
                let mut remaining = size;
                while remaining > 0 {
                    let wanted = remaining.min(scratch.len());
                    reader.read_exact(&mut scratch[..wanted]).await?;
                    remaining -= wanted;
                }
                Ok::<_, std::io::Error>(())
            })
            .await
            .map_err(|_| Error::Timeout("reading control frame"))??;
            skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let mut payload = vec![0; size];
        timeout(Duration::from_secs(10), reader.read_exact(&mut payload))
            .await
            .map_err(|_| Error::Timeout("reading control frame"))??;
        return Ok(ControlFrame {
            kind,
            payload: Bytes::from(payload),
        });
    }
}

/// For the `client_messages` libFuzzer target in `fuzz/`: the session's state
/// fed control frames as a hostile server would send them, with the extension
/// negotiated. No network and no clock beyond the session's own: the runtime
/// only awaits the event channel, which is drained after every frame.
#[doc(hidden)]
pub mod fuzzing {
    use super::*;

    thread_local! {
        static RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime");
    }

    /// Frames as a type byte (Mumble's from 0 to 31, the extension's from 32
    /// on), a two-byte big-endian length, and the body. A frame the session
    /// rejects counts as never sent, so later ones still arrive.
    pub fn run(mut data: &[u8]) {
        let mut state = State {
            extension: Some(nuschl::capability::ALL.map(str::to_owned).to_vec()),
            ..State::default()
        };
        let (events, mut received) = mpsc::channel(1 << 16);
        RUNTIME.with(|runtime| {
            runtime.block_on(async {
                while let [kind, high, low, rest @ ..] = data {
                    let kind = match *kind {
                        kind @ 0..=31 => u16::from(kind),
                        other => nuschl::kind::ERROR | u16::from((other - 32) & 0x1f),
                    };
                    let length = usize::from(u16::from_be_bytes([*high, *low])).min(rest.len());
                    let payload = Bytes::copy_from_slice(&rest[..length]);
                    data = &rest[length..];
                    let _ = state.handle(ControlFrame { kind, payload }, &events).await;
                    state.outbound.clear();
                    while received.try_recv().is_ok() {}
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn text_send_policy_counts_utf16_and_html_total_before_any_wire_or_echo() {
        for (html, text_limit, image_limit, body, accepted) in [
            (true, 1, 0, "漢".to_owned(), true),
            (true, 1, 0, "🙂".to_owned(), false),
            (true, 2, 0, "🙂".to_owned(), true),
            (false, 1, 0, "é".to_owned(), true),
            (true, 17, 0, "<&>\n".to_owned(), false),
            (true, 18, 0, "<&>\n".to_owned(), true),
            (true, 0, 128, "x".repeat(129), false),
            (true, 0, 128, "x".repeat(128), true),
            (true, 0, 0, "&".repeat(60_000), false),
        ] {
            let mut state = State {
                own_session: Some(7),
                ..Default::default()
            };
            state.limits.apply(nuschl_core::model::LimitsUpdate {
                allow_html: Some(html),
                message_length: Some(text_limit),
                image_message_length: Some(image_limit),
                ..Default::default()
            });
            state.channels.insert(
                0,
                Channel {
                    id: 0,
                    ..Default::default()
                },
            );
            let (tx, mut rx) = mpsc::channel(64);
            state
                .command(
                    Command::SendText {
                        target: TextTarget::Channel(0),
                        body: body.clone(),
                        expected_peer: None,
                    },
                    &tx,
                )
                .await
                .unwrap();
            let events = drain(&mut rx).await;
            assert_eq!(
                state
                    .outbound
                    .iter()
                    .any(|(kind, _)| *kind == kind::TEXT_MESSAGE),
                accepted,
                "{html}/{text_limit}/{image_limit}"
            );
            assert_eq!(
                events
                    .iter()
                    .any(|event| matches!(event,SessionEvent::Text(text) if text.outgoing)),
                accepted
            );
            if accepted {
                let (_, bytes) = state
                    .outbound
                    .iter()
                    .find(|(kind, _)| *kind == kind::TEXT_MESSAGE)
                    .unwrap();
                assert_eq!(
                    proto::TextMessage::decode(bytes.as_slice())
                        .unwrap()
                        .message,
                    legacy_body(&body, html)
                );
            } else {
                assert!(
                    events
                        .iter()
                        .any(|event| matches!(event, SessionEvent::Notice(_)))
                );
            }
        }
    }

    #[tokio::test]
    async fn rich_html_total_cap_is_not_bypassed_by_a_larger_text_cap() {
        let mut state = State {
            own_session: Some(7),
            ..Default::default()
        };
        state.channels.insert(
            0,
            Channel {
                id: 0,
                ..Default::default()
            },
        );
        state.limits.image_message_length = 128;
        let (tx, mut rx) = mpsc::channel(64);
        let send = |body: String| Command::SendHtml {
            target: TextTarget::Channel(0),
            html: vec![body],
            expected_peer: None,
        };
        state.command(send("x".repeat(129)), &tx).await.unwrap();
        assert!(state.outbound.is_empty());
        assert!(
            matches!(rx.try_recv(),Ok(SessionEvent::Notice(reason)) if reason.contains("128-character total HTML limit"))
        );
        state.command(send("x".repeat(128)), &tx).await.unwrap();
        assert_eq!(state.outbound.len(), 1);
    }

    #[tokio::test]
    async fn sync_publishes_initial_limits_without_inventing_negotiated_codec_support() {
        for bandwidth in [None, Some(0), Some(24_000)] {
            let mut state = State::default();
            let (tx, mut rx) = mpsc::channel(16);
            state
                .handle(
                    frame(
                        kind::SERVER_SYNC,
                        proto::ServerSync {
                            session: Some(1),
                            max_bandwidth: bandwidth,
                            ..Default::default()
                        },
                    ),
                    &tx,
                )
                .await
                .unwrap();
            let events = drain(&mut rx).await;
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, SessionEvent::OpusAvailable(_))),
                "private routing default is not negotiated knowledge"
            );
            let limits = events
                .iter()
                .find_map(|event| match event {
                    SessionEvent::Limits(limits) => Some(limits),
                    _ => None,
                })
                .expect("Sync without ServerConfig must publish current limits");
            assert_eq!(limits.max_bandwidth, Some(bandwidth.unwrap_or(72_000)));
            assert_eq!(limits.message_length, 5000);
            for opus in [Some(false), Some(true), None] {
                state
                    .handle(
                        frame(
                            kind::CODEC_VERSION,
                            proto::CodecVersion {
                                opus,
                                ..Default::default()
                            },
                        ),
                        &tx,
                    )
                    .await
                    .unwrap();
                assert!(drain(&mut rx).await.iter().any(|event| matches!(event,
                    SessionEvent::OpusAvailable(value) if *value == opus.unwrap_or(false))));
            }
        }
    }

    fn broken_pipe() -> Error {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "initial write failed").into()
    }

    fn assert_broken_pipe(error: Error) {
        assert!(matches!(error, Error::Io(ref e) if e.kind() == std::io::ErrorKind::BrokenPipe));
    }

    #[tokio::test]
    async fn buffered_reject_survives_a_real_initial_write_failure() {
        // An established transport closes after writing Reject, before our
        // next write. Its reader has not run yet: this exercises read_control
        // plus the error drain, not an invented password error from EOF.
        for _ in 0..32 {
            let (local, mut server) = tokio::io::duplex(4096);
            let reject = proto::Reject {
                r#type: Some(4),
                reason: Some("private fixture wrong server password".into()),
            };
            send(&mut server, kind::REJECT, &reject).await.unwrap();
            drop(server);
            let (mut reader, mut writer) = tokio::io::split(local);
            let original = write_control(&mut writer, kind::USER_STATE, &[])
                .await
                .unwrap_err();
            assert!(
                matches!(original, Error::Io(ref e) if e.kind() == std::io::ErrorKind::BrokenPipe)
            );
            let (tx, mut rx) = mpsc::channel(32);
            let task = tokio::spawn(async move {
                tx.send(read_control(&mut reader, &AtomicU64::new(0)).await)
                    .await
                    .unwrap();
            });
            assert!(matches!(
                initial_login_error(original, false, &mut rx).await,
                Error::Rejected {
                    kind: RejectKind::WrongServerPassword,
                    ..
                }
            ));
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn login_error_never_invents_a_reject_from_eof_malformed_or_sync() {
        for first in [
            Err(Error::Protocol("peer closed".into())),
            Ok(ControlFrame {
                kind: kind::REJECT,
                payload: Bytes::from_static(&[0xff]),
            }),
            Ok(frame(kind::SERVER_SYNC, proto::ServerSync::default())),
        ] {
            let (tx, mut rx) = mpsc::channel(4);
            tx.send(first).await.unwrap();
            tx.send(Ok(frame(
                kind::REJECT,
                proto::Reject {
                    r#type: Some(4),
                    ..Default::default()
                },
            )))
            .await
            .unwrap();
            assert_broken_pipe(initial_login_error(broken_pipe(), false, &mut rx).await);
            assert_eq!(
                rx.len(),
                1,
                "must stop at the first terminal/boundary frame"
            );
        }
        let (tx, mut rx) = mpsc::channel(1);
        drop(tx);
        assert_broken_pipe(initial_login_error(broken_pipe(), false, &mut rx).await);
    }

    #[tokio::test(start_paused = true)]
    async fn login_error_budget_is_absolute_and_post_login_has_no_wait() {
        let (tx, mut rx) = mpsc::channel(64);
        let start = Instant::now();
        assert_broken_pipe(initial_login_error(broken_pipe(), true, &mut rx).await);
        assert_eq!(Instant::now(), start);
        assert!(matches!(
            initial_login_error(Error::Protocol("bad frame".into()), false, &mut rx).await,
            Error::Protocol(_)
        ));
        assert_eq!(Instant::now(), start);
        let sender = tokio::spawn(async move {
            for _ in 0..20 {
                time::sleep(Duration::from_millis(10)).await;
                if tx
                    .send(Ok(frame(kind::VERSION, proto::Version::default())))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        assert_broken_pipe(initial_login_error(broken_pipe(), false, &mut rx).await);
        assert_eq!(start.elapsed(), Duration::from_millis(100));
        sender.abort();
    }

    #[tokio::test]
    async fn login_error_frame_cap_and_rejection_bounds_match_normal_handling() {
        let (tx, mut rx) = mpsc::channel(64);
        for _ in 0..32 {
            tx.send(Ok(frame(kind::VERSION, proto::Version::default())))
                .await
                .unwrap();
        }
        let reject = frame(
            kind::REJECT,
            proto::Reject {
                r#type: Some(3),
                reason: Some("é".repeat(1000)),
            },
        );
        tx.send(Ok(reject.clone())).await.unwrap();
        assert_broken_pipe(initial_login_error(broken_pipe(), false, &mut rx).await);
        assert_eq!(rx.len(), 1);
        let expected = rejection_error(&reject).unwrap();
        let error = initial_login_error(broken_pipe(), false, &mut rx).await;
        assert_eq!(error.to_string(), expected.to_string());
        assert!(
            matches!(error, Error::Rejected { kind: RejectKind::WrongUserPassword, ref reason } if reason.len() == 1024)
        );
    }

    #[test]
    fn explicit_registered_lookup_refuses_partial_bulk_unrelated_and_unknown_pairs() {
        let pair = |ids, names: Vec<&str>| proto::QueryUsers {
            ids,
            names: names.into_iter().map(str::to_owned).collect(),
        };
        assert_eq!(
            resolved_account("Alice", &pair(vec![99], vec!["ALICE"]))
                .unwrap()
                .id,
            99
        );
        for query in [
            pair(vec![1, 99], vec!["Alice"]),
            pair(vec![1, 99], vec!["Alice", "Bob"]),
            pair(vec![u32::MAX], vec!["Alice"]),
            pair(vec![99], vec!["Different account"]),
            pair(vec![], vec![]),
        ] {
            assert!(
                resolved_account("Alice", &query).is_err(),
                "must not bind {query:?}"
            );
        }
        assert!(
            resolved_account(
                &"x".repeat(513),
                &proto::QueryUsers {
                    ids: vec![99],
                    names: vec!["x".repeat(513)]
                }
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn registered_lookup_drains_old_names_preserves_ping_stats_and_serializes_queries() {
        let mut state = State {
            own_session: Some(7),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::channel(64);
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
                        session: Some(7),
                        name: Some("Operator".into()),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        drain(&mut rx).await;
        state.tcp_ping.record(12.5);
        state
            .lookup_account(0, 10, "Alice".into(), &tx)
            .await
            .unwrap();
        let lookup = state.account_lookup.as_ref().unwrap();
        let barrier = lookup.barrier;
        assert_ne!(barrier & (1 << 63), 0);
        let (_, body) = state.outbound.last().unwrap();
        let ping = proto::Ping::decode(body.as_slice()).unwrap();
        assert_eq!(
            ping,
            state.ping_message(barrier),
            "barrier must carry every transport counter"
        );
        assert_eq!(ping.tcp_packets, Some(1));
        for id in 0..32 {
            state
                .command(Command::QueryUserNames(vec![id]), &tx)
                .await
                .unwrap();
        }
        assert_eq!(state.deferred_name_queries.len(), 16);
        assert_eq!(
            state.outbound.len(),
            1,
            "automatic name queries cannot overtake the barrier"
        );
        state.query_acl(0, Some(20), &tx).await.unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionEvent::AclReply(AclReply {
                request: 20,
                result: Err(_)
            })
        ));
        state
            .handle(
                frame(
                    kind::QUERY_USERS,
                    proto::QueryUsers {
                        ids: vec![1],
                        names: vec!["Alice".into()],
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        assert!(
            matches!(rx.try_recv().unwrap(), SessionEvent::UserNames(_)),
            "old cached names are labels only"
        );
        let packets = state.tcp_ping.packets;
        state
            .handle(
                frame(
                    kind::PING,
                    proto::Ping {
                        timestamp: Some(barrier),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        assert_eq!(
            state.tcp_ping.packets, packets,
            "barrier must not pollute RTT"
        );
        let (kind, body) = state.outbound.last().unwrap();
        assert_eq!(*kind, kind::QUERY_USERS);
        assert_eq!(
            proto::QueryUsers::decode(body.as_slice()).unwrap().names,
            vec!["Alice"]
        );
        state
            .lookup_account(0, 11, "Alice".into(), &tx)
            .await
            .unwrap();
        assert!(
            matches!(
                rx.try_recv().unwrap(),
                SessionEvent::AccountReply(AccountReply {
                    request: 11,
                    result: Err(_)
                })
            ),
            "unresolved lane is never reassigned"
        );
        state
            .handle(
                frame(
                    kind::QUERY_USERS,
                    proto::QueryUsers {
                        ids: vec![99],
                        names: vec!["ALICE".into()],
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        match rx.try_recv().unwrap() {
            SessionEvent::AccountReply(AccountReply {
                request: 10,
                result: Ok(account),
            }) => assert_eq!(account.id, 99),
            other => panic!("unexpected {other:?}"),
        }
        assert!(state.account_lookup.is_none());
        assert!(state.deferred_name_queries.is_empty());
        assert_eq!(
            state
                .outbound
                .iter()
                .filter(|(kind, _)| *kind == kind::QUERY_USERS)
                .count(),
            17
        );
    }

    #[tokio::test]
    async fn registered_lookup_retains_expired_reservation_and_rejects_channel_reuse() {
        let mut state = State {
            own_session: Some(7),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::channel(32);
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
                        session: Some(7),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        drain(&mut rx).await;
        state
            .lookup_account(0, 1, "Offline Alice".into(), &tx)
            .await
            .unwrap();
        let barrier = state.account_lookup.as_ref().unwrap().barrier;
        // UI timeout/close cannot cancel this untagged legacy transaction.
        state
            .lookup_account(0, 2, "Offline Bob".into(), &tx)
            .await
            .unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionEvent::AccountReply(AccountReply {
                request: 2,
                result: Err(_)
            })
        ));
        state.channels.get_mut(&0).unwrap().instance += 1;
        state
            .handle(
                frame(
                    kind::PING,
                    proto::Ping {
                        timestamp: Some(barrier),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionEvent::AccountReply(AccountReply {
                request: 1,
                result: Err(_)
            })
        ));
        assert!(state.account_lookup.is_none());
        assert!(
            state
                .outbound
                .iter()
                .all(|(kind, _)| *kind != kind::QUERY_USERS)
        );
    }

    #[tokio::test]
    async fn acl_query_reservations_prevent_reopen_timeout_and_channel_reuse_retargeting() {
        let mut state = State::default();
        let (tx, mut rx) = mpsc::channel(64);
        state
            .handle(
                frame(
                    kind::CHANNEL_STATE,
                    proto::ChannelState {
                        channel_id: Some(7),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        drain(&mut rx).await;
        let original = state.channels[&7].instance;
        state.query_acl(7, Some(100), &tx).await.unwrap();
        assert_eq!(state.outbound.len(), 1);
        // A closed/expired UI request does not cancel this reservation. Reopen
        // and retry must not consume its eventual reply under a newer ticket.
        state.query_acl(7, Some(101), &tx).await.unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionEvent::AclReply(AclReply {
                request: 101,
                result: Err(_)
            })
        ));
        assert_eq!(state.outbound.len(), 1);
        state
            .handle(
                frame(kind::CHANNEL_REMOVE, proto::ChannelRemove { channel_id: 7 }),
                &tx,
            )
            .await
            .unwrap();
        state
            .handle(
                frame(
                    kind::CHANNEL_STATE,
                    proto::ChannelState {
                        channel_id: Some(7),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        assert_ne!(state.channels[&7].instance, original);
        drain(&mut rx).await;
        state.query_acl(7, Some(102), &tx).await.unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionEvent::AclReply(AclReply {
                request: 102,
                result: Err(_)
            })
        ));
        state
            .handle(
                frame(
                    kind::ACL,
                    proto::Acl {
                        channel_id: 7,
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionEvent::AclReply(AclReply {
                request: 100,
                result: Err(_)
            })
        ));
        assert!(state.acl_queries.is_empty());
        state.query_acl(7, Some(103), &tx).await.unwrap();
        state
            .handle(
                frame(
                    kind::ACL,
                    proto::Acl {
                        channel_id: 7,
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionEvent::AclReply(AclReply {
                request: 103,
                result: Ok(_)
            })
        ));
        // Unsolicited replies have no draft ticket and cannot reset a draft.
        state
            .handle(
                frame(
                    kind::ACL,
                    proto::Acl {
                        channel_id: 7,
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        assert!(matches!(rx.try_recv().unwrap(), SessionEvent::Acl(_)));
        assert!(
            State::default().acl_queries.is_empty(),
            "reconnect owns a fresh query lane"
        );
    }

    #[tokio::test]
    async fn draft_acl_replies_refuse_lossy_snapshots_and_bound_unresolved_queries() {
        let mut state = State::default();
        let (tx, mut rx) = mpsc::channel(8);
        state
            .handle(
                frame(
                    kind::CHANNEL_STATE,
                    proto::ChannelState {
                        channel_id: Some(7),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        drain(&mut rx).await;
        for acl in [
            proto::Acl {
                channel_id: 7,
                acls: vec![proto::acl::ChanAcl::default(); 4097],
                ..Default::default()
            },
            proto::Acl {
                channel_id: 7,
                groups: vec![proto::acl::ChanGroup {
                    name: "g".repeat(257),
                    ..Default::default()
                }],
                ..Default::default()
            },
            proto::Acl {
                channel_id: 7,
                acls: vec![proto::acl::ChanAcl {
                    group: Some("g".repeat(257)),
                    ..Default::default()
                }],
                ..Default::default()
            },
            proto::Acl {
                channel_id: 7,
                groups: vec![proto::acl::ChanGroup {
                    name: "huge".into(),
                    add: vec![1; 8193],
                    ..Default::default()
                }],
                ..Default::default()
            },
        ] {
            state.query_acl(7, Some(9), &tx).await.unwrap();
            state.handle(frame(kind::ACL, acl), &tx).await.unwrap();
            assert!(matches!(
                rx.try_recv().unwrap(),
                SessionEvent::AclReply(AclReply {
                    request: 9,
                    result: Err(_)
                })
            ));
        }
        state.acl_queries = (100..228)
            .map(|id| (id, (id as u64, Some(id as u64))))
            .collect();
        state.query_acl(7, Some(10), &tx).await.unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionEvent::AclReply(AclReply {
                request: 10,
                result: Err(_)
            })
        ));
        assert_eq!(state.acl_queries.len(), 128);
    }

    #[tokio::test]
    async fn acl_wire_preserves_inherited_group_overrides_and_unknown_permission_bits() {
        let mut state = State {
            own_session: Some(7),
            ..Default::default()
        };
        let (tx, _rx) = mpsc::channel(8);
        let pristine = AclGroup {
            name: "inherited".into(),
            inherited: true,
            inherit: true,
            inheritable: true,
            add: vec![],
            remove: vec![],
            inherited_members: vec![123],
        };
        let mut override_group = pristine.clone();
        override_group.name = "overridden".into();
        override_group.remove.push(123);
        override_group.add.push(9876);
        let mut private = pristine.clone();
        private.name = "not-inheritable".into();
        private.inheritable = false;
        state
            .command(
                Command::UpdateAcl(Acl {
                    channel: 0,
                    inherit_acls: false,
                    groups: vec![pristine, override_group, private],
                    entries: vec![AclEntry {
                        apply_here: false,
                        apply_subs: true,
                        inherited: false,
                        user_id: Some(9876),
                        group: None,
                        grant: 0x8000_0008,
                        deny: 0x4000_0000,
                    }],
                }),
                &tx,
            )
            .await
            .unwrap();
        let (kind, body) = state.outbound.pop().unwrap();
        assert_eq!(kind, kind::ACL);
        let acl = proto::Acl::decode(body.as_slice()).unwrap();
        assert_eq!(
            acl.groups
                .iter()
                .map(|group| group.name.as_str())
                .collect::<Vec<_>>(),
            vec!["overridden", "not-inheritable"]
        );
        assert_eq!(acl.groups[0].add, vec![9876]);
        assert_eq!(acl.groups[0].remove, vec![123]);
        assert!(
            acl.groups
                .iter()
                .all(|group| group.inherited_members.is_empty())
        );
        assert_eq!(acl.acls[0].grant, Some(0x8000_0008));
        assert_eq!(acl.acls[0].deny, Some(0x4000_0000));
        assert_eq!(acl.acls[0].user_id, Some(9876));
        assert_eq!(acl.acls[0].apply_here, Some(false));
    }

    #[tokio::test]
    async fn sparse_config_preserves_full_limits_and_reconnect_defaults() {
        let mut state = State::default();
        let (tx, mut rx) = mpsc::channel(16);
        state
            .handle(
                frame(
                    kind::SERVER_SYNC,
                    proto::ServerSync {
                        session: Some(3),
                        max_bandwidth: Some(48_000),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        drain(&mut rx).await;
        for (config, expected) in [
            (
                proto::ServerConfig {
                    allow_html: Some(false),
                    message_length: Some(128),
                    image_message_length: Some(256),
                    max_users: Some(12),
                    recording_allowed: Some(false),
                    ..Default::default()
                },
                (false, 128, 256, Some(12), false, 48_000),
            ),
            (
                proto::ServerConfig {
                    max_bandwidth: Some(24_000),
                    ..Default::default()
                },
                (false, 128, 256, Some(12), false, 24_000),
            ),
            (
                proto::ServerConfig {
                    message_length: Some(0),
                    ..Default::default()
                },
                (false, 0, 256, Some(12), false, 24_000),
            ),
            (
                proto::ServerConfig {
                    image_message_length: Some(0),
                    max_users: Some(0),
                    ..Default::default()
                },
                (false, 0, 0, Some(0), false, 24_000),
            ),
            (
                proto::ServerConfig {
                    recording_allowed: Some(true),
                    ..Default::default()
                },
                (false, 0, 0, Some(0), true, 24_000),
            ),
            (
                proto::ServerConfig {
                    allow_html: Some(true),
                    message_length: Some(u32::MAX),
                    ..Default::default()
                },
                (true, u32::MAX as usize, 0, Some(0), true, 24_000),
            ),
        ] {
            state
                .handle(frame(kind::SERVER_CONFIG, config), &tx)
                .await
                .unwrap();
            let events = drain(&mut rx).await;
            let [SessionEvent::Limits(value)] = events.as_slice() else {
                panic!("limits missing")
            };
            assert_eq!(
                (
                    value.allow_html,
                    value.message_length,
                    value.image_message_length,
                    value.max_users,
                    value.recording_allowed,
                    value.max_bandwidth.unwrap()
                ),
                expected
            );
        }
        let mut fresh = State::default();
        fresh
            .handle(
                frame(
                    kind::SERVER_SYNC,
                    proto::ServerSync {
                        session: Some(3),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        drain(&mut rx).await;
        fresh
            .handle(
                frame(
                    kind::SERVER_CONFIG,
                    proto::ServerConfig {
                        max_users: Some(1),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        let events = drain(&mut rx).await;
        let [SessionEvent::Limits(value)] = events.as_slice() else {
            panic!("limits missing")
        };
        assert_eq!(
            (
                value.allow_html,
                value.message_length,
                value.image_message_length,
                value.max_users,
                value.recording_allowed,
                value.max_bandwidth
            ),
            (true, 5000, 131_072, Some(1), true, Some(72_000))
        );
    }

    #[tokio::test]
    async fn live_welcomes_are_connected_exact_source_updates_without_policy_side_effects() {
        let mut state = State::default();
        let (tx, mut rx) = mpsc::channel(32);
        state
            .handle(
                frame(
                    kind::SERVER_CONFIG,
                    proto::ServerConfig {
                        welcome_text: Some("before".into()),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        assert!(drain(&mut rx).await.is_empty());
        assert!(state.welcome_source.is_none());
        state
            .handle(
                frame(
                    kind::SERVER_SYNC,
                    proto::ServerSync {
                        session: Some(3),
                        welcome_text: Some("<b>initial.test</b>".into()),
                        ..Default::default()
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        let events = drain(&mut rx).await;
        assert!(matches!(
            &events[0],
            SessionEvent::Connected { session: 3, .. }
        ));
        assert!(matches!(&events[1], SessionEvent::Welcome(s) if s == "<b>initial.test</b>"));
        let policy = (
            state.limits.allow_html,
            state.limits.message_length,
            state.limits.image_message_length,
            state.local_muted,
            state.local_deafened,
            state.voice_revision,
            state.voice_route.revision(),
            state.voice_route.target(),
        );
        let outbound = state.outbound.len();
        for (source, expected) in [
            (None, None),
            (Some("<b>initial.test</b>"), None),
            (
                Some("<b>changed.test</b>\nsecond"),
                Some("<b>changed.test</b>\nsecond"),
            ),
            (Some(""), Some("")),
            (Some(""), None),
            (None, None),
            (Some("<b>initial.test</b>"), Some("<b>initial.test</b>")),
        ] {
            state
                .handle(
                    frame(
                        kind::SERVER_CONFIG,
                        proto::ServerConfig {
                            welcome_text: source.map(str::to_owned),
                            ..Default::default()
                        },
                    ),
                    &tx,
                )
                .await
                .unwrap();
            let events = drain(&mut rx).await;
            match expected {
                Some(expected) => assert!(
                    matches!(events.as_slice(), [SessionEvent::Welcome(value)] if value == expected)
                ),
                None => assert!(events.is_empty()),
            }
            assert_eq!(
                (
                    state.limits.allow_html,
                    state.limits.message_length,
                    state.limits.image_message_length,
                    state.local_muted,
                    state.local_deafened,
                    state.voice_revision,
                    state.voice_route.revision(),
                    state.voice_route.target()
                ),
                policy
            );
            assert_eq!(state.outbound.len(), outbound);
        }
    }

    #[tokio::test]
    async fn live_welcome_bounds_and_new_connections_do_not_share_dismissal_state() {
        let (tx, mut rx) = mpsc::channel(16);
        for _ in 0..2 {
            let mut state = State {
                own_session: Some(3),
                ..Default::default()
            };
            state
                .handle(
                    frame(
                        kind::SERVER_CONFIG,
                        proto::ServerConfig {
                            welcome_text: Some("界".repeat(MAX_CONTENT_BYTES)),
                            allow_html: Some(false),
                            ..Default::default()
                        },
                    ),
                    &tx,
                )
                .await
                .unwrap();
            let events = drain(&mut rx).await;
            let SessionEvent::Welcome(source) = &events[0] else {
                panic!("welcome missing")
            };
            assert!(source.len() <= MAX_CONTENT_BYTES);
            assert!(source.ends_with('界'));
            assert!(matches!(events[1], SessionEvent::Limits(_)));
            assert!(!state.limits.allow_html);
        }
    }

    #[tokio::test]
    async fn encrypted_session_emits_no_plaintext_chat_frames_from_raw_or_restored_commands() {
        let mut state = State {
            own_session: Some(7),
            extension: Some(nuschl::capability::ALL.map(str::to_owned).to_vec()),
            ..State::default()
        };
        let (tx, mut rx) = mpsc::channel(64);
        for command in [
            Command::SendText {
                target: TextTarget::Channel(0),
                body: "private caption".into(),
                expected_peer: None,
            },
            Command::SendHtml {
                target: TextTarget::Channel(0),
                html: vec!["secret inline image".into()],
                expected_peer: None,
            },
            Command::Nuschl(NuschlRequest::Send(v1::ChatSend {
                request_id: 1,
                content: "plaintext send".into(),
                ..Default::default()
            })),
            Command::Nuschl(NuschlRequest::Edit(v1::ChatEdit {
                request_id: 2,
                content: "plaintext edit".into(),
                ..Default::default()
            })),
            Command::Nuschl(NuschlRequest::Delete(v1::ChatDelete {
                request_id: 3,
                ..Default::default()
            })),
        ] {
            state.command(command, &tx).await.unwrap();
        }
        let pending = PendingChat {
            key: [1; 16],
            conversation: [2; 16],
            target: TextTarget::Channel(0),
            content: "restored secret".into(),
        };
        state.outbox.push(Outgoing {
            target: TextTarget::Channel(0),
            expected_peer: None,
            wire: "restored secret".into(),
            body: "restored secret".into(),
            html: false,
            chat: Some((pending, true)),
        });
        state.flush_outbox(&tx).await.unwrap();
        assert!(
            state.outbox.waiting.is_empty(),
            "restored plaintext was drained as refused"
        );
        assert!(state.outbound.iter().all(|(kind, _)| !matches!(
            *kind,
            kind::TEXT_MESSAGE
                | nuschl::kind::CHAT_SEND
                | nuschl::kind::CHAT_EDIT
                | nuschl::kind::CHAT_DELETE
        )));
        assert!(
            rx.try_recv().is_ok(),
            "the caller receives an explicit local refusal"
        );
    }

    use nuschl_protocol::nuschl::version;

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

    fn negotiated(policy: v1::Policy) -> Negotiated {
        Negotiated {
            nuschl: true,
            capabilities: Vec::new(),
            policy: Some(policy),
            kt_log: Vec::new(),
        }
    }

    fn above(version: &Version) -> u64 {
        version::to_wire(&Version::new(
            version.major,
            version.minor,
            version.patch + 1,
        ))
        .unwrap()
    }

    #[test]
    fn the_hello_reports_this_clients_version_and_the_policy_is_read_by_semver() {
        let own = client_version();
        assert_eq!(own.to_string(), env!("CARGO_PKG_VERSION"));
        let ahead = above(&own);
        let current = version::to_wire(&own).unwrap();
        let recommended = negotiated(v1::Policy {
            recommended_client: ahead,
            ..Default::default()
        });
        assert!(matches!(
            recommended.standing(0),
            Standing::Recommended { .. }
        ));
        let met = negotiated(v1::Policy {
            minimum_client: current,
            recommended_client: current,
            ..Default::default()
        });
        assert_eq!(met.standing(0), Standing::Current);
        // Below the minimum: a warning until the date (milliseconds here,
        // seconds on the wire), refused from it on.
        let graced = negotiated(v1::Policy {
            minimum_client: ahead,
            minimum_client_from: 2_000,
            ..Default::default()
        });
        assert!(matches!(
            graced.standing(1_999_999),
            Standing::Required { from: 2_000, .. }
        ));
        assert!(matches!(
            graced.standing(2_000_000),
            Standing::Refused { .. }
        ));
        // A stock server's (absent) policy asks nothing.
        let stock = Negotiated {
            policy: None,
            ..negotiated(v1::Policy::default())
        };
        assert_eq!(stock.standing(0), Standing::Current);
    }

    #[test]
    fn a_wrong_version_rejection_below_the_minimum_names_the_version() {
        let rejected = |kind| Error::Rejected {
            kind,
            reason: "whatever the server says".into(),
        };
        let reset = || Error::Io(std::io::ErrorKind::ConnectionReset.into());
        let policy = v1::Policy {
            minimum_client: version::to_wire(&Version::new(9, 1, 0)).unwrap(),
            minimum_client_from: 2_000,
            ..Default::default()
        };
        let graced = Below::of(&policy, 1_999_999).unwrap();
        let refused = Below::of(&policy, 2_000_000).unwrap();
        assert!(!graced.refused && refused.refused);
        for below in [&graced, &refused] {
            match outdated(rejected(RejectKind::WrongVersion), Some(below)) {
                Error::Outdated { minimum, current } => {
                    assert_eq!(minimum, "9.1.0");
                    assert_eq!(current, env!("CARGO_PKG_VERSION"));
                }
                other => panic!("{other:?}"),
            }
            // Another rejection stays the server's.
            assert!(matches!(
                outdated(rejected(RejectKind::ServerFull), Some(below)),
                Error::Rejected { .. }
            ));
        }
        // A connection closed before the Reject was read: the policy only once
        // the hello said the date has passed.
        assert!(matches!(
            outdated(reset(), Some(&refused)),
            Error::Outdated { .. }
        ));
        assert!(matches!(outdated(reset(), Some(&graced)), Error::Io(_)));
        // A server whose minimum we meet (or stock Murmur's "update Mumble").
        assert!(matches!(
            outdated(rejected(RejectKind::WrongVersion), None),
            Error::Rejected { .. }
        ));
        assert_eq!(
            Below::of(
                &v1::Policy {
                    minimum_client: version::to_wire(&client_version()).unwrap(),
                    ..Default::default()
                },
                0
            ),
            None
        );
    }

    #[test]
    fn end_to_end_mode_never_sends_plaintext_voice_even_a_terminator() {
        let state = State {
            own_session: Some(7),
            opus: true,
            extension: Some(vec![nuschl::capability::E2EE.into()]),
            ..State::default()
        };
        let packet = VoicePacket {
            sender: 0,
            sequence: 0,
            opus: vec![1, 2, 3],
            end: true,
            volume: 1.0,
        };
        assert!(state.outgoing_voice(packet).is_none());
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
    async fn private_presence_tokens_survive_neither_reconnect_nor_identity_replacement() {
        let (tx, mut rx) = mpsc::channel(32);
        let mut first = State::default();
        let mut second = State::default();
        let update = |hash: &str| proto::UserState {
            session: Some(4),
            hash: Some(hash.into()),
            ..Default::default()
        };
        first.user_state(update("abc"), &tx).await.unwrap();
        let before = first.users[&4].instance;
        // A wholly new control session reusing every server-side identifier.
        second.user_state(update("abc"), &tx).await.unwrap();
        assert_ne!(before, second.users[&4].instance);
        first.user_state(update("abc"), &tx).await.unwrap();
        assert_eq!(before, first.users[&4].instance);
        // A changed identity is not merely another presence update.
        first.user_state(update("different"), &tx).await.unwrap();
        assert_ne!(before, first.users[&4].instance);
        let current = first.users[&4].instance;
        first.outbound.clear();
        first
            .send_text(
                Outgoing {
                    target: TextTarget::Direct(4),
                    expected_peer: Some(before),
                    wire: "private".into(),
                    body: "private".into(),
                    html: false,
                    chat: None,
                },
                &tx,
            )
            .await
            .unwrap();
        assert!(
            first.outbound.is_empty(),
            "never send to the replacement identity"
        );
        assert_eq!(current, first.users[&4].instance);
        assert!(drain(&mut rx).await.iter().any(|event| matches!(event, SessionEvent::Notice(note) if note == nuschl_core::model::RECIPIENT_UNAVAILABLE_NOTICE)));
    }

    #[tokio::test]
    async fn sparse_deafness_updates_always_imply_mute() {
        let mut state = State::default();
        let (tx, mut rx) = mpsc::channel(8);
        for update in [
            proto::UserState {
                session: Some(42),
                self_deaf: Some(true),
                ..Default::default()
            },
            proto::UserState {
                session: Some(42),
                // A contradictory partial echo must not expose an
                // unmuted/deafened aggregate to the SDK.
                self_mute: Some(false),
                ..Default::default()
            },
            proto::UserState {
                session: Some(42),
                deaf: Some(true),
                ..Default::default()
            },
        ] {
            state
                .handle(frame(kind::USER_STATE, update), &tx)
                .await
                .unwrap();
            let Some(SessionEvent::User(user)) = rx.recv().await else {
                panic!("expected a user update")
            };
            assert!(user.self_deafened && user.self_muted);
            assert!(!user.deafened || user.muted);
        }
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
    async fn direct_conversations_carry_sends_and_messages_from_others() {
        let mut state = State {
            own_session: Some(7),
            extension: Some(vec![nuschl::capability::HISTORY.into()]),
            ..State::default()
        };
        let (tx, mut rx) = mpsc::channel(64);
        for (session, name, user_id) in [(7, "Ada", 1), (9, "Bob", 2)] {
            let user = proto::UserState {
                session: Some(session),
                name: Some(name.into()),
                user_id: Some(user_id),
                ..Default::default()
            };
            state
                .handle(frame(kind::USER_STATE, user), &tx)
                .await
                .unwrap();
        }
        while rx.try_recv().is_ok() {}
        let send = |body: &str| Command::SendText {
            target: TextTarget::DirectConversation([5; 16]),
            body: body.into(),
            expected_peer: None,
        };
        let list = v1::DirectList {
            request_id: 0,
            directs: vec![v1::Direct {
                conversation: vec![5; 16],
                user_id: 2,
                name: "Bob".into(),
                ..Default::default()
            }],
        };
        state
            .nuschl(NuschlEvent::DirectList(list), &tx)
            .await
            .unwrap();
        while rx.try_recv().is_ok() {}
        state.command(send("hi Bob"), &tx).await.unwrap();
        let [(nuschl::kind::CHAT_SEND, wire)] = state.outbound.as_slice() else {
            panic!("one ChatSend: {:?}", state.outbound)
        };
        let wire = v1::ChatSend::decode(wire.as_slice()).unwrap();
        assert_eq!(
            (wire.conversation.as_slice(), wire.content.as_str()),
            (&[5; 16][..], "hi Bob")
        );
        let Ok(SessionEvent::ChatPending(pending)) = rx.try_recv() else {
            panic!("not pending")
        };
        assert_eq!(pending.target, TextTarget::DirectConversation([5; 16]));
        let Ok(SessionEvent::Text(echo)) = rx.try_recv() else {
            panic!("no outgoing echo")
        };
        assert_eq!(echo.target, TextTarget::DirectConversation([5; 16]));
        assert_eq!(echo.peer.map(|peer| peer.session), Some(9));
        // A message in a conversation not known before is a new direct one.
        state.outbound.clear();
        let chat = v1::ChatEvent {
            conversation: vec![6; 16],
            message: Some(v1::Message {
                seq: 1,
                author: Some(v1::Author {
                    user_id: 3,
                    name: "Cy".into(),
                    ..Default::default()
                }),
                content: "hello".into(),
                format: v1::Format::Text as i32,
                ..Default::default()
            }),
        };
        state.nuschl(NuschlEvent::Chat(chat), &tx).await.unwrap();
        let Ok(SessionEvent::Nuschl(NuschlEvent::Chat(_))) = rx.try_recv() else {
            panic!("no chat event")
        };
        let Ok(SessionEvent::Text(text)) = rx.try_recv() else {
            panic!("no text")
        };
        assert_eq!(
            (text.target, text.author.as_str(), text.seq, text.outgoing),
            (
                TextTarget::DirectConversation([6; 16]),
                "Cy",
                Some(1),
                false
            )
        );
        assert!(state.directs.contains_key(&[6; 16]));
        assert_eq!(
            state
                .outbound
                .iter()
                .map(|(kind, _)| *kind)
                .collect::<Vec<_>>(),
            [nuschl::kind::LIST_DIRECT],
            "the list is asked for again"
        );
        // An image goes the stock way to Bob's session, shown in the conversation.
        state.outbound.clear();
        while rx.try_recv().is_ok() {}
        let image = "<img src=\"data:image/png;base64,iVBORw0KGgo%3D\" />".to_string();
        let html = Command::SendHtml {
            target: TextTarget::DirectConversation([5; 16]),
            html: vec![image.clone()],
            expected_peer: None,
        };
        state.command(html, &tx).await.unwrap();
        let [(kind::TEXT_MESSAGE, wire)] = state.outbound.as_slice() else {
            panic!("one text message: {:?}", state.outbound)
        };
        let wire = proto::TextMessage::decode(wire.as_slice()).unwrap();
        assert_eq!((wire.message, wire.session), (image, vec![9]));
        let Ok(SessionEvent::Text(echo)) = rx.try_recv() else {
            panic!("no outgoing echo")
        };
        assert_eq!(echo.target, TextTarget::DirectConversation([5; 16]));
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
            .command(send("x".repeat(state.limits.image_message_length + 1)), &tx)
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
            opus: nuschl_core::audio::SILENT_OPUS.to_vec(),
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
            read_control(&mut input, &AtomicU64::new(0)).await,
            Err(Error::Protocol(_))
        ));
    }
}

#[cfg(test)]
mod delayed_action_authority_tests {
    use super::*;

    fn observed() -> State {
        let mut state = State {
            own_session: Some(1),
            ..State::default()
        };
        for (session, instance, user_id) in [(1, 11, 101), (2, 22, 202), (3, 33, 303)] {
            state.users.insert(
                session,
                User {
                    session,
                    instance,
                    user_id: Some(user_id),
                    channel: 5,
                    ..User::default()
                },
            );
        }
        for (id, instance) in [(0, 40), (5, 45), (6, 46)] {
            state.channels.insert(
                id,
                Channel {
                    id,
                    instance,
                    ..Channel::default()
                },
            );
        }
        state.conversations.insert(5, [5; 16]);
        state.conversations.insert(6, [6; 16]);
        state
    }
    fn snapshot() -> CommandAuthority {
        CommandAuthority {
            own: (1, 11),
            subject: Some(ActionSubject::User(2, 22)),
            users: vec![(1, 11), (2, 22)],
            channels: vec![(0, 40), (5, 45), (6, 46)],
            conversations: vec![[5; 16]],
            accounts: Vec::new(),
        }
    }
    fn scoped(authority: CommandAuthority, command: Command) -> Command {
        Command::Scoped {
            authority,
            command: Box::new(command),
        }
    }
    fn acl_lease() -> CommandAuthority {
        CommandAuthority {
            own: (1, 11),
            subject: Some(ActionSubject::Channel(5, 45)),
            users: vec![(1, 11)],
            channels: vec![(5, 45)],
            conversations: Vec::new(),
            accounts: Vec::new(),
        }
    }
    fn acl_update() -> Command {
        Command::UpdateAcl(Acl {
            channel: 5,
            inherit_acls: true,
            groups: Vec::new(),
            entries: vec![AclEntry {
                apply_here: true,
                apply_subs: true,
                inherited: false,
                user_id: Some(202),
                group: None,
                grant: 8,
                deny: 0,
            }],
        })
    }

    #[test]
    fn root_administrators_can_edit_acl_but_do_not_gain_other_channel_actions() {
        let mut state = observed();
        state.permissions.insert(0, crate::model::permission::WRITE);
        state.permissions.insert(5, 0);
        for command in [
            Command::QueryAclDraft {
                channel: 5,
                request: 1,
            },
            acl_update(),
            Command::LookupAccount {
                channel: 5,
                request: 2,
                name: "Offline".into(),
            },
        ] {
            assert!(state.authorize(scoped(acl_lease(), command)).is_some());
        }
        assert!(
            state
                .authorize(scoped(acl_lease(), Command::RemoveChannel(5)))
                .is_none()
        );
        state.permissions.remove(&0);
        assert!(
            state.authorize(scoped(acl_lease(), acl_update())).is_some(),
            "unknown root alternative is left to authoritative server rejection"
        );
        state.permissions.insert(0, 0);
        assert!(state.authorize(scoped(acl_lease(), acl_update())).is_none());
        assert!(
            state
                .authorize(scoped(
                    acl_lease(),
                    Command::QueryAclDraft {
                        channel: 5,
                        request: 3
                    }
                ))
                .is_none()
        );
    }

    #[tokio::test]
    async fn paced_acl_commands_reauthorize_channel_presence_permissions_and_incomplete_envelopes()
    {
        for change in 0..4 {
            let mut state = observed();
            let (tx, mut rx) = mpsc::channel(16);
            state.outbox.level = state.outbox.capacity();
            let mut lease = acl_lease();
            if change == 3 {
                lease.channels.clear();
            }
            state
                .enqueue_admin(
                    scoped(
                        lease.clone(),
                        Command::QueryAclDraft {
                            channel: 5,
                            request: 70,
                        },
                    ),
                    &tx,
                )
                .await
                .unwrap();
            state
                .enqueue_admin(scoped(lease, acl_update()), &tx)
                .await
                .unwrap();
            state.dispatch_admin(&tx).await.unwrap();
            assert!(
                state.outbound.is_empty(),
                "full credit must defer semantic controls"
            );
            match change {
                0 => state.channels.get_mut(&5).unwrap().instance += 1,
                1 => state.users.get_mut(&1).unwrap().instance += 1,
                2 => {
                    state.permissions.insert(5, 0);
                    state.permissions.insert(0, 0);
                }
                _ => {}
            }
            state.outbox.at -= Duration::from_secs(10);
            state.dispatch_admin(&tx).await.unwrap();
            state.dispatch_admin(&tx).await.unwrap();
            assert!(
                state.outbound.is_empty(),
                "expired/incomplete queued controls must emit no ACL frame"
            );
            assert!(state.acl_queries.is_empty());
            assert_eq!(state.admin_bytes, 0);
            assert!(matches!(
                rx.try_recv().unwrap(),
                SessionEvent::AclReply(AclReply {
                    request: 70,
                    result: Err(_)
                })
            ));
        }
    }

    #[tokio::test]
    async fn paced_acl_writes_preserve_fifo_and_shared_credit_without_delaying_privacy() {
        let mut state = observed();
        let (tx, mut rx) = mpsc::channel(16);
        state.outbox.level = state.outbox.capacity();
        state
            .enqueue_admin(scoped(acl_lease(), acl_update()), &tx)
            .await
            .unwrap();
        state
            .enqueue_admin(
                scoped(
                    acl_lease(),
                    Command::QueryAclDraft {
                        channel: 5,
                        request: 80,
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        state
            .command(
                Command::SetSelf {
                    muted: true,
                    deafened: true,
                },
                &tx,
            )
            .await
            .unwrap();
        assert!(
            matches!(state.outbound.first(), Some((kind::USER_STATE, _))),
            "privacy must bypass admin waiting"
        );
        assert!(state.local_muted && state.local_deafened);
        state.outbound.clear();
        state.dispatch_admin(&tx).await.unwrap();
        assert!(state.outbound.is_empty());
        state.outbox.at = Instant::now();
        state.outbox.level = state.outbox.capacity(); // enough for exactly one control after refill
        state.outbox.at -= Duration::from_secs_f64(1.3);
        state.dispatch_admin(&tx).await.unwrap();
        assert_eq!(state.outbound.len(), 1);
        let write = proto::Acl::decode(state.outbound[0].1.as_slice()).unwrap();
        assert_eq!(write.query, Some(false));
        assert_eq!(write.acls[0].user_id, Some(202));
        state.dispatch_admin(&tx).await.unwrap();
        assert_eq!(
            state.outbound.len(),
            1,
            "verification waits for the next credit"
        );
        state.outbox.at -= Duration::from_secs_f64(1.3);
        state.dispatch_admin(&tx).await.unwrap();
        assert_eq!(state.outbound.len(), 2);
        assert_eq!(
            proto::Acl::decode(state.outbound[1].1.as_slice())
                .unwrap()
                .query,
            Some(true)
        );
        state
            .handle(
                ControlFrame {
                    kind: kind::ACL,
                    payload: proto::Acl {
                        channel_id: 5,
                        ..Default::default()
                    }
                    .encode_to_vec()
                    .into(),
                },
                &tx,
            )
            .await
            .unwrap();
        assert!(
            std::iter::from_fn(|| rx.try_recv().ok()).any(|event| matches!(
                event,
                SessionEvent::AclReply(AclReply {
                    request: 80,
                    result: Ok(_)
                })
            ))
        );
        assert!(state.admin_due().is_none());
    }

    #[tokio::test]
    async fn admin_queue_bounds_count_and_body_bytes_and_never_overtakes_lookup_barrier() {
        let mut state = observed();
        let (tx, mut rx) = mpsc::channel(32);
        for request in 0..17 {
            state
                .enqueue_admin(
                    scoped(
                        acl_lease(),
                        Command::QueryAclDraft {
                            channel: 5,
                            request,
                        },
                    ),
                    &tx,
                )
                .await
                .unwrap();
        }
        assert_eq!(state.admin_pending.len(), 16);
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionEvent::AclReply(AclReply {
                request: 16,
                result: Err(_)
            })
        ));
        assert!(state.outbound.is_empty());
        state.admin_pending.clear();
        state.admin_bytes = 0;
        let groups = (0..160)
            .map(|index| AclGroup {
                name: format!("g{index}"),
                inherited: false,
                inherit: true,
                inheritable: true,
                add: (0..8192).collect(),
                remove: vec![],
                inherited_members: vec![],
            })
            .collect();
        let body = Acl {
            channel: 5,
            inherit_acls: true,
            entries: vec![],
            groups,
        };
        for _ in 0..3 {
            state
                .enqueue_admin(scoped(acl_lease(), Command::UpdateAcl(body.clone())), &tx)
                .await
                .unwrap();
        }
        assert_eq!(
            state.admin_pending.len(),
            2,
            "byte cap must bind before command-count cap"
        );
        assert!(state.admin_bytes <= 16 * 1024 * 1024);
        assert!(matches!(rx.try_recv().unwrap(), SessionEvent::Notice(_)));
        state.admin_pending.clear();
        state.admin_bytes = 0;
        state
            .enqueue_admin(
                scoped(
                    acl_lease(),
                    Command::QueryAclDraft {
                        channel: 5,
                        request: 90,
                    },
                ),
                &tx,
            )
            .await
            .unwrap();
        state
            .lookup_account(5, 91, "Offline Alice".into(), &tx)
            .await
            .unwrap();
        assert_eq!(state.outbound[0].0, kind::PING);
        state.dispatch_admin(&tx).await.unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionEvent::AclReply(AclReply {
                request: 90,
                result: Err(_)
            })
        ));
        assert_eq!(
            state.outbound.len(),
            1,
            "an ACL response must not create unsolicited names after the barrier"
        );
        assert!(state.acl_queries.is_empty());
    }

    #[tokio::test]
    async fn readonly_stats_never_retargets_reused_session_or_reconnected_own_presence() {
        let mut state = observed();
        let (events, _) = mpsc::channel(16);
        let lease = snapshot().for_command(&Command::RequestStats(2)).unwrap();
        // Harmless updates and unrelated removals do not revoke a precise action.
        state.users.get_mut(&2).unwrap().name = "renamed".into();
        state.users.get_mut(&2).unwrap().channel = 6;
        state.channels.remove(&0);
        let command = state
            .authorize(scoped(lease.clone(), Command::RequestStats(2)))
            .unwrap();
        state.command(command, &events).await.unwrap();
        assert_eq!(state.outbound.len(), 1);
        assert_eq!(state.outbound[0].0, kind::USER_STATS);
        let stats = proto::UserStats::decode(state.outbound[0].1.as_slice()).unwrap();
        assert_eq!(stats.session, Some(2));
        state.outbound.clear();
        state.users.get_mut(&2).unwrap().instance = 222;
        assert!(
            state
                .authorize(scoped(lease.clone(), Command::RequestStats(2)))
                .is_none()
        );
        state.users.get_mut(&2).unwrap().instance = 22;
        state.users.get_mut(&1).unwrap().instance = 111;
        assert!(
            state
                .authorize(scoped(lease, Command::RequestStats(2)))
                .is_none()
        );
        assert!(state.outbound.is_empty());
    }
    #[test]
    fn channel_lifetime_and_known_permission_revocations_are_rechecked() {
        let mut state = observed();
        let mut origin = snapshot();
        origin.subject = Some(ActionSubject::Channel(5, 45));
        let lease = origin.for_command(&Command::Join(5)).unwrap();
        state.permissions.insert(5, crate::model::permission::ENTER);
        assert!(
            state
                .authorize(scoped(lease.clone(), Command::Join(5)))
                .is_some()
        );
        state.permissions.insert(5, 0);
        assert!(
            state
                .authorize(scoped(lease.clone(), Command::Join(5)))
                .is_none()
        );
        state.permissions.remove(&5); // Unknown is left to authoritative server ACLs.
        assert!(
            state
                .authorize(scoped(lease.clone(), Command::Join(5)))
                .is_some()
        );
        state.channels.get_mut(&5).unwrap().instance = 450;
        assert!(state.authorize(scoped(lease, Command::Join(5))).is_none());
    }
    #[test]
    fn incomplete_nested_oversized_and_wrong_semantic_envelopes_fail_closed() {
        let mut state = observed();
        let mut lease = snapshot();
        lease.subject = None;
        lease.users.clear();
        assert!(
            state
                .authorize(scoped(lease.clone(), Command::RequestStats(2)))
                .is_none()
        );
        assert!(
            state
                .authorize(scoped(
                    lease.clone(),
                    scoped(snapshot(), Command::RequestStats(2))
                ))
                .is_none()
        );
        assert!(
            state
                .authorize(scoped(
                    lease.clone(),
                    Command::Nuschl(NuschlRequest::OpenDirect(v1::OpenDirect {
                        request_id: 1,
                        user_id: 202
                    }))
                ))
                .is_none()
        );
        lease.subject = Some(ActionSubject::User(2, 22));
        assert!(
            state
                .authorize(scoped(
                    lease.clone(),
                    Command::Nuschl(NuschlRequest::OpenDirect(v1::OpenDirect {
                        request_id: 1,
                        user_id: 303
                    }))
                ))
                .is_none()
        );
        assert!(
            state
                .authorize(scoped(
                    lease.clone(),
                    Command::Nuschl(NuschlRequest::OpenDirect(v1::OpenDirect {
                        request_id: 1,
                        user_id: 202
                    }))
                ))
                .is_some()
        );
        for id in [[5; 16], [6; 16]] {
            let command = Command::Nuschl(NuschlRequest::Delete(v1::ChatDelete {
                request_id: 1,
                conversation: id.to_vec(),
                target: 4,
            }));
            // Channel conversations require their original channel lifetime,
            // even when an explicit conversation ID was supplied.
            let mut minimal = lease.clone();
            minimal.channels.clear();
            assert!(state.authorize(scoped(minimal, command)).is_none());
        }
        lease.subject = Some(ActionSubject::Channel(5, 45));
        lease.channels.clear();
        assert!(
            state
                .authorize(scoped(
                    lease.clone(),
                    Command::Nuschl(NuschlRequest::History(v1::HistoryRequest {
                        conversation: vec![5; 16],
                        ..Default::default()
                    }))
                ))
                .is_some()
        );
        assert!(
            state
                .authorize(scoped(
                    lease.clone(),
                    Command::Nuschl(NuschlRequest::History(v1::HistoryRequest {
                        conversation: vec![6; 16],
                        ..Default::default()
                    }))
                ))
                .is_none()
        );
        assert!(
            state
                .authorize(scoped(
                    lease.clone(),
                    Command::Mls(crate::mls::MlsCommand::OpenDirect { user_id: 303 })
                ))
                .is_none()
        );
        let mut combined = snapshot();
        combined.users = vec![(2, 22); 65];
        combined.channels = vec![(5, 45); 64];
        assert!(
            state
                .authorize(scoped(combined, Command::RequestStats(2)))
                .is_none(),
            "combined envelope refs are bounded too"
        );
        lease.users = vec![(2, 22); MAX_ACTION_TARGETS + 1];
        assert!(
            state
                .authorize(scoped(lease, Command::RequestStats(2)))
                .is_none()
        );
        assert!(
            Command::RequestBlobs {
                textures: vec![2; MAX_ACTION_TARGETS + 1],
                comments: vec![],
                descriptions: vec![]
            }
            .targets()
            .is_none()
        );
    }
    #[test]
    fn whisper_reuse_and_failed_replacement_suspend_without_fallback() {
        let mut state = observed();
        let install = |revision| Command::Whisper {
            revision,
            id: 1,
            targets: vec![VoiceTargetSpec::Users(vec![2])],
        };
        let lease = snapshot().for_command(&install(1)).unwrap();
        let revision = state.voice_route.suspend();
        assert!(
            state
                .authorize(scoped(lease.clone(), install(revision)))
                .is_some()
        );
        assert!(state.voice_target_valid(1));
        state.select_voice_target(1);
        state.users.get_mut(&2).unwrap().instance = 222;
        assert!(!state.voice_target_valid(state.voice_target));
        assert!(
            state
                .authorize(scoped(lease.clone(), install(state.voice_route.suspend())))
                .is_none()
        );
        assert_eq!(state.voice_target, 31);
        assert!(!state.voice_target_valid(state.voice_target));
        assert!(!state.voice_authorities.contains_key(&1));
        assert!(
            state
                .authorize(scoped(lease, Command::SelectVoiceTarget(1)))
                .is_none()
        );
        assert_eq!(state.voice_route.target(), 31);
        assert!(!state.voice_target_valid(state.voice_target));
        // Only an explicit accepted normal selection clears the suspended route.
        state.select_voice_target(0);
        assert!(state.voice_target_valid(0));
        assert_eq!(state.voice_route.target(), 0);
    }
    #[tokio::test]
    async fn automatic_metadata_queued_before_reconnect_or_reuse_produces_no_wire_request() {
        let mut state = observed();
        let (events, _) = mpsc::channel(16);
        let authority = CommandAuthority {
            subject: None,
            own: (1, 11),
            users: vec![(2, 22)],
            channels: vec![],
            conversations: vec![],
            accounts: vec![],
        };
        let request = || Command::RequestBlobs {
            textures: vec![],
            comments: vec![2],
            descriptions: vec![],
        };
        let accepted = state
            .authorize(scoped(authority.clone(), request()))
            .unwrap();
        state.command(accepted, &events).await.unwrap();
        assert_eq!(state.outbound[0].0, kind::REQUEST_BLOB);
        state.outbound.clear();
        // A queued envelope retains old tokens even when the wire ID survives.
        let queued = scoped(authority.clone(), request());
        state.users.get_mut(&1).unwrap().instance = 111;
        assert!(state.authorize(queued).is_none());
        state.users.get_mut(&1).unwrap().instance = 11;
        state.users.get_mut(&2).unwrap().instance = 222;
        assert!(state.authorize(scoped(authority, request())).is_none());
        assert!(state.outbound.is_empty());
    }
    #[tokio::test]
    async fn transactional_whisper_and_reconnect_never_select_previous_or_normal_recipient() {
        let mut state = observed();
        let (events, _) = mpsc::channel(16);
        let command = |revision| Command::Whisper {
            revision,
            id: 1,
            targets: vec![VoiceTargetSpec::Users(vec![2])],
        };
        let lease = snapshot().for_command(&command(1)).unwrap();
        let accepted = state
            .authorize(scoped(lease.clone(), command(state.voice_route.suspend())))
            .unwrap();
        state.command(accepted, &events).await.unwrap();
        assert_eq!(state.voice_target, 1);
        assert!(state.voice_target_valid(1));
        let latch = state.voice_route.clone();
        let mut reconnected = observed();
        reconnected.restore_voice_route(latch.clone());
        assert_eq!(reconnected.voice_target, 31);
        assert!(!reconnected.voice_target_valid(31));
        let mut wrong = lease.clone();
        wrong.users.clear();
        assert!(
            state
                .authorize(scoped(
                    wrong,
                    Command::Whisper {
                        revision: state.voice_route.suspend(),
                        id: 1,
                        targets: vec![VoiceTargetSpec::Users(vec![3])]
                    }
                ))
                .is_none()
        );
        assert_eq!(state.voice_target, 31);
        assert!(!state.voice_target_valid(state.voice_target));
        // The previous installation was revoked by the rejected replacement.
        assert!(
            state
                .authorize(scoped(lease, Command::SelectVoiceTarget(1)))
                .is_none()
        );
        assert!(!state.voice_authorities.contains_key(&1));
        assert_eq!(latch.target(), 31);
    }
    #[test]
    fn immutable_account_operations_require_exact_semantic_coverage() {
        let mut state = observed();
        let mut lease = snapshot();
        let command = || {
            Command::Identity(crate::identity::IdentityCommand::MarkVerified { account: [9; 16] })
        };
        assert!(state.authorize(scoped(lease.clone(), command())).is_none());
        lease.accounts = vec![[8; 16]];
        assert!(state.authorize(scoped(lease.clone(), command())).is_none());
        lease.accounts = vec![[9; 16]];
        assert!(state.authorize(scoped(lease.clone(), command())).is_some());
        state.users.get_mut(&2).unwrap().instance = 222;
        assert!(state.authorize(scoped(lease, command())).is_none());
    }
    #[tokio::test]
    async fn rapid_voice_intents_old_install_and_stop_cannot_clear_newer_suspension() {
        let mut state = observed();
        state.local_muted = false;
        let packet = || VoicePacket {
            sender: 0,
            sequence: 1,
            opus: vec![0xf8, 0xff, 0xfe],
            end: false,
            volume: 1.,
        };
        let (events, _) = mpsc::channel(16);
        let install = |revision| Command::Whisper {
            revision,
            id: 1,
            targets: vec![VoiceTargetSpec::Users(vec![2])],
        };
        let lease = snapshot().for_command(&install(1)).unwrap();
        let a = state.voice_route.suspend();
        let accepted = state.authorize(scoped(lease.clone(), install(a))).unwrap();
        state.command(accepted, &events).await.unwrap();
        assert!(state.outgoing_voice(packet()).is_some());
        let b = state.voice_route.suspend();
        let queued_b = scoped(lease.clone(), install(b));
        let c = state.voice_route.suspend(); // Newest intent may fail enqueue; suspension is already active.
        assert!(state.authorize(queued_b).is_none());
        assert_eq!(state.voice_route.revision(), c);
        assert_eq!(state.voice_route.target(), 31);
        assert!(state.outgoing_voice(packet()).is_none());
        let old_stop = Command::SelectVoiceTargetAt { id: 0, revision: c };
        let d = state.voice_route.suspend();
        assert!(state.authorize(old_stop).is_none());
        assert!(!state.admit_voice_target(b, 1));
        assert_eq!(state.voice_route.revision(), d);
        assert_eq!(state.voice_route.target(), 31);
        assert!(state.outgoing_voice(packet()).is_none());
        // Missing native intent metadata cannot manufacture a fresh actor intent.
        assert!(state.authorize(scoped(lease, install(0))).is_none());
        assert_eq!(state.voice_route.revision(), d);
        let normal = state.voice_route.suspend();
        assert!(matches!(
            state.authorize(Command::SelectVoiceTargetAt {
                id: 0,
                revision: normal
            }),
            Some(Command::SelectVoiceTargetAt { .. })
        ));
        assert!(state.admit_voice_target(normal, 0));
        assert!(state.outgoing_voice(packet()).is_some());
        let mut reconnected = observed();
        reconnected.restore_voice_route(state.voice_route.clone());
        assert_eq!(
            reconnected.voice_target, 0,
            "explicit latest normal choice survives reconnect"
        );
        state.select_voice_target(1);
        reconnected.restore_voice_route(state.voice_route.clone());
        assert_eq!(reconnected.voice_target, 31);
        assert_eq!(reconnected.voice_route.target(), 31);
        assert!(reconnected.outgoing_voice(packet()).is_none());
    }
}

#[cfg(test)]
mod plugin_data_tests {
    use super::*;
    use plugin::Refusal;

    const V1_5: u64 = (1 << 48) | (5 << 32);

    fn connected() -> State {
        let mut state = State {
            own_session: Some(1),
            version: V1_5,
            accept_plugin_data: true,
            ..State::default()
        };
        for (session, instance, user_id, hash) in [
            (1, 11, Some(101), "own"),
            (2, 22, Some(202), "a2"),
            (3, 33, None, "b3"),
        ] {
            state.users.insert(
                session,
                User {
                    session,
                    instance,
                    user_id,
                    certificate_hash: Some(hash.into()),
                    ..User::default()
                },
            );
        }
        state
    }

    fn incoming(sender: Option<u32>, data_id: Option<&str>, data: Option<&[u8]>) -> ControlFrame {
        ControlFrame {
            kind: kind::PLUGIN_DATA_TRANSMISSION,
            payload: proto::PluginDataTransmission {
                sender_session: sender,
                receiver_sessions: vec![],
                data: data.map(<[u8]>::to_vec),
                data_id: data_id.map(str::to_owned),
            }
            .encode_to_vec()
            .into(),
        }
    }

    fn payload(data_id: &str, data: &[u8]) -> plugin::Payload {
        plugin::Payload::new(data_id.into(), data.to_vec()).unwrap()
    }

    fn send(request: u64, receivers: Vec<(u32, u64)>, data_id: &str) -> Command {
        Command::PluginData {
            request,
            receivers,
            payload: payload(data_id, b"\x01\x02"),
        }
    }

    fn drain(rx: &mut mpsc::Receiver<SessionEvent>) -> Vec<SessionEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn received(events: Vec<SessionEvent>) -> Vec<IncomingPluginData> {
        events
            .into_iter()
            .filter_map(|event| match event {
                SessionEvent::PluginData(data) => Some(data),
                _ => None,
            })
            .collect()
    }

    fn admissions(events: &[SessionEvent]) -> Vec<(u64, Result<(), Refusal>)> {
        events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::PluginDataAdmission { request, result } => {
                    Some((*request, result.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn dropped(state: &State) -> u64 {
        state.plugin_dropped.load(Ordering::Relaxed)
    }

    #[tokio::test]
    async fn received_plugin_data_carries_the_senders_identity_at_arrival() {
        let mut state = connected();
        let (tx, mut rx) = mpsc::channel(64);
        state
            .handle(
                incoming(Some(2), Some("game"), Some(b"\x00\xff".as_slice())),
                &tx,
            )
            .await
            .unwrap();
        // Present but empty data and data ID are valid, as stock servers
        // forward them; data we sent ourselves is ours.
        state
            .handle(incoming(Some(3), Some(""), Some(b"".as_slice())), &tx)
            .await
            .unwrap();
        state
            .handle(incoming(Some(1), Some("self"), Some(b"me".as_slice())), &tx)
            .await
            .unwrap();
        // Session 2 is taken over by someone else: their packets carry their
        // own identity, the earlier one keeps the first occupant's.
        state.users.insert(
            2,
            User {
                session: 2,
                instance: 99,
                certificate_hash: Some("other".into()),
                ..User::default()
            },
        );
        state
            .handle(
                incoming(Some(2), Some("game"), Some(b"next".as_slice())),
                &tx,
            )
            .await
            .unwrap();
        let received = received(drain(&mut rx));
        let identity = |data: &IncomingPluginData| {
            (
                data.sender,
                data.sender_instance,
                data.sender_user_id,
                data.sender_certificate_hash.clone(),
            )
        };
        assert_eq!(
            received.iter().map(identity).collect::<Vec<_>>(),
            [
                (2, 22, Some(202), Some("a2".into())),
                (3, 33, None, Some("b3".into())),
                (1, 11, Some(101), Some("own".into())),
                (2, 99, None, Some("other".into())),
            ]
        );
        assert_eq!(received[0].payload, payload("game", b"\x00\xff"));
        assert_eq!(received[1].payload, payload("", b""));
        assert_eq!(received[3].payload, payload("game", b"next"));
        assert!(!format!("{received:?}").contains("next"));
        assert_eq!(dropped(&state), 0);
    }

    #[tokio::test]
    async fn unusable_plugin_data_is_dropped_and_counted_never_fatal() {
        let (tx, mut rx) = mpsc::channel(64);
        let long_id = "i".repeat(101);
        let not_utf8 = ControlFrame {
            kind: kind::PLUGIN_DATA_TRANSMISSION,
            payload: vec![0x08, 2, 0x1a, 0, 0x22, 1, 0xff].into(),
        };
        let mut state = connected();
        for frame in [
            incoming(None, Some("id"), Some(b"x".as_slice())),
            incoming(Some(9), Some("id"), Some(b"x".as_slice())),
            incoming(Some(2), None, Some(b"x".as_slice())),
            incoming(Some(2), Some("id"), None),
            incoming(Some(2), Some("id"), Some(&[0; 1001][..])),
            incoming(Some(2), Some(long_id.as_str()), Some(b"x".as_slice())),
            not_utf8,
            ControlFrame {
                kind: kind::PLUGIN_DATA_TRANSMISSION,
                payload: Bytes::from_static(&[0xff]),
            },
        ] {
            state.handle(frame, &tx).await.unwrap();
        }
        assert_eq!(dropped(&state), 8);
        // The reserved namespace has no generic consumer: not surfaced, and
        // not a failure either.
        state
            .handle(
                incoming(Some(2), Some("nuschl/signal"), Some(b"x".as_slice())),
                &tx,
            )
            .await
            .unwrap();
        assert_eq!(dropped(&state), 8);
        assert!(drain(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn plugin_data_is_skipped_undecoded_unless_wanted_and_allowed() {
        let (tx, mut rx) = mpsc::channel(64);
        // Garbage would count as dropped if it were decoded.
        let garbage = || ControlFrame {
            kind: kind::PLUGIN_DATA_TRANSMISSION,
            payload: Bytes::from_static(&[0xff, 0xff]),
        };
        let mut off = State {
            accept_plugin_data: false,
            ..connected()
        };
        let mut early = State {
            own_session: None,
            ..connected()
        };
        let mut encrypted = State {
            extension: Some(vec![
                nuschl::capability::KT.into(),
                nuschl::capability::MLS.into(),
                nuschl::capability::E2EE.into(),
            ]),
            ..connected()
        };
        for state in [&mut off, &mut early, &mut encrypted] {
            state.handle(garbage(), &tx).await.unwrap();
            state
                .handle(incoming(Some(2), Some("id"), Some(b"x".as_slice())), &tx)
                .await
                .unwrap();
            assert_eq!(dropped(state), 0);
        }
        assert!(drain(&mut rx).is_empty());
        // The opt-in is a command, taken even before login.
        early
            .command(Command::AcceptPluginData(false), &tx)
            .await
            .unwrap();
        assert!(!early.accept_plugin_data);
        off.command(Command::AcceptPluginData(true), &tx)
            .await
            .unwrap();
        off.handle(incoming(Some(2), Some("id"), Some(b"x".as_slice())), &tx)
            .await
            .unwrap();
        assert_eq!(received(drain(&mut rx)).len(), 1);
        assert!(drain(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn received_plugin_data_never_waits_and_is_budgeted() {
        // A full event channel: dropped and counted, the session goes on.
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(SessionEvent::Notice("busy".into())).unwrap();
        let mut state = connected();
        state
            .handle(incoming(Some(2), Some("id"), Some(b"x".as_slice())), &tx)
            .await
            .unwrap();
        assert_eq!(dropped(&state), 1);
        drain(&mut rx);
        // An exhausted receive budget drops before decoding.
        state.plugin_receive.items = 0.0;
        state.plugin_receive.at = Instant::now();
        state
            .handle(incoming(Some(2), Some("id"), Some(b"x".as_slice())), &tx)
            .await
            .unwrap();
        assert_eq!(dropped(&state), 2);
        assert!(drain(&mut rx).is_empty());
        // The budget itself: a second of items or bytes, refilled continuously.
        let start = Instant::now();
        let mut budget = PluginReceive {
            items: PluginReceive::ITEMS,
            bytes: PluginReceive::BYTES,
            at: start,
        };
        assert_eq!((0..100).filter(|_| budget.admit(10, start)).count(), 64);
        let later = start + Duration::from_millis(500);
        assert_eq!((0..100).filter(|_| budget.admit(10, later)).count(), 32);
        let much_later = later + Duration::from_secs(60);
        assert_eq!(
            (0..100).filter(|_| budget.admit(1100, much_later)).count(),
            59,
            "64 KiB of 1,100-byte messages"
        );
    }

    #[tokio::test]
    async fn sends_reach_named_receivers_once_and_report_admission() {
        let mut state = connected();
        let (tx, mut rx) = mpsc::channel(64);
        state
            .command(send(5, vec![(3, 33), (2, 22), (3, 33)], "game"), &tx)
            .await
            .unwrap();
        assert_eq!(admissions(&drain(&mut rx)), [(5, Ok(()))]);
        let [(kind, body)] = state.outbound.as_slice() else {
            panic!("one frame: {:?}", state.outbound.len());
        };
        assert_eq!(*kind, kind::PLUGIN_DATA_TRANSMISSION);
        let sent = plugin::PluginData::decode(body, 2).unwrap();
        assert_eq!(sent.sender, Some(1));
        assert_eq!(sent.receivers, [2, 3]);
        assert_eq!(sent.payload, payload("game", b"\x01\x02"));
        // Present-empty fields are sent as present; we may send to ourselves.
        state.outbound.clear();
        state
            .command(
                Command::PluginData {
                    request: 6,
                    receivers: vec![(1, 11)],
                    payload: payload("", b""),
                },
                &tx,
            )
            .await
            .unwrap();
        let message =
            proto::PluginDataTransmission::decode(state.outbound[0].1.as_slice()).unwrap();
        assert_eq!(
            (message.data, message.data_id, message.receiver_sessions),
            (Some(vec![]), Some(String::new()), vec![1])
        );
        assert_eq!(admissions(&drain(&mut rx)), [(6, Ok(()))]);
    }

    #[tokio::test]
    async fn sends_are_refused_locally_with_a_typed_reason() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut state = connected();
        let too_many: Vec<(u32, u64)> = (0..=MAX_ACTION_TARGETS as u32).map(|s| (s, 1)).collect();
        for command in [
            send(1, vec![(9, 1)], "game"),
            send(2, vec![(2, 22), (3, 34)], "game"),
            send(3, vec![(2, 0)], "game"),
            send(4, vec![], "game"),
            send(5, too_many, "game"),
            send(6, vec![(2, 22)], "nuschl/rtc"),
        ] {
            state.command(command, &tx).await.unwrap();
        }
        let mut old = State {
            version: (1 << 48) | (3 << 32),
            ..connected()
        };
        old.command(send(7, vec![(2, 22)], "game"), &tx)
            .await
            .unwrap();
        let mut encrypted = State {
            extension: Some(vec![nuschl::capability::E2EE.into()]),
            ..connected()
        };
        encrypted
            .command(send(8, vec![(2, 22)], "game"), &tx)
            .await
            .unwrap();
        let mut early = State {
            own_session: None,
            ..connected()
        };
        early
            .command(send(9, vec![(2, 22)], "game"), &tx)
            .await
            .unwrap();
        // A full bucket: refused with a hint, nothing charged or queued.
        let mut limited = connected();
        limited.plugin_rate.level = PluginRate::BURST;
        limited.plugin_rate.at = Instant::now();
        limited
            .command(send(10, vec![(2, 22)], "game"), &tx)
            .await
            .unwrap();
        let reported = admissions(&drain(&mut rx));
        let changed = Err(Refusal::ReceiverChanged);
        assert_eq!(
            reported[..9],
            [
                (1, changed.clone()),
                (2, changed.clone()),
                (3, changed),
                (4, Err(Refusal::Receivers(MAX_ACTION_TARGETS))),
                (5, Err(Refusal::Receivers(MAX_ACTION_TARGETS))),
                (6, Err(Refusal::Reserved)),
                (7, Err(Refusal::ServerTooOld)),
                (8, Err(Refusal::EndToEnd)),
                (9, Err(Refusal::NotConnected)),
            ]
        );
        let (10, Err(Refusal::RateLimited { retry_after_ms })) = &reported[9] else {
            panic!("{:?}", reported[9]);
        };
        assert!((1..=500).contains(retry_after_ms), "{retry_after_ms}");
        for state in [&state, &old, &encrypted, &early, &limited] {
            assert!(state.outbound.is_empty());
        }
        assert!(limited.plugin_rate.level <= PluginRate::BURST);
    }

    #[test]
    fn the_local_bucket_admits_its_burst_then_two_a_second() {
        let start = Instant::now();
        let mut rate = PluginRate {
            level: 0.0,
            at: start,
        };
        assert_eq!((0..20).filter(|_| rate.admit(start).is_ok()).count(), 10);
        assert_eq!(rate.admit(start), Err(500));
        let later = start + Duration::from_millis(1250);
        assert_eq!((0..5).filter(|_| rate.admit(later).is_ok()).count(), 2);
        assert_eq!(rate.admit(later), Err(250));
        let much_later = later + Duration::from_secs(3600);
        assert_eq!(
            (0..20).filter(|_| rate.admit(much_later).is_ok()).count(),
            10
        );
    }

    /// Murmur's `LeakyBucket::ratelimit` (`ServerUser.cpp`): whole tokens
    /// drained from integer milliseconds, the timer restarted only when one
    /// drained, a message limited when it would exceed the burst.
    struct MurmurBucket {
        per_second: i64,
        burst: i64,
        tokens: i64,
        since_ms: i64,
    }

    impl MurmurBucket {
        fn limited(&mut self, now_ms: i64) -> bool {
            let drain = (now_ms - self.since_ms) * self.per_second / 1000;
            if drain > 0 {
                self.since_ms = now_ms;
            }
            self.tokens = (self.tokens - drain).max(0);
            let limited = self.tokens > self.burst - 1;
            if !limited {
                self.tokens += 1;
            }
            limited
        }
    }

    #[test]
    fn the_local_bucket_never_overruns_murmurs_default_bucket() {
        // Attempts at these times (ms); every one the local bucket admits must
        // pass Murmur's default bucket (4 a second, burst 15).
        let alternating: Vec<i64> = (0..2000)
            .flat_map(|step| std::iter::repeat_n(step * 499, if step % 2 == 0 { 2 } else { 1 }))
            .collect();
        let flood: Vec<i64> = (0..120_000).collect();
        let bursts: Vec<i64> = (0..60)
            .flat_map(|burst| std::iter::repeat_n(burst * 4_999, 12))
            .collect();
        let jittered: Vec<i64> = (0..20_000)
            .map(|step| step * 251 + (step * 7919) % 97)
            .collect();
        for (name, schedule) in [
            ("alternating 2 and 1 every 499 ms", alternating),
            ("one attempt every millisecond", flood),
            ("bursts of 12 every 5 s", bursts),
            ("jittered 251 ms", jittered),
        ] {
            let start = Instant::now();
            let mut local = PluginRate {
                level: 0.0,
                at: start,
            };
            let mut murmur = MurmurBucket {
                per_second: 4,
                burst: 15,
                tokens: 0,
                since_ms: 0,
            };
            let mut admitted = 0;
            for &at in &schedule {
                let now = start + Duration::from_millis(at as u64);
                if local.admit(now).is_ok() {
                    admitted += 1;
                    assert!(!murmur.limited(at), "{name}: Murmur dropped at {at} ms");
                }
            }
            let seconds = (schedule.last().unwrap() / 1000).max(1);
            assert!(
                admitted >= seconds.min(schedule.len() as i64 / 2),
                "{name}: {admitted}"
            );
        }
    }

    #[tokio::test]
    async fn a_queued_send_never_reaches_a_reused_session() {
        let mut state = connected();
        let (tx, mut rx) = mpsc::channel(64);
        let authority = CommandAuthority {
            subject: None,
            own: (1, 11),
            users: vec![(2, 22)],
            channels: vec![],
            conversations: vec![],
            accounts: vec![],
        };
        let scoped = |receivers| Command::Scoped {
            authority: authority.clone(),
            command: Box::new(send(1, receivers, "game")),
        };
        let Some(admitted) = state.authorize(scoped(vec![(2, 22)])) else {
            panic!("a current receiver is authorised");
        };
        // A receiver the authority does not cover is never added.
        assert!(state.authorize(scoped(vec![(2, 22), (3, 33)])).is_none());
        // The receiver's number now belongs to someone else: the authority
        // fails, and a command already past it is refused by its own pair.
        state.users.get_mut(&2).unwrap().instance = 23;
        assert!(state.authorize(scoped(vec![(2, 22)])).is_none());
        state.command(admitted, &tx).await.unwrap();
        assert_eq!(
            admissions(&drain(&mut rx)),
            [(1, Err(Refusal::ReceiverChanged))]
        );
        assert!(state.outbound.is_empty());
        // More receivers than an authority may name never get that far.
        let many: Vec<(u32, u64)> = (0..=MAX_ACTION_TARGETS as u32).map(|s| (s, 1)).collect();
        assert!(send(1, many, "game").targets().is_none());
        assert!(send(1, vec![], "game").targets().is_none());
    }

    #[tokio::test]
    async fn oversized_plugin_frames_are_skipped_without_buffering() {
        let limit = plugin::max_body(MAX_RECEIVED_RECEIVERS);
        assert_eq!(limit, plugin::max_body(0) + 7 * 512);
        let skipped = AtomicU64::new(0);
        let frame = |size: usize| {
            let mut bytes = Vec::new();
            bytes.extend(kind::PLUGIN_DATA_TRANSMISSION.to_be_bytes());
            bytes.extend((size as u32).to_be_bytes());
            bytes.extend(vec![0; size]);
            bytes
        };
        // Up to stock Mumble's frame limit: read past, the next frame follows.
        let mut input = frame(limit + 1);
        input.extend(frame(MAX_CONTROL_BYTES));
        input.extend([0, 3, 0, 0, 0, 1, 9]);
        let frame_after = read_control(&mut input.as_slice(), &skipped).await.unwrap();
        assert_eq!(
            (frame_after.kind, frame_after.payload.as_ref()),
            (3, &[9][..])
        );
        assert_eq!(skipped.load(Ordering::Relaxed), 2);
        // One at the limit is kept for the decoder to judge.
        let kept = read_control(&mut frame(limit).as_slice(), &skipped)
            .await
            .unwrap();
        assert_eq!(kept.payload.len(), limit);
        // A truncated one ends the connection like any short frame.
        let mut truncated = frame(limit + 1);
        truncated.truncate(100);
        assert!(
            read_control(&mut truncated.as_slice(), &skipped)
                .await
                .is_err()
        );
        assert_eq!(skipped.load(Ordering::Relaxed), 2);
    }
}
