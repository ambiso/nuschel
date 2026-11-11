// SPDX-License-Identifier: EUPL-1.2
//! A model of the delivery service in `nuscheld` (docs/e2ee.md, "Delivery service").
//!
//! The server is not a member. It tracks each group's public state with OpenMLS's
//! [`PublicGroup`] from handshake messages sent as PublicMessage, so it can
//! - accept exactly one commit per epoch (first come, first served) and reject the
//!   others with the current epoch, so clients never fork;
//! - reject malformed or badly signed handshake messages before fan-out;
//! - reject application messages that are not for the current epoch, so receivers
//!   never need old epoch secrets (forward secrecy with `max_past_epochs = 0`);
//! - serve the ratchet tree and the newest member-signed GroupInfo for external joins.
//!
//! Every accepted message gets the conversation's next `seq` (docs/extension.md):
//! commits and application messages share one ordered stream.

use std::collections::HashMap;

use openmls::messages::group_info::VerifiableGroupInfo;
use openmls::prelude::*;
use openmls::treesync::RatchetTree;
use openmls_libcrux_crypto::CryptoProvider;
use openmls_memory_storage::MemoryStorage;

use crate::from_wire;

#[derive(Debug, PartialEq, Eq)]
pub enum DsError {
    UnknownGroup,
    /// A handshake message for an epoch other than the current one. The client
    /// fetches the stream from its last `seq`, processes the winning commit and
    /// retries (with the same idempotency key).
    StaleEpoch {
        current: u64,
    },
    /// Application messages are accepted only for the current epoch.
    StaleApplicationEpoch {
        current: u64,
    },
    /// Handshake messages must be PublicMessage so the server can validate them.
    EncryptedHandshake,
    Invalid(String),
    /// The welcome/group info pair does not match the server's view.
    GroupInfoMismatch,
}

pub struct Stored {
    pub seq: u64,
    pub epoch: u64,
    pub handshake: bool,
    pub bytes: Vec<u8>,
}

struct DsGroup {
    public: PublicGroup,
    stream: Vec<Stored>,
    group_info: Vec<u8>,
}

pub struct DeliveryService {
    crypto: CryptoProvider,
    storage: MemoryStorage,
    groups: HashMap<Vec<u8>, DsGroup>,
}

impl Default for DeliveryService {
    fn default() -> Self {
        Self::new()
    }
}

impl DeliveryService {
    pub fn new() -> Self {
        Self {
            crypto: CryptoProvider::new().expect("operating system randomness"),
            storage: MemoryStorage::default(),
            groups: HashMap::new(),
        }
    }

    /// Registers a new group from the creator's signed GroupInfo and ratchet tree.
    pub fn create_group(&mut self, group_info: &[u8], tree: RatchetTreeIn) -> Result<(), DsError> {
        let info = group_info_in(group_info).ok_or(DsError::Invalid("not a group info".into()))?;
        let (public, _) = PublicGroup::from_external(
            &self.crypto,
            &self.storage,
            tree,
            info,
            ProposalStore::new(),
        )
        .map_err(|e| DsError::Invalid(format!("{e:?}")))?;
        let id = public.group_id().as_slice().to_vec();
        self.groups.insert(
            id,
            DsGroup {
                public,
                stream: Vec::new(),
                group_info: group_info.to_vec(),
            },
        );
        Ok(())
    }

    pub fn epoch(&self, group: &GroupId) -> Option<u64> {
        self.groups
            .get(group.as_slice())
            .map(|g| g.public.group_context().epoch().as_u64())
    }

    pub fn members(&self, group: &GroupId) -> Vec<Credential> {
        self.groups
            .get(group.as_slice())
            .map(|g| g.public.members().map(|m| m.credential).collect())
            .unwrap_or_default()
    }

    /// Accepts one message into the conversation stream and returns its `seq`.
    /// `group_info` is the committer's GroupInfo for the new epoch (required with
    /// commits, so external joiners always get a current one).
    pub fn submit(&mut self, bytes: &[u8], group_info: Option<&[u8]>) -> Result<u64, DsError> {
        let message = from_wire(bytes)
            .try_into_protocol_message()
            .map_err(|e| DsError::Invalid(format!("{e:?}")))?;
        let group = self
            .groups
            .get_mut(message.group_id().as_slice())
            .ok_or(DsError::UnknownGroup)?;
        let current = group.public.group_context().epoch().as_u64();
        let epoch = message.epoch().as_u64();
        let handshake = message.is_handshake_message();

        if handshake {
            let ProtocolMessage::PublicMessage(_) = &message else {
                return Err(DsError::EncryptedHandshake);
            };
            if epoch != current {
                return Err(DsError::StaleEpoch { current });
            }
            let processed = group
                .public
                .process_message(&self.crypto, message)
                .map_err(|e| DsError::Invalid(format!("{e:?}")))?;
            match processed.into_content() {
                ProcessedMessageContent::StagedCommitMessage(commit) => {
                    if let Some(info) = group_info {
                        let info_epoch = group_info_in(info)
                            .map(|i| i.epoch().as_u64())
                            .ok_or(DsError::GroupInfoMismatch)?;
                        if info_epoch != current + 1 {
                            return Err(DsError::GroupInfoMismatch);
                        }
                        group.group_info = info.to_vec();
                    }
                    group
                        .public
                        .merge_commit(&self.storage, *commit)
                        .map_err(|e| DsError::Invalid(format!("{e:?}")))?;
                }
                ProcessedMessageContent::ProposalMessage(proposal) => {
                    group
                        .public
                        .add_proposal(&self.storage, *proposal)
                        .map_err(|e| DsError::Invalid(format!("{e:?}")))?;
                }
                other => return Err(DsError::Invalid(format!("unexpected {other:?}"))),
            }
        } else if epoch != current {
            return Err(DsError::StaleApplicationEpoch { current });
        }

        let seq = group.stream.len() as u64 + 1;
        group.stream.push(Stored {
            seq,
            epoch,
            handshake,
            bytes: bytes.to_vec(),
        });
        Ok(seq)
    }

    /// The conversation stream after `seq` (a history page, docs/extension.md).
    pub fn after(&self, group: &GroupId, seq: u64) -> Vec<&Stored> {
        self.groups
            .get(group.as_slice())
            .map(|g| g.stream.iter().filter(|s| s.seq > seq).collect())
            .unwrap_or_default()
    }

    /// What an external joiner needs: the newest member-signed GroupInfo and the
    /// server's copy of the ratchet tree.
    pub fn join_info(&self, group: &GroupId) -> Option<(Vec<u8>, RatchetTree)> {
        self.groups
            .get(group.as_slice())
            .map(|g| (g.group_info.clone(), g.public.export_ratchet_tree()))
    }
}

fn group_info_in(bytes: &[u8]) -> Option<VerifiableGroupInfo> {
    match from_wire(bytes).extract() {
        MlsMessageBodyIn::GroupInfo(info) => Some(info),
        _ => None,
    }
}
