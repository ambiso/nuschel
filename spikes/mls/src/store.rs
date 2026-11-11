// SPDX-License-Identifier: EUPL-1.2
//! Encrypted, transactional persistence behind OpenMLS's storage interface.
//!
//! OpenMLS writes its state through `StorageProvider`: about 60 typed methods
//! that all reduce to put/get/delete of serde-serialised values under a
//! `(label, key, version)` key (see `openmls_memory_storage`). The crypto worker can
//! therefore own the store completely. This spike keeps OpenMLS's in-memory map as
//! the working set and, after each operation, commits the difference as one batch
//! of AEAD-encrypted records:
//!
//! - record id = HMAC-SHA-256(index key, storage key): labels, group ids and
//!   public keys are not visible on disk;
//! - record = nonce || ChaCha20-Poly1305(store key, storage key || value), with the
//!   record id as associated data, so records cannot be swapped or altered;
//! - one batch per worker request: a crash leaves the state of the previous request,
//!   never half a commit;
//! - deleted keys are deleted records. Forward secrecy needs the backend to really
//!   forget them (SQLite `secure_delete`, WAL checkpoints) and periodic re-keying
//!   ([`EncryptedStore::rekey`]) so that copies left on flash are unreadable.
//!
//! A production store implements `StorageProvider` directly over the encrypted
//! backend with a write overlay per request (as `openmls_sqlite_storage` does over
//! SQLite), instead of keeping every group in memory.

use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

use openmls_libcrux_crypto::CryptoProvider;
use openmls_memory_storage::MemoryStorage;
use openmls_traits::crypto::OpenMlsCrypto;
use openmls_traits::random::OpenMlsRand;
use openmls_traits::types::{AeadType, HashType};
use zeroize::Zeroizing;

const AEAD: AeadType = AeadType::ChaCha20Poly1305;
const NONCE: usize = 12;

/// The labels OpenMLS 0.9 uses in storage keys (openmls_memory_storage 0.6).
pub const LABELS: &[&str] = &[
    "KeyPackage",
    "Psk",
    "EncryptionKeyPair",
    "SignatureKeyPair",
    "EpochKeyPairs",
    "Tree",
    "GroupContext",
    "InterimTranscriptHash",
    "ConfirmationTag",
    "MlsGroupJoinConfig",
    "OwnLeafNodes",
    "GroupState",
    "QueuedProposal",
    "ProposalQueueRefs",
    "OwnLeafNodeIndex",
    "EpochSecrets",
    "ResumptionPsk",
    "MessageSecrets",
];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CommitStats {
    pub written: usize,
    pub deleted: usize,
    pub bytes: usize,
}

pub struct EncryptedStore {
    crypto: CryptoProvider,
    store_key: Zeroizing<Vec<u8>>,
    index_key: Zeroizing<Vec<u8>>,
    /// Plaintext view as of the last commit (worker memory only).
    committed: HashMap<Vec<u8>, Vec<u8>>,
    /// What is on disk: record id -> nonce || ciphertext.
    pub disk: BTreeMap<Vec<u8>, Vec<u8>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    /// A record failed authentication: tampering, corruption or the wrong key.
    Corrupt,
}

impl EncryptedStore {
    /// In production the keys come from the OS credential store (Secret Service,
    /// Keychain, DPAPI), never from the history database or the server.
    pub fn new() -> Self {
        let crypto = CryptoProvider::new().expect("operating system randomness");
        let store_key = Zeroizing::new(crypto.random_vec(32).expect("randomness"));
        let index_key = Zeroizing::new(crypto.random_vec(32).expect("randomness"));
        Self {
            crypto,
            store_key,
            index_key,
            committed: HashMap::new(),
            disk: BTreeMap::new(),
        }
    }

    fn record_id(&self, storage_key: &[u8]) -> Vec<u8> {
        self.crypto
            .hmac(HashType::Sha2_256, &self.index_key, storage_key)
            .expect("hmac")
            .as_slice()
            .to_vec()
    }

    fn seal(&self, id: &[u8], storage_key: &[u8], value: &[u8]) -> Vec<u8> {
        let mut plain = Zeroizing::new(Vec::with_capacity(4 + storage_key.len() + value.len()));
        plain.extend_from_slice(&(storage_key.len() as u32).to_be_bytes());
        plain.extend_from_slice(storage_key);
        plain.extend_from_slice(value);
        let nonce = self.crypto.random_vec(NONCE).expect("randomness");
        let sealed = self
            .crypto
            .aead_encrypt(AEAD, &self.store_key, &plain, &nonce, id)
            .expect("aead");
        let mut record = nonce;
        record.extend_from_slice(&sealed);
        record
    }

    fn open(&self, id: &[u8], record: &[u8]) -> Result<(Vec<u8>, Vec<u8>), StoreError> {
        if record.len() < NONCE {
            return Err(StoreError::Corrupt);
        }
        let (nonce, sealed) = record.split_at(NONCE);
        let plain = Zeroizing::new(
            self.crypto
                .aead_decrypt(AEAD, &self.store_key, sealed, nonce, id)
                .map_err(|_| StoreError::Corrupt)?,
        );
        let len = u32::from_be_bytes(
            plain
                .get(..4)
                .ok_or(StoreError::Corrupt)?
                .try_into()
                .unwrap(),
        ) as usize;
        let key = plain.get(4..4 + len).ok_or(StoreError::Corrupt)?.to_vec();
        if self.record_id(&key) != id {
            return Err(StoreError::Corrupt);
        }
        Ok((key, plain[4 + len..].to_vec()))
    }

    /// Commits everything OpenMLS changed since the last commit as one batch.
    pub fn commit(&mut self, storage: &MemoryStorage) -> CommitStats {
        let current = storage.values.read().expect("storage lock").clone();
        let mut stats = CommitStats::default();
        let mut batch: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        for (key, value) in &current {
            if self.committed.get(key) != Some(value) {
                let id = self.record_id(key);
                let record = self.seal(&id, key, value);
                stats.written += 1;
                stats.bytes += record.len();
                batch.push((id, Some(record)));
            }
        }
        for key in self.committed.keys() {
            if !current.contains_key(key) {
                stats.deleted += 1;
                batch.push((self.record_id(key), None));
            }
        }
        // One atomic batch (an SQLite transaction or a renamed file in production).
        for (id, record) in batch {
            match record {
                Some(record) => self.disk.insert(id, record),
                None => self.disk.remove(&id),
            };
        }
        self.committed = current;
        stats
    }

    /// Rebuilds OpenMLS storage from disk, as after a restart of the crypto worker.
    pub fn load(&self) -> Result<MemoryStorage, StoreError> {
        let mut values = HashMap::new();
        for (id, record) in &self.disk {
            let (key, value) = self.open(id, record)?;
            values.insert(key, value);
        }
        Ok(MemoryStorage {
            values: RwLock::new(values),
        })
    }

    /// Re-encrypts every live record under a fresh key and forgets the old one, so
    /// deleted records that survive on flash or in old database pages become
    /// unreadable (crypto-erasure).
    pub fn rekey(&mut self) {
        let live: Vec<(Vec<u8>, Vec<u8>)> = self
            .disk
            .iter()
            .map(|(id, record)| self.open(id, record).expect("own records"))
            .collect();
        self.store_key = Zeroizing::new(self.crypto.random_vec(32).expect("randomness"));
        self.disk.clear();
        for (key, value) in live {
            let id = self.record_id(&key);
            let record = self.seal(&id, &key, &value);
            self.disk.insert(id, record);
        }
    }
}

impl Default for EncryptedStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Counts OpenMLS's records by label: what the library persists.
pub fn census(storage: &MemoryStorage) -> BTreeMap<&'static str, (usize, usize)> {
    let values = storage.values.read().expect("storage lock");
    let mut out = BTreeMap::new();
    for (key, value) in values.iter() {
        // Longest matching label first ("EncryptionKeyPair" vs "EpochKeyPairs" are
        // distinct; "Psk" is a prefix of nothing else used here).
        let label = LABELS
            .iter()
            .filter(|l| key.starts_with(l.as_bytes()))
            .max_by_key(|l| l.len())
            .copied()
            .unwrap_or("?");
        let entry = out.entry(label).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += value.len();
    }
    out
}
