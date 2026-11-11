// SPDX-License-Identifier: EUPL-1.2
//! Server responses. The envelope is the same for every backend; the `proof` field is
//! encoded by the log's suite.

use crate::codec::{DecodeError, Reader, Writer};
use crate::head::{MAX_EPOCH, SignedHead};
use crate::types::Label;
use crate::{MSG_AUDIT, MSG_HISTORY, MSG_LOOKUP};

/// Largest lookup proof.
pub const MAX_LOOKUP_PROOF: usize = 256 * 1024;
/// Largest history proof.
pub const MAX_HISTORY_PROOF: usize = 8 * 1024 * 1024;
/// Largest audit proof in one response.
pub const MAX_AUDIT_PROOF: usize = 32 * 1024 * 1024;
/// Most epochs one audit response may span; longer gaps are audited in steps between
/// signed heads ([`crate::Directory::head_at`]).
pub const MAX_AUDIT_SPAN: u64 = 4096;

const ENVELOPE: usize = 2 + crate::head::HEAD_BYTES + crate::types::MAX_LABEL_ENCODED + 4;

/// The latest value of a label, with a proof against `head`.
///
/// `struct { uint8 version = 1; uint8 type = 3; SignedHead head; Label label; opaque proof<0..2^32-1>; }`
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LookupResponse {
    pub head: SignedHead,
    pub label: Label,
    pub proof: Vec<u8>,
}

/// Every version of a label, with a proof against `head`.
///
/// `struct { uint8 version = 1; uint8 type = 4; SignedHead head; Label label; opaque proof<0..2^32-1>; }`
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HistoryResponse {
    pub head: SignedHead,
    pub label: Label,
    pub proof: Vec<u8>,
}

fn encode_labelled(
    message_type: u8,
    head: &SignedHead,
    label: &Label,
    proof: &[u8],
    max: usize,
) -> Result<Vec<u8>, crate::codec::EncodeError> {
    let mut w = Writer::new();
    w.header(message_type);
    head.write(&mut w);
    label.write(&mut w)?;
    w.bytes_u32("proof", proof, max)?;
    Ok(w.finish())
}

fn decode_labelled(
    bytes: &[u8],
    message_type: u8,
    max: usize,
) -> Result<(SignedHead, Label, Vec<u8>), DecodeError> {
    let mut r = Reader::new(bytes, ENVELOPE + max)?;
    r.header(message_type)?;
    let head = SignedHead::read(&mut r)?;
    let label = Label::read(&mut r)?;
    let proof = r.bytes_u32("proof", max)?.to_vec();
    r.finish()?;
    Ok((head, label, proof))
}

impl LookupResponse {
    pub fn encode(&self) -> Result<Vec<u8>, crate::codec::EncodeError> {
        encode_labelled(
            MSG_LOOKUP,
            &self.head,
            &self.label,
            &self.proof,
            MAX_LOOKUP_PROOF,
        )
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let (head, label, proof) = decode_labelled(bytes, MSG_LOOKUP, MAX_LOOKUP_PROOF)?;
        Ok(Self { head, label, proof })
    }
}

impl HistoryResponse {
    pub fn encode(&self) -> Result<Vec<u8>, crate::codec::EncodeError> {
        encode_labelled(
            MSG_HISTORY,
            &self.head,
            &self.label,
            &self.proof,
            MAX_HISTORY_PROOF,
        )
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let (head, label, proof) = decode_labelled(bytes, MSG_HISTORY, MAX_HISTORY_PROOF)?;
        Ok(Self { head, label, proof })
    }
}

/// Proof that the directory at `to_epoch` extends the one at `from_epoch` by additions
/// only. `roots` are the root hashes of epochs `from_epoch + 1 ..= to_epoch`; each step
/// is proven, so only the two ends need signed heads.
///
/// ```text
/// struct {
///   uint8  version = 1;
///   uint8  type = 5;
///   uint64 from_epoch;
///   uint64 to_epoch;                 // from_epoch < to_epoch <= from_epoch + 4096
///   opaque roots<32 * (to_epoch - from_epoch)>;   // u32 count, then the roots
///   opaque proof<0..2^32-1>;
/// } AuditResponse;
/// ```
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AuditResponse {
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub roots: Vec<[u8; 32]>,
    pub proof: Vec<u8>,
}

impl AuditResponse {
    pub fn encode(&self) -> Result<Vec<u8>, crate::codec::EncodeError> {
        let mut w = Writer::new();
        w.header(MSG_AUDIT);
        w.u64(self.from_epoch);
        w.u64(self.to_epoch);
        w.count_u32("roots", self.roots.len(), MAX_AUDIT_SPAN as usize)?;
        for root in &self.roots {
            w.raw(root);
        }
        w.bytes_u32("proof", &self.proof, MAX_AUDIT_PROOF)?;
        Ok(w.finish())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let max = 2 + 16 + 4 + 32 * MAX_AUDIT_SPAN as usize + 4 + MAX_AUDIT_PROOF;
        let mut r = Reader::new(bytes, max)?;
        r.header(MSG_AUDIT)?;
        let from_epoch = r.u64("audit.from_epoch")?;
        let to_epoch = r.u64("audit.to_epoch")?;
        if from_epoch >= to_epoch || to_epoch > MAX_EPOCH || to_epoch - from_epoch > MAX_AUDIT_SPAN
        {
            return Err(DecodeError::InvalidValue {
                field: "audit.epochs",
            });
        }
        let count = r.count_u32("audit.roots", MAX_AUDIT_SPAN as usize, 32)?;
        if count as u64 != to_epoch - from_epoch {
            return Err(DecodeError::InvalidValue {
                field: "audit.roots",
            });
        }
        let mut roots = Vec::with_capacity(count);
        for _ in 0..count {
            roots.push(r.array("audit.root")?);
        }
        let proof = r.bytes_u32("proof", MAX_AUDIT_PROOF)?.to_vec();
        r.finish()?;
        Ok(Self {
            from_epoch,
            to_epoch,
            roots,
            proof,
        })
    }
}
