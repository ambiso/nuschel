// SPDX-License-Identifier: EUPL-1.2
//! The project auditor's checks (`nuschl-witness`, docs/e2ee.md §5.9): a witness that
//! also verifies every akd transition before it cosigns, so its cosignature means
//! "this is the only epoch log I saw for this origin, and every epoch in it is an
//! append-only update of the one before".
//!
//! A server submits [`AddEpochs`]: its origin and log descriptor, the size the auditor
//! holds for it, akd's append-only proof from that epoch to its newest, and a signed
//! checkpoint at the new size. The auditor rebuilds the new leaves from the proof's
//! roots, appends them to its frontier (no consistency proof needed), and cosigns only
//! if the result is the checkpoint's root. The same request without a proof refreshes
//! the timestamp of an unchanged checkpoint (hourly).
//!
//! The auditor starts every log at epoch 0, which must be akd's empty directory, so
//! nothing in a log escapes its checks. Which log an origin has is decided once, by
//! the network enrolment check in `nuschl-witness`; afterwards a submission with
//! another descriptor for that origin is refused.

use super::note::{LogNoteKey, Note};
use super::tlog::{Frontier, Hash};
use super::{Checkpoint, EpochLeaf};
use crate::MSG_ADD_EPOCHS;
use crate::MSG_AUDITOR_STATE;
use crate::codec::{DecodeError, EncodeError, Reader, Writer};
use crate::log::{LogDescriptor, MAX_DESCRIPTOR_BYTES};
use crate::messages::{AuditResponse, MAX_AUDIT_PROOF, MAX_AUDIT_SPAN};
use thiserror::Error;

/// Largest encoded [`AuditResponse`].
const MAX_AUDIT_ENCODED: usize = 2 + 16 + 4 + 32 * MAX_AUDIT_SPAN as usize + 4 + MAX_AUDIT_PROOF;
/// Largest encoded [`AddEpochs`].
pub const MAX_ADD_EPOCHS_BYTES: usize = 2
    + 1
    + 255
    + 2
    + MAX_DESCRIPTOR_BYTES
    + 8
    + 1
    + 4
    + MAX_AUDIT_ENCODED
    + 2
    + super::note::MAX_NOTE_BYTES;

/// A server's submission to the auditor.
///
/// ```text
/// struct {
///   uint8  version = 1;
///   uint8  type = 10;
///   opaque origin<1..255>;            // u8 length
///   opaque descriptor<1..512>;        // u16 length: the LogDescriptor
///   uint64 old_size;                  // the size the auditor holds, 0 for a new log
///   uint8  has_audit;                 // 0 or 1
///   opaque audit<0..2^32-1>;          // if present: AuditResponse from epoch
///                                     //   max(old_size, 1) - 1 to the newest epoch
///   opaque checkpoint<1..2^16-1>;     // the log-signed checkpoint at the new size
/// } AddEpochs;
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddEpochs {
    pub origin: String,
    pub descriptor: Vec<u8>,
    pub old_size: u64,
    pub audit: Option<AuditResponse>,
    pub checkpoint: Vec<u8>,
}

impl AddEpochs {
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let mut w = Writer::new();
        w.header(MSG_ADD_EPOCHS);
        w.bytes_u8("add.origin", self.origin.as_bytes(), 255)?;
        w.bytes_u16("add.descriptor", &self.descriptor, MAX_DESCRIPTOR_BYTES)?;
        w.u64(self.old_size);
        w.present(self.audit.is_some());
        if let Some(audit) = &self.audit {
            w.bytes_u32("add.audit", &audit.encode()?, MAX_AUDIT_ENCODED)?;
        }
        w.bytes_u16(
            "add.checkpoint",
            &self.checkpoint,
            super::note::MAX_NOTE_BYTES,
        )?;
        Ok(w.finish())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes, MAX_ADD_EPOCHS_BYTES)?;
        r.header(MSG_ADD_EPOCHS)?;
        let origin = std::str::from_utf8(r.bytes_u8("add.origin", 255)?)
            .map_err(|_| DecodeError::InvalidValue {
                field: "add.origin",
            })?
            .to_owned();
        if !super::note::valid_name(&origin) {
            return Err(DecodeError::InvalidValue {
                field: "add.origin",
            });
        }
        let descriptor = r
            .bytes_u16("add.descriptor", MAX_DESCRIPTOR_BYTES)?
            .to_vec();
        let old_size = r.u64("add.old_size")?;
        let audit = if r.present("add.has_audit")? {
            Some(AuditResponse::decode(
                r.bytes_u32("add.audit", MAX_AUDIT_ENCODED)?,
            )?)
        } else {
            None
        };
        let checkpoint = r
            .bytes_u16("add.checkpoint", super::note::MAX_NOTE_BYTES)?
            .to_vec();
        r.finish()?;
        Ok(Self {
            origin,
            descriptor,
            old_size,
            audit,
            checkpoint,
        })
    }
}

/// What the auditor keeps per origin.
///
/// ```text
/// struct {
///   uint8  version = 1;
///   uint8  type = 11;
///   opaque origin<1..255>;
///   opaque descriptor<1..512>;
///   uint64 size;
///   uint8  count;  opaque frontier[32][count];   // one per set bit of size
///   opaque akd_root[32];                          // the root of epoch size - 1
///   uint64 timestamp;                             // the newest cosignature's
/// } AuditorState;
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogState {
    pub origin: String,
    pub descriptor: Vec<u8>,
    pub frontier: Frontier,
    pub akd_root: Hash,
    pub timestamp: u64,
}

const MAX_STATE_BYTES: usize = 2 + 1 + 255 + 2 + MAX_DESCRIPTOR_BYTES + 8 + 1 + 64 * 32 + 32 + 8;

impl LogState {
    pub fn size(&self) -> u64 {
        self.frontier.size()
    }

    /// The checkpoint this state stands for.
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            origin: self.origin.clone(),
            size: self.size(),
            root: self.frontier.root(),
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let mut w = Writer::new();
        w.header(MSG_AUDITOR_STATE);
        w.bytes_u8("state.origin", self.origin.as_bytes(), 255)?;
        w.bytes_u16("state.descriptor", &self.descriptor, MAX_DESCRIPTOR_BYTES)?;
        w.u64(self.size());
        w.count_u8("state.frontier", self.frontier.hashes().len(), 64)?;
        for hash in self.frontier.hashes() {
            w.raw(hash);
        }
        w.raw(&self.akd_root);
        w.u64(self.timestamp);
        Ok(w.finish())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let invalid = |field| DecodeError::InvalidValue { field };
        let mut r = Reader::new(bytes, MAX_STATE_BYTES)?;
        r.header(MSG_AUDITOR_STATE)?;
        let origin = std::str::from_utf8(r.bytes_u8("state.origin", 255)?)
            .map_err(|_| invalid("state.origin"))?
            .to_owned();
        let descriptor = r
            .bytes_u16("state.descriptor", MAX_DESCRIPTOR_BYTES)?
            .to_vec();
        LogDescriptor::decode(&descriptor)?;
        let size = r.u64("state.size")?;
        let count = r.count_u8("state.frontier", 64, 32)?;
        let mut hashes = Vec::with_capacity(count);
        for _ in 0..count {
            hashes.push(r.array("state.hash")?);
        }
        let frontier = Frontier::from_parts(size, hashes).ok_or(invalid("state.frontier"))?;
        let akd_root = r.array("state.akd_root")?;
        let timestamp = r.u64("state.timestamp")?;
        r.finish()?;
        if size == 0 || !super::note::valid_name(&origin) {
            return Err(invalid("state"));
        }
        Ok(Self {
            origin,
            descriptor,
            frontier,
            akd_root,
            timestamp,
        })
    }
}

/// Why the auditor refuses a submission; each maps to an HTTP status (C2SP
/// tlog-witness uses the same ones).
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AddError {
    /// 400: unparsable.
    #[error("malformed submission: {0}")]
    Malformed(String),
    /// 403: the checkpoint is not signed by the log, or the origin belongs to another
    /// log (a server showing two logs: logged as possible evidence).
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// 409: `old_size` is not the auditor's size for this origin, which it reports.
    #[error("the auditor holds size {size}")]
    Conflict { size: u64 },
    /// 422: the epochs or the checkpoint do not check out.
    #[error("refused: {0}")]
    Invalid(String),
}

impl AddError {
    pub fn status(&self) -> u16 {
        match self {
            Self::Malformed(_) => 400,
            Self::Forbidden(_) => 403,
            Self::Conflict { .. } => 409,
            Self::Invalid(_) => 422,
        }
    }
}

/// A submission the auditor accepted: the state to persist and the checkpoint to
/// cosign.
#[derive(Clone, Debug)]
pub struct Accepted {
    pub state: LogState,
    pub checkpoint: Checkpoint,
}

/// Checks a submission against the state held for its origin (`None` for an origin
/// whose log the caller has just enrolled). Pure: the caller persists the returned
/// state *before* it hands out the cosignature, and serialises calls per origin.
pub fn add_epochs(state: Option<&LogState>, request: &AddEpochs) -> Result<Accepted, AddError> {
    let descriptor = LogDescriptor::decode(&request.descriptor)
        .map_err(|e| AddError::Malformed(format!("descriptor: {e}")))?;
    let note = Note::parse(&request.checkpoint)
        .map_err(|e| AddError::Malformed(format!("checkpoint note: {e}")))?;
    let checkpoint = Checkpoint::parse(&note.text)
        .map_err(|e| AddError::Malformed(format!("checkpoint: {e}")))?;
    if checkpoint.origin != request.origin {
        return Err(AddError::Malformed(
            "the checkpoint names another origin".into(),
        ));
    }
    let log = LogNoteKey {
        name: request.origin.clone(),
        key: descriptor.signing_key().clone(),
    };
    if !log.verify(&note) {
        return Err(AddError::Forbidden(
            "the checkpoint is not signed by the log's key".into(),
        ));
    }
    let log_id = descriptor.log_id();
    let mut next = match state {
        Some(state) => {
            if state.origin != request.origin {
                return Err(AddError::Malformed("state for another origin".into()));
            }
            if state.descriptor != request.descriptor {
                return Err(AddError::Forbidden(
                    "this origin is enrolled with another log".into(),
                ));
            }
            if request.old_size != state.size() {
                return Err(AddError::Conflict { size: state.size() });
            }
            state.clone()
        }
        None => {
            if request.old_size != 0 {
                return Err(AddError::Conflict { size: 0 });
            }
            // Epoch 0 is akd's empty directory: nothing escapes the audits.
            let genesis = EpochLeaf {
                log_id,
                epoch: 0,
                root: crate::akd_backend::empty_root(),
            };
            let mut frontier = Frontier::new();
            frontier.append(genesis.hash());
            LogState {
                origin: request.origin.clone(),
                descriptor: request.descriptor.clone(),
                frontier,
                akd_root: genesis.root,
                timestamp: 0,
            }
        }
    };
    if let Some(audit) = &request.audit {
        let newest = next.size() - 1;
        if audit.from_epoch != newest {
            return Err(AddError::Invalid(format!(
                "the proof starts at epoch {}, not {newest}",
                audit.from_epoch
            )));
        }
        crate::akd_backend::verify_audit_roots(&next.akd_root, audit)
            .map_err(|e| AddError::Invalid(e.to_string()))?;
        for (offset, root) in audit.roots.iter().enumerate() {
            let leaf = EpochLeaf {
                log_id,
                epoch: newest + 1 + offset as u64,
                root: *root,
            };
            next.frontier
                .append(leaf.hash())
                .ok_or_else(|| AddError::Invalid("the log is full".into()))?;
            next.akd_root = *root;
        }
    }
    if checkpoint.size != next.size() || checkpoint.root != next.frontier.root() {
        return Err(AddError::Invalid(format!(
            "the checkpoint (size {}) is not the log the epochs make (size {})",
            checkpoint.size,
            next.size()
        )));
    }
    Ok(Accepted {
        state: next,
        checkpoint,
    })
}
