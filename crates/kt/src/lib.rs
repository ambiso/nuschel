// SPDX-License-Identifier: EUPL-1.2
//! Key transparency for Nuschel servers (docs/e2ee.md §5).
//!
//! Each Nuschel server runs one verifiable key directory. It maps [`Label`]s (account
//! ids, identity fingerprints, user names) to versioned [`DirectoryValue`]s (for an
//! account: an [`AccountBinding`], the identity root public key and a commitment to the
//! identity's device roster, from `nuschel-identity`), publishes the changes in epochs,
//! and signs a [`SignedHead`] for every epoch with its identity key.
//! Clients check that what the server tells them is what it tells everyone, in the
//! manner of Signal's automatic key verification.
//!
//! # The interface
//!
//! This is the part that stays when the backend changes (today `akd`; later perhaps an
//! IETF KEYTRANS log, as a new [`Suite`]):
//!
//! - **Server:** the [`Directory`] trait: [`publish`](Directory::publish) an epoch,
//!   [`lookup`](Directory::lookup), [`history`](Directory::history),
//!   [`audit`](Directory::audit) between epochs, and signed heads
//!   ([`latest_head`](Directory::latest_head), [`refresh_head`](Directory::refresh_head),
//!   [`head_at`](Directory::head_at)). [`AkdDirectory`] implements it on any
//!   [`KtStore`], the storage trait the server backs with its database.
//! - **Client:** pure functions. [`verify_head`], [`check_freshness`] and
//!   [`check_successor`]/[`KnownHeads`] for heads; [`verify_lookup`];
//!   [`verify_history`] and [`monitor_own`] for self-monitoring; [`verify_audit`] for
//!   the append-only property; [`roster_for_binding`] to check the device certificates
//!   from the directory API against the log; [`ContactTrust::assess`] for the TOFU
//!   decision (docs/e2ee.md §5.5) and [`Assessment::surface`] for what the UI shows.
//! - **Gossip:** [`HeadGossip`] and [`check_gossip`] for heads exchanged in MLS
//!   application messages (docs/e2ee.md §5.3); [`Evidence`] when two heads contradict.
//!
//! # Client flow
//!
//! On every connection: `verify_head` the server's head, `check_freshness` (< 25 h),
//! then `KnownHeads::classify_from_server`. For [`Relation::Newer`], fetch
//! `audit(since, head)`, `verify_audit`, then `KnownHeads::record`. Lookups and
//! histories carry their own head, which goes through the same steps. On connect and
//! every 24 h, `verify_history` + `monitor_own` for the user's own `acct:` and `id:`
//! labels; for DM and verified contacts, `verify_history` and `ContactTrust::assess`.
//!
//! # Wire formats
//!
//! Every message starts with `uint8 version = 1, uint8 type`; integers are big-endian;
//! every variable-length field has a length prefix and a maximum; enum and option tags
//! accept only defined values; trailing bytes are an error; parsers check counts
//! against the remaining input before allocating. Each value has exactly one encoding.
//!
//! | Type | Message | Size |
//! | --- | --- | --- |
//! | 1 | [`LogDescriptor`] | 70 bytes (Ed25519, akd) |
//! | 2 | [`SignedHead`] | 148 bytes |
//! | 3 | [`LookupResponse`] | ≤ 256 KiB proof |
//! | 4 | [`HistoryResponse`] | ≤ 8 MiB proof |
//! | 5 | [`AuditResponse`] | ≤ 4096 epochs, ≤ 32 MiB proof |
//! | 6 | [`HeadGossip`] | 150 bytes |
//! | 7 | [`Evidence`] | 299 bytes |
//!
//! Labels and values have their own canonical encodings ([`Label::encode`],
//! [`DirectoryValue::encode`]); suite 1's proof encodings are documented in
//! `akd_backend/codec.rs`. Known-answer vectors: `tests/vectors/kt-v1.txt`.
//!
//! # Privacy
//!
//! - **Labels are hidden by a VRF.** A label's position in the tree is
//!   `VRF(label, version)` (ECVRF-EDWARDS25519-SHA512-TAI). Only the server, holding
//!   the VRF secret, can compute it; a client can only check a VRF proof for a label it
//!   already knows. Proofs show other users only as pseudorandom 256-bit node labels,
//!   so the tree cannot be enumerated or joined with a list of account ids or names
//!   without asking the server, which sees and can rate-limit every lookup. (Account
//!   ids are random 128-bit values anyway; names and identity fingerprints are not.)
//! - **Values are hidden by commitments.** Leaves hold `H(value, nonce)` with a nonce
//!   derived from the VRF secret; sibling hashes in a proof reveal nothing about
//!   other users' keys.
//! - **What a lookup reveals about others:** the approximate size of the directory
//!   (path length ≈ log2 of the number of leaves), and, across epochs, whether the
//!   subtrees next to the looked-up label changed. About the looked-up label itself:
//!   its value, its version count and the epoch of its latest change.
//! - **History** reveals every past value and when it changed. The server serves it
//!   only for one's own labels and contacts (docs/e2ee.md §5.4).
//! - **Audit proofs** reveal how many leaves each epoch added (one per new label, two
//!   per change), not whose.
//! - **The server** sees who looks up whom and when (docs/e2ee.md §2).
//! - **Post-quantum:** label and value hiding rest on the discrete logarithm problem
//!   (an attacker who recovers the VRF secret from its public key learns the commitment
//!   key too), and heads are signed with Ed25519. Tree integrity rests on BLAKE3.
//!
//! # Not provable
//!
//! akd has no absence proof in its public API: "no entry" ([`DirectoryError::NotFound`],
//! [`KtError::NotFound`]) is unauthenticated, and clients treat it as a failure, never as
//! "this account has no key".

mod akd_backend;
mod codec;
mod error;
mod gossip;
mod head;
mod log;
mod messages;
mod server;
mod store;
mod tofu;
mod types;
mod values;
mod verify;

pub use akd_backend::{AkdConfig, AkdDirectory, VrfSecret, descriptor as akd_descriptor};
pub use codec::{DecodeError, EncodeError};
pub use error::{DirectoryError, ErrorClass, KtError, ProofKind};
pub use gossip::{
    GOSSIP_BYTES, GOSSIP_INTERVAL_MS, GossipOutcome, HeadGossip, check_gossip, should_gossip,
};
pub use head::{
    Evidence, EvidenceKind, FreshnessPolicy, HEAD_BYTES, KnownHeads, MAX_EPOCH, Relation,
    SignedHead, Succession, VerifiedHead, check_freshness, check_successor, compare_heads,
    verify_head,
};
pub use log::{
    Ed25519HeadSigner, HeadSigner, LogDescriptor, LogId, LogSigningKey, SignatureScheme,
    SignerError, Suite,
};
pub use messages::{
    AuditResponse, HistoryResponse, LookupResponse, MAX_AUDIT_PROOF, MAX_AUDIT_SPAN,
    MAX_HISTORY_PROOF, MAX_LOOKUP_PROOF,
};
pub use server::{Directory, MAX_UPDATES_PER_EPOCH};
pub use store::{KtStore, MemoryStore, StoreError};
pub use tofu::{Assessment, ContactTrust, ConversationKind, Observation, RETRY_WITHIN_MS, Surface};
pub use types::{
    AccountId, DirectoryValue, Label, MAX_NAME_BYTES, MAX_VALUE_DATA, MAX_VALUE_ENCODED,
};
pub use values::{
    AccountBinding, FORMAT_ACCOUNT_BINDING_V1, FORMAT_NAME_BINDING_V1, MAX_ROSTER_ENTRIES,
    NameBinding, RosterCommitment, ValueError, roster_for_binding,
};
pub use verify::{
    MonitorCheckpoint, MonitorReport, VerifiedEntry, VerifiedHistory, monitor_own, verify_audit,
    verify_history, verify_lookup,
};

/// The format version of every message in this crate.
pub const FORMAT_VERSION: u8 = 1;
const MSG_DESCRIPTOR: u8 = 1;
const MSG_HEAD: u8 = 2;
const MSG_LOOKUP: u8 = 3;
const MSG_HISTORY: u8 = 4;
const MSG_AUDIT: u8 = 5;
const MSG_GOSSIP: u8 = 6;
const MSG_EVIDENCE: u8 = 7;

#[doc(hidden)]
pub mod __private {
    //! Test and fuzzing hooks; not part of the interface.
    pub use crate::akd_backend::codec::MAX_HISTORY_VERSIONS;

    /// Drops the newest version from a suite-1 history proof, as a server hiding an
    /// update would.
    pub fn drop_newest_history_version(proof: &[u8]) -> Option<Vec<u8>> {
        let mut p = crate::akd_backend::codec::decode_history(proof).ok()?;
        if p.update_proofs.len() < 2 {
            return None;
        }
        p.update_proofs.remove(0);
        crate::akd_backend::codec::encode_history(&p).ok()
    }
}
