// SPDX-License-Identifier: EUPL-1.2
//! Trust on first use combined with key transparency (docs/e2ee.md §4 and §5.5).
//!
//! TOFU pins a contact's identity root; key transparency shows whether the server tells
//! everyone the same thing. The two answer different questions: a change that KT
//! proves consistent is still a key change (consistent is not benign), and a KT
//! failure never unpins or unverifies anyone, it only withdraws "verified
//! automatically".

use crate::error::{ErrorClass, KtError};
use crate::values::AccountBinding;
use crate::verify::{VerifiedEntry, VerifiedHistory};
use nuschel_identity::IdentityFingerprint;

/// The kind of conversation a result is shown in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversationKind {
    Direct,
    Channel,
}

/// What the client remembers about one contact account (per server account; the pin
/// itself refers to the identity and is shared by all its accounts).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContactTrust {
    /// The pinned identity root, once seen.
    pub pinned: Option<IdentityFingerprint>,
    /// Safety numbers compared out of band (docs/e2ee.md §4).
    pub manually_verified: bool,
    /// The newest KT version of the contact's `acct:` label already assessed.
    pub checked_version: u64,
    /// A KT-consistent rebinding of a manually verified contact, waiting for the user.
    pub pending_change: Option<IdentityFingerprint>,
    /// Consecutive KT failures, and when the first of them happened.
    pub failures: u32,
    pub first_failure_ms: Option<u64>,
}

/// A KT result about a contact's `acct:` label, reduced to identities. Build it with
/// [`Observation::from_lookup`] or [`Observation::from_history`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observation {
    /// A verified lookup of the contact's `acct:` label.
    Lookup { root: IdentityFingerprint, version: u64 },
    /// A verified history (DM and verified contacts, every connect): the current root,
    /// and the roots of all versions after `checked_version` except the current one.
    History {
        root: IdentityFingerprint,
        version: u64,
        intermediate: Vec<IdentityFingerprint>,
    },
    /// Verification failed.
    Failed(KtError),
}

impl Observation {
    /// From a verified lookup; a value that is not an [`AccountBinding`] is a failure.
    pub fn from_lookup(result: Result<VerifiedEntry, KtError>) -> Self {
        match result.and_then(|entry| Ok((binding(&entry)?, entry.version))) {
            Ok((root, version)) => Self::Lookup { root, version },
            Err(e) => Self::Failed(e),
        }
    }

    /// From a verified history; `checked_version` is [`ContactTrust::checked_version`].
    pub fn from_history(result: Result<VerifiedHistory, KtError>, checked_version: u64) -> Self {
        let observe = |history: VerifiedHistory| -> Result<Self, KtError> {
            let latest = history.latest();
            let intermediate = history.entries[1..]
                .iter()
                .take_while(|e| e.version > checked_version)
                .map(binding)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Self::History {
                root: binding(latest)?,
                version: latest.version,
                intermediate,
            })
        };
        result.and_then(observe).unwrap_or_else(Self::Failed)
    }
}

fn binding(entry: &VerifiedEntry) -> Result<IdentityFingerprint, KtError> {
    AccountBinding::from_value(&entry.value)
        .map(|b| b.fingerprint())
        .map_err(|_| KtError::MalformedValue)
}

/// The meaning of an observation for this contact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Assessment {
    /// First sight: the root is pinned now.
    FirstContact { root: IdentityFingerprint },
    /// KT confirms the pinned root.
    Match { manually_verified: bool },
    /// The account is now bound to another root, consistently for everyone.
    KeyChanged {
        from: IdentityFingerprint,
        to: IdentityFingerprint,
        was_verified: bool,
    },
    /// The pinned root is current, but the history shows other roots bound in between:
    /// a temporary fake binding that came and went between lookups (docs/e2ee.md §5.4).
    TransientBinding { seen: Vec<IdentityFingerprint> },
    /// First failure: retry within a day; nothing shown yet.
    Intermittent(KtError),
    /// Repeated failure: "Automatic verification unavailable". A manually verified
    /// contact stays verified.
    Unavailable {
        error: KtError,
        manually_verified: bool,
    },
    /// Proof that the server lied: alarm and keep the evidence ([`KtError::evidence`]).
    ServerMisbehaviour(KtError),
    /// The user's own binding changed without them.
    OwnKeyAlarm(KtError),
}

/// What the UI shows (docs/e2ee.md §4, "Key changes", and §5.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Surface {
    Nothing,
    VerifiedAutomatically,
    VerifiedManually,
    /// An inline notice ("Alice's identity changed"); sending continues.
    KeyChangeNotice,
    /// Red banner; sending blocked until the user accepts (verified direct contact).
    KeyChangeBlocking,
    /// Inline notice; the conversation lock turns amber until acknowledged (verified
    /// channel member).
    KeyChangeAmberLock,
    /// Amber "Automatic verification unavailable" with the reason; offers manual
    /// verification. Messages stay encrypted to the pinned keys.
    AutomaticVerificationUnavailable {
        reason: String,
        still_manually_verified: bool,
    },
    /// The server equivocated or cannot prove its log append-only.
    ServerMisbehaviourAlarm,
    /// Blocking alarm with "This wasn't me: revoke".
    OwnKeyAlarm,
}

impl ContactTrust {
    /// Folds one observation into the contact's state and says what it means.
    pub fn assess(&mut self, observation: Observation, now_ms: u64) -> Assessment {
        let (root, version, intermediate) = match observation {
            Observation::Failed(error) => return self.failed(error, now_ms),
            Observation::Lookup { root, version } => (root, version, Vec::new()),
            Observation::History {
                root,
                version,
                intermediate,
            } => (root, version, intermediate),
        };
        if version < self.checked_version {
            return self.failed(
                KtError::VersionRegressed {
                    known: self.checked_version,
                    got: version,
                },
                now_ms,
            );
        }
        self.failures = 0;
        self.first_failure_ms = None;
        self.checked_version = version;

        let Some(pinned) = self.pinned else {
            self.pinned = Some(root);
            return Assessment::FirstContact { root };
        };
        if root == pinned {
            self.pending_change = None;
            let seen: Vec<IdentityFingerprint> =
                intermediate.into_iter().filter(|r| *r != pinned).collect();
            return if seen.is_empty() {
                Assessment::Match {
                    manually_verified: self.manually_verified,
                }
            } else {
                Assessment::TransientBinding { seen }
            };
        }
        let was_verified = self.manually_verified;
        if was_verified {
            // Keep the verified pin until the user decides; sending is blocked.
            self.pending_change = Some(root);
        } else {
            self.pinned = Some(root);
        }
        Assessment::KeyChanged {
            from: pinned,
            to: root,
            was_verified,
        }
    }

    /// The user accepted a pending change of a verified contact: the new root is
    /// pinned and is no longer verified.
    pub fn accept_change(&mut self) {
        if let Some(root) = self.pending_change.take() {
            self.pinned = Some(root);
            self.manually_verified = false;
        }
    }

    fn failed(&mut self, error: KtError, now_ms: u64) -> Assessment {
        match error.class() {
            ErrorClass::ServerMisbehaviour => Assessment::ServerMisbehaviour(error),
            ErrorClass::OwnKeyAlarm => Assessment::OwnKeyAlarm(error),
            ErrorClass::Failure => {
                self.failures = self.failures.saturating_add(1);
                if self.failures == 1 {
                    self.first_failure_ms = Some(now_ms);
                    Assessment::Intermittent(error)
                } else {
                    Assessment::Unavailable {
                        error,
                        manually_verified: self.manually_verified,
                    }
                }
            }
        }
    }
}

/// Retry an intermittent failure within this time (docs/e2ee.md §5.5).
pub const RETRY_WITHIN_MS: u64 = 24 * 3600 * 1000;

impl Assessment {
    pub fn surface(&self, kind: ConversationKind) -> Surface {
        match self {
            Self::FirstContact { .. } | Self::Intermittent(_) => Surface::Nothing,
            Self::Match {
                manually_verified: true,
            } => Surface::VerifiedManually,
            Self::Match {
                manually_verified: false,
            } => Surface::VerifiedAutomatically,
            Self::KeyChanged { was_verified, .. } => match (kind, was_verified) {
                (ConversationKind::Direct, true) => Surface::KeyChangeBlocking,
                (ConversationKind::Channel, true) => Surface::KeyChangeAmberLock,
                (_, false) => Surface::KeyChangeNotice,
            },
            Self::TransientBinding { .. } => Surface::KeyChangeNotice,
            Self::Unavailable {
                error,
                manually_verified,
            } => Surface::AutomaticVerificationUnavailable {
                reason: error.to_string(),
                still_manually_verified: *manually_verified,
            },
            Self::ServerMisbehaviour(_) => Surface::ServerMisbehaviourAlarm,
            Self::OwnKeyAlarm(_) => Surface::OwnKeyAlarm,
        }
    }
}
