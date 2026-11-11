// SPDX-License-Identifier: EUPL-1.2
//! Witnessing (docs/e2ee.md §5.9, option B of docs/reviews/2026-11-11-key-transparency.md):
//! each server keeps a small append-only *epoch log* whose leaf `i` is its akd epoch
//! `i`, signs C2SP checkpoints of it, and collects cosignatures from a small fixed set
//! of witnesses, led by the project auditor `nuschl-witness`, which checks every akd
//! transition before it cosigns. Clients require those cosignatures, no older than a
//! freshness bound, before they treat the server's view as everyone's.
//!
//! - [`tlog`]: RFC 6962 hashing, the frontier and inclusion proofs.
//! - [`note`]: C2SP signed notes, the log's Ed25519 signature and ML-DSA-44
//!   cosignatures.
//! - [`Checkpoint`], [`EpochLeaf`], [`WitnessedHead`]: the formats.
//! - [`auditor`]: what the auditor checks before it cosigns.
//! - [`policy`]: what clients require.

pub mod auditor;
pub mod note;
pub mod policy;
pub mod tlog;

use crate::codec::{DecodeError, Reader, Writer};
use crate::log::LogId;
use crate::{MSG_EPOCH_LEAF, MSG_WITNESSED};
use tlog::Hash;

/// The suffix of every epoch log's origin: `<host>:<port>/nuschl-kt/v1`.
pub const ORIGIN_SUFFIX: &str = "/nuschl-kt/v1";

/// The origin of the epoch log of the server clients reach as `host:port` (the
/// session's sticky key: lower-case host, then the port). `None` if it would not be
/// a valid C2SP origin (empty, spaces, `+`, control characters, over 255 bytes).
pub fn origin(server_key: &str) -> Option<String> {
    let origin = format!("{}{ORIGIN_SUFFIX}", server_key.to_ascii_lowercase());
    (note::valid_name(&origin) && !server_key.is_empty()).then_some(origin)
}

/// One leaf of the epoch log: an akd epoch's root.
///
/// ```text
/// struct {
///   uint8  version = 1;
///   uint8  type = 8;
///   opaque log_id[32];
///   uint64 epoch;          // equals the leaf index
///   opaque root[32];       // the akd root hash at `epoch`
/// } EpochLeaf;             // 74 bytes; the tree hashes SHA-256(0x00 || EpochLeaf)
/// ```
///
/// No time: freshness comes from the witnesses' timestamps, never from the server's
/// clock (docs/e2ee.md §5.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpochLeaf {
    pub log_id: LogId,
    pub epoch: u64,
    pub root: [u8; 32],
}

pub const EPOCH_LEAF_BYTES: usize = 2 + 32 + 8 + 32;

impl EpochLeaf {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.header(MSG_EPOCH_LEAF);
        w.raw(&self.log_id.0);
        w.u64(self.epoch);
        w.raw(&self.root);
        w.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes, EPOCH_LEAF_BYTES)?;
        r.header(MSG_EPOCH_LEAF)?;
        let leaf = Self {
            log_id: LogId(r.array("leaf.log_id")?),
            epoch: r.u64("leaf.epoch")?,
            root: r.array("leaf.root")?,
        };
        r.finish()?;
        Ok(leaf)
    }

    pub fn hash(&self) -> Hash {
        tlog::leaf_hash(&self.encode())
    }
}

/// A checkpoint of an epoch log (c2sp.org/tlog-checkpoint): origin, tree size and root
/// hash. Nuschl's checkpoints carry no extension lines, and parsing refuses them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub origin: String,
    pub size: u64,
    pub root: Hash,
}

impl Checkpoint {
    /// The note text: `<origin>\n<size>\n<base64 root>\n`.
    pub fn body(&self) -> String {
        format!(
            "{}\n{}\n{}\n",
            self.origin,
            self.size,
            note::base64_encode(&self.root)
        )
    }

    pub fn parse(text: &str) -> Result<Self, DecodeError> {
        let invalid = |field| DecodeError::InvalidValue { field };
        let body = text
            .strip_suffix('\n')
            .ok_or_else(|| invalid("checkpoint"))?;
        let mut lines = body.split('\n');
        let (Some(origin), Some(size), Some(root), None) =
            (lines.next(), lines.next(), lines.next(), lines.next())
        else {
            return Err(invalid("checkpoint.lines"));
        };
        if !note::valid_name(origin) {
            return Err(invalid("checkpoint.origin"));
        }
        // ASCII decimal without leading zeros (zero is "0").
        if size.is_empty()
            || size.len() > 20
            || !size.bytes().all(|b| b.is_ascii_digit())
            || (size.len() > 1 && size.starts_with('0'))
        {
            return Err(invalid("checkpoint.size"));
        }
        let size = size.parse().map_err(|_| invalid("checkpoint.size"))?;
        let root: Hash = note::base64_decode(root, "checkpoint.root")?
            .try_into()
            .map_err(|_| invalid("checkpoint.root"))?;
        Ok(Self {
            origin: origin.to_owned(),
            size,
            root,
        })
    }
}

/// Most hashes in a [`WitnessedHead`]'s inclusion proof (epochs are below 2^48).
pub const MAX_INCLUSION: usize = 48;

/// What a server attaches to a head: its newest cosigned checkpoint and the
/// inclusion proof of the head's [`EpochLeaf`] in it (empty when the head is newer
/// than the checkpoint, which is then *pending*).
///
/// ```text
/// struct {
///   uint8  version = 1;
///   uint8  type = 9;
///   opaque checkpoint<0..2^16-1>;   // a signed note, at most 64 KiB
///   uint8  count;                   // at most 48
///   opaque inclusion[32][count];
/// } WitnessedHead;
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WitnessedHead {
    pub checkpoint: Vec<u8>,
    pub inclusion: Vec<Hash>,
}

pub const MAX_WITNESSED_BYTES: usize = 2 + 2 + note::MAX_NOTE_BYTES + 1 + 32 * MAX_INCLUSION;

impl WitnessedHead {
    pub fn encode(&self) -> Result<Vec<u8>, crate::codec::EncodeError> {
        let mut w = Writer::new();
        w.header(MSG_WITNESSED);
        w.bytes_u16(
            "witnessed.checkpoint",
            &self.checkpoint,
            note::MAX_NOTE_BYTES,
        )?;
        w.count_u8("witnessed.inclusion", self.inclusion.len(), MAX_INCLUSION)?;
        for hash in &self.inclusion {
            w.raw(hash);
        }
        Ok(w.finish())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes, MAX_WITNESSED_BYTES)?;
        r.header(MSG_WITNESSED)?;
        let checkpoint = r
            .bytes_u16("witnessed.checkpoint", note::MAX_NOTE_BYTES)?
            .to_vec();
        let count = r.count_u8("witnessed.inclusion", MAX_INCLUSION, 32)?;
        let mut inclusion = Vec::with_capacity(count);
        for _ in 0..count {
            inclusion.push(r.array("witnessed.hash")?);
        }
        r.finish()?;
        Ok(Self {
            checkpoint,
            inclusion,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_follow_the_sticky_key() {
        assert_eq!(
            origin("Chat.Example.org:64738").unwrap(),
            "chat.example.org:64738/nuschl-kt/v1"
        );
        assert!(origin("").is_none());
        assert!(origin("a b:1").is_none());
        assert!(origin(&"a".repeat(250)).is_none());
    }

    #[test]
    fn checkpoints_are_parsed_strictly() {
        let cp = Checkpoint {
            origin: "a.example:1/nuschl-kt/v1".into(),
            size: 0,
            root: tlog::empty_root(),
        };
        assert_eq!(Checkpoint::parse(&cp.body()).unwrap(), cp);
        let root = note::base64_encode(&[0; 32]);
        for bad in [
            format!("o\n01\n{root}\n"),
            format!("o\n1\n{root}"),
            format!("o\n1\n{root}\next\n"),
            format!("o\n-1\n{root}\n"),
            format!("o p\n1\n{root}\n"),
            "o\n1\nAAAA\n".to_owned(),
            format!("o\n18446744073709551616\n{root}\n"),
        ] {
            assert!(Checkpoint::parse(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn leaves_and_witnessed_heads_round_trip() {
        let leaf = EpochLeaf {
            log_id: LogId([1; 32]),
            epoch: 9,
            root: [2; 32],
        };
        assert_eq!(EpochLeaf::decode(&leaf.encode()).unwrap(), leaf);
        assert_eq!(leaf.encode().len(), EPOCH_LEAF_BYTES);
        let witnessed = WitnessedHead {
            checkpoint: b"x\n\n".to_vec(),
            inclusion: vec![[3; 32]; 4],
        };
        let bytes = witnessed.encode().unwrap();
        assert_eq!(WitnessedHead::decode(&bytes).unwrap(), witnessed);
        let mut long = bytes.clone();
        long.push(0);
        assert!(WitnessedHead::decode(&long).is_err());
        let mut many = witnessed.clone();
        many.inclusion = vec![[0; 32]; MAX_INCLUSION + 1];
        assert!(many.encode().is_err());
    }
}
