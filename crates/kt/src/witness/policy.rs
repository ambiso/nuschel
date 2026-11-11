// SPDX-License-Identifier: EUPL-1.2
//! What clients require of a server's view (docs/e2ee.md §5.9): a checkpoint of its
//! epoch log signed by the log, cosigned by enough of the configured witnesses no
//! longer ago than the freshness bound, with the head's epoch as its newest leaf.
//!
//! While no witness is configured (today: [`DEFAULT_WITNESSES`] is empty until the
//! project auditor is deployed, and nobody configured one), nothing is required and
//! clients behave as before; a checkpoint a server sends anyway is still checked
//! (signature, origin, inclusion), so the code path runs before it matters.

use super::note::{CosignerKey, Note};
use super::tlog;
use super::{Checkpoint, EpochLeaf, WitnessedHead};
use crate::error::KtError;
use crate::head::VerifiedHead;
use crate::log::LogDescriptor;

/// A witness every client checks unless configured otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefaultWitness {
    /// Its C2SP verifier key (`<name>+<key id>+<base64(0x06 || ML-DSA-44 key)>`).
    pub vkey: &'static str,
    /// Where servers submit their epochs (`https://…`, without the
    /// `/nuschl-kt/v1/add-epochs` path).
    pub url: &'static str,
}

/// The built-in witness set, shared by clients and `nuschld`'s defaults.
///
/// **Empty until the project auditor is deployed**: switching it on is one entry
/// here with the auditor's verifier key (`nuschl-witness vkey`) and URL, released
/// with servers first (docs/witness.md, "Switching it on").
pub const DEFAULT_WITNESSES: &[DefaultWitness] = &[];

/// Default freshness bound: a frozen view lasts at most a day (docs/e2ee.md §5.9).
pub const DEFAULT_MAX_AGE_MS: u64 = 24 * 3600 * 1000;
/// Tolerated clock difference for cosignatures from the future.
pub const MAX_FUTURE_MS: u64 = 3600 * 1000;
/// Most witnesses in a policy (signed notes must accept at least 16 signatures).
pub const MAX_WITNESSES: usize = 16;

/// The witnesses a client requires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WitnessPolicy {
    pub witnesses: Vec<CosignerKey>,
    /// How many of `witnesses` must have cosigned; 0 requires nothing. With several
    /// witnesses, every pair of clients' quorums must intersect: all clients that
    /// require `k` of the same `n` need `2k > n` (`3k > 2n` if some may lie).
    pub required: usize,
    pub max_age_ms: u64,
    pub max_future_ms: u64,
}

impl Default for WitnessPolicy {
    fn default() -> Self {
        Self::off()
    }
}

impl WitnessPolicy {
    /// Nothing required.
    pub fn off() -> Self {
        Self {
            witnesses: Vec::new(),
            required: 0,
            max_age_ms: DEFAULT_MAX_AGE_MS,
            max_future_ms: MAX_FUTURE_MS,
        }
    }

    /// Every one of `witnesses` required.
    pub fn all(witnesses: Vec<CosignerKey>, max_age_ms: u64) -> Self {
        Self {
            required: witnesses.len(),
            witnesses,
            max_age_ms,
            max_future_ms: MAX_FUTURE_MS,
        }
    }

    pub fn is_active(&self) -> bool {
        // An impossible quorum is unavailable, never implicitly switched off.
        self.required > 0
    }
}

/// How a head relates to its server's witnessed epoch log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WitnessOutcome {
    /// The head is the newest leaf of a checkpoint that enough witnesses cosigned
    /// within the bound. `cosigned_ms` is the oldest of the counted cosignatures'
    /// times: the view is witnessed until `cosigned_ms + max_age`.
    Witnessed { size: u64, cosigned_ms: u64 },
    /// The checkpoint is valid and fresh, but older than the head (published within
    /// the last minutes): re-check soon.
    Pending { size: u64, cosigned_ms: u64 },
}

/// Checks a head against what its server attached (rules C2–C4 of the review): the
/// checkpoint is signed by the pinned log under `origin` (the server as the client
/// reaches it), cosigned by `policy.required` of the policy's witnesses no older than
/// `policy.max_age_ms`, and contains the head as its newest leaf.
///
/// - [`KtError::CheckpointInvalid`]: malformed, not the log's, another origin, a
///   known witness's cosignature that fails, or a wrong inclusion proof.
/// - [`KtError::NotNewest`]: the server presents an older epoch than its witnessed
///   log holds (a frozen view).
/// - [`KtError::WitnessMissing`]: too few fresh cosignatures ("Automatic
///   verification unavailable"). Never returned when the policy requires nothing.
pub fn check_witnessed(
    descriptor: &LogDescriptor,
    origin: &str,
    head: &VerifiedHead,
    witnessed: &WitnessedHead,
    policy: &WitnessPolicy,
    now_ms: u64,
) -> Result<WitnessOutcome, KtError> {
    let invalid = |reason: &str| KtError::CheckpointInvalid(reason.to_owned());
    let note = Note::parse(&witnessed.checkpoint)
        .map_err(|e| KtError::CheckpointInvalid(format!("malformed note: {e}")))?;
    let log = super::note::LogNoteKey {
        name: origin.to_owned(),
        key: descriptor.signing_key().clone(),
    };
    if !log.verify(&note) {
        return Err(invalid("the checkpoint is not signed by this server's log"));
    }
    let checkpoint = Checkpoint::parse(&note.text)
        .map_err(|e| KtError::CheckpointInvalid(format!("malformed checkpoint: {e}")))?;
    if checkpoint.origin != origin {
        return Err(invalid("the checkpoint is for another origin"));
    }
    if head.log_id() != descriptor.log_id() {
        return Err(KtError::WrongLog {
            expected: descriptor.log_id(),
            got: head.log_id(),
        });
    }

    // Cosignatures: a known witness's line that fails rejects the note.
    let mut fresh = Vec::new();
    let mut problems = Vec::new();
    for witness in &policy.witnesses {
        match witness.verify(&note, &checkpoint) {
            Err(_) => {
                return Err(KtError::CheckpointInvalid(format!(
                    "the cosignature of {} does not verify",
                    witness.name()
                )));
            }
            Ok(None) => problems.push(format!("{} has not cosigned it", witness.name())),
            Ok(Some(cosigned)) => {
                let at_ms = cosigned.timestamp.saturating_mul(1000);
                if at_ms > now_ms.saturating_add(policy.max_future_ms) {
                    problems.push(format!(
                        "{}'s cosignature is from the future",
                        witness.name()
                    ));
                } else if now_ms.saturating_sub(at_ms) > policy.max_age_ms {
                    problems.push(format!(
                        "{}'s newest cosignature is {} hours old",
                        witness.name(),
                        now_ms.saturating_sub(at_ms) / 3_600_000
                    ));
                } else {
                    fresh.push(at_ms);
                }
            }
        }
    }
    if policy.is_active() && fresh.len() < policy.required {
        return Err(KtError::WitnessMissing(if problems.is_empty() {
            "too few witnesses cosigned the server's log".into()
        } else {
            problems.join("; ")
        }));
    }
    // The oldest of the newest `required` cosignatures bounds the view's age.
    fresh.sort_unstable_by(|a, b| b.cmp(a));
    let cosigned_ms = match policy.required {
        0 => fresh.first().copied().unwrap_or(0),
        k => fresh.get(k - 1).copied().unwrap_or(0),
    };

    let size = checkpoint.size;
    if head.epoch() >= size {
        if !witnessed.inclusion.is_empty() {
            return Err(invalid(
                "an inclusion proof for a head the checkpoint lacks",
            ));
        }
        return Ok(WitnessOutcome::Pending { size, cosigned_ms });
    }
    let leaf = EpochLeaf {
        log_id: head.log_id(),
        epoch: head.epoch(),
        root: *head.root(),
    };
    if !tlog::verify_inclusion(
        head.epoch(),
        size,
        &leaf.hash(),
        &witnessed.inclusion,
        &checkpoint.root,
    ) {
        return Err(invalid("the head is not in the witnessed checkpoint"));
    }
    if head.epoch() + 1 != size {
        return Err(KtError::NotNewest {
            epoch: head.epoch(),
            size,
        });
    }
    Ok(WitnessOutcome::Witnessed { size, cosigned_ms })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_set_is_empty_until_the_auditor_is_deployed() {
        // Turning it on is a one-line change, made by the user when the auditor
        // runs; until then clients require nothing (docs/witness.md).
        assert!(DEFAULT_WITNESSES.is_empty());
        for witness in DEFAULT_WITNESSES {
            assert!(CosignerKey::from_vkey(witness.vkey).is_ok());
            assert!(witness.url.starts_with("https://"));
        }
        assert!(!WitnessPolicy::off().is_active());
    }
}
