// SPDX-License-Identifier: EUPL-1.2
//! Key transparency (docs/e2ee.md §5): the server's verifiable key directory.
//!
//! - **The log** is nuschl-kt's `AkdDirectory` on the `kt_records` table, through
//!   its own storage worker (`StorageKt`). Its head-signing key (the server's
//!   identity key) and VRF secret come from credential files, or are generated
//!   once in `state_directory`.
//! - **Publishing.** A registered user binds their own account: `KtPublish` carries
//!   the identity root, its device-signing certificate (signed by the root), the
//!   device certificates and revocations, and the identity's `AccountStatement`
//!   for this account on this log (signed by the device-signing key). Everything
//!   must verify under the root, so the upload proves possession of the identity
//!   and the server cannot bind an identity to an account without its consent. The
//!   value (`AccountBinding` format 3) waits for the next epoch: every
//!   `publish_interval` while bindings wait, and the current epoch is re-signed
//!   every `refresh_interval` otherwise.
//! - **The directory API.** Beside each value the server keeps the record it
//!   commits to (`R ‖ account ‖ SHA-256(value)`) and which one is current
//!   (`C ‖ account`), in the same table: nuschl-kt's keys start with lowercase
//!   letters. One task serves every request in order, so a lookup never sees a
//!   new value with an old record.
//! - **Who may ask** (decided in authz.rs, `Action::Kt*`). Lookups: any
//!   connection, for any registered account of this server. Histories: the
//!   account's owner and its contacts (a direct conversation, or the same
//!   channel now), or everyone with `history = "members"`; checked again when
//!   the answer is sent, as history pages are. Audits and heads: anyone. Each
//!   kind has its own rate limit.
//! - **Names and registration numbers** (`kt-witness/1`, docs/e2ee.md §5.9): every
//!   registered user's `reg:<number>` and `name:<lower-case name>` labels name
//!   their account, published with the bindings; a rename releases the old name
//!   (a visible entry). What was published is kept as `N ‖ label` records, so a
//!   restart publishes only what changed meanwhile. Lookups by number carry both
//!   proofs; owners monitor their own.
//! - **Witnessing** (crate::kt_witness): replies carry the newest cosigned
//!   checkpoint of the epoch log with the head's inclusion proof.
use crate::{
    authz::Action,
    config::{Kt as KtConfig, KtHistoryAccess},
    conversations::Id,
    server::{Server, StartError},
    state::State,
    storage::StorageHandle,
};
use async_trait::async_trait;
use base64::Engine as _;
use nuschl_identity::{
    AccountStatement, DeviceCertificate, DeviceRevocation, DeviceSigningCertificate,
    HybridPublicKey, VerifiedIdentity,
};
use nuschl_ipc::server::{ServerStorageRequest as Request, ServerStorageResponse as Response};
use nuschl_kt::{
    AccountBinding, AccountId, AkdDirectory, Directory, DirectoryError, DirectoryValue,
    Ed25519HeadSigner, KtStore, Label, LogId, MAX_AUDIT_SPAN, NameBinding, RosterCommitment,
    SignedHead, StoreError, VrfSecret, roster_for_binding,
};
use nuschl_protocol::{
    ControlFrame,
    nuschl::{
        self, MAX_KT_CERTIFICATE_BYTES, MAX_KT_LOOKUP_REPLY_BYTES, MAX_KT_REPLY_BYTES, kind,
        v1::{self, error::Code},
    },
};
use prost::Message as _;
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

/// Requests waiting for the key-transparency task, server-wide.
const QUEUE: usize = 64;
/// Bindings in one epoch; more wait for the next.
const MAX_UPDATES_PER_EPOCH: usize = 1000;
/// Keys per storage request.
const GET_CHUNK: usize = 10_000;
const PREFIX_RECORD: u8 = b'R';
const PREFIX_CURRENT: u8 = b'C';
/// `N ‖ label` → the `reg:` or `name:` value last published for it.
const PREFIX_LABEL: u8 = b'N';

// ---- keys ----

/// The log's two secrets.
pub struct Keys {
    signing: Zeroizing<[u8; 32]>,
    vrf: Zeroizing<[u8; 32]>,
}

impl Keys {
    /// From the configured credential files, or generated once in `state_directory`.
    pub fn load(config: &KtConfig, state_directory: &Path) -> io::Result<Self> {
        Ok(Self {
            signing: load_key(
                config.signing_key_file.as_deref(),
                state_directory.join("kt-signing.key"),
            )?,
            vrf: load_key(
                config.vrf_secret_file.as_deref(),
                state_directory.join("kt-vrf.key"),
            )?,
        })
    }

    #[doc(hidden)]
    pub fn from_bytes(signing: [u8; 32], vrf: [u8; 32]) -> Self {
        Self {
            signing: Zeroizing::new(signing),
            vrf: Zeroizing::new(vrf),
        }
    }

    /// The head-signing key's seed, which also signs the MLS groups' external
    /// proposals (crate::mls; docs/e2ee.md §6.2).
    pub(crate) fn signing_seed(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(*self.signing)
    }
}

/// 32 raw bytes, or 64 hex digits with optional surrounding whitespace.
fn parse_key(bytes: &[u8]) -> io::Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0u8; 32]);
    if bytes.len() == 32 {
        key.copy_from_slice(bytes);
        return Ok(key);
    }
    let text = bytes.trim_ascii();
    if text.len() != 64 {
        return Err(io::Error::other(
            "a key file holds 32 bytes or 64 hex digits",
        ));
    }
    for (index, pair) in text.chunks(2).enumerate() {
        let digit = |c: u8| (c as char).to_digit(16);
        match (digit(pair[0]), digit(pair[1])) {
            (Some(high), Some(low)) => key[index] = (high * 16 + low) as u8,
            _ => {
                return Err(io::Error::other(
                    "a key file holds 32 bytes or 64 hex digits",
                ));
            }
        }
    }
    Ok(key)
}

fn load_key(configured: Option<&Path>, generated: PathBuf) -> io::Result<Zeroizing<[u8; 32]>> {
    let read = |path: &Path| -> io::Result<Zeroizing<[u8; 32]>> {
        let bytes = Zeroizing::new(std::fs::read(path)?);
        if bytes.len() > 256 {
            return Err(io::Error::other(format!("{} is too long", path.display())));
        }
        parse_key(&bytes).map_err(|error| io::Error::other(format!("{}: {error}", path.display())))
    };
    if let Some(path) = configured {
        return read(path);
    }
    if generated.exists() {
        return read(&generated);
    }
    let mut key = Zeroizing::new([0u8; 32]);
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut key[..])
        .map_err(|_| io::Error::other("no system randomness"))?;
    let text = Zeroizing::new(format!("{}\n", hex::encode(&key[..])));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(&generated)?, text.as_bytes())?;
    tracing::info!(path = %generated.display(), "generated a key-transparency key");
    Ok(key)
}

// ---- storage ----

fn base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unbase64(text: &str) -> Result<Vec<u8>, StoreError> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|_| StoreError("malformed record from storage".into()))
}

/// nuschl-kt's store on the `kt_records` table, through a storage worker.
#[derive(Clone)]
pub struct StorageKt(StorageHandle);

impl StorageKt {
    pub fn new(storage: StorageHandle) -> Self {
        Self(storage)
    }
}

fn unavailable() -> StoreError {
    StoreError("storage unavailable".into())
}

#[async_trait]
impl KtStore for StorageKt {
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.get_many(&[key.to_vec()]).await?.pop().flatten())
    }

    async fn get_many(&self, keys: &[Vec<u8>]) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        let mut out = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(GET_CHUNK) {
            let request = Request::KtGet {
                keys: chunk.iter().map(|key| base64(key)).collect(),
            };
            match self.0.request(request).await {
                Some(Response::KtValues(values)) if values.len() == chunk.len() => {
                    for value in values {
                        out.push(value.as_deref().map(unbase64).transpose()?);
                    }
                }
                _ => return Err(unavailable()),
            }
        }
        Ok(out)
    }

    async fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StoreError> {
        match self
            .0
            .request(Request::KtScan {
                prefix: base64(prefix),
            })
            .await
        {
            Some(Response::KtRecords(records)) => records
                .iter()
                .map(|(key, value)| Ok((unbase64(key)?, unbase64(value)?)))
                .collect(),
            _ => Err(unavailable()),
        }
    }

    async fn write(&self, records: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), StoreError> {
        let records = records
            .iter()
            .map(|(key, value)| (base64(key), base64(value)))
            .collect();
        match self.0.request(Request::KtWrite { records }).await {
            Some(Response::Done) => Ok(()),
            _ => Err(unavailable()),
        }
    }
}

// ---- requests ----

/// A registered user's number and `name:` label name (lower case).
#[derive(Clone, Debug)]
pub struct Registration {
    pub user_id: u32,
    pub name: String,
}

impl Registration {
    fn labels(&self) -> (Label, Option<Label>) {
        (
            Label::Registration(self.user_id),
            Label::name(self.name.clone()),
        )
    }
}

/// What a connection asked for, already authorised under the state lock.
/// `origin` is the index of the configured origin the client reaches the server
/// under (crate::kt_witness), if any.
pub enum Job {
    Status {
        account: Option<Id>,
        origin: Option<usize>,
        registration: Option<Registration>,
    },
    Publish {
        account: Id,
        publish: v1::KtPublish,
    },
    Lookup {
        account: Id,
        origin: Option<usize>,
        registration: Option<Registration>,
    },
    History {
        account: Id,
        origin: Option<usize>,
        label: Label,
        kind: v1::KtLabel,
    },
    Audit {
        from: u64,
        to: u64,
    },
    Head {
        epoch: u64,
    },
}

pub struct Task {
    pub session: u32,
    pub request_id: u64,
    pub job: Job,
}

/// An answer, sent under the state lock.
pub enum Reply {
    Status(v1::KtStatusReply),
    Published(v1::KtPublished),
    Lookup(v1::KtLookupReply),
    History(v1::KtHistoryReply),
    Audit(v1::KtAuditReply),
    Head(v1::KtHeadReply),
}

/// The handlers' end: the queue and what the hello announces.
#[derive(Clone)]
pub struct KtHandle {
    tasks: mpsc::Sender<Task>,
    pub descriptor: Vec<u8>,
    pub log_id: LogId,
    pub history: KtHistoryAccess,
    pub witnesses: Option<Arc<crate::kt_witness::Witnesses>>,
}

impl KtHandle {
    /// Queue a request; `false` when the task is saturated.
    pub fn submit(&self, task: Task) -> bool {
        self.tasks.try_send(task).is_ok()
    }

    /// A handle whose queue goes nowhere (fuzzing and unit tests).
    pub fn detached(descriptor: Vec<u8>, log_id: LogId) -> Self {
        Self {
            tasks: mpsc::channel(1).0,
            descriptor,
            log_id,
            history: KtHistoryAccess::Contacts,
            witnesses: None,
        }
    }

    /// The configured origin a client named (`host:port`).
    pub fn origin(&self, requested: &str) -> Option<usize> {
        self.witnesses.as_ref()?.origin_index(requested)
    }
}

/// The task's end, started once the server exists.
pub struct Service {
    directory: Arc<AkdDirectory<StorageKt>>,
    witnesses: Arc<crate::kt_witness::Witnesses>,
    store: StorageKt,
    log_id: LogId,
    tasks: mpsc::Receiver<Task>,
    publish_interval: Duration,
    refresh_interval: Duration,
    /// Bindings waiting for the next epoch: the value and its hash.
    pending: BTreeMap<Id, (DirectoryValue, [u8; 32])>,
    /// When the newest head was signed.
    last_head: Instant,
    /// `reg:` and `name:` values as published, and those waiting for an epoch.
    labels: BTreeMap<Label, DirectoryValue>,
    pending_labels: BTreeMap<Label, DirectoryValue>,
    /// The registry generation the labels follow.
    registry_generation: Option<u64>,
}

/// Opens (or creates) the log.
pub async fn open(
    config: &KtConfig,
    keys: Keys,
    storage: StorageHandle,
) -> Result<(KtHandle, Service), StartError> {
    let store = StorageKt(storage);
    let signer = Ed25519HeadSigner::from_seed(&keys.signing);
    let directory = AkdDirectory::open(
        store.clone(),
        VrfSecret::from_bytes(*keys.vrf),
        Box::new(signer),
        now_ms(),
    )
    .await
    .map_err(|error| StartError::Storage(format!("key transparency: {error}")))?;
    let descriptor = directory.descriptor().clone();
    let latest = directory
        .latest_head()
        .await
        .map_err(|error| StartError::Storage(format!("key transparency: {error}")))?;
    let mut labels = BTreeMap::new();
    for (key, value) in store
        .scan_prefix(&[PREFIX_LABEL])
        .await
        .map_err(|error| StartError::Storage(format!("key transparency: {error}")))?
    {
        if let (Ok(label), Ok(value)) = (Label::decode(&key[1..]), DirectoryValue::decode(&value)) {
            labels.insert(label, value);
        }
    }
    let witnesses = Arc::new(crate::kt_witness::Witnesses::new(config));
    let age = Duration::from_millis(now_ms().saturating_sub(latest.issued_at_ms));
    let (tasks, receiver) = mpsc::channel(QUEUE);
    let log_id = descriptor.log_id();
    tracing::info!(
        log = %hex::encode(log_id.0),
        epoch = latest.epoch,
        "key-transparency log"
    );
    Ok((
        KtHandle {
            tasks,
            descriptor: descriptor.encode(),
            log_id,
            history: config.history,
            witnesses: Some(witnesses.clone()),
        },
        Service {
            directory: Arc::new(directory),
            witnesses,
            store,
            log_id,
            tasks: receiver,
            publish_interval: Duration::from_secs(config.publish_interval),
            refresh_interval: Duration::from_secs(config.refresh_interval),
            pending: BTreeMap::new(),
            last_head: Instant::now().checked_sub(age).unwrap_or_else(Instant::now),
            labels,
            pending_labels: BTreeMap::new(),
            registry_generation: None,
        },
    ))
}

pub fn now_ms() -> u64 {
    u64::try_from(crate::chat::now_ms()).unwrap_or(0)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

pub(crate) fn record_key(account: &Id, hash: &[u8; 32]) -> Vec<u8> {
    let mut key = vec![PREFIX_RECORD];
    key.extend_from_slice(account);
    key.extend_from_slice(hash);
    key
}

pub(crate) fn current_key(account: &Id) -> Vec<u8> {
    let mut key = vec![PREFIX_CURRENT];
    key.extend_from_slice(account);
    key
}

fn label(account: &Id) -> Label {
    Label::Account(AccountId(*account))
}

/// Checks a publication (see the module documentation) and builds the value.
pub fn check_publish(
    log_id: LogId,
    account: &Id,
    publish: &v1::KtPublish,
) -> Result<(AccountBinding, Vec<u8>), &'static str> {
    let record = publish.record.as_ref().ok_or("no record")?;
    if !nuschl::valid_kt_record(record) || publish.root.len() > MAX_KT_CERTIFICATE_BYTES {
        return Err("record out of bounds");
    }
    let root = HybridPublicKey::decode(&publish.root).map_err(|_| "root key")?;
    let device_signing = DeviceSigningCertificate::decode(&record.device_signing_certificate)
        .map_err(|_| "device-signing certificate")?;
    let devices = record
        .devices
        .iter()
        .map(|bytes| DeviceCertificate::decode(bytes))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "device certificate")?;
    let revocations = record
        .revocations
        .iter()
        .map(|bytes| DeviceRevocation::decode(bytes))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "revocation")?;
    let statement = AccountStatement::decode(&record.statement).map_err(|_| "statement")?;
    if devices.is_empty() {
        return Err("no device");
    }
    // The root signed the device-signing key, which signed the statement: the
    // uploader holds the identity, and the identity says this account is its own.
    let identity =
        VerifiedIdentity::verify(root.clone(), device_signing.clone()).map_err(|_| "root")?;
    statement
        .verify(&identity)
        .map_err(|_| "statement signature")?;
    if statement.server() != &log_id.0 || statement.account() != account {
        return Err("statement for another account or server");
    }
    let roster =
        RosterCommitment::compute(&device_signing, &devices, &revocations).map_err(|_| "roster")?;
    let binding = AccountBinding::with_statement(root, roster, &statement);
    roster_for_binding(&binding, &device_signing, &devices, &revocations)
        .map_err(|_| "certificates")?;
    Ok((binding, record.encode_to_vec()))
}

fn directory_failure(error: DirectoryError) -> Code {
    match error {
        DirectoryError::NotFound => Code::NotFound,
        DirectoryError::InvalidRequest(_) => Code::Malformed,
        error => {
            tracing::error!(%error, "key-transparency request failed");
            Code::Busy
        }
    }
}

impl Service {
    pub async fn run(mut self, server: Arc<Server>) {
        let mut tick = tokio::time::interval(self.publish_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let pusher = tokio::spawn(self.witnesses.clone().run(self.directory.clone()));
        loop {
            tokio::select! {
                task = self.tasks.recv() => {
                    let Some(task) = task else { break };
                    let reply = self.answer(task.request_id, task.job).await;
                    server.lock().kt_answer(task.session, task.request_id, reply);
                }
                _ = tick.tick() => {
                    self.follow_registry(&server);
                    self.publish_or_refresh().await;
                }
            }
        }
        pusher.abort();
    }

    /// The `reg:` and `name:` values the registry implies, compared with what was
    /// published: changes wait for the next epoch. A released name stays released
    /// until someone registers it again.
    fn follow_registry(&mut self, server: &Server) {
        let users: Vec<(u32, Id, String)> = {
            let state = server.lock();
            if self.registry_generation == Some(state.registry.generation) {
                return;
            }
            self.registry_generation = Some(state.registry.generation);
            state
                .registry
                .users
                .values()
                .map(|user| (user.id, user.account, user.name.to_lowercase()))
                .collect()
        };
        let mut wanted: BTreeMap<Label, DirectoryValue> = BTreeMap::new();
        for (id, account, name) in users {
            let value = NameBinding::Account(AccountId(account)).to_value();
            wanted.insert(Label::Registration(id), value.clone());
            // Names longer than a label holds stay outside the log.
            if let Some(label) = Label::name(name) {
                wanted.insert(label, value);
            }
        }
        let released = NameBinding::Released.to_value();
        let mut changes = BTreeMap::new();
        for (label, value) in &self.labels {
            if !wanted.contains_key(label) && *value != released {
                changes.insert(label.clone(), released.clone());
            }
        }
        for (label, value) in wanted {
            if self.labels.get(&label) != Some(&value) {
                changes.insert(label, value);
            }
        }
        self.pending_labels = changes;
    }

    async fn publish_or_refresh(&mut self) {
        if !self.pending.is_empty() || !self.pending_labels.is_empty() {
            self.publish().await;
        } else if self.last_head.elapsed() >= self.refresh_interval {
            match self.directory.refresh_head(now_ms()).await {
                Ok(_) => self.last_head = Instant::now(),
                Err(error) => tracing::error!(%error, "could not refresh the signed head"),
            }
        }
    }

    /// Publishes waiting bindings as one epoch, then points the directory API at
    /// their records.
    pub async fn publish(&mut self) {
        let batch: Vec<(Id, (DirectoryValue, [u8; 32]))> = self
            .pending
            .iter()
            .take(MAX_UPDATES_PER_EPOCH)
            .map(|(account, entry)| (*account, entry.clone()))
            .collect();
        let names: Vec<(Label, DirectoryValue)> = self
            .pending_labels
            .iter()
            .take(MAX_UPDATES_PER_EPOCH - batch.len())
            .map(|(label, value)| (label.clone(), value.clone()))
            .collect();
        let updates = batch
            .iter()
            .map(|(account, (value, _))| (label(account), value.clone()))
            .chain(names.iter().cloned())
            .collect();
        match self.directory.publish(updates, now_ms()).await {
            Ok(head) => {
                let mut pointers: Vec<(Vec<u8>, Vec<u8>)> = batch
                    .iter()
                    .map(|(account, (_, hash))| (current_key(account), hash.to_vec()))
                    .collect();
                for (label, value) in &names {
                    if let Ok(encoded) = label.encode() {
                        let mut key = vec![PREFIX_LABEL];
                        key.extend(encoded);
                        pointers.push((key, value.encode()));
                    }
                }
                if let Err(error) = self.store.write(pointers).await {
                    tracing::error!(%error, "could not record the published bindings");
                }
                for (label, value) in names {
                    if self.pending_labels.get(&label) == Some(&value) {
                        self.pending_labels.remove(&label);
                    }
                    self.labels.insert(label, value);
                }
                for (account, (_, hash)) in &batch {
                    if self
                        .pending
                        .get(account)
                        .is_some_and(|(_, waiting)| waiting == hash)
                    {
                        self.pending.remove(account);
                    }
                }
                self.last_head = Instant::now();
                tracing::debug!(epoch = head.epoch, bindings = batch.len(), "published");
            }
            Err(error) => tracing::error!(%error, "could not publish an epoch"),
        }
    }

    /// The witnessed checkpoint for a head, for clients of `origin`.
    async fn witnessed(&self, origin: Option<usize>, head: &SignedHead) -> Vec<u8> {
        match origin {
            Some(origin) => self
                .witnesses
                .witnessed(&self.directory, origin, head.epoch)
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    async fn current(&self, account: &Id) -> Result<Option<[u8; 32]>, Code> {
        let pointer = self
            .store
            .get(&current_key(account))
            .await
            .map_err(|_| Code::Busy)?;
        Ok(pointer.and_then(|bytes| bytes.try_into().ok()))
    }

    async fn answer(&mut self, request_id: u64, job: Job) -> Result<Reply, Code> {
        match job {
            Job::Status {
                account,
                origin,
                registration,
            } => {
                let head = self
                    .directory
                    .latest_head()
                    .await
                    .map_err(directory_failure)?;
                Ok(Reply::Status(v1::KtStatusReply {
                    request_id,
                    account: account.map(|a| a.to_vec()).unwrap_or_default(),
                    witnessed: self.witnessed(origin, &head).await,
                    head: head.encode(),
                    user_id: registration.as_ref().map(|r| r.user_id),
                    name: registration.map(|r| r.name).unwrap_or_default(),
                }))
            }
            Job::Publish { account, publish } => {
                let (binding, record) =
                    check_publish(self.log_id, &account, &publish).map_err(|reason| {
                        tracing::info!(reason, "refused a key-transparency binding");
                        Code::Malformed
                    })?;
                let value = binding.to_value();
                let hash = sha256(&value.encode());
                self.store
                    .write(vec![(record_key(&account, &hash), record)])
                    .await
                    .map_err(|_| Code::Busy)?;
                let current = self.current(&account).await? == Some(hash);
                if current {
                    self.pending.remove(&account);
                } else {
                    self.pending.insert(account, (value.clone(), hash));
                }
                Ok(Reply::Published(v1::KtPublished {
                    request_id,
                    value: value.encode(),
                    current,
                }))
            }
            Job::Lookup {
                account,
                origin,
                registration,
            } => {
                let response = self
                    .directory
                    .lookup(&label(&account))
                    .await
                    .map_err(directory_failure)?;
                let witnessed = self.witnessed(origin, &response.head).await;
                // The user's number and name, from the same epoch (this task also
                // publishes, so nothing moves in between); a label still waiting
                // for its epoch is left out.
                let mut registration_lookup = Vec::new();
                let mut name_lookup = Vec::new();
                let mut name = String::new();
                if let Some(registration) = registration {
                    let (number, named) = registration.labels();
                    if let Ok(found) = self.directory.lookup(&number).await
                        && found.head == response.head
                    {
                        registration_lookup = found.encode().map_err(|_| Code::TooLarge)?;
                    }
                    if let Some(named) = named
                        && let Ok(found) = self.directory.lookup(&named).await
                        && found.head == response.head
                    {
                        name_lookup = found.encode().map_err(|_| Code::TooLarge)?;
                        name = registration.name;
                    }
                }
                let record = match self.current(&account).await? {
                    Some(hash) => self
                        .store
                        .get(&record_key(&account, &hash))
                        .await
                        .map_err(|_| Code::Busy)?
                        .and_then(|bytes| v1::KtRecord::decode(bytes.as_slice()).ok()),
                    None => None,
                };
                let reply = v1::KtLookupReply {
                    request_id,
                    account: account.to_vec(),
                    response: response.encode().map_err(|_| Code::TooLarge)?,
                    record,
                    witnessed,
                    registration: registration_lookup,
                    name_lookup,
                    name,
                };
                if reply.encoded_len() > MAX_KT_LOOKUP_REPLY_BYTES {
                    return Err(Code::TooLarge);
                }
                Ok(Reply::Lookup(reply))
            }
            Job::History {
                account,
                origin,
                label,
                kind,
            } => {
                let history = self
                    .directory
                    .history(&label)
                    .await
                    .map_err(directory_failure)?;
                let witnessed = self.witnessed(origin, &history.head).await;
                let response = history.encode().map_err(|_| Code::TooLarge)?;
                if response.len() + witnessed.len() + 64 > MAX_KT_REPLY_BYTES {
                    return Err(Code::TooLarge);
                }
                Ok(Reply::History(v1::KtHistoryReply {
                    request_id,
                    account: account.to_vec(),
                    response,
                    witnessed,
                    label: kind as i32,
                }))
            }
            Job::Audit { from, to } => {
                if from >= to || to - from > MAX_AUDIT_SPAN {
                    return Err(Code::Malformed);
                }
                let response = self
                    .directory
                    .audit(from, to)
                    .await
                    .map_err(directory_failure)?
                    .encode()
                    .map_err(|_| Code::TooLarge)?;
                if response.len() + 64 > MAX_KT_REPLY_BYTES {
                    return Err(Code::TooLarge);
                }
                Ok(Reply::Audit(v1::KtAuditReply {
                    request_id,
                    response,
                }))
            }
            Job::Head { epoch } => {
                let head = self
                    .directory
                    .head_at(epoch)
                    .await
                    .map_err(directory_failure)?;
                Ok(Reply::Head(v1::KtHeadReply {
                    request_id,
                    head: head.encode(),
                }))
            }
        }
    }
}

// ---- handlers ----

type Failure = (Code, u64);

impl State {
    /// A `kt/1` request from an authenticated connection that negotiated it.
    pub(crate) fn kt_frame(&mut self, session: u32, frame: &ControlFrame) {
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
        let (request_id, request) = match frame.kind {
            kind::KT_STATUS => {
                let message = decoded!(v1::KtStatus);
                (message.request_id, Pending::Status(message.origin))
            }
            kind::KT_PUBLISH => {
                let message = decoded!(v1::KtPublish);
                (message.request_id, Pending::Publish(message))
            }
            kind::KT_LOOKUP => {
                let message = decoded!(v1::KtLookup);
                (
                    message.request_id,
                    Pending::Lookup(message.target, message.origin),
                )
            }
            kind::KT_HISTORY => {
                let message = decoded!(v1::KtHistory);
                let label = message.label();
                (
                    message.request_id,
                    Pending::History(message.account, message.origin, label),
                )
            }
            kind::KT_AUDIT => {
                let message = decoded!(v1::KtAudit);
                (
                    message.request_id,
                    Pending::Audit(message.from_epoch, message.to_epoch),
                )
            }
            kind::KT_HEAD => {
                let message = decoded!(v1::KtHead);
                (message.request_id, Pending::Head(message.epoch))
            }
            _ => {
                self.extension_error(session, 0, (Code::Unsupported, 0));
                return;
            }
        };
        self.deferred(session, request_id, |state| {
            let job = state.kt_authorise(session, request)?;
            let Some(kt) = state.kt.as_ref() else {
                return Err((Code::Unsupported, 0));
            };
            if kt.submit(Task {
                session,
                request_id,
                job,
            }) {
                Ok(())
            } else {
                Err((Code::Busy, 1000))
            }
        });
    }

    fn kt_limited(
        &mut self,
        session: u32,
        bucket: impl FnOnce(&mut crate::extension::Extension) -> &mut crate::limits::LeakyBucket,
    ) -> Result<(), Failure> {
        let limited = self
            .users
            .get_mut(&session)
            .and_then(|user| user.extension.as_mut())
            .is_none_or(|extension| bucket(extension).limited(Instant::now()));
        if limited {
            self.counters
                .rate_limited
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Err((Code::RateLimited, 2000));
        }
        Ok(())
    }

    /// The account a lookup names, by registration number or id.
    fn kt_account(&self, target: Option<v1::kt_lookup::Target>) -> Result<Id, Failure> {
        let account = match target {
            Some(v1::kt_lookup::Target::UserId(id)) => self
                .registry
                .users
                .get(&id)
                .map(|registered| registered.account),
            Some(v1::kt_lookup::Target::Account(bytes)) => nuschl::id(&bytes),
            None => None,
        };
        account.ok_or((Code::NotFound, 0))
    }

    /// A configured origin, if the client named one and this server witnesses.
    fn kt_origin(&self, requested: &str) -> Option<usize> {
        if requested.is_empty() || requested.len() > 255 {
            return None;
        }
        self.kt.as_ref()?.origin(requested)
    }

    /// The registration behind an account, for its `reg:` and `name:` labels.
    fn kt_registration(&self, account: &Id) -> Option<Registration> {
        self.registry
            .by_account(account)
            .map(|registered| Registration {
                user_id: registered.id,
                name: registered.name.to_lowercase(),
            })
    }

    fn kt_authorise(&mut self, session: u32, request: Pending) -> Result<Job, Failure> {
        Ok(match request {
            Pending::Status(origin) => {
                self.authorize(session, Action::KtStatus)?;
                let account = self.users.get(&session).and_then(|user| user.account);
                Job::Status {
                    account,
                    origin: self.kt_origin(&origin),
                    registration: account.and_then(|account| self.kt_registration(&account)),
                }
            }
            Pending::Publish(publish) => {
                self.kt_limited(session, |e| &mut e.kt_publishes)?;
                // Only for one's own account, and only for registered users.
                self.authorize(session, Action::KtPublish)?;
                let account = self
                    .users
                    .get(&session)
                    .and_then(|user| user.account)
                    .ok_or((Code::NotFound, 0))?;
                if !publish.record.as_ref().is_some_and(nuschl::valid_kt_record)
                    || publish.root.len() > MAX_KT_CERTIFICATE_BYTES
                {
                    return Err((Code::Malformed, 0));
                }
                Job::Publish { account, publish }
            }
            Pending::Lookup(target, origin) => {
                self.kt_limited(session, |e| &mut e.kt_lookups)?;
                let account = self.kt_account(target)?;
                self.authorize(session, Action::KtLookup { account: &account })?;
                Job::Lookup {
                    account,
                    origin: self.kt_origin(&origin),
                    registration: self.kt_registration(&account),
                }
            }
            Pending::History(account, origin, kind) => {
                self.kt_limited(session, |e| &mut e.kt_histories)?;
                let account = nuschl::id(&account).ok_or((Code::Malformed, 0))?;
                self.authorize(session, Action::KtHistory { account: &account })?;
                let label = match kind {
                    v1::KtLabel::Account => label(&account),
                    // Numbers and names: only one's own, for self-monitoring.
                    _ => {
                        let own = self.users.get(&session).and_then(|user| user.account);
                        let registration = own
                            .filter(|own| *own == account)
                            .and_then(|own| self.kt_registration(&own))
                            .ok_or((Code::NotFound, 0))?;
                        let (number, name) = registration.labels();
                        match kind {
                            v1::KtLabel::Registration => number,
                            _ => name.ok_or((Code::NotFound, 0))?,
                        }
                    }
                };
                Job::History {
                    account,
                    origin: self.kt_origin(&origin),
                    label,
                    kind,
                }
            }
            Pending::Audit(from, to) => {
                self.kt_limited(session, |e| &mut e.kt_histories)?;
                self.authorize(session, Action::KtAudit)?;
                if from >= to || to - from > MAX_AUDIT_SPAN {
                    return Err((Code::Malformed, 0));
                }
                Job::Audit { from, to }
            }
            Pending::Head(epoch) => {
                self.kt_limited(session, |e| &mut e.kt_histories)?;
                self.authorize(session, Action::KtHead)?;
                Job::Head { epoch }
            }
        })
    }

    /// The key-transparency task's answer. A binding history is authorised
    /// again now, as history pages are: whoever stopped being a contact while
    /// it was read does not get it.
    pub fn kt_answer(&mut self, session: u32, request_id: u64, reply: Result<Reply, Code>) {
        if let Ok(Reply::History(history)) = &reply {
            let allowed = nuschl::id(&history.account).is_some_and(|account| {
                self.allowed(session, Action::KtHistory { account: &account })
            });
            if !allowed {
                self.failed(session, request_id, (Code::NotFound, 0));
                return;
            }
        }
        match reply {
            Ok(reply) => {
                match reply {
                    Reply::Status(m) => self.send(session, kind::KT_STATUS_REPLY, &m),
                    Reply::Published(m) => self.send(session, kind::KT_PUBLISHED, &m),
                    Reply::Lookup(m) => self.send(session, kind::KT_LOOKUP_REPLY, &m),
                    Reply::History(m) => self.send(session, kind::KT_HISTORY_REPLY, &m),
                    Reply::Audit(m) => self.send(session, kind::KT_AUDIT_REPLY, &m),
                    Reply::Head(m) => self.send(session, kind::KT_HEAD_REPLY, &m),
                }
                self.end_request(session, request_id);
            }
            Err(code) => {
                let retry = if code == Code::Busy { 1000 } else { 0 };
                self.failed(session, request_id, (code, retry));
            }
        }
    }
}

/// A decoded request before authorisation.
enum Pending {
    Status(String),
    Publish(v1::KtPublish),
    Lookup(Option<v1::kt_lookup::Target>, String),
    History(Vec<u8>, String, v1::KtLabel),
    Audit(u64, u64),
    Head(u64),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_files_hold_raw_bytes_or_hex() {
        assert_eq!(*parse_key(&[7; 32]).unwrap(), [7; 32]);
        let hex = format!("{}\n", "0a".repeat(32));
        assert_eq!(*parse_key(hex.as_bytes()).unwrap(), [10; 32]);
        assert!(parse_key(b"short").is_err());
        assert!(parse_key("zz".repeat(32).as_bytes()).is_err());
    }

    #[test]
    fn generated_keys_are_private_and_stable() {
        let directory = tempfile::tempdir().unwrap();
        let config = KtConfig::default();
        let first = Keys::load(&config, directory.path()).unwrap();
        let again = Keys::load(&config, directory.path()).unwrap();
        assert_eq!(*first.signing, *again.signing);
        assert_eq!(*first.vrf, *again.vrf);
        assert_ne!(*first.signing, *first.vrf);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(directory.path().join("kt-vrf.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
