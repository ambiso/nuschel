// SPDX-License-Identifier: EUPL-1.2
//! The server-side interface. `nuscheld` uses only this trait, so the backend can be
//! replaced (for example by an IETF KEYTRANS log) without touching the server.

use crate::error::DirectoryError;
use crate::head::SignedHead;
use crate::log::LogDescriptor;
use crate::messages::{AuditResponse, HistoryResponse, LookupResponse};
use crate::types::{DirectoryValue, Label};
use crate::witness::Checkpoint;
use crate::witness::auditor::AddEpochs;
use crate::witness::tlog::Hash;
use async_trait::async_trait;

/// Most label updates in one epoch.
pub const MAX_UPDATES_PER_EPOCH: usize = 100_000;

/// One verifiable key directory: the server's KT log (docs/e2ee.md §5.1).
///
/// The server decides *who* may ask for what (history only for one's own labels and
/// one's contacts, rate limits on lookups); the directory only proves.
#[async_trait]
pub trait Directory: Send + Sync {
    /// What clients pin.
    fn descriptor(&self) -> &LogDescriptor;

    /// Publishes a new epoch containing `updates` (distinct labels) and returns its
    /// signed head. Updates whose value equals the current one are skipped; if nothing
    /// changes, no epoch is created and the current head is returned. `now_ms` is the
    /// signing time; heads never go back in time even if the clock does.
    async fn publish(
        &self,
        updates: Vec<(Label, DirectoryValue)>,
        now_ms: u64,
    ) -> Result<SignedHead, DirectoryError>;

    /// Re-signs the current epoch at `now_ms`, so that a fresh head exists while nothing
    /// changes (hourly, docs/e2ee.md §5.1).
    async fn refresh_head(&self, now_ms: u64) -> Result<SignedHead, DirectoryError>;

    /// The newest signed head.
    async fn latest_head(&self) -> Result<SignedHead, DirectoryError>;

    /// The newest signed head of an earlier epoch, for audits in steps.
    async fn head_at(&self, epoch: u64) -> Result<SignedHead, DirectoryError>;

    /// The current value of `label` with a proof against the latest head.
    /// [`DirectoryError::NotFound`] if the label was never published (not provable).
    async fn lookup(&self, label: &Label) -> Result<LookupResponse, DirectoryError>;

    /// Every value `label` ever had, with a proof against the latest head.
    async fn history(&self, label: &Label) -> Result<HistoryResponse, DirectoryError>;

    /// Append-only proof from `from_epoch` to `to_epoch` (at most
    /// [`crate::MAX_AUDIT_SPAN`] epochs apart).
    async fn audit(&self, from_epoch: u64, to_epoch: u64) -> Result<AuditResponse, DirectoryError>;

    /// The epoch log's size: the newest epoch + 1 (docs/e2ee.md §5.9).
    async fn log_size(&self) -> Result<u64, DirectoryError>;

    /// The epoch log's checkpoint at its current size under `origin`, and the signed
    /// note (the log's signature only) to collect cosignatures on.
    async fn checkpoint(&self, origin: &str) -> Result<(Checkpoint, String), DirectoryError>;

    /// The inclusion proof of epoch `epoch`'s leaf in the epoch log of `size` leaves.
    async fn inclusion(&self, epoch: u64, size: u64) -> Result<Vec<Hash>, DirectoryError>;

    /// The project auditor's next submission for `origin`, from the `old_size` it
    /// holds (0 for none) forward by at most `max_epochs` (at most
    /// [`crate::MAX_AUDIT_SPAN`]); without new epochs it refreshes the cosignature.
    async fn add_epochs(
        &self,
        origin: &str,
        old_size: u64,
        max_epochs: u64,
    ) -> Result<AddEpochs, DirectoryError>;
}
