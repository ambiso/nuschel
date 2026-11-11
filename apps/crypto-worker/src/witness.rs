// SPDX-License-Identifier: EUPL-1.2
//! Witnessing on the client (docs/e2ee.md §5.9, rules C0–C5 of
//! docs/reviews/2026-11-11-key-transparency.md as the user decided them).
//!
//! The witnesses the user requires are the built-in ones (`DEFAULT_WITNESSES`, empty
//! until the project auditor is deployed) and those in the settings. While none is
//! required, nothing changes: a checkpoint a server attaches anyway is still checked
//! (the log's signature, the origin and the inclusion proof), and a bad one fails
//! the head like any other bad proof.
//!
//! With witnesses required, every head the server presents as its newest must be the
//! newest leaf of a checkpoint they cosigned within the freshness bound (pending for
//! up to ten minutes after an epoch). Freshness then comes from the witnesses'
//! clocks, not from the server's `issued_at`, which the server can re-sign at will
//! and whose clamp turned a clock jump into an outage. If the cosignatures are
//! missing, stale or unreachable, verification is "unavailable": heads are still
//! checked as before (first contact proceeds as "not verified"), contacts show
//! "Automatic verification unavailable", and contacts' new devices are not added to
//! existing conversations (`block_new_devices`). A checkpoint that contradicts the
//! head (another log, a wrong inclusion proof, an older epoch than the witnessed
//! log holds) fails the head.

use crate::worker::Worker;
use nuschl_ipc::crypto::WitnessStatus;
use nuschl_kt::witness::note::CosignerKey;
use nuschl_kt::{
    DEFAULT_WITNESSES, KtError, LogDescriptor, VerifiedHead, WitnessOutcome, WitnessPolicy,
    WitnessedHead, check_witnessed,
};

/// How long a head may wait for its cosignatures before verification counts as
/// unavailable (rule C4).
const PENDING_MS: u64 = 10 * 60 * 1000;

impl Worker {
    /// The witnesses required now: built-in (unless turned off) and configured.
    pub(crate) fn witness_policy(&self) -> WitnessPolicy {
        let settings = &self.state.witnessing;
        let disabled: Vec<CosignerKey> = settings
            .witnesses
            .iter()
            .filter(|w| !w.enabled)
            .filter_map(|w| CosignerKey::from_vkey(&w.key).ok())
            .collect();
        let builtin = DEFAULT_WITNESSES
            .iter()
            .filter(|_| settings.default_witnesses)
            .map(|w| w.vkey);
        let configured = settings
            .witnesses
            .iter()
            .filter(|w| w.enabled)
            .map(|w| w.key.as_str());
        let mut witnesses: Vec<CosignerKey> = Vec::new();
        let mut invalid = Vec::new();
        let mut intended = Vec::new();
        for vkey in builtin.chain(configured) {
            match CosignerKey::from_vkey(vkey) {
                Ok(key) if !disabled.contains(&key) && !intended.contains(&key) => {
                    intended.push(key.clone());
                    if witnesses.len() < nuschl_kt::witness::policy::MAX_WITNESSES {
                        witnesses.push(key);
                    }
                }
                Err(_) if !invalid.contains(&vkey) => invalid.push(vkey),
                _ => {}
            }
        }
        // Invalid keys and a missing configured quorum must not lower a security
        // requirement. They make verification unavailable until corrected.
        let required = settings
            .required
            .map_or(intended.len() + invalid.len(), |r| r as usize);
        WitnessPolicy {
            witnesses,
            required,
            max_age_ms: u64::from(settings.max_age_hours.clamp(1, 168)) * 3_600_000,
            max_future_ms: nuschl_kt::witness::policy::MAX_FUTURE_MS,
        }
    }

    /// Checks what the server attached to a head it presents as its newest and
    /// records the server's witness state. `Ok(true)`: the witnesses vouch for the
    /// head's freshness, so the server's own `issued_at` is not consulted. `Err`:
    /// the checkpoint contradicts the head, which then fails.
    pub(crate) fn check_witness(
        &mut self,
        key: &str,
        descriptor: &LogDescriptor,
        head: &VerifiedHead,
        witnessed: Option<&[u8]>,
    ) -> Result<bool, KtError> {
        let policy = self.witness_policy();
        let active = policy.is_active();
        let now = self.now();
        let origin = nuschl_kt::witness::origin(key);
        let outcome = match (witnessed, &origin) {
            (None, _) => Err(KtError::WitnessMissing(
                "this server does not offer witnessed key transparency (kt-witness/1)".into(),
            )),
            (Some([]), _) => Err(KtError::WitnessMissing(
                "the server sent no witnessed checkpoint for the address you use".into(),
            )),
            (Some(_), None) => Err(KtError::WitnessMissing(
                "this server's address cannot name a log".into(),
            )),
            (Some(bytes), Some(origin)) => WitnessedHead::decode(bytes)
                .map_err(|e| KtError::CheckpointInvalid(format!("malformed: {e}")))
                .and_then(|witnessed| {
                    check_witnessed(descriptor, origin, head, &witnessed, &policy, now)
                }),
        };
        let Ok(entry) = self.server_mut(key) else {
            return Ok(false);
        };
        entry.witness.offered = witnessed.is_some();
        match outcome {
            Ok(_) if !active => Ok(false),
            Ok(WitnessOutcome::Witnessed { cosigned_ms, .. }) => {
                entry.witness.covered_epoch = Some(head.epoch());
                entry.witness.until_ms = cosigned_ms.saturating_add(policy.max_age_ms);
                entry.witness.pending_since_ms = None;
                entry.witness.failure = None;
                Ok(true)
            }
            Ok(WitnessOutcome::Pending { cosigned_ms, .. }) => {
                entry.witness.until_ms = cosigned_ms.saturating_add(policy.max_age_ms);
                entry.witness.pending_since_ms.get_or_insert(now);
                entry.witness.failure = None;
                Ok(true)
            }
            // Without required witnesses a missing checkpoint changes nothing.
            Err(KtError::WitnessMissing(_)) if !active => Ok(false),
            Err(KtError::WitnessMissing(reason)) => {
                entry.witness.failure = Some(reason);
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Whether a server's view is witnessed, for the UI and the MLS layer.
    pub(crate) fn witness_status(&self, key: &str) -> WitnessStatus {
        if !self.witness_policy().is_active() {
            return WitnessStatus::Off;
        }
        let Some(entry) = self.state.servers.get(key) else {
            return WitnessStatus::Pending;
        };
        let witness = &entry.witness;
        let now = self.now();
        if let Some(reason) = &witness.failure {
            return WitnessStatus::Unavailable {
                reason: reason.clone(),
            };
        }
        if witness.until_ms != 0 && now > witness.until_ms {
            return WitnessStatus::Unavailable {
                reason: format!(
                    "the witnesses' newest cosignatures for this server are older than {} hours",
                    self.state.witnessing.max_age_hours
                ),
            };
        }
        match (witness.pending_since_ms, witness.covered_epoch) {
            (Some(since), _) if now.saturating_sub(since) > PENDING_MS => {
                WitnessStatus::Unavailable {
                    reason: "the server's newest key-log entries have waited more than ten \
                             minutes for the witnesses"
                        .into(),
                }
            }
            (Some(_), _) | (None, None) => WitnessStatus::Pending,
            (None, Some(_)) => WitnessStatus::Witnessed {
                until_ms: witness.until_ms,
            },
        }
    }

    /// Why contacts' new devices must not be added to existing conversations on this
    /// server now, if they must not (`block_new_devices`, the user's proposed
    /// default).
    pub fn new_devices_blocked(&self, key: &str) -> Option<String> {
        if !self.state.witnessing.block_new_devices {
            return None;
        }
        match self.witness_status(key) {
            WitnessStatus::Off | WitnessStatus::Witnessed { .. } => None,
            WitnessStatus::Pending => Some(
                "New devices are added once the witnesses have confirmed this server's key log \
                 (usually within a minute)."
                    .into(),
            ),
            WitnessStatus::Unavailable { reason } => Some(format!(
                "Automatic verification unavailable ({reason}); new devices are added once it \
                 is available again."
            )),
        }
    }
}
