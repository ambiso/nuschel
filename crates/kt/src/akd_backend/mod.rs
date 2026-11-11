// SPDX-License-Identifier: EUPL-1.2
//! Suite 1: Meta's auditable key directory (`akd` 0.13, as used by WhatsApp).
//!
//! Configuration: akd's current configuration (`ExperimentalConfiguration`, the one
//! akd's main branch deploys; WhatsApp's v1 configuration exists only for WhatsApp's
//! deployed tree) with the domain label `nuschel-kt/v1 akd`, so no hash of this log can
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
use akd::append_only_zks::AzksParallelismConfig;
use akd::ecvrf::{VRFKeyStorage, VRFPrivateKey, VRFPublicKey, VrfError};
use akd::errors::{AkdError, StorageError};
use akd::storage::StorageManager;
use akd::{AkdLabel, AkdValue, HistoryParams, HistoryVerificationParams};
use async_trait::async_trait;
use db::KvDatabase;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use zeroize::Zeroizing;

/// The domain separator of every hash in a Nuschel akd log.
#[derive(Clone)]
pub struct NuschelDomain;

impl akd::DomainLabel for NuschelDomain {
    fn domain_label() -> &'static [u8] {
        b"nuschel-kt/v1 akd"
    }
}

/// The akd configuration of suite 1.
pub type AkdConfig = akd::ExperimentalConfiguration<NuschelDomain>;

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
    /// The newest head. Writers (publish, refresh) hold it exclusively, so a proof and
    /// the head attached to it always refer to the same epoch.
    latest: RwLock<SignedHead>,
}

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
        let akd = akd::Directory::<AkdConfig, _, _>::new(
            manager,
            vrf,
            AzksParallelismConfig::default(),
        )
        .await
        .map_err(akd_err)?;
        let current = akd.get_epoch_hash().await.map_err(akd_err)?;
        let head = match load_head(&*store, current.epoch()).await? {
            Some(head) if head.root == current.hash() && head.log_id == log_id => head,
            Some(_) => {
                return Err(DirectoryError::Corrupt(
                    "the stored head does not match the tree".into(),
                ));
            }
            None => {
                // The previous head (if any) bounds the signing time from below.
                let floor = match current.epoch().checked_sub(1) {
                    Some(prev) => load_head(&*store, prev).await?.map_or(0, |h| h.issued_at_ms),
                    None => 0,
                };
                let head = SignedHead::sign(
                    signer.as_ref(),
                    log_id,
                    current.epoch(),
                    current.hash(),
                    now_ms.max(floor),
                )?;
                store_head(&*store, &head).await?;
                head
            }
        };
        Ok(Self {
            akd,
            store,
            signer,
            descriptor,
            log_id,
            latest: RwLock::new(head),
        })
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

async fn load_head<S: KtStore>(store: &S, epoch: u64) -> Result<Option<SignedHead>, DirectoryError> {
    match store.get(&db::head_key(epoch)).await? {
        Some(bytes) => SignedHead::decode(&bytes)
            .map(Some)
            .map_err(|e| DirectoryError::Corrupt(format!("stored head: {e}"))),
        None => Ok(None),
    }
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
        if latest.epoch >= MAX_EPOCH {
            return Err(DirectoryError::InvalidRequest("the log is full".into()));
        }
        let published = self.akd.publish(entries).await.map_err(akd_err)?;
        if published.epoch() == latest.epoch {
            return Ok(latest.clone());
        }
        let head = SignedHead::sign(
            self.signer.as_ref(),
            self.log_id,
            published.epoch(),
            published.hash(),
            now_ms.max(latest.issued_at_ms),
        )?;
        // If this write fails the epoch exists without a head; `open` repairs that,
        // and until then proofs for it fail with `Corrupt`.
        store_head(&*self.store, &head).await?;
        *latest = head.clone();
        Ok(head)
    }

    async fn refresh_head(&self, now_ms: u64) -> Result<SignedHead, DirectoryError> {
        let mut latest = self.latest.write().await;
        if now_ms <= latest.issued_at_ms {
            return Ok(latest.clone());
        }
        let head = SignedHead::sign(
            self.signer.as_ref(),
            self.log_id,
            latest.epoch,
            latest.root,
            now_ms,
        )?;
        store_head(&*self.store, &head).await?;
        *latest = head.clone();
        Ok(head)
    }

    async fn latest_head(&self) -> Result<SignedHead, DirectoryError> {
        Ok(self.latest.read().await.clone())
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
            || to_epoch > latest.epoch
            || to_epoch - from_epoch > MAX_AUDIT_SPAN
        {
            return Err(DirectoryError::InvalidRequest(format!(
                "audit range {from_epoch}..{to_epoch} (latest {}, at most {MAX_AUDIT_SPAN} epochs)",
                latest.epoch
            )));
        }
        let proof = self
            .akd
            .audit(from_epoch, to_epoch)
            .await
            .map_err(akd_err)?;
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
    let fail = |reason: String| KtError::AuditFailed {
        from: from.epoch(),
        to: to.epoch(),
        reason,
    };
    let proof = codec::decode_append_only(&response.proof)?;
    let expected: Vec<u64> = (from.epoch()..to.epoch()).collect();
    if proof.epochs != expected {
        return Err(fail("the proof covers other epochs".into()));
    }
    let mut hashes = Vec::with_capacity(response.roots.len() + 1);
    hashes.push(*from.root());
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
