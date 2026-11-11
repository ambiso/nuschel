// SPDX-License-Identifier: EUPL-1.2
//! The authoritative server state: channels, connected users, registrations
//! and bans, behind one mutex. Control handlers and the voice path take it
//! briefly and never across an `await`; outgoing frames go to per-connection
//! queues ([`Link`]) without blocking.
use crate::{
    acl::{self, AclEntry, Group, Node, Subject, Tree, perm},
    config::Settings,
    conversations::{self, Conversations, Id},
    limits::{BandwidthRecord, LeakyBucket},
    storage::StorageHandle,
    text::utf16_len,
};
use bytes::Bytes;
use nuschel_ipc::server::{
    HistoryVisibility, StoredAcl, StoredBan, StoredChannel, StoredGroup, StoredUser,
};
use nuschel_protocol::{control as proto, crypt::CryptState, encode_control, kind};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, mpsc};

/// Voice frames are dropped (not queued) once this much is waiting for a client.
const VOICE_BACKLOG: usize = 64 * 1024;
/// Blobs (comments, descriptions, avatars) at least this long are announced by
/// hash and fetched with `RequestBlob`, as Murmur does.
pub const BLOB_HASH_THRESHOLD: usize = 128;

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

pub fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    // The system RNG failing is unrecoverable for a TLS server anyway.
    if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).is_err() {
        panic!("system random number generator failed");
    }
    bytes
}

pub fn sha1(bytes: &[u8]) -> [u8; 20] {
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, bytes);
    let mut out = [0; 20];
    out.copy_from_slice(digest.as_ref());
    out
}

/// A blob hash when the blob is long enough to be fetched on demand.
pub fn blob_hash(text_length: usize, bytes: &[u8]) -> Option<[u8; 20]> {
    (text_length >= BLOB_HASH_THRESHOLD).then(|| sha1(bytes))
}

/// The queue to one client's TLS writer. Control frames are never dropped: a
/// client that lets more than the configured limit pile up is disconnected.
/// Voice frames are dropped once a small backlog builds up.
#[derive(Clone)]
pub struct Link {
    sender: mpsc::UnboundedSender<Bytes>,
    queued: Arc<AtomicUsize>,
    limit: usize,
    pub signal: Arc<LinkSignal>,
}

#[derive(Default)]
pub struct LinkSignal {
    /// Wakes the connection task to close (gracefully unless `overflowed`).
    pub close: Notify,
    pub overflowed: AtomicBool,
}

impl Link {
    pub fn new(limit: usize) -> (Self, mpsc::UnboundedReceiver<Bytes>, Arc<AtomicUsize>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        (
            Self {
                sender,
                queued: queued.clone(),
                limit,
                signal: Arc::new(LinkSignal::default()),
            },
            receiver,
            queued,
        )
    }

    pub fn send(&self, frame: Bytes) {
        let length = frame.len();
        if self.queued.fetch_add(length, Ordering::Relaxed) + length > self.limit {
            self.queued.fetch_sub(length, Ordering::Relaxed);
            if !self.signal.overflowed.swap(true, Ordering::Relaxed) {
                self.signal.close.notify_one();
            }
            return;
        }
        if self.sender.send(frame).is_err() {
            self.queued.fetch_sub(length, Ordering::Relaxed);
        }
    }

    pub fn send_voice(&self, frame: Bytes) {
        if self.queued.load(Ordering::Relaxed) + frame.len() > VOICE_BACKLOG {
            return;
        }
        self.send(frame);
    }

    pub fn message(&self, kind: u16, message: &impl prost::Message) {
        match encode_control(kind, message) {
            Some(frame) => self.send(frame),
            None => tracing::warn!(kind, "dropped a message larger than clients accept"),
        }
    }

    /// Ask the connection to flush what is queued and close.
    pub fn close(&self) {
        self.signal.close.notify_one();
    }
}

#[derive(Clone, Debug, Default)]
pub struct ClientInfo {
    /// In the 1.5 format (`major << 48 | minor << 32 | patch << 16`); 0 unknown.
    pub version: u64,
    pub release: String,
    pub os: String,
    pub os_version: String,
}

impl ClientInfo {
    /// The one place a `Version` message is interpreted, before and after
    /// authentication. The Nuschel extension's hello (field 7200) will be
    /// decoded here too (docs/extension.md).
    pub fn apply(&mut self, message: &proto::Version) {
        self.version = match (message.version_v2, message.version_v1) {
            (Some(v2), _) => v2,
            (None, Some(v1)) => {
                let (major, minor, patch) = (v1 >> 16, (v1 >> 8) & 0xff, v1 & 0xff);
                (u64::from(major) << 48) | (u64::from(minor) << 32) | (u64::from(patch) << 16)
            }
            (None, None) => 0,
        };
        let bounded = |text: &Option<String>, limit: usize| match text {
            Some(text) if text.len() > limit => "[[Invalid]]".to_owned(),
            Some(text) => text.clone(),
            None => String::new(),
        };
        if message.release.is_some() {
            self.release = bounded(&message.release, 100);
        }
        if message.os.is_some() {
            self.os = bounded(&message.os, 40);
            if message.os_version.is_some() {
                self.os_version = bounded(&message.os_version, 60);
            }
        }
    }

    pub fn at_least(&self, major: u64, minor: u64, patch: u64) -> bool {
        self.version >= (major << 48) | (minor << 32) | (patch << 16)
    }

    pub fn to_proto(&self) -> proto::Version {
        let legacy = |version: u64| {
            (((version >> 48) as u32) << 16)
                | (((version >> 32) & 0xffff).min(255) as u32 * 256)
                | (((version >> 16) & 0xffff).min(255) as u32)
        };
        proto::Version {
            version_v1: (self.version != 0).then(|| legacy(self.version)),
            version_v2: (self.version != 0).then_some(self.version),
            release: (!self.release.is_empty()).then(|| self.release.clone()),
            os: (!self.os.is_empty()).then(|| self.os.clone()),
            os_version: (!self.os_version.is_empty()).then(|| self.os_version.clone()),
        }
    }
}

/// What a connection learned before authenticating.
pub struct Peer {
    pub address: SocketAddr,
    /// Leaf first, DER.
    pub certificates: Vec<Vec<u8>>,
    pub certificate_hash: Option<String>,
    /// The Nuschel extension's negotiation (frozen at `Authenticate`).
    pub negotiation: crate::extension::Negotiation,
    pub info: ClientInfo,
    pub link: Link,
}

#[derive(Clone, Debug, Default)]
pub struct WhisperTarget {
    pub sessions: Vec<u32>,
    /// Channel, children, links, group.
    pub channels: Vec<(u32, bool, bool, String)>,
}

/// A computed receiver list for one voice target, valid for one routing generation.
#[derive(Clone, Debug, Default)]
pub struct TargetCache {
    pub generation: u64,
    pub receivers: Vec<crate::routing::Receiver>,
}

pub struct Voice {
    pub crypt: CryptState,
    /// The UDP socket (index into the bound sockets) and address in use.
    pub udp: Option<(usize, SocketAddr)>,
    /// Murmur's `aiUdpFlag`: send voice over UDP until the client tunnels
    /// something over TCP, and again after it sends voice over UDP.
    pub udp_active: bool,
    pub last_good: Instant,
    pub last_resync_request: Option<Instant>,
    pub remote: nuschel_protocol::crypt::CryptStats,
    pub bandwidth: BandwidthRecord,
    pub udp_ping: (f32, f32, u32),
    pub tcp_ping: (f32, f32, u32),
    /// Decryption failures from the bound address; a flood pauses decryption.
    pub failures: LeakyBucket,
    pub blocked_until: Option<Instant>,
}

pub struct User {
    pub session: u32,
    pub name: String,
    pub user_id: Option<u32>,
    pub certificate_hash: Option<String>,
    pub certificates: Vec<Vec<u8>>,
    pub address: SocketAddr,
    /// The immutable account id of a registered user.
    pub account: Option<Id>,
    /// The Nuschel extension, when negotiated (docs/extension.md).
    pub extension: Option<Box<crate::extension::Extension>>,
    /// Channels whose retention this stock client was told about.
    pub notified: BTreeSet<u32>,
    pub info: ClientInfo,
    pub opus: bool,
    pub celt_versions: Vec<i32>,
    pub bot: bool,
    /// Access-token digests ([`acl::token_digest`]).
    pub tokens: Vec<[u8; 32]>,
    pub channel: u32,
    pub mute: bool,
    pub deaf: bool,
    pub suppress: bool,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub priority_speaker: bool,
    pub recording: bool,
    pub comment: String,
    pub comment_hash: Option<[u8; 20]>,
    pub texture: Bytes,
    pub texture_hash: Option<[u8; 20]>,
    pub plugin_context: Vec<u8>,
    pub plugin_identity: String,
    /// Channel → volume adjustment.
    pub listening: BTreeMap<u32, f32>,
    pub targets: HashMap<u8, WhisperTarget>,
    pub target_cache: RefCell<HashMap<u8, TargetCache>>,
    pub voice: Voice,
    pub link: Link,
    /// Murmur's `messagelimit`/`messageburst` bucket for the messages Murmur limits.
    pub rate: LeakyBucket,
    pub plugin_rate: LeakyBucket,
    /// A generous bound on all other control messages, against floods.
    pub flood: LeakyBucket,
    /// Queries that evaluate permissions or send data back.
    pub queries: LeakyBucket,
    pub permission_sent: HashMap<u32, u32>,
    pub last_permission_check: Option<u32>,
    pub last_activity: Instant,
}

impl User {
    pub fn subject(&self) -> Subject<'_> {
        Subject {
            user_id: self.user_id,
            channel: self.channel,
            hash: self.certificate_hash.as_deref(),
            tokens: &self.tokens,
            strong: false,
        }
    }

    /// The full `UserState` another client receives (blobs by hash when long).
    pub fn state_message(&self) -> proto::UserState {
        let mut message = proto::UserState {
            session: Some(self.session),
            name: Some(self.name.clone()),
            user_id: self.user_id,
            channel_id: Some(self.channel),
            ..Default::default()
        };
        if let Some(hash) = self.texture_hash {
            message.texture_hash = Some(hash.to_vec());
        } else if !self.texture.is_empty() {
            message.texture = Some(self.texture.to_vec());
        }
        if self.deaf {
            message.deaf = Some(true);
        } else if self.mute {
            message.mute = Some(true);
        }
        if self.suppress {
            message.suppress = Some(true);
        }
        if self.priority_speaker {
            message.priority_speaker = Some(true);
        }
        if self.recording {
            message.recording = Some(true);
        }
        if self.self_deaf {
            message.self_deaf = Some(true);
        } else if self.self_mute {
            message.self_mute = Some(true);
        }
        if let Some(hash) = self.comment_hash {
            message.comment_hash = Some(hash.to_vec());
        } else if !self.comment.is_empty() {
            message.comment = Some(self.comment.clone());
        }
        message.hash = self.certificate_hash.clone();
        message.listening_channel_add = self.listening.keys().copied().collect();
        message
    }

    pub fn set_comment(&mut self, comment: String) {
        self.comment_hash = blob_hash(utf16_len(&comment), comment.as_bytes());
        self.comment = comment;
    }

    pub fn set_texture(&mut self, texture: Bytes) {
        self.texture_hash = blob_hash(texture.len(), &texture);
        self.texture = texture;
    }
}

pub struct Channel {
    pub id: u32,
    pub node: Node,
    pub children: Vec<u32>,
    pub name: String,
    pub description: String,
    pub description_hash: Option<[u8; 20]>,
    pub position: i32,
    pub max_users: u32,
    pub temporary: bool,
    pub links: BTreeSet<u32>,
    /// Sessions in this channel.
    pub users: Vec<u32>,
    /// Sessions listening to this channel.
    pub listeners: BTreeSet<u32>,
    pub conversation_id: [u8; 16],
    pub history_visibility: HistoryVisibility,
}

impl Channel {
    pub fn new(id: u32, parent: Option<u32>, name: String) -> Self {
        Self {
            id,
            node: Node {
                parent,
                inherit_acl: true,
                acls: Vec::new(),
                groups: BTreeMap::new(),
            },
            children: Vec::new(),
            name,
            description: String::new(),
            description_hash: None,
            position: 0,
            max_users: 0,
            temporary: false,
            links: BTreeSet::new(),
            users: Vec::new(),
            listeners: BTreeSet::new(),
            conversation_id: random(),
            history_visibility: HistoryVisibility::Members,
        }
    }

    pub fn parent(&self) -> Option<u32> {
        self.node.parent
    }

    pub fn set_description(&mut self, description: String) {
        self.description_hash = blob_hash(utf16_len(&description), description.as_bytes());
        self.description = description;
    }

    pub fn is_enter_restricted(&self) -> bool {
        self.node.acls.iter().any(|acl| acl.deny & perm::ENTER != 0)
    }

    pub fn to_stored(&self) -> StoredChannel {
        StoredChannel {
            id: self.id,
            parent: self.node.parent,
            name: self.name.clone(),
            description: self.description.clone(),
            position: self.position,
            max_users: self.max_users,
            inherit_acl: self.node.inherit_acl,
            conversation_id: conversations::hex(&self.conversation_id),
            history_visibility: self.history_visibility,
            acls: self
                .node
                .acls
                .iter()
                .map(|acl| StoredAcl {
                    apply_here: acl.apply_here,
                    apply_subs: acl.apply_subs,
                    user_id: acl.user_id,
                    group: acl.user_id.is_none().then(|| acl.group.clone()),
                    grant: acl.grant,
                    deny: acl.deny,
                })
                .collect(),
            groups: self
                .node
                .groups
                .iter()
                .map(|(name, group)| StoredGroup {
                    name: name.clone(),
                    inherit: group.inherit,
                    inheritable: group.inheritable,
                    add: group.add.iter().copied().collect(),
                    remove: group.remove.iter().copied().collect(),
                })
                .collect(),
        }
    }

    pub fn from_stored(stored: StoredChannel) -> Self {
        let mut channel = Self::new(stored.id, stored.parent, stored.name);
        channel.set_description(stored.description);
        channel.position = stored.position;
        channel.max_users = stored.max_users;
        channel.node.inherit_acl = stored.inherit_acl;
        channel.history_visibility = stored.history_visibility;
        if let Some(id) = conversations::parse(&stored.conversation_id) {
            channel.conversation_id = id;
        }
        channel.node.acls = stored
            .acls
            .into_iter()
            .map(|acl| AclEntry {
                apply_here: acl.apply_here,
                apply_subs: acl.apply_subs,
                user_id: acl.user_id,
                group: acl.group.unwrap_or_default(),
                grant: acl.grant & perm::ALL,
                deny: acl.deny & perm::ALL,
            })
            .collect();
        channel.node.groups = stored
            .groups
            .into_iter()
            .map(|group| {
                (
                    group.name,
                    Group {
                        inherit: group.inherit,
                        inheritable: group.inheritable,
                        add: group.add.into_iter().collect(),
                        remove: group.remove.into_iter().collect(),
                    },
                )
            })
            .collect();
        channel
    }
}

#[derive(Default)]
pub struct Channels(pub HashMap<u32, Channel>);

impl Tree for Channels {
    fn node(&self, channel: u32) -> Option<&Node> {
        self.0.get(&channel).map(|channel| &channel.node)
    }
}

impl Channels {
    pub fn get(&self, id: u32) -> Option<&Channel> {
        self.0.get(&id)
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut Channel> {
        self.0.get_mut(&id)
    }

    /// Depth of `id` (root is 0).
    pub fn level(&self, id: u32) -> usize {
        let mut level = 0;
        let mut current = self.get(id).and_then(Channel::parent);
        while let Some(parent) = current {
            level += 1;
            if level > 4096 {
                break;
            }
            current = self.get(parent).and_then(Channel::parent);
        }
        level
    }

    /// Height of the subtree under `id` (a leaf is 0), Murmur's `getDepth`.
    pub fn height(&self, id: u32) -> usize {
        let mut height = 0;
        let mut level = vec![id];
        while !level.is_empty() && height < 4096 {
            level = level
                .iter()
                .filter_map(|id| self.get(*id))
                .flat_map(|channel| channel.children.iter().copied())
                .collect();
            if !level.is_empty() {
                height += 1;
            }
        }
        height
    }

    pub fn is_ancestor(&self, ancestor: u32, mut id: u32) -> bool {
        for _ in 0..4096 {
            if id == ancestor {
                return true;
            }
            match self.get(id).and_then(Channel::parent) {
                Some(parent) => id = parent,
                None => return false,
            }
        }
        false
    }

    /// `id` and every channel below it, parents before children.
    pub fn subtree(&self, id: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut queue = VecDeque::from([id]);
        while let Some(current) = queue.pop_front() {
            if let Some(channel) = self.get(current) {
                out.push(current);
                queue.extend(channel.children.iter().copied());
            }
        }
        out
    }

    /// Every channel reachable over links, including `id`.
    pub fn all_links(&self, id: u32) -> BTreeSet<u32> {
        let mut seen = BTreeSet::from([id]);
        let mut queue = vec![id];
        while let Some(current) = queue.pop() {
            if let Some(channel) = self.get(current) {
                for link in &channel.links {
                    if seen.insert(*link) {
                        queue.push(*link);
                    }
                }
            }
        }
        seen
    }
}

#[derive(Clone, Debug)]
pub struct Registered {
    pub id: u32,
    /// Immutable and never reused; the registration number is only an alias.
    pub account: Id,
    pub name: String,
    pub password_hash: Option<String>,
    pub certificate_hash: Option<String>,
    pub last_channel: Option<u32>,
    pub last_active: Option<i64>,
}

impl Registered {
    pub fn to_stored(&self) -> StoredUser {
        StoredUser {
            id: self.id,
            account: Some(conversations::hex(&self.account)),
            name: self.name.clone(),
            password_hash: self.password_hash.clone(),
            certificate_hash: self.certificate_hash.clone(),
            last_channel: self.last_channel,
            last_active: self.last_active,
        }
    }
}

/// Registered users, without their blobs (loaded when they connect).
#[derive(Default)]
pub struct Registry {
    pub users: BTreeMap<u32, Registered>,
    by_name: HashMap<String, u32>,
    by_account: HashMap<Id, u32>,
    /// Registration ids are never reused, even after an account is deleted:
    /// ACL entries and group memberships refer to them.
    pub next_id: u32,
}

impl Registry {
    pub fn insert(&mut self, user: Registered) {
        if let Some(old) = self.users.get(&user.id) {
            self.by_name.remove(&old.name.to_lowercase());
            self.by_account.remove(&old.account);
        }
        self.by_name.insert(user.name.to_lowercase(), user.id);
        self.by_account.insert(user.account, user.id);
        self.users.insert(user.id, user);
    }

    pub fn remove(&mut self, id: u32) -> Option<Registered> {
        let user = self.users.remove(&id)?;
        self.by_name.remove(&user.name.to_lowercase());
        self.by_account.remove(&user.account);
        Some(user)
    }

    pub fn by_account(&self, account: &Id) -> Option<&Registered> {
        self.by_account
            .get(account)
            .and_then(|id| self.users.get(id))
    }

    pub fn by_name(&self, name: &str) -> Option<&Registered> {
        self.by_name
            .get(&name.to_lowercase())
            .and_then(|id| self.users.get(id))
    }

    pub fn by_certificate(&self, hash: &str) -> Option<&Registered> {
        // Registration is rare and the list is in memory; a linear scan avoids
        // keeping a second index consistent. Lowest id wins, as in Murmur's query.
        self.users
            .values()
            .find(|user| user.certificate_hash.as_deref() == Some(hash))
    }

    /// Take the next registration id (the caller persists the new high mark).
    pub fn allocate_id(&mut self) -> u32 {
        let id = self
            .users
            .keys()
            .next_back()
            .map_or(1, |id| id.saturating_add(1))
            .max(self.next_id)
            .max(1);
        self.next_id = id.saturating_add(1);
        id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ban {
    pub address: Option<Ipv6Addr>,
    /// Prefix length on 128 bits (IPv4 is mapped).
    pub mask: u8,
    pub name: String,
    pub hash: String,
    pub reason: String,
    pub start: i64,
    pub duration: u32,
}

pub fn mapped(address: IpAddr) -> Ipv6Addr {
    match address {
        IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        IpAddr::V6(v6) => v6,
    }
}

impl Ban {
    pub fn matches_address(&self, address: IpAddr) -> bool {
        let Some(banned) = self.address else {
            return false;
        };
        let mask = u32::from(self.mask.min(128));
        if mask == 0 {
            return false;
        }
        let bits = u128::MAX << (128 - mask);
        (u128::from(banned) & bits) == (u128::from(mapped(address)) & bits)
    }

    pub fn expired(&self, now: i64) -> bool {
        self.duration != 0 && now >= self.start.saturating_add(i64::from(self.duration))
    }

    pub fn valid(&self) -> bool {
        self.address.is_some() || !self.hash.is_empty()
    }

    pub fn to_stored(&self) -> StoredBan {
        StoredBan {
            address: self.address.map(|address| address.to_string()),
            mask: self.mask,
            name: self.name.clone(),
            hash: self.hash.clone(),
            reason: self.reason.clone(),
            start: self.start,
            duration: self.duration,
        }
    }

    pub fn from_stored(stored: StoredBan) -> Self {
        Self {
            address: stored
                .address
                .and_then(|address| address.parse::<IpAddr>().ok())
                .map(mapped),
            mask: stored.mask.min(128),
            name: stored.name,
            hash: stored.hash,
            reason: stored.reason,
            start: stored.start,
            duration: stored.duration,
        }
    }
}

/// Things worth counting for operators.
#[derive(Default)]
pub struct Counters {
    /// Control frames in the Nuschel extension's reserved range, ignored until
    /// phase 2 handles them.
    pub extension_frames: AtomicU64,
    pub unknown_frames: AtomicU64,
    pub rate_limited: AtomicU64,
}

pub struct State {
    pub settings: Arc<Settings>,
    pub channels: Channels,
    pub users: HashMap<u32, User>,
    pub registry: Registry,
    pub bans: Vec<Ban>,
    pub udp_peers: HashMap<SocketAddr, u32>,
    pub storage: StorageHandle,
    pub counters: Arc<Counters>,
    /// (session, channel) → permissions; cleared whenever anything that
    /// permissions depend on changes.
    cache: RefCell<HashMap<(u32, u32), u32>>,
    /// Bumped with the cache; voice target caches compare against it.
    pub generation: u64,
    next_session: u32,
    pub scratch: crate::routing::Scratch,
    /// Temporary channels that may have become empty (see `State::finish`).
    pub emptied: Vec<u32>,
    /// Authenticated sessions by the address of their TLS connection, to bind
    /// UDP only to the address that authenticated.
    pub hosts: HashMap<IpAddr, Vec<u32>>,
    /// Durable conversations, subscriptions, access intervals, read markers.
    pub conversations: Conversations,
    /// History writes and page reads (docs/extension.md §4, §6).
    pub history: crate::history::Queues,
    /// Counts changes to authorisation state; history pages in flight are
    /// re-checked when it moved.
    pub authz_epoch: u64,
    pub authz_pending: conversations::Pending,
    /// History pages per network and account (page_limits.rs).
    pub page_limits: crate::page_limits::NetworkLimits,

    /// The key-transparency directory's queue, when `kt/1` is offered.
    pub kt: Option<crate::kt::KtHandle>,
}

impl State {
    pub fn new(settings: Arc<Settings>, storage: StorageHandle, counters: Arc<Counters>) -> Self {
        Self {
            page_limits: crate::page_limits::NetworkLimits::new(&settings.config.history),
            settings,
            channels: Channels::default(),
            users: HashMap::new(),
            registry: Registry::default(),
            bans: Vec::new(),
            udp_peers: HashMap::new(),
            storage,
            counters,
            cache: RefCell::new(HashMap::new()),
            generation: 0,
            next_session: 1,
            scratch: Default::default(),
            emptied: Vec::new(),
            hosts: HashMap::new(),
            conversations: Conversations::default(),
            history: crate::history::Queues::detached(),
            authz_epoch: 0,
            authz_pending: Default::default(),
            kt: None,
        }
    }

    /// Fresh databases get Murmur's initial state: a root channel, the `admin`
    /// group, and ACLs granting admins Write, registered users temporary
    /// channels and everyone self-registration.
    pub fn initialise_root(&mut self) {
        if self.channels.get(0).is_some() {
            return;
        }
        let mut root = Channel::new(0, None, "Root".into());
        root.node.groups.insert("admin".into(), Group::new());
        let entry = |group: &str, grant| AclEntry {
            apply_here: true,
            apply_subs: true,
            user_id: None,
            group: group.into(),
            grant,
            deny: 0,
        };
        root.node.acls = vec![
            entry("admin", perm::WRITE),
            entry("auth", perm::MAKE_TEMP_CHANNEL),
            entry("all", perm::SELF_REGISTER),
        ];
        self.conversations.add_channel(root.conversation_id, 0);
        self.channels.0.insert(0, root);
        self.persist_channel(0);
        if !self.registry.users.contains_key(&0) {
            self.registry.insert(Registered {
                id: 0,
                account: random(),
                name: "SuperUser".into(),
                password_hash: None,
                certificate_hash: None,
                last_channel: None,
                last_active: None,
            });
            self.persist_registration(0);
        }
    }

    pub fn allocate_session(&mut self) -> Option<u32> {
        for _ in 0..=self.users.len() {
            let session = self.next_session;
            self.next_session = if self.next_session >= i32::MAX as u32 {
                1
            } else {
                self.next_session + 1
            };
            if !self.users.contains_key(&session) {
                return Some(session);
            }
        }
        None
    }

    // ---- permissions ----

    pub fn invalidate(&mut self) {
        self.cache.get_mut().clear();
        self.generation += 1;
        self.authz_epoch += 1;
    }

    pub fn permissions(&self, session: u32, channel: u32) -> u32 {
        if let Some(&cached) = self.cache.borrow().get(&(session, channel)) {
            return cached;
        }
        let Some(user) = self.users.get(&session) else {
            return perm::NONE;
        };
        let granted = acl::effective_permissions(&self.channels, &user.subject(), channel);
        self.cache.borrow_mut().insert((session, channel), granted);
        granted
    }

    pub fn has(&self, session: u32, channel: u32, permission: u32) -> bool {
        self.permissions(session, channel) & permission != 0
    }

    /// Murmur's `PERM_DENIED`: tell the client which permission it lacks.
    pub fn deny(&self, to: u32, who: u32, channel: u32, permission: u32) {
        if let Some(user) = self.users.get(&to) {
            user.link.message(
                kind::PERMISSION_DENIED,
                &proto::PermissionDenied {
                    permission: Some(permission),
                    channel_id: Some(channel),
                    session: Some(who),
                    r#type: Some(proto::permission_denied::DenyType::Permission as i32),
                    ..Default::default()
                },
            );
        }
    }

    pub fn deny_type(&self, to: u32, deny: proto::permission_denied::DenyType) {
        self.deny_with(to, deny, None, None);
    }

    pub fn deny_with(
        &self,
        to: u32,
        deny: proto::permission_denied::DenyType,
        reason: Option<&str>,
        session: Option<u32>,
    ) {
        if let Some(user) = self.users.get(&to) {
            user.link.message(
                kind::PERMISSION_DENIED,
                &proto::PermissionDenied {
                    r#type: Some(deny as i32),
                    reason: reason.map(str::to_owned),
                    session,
                    ..Default::default()
                },
            );
        }
    }

    /// Send `PermissionQuery` for `channel` if it changed since last sent (or
    /// always when the client asked).
    pub fn send_permissions(&mut self, session: u32, channel: u32, requested: bool) {
        let granted = self.permissions(session, channel);
        let Some(user) = self.users.get_mut(&session) else {
            return;
        };
        if user.user_id == Some(0) {
            return;
        }
        if requested {
            user.last_permission_check = Some(channel);
        }
        if requested || user.permission_sent.get(&channel) != Some(&granted) {
            user.permission_sent.insert(channel, granted);
            user.link.message(
                kind::PERMISSION_QUERY,
                &proto::PermissionQuery {
                    channel_id: Some(channel),
                    permissions: Some(granted),
                    flush: None,
                },
            );
        }
    }

    /// After permissions may have changed for `who` (or everyone): Murmur's
    /// `clearACLCache`. Stale permission caches in clients are flushed, and
    /// suppression follows the Speak permission in the user's channel.
    pub fn permissions_changed(&mut self, who: Option<u32>) {
        self.invalidate();
        let sessions: Vec<u32> = match who {
            Some(session) => vec![session],
            None => self.users.keys().copied().collect(),
        };
        for session in sessions {
            self.flush_permission_cache(session);
            let Some(user) = self.users.get(&session) else {
                continue;
            };
            let may_speak = self.has(session, user.channel, perm::SPEAK);
            if may_speak == user.suppress {
                if let Some(user) = self.users.get_mut(&session) {
                    user.suppress = !may_speak;
                }
                self.broadcast(
                    kind::USER_STATE,
                    &proto::UserState {
                        session: Some(session),
                        suppress: Some(!may_speak),
                        ..Default::default()
                    },
                    None,
                );
            }
        }
    }

    fn flush_permission_cache(&mut self, session: u32) {
        let Some(user) = self.users.get(&session) else {
            return;
        };
        let sent: Vec<(u32, u32)> = user.permission_sent.iter().map(|(c, p)| (*c, *p)).collect();
        let matches = sent.len() < 20
            && sent.iter().all(|(channel, granted)| {
                self.channels.get(*channel).is_some()
                    && self.permissions(session, *channel) == *granted
            });
        if matches {
            return;
        }
        let channel = user
            .last_permission_check
            .filter(|channel| self.channels.get(*channel).is_some())
            .unwrap_or(user.channel);
        let granted = self.permissions(session, channel);
        if let Some(user) = self.users.get_mut(&session) {
            user.permission_sent.clear();
            user.permission_sent.insert(channel, granted);
            user.last_permission_check = Some(channel);
            user.link.message(
                kind::PERMISSION_QUERY,
                &proto::PermissionQuery {
                    channel_id: Some(channel),
                    permissions: Some(granted),
                    flush: Some(true),
                },
            );
        }
    }

    pub fn channel_full(&self, channel: u32, session: Option<u32>) -> bool {
        if session.is_some_and(|session| self.has(session, channel, perm::WRITE)) {
            return false;
        }
        let Some(channel) = self.channels.get(channel) else {
            return true;
        };
        let limit = if channel.max_users != 0 {
            channel.max_users
        } else {
            self.settings.config.max_users_per_channel
        };
        limit != 0 && channel.users.len() >= limit as usize
    }

    // ---- sending ----

    /// Encode once and queue for every authenticated user (except one).
    /// Phase 2: connections that negotiated the Nuschel extension may get a
    /// second encoding with extension fields; stock clients never do.
    pub fn broadcast(&self, kind: u16, message: &impl prost::Message, except: Option<u32>) {
        let Some(frame) = encode_control(kind, message) else {
            tracing::warn!(kind, "dropped a broadcast larger than clients accept");
            return;
        };
        for user in self.users.values() {
            if Some(user.session) != except {
                user.link.send(frame.clone());
            }
        }
    }

    pub fn send(&self, session: u32, kind: u16, message: &impl prost::Message) {
        if let Some(user) = self.users.get(&session) {
            user.link.message(kind, message);
        }
    }

    pub fn text_to(&self, session: u32, text: &str) {
        self.send(
            session,
            kind::TEXT_MESSAGE,
            &proto::TextMessage {
                message: text.into(),
                ..Default::default()
            },
        );
    }

    pub fn channel_state(&self, channel: &Channel, viewer: u32) -> proto::ChannelState {
        let mut message = proto::ChannelState {
            channel_id: Some(channel.id),
            parent: channel.parent(),
            name: Some(
                if channel.id == 0 && !self.settings.config.registered_name.is_empty() {
                    self.settings.config.registered_name.clone()
                } else {
                    channel.name.clone()
                },
            ),
            position: Some(channel.position),
            max_users: Some(channel.max_users),
            is_enter_restricted: Some(channel.is_enter_restricted()),
            can_enter: Some(self.has(viewer, channel.id, perm::ENTER)),
            ..Default::default()
        };
        if let Some(hash) = channel.description_hash {
            message.description_hash = Some(hash.to_vec());
        } else if !channel.description.is_empty() {
            message.description = Some(channel.description.clone());
        }
        message
    }

    // ---- persistence (write-through, never awaited on the control path) ----

    pub fn persist_channel(&self, id: u32) {
        if let Some(channel) = self.channels.get(id).filter(|channel| !channel.temporary) {
            self.storage.save_channel(channel.to_stored());
        }
    }

    pub fn persist_registration(&self, id: u32) {
        if let Some(user) = self.registry.users.get(&id) {
            self.storage.save_user(user.to_stored());
        }
    }

    pub fn persist_bans(&self) {
        self.storage
            .save_bans(self.bans.iter().map(Ban::to_stored).collect());
    }

    /// Remember a registered user's channel (Murmur's `setLastChannel`).
    pub fn remember_channel(&mut self, session: u32) {
        let Some(user) = self.users.get(&session) else {
            return;
        };
        let (Some(id), channel) = (user.user_id, user.channel) else {
            return;
        };
        if self
            .channels
            .get(channel)
            .is_some_and(|channel| channel.temporary)
        {
            return;
        }
        if let Some(registered) = self.registry.users.get_mut(&id) {
            registered.last_channel = Some(channel);
            registered.last_active = Some(now_unix());
        }
        self.persist_registration(id);
    }

    pub fn log_name(&self, session: u32) -> String {
        self.users.get(&session).map_or_else(
            || format!("session {session}"),
            |user| {
                format!(
                    "{}:{}({})",
                    user.name,
                    user.session,
                    user.user_id.map_or(-1, i64::from)
                )
            },
        )
    }
}
