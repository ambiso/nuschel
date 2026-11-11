// SPDX-License-Identifier: EUPL-1.2
//! Suite 1: Meta's auditable key directory (`akd` 0.13, as used by WhatsApp).
//!
//! Configuration: akd's current configuration (`ExperimentalConfiguration`, the one
//! akd's main branch deploys; WhatsApp's v1 configuration exists only for WhatsApp's
//! deployed tree) with the domain label `nuschl-kt/v1 akd`, so no hash of this log can
//! be confused with another application's. Hash: BLAKE3 (portable Rust build). VRF:
//! ECVRF-EDWARDS25519-SHA512-TAI (RFC 9381), akd's only VRF. Value commitments are
//! keyed by a hash of the VRF secret key.

pub(crate) mod codec;
pub(crate) mod db;

use crate::codec::DecodeError;
use crate::error::{DirectoryError, KtError, ProofKind};
use crate::head::{MAX_EPOCH, SignedHead, VerifiedHead};
use crate::log::{HeadSigner, LogDescriptor, LogId, Suite};
use crate::messages::{AuditResponse, HistoryResponse, LookupResponse, MAX_AUDIT_SPAN};
use crate::server::{Directory, MAX_UPDATES_PER_EPOCH};
use crate::store::KtStore;
use crate::types::{DirectoryValue, Label};
use crate::verify::VerifiedEntry;
use crate::witness::auditor::AddEpochs;
use crate::witness::note::{LogNoteKey, Note, NoteSignature};
use crate::witness::tlog::{self, Frontier, Hash};
use crate::witness::{Checkpoint, EpochLeaf};
use akd::append_only_zks::AzksParallelismConfig;
use akd::ecvrf::{VRFKeyStorage, VRFPrivateKey, VRFPublicKey, VrfError};
use akd::errors::{AkdError, StorageError};
use akd::storage::StorageManager;
use akd::{AkdLabel, AkdValue, AppendOnlyProof, HistoryParams, HistoryVerificationParams};
use async_trait::async_trait;
use db::KvDatabase;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use zeroize::Zeroizing;

/// The domain separator of every hash in a Nuschl akd log.
#[derive(Clone)]
pub struct NuschlDomain;

impl akd::DomainLabel for NuschlDomain {
    fn domain_label() -> &'static [u8] {
        b"nuschl-kt/v1 akd"
    }
}

/// The akd configuration of suite 1.
pub type AkdConfig = akd::ExperimentalConfiguration<NuschlDomain>;

pub(crate) fn check_suite_params(params: &[u8]) -> Result<(), DecodeError> {
    // Decompresses the point and rejects small-order keys.
    VRFPublicKey::try_from(params)
        .map(|_| ())
        .map_err(|_| DecodeError::InvalidValue {
            field: "suite_params",
        })
}

/// The VRF secret key of a log (32 bytes, an Ed25519 seed). It hides labels and keys
/// the value commitments; it can never change for the life of the log. Generate it
/// with a CSPRNG and store it with the server's other secrets.
#[derive(Clone)]
pub struct VrfSecret(Arc<Zeroizing<[u8; 32]>>);

impl VrfSecret {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Arc::new(Zeroizing::new(bytes)))
    }

    pub fn public_key(&self) -> [u8; 32] {
        let private = VRFPrivateKey::try_from(&self.0[..]).expect("32 bytes");
        VRFPublicKey::from(&private).to_bytes()
    }
}

#[async_trait]
impl VRFKeyStorage for VrfSecret {
    async fn retrieve(&self) -> Result<Vec<u8>, VrfError> {
        // akd copies the key into a plain Vec for each use; it is not zeroised there.
        Ok(self.0.to_vec())
    }
}

/// A descriptor for an akd log with these keys.
pub fn descriptor(signer: &dyn HeadSigner, vrf: &VrfSecret) -> LogDescriptor {
    LogDescriptor::new(
        Suite::AkdBlake3Ecvrf25519V1,
        signer.public_key(),
        vrf.public_key().to_vec(),
    )
    .expect("a VRF public key derived from a secret is valid")
}

fn akd_err(e: AkdError) -> DirectoryError {
    match e {
        AkdError::Storage(StorageError::NotFound(_)) => DirectoryError::NotFound,
        AkdError::Storage(StorageError::Connection(msg)) => {
            DirectoryError::Store(crate::store::StoreError(msg))
        }
        other => DirectoryError::Backend(other.to_string()),
    }
}

/// The server side of suite 1: an akd directory on a [`KtStore`], with a signed head
/// for every epoch.
pub struct AkdDirectory<S: KtStore> {
    akd: akd::Directory<AkdConfig, KvDatabase<S>, VrfSecret>,
    store: Arc<S>,
    signer: Box<dyn HeadSigner>,
    descriptor: LogDescriptor,
    log_id: LogId,
    /// The newest head and the epoch log. Writers (publish, refresh) hold it
    /// exclusively, so a proof and the head attached to it always refer to the same
    /// epoch.
    latest: RwLock<Latest>,
}

struct Latest {
    head: SignedHead,
    /// The epoch log (docs/e2ee.md §5.9): leaf `i` is epoch `i`, so its size is the
    /// newest epoch + 1.
    frontier: Frontier,
}

/// How far in the future a head may be signed before clients refuse it
/// ([`crate::FreshnessPolicy`]'s default).
const FUTURE_TOLERANCE_MS: u64 = 3600 * 1000;
/// Epochs whose leaves are written in one batch when the epoch log is built for an
/// existing directory.
const BACKFILL_CHUNK: u64 = 1024;

impl<S: KtStore> AkdDirectory<S> {
    /// Opens the log in `store`, creating it if the store is empty. Fails with
    /// [`DirectoryError::KeyMismatch`] if the store holds a log with other keys. Signs
    /// the current epoch if a crash left it without a head.
    pub async fn open(
        store: S,
        vrf: VrfSecret,
        signer: Box<dyn HeadSigner>,
        now_ms: u64,
    ) -> Result<Self, DirectoryError> {
        let store = Arc::new(store);
        let descriptor = descriptor(signer.as_ref(), &vrf);
        let log_id = descriptor.log_id();
        match store.get(db::KEY_DESCRIPTOR).await? {
            Some(stored) if stored != descriptor.encode() => {
                return Err(DirectoryError::KeyMismatch);
            }
            Some(_) => {}
            None => {
                store
                    .write(vec![(db::KEY_DESCRIPTOR.to_vec(), descriptor.encode())])
                    .await?;
            }
        }
        let manager = StorageManager::new_no_cache(KvDatabase::new(store.clone()));
        let akd =
            akd::Directory::<AkdConfig, _, _>::new(manager, vrf, AzksParallelismConfig::default())
                .await
                .map_err(akd_err)?;
        let current = akd.get_epoch_hash().await.map_err(akd_err)?;
        let mut frontier = load_frontier(&*store).await?;
        backfill(&*store, log_id, &mut frontier, current.epoch()).await?;
        let head = match load_head(&*store, current.epoch()).await? {
            Some(head) if head.root == current.hash() && head.log_id == log_id => {
                backfill(&*store, log_id, &mut frontier, current.epoch() + 1).await?;
                head
            }
            Some(_) => {
                return Err(DirectoryError::Corrupt(
                    "the stored head does not match the tree".into(),
                ));
            }
            None => {
                // The previous head (if any) bounds the signing time from below.
                let floor = match current.epoch().checked_sub(1) {
                    Some(prev) => load_head(&*store, prev)
                        .await?
                        .map_or(0, |h| h.issued_at_ms),
                    None => 0,
                };
                let head = SignedHead::sign(
                    signer.as_ref(),
                    log_id,
                    current.epoch(),
                    current.hash(),
                    now_ms.max(floor),
                )?;
                let mut next = frontier.clone();
                store
                    .write(epoch_records(&akd, &head, &mut next).await?)
                    .await?;
                frontier = next;
                head
            }
        };
        if frontier.size() != current.epoch() + 1 {
            return Err(DirectoryError::Corrupt(
                "the epoch log does not end at the newest epoch".into(),
            ));
        }
        let directory = Self {
            akd,
            store,
            signer,
            descriptor,
            log_id,
            latest: RwLock::new(Latest { head, frontier }),
        };
        {
            let mut latest = directory.latest.write().await;
            directory.rewind_future_heads(&mut latest, now_ms).await?;
        }
        Ok(directory)
    }

    /// Repairs the aftermath of a clock that ran ahead and was set back (the
    /// key-transparency review's finding on the clamp): heads never go back in time,
    /// so after a forward jump every later head would be signed in the future, and
    /// clients refuse heads more than an hour ahead until real time catches up.
    ///
    /// Heads signed more than that tolerance ahead of `now` cannot have been accepted
    /// by any client with a correct clock, so they are signed again, at the newest
    /// time that is not in the future (or `now`): epochs keep their order in time,
    /// and no client that saw only acceptable heads can hold stale-signing evidence
    /// against the result. Only a client whose clock was just as far ahead could.
    async fn rewind_future_heads(
        &self,
        latest: &mut Latest,
        now_ms: u64,
    ) -> Result<(), DirectoryError> {
        let limit = now_ms.saturating_add(FUTURE_TOLERANCE_MS);
        if latest.head.issued_at_ms <= limit {
            return Ok(());
        }
        let mut future = vec![latest.head.clone()];
        let mut floor = 0;
        let mut epoch = latest.head.epoch;
        while let Some(previous) = epoch.checked_sub(1) {
            let head = load_head(&*self.store, previous)
                .await?
                .ok_or_else(|| DirectoryError::Corrupt(format!("no head for epoch {previous}")))?;
            if head.issued_at_ms <= limit {
                floor = head.issued_at_ms;
                break;
            }
            future.push(head);
            epoch = previous;
        }
        let at = now_ms.max(floor);
        let mut records = Vec::with_capacity(future.len());
        let mut newest = None;
        for head in future {
            let signed =
                SignedHead::sign(self.signer.as_ref(), self.log_id, head.epoch, head.root, at)?;
            records.push((db::head_key(signed.epoch), signed.encode()));
            newest.get_or_insert(signed);
        }
        self.store.write(records).await?;
        if let Some(head) = newest {
            latest.head = head;
        }
        Ok(())
    }

    /// If an earlier publish committed an epoch but failed to store its head and audit
    /// proof, signs and stores them now, while that epoch is still the newest (so its
    /// proof is still minimal). Called with the write lock held.
    async fn repair(&self, latest: &mut Latest, now_ms: u64) -> Result<(), DirectoryError> {
        self.rewind_future_heads(latest, now_ms).await?;
        let current = self.akd.get_epoch_hash().await.map_err(akd_err)?;
        if current.epoch() == latest.head.epoch {
            return Ok(());
        }
        let head = SignedHead::sign(
            self.signer.as_ref(),
            self.log_id,
            current.epoch(),
            current.hash(),
            now_ms.max(latest.head.issued_at_ms),
        )?;
        let mut frontier = latest.frontier.clone();
        self.store
            .write(epoch_records(&self.akd, &head, &mut frontier).await?)
            .await?;
        latest.head = head;
        latest.frontier = frontier;
        Ok(())
    }

    /// The epoch log's root at `size` (at most the current size).
    async fn root_at(&self, latest: &Latest, size: u64) -> Result<Hash, DirectoryError> {
        if size == latest.frontier.size() {
            return Ok(latest.frontier.root());
        }
        let positions = Frontier::positions(size);
        let keys: Vec<_> = positions
            .iter()
            .map(|(l, i)| db::tree_key(*l, *i))
            .collect();
        let hashes = self
            .store
            .get_many(&keys)
            .await?
            .into_iter()
            .map(|value| value.and_then(|v| <Hash>::try_from(v).ok()))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| DirectoryError::Corrupt("a node of the epoch log is missing".into()))?;
        Frontier::from_parts(size, hashes)
            .map(|f| f.root())
            .ok_or_else(|| DirectoryError::Corrupt("the epoch log's frontier".into()))
    }

    fn sign_checkpoint(&self, checkpoint: &Checkpoint) -> Result<String, DirectoryError> {
        let log = LogNoteKey {
            name: checkpoint.origin.clone(),
            key: self.descriptor.signing_key().clone(),
        };
        let text = checkpoint.body();
        let signature = self.signer.sign(text.as_bytes())?;
        Ok(Note {
            text,
            signatures: vec![NoteSignature {
                name: log.name.clone(),
                key_id: log.key_id(),
                signature,
            }],
        }
        .format())
    }

    async fn head_for(&self, epoch: u64, root: [u8; 32]) -> Result<SignedHead, DirectoryError> {
        let head = load_head(&*self.store, epoch)
            .await?
            .ok_or_else(|| DirectoryError::Corrupt(format!("no head for epoch {epoch}")))?;
        if head.root != root {
            return Err(DirectoryError::Corrupt(format!(
                "the head of epoch {epoch} names another root"
            )));
        }
        Ok(head)
    }
}

async fn load_head<S: KtStore>(
    store: &S,
    epoch: u64,
) -> Result<Option<SignedHead>, DirectoryError> {
    match store.get(&db::head_key(epoch)).await? {
        Some(bytes) => SignedHead::decode(&bytes)
            .map(Some)
            .map_err(|e| DirectoryError::Corrupt(format!("stored head: {e}"))),
        None => Ok(None),
    }
}

type Records = Vec<(Vec<u8>, Vec<u8>)>;

/// Appends an epoch's leaf to the epoch log; returns the records of the completed
/// subtrees and the new size.
fn append_leaf(frontier: &mut Frontier, head: &SignedHead) -> Result<Records, DirectoryError> {
    if frontier.size() != head.epoch {
        return Err(DirectoryError::Corrupt(format!(
            "the epoch log has {} leaves before epoch {}",
            frontier.size(),
            head.epoch
        )));
    }
    let leaf = EpochLeaf {
        log_id: head.log_id,
        epoch: head.epoch,
        root: head.root,
    };
    let nodes = frontier
        .append(leaf.hash())
        .ok_or_else(|| DirectoryError::InvalidRequest("the epoch log is full".into()))?;
    let mut records: Vec<_> = nodes
        .into_iter()
        .map(|(level, index, hash)| (db::tree_key(level, index), hash.to_vec()))
        .collect();
    records.push((
        db::KEY_TREE_SIZE.to_vec(),
        frontier.size().to_be_bytes().to_vec(),
    ));
    Ok(records)
}

async fn load_frontier<S: KtStore>(store: &S) -> Result<Frontier, DirectoryError> {
    let size = match store.get(db::KEY_TREE_SIZE).await? {
        Some(bytes) => u64::from_be_bytes(
            bytes
                .try_into()
                .map_err(|_| DirectoryError::Corrupt("the epoch log's size".into()))?,
        ),
        None => 0,
    };
    let keys: Vec<_> = Frontier::positions(size)
        .iter()
        .map(|(l, i)| db::tree_key(*l, *i))
        .collect();
    let hashes = store
        .get_many(&keys)
        .await?
        .into_iter()
        .map(|value| value.and_then(|v| <Hash>::try_from(v).ok()))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| DirectoryError::Corrupt("a node of the epoch log is missing".into()))?;
    Frontier::from_parts(size, hashes)
        .ok_or_else(|| DirectoryError::Corrupt("the epoch log's frontier".into()))
}

/// Builds the epoch log up to `size` leaves from the stored heads (a directory made
/// before the epoch log existed, or a crash between an epoch and its leaf).
async fn backfill<S: KtStore>(
    store: &S,
    log_id: LogId,
    frontier: &mut Frontier,
    size: u64,
) -> Result<(), DirectoryError> {
    while frontier.size() < size {
        let end = size.min(frontier.size() + BACKFILL_CHUNK);
        let keys: Vec<_> = (frontier.size()..end).map(db::head_key).collect();
        let mut records = Vec::new();
        for (bytes, epoch) in store
            .get_many(&keys)
            .await?
            .into_iter()
            .zip(frontier.size()..end)
        {
            let head = bytes
                .ok_or_else(|| DirectoryError::Corrupt(format!("no head for epoch {epoch}")))
                .and_then(|bytes| {
                    SignedHead::decode(&bytes)
                        .map_err(|e| DirectoryError::Corrupt(format!("stored head: {e}")))
                })?;
            if head.log_id != log_id || head.epoch != epoch {
                return Err(DirectoryError::Corrupt(format!(
                    "the stored head of epoch {epoch}"
                )));
            }
            records.extend(append_leaf(frontier, &head)?);
        }
        store.write(records).await?;
    }
    Ok(())
}

/// The records of a newly published epoch: its head and, from epoch 1 on, the
/// append-only proof from the previous epoch.
///
/// akd builds every audit proof against its *latest* tree, so a proof for an older
/// epoch also walks every path changed since: a range audit costs the span times the
/// later changes (238 KB for four quiet epochs at 10,000 accounts). Made while `epoch`
/// is the latest, the proof is minimal, and it stays valid because it only connects
/// two fixed roots. Range audits concatenate these.
async fn epoch_records<S: KtStore>(
    akd: &akd::Directory<AkdConfig, KvDatabase<S>, VrfSecret>,
    head: &SignedHead,
    frontier: &mut Frontier,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, DirectoryError> {
    let mut records = vec![(db::head_key(head.epoch), head.encode())];
    records.extend(append_leaf(frontier, head)?);
    if head.epoch > 0 {
        let proof = akd
            .audit(head.epoch - 1, head.epoch)
            .await
            .map_err(akd_err)?;
        records.push((
            db::audit_key(head.epoch),
            codec::encode_append_only(&proof)?,
        ));
    }
    Ok(records)
}

/// The stored single-epoch proofs for `from..to`, joined; `None` if one is missing.
async fn stored_audit<S: KtStore>(
    store: &S,
    from: u64,
    to: u64,
) -> Result<Option<AppendOnlyProof>, DirectoryError> {
    let keys: Vec<_> = (from + 1..=to).map(db::audit_key).collect();
    let mut joined = AppendOnlyProof {
        proofs: Vec::with_capacity(keys.len()),
        epochs: Vec::with_capacity(keys.len()),
    };
    for (bytes, epoch) in store.get_many(&keys).await?.into_iter().zip(from..to) {
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let single = codec::decode_append_only(&bytes)
            .map_err(|e| DirectoryError::Corrupt(format!("stored audit proof: {e}")))?;
        if single.epochs != [epoch] {
            return Err(DirectoryError::Corrupt(format!(
                "the stored audit proof of epoch {} covers other epochs",
                epoch + 1
            )));
        }
        joined.proofs.extend(single.proofs);
        joined.epochs.push(epoch);
    }
    Ok(Some(joined))
}

async fn store_head<S: KtStore>(store: &S, head: &SignedHead) -> Result<(), DirectoryError> {
    store
        .write(vec![(db::head_key(head.epoch), head.encode())])
        .await?;
    Ok(())
}

#[async_trait]
impl<S: KtStore> Directory for AkdDirectory<S> {
    fn descriptor(&self) -> &LogDescriptor {
        &self.descriptor
    }

    async fn publish(
        &self,
        updates: Vec<(Label, DirectoryValue)>,
        now_ms: u64,
    ) -> Result<SignedHead, DirectoryError> {
        if updates.len() > MAX_UPDATES_PER_EPOCH {
            return Err(DirectoryError::InvalidRequest(format!(
                "more than {MAX_UPDATES_PER_EPOCH} updates"
            )));
        }
        let mut seen = HashSet::with_capacity(updates.len());
        let mut entries = Vec::with_capacity(updates.len());
        for (label, value) in updates {
            let encoded = label.encode()?;
            if !seen.insert(encoded.clone()) {
                return Err(DirectoryError::InvalidRequest(format!(
                    "{label:?} appears twice"
                )));
            }
            entries.push((AkdLabel(encoded), AkdValue(value.encode())));
        }
        let mut latest = self.latest.write().await;
        self.repair(&mut latest, now_ms).await?;
        if latest.head.epoch >= MAX_EPOCH {
            return Err(DirectoryError::InvalidRequest("the log is full".into()));
        }
        let published = self.akd.publish(entries).await.map_err(akd_err)?;
        if published.epoch() == latest.head.epoch {
            return Ok(latest.head.clone());
        }
        let head = SignedHead::sign(
            self.signer.as_ref(),
            self.log_id,
            published.epoch(),
            published.hash(),
            now_ms.max(latest.head.issued_at_ms),
        )?;
        // Head, audit proof and epoch-log leaf are written together. If this fails,
        // the epoch exists without them; `open` and `repair` complete it, and until
        // then proofs fail with `Corrupt`.
        let mut frontier = latest.frontier.clone();
        self.store
            .write(epoch_records(&self.akd, &head, &mut frontier).await?)
            .await?;
        latest.head = head.clone();
        latest.frontier = frontier;
        Ok(head)
    }

    async fn refresh_head(&self, now_ms: u64) -> Result<SignedHead, DirectoryError> {
        let mut latest = self.latest.write().await;
        self.repair(&mut latest, now_ms).await?;
        if now_ms <= latest.head.issued_at_ms {
            return Ok(latest.head.clone());
        }
        let head = SignedHead::sign(
            self.signer.as_ref(),
            self.log_id,
            latest.head.epoch,
            latest.head.root,
            now_ms,
        )?;
        store_head(&*self.store, &head).await?;
        latest.head = head.clone();
        Ok(head)
    }

    async fn latest_head(&self) -> Result<SignedHead, DirectoryError> {
        Ok(self.latest.read().await.head.clone())
    }

    async fn log_size(&self) -> Result<u64, DirectoryError> {
        Ok(self.latest.read().await.frontier.size())
    }

    async fn checkpoint(&self, origin: &str) -> Result<(Checkpoint, String), DirectoryError> {
        let latest = self.latest.read().await;
        let checkpoint = Checkpoint {
            origin: origin.to_owned(),
            size: latest.frontier.size(),
            root: latest.frontier.root(),
        };
        if !crate::witness::note::valid_name(origin) {
            return Err(DirectoryError::InvalidRequest(format!("origin {origin:?}")));
        }
        let note = self.sign_checkpoint(&checkpoint)?;
        Ok((checkpoint, note))
    }

    async fn inclusion(&self, epoch: u64, size: u64) -> Result<Vec<Hash>, DirectoryError> {
        let latest = self.latest.read().await;
        if epoch >= size || size > latest.frontier.size() {
            return Err(DirectoryError::InvalidRequest(format!(
                "leaf {epoch} of {size} (the log has {})",
                latest.frontier.size()
            )));
        }
        let wanted = tlog::inclusion_nodes(epoch, size);
        let keys: Vec<_> = wanted.iter().map(|(l, i)| db::tree_key(*l, *i)).collect();
        let mut nodes = std::collections::HashMap::with_capacity(wanted.len());
        for (position, value) in wanted.into_iter().zip(self.store.get_many(&keys).await?) {
            let hash = value
                .and_then(|v| <Hash>::try_from(v).ok())
                .ok_or_else(|| {
                    DirectoryError::Corrupt("a node of the epoch log is missing".into())
                })?;
            nodes.insert(position, hash);
        }
        tlog::inclusion_proof(epoch, size, &mut |level, index| {
            nodes.get(&(level, index)).copied().unwrap_or([0; 32])
        })
        .ok_or_else(|| DirectoryError::InvalidRequest("leaf out of range".into()))
    }

    async fn add_epochs(
        &self,
        origin: &str,
        old_size: u64,
        max_epochs: u64,
    ) -> Result<AddEpochs, DirectoryError> {
        if !crate::witness::note::valid_name(origin) {
            return Err(DirectoryError::InvalidRequest(format!("origin {origin:?}")));
        }
        let latest = self.latest.read().await;
        let size = latest.frontier.size();
        if old_size > size {
            return Err(DirectoryError::InvalidRequest(format!(
                "the auditor holds {old_size} epochs, more than the log's {size}"
            )));
        }
        // The auditor builds leaf 0 itself; afterwards it needs a proof from its
        // newest epoch.
        let from = old_size.max(1) - 1;
        let to = (size - 1).min(from + max_epochs.clamp(1, MAX_AUDIT_SPAN));
        let audit = if to > from {
            drop(latest);
            let audit = self.audit(from, to).await?;
            let latest = self.latest.read().await;
            if latest.frontier.size() < to + 1 {
                return Err(DirectoryError::Corrupt("the epoch log shrank".into()));
            }
            Some(audit)
        } else {
            drop(latest);
            None
        };
        let new_size = to + 1;
        let latest = self.latest.read().await;
        let checkpoint = Checkpoint {
            origin: origin.to_owned(),
            size: new_size,
            root: self.root_at(&latest, new_size).await?,
        };
        Ok(AddEpochs {
            origin: origin.to_owned(),
            descriptor: self.descriptor.encode(),
            old_size,
            audit,
            checkpoint: self.sign_checkpoint(&checkpoint)?.into_bytes(),
        })
    }

    async fn head_at(&self, epoch: u64) -> Result<SignedHead, DirectoryError> {
        let _guard = self.latest.read().await;
        load_head(&*self.store, epoch)
            .await?
            .ok_or(DirectoryError::NotFound)
    }

    async fn lookup(&self, label: &Label) -> Result<LookupResponse, DirectoryError> {
        let _guard = self.latest.read().await;
        let (proof, at) = self
            .akd
            .lookup(AkdLabel(label.encode()?))
            .await
            .map_err(akd_err)?;
        Ok(LookupResponse {
            head: self.head_for(at.epoch(), at.hash()).await?,
            label: label.clone(),
            proof: codec::encode_lookup(&proof)?,
        })
    }

    async fn history(&self, label: &Label) -> Result<HistoryResponse, DirectoryError> {
        let _guard = self.latest.read().await;
        let (proof, at) = self
            .akd
            .key_history(&AkdLabel(label.encode()?), HistoryParams::Complete)
            .await
            .map_err(akd_err)?;
        Ok(HistoryResponse {
            head: self.head_for(at.epoch(), at.hash()).await?,
            label: label.clone(),
            proof: codec::encode_history(&proof)?,
        })
    }

    async fn audit(&self, from_epoch: u64, to_epoch: u64) -> Result<AuditResponse, DirectoryError> {
        let latest = self.latest.read().await;
        if from_epoch >= to_epoch
            || to_epoch > latest.head.epoch
            || to_epoch - from_epoch > MAX_AUDIT_SPAN
        {
            return Err(DirectoryError::InvalidRequest(format!(
                "audit range {from_epoch}..{to_epoch} (latest {}, at most {MAX_AUDIT_SPAN} epochs)",
                latest.head.epoch
            )));
        }
        let proof = match stored_audit(&*self.store, from_epoch, to_epoch).await? {
            Some(proof) => proof,
            None => self
                .akd
                .audit(from_epoch, to_epoch)
                .await
                .map_err(akd_err)?,
        };
        let mut roots = Vec::with_capacity((to_epoch - from_epoch) as usize);
        for epoch in from_epoch + 1..=to_epoch {
            let head = load_head(&*self.store, epoch)
                .await?
                .ok_or_else(|| DirectoryError::Corrupt(format!("no head for epoch {epoch}")))?;
            roots.push(head.root);
        }
        Ok(AuditResponse {
            from_epoch,
            to_epoch,
            roots,
            proof: codec::encode_append_only(&proof)?,
        })
    }
}

fn vrf_key(descriptor: &LogDescriptor) -> &[u8] {
    descriptor.suite_params()
}

fn akd_label(label: &Label) -> Result<AkdLabel, KtError> {
    label
        .encode()
        .map(AkdLabel)
        .map_err(|_| KtError::Malformed(DecodeError::InvalidValue { field: "label" }))
}

/// Runs akd's verification so that a panic on hostile input (akd asserts in places)
/// becomes a verification failure instead of taking the client down.
fn contain<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err("the verifier panicked on this proof".into()))
}

fn value_of(bytes: &AkdValue) -> Result<DirectoryValue, KtError> {
    DirectoryValue::decode(&bytes.0).map_err(|_| KtError::MalformedValue)
}

pub(crate) fn verify_lookup(
    descriptor: &LogDescriptor,
    head: &VerifiedHead,
    label: &Label,
    proof: &[u8],
) -> Result<VerifiedEntry, KtError> {
    let proof = codec::decode_lookup(proof)?;
    let invalid = |reason: String| KtError::ProofInvalid {
        proof: ProofKind::Lookup,
        reason,
    };
    // akd asserts (panics) on version 0; versions and epochs never exceed the head.
    if proof.version == 0 || proof.version > proof.epoch || proof.epoch > head.epoch() {
        return Err(invalid(format!(
            "version {} at epoch {} under head epoch {}",
            proof.version,
            proof.epoch,
            head.epoch()
        )));
    }
    let akd_label = akd_label(label)?;
    let result = contain(|| {
        akd_core::verify::lookup::lookup_verify::<AkdConfig>(
            vrf_key(descriptor),
            *head.root(),
            head.epoch(),
            akd_label,
            proof,
        )
        .map_err(|e| e.to_string())
    })
    .map_err(invalid)?;
    Ok(VerifiedEntry {
        label: label.clone(),
        value: value_of(&result.value)?,
        version: result.version,
        epoch: result.epoch,
    })
}

pub(crate) fn verify_history(
    descriptor: &LogDescriptor,
    head: &VerifiedHead,
    label: &Label,
    proof: &[u8],
) -> Result<Vec<VerifiedEntry>, KtError> {
    let proof = codec::decode_history(proof)?;
    let invalid = |reason: String| KtError::ProofInvalid {
        proof: ProofKind::History,
        reason,
    };
    // Versions must run n, n-1, …, 1 with strictly decreasing epochs no later than the
    // head. Checked before akd sees the proof, so its arithmetic stays in range.
    let n = proof.update_proofs.len() as u64;
    let mut previous_epoch = head.epoch().saturating_add(1);
    for (i, update) in proof.update_proofs.iter().enumerate() {
        if update.version != n - i as u64 || update.epoch >= previous_epoch {
            return Err(invalid(format!(
                "update {i}: version {} at epoch {}",
                update.version, update.epoch
            )));
        }
        previous_epoch = update.epoch;
    }
    if n == 0 {
        return Err(invalid("no versions".into()));
    }
    let akd_label = akd_label(label)?;
    let results = contain(|| {
        akd_core::verify::history::key_history_verify::<AkdConfig>(
            vrf_key(descriptor),
            *head.root(),
            head.epoch(),
            akd_label,
            proof,
            HistoryVerificationParams::Default {
                history_params: HistoryParams::Complete,
            },
        )
        .map_err(|e| e.to_string())
    })
    .map_err(invalid)?;
    results
        .into_iter()
        .map(|r| {
            Ok(VerifiedEntry {
                label: label.clone(),
                value: value_of(&r.value)?,
                version: r.version,
                epoch: r.epoch,
            })
        })
        .collect()
}

pub(crate) fn verify_audit(
    from: &VerifiedHead,
    to: &VerifiedHead,
    response: &AuditResponse,
) -> Result<(), KtError> {
    verify_audit_roots(from.root(), response).map_err(|e| match e {
        KtError::AuditFailed { reason, .. } => KtError::AuditFailed {
            from: from.epoch(),
            to: to.epoch(),
            reason,
        },
        other => other,
    })
}

/// Verifies an audit response that starts at `from_root` (the root of
/// `response.from_epoch`): every step from there to `response.roots` is an
/// append-only update. The project auditor checks submissions with this.
pub(crate) fn verify_audit_roots(
    from_root: &[u8; 32],
    response: &AuditResponse,
) -> Result<(), KtError> {
    let fail = |reason: String| KtError::AuditFailed {
        from: response.from_epoch,
        to: response.to_epoch,
        reason,
    };
    if response.from_epoch >= response.to_epoch
        || response.roots.len() as u64 != response.to_epoch - response.from_epoch
    {
        return Err(fail("the response's epochs and roots do not match".into()));
    }
    let proof = codec::decode_append_only(&response.proof)?;
    let expected: Vec<u64> = (response.from_epoch..response.to_epoch).collect();
    if proof.epochs != expected {
        return Err(fail("the proof covers other epochs".into()));
    }
    let mut hashes = Vec::with_capacity(response.roots.len() + 1);
    hashes.push(*from_root);
    hashes.extend_from_slice(&response.roots);
    // akd's auditor is async and spawns Tokio tasks: it gets its own runtime on its
    // own thread, which works whether or not the caller is inside a runtime, and turns
    // a panic on hostile input into a failure.
    let result = std::thread::scope(|s| {
        s.spawn(move || -> Result<(), String> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .map_err(|e| format!("runtime: {e}"))?;
            runtime
                .block_on(akd::auditor::audit_verify::<AkdConfig>(hashes, proof))
                .map_err(|e| e.to_string())
        })
        .join()
        .unwrap_or_else(|_| Err("the audit verifier panicked on this proof".into()))
    });
    result.map_err(fail)
}

/// The root of epoch 0: akd's empty directory under this suite's configuration. It
/// depends on no key (an empty tree has no labels or values); the auditor requires
/// every log to start there.
pub(crate) fn empty_root() -> [u8; 32] {
    static ROOT: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    *ROOT.get_or_init(|| {
        std::thread::scope(|s| {
            s.spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("a current-thread runtime");
                runtime.block_on(async {
                    let directory = AkdDirectory::open(
                        crate::store::MemoryStore::new(),
                        VrfSecret::from_bytes([1; 32]),
                        Box::new(crate::log::Ed25519HeadSigner::from_seed(&[1; 32])),
                        0,
                    )
                    .await
                    .expect("an empty in-memory directory opens");
                    directory.latest.read().await.head.root
                })
            })
            .join()
            .expect("computing akd's empty root")
        })
    })
}
