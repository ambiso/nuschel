// SPDX-License-Identifier: EUPL-1.2
//! Client-side verification: pure functions over pinned descriptors and server
//! responses. No I/O and no clock; the caller passes the time where it matters.

use crate::akd_backend;
use crate::error::KtError;
use crate::head::{VerifiedHead, verify_head};
use crate::log::{LogDescriptor, Suite};
use crate::messages::{AuditResponse, HistoryResponse, LookupResponse};
use crate::types::{DirectoryValue, Label};

/// One verified version of a label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedEntry {
    pub label: Label,
    pub value: DirectoryValue,
    /// 1 for the first value, +1 for every change.
    pub version: u64,
    /// The epoch in which this version was published.
    pub epoch: u64,
}

/// The complete, verified history of a label at `head`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedHistory {
    pub label: Label,
    pub head: VerifiedHead,
    /// Newest first; versions `n, n-1, …, 1`, epochs strictly decreasing.
    pub entries: Vec<VerifiedEntry>,
}

impl VerifiedHistory {
    pub fn latest(&self) -> &VerifiedEntry {
        // `verify_history` never returns an empty history.
        &self.entries[0]
    }

    pub fn version(&self, version: u64) -> Option<&VerifiedEntry> {
        let latest = self.latest().version;
        let index = latest.checked_sub(version)?;
        self.entries.get(usize::try_from(index).ok()?)
    }
}

/// Verifies a lookup: the head belongs to the pinned log and is signed, the response is
/// for `label`, and the proof shows `value` as the label's latest version at that head.
///
/// The caller then checks the head against what it knows ([`crate::KnownHeads`]) and
/// its freshness.
pub fn verify_lookup(
    descriptor: &LogDescriptor,
    label: &Label,
    response: &LookupResponse,
) -> Result<(VerifiedHead, VerifiedEntry), KtError> {
    let head = verify_head(descriptor, &response.head)?;
    if &response.label != label {
        return Err(KtError::LabelMismatch);
    }
    let entry = match descriptor.suite() {
        Suite::AkdBlake3Ecvrf25519V1 => {
            akd_backend::verify_lookup(descriptor, &head, label, &response.proof)?
        }
    };
    Ok((head, entry))
}

/// Verifies a complete history of `label` at the response's head.
pub fn verify_history(
    descriptor: &LogDescriptor,
    label: &Label,
    response: &HistoryResponse,
) -> Result<VerifiedHistory, KtError> {
    let head = verify_head(descriptor, &response.head)?;
    if &response.label != label {
        return Err(KtError::LabelMismatch);
    }
    let entries = match descriptor.suite() {
        Suite::AkdBlake3Ecvrf25519V1 => {
            akd_backend::verify_history(descriptor, &head, label, &response.proof)?
        }
    };
    Ok(VerifiedHistory {
        label: label.clone(),
        head,
        entries,
    })
}

/// Verifies that the directory at `to` extends the one at `from` by additions only.
/// Both heads must already be verified ([`crate::verify_head`]).
///
/// Blocking and CPU-bound (it rebuilds the changed part of the tree); run it off the
/// UI thread. Works inside or outside a Tokio runtime.
pub fn verify_audit(
    descriptor: &LogDescriptor,
    from: &VerifiedHead,
    to: &VerifiedHead,
    response: &AuditResponse,
) -> Result<(), KtError> {
    let fail = |reason: &str| KtError::AuditFailed {
        from: from.epoch(),
        to: to.epoch(),
        reason: reason.to_owned(),
    };
    let log_id = descriptor.log_id();
    if from.log_id() != log_id || to.log_id() != log_id {
        return Err(KtError::WrongLog {
            expected: log_id,
            got: if from.log_id() != log_id {
                from.log_id()
            } else {
                to.log_id()
            },
        });
    }
    if from.epoch() >= to.epoch() {
        return Err(fail("the end is not after the start"));
    }
    if response.from_epoch != from.epoch() || response.to_epoch != to.epoch() {
        return Err(fail("the response covers other epochs"));
    }
    if response.roots.last() != Some(to.root()) {
        return Err(fail("the last root differs from the end head"));
    }
    match descriptor.suite() {
        Suite::AkdBlake3Ecvrf25519V1 => akd_backend::verify_audit(from, to, response),
    }
}

/// The newest state of an own label that the client has verified: the base for the
/// next self-monitoring check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorCheckpoint {
    pub version: u64,
    pub epoch: u64,
    pub value: DirectoryValue,
}

/// The result of a successful self-monitoring check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorReport {
    /// Store this for the next check.
    pub checkpoint: MonitorCheckpoint,
    /// Versions published since the previous checkpoint (all authorised), newest first.
    pub new_entries: Vec<VerifiedEntry>,
}

/// Self-monitoring (docs/e2ee.md §5.4): every version of one of the user's own labels
/// since `previous` must carry a value the user's devices made (`authorised`), and the
/// history must still contain `previous` unchanged.
///
/// - [`KtError::UnexpectedOwnValue`]: someone else's key was bound to the user: the
///   blocking "This wasn't me" alarm.
/// - [`KtError::HistoryRewritten`], [`KtError::VersionRegressed`]: the server shows a
///   history that contradicts one it proved before.
///
/// Whether the user's newest value is already published is the caller's comparison of
/// `checkpoint.value` with what it submitted (inclusion takes up to a minute).
pub fn monitor_own(
    history: &VerifiedHistory,
    previous: Option<&MonitorCheckpoint>,
    authorised: impl Fn(&DirectoryValue) -> bool,
) -> Result<MonitorReport, KtError> {
    let latest = history.latest();
    let since = match previous {
        Some(prev) => {
            if latest.version < prev.version {
                return Err(KtError::VersionRegressed {
                    known: prev.version,
                    got: latest.version,
                });
            }
            match history.version(prev.version) {
                Some(entry) if entry.value == prev.value && entry.epoch == prev.epoch => {}
                _ => {
                    return Err(KtError::HistoryRewritten {
                        version: prev.version,
                    });
                }
            }
            prev.version
        }
        None => 0,
    };
    let new_entries: Vec<VerifiedEntry> = history
        .entries
        .iter()
        .take_while(|e| e.version > since)
        .cloned()
        .collect();
    // Oldest unexpected version first: that is when the takeover began.
    if let Some(bad) = new_entries.iter().rev().find(|e| !authorised(&e.value)) {
        return Err(KtError::UnexpectedOwnValue {
            version: bad.version,
            epoch: bad.epoch,
        });
    }
    Ok(MonitorReport {
        checkpoint: MonitorCheckpoint {
            version: latest.version,
            epoch: latest.epoch,
            value: latest.value.clone(),
        },
        new_entries,
    })
}
