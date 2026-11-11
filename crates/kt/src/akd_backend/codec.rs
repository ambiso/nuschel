// SPDX-License-Identifier: EUPL-1.2
//! Canonical encodings of akd's proofs (suite 1). akd's own protobuf encoding is not
//! used: it would add a protobuf runtime on the client's untrusted-input path, and it
//! is neither canonical nor bounded.
//!
//! ```text
//! NodeLabel          = uint16 len_bits (<= 256) || opaque val[32]
//! AzksElement        = NodeLabel label || opaque value[32]
//! SiblingProof       = NodeLabel label || AzksElement sibling || uint8 direction (0 left, 1 right)
//! MembershipProof    = NodeLabel label || opaque hash[32] || uint16 n (<= 256) || SiblingProof[n]
//! NonMembershipProof = NodeLabel label || NodeLabel longest_prefix || AzksElement children[2]
//!                      || MembershipProof longest_prefix_proof
//! VrfProof           = opaque proof[80]
//! LookupProof        = uint64 epoch || uint64 version || opaque value<0..4100> (u32 len)
//!                      || VrfProof existence_vrf || MembershipProof existence
//!                      || VrfProof marker_vrf || MembershipProof marker
//!                      || VrfProof freshness_vrf || NonMembershipProof freshness
//!                      || opaque commitment_nonce[32]
//! UpdateProof        = uint64 epoch || uint64 version || opaque value<0..4100> (u32 len)
//!                      || VrfProof existence_vrf || MembershipProof existence
//!                      || [version > 1] VrfProof previous_vrf || MembershipProof previous
//!                      || opaque commitment_nonce[32]
//! HistoryProof       = uint32 n (<= 4096) || UpdateProof[n]
//!                      || uint8 p (<= 128) || VrfProof[p] || uint8 p' (= p) || MembershipProof[p']
//!                      || uint8 f (<= 128) || VrfProof[f] || uint8 f' (= f) || NonMembershipProof[f']
//! AppendOnlyProof    = uint32 n (<= 4096) || ( uint32 i || AzksElement inserted[i]
//!                      || uint32 u || AzksElement unchanged[u] )[n] || uint32 n' (= n) || uint64 epochs[n']
//! ```
//!
//! The previous-version fields of an update proof are present exactly when its version
//! is above 1, as akd produces them; anything else is rejected, so each proof has one
//! encoding.

use crate::codec::{DecodeError, EncodeError, Reader, Writer};
use crate::messages::{MAX_AUDIT_SPAN, MAX_HISTORY_PROOF, MAX_LOOKUP_PROOF};
use crate::types::MAX_VALUE_ENCODED;
use akd_core::{
    AkdValue, AppendOnlyProof, AzksElement, AzksValue, Direction, HistoryProof, LookupProof,
    MembershipProof, NodeLabel, NonMembershipProof, SingleAppendOnlyProof, SiblingProof,
    UpdateProof,
};

const VRF_PROOF: usize = 80;
const MAX_SIBLINGS: usize = 256;
const MAX_MARKERS: usize = 128;
/// Most versions in a history proof.
pub const MAX_HISTORY_VERSIONS: usize = 4096;
const NODE_LABEL: usize = 2 + 32;
const AZKS_ELEMENT: usize = NODE_LABEL + 32;
const MEMBERSHIP_MIN: usize = NODE_LABEL + 32 + 2;
const NON_MEMBERSHIP_MIN: usize = 2 * NODE_LABEL + 2 * AZKS_ELEMENT + MEMBERSHIP_MIN;
const UPDATE_MIN: usize = 16 + 4 + VRF_PROOF + MEMBERSHIP_MIN + 32;

fn write_label(w: &mut Writer, label: &NodeLabel) -> Result<(), EncodeError> {
    if label.label_len > 256 {
        return Err(EncodeError {
            field: "node_label.len",
            len: label.label_len as usize,
            max: 256,
        });
    }
    w.u16(label.label_len as u16);
    w.raw(&label.label_val);
    Ok(())
}

fn read_label(r: &mut Reader<'_>) -> Result<NodeLabel, DecodeError> {
    let len = r.u16("node_label.len")?;
    if len > 256 {
        return Err(DecodeError::InvalidValue {
            field: "node_label.len",
        });
    }
    Ok(NodeLabel::new(r.array("node_label.val")?, u32::from(len)))
}

fn write_element(w: &mut Writer, e: &AzksElement) -> Result<(), EncodeError> {
    write_label(w, &e.label)?;
    w.raw(&e.value.0);
    Ok(())
}

fn read_element(r: &mut Reader<'_>) -> Result<AzksElement, DecodeError> {
    Ok(AzksElement {
        label: read_label(r)?,
        value: AzksValue(r.array("azks_element.value")?),
    })
}

fn write_membership(w: &mut Writer, p: &MembershipProof) -> Result<(), EncodeError> {
    write_label(w, &p.label)?;
    w.raw(&p.hash_val.0);
    w.count_u16("siblings", p.sibling_proofs.len(), MAX_SIBLINGS)?;
    for s in &p.sibling_proofs {
        write_label(w, &s.label)?;
        write_element(w, &s.siblings[0])?;
        w.u8(s.direction as u8);
    }
    Ok(())
}

fn read_membership(r: &mut Reader<'_>) -> Result<MembershipProof, DecodeError> {
    let label = read_label(r)?;
    let hash_val = AzksValue(r.array("membership.hash")?);
    let n = r.count_u16("siblings", MAX_SIBLINGS, NODE_LABEL + AZKS_ELEMENT + 1)?;
    let mut sibling_proofs = Vec::with_capacity(n);
    for _ in 0..n {
        let label = read_label(r)?;
        let sibling = read_element(r)?;
        let direction = match r.u8("sibling.direction")? {
            0 => Direction::Left,
            1 => Direction::Right,
            _ => {
                return Err(DecodeError::InvalidValue {
                    field: "sibling.direction",
                });
            }
        };
        sibling_proofs.push(SiblingProof {
            label,
            siblings: [sibling],
            direction,
        });
    }
    Ok(MembershipProof {
        label,
        hash_val,
        sibling_proofs,
    })
}

fn write_non_membership(w: &mut Writer, p: &NonMembershipProof) -> Result<(), EncodeError> {
    write_label(w, &p.label)?;
    write_label(w, &p.longest_prefix)?;
    write_element(w, &p.longest_prefix_children[0])?;
    write_element(w, &p.longest_prefix_children[1])?;
    write_membership(w, &p.longest_prefix_membership_proof)
}

fn read_non_membership(r: &mut Reader<'_>) -> Result<NonMembershipProof, DecodeError> {
    Ok(NonMembershipProof {
        label: read_label(r)?,
        longest_prefix: read_label(r)?,
        longest_prefix_children: [read_element(r)?, read_element(r)?],
        longest_prefix_membership_proof: read_membership(r)?,
    })
}

fn write_vrf(w: &mut Writer, proof: &[u8]) -> Result<(), EncodeError> {
    if proof.len() != VRF_PROOF {
        return Err(EncodeError {
            field: "vrf_proof",
            len: proof.len(),
            max: VRF_PROOF,
        });
    }
    w.raw(proof);
    Ok(())
}

fn read_vrf(r: &mut Reader<'_>) -> Result<Vec<u8>, DecodeError> {
    Ok(r.take(VRF_PROOF, "vrf_proof")?.to_vec())
}

fn write_nonce(w: &mut Writer, nonce: &[u8]) -> Result<(), EncodeError> {
    if nonce.len() != 32 {
        return Err(EncodeError {
            field: "commitment_nonce",
            len: nonce.len(),
            max: 32,
        });
    }
    w.raw(nonce);
    Ok(())
}

fn read_nonce(r: &mut Reader<'_>) -> Result<Vec<u8>, DecodeError> {
    Ok(r.take(32, "commitment_nonce")?.to_vec())
}

pub(crate) fn encode_lookup(p: &LookupProof) -> Result<Vec<u8>, EncodeError> {
    let mut w = Writer::new();
    w.u64(p.epoch);
    w.u64(p.version);
    w.bytes_u32("value", &p.value.0, MAX_VALUE_ENCODED)?;
    write_vrf(&mut w, &p.existence_vrf_proof)?;
    write_membership(&mut w, &p.existence_proof)?;
    write_vrf(&mut w, &p.marker_vrf_proof)?;
    write_membership(&mut w, &p.marker_proof)?;
    write_vrf(&mut w, &p.freshness_vrf_proof)?;
    write_non_membership(&mut w, &p.freshness_proof)?;
    write_nonce(&mut w, &p.commitment_nonce)?;
    Ok(w.finish())
}

pub(crate) fn decode_lookup(bytes: &[u8]) -> Result<LookupProof, DecodeError> {
    let mut r = Reader::new(bytes, MAX_LOOKUP_PROOF)?;
    let p = LookupProof {
        epoch: r.u64("lookup.epoch")?,
        version: r.u64("lookup.version")?,
        value: AkdValue(r.bytes_u32("value", MAX_VALUE_ENCODED)?.to_vec()),
        existence_vrf_proof: read_vrf(&mut r)?,
        existence_proof: read_membership(&mut r)?,
        marker_vrf_proof: read_vrf(&mut r)?,
        marker_proof: read_membership(&mut r)?,
        freshness_vrf_proof: read_vrf(&mut r)?,
        freshness_proof: read_non_membership(&mut r)?,
        commitment_nonce: read_nonce(&mut r)?,
    };
    r.finish()?;
    Ok(p)
}

fn write_update(w: &mut Writer, p: &UpdateProof) -> Result<(), EncodeError> {
    w.u64(p.epoch);
    w.u64(p.version);
    w.bytes_u32("value", &p.value.0, MAX_VALUE_ENCODED)?;
    write_vrf(w, &p.existence_vrf_proof)?;
    write_membership(w, &p.existence_proof)?;
    match (
        p.version > 1,
        &p.previous_version_vrf_proof,
        &p.previous_version_proof,
    ) {
        (true, Some(vrf), Some(proof)) => {
            write_vrf(w, vrf)?;
            write_membership(w, proof)?;
        }
        (false, None, None) => {}
        _ => {
            return Err(EncodeError {
                field: "update.previous_version",
                len: 0,
                max: 0,
            });
        }
    }
    write_nonce(w, &p.commitment_nonce)
}

fn read_update(r: &mut Reader<'_>) -> Result<UpdateProof, DecodeError> {
    let epoch = r.u64("update.epoch")?;
    let version = r.u64("update.version")?;
    let value = AkdValue(r.bytes_u32("value", MAX_VALUE_ENCODED)?.to_vec());
    let existence_vrf_proof = read_vrf(r)?;
    let existence_proof = read_membership(r)?;
    let (previous_version_vrf_proof, previous_version_proof) = if version > 1 {
        (Some(read_vrf(r)?), Some(read_membership(r)?))
    } else {
        (None, None)
    };
    Ok(UpdateProof {
        epoch,
        value,
        version,
        existence_vrf_proof,
        existence_proof,
        previous_version_vrf_proof,
        previous_version_proof,
        commitment_nonce: read_nonce(r)?,
    })
}

pub(crate) fn encode_history(p: &HistoryProof) -> Result<Vec<u8>, EncodeError> {
    let mut w = Writer::new();
    w.count_u32("updates", p.update_proofs.len(), MAX_HISTORY_VERSIONS)?;
    for u in &p.update_proofs {
        write_update(&mut w, u)?;
    }
    w.count_u8("past_markers", p.past_marker_vrf_proofs.len(), MAX_MARKERS)?;
    for v in &p.past_marker_vrf_proofs {
        write_vrf(&mut w, v)?;
    }
    w.count_u8(
        "past_marker_proofs",
        p.existence_of_past_marker_proofs.len(),
        MAX_MARKERS,
    )?;
    for m in &p.existence_of_past_marker_proofs {
        write_membership(&mut w, m)?;
    }
    w.count_u8("future_markers", p.future_marker_vrf_proofs.len(), MAX_MARKERS)?;
    for v in &p.future_marker_vrf_proofs {
        write_vrf(&mut w, v)?;
    }
    w.count_u8(
        "future_marker_proofs",
        p.non_existence_of_future_marker_proofs.len(),
        MAX_MARKERS,
    )?;
    for m in &p.non_existence_of_future_marker_proofs {
        write_non_membership(&mut w, m)?;
    }
    if w.len() > MAX_HISTORY_PROOF {
        return Err(EncodeError {
            field: "history_proof",
            len: w.len(),
            max: MAX_HISTORY_PROOF,
        });
    }
    Ok(w.finish())
}

pub(crate) fn decode_history(bytes: &[u8]) -> Result<HistoryProof, DecodeError> {
    let mut r = Reader::new(bytes, MAX_HISTORY_PROOF)?;
    let n = r.count_u32("updates", MAX_HISTORY_VERSIONS, UPDATE_MIN)?;
    let mut update_proofs = Vec::with_capacity(n);
    for _ in 0..n {
        update_proofs.push(read_update(&mut r)?);
    }
    let p = r.count_u8("past_markers", MAX_MARKERS, VRF_PROOF)?;
    let past_marker_vrf_proofs = (0..p)
        .map(|_| read_vrf(&mut r))
        .collect::<Result<Vec<_>, _>>()?;
    if r.count_u8("past_marker_proofs", MAX_MARKERS, MEMBERSHIP_MIN)? != p {
        return Err(DecodeError::InvalidValue {
            field: "past_marker_proofs",
        });
    }
    let existence_of_past_marker_proofs = (0..p)
        .map(|_| read_membership(&mut r))
        .collect::<Result<Vec<_>, _>>()?;
    let f = r.count_u8("future_markers", MAX_MARKERS, VRF_PROOF)?;
    let future_marker_vrf_proofs = (0..f)
        .map(|_| read_vrf(&mut r))
        .collect::<Result<Vec<_>, _>>()?;
    if r.count_u8("future_marker_proofs", MAX_MARKERS, NON_MEMBERSHIP_MIN)? != f {
        return Err(DecodeError::InvalidValue {
            field: "future_marker_proofs",
        });
    }
    let non_existence_of_future_marker_proofs = (0..f)
        .map(|_| read_non_membership(&mut r))
        .collect::<Result<Vec<_>, _>>()?;
    r.finish()?;
    Ok(HistoryProof {
        update_proofs,
        past_marker_vrf_proofs,
        existence_of_past_marker_proofs,
        future_marker_vrf_proofs,
        non_existence_of_future_marker_proofs,
    })
}

pub(crate) fn encode_append_only(p: &AppendOnlyProof) -> Result<Vec<u8>, EncodeError> {
    let mut w = Writer::new();
    w.count_u32("audit.proofs", p.proofs.len(), MAX_AUDIT_SPAN as usize)?;
    for single in &p.proofs {
        w.count_u32("audit.inserted", single.inserted.len(), usize::MAX)?;
        for e in &single.inserted {
            write_element(&mut w, e)?;
        }
        w.count_u32("audit.unchanged", single.unchanged_nodes.len(), usize::MAX)?;
        for e in &single.unchanged_nodes {
            write_element(&mut w, e)?;
        }
    }
    w.count_u32("audit.epochs", p.epochs.len(), MAX_AUDIT_SPAN as usize)?;
    for e in &p.epochs {
        w.u64(*e);
    }
    Ok(w.finish())
}

pub(crate) fn decode_append_only(bytes: &[u8]) -> Result<AppendOnlyProof, DecodeError> {
    let mut r = Reader::new(bytes, crate::messages::MAX_AUDIT_PROOF)?;
    let n = r.count_u32("audit.proofs", MAX_AUDIT_SPAN as usize, 8)?;
    let mut proofs = Vec::with_capacity(n);
    for _ in 0..n {
        let i = r.count_u32("audit.inserted", usize::MAX, AZKS_ELEMENT)?;
        let inserted = (0..i)
            .map(|_| read_element(&mut r))
            .collect::<Result<Vec<_>, _>>()?;
        let u = r.count_u32("audit.unchanged", usize::MAX, AZKS_ELEMENT)?;
        let unchanged_nodes = (0..u)
            .map(|_| read_element(&mut r))
            .collect::<Result<Vec<_>, _>>()?;
        proofs.push(SingleAppendOnlyProof {
            inserted,
            unchanged_nodes,
        });
    }
    if r.count_u32("audit.epochs", MAX_AUDIT_SPAN as usize, 8)? != n {
        return Err(DecodeError::InvalidValue {
            field: "audit.epochs",
        });
    }
    let epochs = (0..n)
        .map(|_| r.u64("audit.epoch"))
        .collect::<Result<Vec<_>, _>>()?;
    r.finish()?;
    Ok(AppendOnlyProof { proofs, epochs })
}
