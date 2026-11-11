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
//! - **Persistence** is the server's key-value table (`kt_records`) through its own
//!   storage worker, under uppercase prefixes (nuschel-kt uses lowercase ones, the
//!   key-transparency module `R` and `C`):
//!
//!   | Key | Value |
//!   | --- | --- |
//!   | `E` ‖ group id | epoch (8), newest commit's seq (8), conversation (16) |
//!   | `G` ‖ group id | version 1, conversation, commit seq, recent commit keys, `DeliveryGroup::state` |
//!   | `P` ‖ account ‖ device ‖ time (8) ‖ n (4) | a one-time key package |
//!   | `L` ‖ account ‖ device | the device's last-resort key package |
//!   | `K` ‖ key package ref | account ‖ device ‖ last resort (1): where a Welcome goes |
//!   | `W` ‖ account ‖ device ‖ id (16) | a waiting Welcome (`v1::MlsWelcome`) |
//!
//!   An empty value deletes a key.
//!
//! Known limit: a crash between storing a commit's row and saving the group's
//! state leaves the server's view one epoch behind its stream (replaying stored
//! commits after the saved commit seq on start is the follow-up).
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
    ds::{DeliveryGroup, DsError, check_key_package},
    prevalidate::{self, ContentType, WireFormat},
};
use nuschel_identity::{DeviceCertificate, DeviceCredential, IdentityFingerprint};
use nuschel_ipc::server::{MessageKind, ServerStorageRequest as Request, StoredMessage};
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
    collections::{BTreeSet, HashMap, VecDeque},
    sync::Arc,
    time::Instant,
};
use tokio::sync::mpsc;

/// Requests waiting for the task, server-wide.
const QUEUE: usize = 256;
/// Commit idempotency keys remembered per group.
const RECENT: usize = 64;
/// Accounts with a rate-limit bucket before the oldest are forgotten.
const MAX_BUCKETS: usize = 10_000;
const PREFIX_EPOCH: u8 = b'E';
const PREFIX_GROUP: u8 = b'G';
const PREFIX_PACKAGE: u8 = b'P';
const PREFIX_LAST_RESORT: u8 = b'L';
const PREFIX_REFERENCE: u8 = b'K';
const PREFIX_WELCOME: u8 = b'W';

type Device = [u8; 16];
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
#[derive(Clone, Debug)]
pub struct GroupCache {
    pub group_id: Vec<u8>,
    pub epoch: u64,
    /// The newest commit's seq.
    pub commit_seq: u64,
    /// Accepted by the service (a creation in flight is not yet).
    pub established: bool,
    /// A commit is being validated or stored.
    pub committing: bool,
}

/// The handlers' end: the queue, each group's epoch and the Welcome listeners.
pub struct MlsHandle {
    tasks: mpsc::Sender<Task>,
    stored: mpsc::UnboundedSender<StoredNotice>,
    pub groups: HashMap<Id, GroupCache>,
    /// (account, device) → connections that fetched its mailbox.
    listeners: HashMap<(Id, Device), BTreeSet<u32>>,
    claims: HashMap<Id, LeakyBucket>,
    uploads: HashMap<Id, LeakyBucket>,
    config: MlsConfig,
}

impl MlsHandle {
    pub fn has_group(&self, conversation: &Id) -> bool {
        self.groups.contains_key(conversation)
    }

    fn limited(&mut self, account: Id, claim: bool) -> bool {
        let now = Instant::now();
        let (buckets, per_minute) = if claim {
            (&mut self.claims, self.config.claims_per_minute)
        } else {
            (&mut self.uploads, self.config.uploads_per_minute)
        };
        if buckets.len() >= MAX_BUCKETS && !buckets.contains_key(&account) {
            buckets.clear();
        }
        buckets
            .entry(account)
            .or_insert_with(|| LeakyBucket::per_minute(per_minute, per_minute.div_ceil(2), now))
            .limited(now)
    }

    /// A handle whose queue goes nowhere (fuzzing and unit tests).
    pub fn detached(config: MlsConfig) -> Self {
        Self {
            tasks: mpsc::channel(1).0,
            stored: mpsc::unbounded_channel().0,
            groups: HashMap::new(),
            listeners: HashMap::new(),
            claims: HashMap::new(),
            uploads: HashMap::new(),
            config,
        }
    }
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
}

struct CommitRollback {
    group_id: Vec<u8>,
    created: bool,
    epoch: u64,
    commit_seq: u64,
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
) -> Result<(MlsHandle, Service), crate::server::StartError> {
    let store = StorageKt::new(storage);
    let epochs = store
        .scan_prefix(&[PREFIX_EPOCH])
        .await
        .map_err(|error| crate::server::StartError::Storage(format!("mls: {error}")))?;
    let mut groups = HashMap::new();
    for (key, value) in epochs {
        if value.len() != 32 {
            continue;
        }
        let Ok(conversation) = <Id>::try_from(&value[16..32]) else {
            continue;
        };
        groups.insert(
            conversation,
            GroupCache {
                group_id: key[1..].to_vec(),
                epoch: u64::from_be_bytes(value[..8].try_into().unwrap_or_default()),
                commit_seq: u64::from_be_bytes(value[8..16].try_into().unwrap_or_default()),
                established: true,
                committing: false,
            },
        );
    }
    tracing::info!(groups = groups.len(), "MLS delivery service");
    let (tasks, receiver) = mpsc::channel(QUEUE);
    let (stored, stored_receiver) = mpsc::unbounded_channel();
    Ok((
        MlsHandle {
            tasks,
            stored,
            groups,
            listeners: HashMap::new(),
            claims: HashMap::new(),
            uploads: HashMap::new(),
            config: config.clone(),
        },
        Service {
            store,
            tasks: receiver,
            stored: stored_receiver,
            groups: HashMap::new(),
            server_id,
            config: config.clone(),
        },
    ))
}

fn encode_group(loaded: &Loaded) -> Vec<u8> {
    let state = loaded.group.state();
    let mut out = Vec::with_capacity(64 + loaded.recent.len() * 80 + state.len());
    out.push(1);
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
    if take(1)? != [1] {
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
    let state = bytes.get(at..)?;
    let group = DeliveryGroup::restore(group_id, state).ok()?;
    Some(Loaded {
        group,
        conversation,
        commit_seq,
        recent,
        pending: None,
    })
}

fn credential_identity(credential: &[u8]) -> Option<(IdentityFingerprint, DeviceCredential)> {
    let credential = DeviceCredential::decode(credential).ok()?;
    Some((credential.identity(), credential))
}

impl Service {
    pub async fn run(mut self, server: Arc<Server>) {
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
        };
        result.unwrap_or_else(Reply::Failed)
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
                records.push((
                    key(
                        PREFIX_PACKAGE,
                        &[
                            &account,
                            &id,
                            &now.to_be_bytes(),
                            &(index as u32).to_be_bytes(),
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
        let drop: Vec<(Vec<u8>, Vec<u8>)> = existing
            .iter()
            .take(excess)
            .map(|(key, _)| (key.clone(), Vec::new()))
            .collect();
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
                )
                .map_err(|error| {
                    tracing::info!(%error, "refused a new MLS group");
                    (Code::Malformed, 0)
                })?;
                self.groups.insert(
                    group_id.clone(),
                    Loaded {
                        group,
                        conversation,
                        commit_seq: 0,
                        recent: VecDeque::new(),
                        pending: None,
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
        // The identities that may be in the group, before touching it.
        let mut identities = Vec::new();
        for allowed in allowed {
            if let Some(roster) = self.roster(allowed).await? {
                identities.push(roster.identity);
            }
        }
        let max_devices = self.config.max_devices as usize;
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
        // Every member must belong to one of the conversation's identities.
        let strangers = outcome.members.iter().any(|member| {
            credential_identity(&member.credential)
                .is_none_or(|(identity, _)| !identities.contains(&identity))
        });
        if strangers || outcome.members.len() > max_devices {
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
        loaded.pending = Some(Pending {
            previous,
            welcome: welcome.map(<[u8]>::to_vec),
            recipients,
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
        Ok(Reply::Accepted(Box::new(Accepted {
            conversation,
            group_id: group_id.to_vec(),
            epoch,
            commit: commit.commit.clone(),
            account,
            key: key_bytes,
            hash,
        })))
    }

    /// A commit's row was stored (or not): save the group and deliver the Welcome,
    /// or go back. Returns the Welcomes to push.
    async fn stored(&mut self, notice: StoredNotice) -> Vec<(Id, Device, v1::MlsWelcome)> {
        let Some(loaded) = self.groups.get_mut(&notice.group_id) else {
            return Vec::new();
        };
        let Some(pending) = loaded.pending.take() else {
            return Vec::new();
        };
        if !notice.ok {
            match &pending.previous {
                None => {
                    self.groups.remove(&notice.group_id);
                }
                Some(previous) => {
                    if let Ok(group) = DeliveryGroup::restore(&notice.group_id, previous) {
                        loaded.group = group;
                    }
                    loaded.recent.pop_back();
                }
            }
            return Vec::new();
        }
        loaded.commit_seq = notice.seq;
        if let Some(last) = loaded.recent.back_mut()
            && last.seq == 0
        {
            last.seq = notice.seq;
        }
        let conversation = loaded.conversation;
        let mut epoch_value = pending.epoch.to_be_bytes().to_vec();
        epoch_value.extend_from_slice(&notice.seq.to_be_bytes());
        epoch_value.extend_from_slice(&conversation);
        let mut records = vec![
            (key(PREFIX_GROUP, &[&notice.group_id]), encode_group(loaded)),
            (key(PREFIX_EPOCH, &[&notice.group_id]), epoch_value),
        ];
        let mut pushes = Vec::new();
        if let Some(welcome) = &pending.welcome {
            for (account, device, last_resort, reference) in &pending.recipients {
                let mut id = now_ms().to_be_bytes().to_vec();
                id.extend_from_slice(&crate::state::random::<8>());
                let entry = v1::MlsWelcome {
                    id: id.clone(),
                    conversation: conversation.to_vec(),
                    seq: notice.seq,
                    epoch: pending.epoch,
                    welcome: welcome.clone(),
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
        if let Err(error) = self.store.write(records).await {
            // The row is in the stream; the next start replays from the saved state.
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
        Ok(Reply::JoinInfo(v1::MlsJoinInfo {
            request_id: 0,
            conversation: conversation.to_vec(),
            epoch: loaded.group.epoch(),
            group_info,
            tree: if tree { tree_bytes } else { Vec::new() },
            commit_seq: loaded.commit_seq,
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
    /// Whether the conversation has (or is getting) an MLS group: plaintext,
    /// edits and deletions are refused for it (docs/e2ee.md §7.2).
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
                    if mls.limited(account, false) {
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
                    if mls.limited(account, true) {
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
                    let mls = state.mls.as_mut().ok_or((Code::Unsupported, 0))?;
                    let listeners = mls.listeners.entry((account, device)).or_default();
                    listeners.retain(|session| state.users.contains_key(session));
                    listeners.insert(session);
                    state.mls_submit(session, request_id, Job::Welcomes { account, device })
                });
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
        // Direct conversations only for now (channels are phase 4c).
        let allowed = match self.conversations.kind(&conversation) {
            Some(Kind::Direct(a, b)) => vec![a, b],
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
                        epoch: 0,
                        commit_seq: 0,
                        established: false,
                        committing: true,
                    },
                );
            }
        }
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
        }
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
                epoch: 0,
                commit_seq: 0,
                established: false,
                committing: true,
            });
        let rollback = CommitRollback {
            group_id: group_id.clone(),
            created: !cache.established,
            epoch: cache.epoch,
            commit_seq: cache.commit_seq,
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
        } = stored;
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
                if let Some(cache) = mls.groups.get_mut(&conversation) {
                    cache.committing = false;
                    if !ok {
                        cache.epoch = rollback.epoch;
                        cache.commit_seq = rollback.commit_seq;
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
    fn mls_rows_carry_their_message_as_base64() {
        let (content, mls) = row_content(v1::Format::MlsHandshake as u8, base64(&[1, 2, 3]));
        assert_eq!((content.as_str(), mls.as_slice()), ("", &[1u8, 2, 3][..]));
        let (content, mls) = row_content(v1::Format::Text as u8, "hi".into());
        assert_eq!((content.as_str(), mls.len()), ("hi", 0));
    }
}
