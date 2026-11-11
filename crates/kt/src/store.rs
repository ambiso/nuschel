// SPDX-License-Identifier: EUPL-1.2
//! The storage a server's directory runs on: an ordered key-value store with atomic
//! batch writes. The server backs it with its database through the storage worker; the
//! layout of keys and records belongs to this crate and its backend, so the storage
//! worker never interprets them.
//!
//! A SQL table is enough:
//!
//! ```sql
//! CREATE TABLE kt_records (key BYTEA PRIMARY KEY, value BYTEA NOT NULL);
//! -- get:         SELECT value FROM kt_records WHERE key = $1
//! -- scan_prefix: SELECT key, value FROM kt_records WHERE key >= $1 AND key < $2 ORDER BY key
//! --              ($2 = the prefix with its last byte below 0xff incremented)
//! -- write:       one transaction of INSERT … ON CONFLICT (key) DO UPDATE SET value = excluded.value
//! ```
//!
//! Records are only inserted or overwritten, never deleted. Keys are at most a few
//! hundred bytes, values at most [`crate::MAX_VALUE_ENCODED`] plus some 200 bytes.

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Mutex;
use thiserror::Error;

/// A storage failure (connection, I/O, worker gone). The directory retries nothing
/// itself; a failed `write` must have written nothing.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{0}")]
pub struct StoreError(pub String);

/// An ordered byte-key store with atomic batches.
#[async_trait]
pub trait KtStore: Send + Sync + 'static {
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError>;

    /// Values for several keys, in order. Implementations should fetch them in one
    /// round trip.
    async fn get_many(&self, keys: &[Vec<u8>]) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            out.push(self.get(key).await?);
        }
        Ok(out)
    }

    /// Every record whose key starts with `prefix`, in key order.
    async fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StoreError>;

    /// Inserts or overwrites all records, atomically: all or none.
    async fn write(&self, records: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), StoreError>;
}

/// An in-memory [`KtStore`] for tests and benchmarks.
#[derive(Default)]
pub struct MemoryStore {
    map: Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of records.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total size of keys and values in bytes.
    pub fn bytes(&self) -> usize {
        self.lock().iter().map(|(k, v)| k.len() + v.len()).sum()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<Vec<u8>, Vec<u8>>> {
        // A panic while holding the lock cannot leave a half-applied batch: batches are
        // applied with a single `extend` after validation.
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl KtStore for MemoryStore {
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.lock().get(key).cloned())
    }

    async fn get_many(&self, keys: &[Vec<u8>]) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        let map = self.lock();
        Ok(keys.iter().map(|k| map.get(k).cloned()).collect())
    }

    async fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StoreError> {
        let map = self.lock();
        Ok(map
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }

    async fn write(&self, records: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), StoreError> {
        self.lock().extend(records);
        Ok(())
    }
}
