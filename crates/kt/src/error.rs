// SPDX-License-Identifier: EUPL-1.2
//! Errors of the client checks, and what each means for the user (docs/e2ee.md §5.5).

use crate::codec::{DecodeError, EncodeError};
use crate::head::Evidence;
use crate::log::{LogId, SignerError};
use thiserror::Error;

/// Which proof failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofKind {
    Lookup,
    History,
}

/// A failed client-side check.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum KtError {
    #[error("malformed key-transparency message: {0}")]
    Malformed(#[from] DecodeError),
    #[error("the head belongs to another log ({got:?}, expected {expected:?})")]
    WrongLog { expected: LogId, got: LogId },
    #[error("the head's signature is invalid")]
    BadSignature,
    #[error("the response is for another label than the one requested")]
    LabelMismatch,
    #[error("the head is {age_ms} ms old")]
    StaleHead { age_ms: u64 },
    #[error("the head was signed {ahead_ms} ms in the future")]
    HeadFromFuture { ahead_ms: u64 },
    #[error("the server went back from epoch {latest} to {got}")]
    Rollback { latest: u64, got: u64 },
    #[error("the server signed two different roots for epoch {}", .0.first.epoch)]
    Equivocation(Box<Evidence>),
    #[error("the server signed epoch {} after epoch {}", .0.first.epoch, .0.second.epoch)]
    StaleSigning(Box<Evidence>),
    #[error("the {proof:?} proof does not verify: {reason}")]
    ProofInvalid { proof: ProofKind, reason: String },
    #[error("the server cannot prove that epoch {to} extends epoch {from}: {reason}")]
    AuditFailed { from: u64, to: u64, reason: String },
    #[error("a directory value is malformed or of an unknown format")]
    MalformedValue,
    #[error("the identity record does not match the log's roster commitment")]
    RosterMismatch,
    #[error("the committed identity record does not verify: {0}")]
    InvalidRecord(String),
    #[error("the server reports no entry (unauthenticated)")]
    NotFound,
    #[error("own key changed without this identity: version {version} at epoch {epoch}")]
    UnexpectedOwnValue { version: u64, epoch: u64 },
    #[error("version {version} changed between two verified histories")]
    HistoryRewritten { version: u64 },
    #[error("the label went back from version {known} to {got}")]
    VersionRegressed { known: u64, got: u64 },
    #[error("the server's checkpoint does not check out: {0}")]
    CheckpointInvalid(String),
    #[error(
        "the server showed epoch {epoch} as its newest although its witnessed log holds {size} epochs"
    )]
    NotNewest { epoch: u64, size: u64 },
    #[error("automatic verification unavailable: {0}")]
    WitnessMissing(String),
    #[error("the log maps {what} to another account")]
    NameMismatch { what: String },
}

/// What a [`KtError`] means for the user, following Signal's flow (docs/e2ee.md §5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// Verification did not succeed, without proof that the server lied (a bug, a
    /// network problem, or an attack that cannot be shown to others). First time:
    /// "intermittent", retry within a day; again: "Automatic verification
    /// unavailable" (amber). Messages stay encrypted to the pinned keys.
    Failure,
    /// The server contradicted itself: two heads it signed, two verified histories, or
    /// heads it cannot connect by an append-only proof. Alarm; keep the evidence.
    ServerMisbehaviour,
    /// The user's own binding changed without their devices: the blocking alarm with
    /// "This wasn't me: revoke" (docs/e2ee.md §4).
    OwnKeyAlarm,
}

impl KtError {
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::Equivocation(_)
            | Self::StaleSigning(_)
            | Self::AuditFailed { .. }
            | Self::HistoryRewritten { .. }
            | Self::VersionRegressed { .. } => ErrorClass::ServerMisbehaviour,
            Self::UnexpectedOwnValue { .. } => ErrorClass::OwnKeyAlarm,
            Self::Malformed(_)
            | Self::WrongLog { .. }
            | Self::BadSignature
            | Self::LabelMismatch
            | Self::StaleHead { .. }
            | Self::HeadFromFuture { .. }
            | Self::Rollback { .. }
            | Self::ProofInvalid { .. }
            | Self::MalformedValue
            | Self::RosterMismatch
            | Self::InvalidRecord(_)
            | Self::NotFound
            | Self::CheckpointInvalid(_)
            | Self::NotNewest { .. }
            | Self::WitnessMissing(_)
            | Self::NameMismatch { .. } => ErrorClass::Failure,
        }
    }

    /// Signed evidence that can be shown to others, if the error carries it.
    pub fn evidence(&self) -> Option<&Evidence> {
        match self {
            Self::Equivocation(e) | Self::StaleSigning(e) => Some(e),
            _ => None,
        }
    }
}

/// Errors of the server-side [`crate::Directory`].
#[derive(Debug, Error)]
pub enum DirectoryError {
    #[error("no entry for this label")]
    NotFound,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("the store holds a log with other keys or parameters")]
    KeyMismatch,
    #[error("the store is inconsistent: {0}")]
    Corrupt(String),
    #[error("storage: {0}")]
    Store(#[from] crate::store::StoreError),
    #[error(transparent)]
    Signer(#[from] SignerError),
    #[error("encoding: {0}")]
    Encode(#[from] EncodeError),
    #[error("backend: {0}")]
    Backend(String),
}
