// SPDX-License-Identifier: EUPL-1.2
//! What the worker remembers besides the keystore: per server, the pinned
//! key-transparency log, the heads it accepted, its own binding and
//! self-monitoring checkpoint, and trust on first use per contact; per identity,
//! where it was verified out of band. Sealed under the store key as JSON.
use nuschel_ipc::crypto::{ContactState, RecoveryPolicy, base64_bytes, base64_list};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Values of our own binding kept as authorised for self-monitoring.
pub const MAX_PUBLISHED: usize = 16;
/// Evidence of misbehaviour kept per server.
pub const MAX_EVIDENCE: usize = 16;
/// Contacts per server.
pub const MAX_CONTACTS: usize = 10_000;
pub const MAX_SERVERS: usize = 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub version: u32,
    pub device_name: String,
    pub recovery: RecoveryPolicy,
    pub servers: BTreeMap<String, ServerState>,
    /// Identities compared out of band, by fingerprint (hex): verification belongs to
    /// the identity and holds on every server (docs/e2ee.md §4).
    pub verified: BTreeMap<String, Verified>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verified {
    /// The server (sticky key) and its label where the user compared.
    pub server: String,
    pub label: String,
    pub at_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub version: u64,
    pub epoch: u64,
    #[serde(with = "base64_bytes")]
    pub value: Vec<u8>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerState {
    pub label: String,
    /// The pinned log descriptor.
    #[serde(with = "base64_bytes")]
    pub descriptor: Vec<u8>,
    /// Accepted heads (`KnownHeads`), oldest first.
    #[serde(with = "base64_list")]
    pub heads: Vec<Vec<u8>>,
    /// A newer head waiting for its audit proof.
    #[serde(with = "base64_bytes")]
    pub target: Vec<u8>,
    /// Our account there (16 bytes), empty as a guest.
    #[serde(with = "base64_bytes")]
    pub account: Vec<u8>,
    /// Our statement for that account.
    #[serde(with = "base64_bytes")]
    pub statement: Vec<u8>,
    /// Binding values we made, oldest first.
    #[serde(with = "base64_list")]
    pub published: Vec<Vec<u8>>,
    pub checkpoint: Option<Checkpoint>,
    pub last_monitor_ms: u64,
    /// The blocking own-key alarm, until resolved.
    pub alarm: Option<String>,
    /// The server contradicted itself (equivocation, a failed audit, a new log).
    pub misbehaviour: Option<String>,
    #[serde(with = "base64_list")]
    pub evidence: Vec<Vec<u8>>,
    /// Contacts by account id (hex).
    pub contacts: BTreeMap<String, Contact>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contact {
    /// The pinned identity (hex fingerprint).
    pub pinned: Option<String>,
    pub checked_version: u64,
    /// A new identity of a verified contact, until the user accepts it.
    pub pending_change: Option<String>,
    pub failures: u32,
    pub first_failure_ms: Option<u64>,
    pub direct: bool,
    pub last_history_ms: u64,
    pub state: ContactState,
    pub notice: Option<String>,
}

impl Default for Contact {
    fn default() -> Self {
        Self {
            pinned: None,
            checked_version: 0,
            pending_change: None,
            failures: 0,
            first_failure_ms: None,
            direct: false,
            last_history_ms: 0,
            state: ContactState::Unknown,
            notice: None,
        }
    }
}
