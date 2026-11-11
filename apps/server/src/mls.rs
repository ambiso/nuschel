// SPDX-License-Identifier: EUPL-1.2
//! The MLS delivery service (`mls/1`, docs/e2ee.md §6.3).
//!
//! - **Groups.** One per conversation (generation 0 for now). The service task owns
//!   each group's public state (nuschel-e2ee's `DeliveryGroup`, OpenMLS's
//!   `PublicGroup`), validates commits (structure and every key first, then
//!   signatures and proposal rules, the new tree, the committer's GroupInfo, that
//!   the Welcome goes to exactly the added key packages) and accepts one commit per
//!   epoch. All MLS work happens in the task, never under the state lock, and all
//!   of it goes through nuschel-e2ee (prevalidation, `catch_unwind`).
//! - **One stream.** Commits and application messages are history rows of the
//!   conversation (formats `MLS_HANDSHAKE`, `MLS_APPLICATION`; the message as base64
//!   in `content`), so their `seq` is assigned under the state lock with everything
//!   else, and pages replay them in order. The state keeps each group's current
//!   epoch ([`GroupCache`]) and fences application messages with it when it assigns
//!   their `seq`, so every application message of epoch N comes before the commit
//!   that ends N.
//! - **A commit** is validated and applied in the task, then stored as a row; only
//!   when the row is stored is it acknowledged, fanned out and the group's state
//!   saved, with the Welcome put in each new device's mailbox (and pushed to its
//!   connection). If storing fails, the group goes back to its previous state.
//!   While a commit waits for storage, other commits for the group are `BUSY`.
//! - **Who may.** Registered accounts with a key-transparency binding. Key packages
//!   only for the uploader's own devices (credential identity and device in the
//!   account's current record); claims for one's own account and direct-conversation
//!   partners, rate-limited per account; groups of a direct conversation hold only
//!   the two accounts' identities' devices.
//! - **Channels** (phase 4c, docs/e2ee.md §6.2). A reader of the channel creates its
//!   group (alone) or joins it by external commit; the joiner's credential must be
//!   its own account's, in that account's key-transparency record. Members never add
//!   anyone; the server removes devices by external proposals signed with its
//!   key-transparency signing key, the groups' only external sender: when the
//!   account can no longer read the channel, when the device was not seen for 90
//!   days (any group), or did not open the channel for 30 days. The proposals are
//!   rows of the stream; then one online member device is asked to commit them
//!   (`MlsTask`), the next one if it does not within [`TASK_TIMEOUT_MS`]. Who is a
//!   member is kept per group (device → account), and when each device was last
//!   seen and opened each channel (`MlsOpened`, sends and commits), against a clock
//!   tests can move ([`MlsHandle::advance_clock`]).
//! - **Persistence** is the server's key-value table (`kt_records`) through its own
//!   storage worker, under uppercase prefixes (nuschel-kt uses lowercase ones, the
//!   key-transparency module `R` and `C`):
//!
//!   | Key | Value |
//!   | --- | --- |
//!   | `E` ‖ group id | epoch (8), newest commit's seq (8), conversation (16) |
//!   | `G` ‖ group id | version 1, conversation, commit seq, recent commit keys, `DeliveryGroup::state` |
//!   | `P` ‖ account ‖ device ‖ time (8) ‖ n (4) ‖ ref | a one-time key package |
//!   | `L` ‖ account ‖ device | the device's last-resort key package |
//!   | `K` ‖ key package ref | account ‖ device ‖ last resort (1): where a Welcome goes |
//!   | `W` ‖ account ‖ device ‖ id (16) | a waiting Welcome (`v1::MlsWelcome`) |
//!   | `N` ‖ group id | a commit waiting for its row: the group after it ([`pending_record`]) |
//!   | `A` ‖ account ‖ device | when the device was last seen (Unix ms, 8) |
//!   | `O` ‖ conversation ‖ device | when the device last opened the conversation (Unix ms, 8) |
//!
//!   `E` carries, after its first 32 bytes, whether the group is a channel's (1) and
//!   its members: count (2) and device ‖ account (32 each); `G` (version 2) the same
//!   member list after the recent commit keys.
//!
//!   An empty value deletes a key.
//!
//! **Crash safety.** A commit's new state is written as `N` before its row is
//! queued; once the row is stored, `G`, `E`, the Welcomes and the deletion of `N`
//! are one batch. On start, each `N` left behind is resolved against the stream:
//! adopted if the conversation has an `MLS_HANDSHAKE` row with that commit after
//! the saved commit seq (the server crashed between the two writes), deleted
//! otherwise (it crashed before the row was stored, and the committer, never
//! acknowledged, retries). So the server's view never falls behind its stream.
use crate::{
    authz::Action,
    config::Mls as MlsConfig,
    conversations::{Id, Kind, hex},
    history::{Delivery, Done},
    kt::StorageKt,
    limits::LeakyBucket,
    server::Server,
    state::State,
    storage::StorageHandle,
};
use base64::Engine as _;
use nuschel_e2ee::{
    INACTIVE_DEVICE_DAYS, UNOPENED_CHANNEL_DAYS,
    ds::{DeliveryGroup, DsError, ServerSigner, check_key_package},
    prevalidate::{self, ContentType, WireFormat},
};
use nuschel_identity::{DeviceCertificate, DeviceCredential, IdentityFingerprint};
use nuschel_ipc::server::{
    Anchor, MessageKind, PageQuery, ServerStorageRequest as Request,
    ServerStorageResponse as Response, StoredMessage,
};
use nuschel_kt::KtStore;
use nuschel_protocol::{
    ControlFrame,
    nuschel::{
        self, MAX_MLS_APPLICATION_BYTES, MAX_MLS_DEVICE_PACKAGES, MAX_MLS_GROUP_INFO_BYTES,
        MAX_MLS_HANDSHAKE_BYTES, MAX_MLS_KEY_PACKAGE_BYTES, MAX_MLS_TREE_BYTES,
        MAX_MLS_UPLOAD_PACKAGES, MAX_MLS_WELCOME_BYTES, MAX_MLS_WELCOMES, is_mls_format, kind,
        v1::{self, error::Code},
    },
};
use prost::Message as _;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Instant,
};
use tokio::sync::mpsc;

/// Requests waiting for the task, server-wide.
const QUEUE: usize = 256;
/// Commit idempotency keys remembered per group.
const RECENT: usize = 64;
/// Welcome fetches and acknowledgements per account and minute.
const MAILBOX_PER_MINUTE: u32 = 60;
/// Accounts with a rate-limit bucket before the oldest are forgotten.
const MAX_BUCKETS: usize = 10_000;
const PREFIX_EPOCH: u8 = b'E';
const PREFIX_GROUP: u8 = b'G';
const PREFIX_PACKAGE: u8 = b'P';
const PREFIX_LAST_RESORT: u8 = b'L';
const PREFIX_REFERENCE: u8 = b'K';
const PREFIX_WELCOME: u8 = b'W';
const PREFIX_PENDING: u8 = b'N';
const PREFIX_SEEN: u8 = b'A';
const PREFIX_OPENED: u8 = b'O';
/// A member asked to commit the server's removals gets this long before the next
/// one is asked.
pub const TASK_TIMEOUT_MS: u64 = 30_000;
/// Activity records are rewritten at most this often per device.
const TOUCH_WRITE_MS: u64 = 3_600_000;
const DAY_MS: u64 = 86_400_000;
/// `P` ‖ account ‖ device ‖ time ‖ n, before the key package reference.
const PACKAGE_KEY_FIXED: usize = 1 + 16 + 16 + 8 + 4;
/// History pages read per pending record at start.
const MAX_RESOLVE_PAGES: usize = 1000;

type Device = [u8; 16];
/// Key-value writes (an empty value deletes).
type Records = Vec<(Vec<u8>, Vec<u8>)>;
/// A Welcome to push to a device's connections.
type Push = (Id, Device, v1::MlsWelcome);
type Failure = (Code, u64);

/// An MLS row keeps its message as base64 in the stored `content` (the history
/// table has no binary column; a later schema change is mechanical). Returns the
/// wire `content` and `mls`.
pub fn row_content(format: u8, content: String) -> (String, Vec<u8>) {
    if is_mls_format(i32::from(format)) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(content.as_bytes())
            .unwrap_or_default();
        (String::new(), bytes)
    } else {
        (content, Vec::new())
    }
}

fn base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    for part in parts {
        context.update(part);
    }
    let mut out = [0; 32];
    out.copy_from_slice(context.finish().as_ref());
    out
}

fn now_ms() -> u64 {
    crate::kt::now_ms()
}

fn key(prefix: u8, parts: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![prefix];
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}

/// The group id of a conversation's group (generation 0).
pub fn group_id(conversation: &Id) -> Vec<u8> {
    nuschel_e2ee::binding::group_id(conversation, 0)
}

// ---- the state's end ----

/// What the state knows of a conversation's group.
#[derive(Clone, Debug, Default)]
pub struct GroupCache {
    pub group_id: Vec<u8>,
    pub epoch: u64,
    /// The newest commit's seq.
    pub commit_seq: u64,
    /// Accepted by the service (a creation in flight is not yet).
    pub established: bool,
    /// A commit is being validated or stored.
    pub committing: bool,
    /// A channel's group (server-driven membership).
    pub channel: bool,
    /// Member devices and their accounts, as of the newest commit.
    pub members: BTreeMap<Device, Id>,
    /// The server's removals proposed in an epoch, waiting for a commit.
    pub proposed: Option<Proposed>,
}

/// Removals the server proposed, and who was asked to commit them.
#[derive(Clone, Debug, Default)]
pub struct Proposed {
    pub epoch: u64,
    pub devices: BTreeSet<Device>,
    /// Proposal rows stored so far, of `total`.
    pub stored: usize,
    pub total: usize,
    /// The connection asked, and when (server clock, ms).
    pub task: Option<(u32, u64)>,
    /// Connections asked already in this epoch.
    pub asked: BTreeSet<u32>,
}

/// When a device was last seen (or opened a conversation), and last written.
#[derive(Clone, Copy, Debug, Default)]
struct Touch {
    at: u64,
    written: u64,
}

/// The handlers' end: the queue, each group's epoch and the Welcome listeners.
pub struct MlsHandle {
    tasks: mpsc::Sender<Task>,
    stored: mpsc::UnboundedSender<StoredNotice>,
    pub groups: HashMap<Id, GroupCache>,
    /// (account, device) → connections that fetched its mailbox.
    listeners: HashMap<(Id, Device), BTreeSet<u32>>,
    /// Session → the (account, device) pairs it listens for, to forget them when
    /// it disconnects.
    listening: HashMap<u32, Vec<(Id, Device)>>,
    claims: HashMap<Id, LeakyBucket>,
    uploads: HashMap<Id, LeakyBucket>,
    mailboxes: HashMap<Id, LeakyBucket>,
    config: MlsConfig,
    /// (account, device) → last seen.
    seen: HashMap<(Id, Device), Touch>,
    /// (conversation, device) → last opened.
    opened: HashMap<(Id, Device), Touch>,
    /// Added to the system clock (tests move time with it).
    clock_offset: Arc<AtomicI64>,
}

impl MlsHandle {
    pub fn has_group(&self, conversation: &Id) -> bool {
        self.groups.contains_key(conversation)
    }

    /// The delivery service's clock (Unix ms).
    pub fn now_ms(&self) -> u64 {
        now_ms().saturating_add_signed(self.clock_offset.load(Ordering::Relaxed))
    }

    /// Moves the service's clock (tests: a device inactive for 90 days).
    #[doc(hidden)]
    pub fn advance_clock(&self, ms: i64) {
        self.clock_offset.fetch_add(ms, Ordering::Relaxed);
    }

    /// The device of a connection: the one whose mailbox it fetched.
    fn device_of(&self, session: u32) -> Option<(Id, Device)> {
        self.listening.get(&session)?.first().copied()
    }

    /// Records activity; returns the record to write when it is due.
    fn touch(
        map: &mut HashMap<(Id, Device), Touch>,
        prefix: u8,
        key: (Id, Device),
        now: u64,
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        let entry = map.entry(key).or_default();
        entry.at = entry.at.max(now);
        if now.saturating_sub(entry.written) < TOUCH_WRITE_MS {
            return None;
        }
        entry.written = now;
        Some((
            self::key(prefix, &[&key.0, &key.1]),
            now.to_be_bytes().to_vec(),
        ))
    }

    fn limited(&mut self, account: Id, bucket: Bucket) -> bool {
        let now = Instant::now();
        let (buckets, per_minute) = match bucket {
            Bucket::Claim => (&mut self.claims, self.config.claims_per_minute),
            Bucket::Upload => (&mut self.uploads, self.config.uploads_per_minute),
            Bucket::Mailbox => (&mut self.mailboxes, MAILBOX_PER_MINUTE),
        };
        if buckets.len() >= MAX_BUCKETS && !buckets.contains_key(&account) {
            buckets.clear();
        }
        buckets
            .entry(account)
            .or_insert_with(|| LeakyBucket::per_minute(per_minute, per_minute.div_ceil(2), now))
            .limited(now)
    }

    /// Push this device's Welcomes to `session`. At most a roster's worth of
    /// devices per account (the key-transparency record's limit) are listened for;
    /// beyond that the fetch is answered but nothing is pushed.
    fn listen(&mut self, session: u32, account: Id, device: Device, live: impl Fn(u32) -> bool) {
        if !self.listeners.contains_key(&(account, device)) {
            self.listeners.retain(|(owner, _), sessions| {
                if *owner == account {
                    sessions.retain(|session| live(*session));
                }
                !sessions.is_empty()
            });
            let devices = self
                .listeners
                .keys()
                .filter(|(owner, _)| *owner == account)
                .count();
            if devices >= nuschel::MAX_KT_DEVICES {
                return;
            }
        }
        if self
            .listeners
            .entry((account, device))
            .or_default()
            .insert(session)
        {
            self.listening
                .entry(session)
                .or_default()
                .push((account, device));
        }
    }

    /// A connection went away: it listens for nothing any more.
    pub fn disconnected(&mut self, session: u32) {
        for pair in self.listening.remove(&session).unwrap_or_default() {
            if let Some(sessions) = self.listeners.get_mut(&pair) {
                sessions.remove(&session);
                if sessions.is_empty() {
                    self.listeners.remove(&pair);
                }
            }
        }
    }

    /// A handle whose queue goes nowhere (fuzzing and unit tests).
    pub fn detached(config: MlsConfig) -> Self {
        Self {
            tasks: mpsc::channel(1).0,
            stored: mpsc::unbounded_channel().0,
            groups: HashMap::new(),
            listeners: HashMap::new(),
            listening: HashMap::new(),
            claims: HashMap::new(),
            uploads: HashMap::new(),
            mailboxes: HashMap::new(),
            config,
            seen: HashMap::new(),
            opened: HashMap::new(),
            clock_offset: Arc::new(AtomicI64::new(0)),
        }
    }
}

#[derive(Clone, Copy)]
enum Bucket {
    Claim,
    Upload,
    Mailbox,
}

/// What a connection asked for, authorised under the state lock.
pub enum Job {
    Upload {
        account: Id,
        upload: v1::MlsKeyPackageUpload,
    },
    Claim {
        target: Id,
    },
    Commit {
        account: Id,
        conversation: Id,
        /// The accounts whose identities may be in the group.
        allowed: Vec<Id>,
        commit: v1::MlsCommit,
    },
    Welcomes {
        account: Id,
        device: Device,
    },
    WelcomeAck {
        account: Id,
        device: Device,
        ids: Vec<Vec<u8>>,
    },
    JoinInfo {
        conversation: Id,
        tree: bool,
    },
    /// The server's removals of these devices (docs/e2ee.md §6.2).
    Propose {
        conversation: Id,
        devices: BTreeSet<Device>,
    },
    /// Activity records to write.
    Touch(Records),
}

pub struct Task {
    session: u32,
    request_id: u64,
    job: Job,
}

/// A stored (or failed) commit row, from the state to the service.
struct StoredNotice {
    group_id: Vec<u8>,
    seq: u64,
    ok: bool,
}

/// A commit the service applied, waiting for its row.
pub struct Accepted {
    conversation: Id,
    group_id: Vec<u8>,
    epoch: u64,
    commit: Vec<u8>,
    account: Id,
    key: [u8; 16],
    hash: [u8; 32],
    /// The members after the commit, and the devices it added.
    members: BTreeMap<Device, Id>,
    added: Vec<Device>,
    channel: bool,
}

pub enum Reply {
    Status(v1::MlsKeyPackageStatus),
    Packages(v1::MlsKeyPackages),
    Accepted(Box<Accepted>),
    /// A retried commit that was already accepted.
    Ack(v1::MlsAck),
    Stale {
        conversation: Id,
        epoch: u64,
    },
    Welcomes(v1::MlsWelcomes),
    Done,
    JoinInfo(v1::MlsJoinInfo),
    /// The server's removal proposals, for the stream.
    Proposed {
        conversation: Id,
        epoch: u64,
        devices: BTreeSet<Device>,
        rows: Vec<Vec<u8>>,
    },
    Failed(Failure),
    /// A commit refused before storage: its group is free again.
    CommitFailed {
        conversation: Id,
        failure: Failure,
    },
}

/// What to do once an MLS row is stored.
pub struct Stored {
    session: u32,
    request_id: u64,
    conversation: Id,
    seq: u64,
    epoch: u64,
    message: v1::Message,
    /// For a commit: the group, and its cache before the commit.
    commit: Option<CommitRollback>,
    /// The server's own proposal: nobody to acknowledge.
    proposal: bool,
}

struct CommitRollback {
    group_id: Vec<u8>,
    created: bool,
    epoch: u64,
    commit_seq: u64,
    /// The cache's members once the commit is stored, and the devices it added.
    members: BTreeMap<Device, Id>,
    added: Vec<Device>,
    channel: bool,
}

// ---- the service ----

struct Recent {
    account: Id,
    key: [u8; 16],
    hash: [u8; 32],
    seq: u64,
    epoch: u64,
}

struct Pending {
    /// The state before the commit (`None`: the commit created the group).
    previous: Option<Vec<u8>>,
    previous_members: BTreeMap<Device, Id>,
    welcome: Option<Vec<u8>>,
    recipients: Vec<(Id, Device, bool, Vec<u8>)>,
    epoch: u64,
}

struct Loaded {
    group: DeliveryGroup,
    conversation: Id,
    commit_seq: u64,
    recent: VecDeque<Recent>,
    pending: Option<Pending>,
    /// Member devices → accounts.
    members: BTreeMap<Device, Id>,
}

impl Loaded {
    fn channel(&self) -> bool {
        self.group.binding().kind == nuschel_e2ee::ConversationKind::Channel
    }
}

/// One account's devices under its current key-transparency record.
struct Roster {
    identity: IdentityFingerprint,
    devices: Vec<DeviceCertificate>,
    /// Device id → revoked through this key epoch.
    revoked: HashMap<Device, u32>,
}

impl Roster {
    fn device(&self, credential: &DeviceCredential) -> Option<&DeviceCertificate> {
        let device = *credential.device_id().as_bytes();
        if self
            .revoked
            .get(&device)
            .is_some_and(|through| *through >= credential.key_epoch())
        {
            return None;
        }
        self.devices.iter().find(|certificate| {
            *certificate.device_id().as_bytes() == device
                && certificate.matches_credential(credential)
        })
    }
}

pub struct Service {
    store: StorageKt,
    tasks: mpsc::Receiver<Task>,
    stored: mpsc::UnboundedReceiver<StoredNotice>,
    groups: HashMap<Vec<u8>, Loaded>,
    /// The server's binding id: its key-transparency log id.
    server_id: [u8; 32],
    /// Its key-transparency signing key: the groups' external sender.
    signer: ServerSigner,
    config: MlsConfig,
}

fn store_failure(_: nuschel_kt::StoreError) -> Failure {
    (Code::Busy, 1000)
}

fn malformed(reason: &str) -> Failure {
    tracing::info!(reason, "refused an MLS request");
    (Code::Malformed, 0)
}

/// Opens the service: the epochs of every known group (the groups themselves
/// are loaded on first use).
pub async fn open(
    config: &MlsConfig,
    storage: StorageHandle,
    server_id: [u8; 32],
    signer: ServerSigner,
) -> Result<(MlsHandle, Service), crate::server::StartError> {
    let failed =
        |error: nuschel_kt::StoreError| crate::server::StartError::Storage(format!("mls: {error}"));
    let store = StorageKt::new(storage.clone());
    resolve_pending(&store, &storage)
        .await
        .map_err(|error| crate::server::StartError::Storage(format!("mls: {error}")))?;
    let epochs = store.scan_prefix(&[PREFIX_EPOCH]).await.map_err(failed)?;
    let mut groups = HashMap::new();
    for (key, value) in epochs {
        let Some((conversation, cache)) = decode_epoch(&key[1..], &value) else {
            continue;
        };
        groups.insert(conversation, cache);
    }
    let read_touches = |records: Vec<(Vec<u8>, Vec<u8>)>| {
        records
            .into_iter()
            .filter_map(|(key, value)| {
                let first: Id = key.get(1..17)?.try_into().ok()?;
                let device: Device = key.get(17..33)?.try_into().ok()?;
                let at = u64::from_be_bytes(value.as_slice().try_into().ok()?);
                Some(((first, device), Touch { at, written: at }))
            })
            .collect::<HashMap<_, _>>()
    };
    let seen = read_touches(store.scan_prefix(&[PREFIX_SEEN]).await.map_err(failed)?);
    let opened = read_touches(store.scan_prefix(&[PREFIX_OPENED]).await.map_err(failed)?);
    tracing::info!(groups = groups.len(), "MLS delivery service");
    let (tasks, receiver) = mpsc::channel(QUEUE);
    let (stored, stored_receiver) = mpsc::unbounded_channel();
    Ok((
        MlsHandle {
            tasks,
            stored,
            groups,
            listeners: HashMap::new(),
            listening: HashMap::new(),
            claims: HashMap::new(),
            uploads: HashMap::new(),
            mailboxes: HashMap::new(),
            config: config.clone(),
            seen,
            opened,
            clock_offset: Arc::new(AtomicI64::new(0)),
        },
        Service {
            store,
            tasks: receiver,
            stored: stored_receiver,
            groups: HashMap::new(),
            server_id,
            signer,
            config: config.clone(),
        },
    ))
}

/// `E` ‖ group id: epoch, commit seq, conversation, then (since phase 4c) the
/// channel flag and the members.
fn encode_epoch(
    epoch: u64,
    commit_seq: u64,
    conversation: &Id,
    channel: bool,
    members: &BTreeMap<Device, Id>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(35 + members.len() * 32);
    out.extend_from_slice(&epoch.to_be_bytes());
    out.extend_from_slice(&commit_seq.to_be_bytes());
    out.extend_from_slice(conversation);
    out.push(u8::from(channel));
    out.extend_from_slice(&(members.len().min(usize::from(u16::MAX)) as u16).to_be_bytes());
    for (device, account) in members.iter().take(usize::from(u16::MAX)) {
        out.extend_from_slice(device);
        out.extend_from_slice(account);
    }
    out
}

fn decode_epoch(group_id: &[u8], value: &[u8]) -> Option<(Id, GroupCache)> {
    let conversation: Id = value.get(16..32)?.try_into().ok()?;
    let mut cache = GroupCache {
        group_id: group_id.to_vec(),
        epoch: u64::from_be_bytes(value.get(..8)?.try_into().ok()?),
        commit_seq: u64::from_be_bytes(value.get(8..16)?.try_into().ok()?),
        established: true,
        ..GroupCache::default()
    };
    if value.len() > 32 {
        cache.channel = *value.get(32)? == 1;
        let count = usize::from(u16::from_be_bytes(value.get(33..35)?.try_into().ok()?));
        let list = value.get(35..35 + count * 32)?;
        for entry in list.as_chunks::<32>().0 {
            cache
                .members
                .insert(entry[..16].try_into().ok()?, entry[16..].try_into().ok()?);
        }
    }
    Some((conversation, cache))
}

fn encode_members(out: &mut Vec<u8>, members: &BTreeMap<Device, Id>) {
    out.extend_from_slice(&(members.len().min(usize::from(u16::MAX)) as u16).to_be_bytes());
    for (device, account) in members.iter().take(usize::from(u16::MAX)) {
        out.extend_from_slice(device);
        out.extend_from_slice(account);
    }
}

fn encode_group(loaded: &Loaded) -> Vec<u8> {
    let state = loaded.group.state();
    let mut out =
        Vec::with_capacity(64 + loaded.recent.len() * 80 + loaded.members.len() * 32 + state.len());
    out.push(2);
    out.extend_from_slice(&loaded.conversation);
    out.extend_from_slice(&loaded.commit_seq.to_be_bytes());
    out.push(loaded.recent.len() as u8);
    for recent in &loaded.recent {
        out.extend_from_slice(&recent.account);
        out.extend_from_slice(&recent.key);
        out.extend_from_slice(&recent.hash);
        out.extend_from_slice(&recent.seq.to_be_bytes());
        out.extend_from_slice(&recent.epoch.to_be_bytes());
    }
    encode_members(&mut out, &loaded.members);
    out.extend_from_slice(&state);
    out
}

fn decode_group(group_id: &[u8], bytes: &[u8]) -> Option<Loaded> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let slice = bytes.get(at..at + n)?;
        at += n;
        Some(slice)
    };
    let version = take(1)?[0];
    if version != 1 && version != 2 {
        return None;
    }
    let conversation: Id = take(16)?.try_into().ok()?;
    let commit_seq = u64::from_be_bytes(take(8)?.try_into().ok()?);
    let count = take(1)?[0] as usize;
    let mut recent = VecDeque::with_capacity(count);
    for _ in 0..count {
        recent.push_back(Recent {
            account: take(16)?.try_into().ok()?,
            key: take(16)?.try_into().ok()?,
            hash: take(32)?.try_into().ok()?,
            seq: u64::from_be_bytes(take(8)?.try_into().ok()?),
            epoch: u64::from_be_bytes(take(8)?.try_into().ok()?),
        });
    }
    let mut members = BTreeMap::new();
    if version == 2 {
        let count = usize::from(u16::from_be_bytes(take(2)?.try_into().ok()?));
        for _ in 0..count {
            let device: Device = take(16)?.try_into().ok()?;
            members.insert(device, take(16)?.try_into().ok()?);
        }
    }
    let state = bytes.get(at..)?;
    let group = DeliveryGroup::restore(group_id, state).ok()?;
    Some(Loaded {
        group,
        conversation,
        commit_seq,
        recent,
        pending: None,
        members,
    })
}

/// A commit waiting for its row: what [`Service`] writes as `N` ‖ group id.
struct PendingRecord {
    /// The group's newest commit seq before this one: its row comes after.
    after_seq: u64,
    epoch: u64,
    /// SHA-256 of the commit (the row's content).
    hash: [u8; 32],
    welcome: Option<Vec<u8>>,
    recipients: Vec<(Id, Device, bool, Vec<u8>)>,
    /// The group after the commit (the `G` encoding).
    group: Vec<u8>,
}

fn encode_pending(record: &PendingRecord) -> Vec<u8> {
    let welcome = record.welcome.as_deref().unwrap_or_default();
    let mut out = Vec::with_capacity(64 + welcome.len() + record.group.len());
    out.push(1);
    out.extend_from_slice(&record.after_seq.to_be_bytes());
    out.extend_from_slice(&record.epoch.to_be_bytes());
    out.extend_from_slice(&record.hash);
    out.push(u8::from(record.welcome.is_some()));
    out.extend_from_slice(&(welcome.len() as u32).to_be_bytes());
    out.extend_from_slice(welcome);
    out.extend_from_slice(&(record.recipients.len() as u16).to_be_bytes());
    for (account, device, last_resort, reference) in &record.recipients {
        out.extend_from_slice(account);
        out.extend_from_slice(device);
        out.push(u8::from(*last_resort));
        out.push(reference.len().min(255) as u8);
        out.extend_from_slice(&reference[..reference.len().min(255)]);
    }
    out.extend_from_slice(&record.group);
    out
}

fn decode_pending(bytes: &[u8]) -> Option<PendingRecord> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let slice = bytes.get(at..at + n)?;
        at += n;
        Some(slice)
    };
    if take(1)? != [1] {
        return None;
    }
    let after_seq = u64::from_be_bytes(take(8)?.try_into().ok()?);
    let epoch = u64::from_be_bytes(take(8)?.try_into().ok()?);
    let hash: [u8; 32] = take(32)?.try_into().ok()?;
    let has_welcome = take(1)?[0] == 1;
    let length = u32::from_be_bytes(take(4)?.try_into().ok()?) as usize;
    let welcome = take(length)?.to_vec();
    let count = u16::from_be_bytes(take(2)?.try_into().ok()?) as usize;
    let mut recipients = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let account: Id = take(16)?.try_into().ok()?;
        let device: Device = take(16)?.try_into().ok()?;
        let last_resort = take(1)?[0] == 1;
        let length = take(1)?[0] as usize;
        recipients.push((account, device, last_resort, take(length)?.to_vec()));
    }
    let group = bytes.get(at..)?.to_vec();
    Some(PendingRecord {
        after_seq,
        epoch,
        hash,
        welcome: has_welcome.then_some(welcome),
        recipients,
        group,
    })
}

/// A pending record for a commit without a Welcome: what the service writes
/// before the commit's row (for tests that simulate a crash).
#[doc(hidden)]
pub fn pending_record(after_seq: u64, epoch: u64, commit: &[u8], group: &[u8]) -> Vec<u8> {
    encode_pending(&PendingRecord {
        after_seq,
        epoch,
        hash: sha256(&[commit]),
        welcome: None,
        recipients: Vec::new(),
        group: group.to_vec(),
    })
}

/// The writes that make a stored commit the group's state: `G`, `E`, a mailbox
/// entry per Welcome recipient (and the end of its one-time reference). Returns
/// them and the Welcomes to push.
fn promote(
    group_id: &[u8],
    loaded: &mut Loaded,
    seq: u64,
    epoch: u64,
    welcome: Option<&[u8]>,
    recipients: &[(Id, Device, bool, Vec<u8>)],
) -> (Records, Vec<Push>) {
    loaded.commit_seq = seq;
    if let Some(last) = loaded.recent.back_mut()
        && last.seq == 0
    {
        last.seq = seq;
    }
    let conversation = loaded.conversation;
    let epoch_value = encode_epoch(epoch, seq, &conversation, loaded.channel(), &loaded.members);
    let mut records = vec![
        (key(PREFIX_GROUP, &[group_id]), encode_group(loaded)),
        (key(PREFIX_EPOCH, &[group_id]), epoch_value),
    ];
    let mut pushes = Vec::new();
    if let Some(welcome) = welcome {
        for (account, device, last_resort, reference) in recipients {
            let mut id = now_ms().to_be_bytes().to_vec();
            id.extend_from_slice(&crate::state::random::<8>());
            let entry = v1::MlsWelcome {
                id: id.clone(),
                conversation: conversation.to_vec(),
                seq,
                epoch,
                welcome: welcome.to_vec(),
            };
            records.push((
                key(PREFIX_WELCOME, &[account, device, &id]),
                entry.encode_to_vec(),
            ));
            if !last_resort {
                records.push((key(PREFIX_REFERENCE, &[reference]), Vec::new()));
            }
            pushes.push((*account, *device, entry));
        }
    }
    (records, pushes)
}

/// The seq of the conversation's `MLS_HANDSHAKE` row after `after` that carries
/// the commit with this hash, if there is one.
async fn find_commit_row(
    history: &StorageHandle,
    conversation: &Id,
    after: u64,
    hash: &[u8; 32],
) -> Result<Option<u64>, String> {
    let mut anchor = after;
    for _ in 0..MAX_RESOLVE_PAGES {
        let query = PageQuery {
            conversation: hex(conversation),
            anchor: Anchor::After(anchor),
            limit: nuschel::MAX_PAGE_MESSAGES as u32,
            ranges: vec![(1, u64::MAX)],
            max_bytes: nuschel::MAX_PAGE_BYTES as u32,
        };
        let page = match history.request(Request::HistoryPage { query }).await {
            Some(Response::Page(page)) => page,
            _ => return Err("history unavailable".into()),
        };
        for row in &page.rows {
            if row.kind == MessageKind::Message
                && row.format == v1::Format::MlsHandshake as u8
                && sha256(&[&row_content(row.format, row.content.clone()).1]) == *hash
            {
                return Ok(Some(row.seq));
            }
        }
        match page.rows.last() {
            Some(last) if page.more_after => anchor = last.seq,
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// Resolves the commits a crash left pending (see the module documentation).
async fn resolve_pending(store: &StorageKt, history: &StorageHandle) -> Result<usize, String> {
    let pending = store
        .scan_prefix(&[PREFIX_PENDING])
        .await
        .map_err(|error| error.to_string())?;
    let mut adopted = 0;
    for (pending_key, value) in pending {
        let group_id = pending_key[1..].to_vec();
        let mut records = vec![(pending_key, Vec::new())];
        let resolved = decode_pending(&value).and_then(|record| {
            decode_group(&group_id, &record.group).map(|loaded| (record, loaded))
        });
        match resolved {
            Some((record, mut loaded)) => {
                let conversation = loaded.conversation;
                match find_commit_row(history, &conversation, record.after_seq, &record.hash)
                    .await?
                {
                    Some(seq) => {
                        let (promotion, _) = promote(
                            &group_id,
                            &mut loaded,
                            seq,
                            record.epoch,
                            record.welcome.as_deref(),
                            &record.recipients,
                        );
                        records.extend(promotion);
                        adopted += 1;
                        tracing::warn!(seq, "adopted an MLS commit stored before a crash");
                    }
                    None => tracing::info!("dropped an MLS commit that was never stored"),
                }
            }
            None => tracing::error!("an MLS commit's pending record is unreadable; dropped"),
        }
        store
            .write(records)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(adopted)
}

fn credential_identity(credential: &[u8]) -> Option<(IdentityFingerprint, DeviceCredential)> {
    let credential = DeviceCredential::decode(credential).ok()?;
    Some((credential.identity(), credential))
}

impl Service {
    pub async fn run(mut self, server: Arc<Server>) {
        // Removals follow the clock too (inactive devices, channels not opened).
        let mut sweep =
            tokio::time::interval(std::time::Duration::from_secs(self.config.sweep_interval));
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                notice = self.stored.recv() => {
                    let Some(notice) = notice else { break };
                    let pushes = self.stored(notice).await;
                    if !pushes.is_empty() {
                        server.lock().mls_push(pushes);
                    }
                }
                task = self.tasks.recv() => {
                    let Some(task) = task else { break };
                    let reply = self.answer(task.session, task.job).await;
                    server.lock().mls_answer(task.session, task.request_id, reply);
                }
                _ = sweep.tick() => server.lock().mls_sweep(),
            }
        }
    }

    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Failure> {
        self.store.get(key).await.map_err(store_failure)
    }

    async fn write(&self, records: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), Failure> {
        if records.is_empty() {
            return Ok(());
        }
        self.store.write(records).await.map_err(store_failure)
    }

    /// The account's devices under its current key-transparency record.
    async fn roster(&self, account: &Id) -> Result<Option<Roster>, Failure> {
        let Some(hash) = self.get(&crate::kt::current_key(account)).await? else {
            return Ok(None);
        };
        let Ok(hash) = <[u8; 32]>::try_from(hash.as_slice()) else {
            return Ok(None);
        };
        let Some(record) = self.get(&crate::kt::record_key(account, &hash)).await? else {
            return Ok(None);
        };
        let Ok(record) = v1::KtRecord::decode(record.as_slice()) else {
            return Ok(None);
        };
        // The server checked the record under the identity's root before keeping it.
        let Ok(signing) =
            nuschel_identity::DeviceSigningCertificate::decode(&record.device_signing_certificate)
        else {
            return Ok(None);
        };
        let devices = record
            .devices
            .iter()
            .filter_map(|bytes| DeviceCertificate::decode(bytes).ok())
            .filter(|device| device.identity() == signing.identity())
            .collect();
        let mut revoked = HashMap::new();
        for bytes in &record.revocations {
            if let Ok(revocation) = nuschel_identity::DeviceRevocation::decode(bytes) {
                let through = revoked
                    .entry(*revocation.device_id().as_bytes())
                    .or_insert(0);
                *through = (*through).max(revocation.through_key_epoch());
            }
        }
        Ok(Some(Roster {
            identity: signing.identity(),
            devices,
            revoked,
        }))
    }

    async fn load(&mut self, group_id: &[u8]) -> Result<bool, Failure> {
        if self.groups.contains_key(group_id) {
            return Ok(true);
        }
        let Some(bytes) = self.get(&key(PREFIX_GROUP, &[group_id])).await? else {
            return Ok(false);
        };
        match decode_group(group_id, &bytes) {
            Some(loaded) => {
                self.groups.insert(group_id.to_vec(), loaded);
                Ok(true)
            }
            None => {
                tracing::error!("an MLS group's stored state is unreadable");
                Err((Code::Busy, 1000))
            }
        }
    }

    async fn answer(&mut self, _session: u32, job: Job) -> Reply {
        let result = match job {
            Job::Upload { account, upload } => self.upload(account, upload).await,
            Job::Claim { target } => self.claim(target).await,
            Job::Commit {
                account,
                conversation,
                allowed,
                commit,
            } => {
                return self
                    .commit(account, conversation, allowed, commit)
                    .await
                    .unwrap_or_else(|failure| Reply::CommitFailed {
                        conversation,
                        failure,
                    });
            }
            Job::Welcomes { account, device } => self.welcomes(account, device).await,
            Job::WelcomeAck {
                account,
                device,
                ids,
            } => {
                let records = ids
                    .iter()
                    .filter(|id| id.len() == 16)
                    .map(|id| (key(PREFIX_WELCOME, &[&account, &device, id]), Vec::new()))
                    .collect();
                self.write(records).await.map(|()| Reply::Done)
            }
            Job::JoinInfo { conversation, tree } => self.join_info(conversation, tree).await,
            Job::Propose {
                conversation,
                devices,
            } => self.propose(conversation, devices).await,
            Job::Touch(records) => self.write(records).await.map(|()| Reply::Done),
        };
        result.unwrap_or_else(Reply::Failed)
    }

    /// The server's removal proposals for `devices` (docs/e2ee.md §6.2), queued in
    /// the group (and saved with it) and returned for the stream.
    async fn propose(
        &mut self,
        conversation: Id,
        devices: BTreeSet<Device>,
    ) -> Result<Reply, Failure> {
        let group_id = group_id(&conversation);
        if !self.load(&group_id).await? {
            return Err((Code::NotFound, 0));
        }
        let signer = &self.signer;
        let loaded = self.groups.get_mut(&group_id).ok_or((Code::NotFound, 0))?;
        if loaded.pending.is_some() {
            return Err((Code::Busy, 500));
        }
        let epoch = loaded.group.epoch();
        let mut rows = Vec::new();
        if loaded.group.takes_server_proposals(signer.public_key()) {
            let members = loaded.group.members().map_err(|error| {
                tracing::error!(%error, "could not read an MLS group");
                (Code::Busy, 1000)
            })?;
            for member in members {
                let Some((_, credential)) = credential_identity(&member.credential) else {
                    continue;
                };
                if !devices.contains(credential.device_id().as_bytes()) {
                    continue;
                }
                match loaded.group.propose_removal(member.index, signer) {
                    Ok(row) => rows.push(row),
                    Err(error) => tracing::warn!(%error, "could not propose an MLS removal"),
                }
            }
        }
        if !rows.is_empty() {
            let record = encode_group(loaded);
            self.write(vec![(key(PREFIX_GROUP, &[&group_id]), record)])
                .await?;
        }
        Ok(Reply::Proposed {
            conversation,
            epoch,
            devices,
            rows,
        })
    }

    async fn upload(
        &mut self,
        account: Id,
        upload: v1::MlsKeyPackageUpload,
    ) -> Result<Reply, Failure> {
        // No binding (yet: it waits for its epoch): publish first.
        let roster = self.roster(&account).await?.ok_or((Code::NotFound, 0))?;
        let mut packages: Vec<(Vec<u8>, bool)> = upload
            .key_packages
            .into_iter()
            .map(|bytes| (bytes, false))
            .collect();
        if !upload.last_resort.is_empty() {
            packages.push((upload.last_resort, true));
        }
        if packages.is_empty() {
            return Err(malformed("no key packages"));
        }
        let mut device: Option<Device> = None;
        let mut records = Vec::new();
        let now = now_ms();
        for (index, (bytes, last_resort)) in packages.into_iter().enumerate() {
            let info = check_key_package(&bytes).map_err(|error| {
                tracing::info!(%error, "refused a key package");
                (Code::Malformed, 0)
            })?;
            let (identity, credential) = credential_identity(&info.credential)
                .ok_or_else(|| malformed("key package credential"))?;
            if identity != roster.identity || roster.device(&credential).is_none() {
                return Err(malformed(
                    "a key package for a device not in the uploader's record",
                ));
            }
            if credential.leaf_key().as_bytes() != &info.signature_key {
                return Err(malformed("the credential does not certify the leaf key"));
            }
            if info.last_resort != last_resort {
                return Err(malformed("last-resort flag"));
            }
            let id = *credential.device_id().as_bytes();
            if device.is_some_and(|device| device != id) {
                return Err(malformed("key packages for several devices"));
            }
            device = Some(id);
            let mut target = account.to_vec();
            target.extend_from_slice(&id);
            target.push(u8::from(last_resort));
            records.push((key(PREFIX_REFERENCE, &[&info.reference]), target));
            if last_resort {
                records.push((key(PREFIX_LAST_RESORT, &[&account, &id]), bytes));
            } else {
                // The reference ends the key, so a dropped package's routing goes too.
                records.push((
                    key(
                        PREFIX_PACKAGE,
                        &[
                            &account,
                            &id,
                            &now.to_be_bytes(),
                            &(index as u32).to_be_bytes(),
                            &info.reference,
                        ],
                    ),
                    bytes,
                ));
            }
        }
        let device = device.ok_or_else(|| malformed("no key packages"))?;
        self.write(records).await?;
        // At most MAX_MLS_DEVICE_PACKAGES one-time packages; the oldest go first.
        let existing = self
            .store
            .scan_prefix(&key(PREFIX_PACKAGE, &[&account, &device]))
            .await
            .map_err(store_failure)?;
        let excess = existing.len().saturating_sub(MAX_MLS_DEVICE_PACKAGES);
        let mut drop: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for (dropped, _) in existing.iter().take(excess) {
            drop.push((dropped.clone(), Vec::new()));
            if let Some(reference) = dropped.get(PACKAGE_KEY_FIXED..)
                && !reference.is_empty()
            {
                drop.push((key(PREFIX_REFERENCE, &[reference]), Vec::new()));
            }
        }
        self.write(drop).await?;
        let last_resort = self
            .get(&key(PREFIX_LAST_RESORT, &[&account, &device]))
            .await?
            .is_some_and(|value| !value.is_empty());
        Ok(Reply::Status(v1::MlsKeyPackageStatus {
            request_id: 0,
            one_time: (existing.len() - excess) as u32,
            last_resort,
        }))
    }

    async fn claim(&mut self, target: Id) -> Result<Reply, Failure> {
        let Some(roster) = self.roster(&target).await? else {
            return Err((Code::NotFound, 0));
        };
        let mut packages = Vec::new();
        let mut consumed = Vec::new();
        let mut seen = BTreeSet::new();
        for certificate in &roster.devices {
            let device = *certificate.device_id().as_bytes();
            if !seen.insert(device)
                || roster
                    .revoked
                    .get(&device)
                    .is_some_and(|through| *through >= certificate.key_epoch())
            {
                continue;
            }
            let one_time = self
                .store
                .scan_prefix(&key(PREFIX_PACKAGE, &[&target, &device]))
                .await
                .map_err(store_failure)?;
            if let Some((key, bytes)) = one_time.into_iter().find(|(_, value)| !value.is_empty()) {
                consumed.push((key, Vec::new()));
                packages.push(bytes);
            } else if let Some(bytes) = self
                .get(&key(PREFIX_LAST_RESORT, &[&target, &device]))
                .await?
                .filter(|value| !value.is_empty())
            {
                packages.push(bytes);
            }
        }
        self.write(consumed).await?;
        Ok(Reply::Packages(v1::MlsKeyPackages {
            request_id: 0,
            account: target.to_vec(),
            key_packages: packages,
        }))
    }

    async fn commit(
        &mut self,
        account: Id,
        conversation: Id,
        allowed: Vec<Id>,
        commit: v1::MlsCommit,
    ) -> Result<Reply, Failure> {
        let group_id = group_id(&conversation);
        let hash = sha256(&[&commit.commit]);
        let key_bytes: [u8; 16] =
            nuschel::id(&commit.idempotency_key).ok_or_else(|| malformed("idempotency key"))?;
        let exists = self.load(&group_id).await?;
        let created = match (&commit.create, exists) {
            (Some(_), true) => {
                let loaded = self.groups.get(&group_id).ok_or((Code::Busy, 1000))?;
                // A retry of the commit that created it is acknowledged again.
                if let Some(recent) = loaded
                    .recent
                    .iter()
                    .find(|recent| recent.account == account && recent.key == key_bytes)
                {
                    return Ok(self.retried(recent, hash, conversation));
                }
                return Ok(Reply::Stale {
                    conversation,
                    epoch: loaded.group.epoch(),
                });
            }
            (None, false) => return Err((Code::NotFound, 0)),
            (Some(init), false) => {
                if init.group_info.len() > MAX_MLS_GROUP_INFO_BYTES
                    || init.tree.len() > MAX_MLS_TREE_BYTES
                {
                    return Err((Code::TooLarge, 0));
                }
                let (group, _) = DeliveryGroup::create(
                    &init.group_info,
                    &init.tree,
                    &self.server_id,
                    &conversation,
                    self.signer.public_key(),
                )
                .map_err(|error| {
                    tracing::info!(%error, "refused a new MLS group");
                    (Code::Malformed, 0)
                })?;
                // The creator's own device: its leaf is the group's first.
                let mut members = BTreeMap::new();
                for member in group.members().unwrap_or_default() {
                    if let Some((_, credential)) = credential_identity(&member.credential) {
                        members.insert(*credential.device_id().as_bytes(), account);
                    }
                }
                self.groups.insert(
                    group_id.clone(),
                    Loaded {
                        group,
                        conversation,
                        commit_seq: 0,
                        recent: VecDeque::new(),
                        pending: None,
                        members,
                    },
                );
                true
            }
            (None, true) => false,
        };
        let result = self
            .apply(
                &group_id, account, &allowed, &commit, key_bytes, hash, created,
            )
            .await;
        if created && !matches!(result, Ok(Reply::Accepted(_))) {
            self.groups.remove(&group_id);
        }
        result
    }

    fn retried(&self, recent: &Recent, hash: [u8; 32], conversation: Id) -> Reply {
        if recent.hash == hash {
            Reply::Ack(v1::MlsAck {
                request_id: 0,
                conversation: conversation.to_vec(),
                seq: recent.seq,
                epoch: recent.epoch,
            })
        } else {
            Reply::Failed((Code::IdempotencyConflict, 0))
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply(
        &mut self,
        group_id: &[u8],
        account: Id,
        allowed: &[Id],
        commit: &v1::MlsCommit,
        key_bytes: [u8; 16],
        hash: [u8; 32],
        created: bool,
    ) -> Result<Reply, Failure> {
        // The identities that may be in the group, before touching it; for a
        // channel, the committer's own (its rosters admit what it adds: itself).
        let mut identities = Vec::new();
        let mut committer_roster = None;
        for allowed in allowed {
            if let Some(roster) = self.roster(allowed).await? {
                identities.push(roster.identity);
                if *allowed == account {
                    committer_roster = Some(roster);
                }
            }
        }
        let loaded = self.groups.get_mut(group_id).ok_or((Code::NotFound, 0))?;
        if loaded.pending.is_some() {
            return Err((Code::Busy, 500));
        }
        if let Some(recent) = loaded
            .recent
            .iter()
            .find(|recent| recent.account == account && recent.key == key_bytes)
        {
            let conversation = loaded.conversation;
            let reply = if recent.hash == hash {
                Reply::Ack(v1::MlsAck {
                    request_id: 0,
                    conversation: conversation.to_vec(),
                    seq: recent.seq,
                    epoch: recent.epoch,
                })
            } else {
                Reply::Failed((Code::IdempotencyConflict, 0))
            };
            return Ok(reply);
        }
        let previous = (!created).then(|| loaded.group.state());
        let welcome = (!commit.welcome.is_empty()).then_some(commit.welcome.as_slice());
        let outcome = match loaded
            .group
            .commit(&commit.commit, &commit.group_info, welcome)
        {
            Ok(outcome) => outcome,
            Err(DsError::StaleEpoch { current }) => {
                return Ok(Reply::Stale {
                    conversation: loaded.conversation,
                    epoch: current,
                });
            }
            Err(DsError::Invalid(error)) => {
                tracing::info!(%error, "refused an MLS commit");
                return Err((Code::Malformed, 0));
            }
        };
        let roll_back = |loaded: &mut Loaded| {
            if let Some(previous) = &previous
                && let Ok(group) = DeliveryGroup::restore(group_id, previous)
            {
                loaded.group = group;
            }
        };
        // A direct conversation: every member belongs to one of its two
        // identities. A channel: the server admitted the readers already in it;
        // what a commit adds (an external joiner, or the creator) must be the
        // committer's own device, in its account's key-transparency record.
        let strangers = if loaded.channel() {
            outcome.added.iter().any(|member| {
                credential_identity(&member.credential).is_none_or(|(identity, credential)| {
                    committer_roster.as_ref().is_none_or(|roster| {
                        identity != roster.identity || roster.device(&credential).is_none()
                    })
                })
            })
        } else {
            outcome.members.iter().any(|member| {
                credential_identity(&member.credential)
                    .is_none_or(|(identity, _)| !identities.contains(&identity))
            })
        };
        if strangers {
            roll_back(loaded);
            return Err(malformed("a member outside the conversation"));
        }
        let epoch = outcome.epoch;
        let conversation = loaded.conversation;
        // Route the Welcome by the key packages' references.
        let mut recipients = Vec::new();
        let mut unroutable = false;
        for reference in &outcome.welcome_for {
            match self.store.get(&key(PREFIX_REFERENCE, &[reference])).await {
                Ok(Some(target)) if target.len() == 33 => {
                    let account: Id = target[..16].try_into().unwrap_or_default();
                    let device: Device = target[16..32].try_into().unwrap_or_default();
                    recipients.push((account, device, target[32] == 1, reference.clone()));
                }
                Ok(_) => unroutable = true,
                Err(error) => {
                    let loaded = self.groups.get_mut(group_id).ok_or((Code::Busy, 1000))?;
                    roll_back(loaded);
                    return Err(store_failure(error));
                }
            }
        }
        let loaded = self.groups.get_mut(group_id).ok_or((Code::Busy, 1000))?;
        if unroutable {
            roll_back(loaded);
            return Err(malformed(
                "a Welcome for a key package this server did not serve",
            ));
        }
        let after_seq = loaded.commit_seq;
        // Who the members are now: the removed go, the added come with their
        // accounts (the Welcome's recipients, or the committer for itself).
        let previous_members = loaded.members.clone();
        let device_of = |member: &nuschel_e2ee::ds::Member| {
            credential_identity(&member.credential).map(|(_, c)| *c.device_id().as_bytes())
        };
        for member in &outcome.removed {
            if let Some(device) = device_of(member) {
                loaded.members.remove(&device);
            }
        }
        let mut added = Vec::new();
        for member in &outcome.added {
            if let Some(device) = device_of(member) {
                let owner = recipients
                    .iter()
                    .find(|(_, recipient, _, _)| *recipient == device)
                    .map_or(account, |(owner, _, _, _)| *owner);
                loaded.members.insert(device, owner);
                added.push(device);
            }
        }
        let members = loaded.members.clone();
        let channel = loaded.channel();
        loaded.pending = Some(Pending {
            previous,
            previous_members,
            welcome: welcome.map(<[u8]>::to_vec),
            recipients: recipients.clone(),
            epoch,
        });
        // Its seq is filled in once stored.
        loaded.recent.push_back(Recent {
            account,
            key: key_bytes,
            hash,
            seq: 0,
            epoch,
        });
        while loaded.recent.len() > RECENT {
            loaded.recent.pop_front();
        }
        // The new state is durable before its row is queued (crash safety, see the
        // module documentation).
        let record = encode_pending(&PendingRecord {
            after_seq,
            epoch,
            hash,
            welcome: welcome.map(<[u8]>::to_vec),
            recipients,
            group: encode_group(loaded),
        });
        if let Err(failure) = self
            .write(vec![(key(PREFIX_PENDING, &[group_id]), record)])
            .await
        {
            self.undo(group_id);
            return Err(failure);
        }
        Ok(Reply::Accepted(Box::new(Accepted {
            conversation,
            group_id: group_id.to_vec(),
            epoch,
            commit: commit.commit.clone(),
            account,
            key: key_bytes,
            hash,
            members,
            added,
            channel,
        })))
    }

    /// Forgets a pending commit: the group goes back to its state before it.
    fn undo(&mut self, group_id: &[u8]) {
        let Some(loaded) = self.groups.get_mut(group_id) else {
            return;
        };
        let Some(pending) = loaded.pending.take() else {
            return;
        };
        match &pending.previous {
            None => {
                self.groups.remove(group_id);
            }
            Some(previous) => {
                if let Ok(group) = DeliveryGroup::restore(group_id, previous) {
                    loaded.group = group;
                }
                loaded.members = pending.previous_members;
                loaded.recent.pop_back();
            }
        }
    }

    /// A commit's row was stored (or not): save the group and deliver the Welcome,
    /// or go back. Returns the Welcomes to push.
    async fn stored(&mut self, notice: StoredNotice) -> Vec<(Id, Device, v1::MlsWelcome)> {
        let pending_key = key(PREFIX_PENDING, &[&notice.group_id]);
        if !notice.ok {
            self.undo(&notice.group_id);
            if let Err(error) = self.store.write(vec![(pending_key, Vec::new())]).await {
                // Harmless: on start, a pending record without its row is deleted.
                tracing::warn!(%error, "could not drop a refused MLS commit's record");
            }
            return Vec::new();
        }
        let Some(loaded) = self.groups.get_mut(&notice.group_id) else {
            return Vec::new();
        };
        let Some(pending) = loaded.pending.take() else {
            return Vec::new();
        };
        let (mut records, pushes) = promote(
            &notice.group_id,
            loaded,
            notice.seq,
            pending.epoch,
            pending.welcome.as_deref(),
            &pending.recipients,
        );
        records.push((pending_key, Vec::new()));
        if let Err(error) = self.store.write(records).await {
            // The pending record stays: the next start adopts it from the stream.
            tracing::error!(%error, "could not save an MLS group after its commit");
        }
        pushes
    }

    async fn welcomes(&mut self, account: Id, device: Device) -> Result<Reply, Failure> {
        let entries = self
            .store
            .scan_prefix(&key(PREFIX_WELCOME, &[&account, &device]))
            .await
            .map_err(store_failure)?;
        let horizon = now_ms().saturating_sub(u64::from(self.config.welcome_days) * 86_400_000);
        let mut expired = Vec::new();
        let mut welcomes = Vec::new();
        for (key, value) in entries {
            if value.is_empty() {
                continue;
            }
            let stamp = key
                .get(33..41)
                .and_then(|bytes| bytes.try_into().ok())
                .map_or(0, u64::from_be_bytes);
            if stamp < horizon {
                expired.push((key, Vec::new()));
                continue;
            }
            if welcomes.len() < MAX_MLS_WELCOMES
                && let Ok(welcome) = v1::MlsWelcome::decode(value.as_slice())
            {
                welcomes.push(welcome);
            }
        }
        self.write(expired).await?;
        Ok(Reply::Welcomes(v1::MlsWelcomes {
            request_id: 0,
            welcomes,
        }))
    }

    async fn join_info(&mut self, conversation: Id, tree: bool) -> Result<Reply, Failure> {
        let group_id = group_id(&conversation);
        if !self.load(&group_id).await? {
            return Err((Code::NotFound, 0));
        }
        let loaded = self.groups.get(&group_id).ok_or((Code::NotFound, 0))?;
        if loaded.pending.is_some() {
            return Err((Code::Busy, 500));
        }
        let (group_info, tree_bytes) = loaded.group.join_info().map_err(|error| {
            tracing::error!(%error, "could not export an MLS group");
            (Code::Busy, 1000)
        })?;
        let accounts: BTreeSet<Id> = loaded.members.values().copied().collect();
        Ok(Reply::JoinInfo(v1::MlsJoinInfo {
            request_id: 0,
            conversation: conversation.to_vec(),
            epoch: loaded.group.epoch(),
            group_info,
            tree: if tree { tree_bytes } else { Vec::new() },
            commit_seq: loaded.commit_seq,
            accounts: accounts
                .into_iter()
                .take(nuschel::MAX_MLS_JOIN_ACCOUNTS)
                .map(|account| account.to_vec())
                .collect(),
        }))
    }
}

// ---- handlers ----

fn conversation_id(bytes: &[u8]) -> Result<Id, Failure> {
    nuschel::id(bytes).ok_or((Code::Malformed, 0))
}

fn device_id(bytes: &[u8]) -> Result<Device, Failure> {
    nuschel::id(bytes).ok_or((Code::Malformed, 0))
}

impl State {
    /// The server is in end-to-end mode: every conversation is an MLS group (or
    /// becomes one), and plaintext, plaintext edits and deletions, and stock text
    /// are refused (docs/e2ee.md §7.2, §10). The delivery service runs exactly then.
    pub fn end_to_end(&self) -> bool {
        self.mls.is_some()
    }

    /// Whether the conversation has (or is getting) an MLS group.
    pub fn mls_group(&self, conversation: &Id) -> bool {
        self.mls
            .as_ref()
            .is_some_and(|mls| mls.has_group(conversation))
    }

    fn mls_account(&self, session: u32) -> Result<Id, Failure> {
        self.users
            .get(&session)
            .and_then(|user| user.account)
            .ok_or((Code::NotFound, 0))
    }

    fn mls_submit(&mut self, session: u32, request_id: u64, job: Job) -> Result<(), Failure> {
        let mls = self.mls.as_ref().ok_or((Code::Unsupported, 0))?;
        mls.tasks
            .try_send(Task {
                session,
                request_id,
                job,
            })
            .map_err(|_| (Code::Busy, 1000))
    }

    /// A `mls/1` request from an authenticated connection that negotiated it.
    pub(crate) fn mls_frame(&mut self, session: u32, frame: &ControlFrame) {
        let payload = frame.payload.clone();
        macro_rules! decoded {
            ($type:ty) => {
                match <$type>::decode(payload) {
                    Ok(message) => message,
                    Err(_) => {
                        let request_id =
                            v1::Done::decode(frame.payload.clone()).map_or(0, |d| d.request_id);
                        self.extension_error(session, request_id, (Code::Malformed, 0));
                        return;
                    }
                }
            };
        }
        match frame.kind {
            kind::MLS_KEY_PACKAGE_UPLOAD => {
                let message = decoded!(v1::MlsKeyPackageUpload);
                let request_id = message.request_id;
                self.deferred(session, request_id, |state| {
                    state.authorize(session, Action::MlsUpload)?;
                    let account = state.mls_account(session)?;
                    if message.key_packages.len() > MAX_MLS_UPLOAD_PACKAGES
                        || message
                            .key_packages
                            .iter()
                            .chain([&message.last_resort])
                            .any(|bytes| bytes.len() > MAX_MLS_KEY_PACKAGE_BYTES)
                    {
                        return Err((Code::TooLarge, 0));
                    }
                    let mls = state.mls.as_mut().ok_or((Code::Unsupported, 0))?;
                    if mls.limited(account, Bucket::Upload) {
                        return Err((Code::RateLimited, 6000));
                    }
                    state.mls_submit(
                        session,
                        request_id,
                        Job::Upload {
                            account,
                            upload: message,
                        },
                    )
                });
            }
            kind::MLS_KEY_PACKAGE_CLAIM => {
                let message = decoded!(v1::MlsKeyPackageClaim);
                let request_id = message.request_id;
                self.deferred(session, request_id, |state| {
                    let account = state.mls_account(session)?;
                    let target = nuschel::id(&message.account).ok_or((Code::Malformed, 0))?;
                    // One's own devices, and direct-conversation partners only.
                    state.authorize(session, Action::MlsClaim { account: &target })?;
                    let mls = state.mls.as_mut().ok_or((Code::Unsupported, 0))?;
                    if mls.limited(account, Bucket::Claim) {
                        return Err((Code::RateLimited, 2000));
                    }
                    state.mls_submit(session, request_id, Job::Claim { target })
                });
            }
            kind::MLS_COMMIT => {
                let message = decoded!(v1::MlsCommit);
                let request_id = message.request_id;
                self.deferred(session, request_id, |state| {
                    state.mls_commit(session, message)
                });
            }
            kind::MLS_SEND => {
                let message = decoded!(v1::MlsSend);
                let request_id = message.request_id;
                self.deferred(session, request_id, |state| {
                    state.mls_send(session, message)
                });
            }
            kind::MLS_WELCOME_FETCH => {
                let message = decoded!(v1::MlsWelcomeFetch);
                let request_id = message.request_id;
                self.deferred(session, request_id, |state| {
                    state.authorize(session, Action::MlsMailbox)?;
                    let account = state.mls_account(session)?;
                    let device = device_id(&message.device)?;
                    let users = &state.users;
                    let mls = state.mls.as_mut().ok_or((Code::Unsupported, 0))?;
                    if mls.limited(account, Bucket::Mailbox) {
                        return Err((Code::RateLimited, 2000));
                    }
                    mls.listen(session, account, device, |session| {
                        users.contains_key(&session)
                    });
                    state.mls_submit(session, request_id, Job::Welcomes { account, device })?;
                    // The device is here: it is seen, and may be the committer a
                    // group waits for.
                    state.mls_seen(session, None);
                    state.mls_sweep();
                    Ok(())
                });
            }
            kind::MLS_OPENED => {
                let Ok(message) = v1::MlsOpened::decode(payload) else {
                    return;
                };
                if let Ok(conversation) = conversation_id(&message.conversation)
                    && self.allowed(session, Action::MlsJoinInfo(&conversation))
                {
                    self.mls_seen(session, Some(conversation));
                }
            }
            kind::MLS_WELCOME_ACK => {
                let message = decoded!(v1::MlsWelcomeAck);
                let request_id = message.request_id;
                self.deferred(session, request_id, |state| {
                    state.authorize(session, Action::MlsMailbox)?;
                    let account = state.mls_account(session)?;
                    let device = device_id(&message.device)?;
                    if message.ids.len() > MAX_MLS_WELCOMES {
                        return Err((Code::TooLarge, 0));
                    }
                    let mls = state.mls.as_mut().ok_or((Code::Unsupported, 0))?;
                    if mls.limited(account, Bucket::Mailbox) {
                        return Err((Code::RateLimited, 2000));
                    }
                    state.mls_submit(
                        session,
                        request_id,
                        Job::WelcomeAck {
                            account,
                            device,
                            ids: message.ids,
                        },
                    )
                });
            }
            kind::MLS_JOIN_INFO_REQUEST => {
                let message = decoded!(v1::MlsJoinInfoRequest);
                let request_id = message.request_id;
                self.deferred(session, request_id, |state| {
                    let conversation = conversation_id(&message.conversation)?;
                    state.authorize(session, Action::MlsJoinInfo(&conversation))?;
                    if !state.mls_group(&conversation) {
                        return Err((Code::NotFound, 0));
                    }
                    state.mls_submit(
                        session,
                        request_id,
                        Job::JoinInfo {
                            conversation,
                            tree: message.tree,
                        },
                    )
                });
            }
            _ => self.extension_error(session, 0, (Code::Unsupported, 0)),
        }
    }

    fn mls_commit(&mut self, session: u32, message: v1::MlsCommit) -> Result<(), Failure> {
        let conversation = conversation_id(&message.conversation)?;
        nuschel::id(&message.idempotency_key).ok_or((Code::Malformed, 0))?;
        if message.commit.len() > MAX_MLS_HANDSHAKE_BYTES
            || message.group_info.len() > MAX_MLS_GROUP_INFO_BYTES
            || message.welcome.len() > MAX_MLS_WELCOME_BYTES
        {
            return Err((Code::TooLarge, 0));
        }
        // Commits share the text bucket with messages (each one costs a validation
        // in the service task).
        self.text_limited(session)?;
        let account = self.mls_account(session)?;
        self.authorize(session, Action::MlsCommit(&conversation))?;
        // A direct conversation's two accounts; a channel's committer, who may
        // add only itself (an external join, or the group's creation).
        let (allowed, channel) = match self.conversations.kind(&conversation) {
            Some(Kind::Direct(a, b)) => (vec![a, b], false),
            Some(Kind::Channel(_)) => (vec![account], true),
            _ => return Err((Code::Unsupported, 0)),
        };
        let request_id = message.request_id;
        let mls = self.mls.as_mut().ok_or((Code::Unsupported, 0))?;
        match mls.groups.get_mut(&conversation) {
            Some(cache) if cache.committing => return Err((Code::Busy, 500)),
            // A creation for an existing group is answered by the service: stale,
            // or the original acknowledgement of a retry.
            Some(cache) => cache.committing = true,
            None if message.create.is_none() => return Err((Code::NotFound, 0)),
            None => {
                mls.groups.insert(
                    conversation,
                    GroupCache {
                        group_id: group_id(&conversation),
                        committing: true,
                        channel,
                        ..GroupCache::default()
                    },
                );
            }
        }
        self.mls_seen(session, Some(conversation));
        let submitted = self.mls_submit(
            session,
            request_id,
            Job::Commit {
                account,
                conversation,
                allowed,
                commit: message,
            },
        );
        if submitted.is_err() {
            self.mls_commit_ended(&conversation);
        }
        submitted
    }

    /// A commit that did not get as far as storage.
    fn mls_commit_ended(&mut self, conversation: &Id) {
        if let Some(mls) = self.mls.as_mut()
            && let Some(cache) = mls.groups.get_mut(conversation)
        {
            cache.committing = false;
            if !cache.established {
                mls.groups.remove(conversation);
            }
        }
    }

    fn mls_send(&mut self, session: u32, message: v1::MlsSend) -> Result<(), Failure> {
        let conversation = conversation_id(&message.conversation)?;
        let key = nuschel::id(&message.idempotency_key).ok_or((Code::Malformed, 0))?;
        if message.message.len() > MAX_MLS_APPLICATION_BYTES {
            return Err((Code::TooLarge, 0));
        }
        self.text_limited(session)?;
        let account = self.mls_account(session)?;
        self.authorize(session, Action::MlsSend(&conversation))?;
        let cache = self
            .mls
            .as_ref()
            .and_then(|mls| mls.groups.get(&conversation))
            .filter(|cache| cache.established)
            .cloned()
            .ok_or((Code::NotFound, 0))?;
        // Framing only: a private application message of this group (no OpenMLS
        // under the lock). Members check the rest.
        let facts = prevalidate::message(&message.message).map_err(|_| (Code::Malformed, 0))?;
        if facts.wire_format != WireFormat::PrivateMessage
            || facts.content_type != ContentType::Application
            || facts.group_id != cache.group_id
        {
            return Err((Code::Malformed, 0));
        }
        if facts.epoch != cache.epoch {
            let head = self.conversations.head(&conversation);
            self.send(
                session,
                kind::ERROR,
                &nuschel::stale_epoch(message.request_id, cache.epoch, head),
            );
            self.end_request(session, message.request_id);
            return Ok(());
        }
        self.mls_seen(session, Some(conversation));
        let format = v1::Format::MlsApplication as u8;
        let (_, user_id, name) = self.mls_author(session);
        let seq = self.conversations.next_seq(conversation);
        let row = StoredMessage {
            conversation: hex(&conversation),
            seq,
            kind: MessageKind::Message,
            author_account: Some(hex(&account)),
            author_user_id: user_id,
            author_name: name,
            format,
            content: base64(&message.message),
            sent_ms: crate::chat::now_ms(),
            idempotency_key: Some(crate::tls::hex(&key)),
            // The key names the message: a resend after STALE_EPOCH re-encrypts the
            // same envelope and gets the original seq back.
            body_hash: Some(crate::tls::hex(&sha256(&[&conversation, &[format], &key]))),
            ..StoredMessage::default()
        };
        let wire = self.wire_message(row.clone());
        self.history.store(
            Request::HistoryAppend { message: row },
            Done::Mls(Box::new(Stored {
                session,
                request_id: message.request_id,
                conversation,
                seq,
                epoch: cache.epoch,
                message: wire,
                commit: None,
                proposal: false,
            })),
        );
        Ok(())
    }

    fn mls_author(&self, session: u32) -> (Option<Id>, Option<u32>, String) {
        self.users
            .get(&session)
            .map_or((None, None, String::new()), |user| {
                (user.account, user.user_id, user.name.clone())
            })
    }

    /// The service's answer.
    pub fn mls_answer(&mut self, session: u32, request_id: u64, reply: Reply) {
        match reply {
            Reply::Status(mut status) => {
                status.request_id = request_id;
                self.send(session, kind::MLS_KEY_PACKAGE_STATUS, &status);
                self.end_request(session, request_id);
            }
            Reply::Packages(mut packages) => {
                packages.request_id = request_id;
                self.send(session, kind::MLS_KEY_PACKAGES, &packages);
                self.end_request(session, request_id);
            }
            Reply::Welcomes(mut welcomes) => {
                welcomes.request_id = request_id;
                self.send(session, kind::MLS_WELCOMES, &welcomes);
                self.end_request(session, request_id);
            }
            Reply::JoinInfo(mut info) => {
                info.request_id = request_id;
                self.send(session, kind::MLS_JOIN_INFO, &info);
                self.end_request(session, request_id);
            }
            Reply::Done => {
                self.send(session, kind::DONE, &v1::Done { request_id });
                self.end_request(session, request_id);
            }
            Reply::Ack(mut ack) => {
                if let Some(conversation) = nuschel::id(&ack.conversation) {
                    self.mls_commit_ended(&conversation);
                }
                ack.request_id = request_id;
                self.send(session, kind::MLS_ACK, &ack);
                self.end_request(session, request_id);
            }
            Reply::Stale {
                conversation,
                epoch,
            } => {
                self.mls_commit_ended(&conversation);
                let head = self.conversations.head(&conversation);
                self.send(
                    session,
                    kind::ERROR,
                    &nuschel::stale_epoch(request_id, epoch, head),
                );
                self.end_request(session, request_id);
            }
            Reply::Failed(failure) => self.failed(session, request_id, failure),
            Reply::CommitFailed {
                conversation,
                failure,
            } => {
                self.mls_commit_ended(&conversation);
                self.failed(session, request_id, failure);
            }
            Reply::Accepted(accepted) => self.mls_accepted(session, request_id, *accepted),
            Reply::Proposed {
                conversation,
                epoch,
                devices,
                rows,
            } => self.mls_proposed(conversation, epoch, devices, rows),
        }
    }

    /// The server's removal proposals: rows of the stream, from nobody (the
    /// server signed them). Once all are stored a member is asked to commit.
    fn mls_proposed(
        &mut self,
        conversation: Id,
        epoch: u64,
        devices: BTreeSet<Device>,
        rows: Vec<Vec<u8>>,
    ) {
        let Some(cache) = self
            .mls
            .as_mut()
            .and_then(|mls| mls.groups.get_mut(&conversation))
        else {
            return;
        };
        if cache.epoch != epoch || cache.committing {
            // The group moved on meanwhile: the next sweep proposes again.
            cache.proposed = None;
            return;
        }
        cache.proposed = Some(Proposed {
            epoch,
            devices,
            stored: 0,
            total: rows.len(),
            task: None,
            asked: BTreeSet::new(),
        });
        for row in rows {
            let seq = self.conversations.next_seq(conversation);
            let stored = StoredMessage {
                conversation: hex(&conversation),
                seq,
                kind: MessageKind::Message,
                author_account: None,
                author_user_id: None,
                author_name: String::new(),
                format: v1::Format::MlsHandshake as u8,
                content: base64(&row),
                sent_ms: crate::chat::now_ms(),
                idempotency_key: None,
                body_hash: Some(crate::tls::hex(&sha256(&[&row]))),
                ..StoredMessage::default()
            };
            let wire = self.wire_message(stored.clone());
            self.history.store(
                Request::HistoryAppend { message: stored },
                Done::Mls(Box::new(Stored {
                    session: 0,
                    request_id: 0,
                    conversation,
                    seq,
                    epoch,
                    message: wire,
                    commit: None,
                    proposal: true,
                })),
            );
        }
    }

    /// A device is here (and has `conversation` open, if given): its activity
    /// clocks restart.
    pub(crate) fn mls_seen(&mut self, session: u32, conversation: Option<Id>) {
        let Some(mls) = self.mls.as_mut() else {
            return;
        };
        let Some((account, device)) = mls.device_of(session) else {
            return;
        };
        let now = mls.now_ms();
        let mut records = Vec::new();
        records.extend(MlsHandle::touch(
            &mut mls.seen,
            PREFIX_SEEN,
            (account, device),
            now,
        ));
        if let Some(conversation) = conversation {
            records.extend(MlsHandle::touch(
                &mut mls.opened,
                PREFIX_OPENED,
                (conversation, device),
                now,
            ));
        }
        if !records.is_empty() {
            let _ = self.mls_submit(session, 0, Job::Touch(records));
        }
    }

    /// Server-driven removals (docs/e2ee.md §6.2), for every group: devices whose
    /// account can no longer read the channel, devices not seen for 90 days, and
    /// channel members that did not open it for 30 days are proposed for removal,
    /// and one online member is asked to commit (another after
    /// [`TASK_TIMEOUT_MS`]). Runs after access changes, when a device connects or a
    /// commit is stored, and every minute.
    pub fn mls_sweep(&mut self) {
        // Connected devices are seen now.
        let connected: Vec<u32> = self
            .mls
            .as_ref()
            .map(|mls| mls.listening.keys().copied().collect())
            .unwrap_or_default();
        for session in connected {
            self.mls_seen(session, None);
        }
        let Some(mls) = self.mls.as_ref() else {
            return;
        };
        let now = mls.now_ms();
        let online = self.sessions_by_account();
        let mut work = Vec::new();
        for (conversation, cache) in &mls.groups {
            if !cache.established || cache.committing || cache.members.is_empty() {
                continue;
            }
            let kind = self.conversations.kind(conversation);
            let mut remove = BTreeSet::new();
            for (device, account) in &cache.members {
                let lost = match kind {
                    Some(Kind::Channel(channel)) => {
                        !self.account_may_read(&online, account, channel)
                    }
                    Some(Kind::Direct(..)) => self.registry.by_account(account).is_none(),
                    _ => false,
                };
                let idle = |map: &HashMap<(Id, Device), Touch>, key, days: u64| {
                    map.get(&key)
                        .is_some_and(|touch| now.saturating_sub(touch.at) > days * DAY_MS)
                };
                let inactive = idle(&mls.seen, (*account, *device), INACTIVE_DEVICE_DAYS);
                let unopened = cache.channel
                    && idle(&mls.opened, (*conversation, *device), UNOPENED_CHANNEL_DAYS);
                if lost || inactive || unopened {
                    remove.insert(*device);
                }
            }
            work.push((*conversation, remove));
        }
        for (conversation, remove) in work {
            self.mls_reconcile(conversation, remove, now);
        }
    }

    /// One group's removals: propose them, or ask a member to commit them.
    fn mls_reconcile(&mut self, conversation: Id, remove: BTreeSet<Device>, now: u64) {
        let Some(mls) = self.mls.as_mut() else {
            return;
        };
        let Some(cache) = mls.groups.get_mut(&conversation) else {
            return;
        };
        if remove.is_empty() {
            cache.proposed = None;
            return;
        }
        // Proposed in this epoch already (or on their way): ask a committer.
        if let Some(proposed) = &cache.proposed
            && proposed.epoch == cache.epoch
            && proposed.devices == remove
        {
            if proposed.stored < proposed.total {
                return;
            }
            if proposed.total == 0 {
                // The group takes no proposals from this server (made before 4c).
                return;
            }
            if proposed
                .task
                .is_some_and(|(_, at)| now.saturating_sub(at) < TASK_TIMEOUT_MS)
            {
                return;
            }
            let members: BTreeSet<(Id, Device)> = cache
                .members
                .iter()
                .filter(|(device, _)| !remove.contains(*device))
                .map(|(device, account)| (*account, *device))
                .collect();
            let asked = proposed.asked.clone();
            let candidate = mls
                .listening
                .iter()
                .filter(|(session, devices)| {
                    !asked.contains(session)
                        && devices.first().is_some_and(|pair| members.contains(pair))
                })
                .map(|(session, _)| *session)
                .min();
            let epoch = cache.epoch;
            let Some(session) = candidate.or_else(|| {
                // Everyone was asked once: start over with whoever is here.
                mls.listening
                    .iter()
                    .filter(|(_, devices)| {
                        devices.first().is_some_and(|pair| members.contains(pair))
                    })
                    .map(|(session, _)| *session)
                    .min()
            }) else {
                return;
            };
            if let Some(proposed) = mls
                .groups
                .get_mut(&conversation)
                .and_then(|cache| cache.proposed.as_mut())
            {
                if proposed.asked.contains(&session) {
                    proposed.asked.clear();
                }
                proposed.asked.insert(session);
                proposed.task = Some((session, now));
            }
            if let Some(user) = self.users.get(&session) {
                user.link.message(
                    kind::MLS_TASK,
                    &v1::MlsTask {
                        conversation: conversation.to_vec(),
                        epoch,
                        commit: true,
                        grant: false,
                    },
                );
            }
            return;
        }
        let epoch = cache.epoch;
        cache.proposed = Some(Proposed {
            epoch,
            devices: remove.clone(),
            stored: 0,
            total: usize::MAX,
            task: None,
            asked: BTreeSet::new(),
        });
        let _ = self.mls_submit(
            0,
            0,
            Job::Propose {
                conversation,
                devices: remove,
            },
        );
    }

    /// The service applied a commit: give it its seq and store it.
    fn mls_accepted(&mut self, session: u32, request_id: u64, accepted: Accepted) {
        let Accepted {
            conversation,
            group_id,
            epoch,
            commit,
            account,
            key,
            hash,
            members,
            added,
            channel,
        } = accepted;
        let seq = self.conversations.next_seq(conversation);
        let Some(mls) = self.mls.as_mut() else {
            return;
        };
        let cache = mls
            .groups
            .entry(conversation)
            .or_insert_with(|| GroupCache {
                group_id: group_id.clone(),
                committing: true,
                ..GroupCache::default()
            });
        let rollback = CommitRollback {
            group_id: group_id.clone(),
            created: !cache.established,
            epoch: cache.epoch,
            commit_seq: cache.commit_seq,
            members,
            added,
            channel,
        };
        // From here on, application messages must be for the new epoch.
        cache.epoch = epoch;
        cache.commit_seq = seq;
        cache.established = true;
        let (_, user_id, name) = self.mls_author(session);
        let row = StoredMessage {
            conversation: hex(&conversation),
            seq,
            kind: MessageKind::Message,
            author_account: Some(hex(&account)),
            author_user_id: user_id,
            author_name: name,
            format: v1::Format::MlsHandshake as u8,
            content: base64(&commit),
            sent_ms: crate::chat::now_ms(),
            idempotency_key: Some(crate::tls::hex(&key)),
            body_hash: Some(crate::tls::hex(&hash)),
            ..StoredMessage::default()
        };
        let wire = self.wire_message(row.clone());
        self.history.store(
            Request::HistoryAppend { message: row },
            Done::Mls(Box::new(Stored {
                session,
                request_id,
                conversation,
                seq,
                epoch,
                message: wire,
                commit: Some(rollback),
                proposal: false,
            })),
        );
    }

    /// An MLS row was stored (or not).
    pub fn mls_stored(
        &mut self,
        stored: Stored,
        response: Option<nuschel_ipc::server::ServerStorageResponse>,
    ) {
        use nuschel_ipc::server::ServerStorageResponse as Response;
        let Stored {
            session,
            request_id,
            conversation,
            seq,
            epoch,
            message,
            commit,
            proposal,
        } = stored;
        if proposal {
            if matches!(
                response,
                Some(Response::Appended {
                    duplicate: false,
                    ..
                })
            ) {
                self.deliver(
                    &conversation,
                    Delivery::Message {
                        message,
                        stock: None,
                    },
                    0,
                );
                if let Some(proposed) = self
                    .mls
                    .as_mut()
                    .and_then(|mls| mls.groups.get_mut(&conversation))
                    .and_then(|cache| cache.proposed.as_mut())
                    .filter(|proposed| proposed.epoch == epoch)
                {
                    proposed.stored += 1;
                }
            } else if let Some(cache) = self
                .mls
                .as_mut()
                .and_then(|mls| mls.groups.get_mut(&conversation))
            {
                // Not in the stream: propose again later.
                cache.proposed = None;
            }
            self.mls_sweep();
            return;
        }
        let outcome = match (&commit, response) {
            (
                _,
                Some(Response::Appended {
                    duplicate: false, ..
                }),
            ) => Ok((seq, true)),
            // A resend of an application message stored before: its original seq.
            (
                None,
                Some(Response::Appended {
                    seq: original,
                    duplicate: true,
                }),
            ) => Ok((original, false)),
            (_, Some(Response::Conflict)) => Err((Code::IdempotencyConflict, 0)),
            _ => Err((Code::Busy, 1000)),
        };
        if let Some(rollback) = &commit {
            let ok = outcome.is_ok();
            if let Some(mls) = self.mls.as_mut() {
                let now = mls.now_ms();
                if let Some(cache) = mls.groups.get_mut(&conversation) {
                    cache.committing = false;
                    if ok {
                        cache.members = rollback.members.clone();
                        cache.channel = rollback.channel;
                        cache.proposed = None;
                    } else {
                        cache.epoch = rollback.epoch;
                        cache.commit_seq = rollback.commit_seq;
                    }
                }
                // A new member's clocks start now (it may not have connected
                // through a mailbox fetch yet).
                if ok {
                    for device in &rollback.added {
                        if let Some(account) = rollback.members.get(device) {
                            mls.seen.entry((*account, *device)).or_insert(Touch {
                                at: now,
                                written: 0,
                            });
                            mls.opened.entry((conversation, *device)).or_insert(Touch {
                                at: now,
                                written: 0,
                            });
                        }
                    }
                }
                if !ok && rollback.created {
                    mls.groups.remove(&conversation);
                }
                let _ = mls.stored.send(StoredNotice {
                    group_id: rollback.group_id.clone(),
                    seq,
                    ok,
                });
            }
        }
        match outcome {
            Ok((seq, fresh)) => {
                self.send(
                    session,
                    kind::MLS_ACK,
                    &v1::MlsAck {
                        request_id,
                        conversation: conversation.to_vec(),
                        seq,
                        epoch,
                    },
                );
                self.end_request(session, request_id);
                if fresh {
                    self.deliver(
                        &conversation,
                        Delivery::Message {
                            message,
                            stock: None,
                        },
                        session,
                    );
                }
            }
            Err(failure) => self.failed(session, request_id, failure),
        }
        if commit.is_some() {
            // The epoch moved: proposals of the old one are void, and the
            // members may have changed.
            self.mls_sweep();
        }
    }

    /// Welcomes for devices whose connections fetched their mailbox.
    pub fn mls_push(&mut self, pushes: Vec<(Id, Device, v1::MlsWelcome)>) {
        let Some(mls) = self.mls.as_mut() else {
            return;
        };
        for (account, device, welcome) in pushes {
            let Some(listeners) = mls.listeners.get_mut(&(account, device)) else {
                continue;
            };
            listeners.retain(|session| {
                self.users
                    .get(session)
                    .is_some_and(|user| user.account == Some(account))
            });
            let push = v1::MlsWelcomePush {
                welcome: Some(welcome),
            };
            for session in listeners.iter() {
                if let Some(user) = self.users.get(session) {
                    user.link.message(kind::MLS_WELCOME_PUSH, &push);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_records_round_trip() {
        let record = PendingRecord {
            after_seq: 7,
            epoch: 3,
            hash: [5; 32],
            welcome: Some(vec![1, 2, 3]),
            recipients: vec![([1; 16], [2; 16], true, vec![9; 32])],
            group: vec![4; 10],
        };
        let decoded = decode_pending(&encode_pending(&record)).unwrap();
        assert_eq!(
            (decoded.after_seq, decoded.epoch, decoded.hash),
            (7, 3, [5; 32])
        );
        assert_eq!(decoded.welcome, Some(vec![1, 2, 3]));
        assert_eq!(decoded.recipients, record.recipients);
        assert_eq!(decoded.group, record.group);
        let plain = decode_pending(&pending_record(1, 2, b"commit", &[8])).unwrap();
        assert_eq!((plain.welcome, plain.hash), (None, sha256(&[b"commit"])));
        assert!(decode_pending(&[2, 0]).is_none());
    }

    #[test]
    fn welcome_listeners_are_bounded_and_forgotten() {
        let mut handle = MlsHandle::detached(MlsConfig::default());
        let account = [1; 16];
        for device in 0..20u8 {
            handle.listen(u32::from(device) + 1, account, [device; 16], |_| true);
        }
        assert_eq!(handle.listeners.len(), nuschel::MAX_KT_DEVICES);
        // Entries of gone connections make room again.
        handle.listen(99, account, [99; 16], |session| session > 4);
        assert_eq!(handle.listeners.len(), nuschel::MAX_KT_DEVICES - 3);
        for session in 1..=99 {
            handle.disconnected(session);
        }
        assert!(handle.listeners.is_empty());
        assert!(handle.listening.is_empty());
        // Fetches and acknowledgements are rate-limited per account.
        let limited = (0..200).any(|_| handle.limited(account, Bucket::Mailbox));
        assert!(limited);
    }

    #[test]
    fn epoch_records_keep_the_members_and_old_ones_still_read() {
        let members: BTreeMap<Device, Id> = [([1; 16], [2; 16]), ([3; 16], [4; 16])].into();
        let value = encode_epoch(7, 9, &[5; 16], true, &members);
        let (conversation, cache) = decode_epoch(&[6; 20], &value).unwrap();
        assert_eq!(conversation, [5; 16]);
        assert_eq!((cache.epoch, cache.commit_seq, cache.channel), (7, 9, true));
        assert_eq!(cache.members, members);
        // A record from before phase 4c: no flag, no members.
        let (_, old) = decode_epoch(&[6; 20], &value[..32]).unwrap();
        assert!(!old.channel && old.members.is_empty());
        assert!(decode_epoch(&[6; 20], &value[..40]).is_none());
    }

    #[test]
    fn mls_rows_carry_their_message_as_base64() {
        let (content, mls) = row_content(v1::Format::MlsHandshake as u8, base64(&[1, 2, 3]));
        assert_eq!((content.as_str(), mls.as_slice()), ("", &[1u8, 2, 3][..]));
        let (content, mls) = row_content(v1::Format::Text as u8, "hi".into());
        assert_eq!((content.as_str(), mls.len()), ("hi", 0));
    }
}
