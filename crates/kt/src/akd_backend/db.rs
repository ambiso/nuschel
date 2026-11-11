// SPDX-License-Identifier: EUPL-1.2
//! akd's storage trait on top of a [`KtStore`].
//!
//! Key layout (all records live in one ordered key space):
//!
//! | Key | Record |
//! | --- | --- |
//! | `a` | akd's aZKS (latest epoch, node count) |
//! | `n` ‖ u32 label_len ‖ label_val\[32\] | a tree node with its previous state |
//! | `v` ‖ u16 len ‖ label ‖ u64 epoch | the value of a label published at an epoch |
//! | `h` ‖ u64 epoch | the newest signed head of an epoch (see `mod.rs`) |
//! | `m` ‖ name | metadata (the log descriptor) |
//!
//! The length prefix in `v` keys makes a prefix scan return exactly one label's
//! versions. Records start with a format byte (1) and use the strict codec.

use crate::codec::{DecodeError, Reader, Writer};
use crate::store::{KtStore, StoreError};
use akd::errors::StorageError;
use akd::storage::types::{
    DbRecord, KeyData, StorageType, ValueState, ValueStateKey, ValueStateRetrievalFlag,
};
use akd::storage::{Database, DbSetState, Storable};
use akd::tree_node::{TreeNode, TreeNodeType, TreeNodeWithPreviousValue};
use akd::{AkdLabel, AkdValue, Azks, AzksValue, NodeLabel};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

const RECORD_FORMAT: u8 = 1;
pub(crate) const KEY_AZKS: &[u8] = b"a";
const PREFIX_NODE: u8 = b'n';
const PREFIX_VALUE: u8 = b'v';
pub(crate) const PREFIX_HEAD: u8 = b'h';
pub(crate) const KEY_DESCRIPTOR: &[u8] = b"mdescriptor";
/// Largest stored record (a value state with the largest value and label).
const MAX_RECORD: usize = 16 * 1024;
const MAX_AKD_LABEL: usize = crate::types::MAX_LABEL_ENCODED;

pub(crate) fn head_key(epoch: u64) -> Vec<u8> {
    let mut k = vec![PREFIX_HEAD];
    k.extend_from_slice(&epoch.to_be_bytes());
    k
}

fn node_key(label: &NodeLabel) -> Vec<u8> {
    let mut k = vec![PREFIX_NODE];
    k.extend_from_slice(&label.label_len.to_be_bytes());
    k.extend_from_slice(&label.label_val);
    k
}

fn value_prefix(username: &[u8]) -> Result<Vec<u8>, StorageError> {
    let len = u16::try_from(username.len())
        .map_err(|_| StorageError::Other("label too long for the value key".into()))?;
    let mut k = vec![PREFIX_VALUE];
    k.extend_from_slice(&len.to_be_bytes());
    k.extend_from_slice(username);
    Ok(k)
}

fn value_key(username: &[u8], epoch: u64) -> Result<Vec<u8>, StorageError> {
    let mut k = value_prefix(username)?;
    k.extend_from_slice(&epoch.to_be_bytes());
    Ok(k)
}

/// The store key for akd's binary record id of type `ty`.
fn key_for(ty: StorageType, binary_id: &[u8]) -> Result<Vec<u8>, StorageError> {
    match ty {
        StorageType::Azks => Ok(KEY_AZKS.to_vec()),
        StorageType::TreeNode => {
            let key = TreeNodeWithPreviousValue::key_from_full_binary(binary_id)
                .map_err(StorageError::Other)?;
            Ok(node_key(&key.0))
        }
        StorageType::ValueState => {
            let ValueStateKey(username, epoch) =
                ValueState::key_from_full_binary(binary_id).map_err(StorageError::Other)?;
            value_key(&username, epoch)
        }
    }
}

fn record_key(record: &DbRecord) -> Result<Vec<u8>, StorageError> {
    match record {
        DbRecord::Azks(_) => Ok(KEY_AZKS.to_vec()),
        DbRecord::TreeNode(node) => Ok(node_key(&node.label)),
        DbRecord::ValueState(state) => value_key(&state.username, state.epoch),
    }
}

fn write_label(w: &mut Writer, label: &NodeLabel) {
    w.u32(label.label_len);
    w.raw(&label.label_val);
}

fn read_label(r: &mut Reader<'_>) -> Result<NodeLabel, DecodeError> {
    let len = r.u32("label_len")?;
    if len > 256 {
        return Err(DecodeError::InvalidValue { field: "label_len" });
    }
    Ok(NodeLabel::new(r.array("label_val")?, len))
}

fn write_opt_label(w: &mut Writer, label: &Option<NodeLabel>) {
    w.present(label.is_some());
    if let Some(l) = label {
        write_label(w, l);
    }
}

fn read_opt_label(r: &mut Reader<'_>) -> Result<Option<NodeLabel>, DecodeError> {
    Ok(if r.present("child")? {
        Some(read_label(r)?)
    } else {
        None
    })
}

fn write_node(w: &mut Writer, n: &TreeNode) {
    write_label(w, &n.label);
    w.u64(n.last_epoch);
    w.u64(n.min_descendant_epoch);
    write_label(w, &n.parent);
    w.u8(n.node_type as u8);
    write_opt_label(w, &n.left_child);
    write_opt_label(w, &n.right_child);
    w.raw(&n.hash.0);
}

fn read_node(r: &mut Reader<'_>) -> Result<TreeNode, DecodeError> {
    Ok(TreeNode {
        label: read_label(r)?,
        last_epoch: r.u64("last_epoch")?,
        min_descendant_epoch: r.u64("min_descendant_epoch")?,
        parent: read_label(r)?,
        node_type: match r.u8("node_type")? {
            1 => TreeNodeType::Leaf,
            2 => TreeNodeType::Root,
            3 => TreeNodeType::Interior,
            _ => return Err(DecodeError::InvalidValue { field: "node_type" }),
        },
        left_child: read_opt_label(r)?,
        right_child: read_opt_label(r)?,
        hash: AzksValue(r.array("hash")?),
    })
}

pub(crate) fn encode_record(record: &DbRecord) -> Result<Vec<u8>, StorageError> {
    let mut w = Writer::new();
    w.u8(RECORD_FORMAT);
    match record {
        DbRecord::Azks(azks) => {
            w.u64(azks.latest_epoch);
            w.u64(azks.num_nodes);
        }
        DbRecord::TreeNode(node) => {
            write_label(&mut w, &node.label);
            write_node(&mut w, &node.latest_node);
            w.present(node.previous_node.is_some());
            if let Some(prev) = &node.previous_node {
                write_node(&mut w, prev);
            }
        }
        DbRecord::ValueState(state) => {
            w.bytes_u32("value", &state.value.0, MAX_RECORD)
                .map_err(|e| StorageError::Other(e.to_string()))?;
            w.u64(state.version);
            write_label(&mut w, &state.label);
            w.u64(state.epoch);
            w.bytes_u16("username", &state.username.0, MAX_AKD_LABEL)
                .map_err(|e| StorageError::Other(e.to_string()))?;
        }
    }
    Ok(w.finish())
}

pub(crate) fn decode_record(ty: StorageType, bytes: &[u8]) -> Result<DbRecord, StorageError> {
    let corrupt = |e: DecodeError| StorageError::Other(format!("corrupt {ty:?} record: {e}"));
    let mut r = Reader::new(bytes, MAX_RECORD).map_err(corrupt)?;
    if r.u8("format").map_err(corrupt)? != RECORD_FORMAT {
        return Err(corrupt(DecodeError::InvalidValue { field: "format" }));
    }
    let record = (|| -> Result<DbRecord, DecodeError> {
        Ok(match ty {
            StorageType::Azks => DbRecord::Azks(Azks {
                latest_epoch: r.u64("latest_epoch")?,
                num_nodes: r.u64("num_nodes")?,
            }),
            StorageType::TreeNode => DbRecord::TreeNode(TreeNodeWithPreviousValue {
                label: read_label(&mut r)?,
                latest_node: read_node(&mut r)?,
                previous_node: if r.present("previous")? {
                    Some(read_node(&mut r)?)
                } else {
                    None
                },
            }),
            StorageType::ValueState => {
                let value = AkdValue(r.bytes_u32("value", MAX_RECORD)?.to_vec());
                let version = r.u64("version")?;
                let label = read_label(&mut r)?;
                let epoch = r.u64("epoch")?;
                let username = AkdLabel(r.bytes_u16("username", MAX_AKD_LABEL)?.to_vec());
                DbRecord::ValueState(ValueState {
                    value,
                    version,
                    label,
                    epoch,
                    username,
                })
            }
        })
    })()
    .map_err(corrupt)?;
    r.finish().map_err(corrupt)?;
    Ok(record)
}

fn store_err(e: StoreError) -> StorageError {
    StorageError::Connection(e.0)
}

/// akd's [`Database`] over a [`KtStore`].
pub(crate) struct KvDatabase<S> {
    store: Arc<S>,
}

impl<S> KvDatabase<S> {
    pub(crate) fn new(store: Arc<S>) -> Self {
        Self { store }
    }
}

impl<S: KtStore> KvDatabase<S> {
    async fn states(&self, username: &AkdLabel) -> Result<Vec<ValueState>, StorageError> {
        let rows = self
            .store
            .scan_prefix(&value_prefix(username)?)
            .await
            .map_err(store_err)?;
        let mut states = Vec::with_capacity(rows.len());
        for (_, bytes) in rows {
            match decode_record(StorageType::ValueState, &bytes)? {
                DbRecord::ValueState(s) => states.push(s),
                _ => unreachable!("decode_record returns the requested type"),
            }
        }
        // Keys end in the big-endian epoch, so the scan is already in epoch order.
        Ok(states)
    }
}

fn select_state(
    states: Vec<ValueState>,
    flag: ValueStateRetrievalFlag,
) -> Option<ValueState> {
    let mut it = states.into_iter();
    match flag {
        ValueStateRetrievalFlag::SpecificVersion(v) => it.find(|s| s.version == v),
        ValueStateRetrievalFlag::SpecificEpoch(e) => it.find(|s| s.epoch == e),
        ValueStateRetrievalFlag::LeqEpoch(e) => it.filter(|s| s.epoch <= e).max_by_key(|s| s.epoch),
        ValueStateRetrievalFlag::MaxEpoch => it.max_by_key(|s| s.epoch),
        ValueStateRetrievalFlag::MinEpoch => it.min_by_key(|s| s.epoch),
    }
}

#[async_trait]
impl<S: KtStore> Database for KvDatabase<S> {
    async fn set(&self, record: DbRecord) -> Result<(), StorageError> {
        self.batch_set(vec![record], DbSetState::General).await
    }

    async fn batch_set(
        &self,
        records: Vec<DbRecord>,
        _state: DbSetState,
    ) -> Result<(), StorageError> {
        let mut rows = Vec::with_capacity(records.len());
        for record in &records {
            rows.push((record_key(record)?, encode_record(record)?));
        }
        self.store.write(rows).await.map_err(store_err)
    }

    async fn get<St: Storable>(&self, id: &St::StorageKey) -> Result<DbRecord, StorageError> {
        let key = key_for(St::data_type(), &St::get_full_binary_key_id(id))?;
        match self.store.get(&key).await.map_err(store_err)? {
            Some(bytes) => decode_record(St::data_type(), &bytes),
            None => Err(StorageError::NotFound(format!("{:?} {id:?}", St::data_type()))),
        }
    }

    async fn batch_get<St: Storable>(
        &self,
        ids: &[St::StorageKey],
    ) -> Result<Vec<DbRecord>, StorageError> {
        let keys = ids
            .iter()
            .map(|id| key_for(St::data_type(), &St::get_full_binary_key_id(id)))
            .collect::<Result<Vec<_>, _>>()?;
        let values = self.store.get_many(&keys).await.map_err(store_err)?;
        values
            .into_iter()
            .flatten()
            .map(|bytes| decode_record(St::data_type(), &bytes))
            .collect()
    }

    async fn get_user_data(&self, username: &AkdLabel) -> Result<KeyData, StorageError> {
        let states = self.states(username).await?;
        if states.is_empty() {
            return Err(StorageError::NotFound("no value states".into()));
        }
        Ok(KeyData { states })
    }

    async fn get_user_state(
        &self,
        username: &AkdLabel,
        flag: ValueStateRetrievalFlag,
    ) -> Result<ValueState, StorageError> {
        select_state(self.states(username).await?, flag)
            .ok_or_else(|| StorageError::NotFound("no matching value state".into()))
    }

    async fn get_user_state_versions(
        &self,
        usernames: &[AkdLabel],
        flag: ValueStateRetrievalFlag,
    ) -> Result<HashMap<AkdLabel, (u64, AkdValue)>, StorageError> {
        let mut out = HashMap::new();
        for username in usernames {
            if let Some(state) = select_state(self.states(username).await?, flag) {
                out.insert(username.clone(), (state.version, state.value));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;

    fn state(user: &[u8], version: u64, epoch: u64) -> ValueState {
        ValueState {
            value: AkdValue(vec![version as u8]),
            version,
            label: NodeLabel::new([version as u8; 32], 256),
            epoch,
            username: AkdLabel(user.to_vec()),
        }
    }

    #[tokio::test]
    async fn value_states_are_selected_per_label_and_flag() {
        let db = KvDatabase::new(Arc::new(MemoryStore::new()));
        db.batch_set(
            vec![
                DbRecord::ValueState(state(b"a", 1, 2)),
                DbRecord::ValueState(state(b"a", 2, 5)),
                DbRecord::ValueState(state(b"a", 3, 9)),
                // A label that extends "a" must not appear in "a"'s scan.
                DbRecord::ValueState(state(b"ab", 1, 3)),
            ],
            DbSetState::General,
        )
        .await
        .unwrap();
        let a = AkdLabel(b"a".to_vec());
        assert_eq!(db.get_user_data(&a).await.unwrap().states.len(), 3);
        let pick = |flag| {
            let db = &db;
            let a = a.clone();
            async move { db.get_user_state(&a, flag).await.map(|s| s.version).ok() }
        };
        assert_eq!(pick(ValueStateRetrievalFlag::LeqEpoch(4)).await, Some(1));
        assert_eq!(pick(ValueStateRetrievalFlag::LeqEpoch(5)).await, Some(2));
        assert_eq!(pick(ValueStateRetrievalFlag::LeqEpoch(100)).await, Some(3));
        assert_eq!(pick(ValueStateRetrievalFlag::LeqEpoch(1)).await, None);
        assert_eq!(pick(ValueStateRetrievalFlag::MaxEpoch).await, Some(3));
        assert_eq!(pick(ValueStateRetrievalFlag::MinEpoch).await, Some(1));
        assert_eq!(pick(ValueStateRetrievalFlag::SpecificEpoch(5)).await, Some(2));
        assert_eq!(pick(ValueStateRetrievalFlag::SpecificVersion(3)).await, Some(3));
        let versions = db
            .get_user_state_versions(
                &[a.clone(), AkdLabel(b"zz".to_vec())],
                ValueStateRetrievalFlag::MaxEpoch,
            )
            .await
            .unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[&a].0, 3);
        let got = db
            .get::<ValueState>(&ValueStateKey(b"a".to_vec(), 5))
            .await
            .unwrap();
        assert_eq!(got, DbRecord::ValueState(state(b"a", 2, 5)));
    }

    #[test]
    fn records_round_trip() {
        let node = TreeNode {
            label: NodeLabel::new([1; 32], 7),
            last_epoch: 3,
            min_descendant_epoch: 1,
            parent: NodeLabel::new([0; 32], 0),
            node_type: TreeNodeType::Interior,
            left_child: Some(NodeLabel::new([2; 32], 8)),
            right_child: None,
            hash: AzksValue([9; 32]),
        };
        let records = [
            DbRecord::Azks(Azks {
                latest_epoch: 4,
                num_nodes: 99,
            }),
            DbRecord::TreeNode(TreeNodeWithPreviousValue {
                label: node.label,
                latest_node: node.clone(),
                previous_node: Some(node),
            }),
            DbRecord::ValueState(state(b"x", 2, 8)),
        ];
        for (record, ty) in records.iter().zip([
            StorageType::Azks,
            StorageType::TreeNode,
            StorageType::ValueState,
        ]) {
            let bytes = encode_record(record).unwrap();
            assert_eq!(&decode_record(ty, &bytes).unwrap(), record);
            assert!(decode_record(ty, &bytes[..bytes.len() - 1]).is_err());
        }
    }
}
