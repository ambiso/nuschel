// SPDX-License-Identifier: EUPL-1.2
//! What the worker remembers besides the keystore: per server, the pinned
//! key-transparency log, the heads it accepted, its own binding and
//! self-monitoring checkpoint, and trust on first use per contact; per identity,
//! where it was verified out of band. Sealed under the store key as JSON.
use nuschel_ipc::crypto::{
    ContactState, KeyTransparencySettings, KtRecordBytes, RecoveryPolicy, base64_bytes, base64_list,
};
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
    /// The identity's other devices (device certificates), from linking and verified
    /// lookups of our own account; published with this one (crate::link).
    #[serde(default, with = "base64_list")]
    pub own_devices: Vec<Vec<u8>>,
    /// The witnesses the user requires (settings; docs/e2ee.md §5.9), as the
    /// session last passed them.
    #[serde(default)]
    pub witnessing: KeyTransparencySettings,
}

/// Whether a server's view is witnessed (crate::witness).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Witness {
    /// The server offers `kt-witness/1`.
    pub offered: bool,
    /// The newest epoch a fresh checkpoint covered, and until when that holds.
    pub covered_epoch: Option<u64>,
    pub until_ms: u64,
    /// Since when the newest head waits for cosignatures.
    pub pending_since_ms: Option<u64>,
    /// Why the last check found no fresh cosignatures, if it did not.
    pub failure: Option<String>,
}

/// A self-monitored `reg:` or `name:` label: the first version that named our
/// account, from which on every version must.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelCheckpoint {
    pub version: u64,
    pub epoch: u64,
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
    /// Registration numbers and the account (hex) the server showed for them.
    /// Registration numbers are never reused on Nuschel servers, so another account
    /// behind one is worth a notice.
    #[serde(default)]
    pub aliases: BTreeMap<u32, String>,
    #[serde(default)]
    pub witness: Witness,
    /// Our registration number and name there, and their monitoring checkpoints
    /// (by label: `reg` or `name:<name>`).
    #[serde(default)]
    pub registration: Option<(u32, String)>,
    #[serde(default)]
    pub labels: BTreeMap<String, LabelCheckpoint>,
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
    /// The record behind the pinned identity's binding, as last verified against
    /// key transparency (the root key and the certificates): the device roster
    /// MLS credentials are checked against (docs/e2ee.md §3.3, §5.4).
    #[serde(default, with = "base64_bytes")]
    pub roster_root: Vec<u8>,
    #[serde(default)]
    pub roster: Option<KtRecordBytes>,
    /// When it was verified (milliseconds).
    #[serde(default)]
    pub roster_ms: u64,
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
            roster_root: Vec::new(),
            roster: None,
            roster_ms: 0,
        }
    }
}
