// SPDX-License-Identifier: EUPL-1.2
//! Between the session and the crypto worker (`nuschel-crypto-worker`, docs/e2ee.md
//! §8.1), which owns every identity secret: the identity root while it is being set
//! up, the device-signing key, the device keys, the encrypted keystore, the
//! key-transparency state (pinned logs, known heads, self-monitoring) and trust on
//! first use per contact. The session moves bytes between the server and the worker
//! and never parses them; the worker has no network.
//!
//! Requests and responses share a `request_id`; the worker answers every request
//! once, in order. Binary fields are base64 strings. Every string and list has a
//! bound the worker checks ([`CryptoRequest::check`]); frames are bounded by
//! [`crate::MAX_FRAME_BYTES`] before they are parsed.
use serde::{Deserialize, Serialize};

/// Bytes as a base64 string.
pub mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(serde::de::Error::custom)
    }
}

/// Several byte strings as base64.
pub mod base64_list {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer, ser::SerializeSeq};

    pub fn serialize<S: Serializer>(list: &[Vec<u8>], serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(list.len()))?;
        for bytes in list {
            seq.serialize_element(&base64::engine::general_purpose::STANDARD.encode(bytes))?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Vec<u8>>, D::Error> {
        Vec::<String>::deserialize(deserializer)?
            .into_iter()
            .map(|text| {
                base64::engine::general_purpose::STANDARD
                    .decode(text)
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

/// Bounds the worker enforces on what the session sends.
pub const MAX_SERVER_KEY: usize = 512;
pub const MAX_LABEL: usize = 512;
pub const MAX_DEVICE_NAME: usize = 64;
pub const MAX_REASON: usize = 512;
/// A key-transparency reply as the server may send it (docs/extension.md).
pub const MAX_KT_BYTES: usize = 6 * 1024 * 1024;
pub const MAX_CERTIFICATE: usize = 8 * 1024;
pub const MAX_RECORD_ENTRIES: usize = 32;
/// MLS (docs/e2ee.md §6): any one MLS message, Welcome, GroupInfo or tree.
pub const MAX_MLS_BYTES: usize = 2 * 1024 * 1024;
/// Key packages in one request (claims for two accounts of 16 devices each).
pub const MAX_MLS_PACKAGES: usize = 32;
/// A key package.
pub const MAX_MLS_KEY_PACKAGE: usize = 4096;
/// Chat text, in bytes of UTF-8.
pub const MAX_MLS_TEXT: usize = 64 * 1024;

/// The server a request is about: its sticky key (`host:port`, as the session's
/// sticky record names it) and how the user knows it, for "verified (same
/// identity as on …)".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CryptoServer {
    pub key: String,
    pub label: String,
}

/// Whether a recovery key is offered at creation (docs/e2ee.md §10, question 1;
/// the user has not decided yet, so it is a setting).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPolicy {
    /// Created with the identity and shown to the user, who may skip it (then the
    /// identity root is deleted).
    #[default]
    Offer,
    /// The same, but skipping is refused.
    Require,
    /// No recovery key: the identity root exists only during creation.
    Never,
}

/// The certificates behind a binding (`KtRecord` on the wire).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KtRecordBytes {
    #[serde(with = "base64_bytes")]
    pub device_signing_certificate: Vec<u8>,
    #[serde(with = "base64_list")]
    pub devices: Vec<Vec<u8>>,
    #[serde(with = "base64_list")]
    pub revocations: Vec<Vec<u8>>,
    #[serde(with = "base64_bytes")]
    pub statement: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CryptoRequest {
    /// The identity, if any: `Identity`.
    Identity,
    /// Creates the identity unless there is one: `Identity`.
    CreateIdentity {
        device_name: String,
        recovery: RecoveryPolicy,
    },
    /// The recovery key made at creation, while the user has not saved or skipped
    /// it: `RecoveryKey`.
    ShowRecoveryKey,
    /// The user wrote the recovery key down: forget it here (`Identity`).
    RecoveryKeySaved,
    /// The user declined a recovery key: forget it and the identity root
    /// (`Identity`). Refused under [`RecoveryPolicy::Require`].
    SkipRecoveryKey,
    /// Connected to a `kt/1` server: pin its log (or check the pin), and check its
    /// newest head. `account` is ours there (empty for guests): `Head`.
    Connect {
        server: CryptoServer,
        #[serde(with = "base64_bytes")]
        descriptor: Vec<u8>,
        #[serde(with = "base64_bytes")]
        account: Vec<u8>,
        #[serde(with = "base64_bytes")]
        head: Vec<u8>,
    },
    /// A newer head (the target of `NeedAudit`, or one in between) with the audit
    /// proof from the latest accepted head: `Head`, or `NeedAudit` for the rest.
    Advance {
        server: String,
        #[serde(with = "base64_bytes")]
        head: Vec<u8>,
        #[serde(with = "base64_bytes")]
        audit: Vec<u8>,
    },
    /// What to publish for our account on this server (creating the identity on
    /// first need): `Bundle`.
    PublishBundle {
        server: String,
        device_name: String,
        recovery: RecoveryPolicy,
    },
    /// The server accepted our publication (`KtPublished`): `Done`.
    Published {
        server: String,
        #[serde(with = "base64_bytes")]
        value: Vec<u8>,
        current: bool,
    },
    /// Self-monitoring: the history of our own account: `Monitor`.
    Monitor {
        server: String,
        #[serde(with = "base64_bytes")]
        history: Vec<u8>,
    },
    /// A contact's lookup and the record behind it: `Contact`.
    Contact {
        server: String,
        #[serde(with = "base64_bytes")]
        account: Vec<u8>,
        #[serde(with = "base64_bytes")]
        lookup: Vec<u8>,
        record: Option<KtRecordBytes>,
        /// We talk to them in a direct conversation.
        direct: bool,
        /// The registration number the lookup named, if it named one: a server that
        /// later shows another account for it is flagged.
        #[serde(default)]
        user_id: Option<u32>,
    },
    /// A contact's history: `Contact`.
    ContactHistory {
        server: String,
        #[serde(with = "base64_bytes")]
        account: Vec<u8>,
        #[serde(with = "base64_bytes")]
        history: Vec<u8>,
    },
    /// The server had nothing (not found, unauthenticated) or refused: `Contact`.
    ContactUnavailable {
        server: String,
        #[serde(with = "base64_bytes")]
        account: Vec<u8>,
        not_found: bool,
        reason: String,
    },
    /// What is known about a contact, without asking the server: `Contact`.
    ContactStatus {
        server: String,
        #[serde(with = "base64_bytes")]
        account: Vec<u8>,
    },
    /// The user compared safety numbers out of band: `Contact`.
    MarkVerified {
        server: String,
        #[serde(with = "base64_bytes")]
        account: Vec<u8>,
    },
    UnmarkVerified {
        server: String,
        #[serde(with = "base64_bytes")]
        account: Vec<u8>,
    },
    /// The user accepted a contact's new identity: `Contact`.
    AcceptKeyChange {
        server: String,
        #[serde(with = "base64_bytes")]
        account: Vec<u8>,
    },
    /// What the schedule of docs/e2ee.md §5.4 asks for now: `Due`.
    Due { server: String, connected: bool },
    /// The newest head of this server, to gossip in a conversation once a day:
    /// `Gossip`.
    MakeGossip {
        server: String,
        last_gossip_ms: Option<u64>,
        newest_seen_epoch: Option<u64>,
    },
    /// A head another member gossiped: `GossipChecked`.
    CheckGossip {
        server: String,
        #[serde(with = "base64_bytes")]
        gossip: Vec<u8>,
    },
    /// The user saw an own-key or misbehaviour alarm of this server (`Identity`).
    AcknowledgeAlarm { server: String },
    /// The user reset this server's key-transparency pin (after a log change).
    ForgetServer { server: String },

    // ---- MLS (`mls/1`, docs/e2ee.md §6, §7) ----
    // Conversations are named by the server's sticky key and the 16-byte
    // conversation id. The worker keeps each conversation's group, pin (§7.2),
    // cursor (the last processed seq) and per-sender chains (§6.4).
    /// Key packages for this device to upload to a `mls/1` server: `one_time`
    /// fresh ones (at most 16) and, with `last_resort`, a last-resort one
    /// (`MlsKeyPackages`).
    MlsKeyPackages {
        server: String,
        one_time: u32,
        last_resort: bool,
    },
    /// What the worker knows about a conversation: `MlsConversation`.
    MlsStatus {
        server: String,
        #[serde(with = "base64_bytes")]
        conversation: Vec<u8>,
    },
    /// Create a direct conversation's group with the account `peer`.
    /// `key_packages` were claimed from the server for the peer's account and for
    /// our own (our other devices; this device's own are skipped). Every
    /// credential must verify against the key-transparency roster the worker
    /// keeps for the peer and for us (look them up first: `Contact`). The commit
    /// is pending until `MlsCommitDone` (`MlsOutgoing`).
    MlsCreateDirect {
        server: String,
        #[serde(with = "base64_bytes")]
        conversation: Vec<u8>,
        #[serde(with = "base64_bytes")]
        peer: Vec<u8>,
        #[serde(with = "base64_list")]
        key_packages: Vec<Vec<u8>>,
    },
    /// Add devices (a contact's new device, or our own) to the conversation's
    /// group, with key packages claimed for them (`MlsOutgoing`).
    MlsAddDevices {
        server: String,
        #[serde(with = "base64_bytes")]
        conversation: Vec<u8>,
        #[serde(with = "base64_list")]
        key_packages: Vec<Vec<u8>>,
    },
    /// The server's answer to our pending commit: accepted as `seq`, or refused
    /// (stale epoch or invalid); a refused creation forgets the group
    /// (`MlsConversation`).
    MlsCommitDone {
        server: String,
        #[serde(with = "base64_bytes")]
        conversation: Vec<u8>,
        accepted: bool,
        seq: u64,
    },
    /// A Welcome from our mailbox, with the server's ratchet tree for it and the
    /// seq of the commit that added us. `peer` is the other account of the direct
    /// conversation. Joined after the binding, the pin and every member's
    /// credential check out (`MlsConversation`), else `Rejected`.
    MlsWelcome {
        server: String,
        #[serde(with = "base64_bytes")]
        conversation: Vec<u8>,
        #[serde(with = "base64_bytes")]
        peer: Vec<u8>,
        #[serde(with = "base64_bytes")]
        welcome: Vec<u8>,
        #[serde(with = "base64_bytes")]
        tree: Vec<u8>,
        seq: u64,
    },
    /// One message of the conversation's stream (`ChatEvent`, `HistoryPage`), in
    /// seq order (`MlsReceived`).
    MlsReceive {
        server: String,
        #[serde(with = "base64_bytes")]
        conversation: Vec<u8>,
        seq: u64,
        #[serde(with = "base64_bytes")]
        message: Vec<u8>,
    },
    /// Encrypt chat text for the conversation's current epoch
    /// (`MlsCiphertext`). The same idempotency key again (after `STALE_EPOCH`)
    /// encrypts the same envelope (counter, chain) for the new epoch. Refused
    /// while sending is blocked (§7.4).
    MlsEncrypt {
        server: String,
        #[serde(with = "base64_bytes")]
        conversation: Vec<u8>,
        #[serde(with = "base64_bytes")]
        idempotency_key: Vec<u8>,
        text: String,
    },
    /// The server stored our application message as `seq` (`Done`).
    MlsSent {
        server: String,
        #[serde(with = "base64_bytes")]
        conversation: Vec<u8>,
        #[serde(with = "base64_bytes")]
        idempotency_key: Vec<u8>,
        seq: u64,
    },

    // ---- Device linking (docs/e2ee.md §3.4) ----
    // nuschel-identity's protocol; the caller relays the messages between the two
    // devices (out of band for now; the server's linking mailbox comes later). The
    // state between steps lives in the worker's memory only.
    /// On a device with the device-signing key: a fresh code to show and the first
    /// SPAKE2 message (`LinkOffer`).
    LinkOffer,
    /// On the new device, which has no identity yet: the code the user typed and the
    /// existing device's message; makes this device's keys (`LinkJoin`).
    LinkJoin {
        code: String,
        #[serde(with = "base64_bytes")]
        pake: Vec<u8>,
        device_name: String,
    },
    /// On the existing device: the new device's messages. Checks the key
    /// confirmation, certifies the new device and seals its provisioning bundle
    /// (`LinkBundle`).
    LinkApprove {
        #[serde(with = "base64_bytes")]
        pake: Vec<u8>,
        #[serde(with = "base64_bytes")]
        device_keys: Vec<u8>,
    },
    /// On the new device: the bundle; stores the identity (`Identity`).
    LinkComplete {
        #[serde(with = "base64_bytes")]
        bundle: Vec<u8>,
    },
}

/// A link code (`<nameplate>-dddd-dddd`), a SPAKE2 message, the new device's keys
/// message, and a sealed provisioning bundle.
pub const MAX_LINK_CODE: usize = 32;
pub const MAX_LINK_PAKE: usize = 64;
pub const MAX_LINK_DEVICE_KEYS: usize = 2048;
pub const MAX_LINK_BUNDLE: usize = 2 * 1024 * 1024;

impl CryptoRequest {
    /// The bounds of every field; `Err` names the first violation.
    pub fn check(&self) -> Result<(), &'static str> {
        fn at_most(len: usize, max: usize, what: &'static str) -> Result<(), &'static str> {
            if len > max { Err(what) } else { Ok(()) }
        }
        let server = |key: &str| at_most(key.len(), MAX_SERVER_KEY, "server key");
        let account = |bytes: &[u8]| {
            if bytes.len() == 16 {
                Ok(())
            } else {
                Err("account id")
            }
        };
        let conversation = |bytes: &[u8]| {
            if bytes.len() == 16 {
                Ok(())
            } else {
                Err("conversation id")
            }
        };
        let key = |bytes: &[u8]| {
            if bytes.len() == 16 {
                Ok(())
            } else {
                Err("idempotency key")
            }
        };
        let packages = |list: &[Vec<u8>]| {
            at_most(list.len(), MAX_MLS_PACKAGES, "key packages")?;
            list.iter()
                .try_for_each(|bytes| at_most(bytes.len(), MAX_MLS_KEY_PACKAGE, "key package"))
        };
        match self {
            Self::Identity
            | Self::ShowRecoveryKey
            | Self::RecoveryKeySaved
            | Self::SkipRecoveryKey => Ok(()),
            Self::CreateIdentity { device_name, .. } => {
                at_most(device_name.len(), MAX_DEVICE_NAME, "device name")
            }
            Self::Connect {
                server: s,
                descriptor,
                account: own,
                head,
            } => {
                server(&s.key)?;
                at_most(s.label.len(), MAX_LABEL, "server label")?;
                at_most(descriptor.len(), 512, "descriptor")?;
                if !own.is_empty() {
                    account(own)?;
                }
                at_most(head.len(), 1024, "head")
            }
            Self::Advance {
                server: s,
                head,
                audit,
            } => {
                server(s)?;
                at_most(head.len(), 1024, "head")?;
                at_most(audit.len(), MAX_KT_BYTES, "audit")
            }
            Self::PublishBundle {
                server: s,
                device_name,
                ..
            } => {
                server(s)?;
                at_most(device_name.len(), MAX_DEVICE_NAME, "device name")
            }
            Self::Published {
                server: s, value, ..
            } => {
                server(s)?;
                at_most(value.len(), 8192, "value")
            }
            Self::Monitor { server: s, history } => {
                server(s)?;
                at_most(history.len(), MAX_KT_BYTES, "history")
            }
            Self::Contact {
                server: s,
                account: a,
                lookup,
                record,
                ..
            } => {
                server(s)?;
                account(a)?;
                at_most(lookup.len(), MAX_KT_BYTES, "lookup")?;
                if let Some(record) = record {
                    at_most(
                        record.devices.len() + record.revocations.len(),
                        MAX_RECORD_ENTRIES * 2,
                        "record entries",
                    )?;
                    for bytes in record
                        .devices
                        .iter()
                        .chain(&record.revocations)
                        .chain([&record.device_signing_certificate, &record.statement])
                    {
                        at_most(bytes.len(), MAX_CERTIFICATE, "certificate")?;
                    }
                }
                Ok(())
            }
            Self::ContactHistory {
                server: s,
                account: a,
                history,
            } => {
                server(s)?;
                account(a)?;
                at_most(history.len(), MAX_KT_BYTES, "history")
            }
            Self::ContactUnavailable {
                server: s,
                account: a,
                reason,
                ..
            } => {
                server(s)?;
                account(a)?;
                at_most(reason.len(), MAX_REASON, "reason")
            }
            Self::ContactStatus {
                server: s,
                account: a,
            }
            | Self::MarkVerified {
                server: s,
                account: a,
            }
            | Self::UnmarkVerified {
                server: s,
                account: a,
            }
            | Self::AcceptKeyChange {
                server: s,
                account: a,
            } => {
                server(s)?;
                account(a)
            }
            Self::Due { server: s, .. }
            | Self::MakeGossip { server: s, .. }
            | Self::AcknowledgeAlarm { server: s }
            | Self::ForgetServer { server: s } => server(s),
            Self::CheckGossip { server: s, gossip } => {
                server(s)?;
                at_most(gossip.len(), 1024, "gossip")
            }
            Self::MlsKeyPackages {
                server: s,
                one_time,
                ..
            } => {
                server(s)?;
                at_most(*one_time as usize, 16, "key packages")
            }
            Self::MlsStatus {
                server: s,
                conversation: c,
            } => {
                server(s)?;
                conversation(c)
            }
            Self::MlsCreateDirect {
                server: s,
                conversation: c,
                peer,
                key_packages,
            } => {
                server(s)?;
                conversation(c)?;
                account(peer)?;
                packages(key_packages)
            }
            Self::MlsAddDevices {
                server: s,
                conversation: c,
                key_packages,
            } => {
                server(s)?;
                conversation(c)?;
                packages(key_packages)
            }
            Self::MlsCommitDone {
                server: s,
                conversation: c,
                ..
            } => {
                server(s)?;
                conversation(c)
            }
            Self::MlsWelcome {
                server: s,
                conversation: c,
                peer,
                welcome,
                tree,
                ..
            } => {
                server(s)?;
                conversation(c)?;
                account(peer)?;
                at_most(welcome.len(), MAX_MLS_BYTES, "welcome")?;
                at_most(tree.len(), MAX_MLS_BYTES, "tree")
            }
            Self::MlsReceive {
                server: s,
                conversation: c,
                message,
                ..
            } => {
                server(s)?;
                conversation(c)?;
                at_most(message.len(), MAX_MLS_BYTES, "message")
            }
            Self::MlsEncrypt {
                server: s,
                conversation: c,
                idempotency_key,
                text,
            } => {
                server(s)?;
                conversation(c)?;
                key(idempotency_key)?;
                at_most(text.len(), MAX_MLS_TEXT, "text")
            }
            Self::MlsSent {
                server: s,
                conversation: c,
                idempotency_key,
                ..
            } => {
                server(s)?;
                conversation(c)?;
                key(idempotency_key)
            }
            Self::LinkOffer => Ok(()),
            Self::LinkJoin {
                code,
                pake,
                device_name,
            } => {
                at_most(code.len(), MAX_LINK_CODE, "link code")?;
                at_most(pake.len(), MAX_LINK_PAKE, "link message")?;
                at_most(device_name.len(), MAX_DEVICE_NAME, "device name")
            }
            Self::LinkApprove { pake, device_keys } => {
                at_most(pake.len(), MAX_LINK_PAKE, "link message")?;
                at_most(device_keys.len(), MAX_LINK_DEVICE_KEYS, "device keys")
            }
            Self::LinkComplete { bundle } => at_most(bundle.len(), MAX_LINK_BUNDLE, "bundle"),
        }
    }
}

/// Our own identity, as the UI shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityView {
    /// The identity fingerprint, 64 hex digits; `None` before creation.
    pub fingerprint: Option<String>,
    pub device_name: String,
    /// Created by the request this answers.
    pub created: bool,
    /// A recovery key is waiting to be saved or skipped.
    pub recovery_pending: bool,
    /// A recovery blob exists (the root can be restored with the recovery key).
    pub recoverable: bool,
    /// This device holds the device-signing key (it can link others).
    pub can_link: bool,
    /// Unresolved self-monitoring alarms: server label and what happened.
    pub alarms: Vec<OwnKeyAlarm>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnKeyAlarm {
    pub server: String,
    pub label: String,
    pub reason: String,
}

/// How a head check ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum HeadOutcome {
    /// The head is the latest accepted one now.
    Accepted { epoch: u64 },
    /// Fetch `KtAudit { from, to }` (bisecting with `KtHead` if it is too large) and
    /// send it with `Advance`.
    NeedAudit { from: u64, to: u64 },
    /// Verification did not succeed (stale head, bad signature, …): retried later;
    /// not proof of misbehaviour.
    Failed { reason: String },
    /// The server contradicted itself (equivocation, a failed audit, a changed
    /// log): an alarm, with the evidence kept.
    Misbehaviour { reason: String },
}

/// What [`CryptoRequest::PublishBundle`] gives the session to send as `KtPublish`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishBundle {
    #[serde(with = "base64_bytes")]
    pub root: Vec<u8>,
    pub record: KtRecordBytes,
    /// The identity was created for this.
    pub created: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum MonitorOutcome {
    /// Every version of our binding is one we made; `current` once the newest one
    /// is published.
    Consistent {
        version: u64,
        current: bool,
    },
    /// Someone else's key was bound to our account: the blocking "This wasn't me"
    /// alarm (docs/e2ee.md §4).
    OwnKeyAlarm {
        reason: String,
    },
    Failed {
        reason: String,
    },
    Misbehaviour {
        reason: String,
    },
}

/// What the UI shows for a contact (docs/e2ee.md §4, §5.5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContactState {
    /// Not checked yet.
    Unknown,
    /// The server has no identity for this account (unauthenticated: never shown
    /// as "has no key", only as "not available").
    NoIdentity,
    /// Pinned on first use; KT consistent; not compared out of band.
    Unverified,
    /// Compared out of band, here or (`elsewhere`) on another server where the
    /// key-transparency log binds the same identity.
    Verified { elsewhere: Option<String> },
    /// The account is now bound to another identity. `blocking`: it was verified,
    /// and the old pin stays until the user accepts (direct conversations block
    /// sending).
    KeyChanged { blocking: bool },
    /// "Automatic verification unavailable": checks failed repeatedly. A manual
    /// verification still holds (`verified`).
    Unavailable { reason: String, verified: bool },
    /// The server contradicted itself about this contact or its log.
    Misbehaviour { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContactView {
    pub server: String,
    #[serde(with = "base64_bytes")]
    pub account: Vec<u8>,
    pub state: ContactState,
    /// The pinned identity fingerprint (64 hex digits).
    pub identity: Option<String>,
    /// 60 digits in 12 groups of 5, ours and theirs (docs/e2ee.md §4).
    pub safety_number: Option<String>,
    /// `NSQ1 || fp_low || fp_high` for a QR code.
    #[serde(with = "base64_bytes")]
    pub qr: Vec<u8>,
    /// Something happened that the conversation should show once: a key change,
    /// a binding that came and went.
    pub notice: Option<String>,
    /// We talk to them in a direct conversation.
    pub direct: bool,
}

/// What the §5.4 schedule asks for on one server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Due {
    /// Publish our binding (none yet, the roster changed, or a renewed certificate).
    pub publish: bool,
    /// Self-monitoring (on connect and every 24 h).
    pub monitor: bool,
    /// Histories of direct and verified contacts (each connect, every 24 h).
    #[serde(with = "base64_list")]
    pub histories: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum CryptoResponse {
    Identity(IdentityView),
    /// Shown once; 14 groups of 4 characters.
    RecoveryKey(String),
    Head(HeadOutcome),
    /// The request's head is newer than the latest accepted one: audit first
    /// (as for [`HeadOutcome::NeedAudit`]), then repeat the request.
    NeedAudit {
        from: u64,
        to: u64,
    },
    Bundle(PublishBundle),
    Monitor(MonitorOutcome),
    Contact(ContactView),
    Due(Due),
    /// Empty when nothing is due.
    Gossip(#[serde(with = "base64_bytes")] Vec<u8>),
    /// "consistent", "newer" (audit it), or the alarm.
    GossipChecked(String),
    Done,
    Rejected(String),
    MlsKeyPackages(KeyPackageBatch),
    MlsConversation(ConversationView),
    MlsOutgoing(MlsOutgoing),
    MlsReceived(Received),
    MlsCiphertext {
        #[serde(with = "base64_bytes")]
        message: Vec<u8>,
        epoch: u64,
    },
    /// Show `code` to the user; relay `pake` to the new device.
    LinkOffer {
        code: String,
        #[serde(with = "base64_bytes")]
        pake: Vec<u8>,
    },
    /// Relay both to the existing device.
    LinkJoin {
        #[serde(with = "base64_bytes")]
        pake: Vec<u8>,
        #[serde(with = "base64_bytes")]
        device_keys: Vec<u8>,
    },
    /// Relay to the new device.
    LinkBundle(#[serde(with = "base64_bytes")] Vec<u8>),
}

// ---- MLS views (docs/e2ee.md §6, §7.4) ----

/// Key packages to upload (`MlsKeyPackageUpload`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyPackageBatch {
    /// This device's id (16 bytes): its Welcome mailbox on the server.
    #[serde(with = "base64_bytes")]
    pub device: Vec<u8>,
    #[serde(with = "base64_list")]
    pub one_time: Vec<Vec<u8>>,
    /// Empty when none was asked for.
    #[serde(with = "base64_bytes")]
    pub last_resort: Vec<u8>,
}

/// The lock in the conversation header (docs/e2ee.md §7.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Lock {
    /// Grey: no group yet, negotiating, or key transparency pending for a new
    /// device.
    Pending { reason: String },
    /// Red: no end-to-end encryption (no `mls/1`, or a conversation that never
    /// had a group).
    Open,
    /// Green: the group is established, every member's credential verified
    /// against key transparency.
    Encrypted,
    /// Green with a check mark: the same, and the contact verified out of band.
    Verified,
    /// Amber: encrypted, but something needs attention (an unverified device, a
    /// key change, a gap or fork, a refused commit).
    Attention { reason: String },
}

/// A member device, as the conversation shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberView {
    /// The identity fingerprint (64 hex digits).
    pub identity: String,
    /// The device id (32 hex digits).
    pub device: String,
    pub device_name: String,
    /// One of our own devices (this one included).
    pub own: bool,
    /// Its credential verified against the identity's key-transparency roster.
    pub verified: bool,
}

/// A conversation's end-to-end state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationView {
    pub server: String,
    #[serde(with = "base64_bytes")]
    pub conversation: Vec<u8>,
    /// The conversation is pinned to an MLS group (§7.2): plaintext for it is
    /// refused from now on.
    pub pinned: bool,
    /// We are a member (not removed).
    pub active: bool,
    pub epoch: u64,
    /// The last processed seq.
    pub cursor: u64,
    pub lock: Lock,
    pub members: Vec<MemberView>,
    /// Why sending is blocked, if it is.
    pub blocked: Option<String>,
    /// Things to show once in the conversation (a device added, a gap, a fork).
    pub notices: Vec<String>,
    /// A commit of ours waits for the server's answer.
    pub pending_commit: bool,
    /// Accounts (16 bytes: the peer, or our own) whose key-transparency roster
    /// lists devices that are not members yet: claim key packages for them and
    /// send `MlsAddDevices`.
    #[serde(default, with = "base64_list")]
    pub missing: Vec<Vec<u8>>,
}

/// A commit to send as `MlsCommit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MlsOutgoing {
    #[serde(with = "base64_bytes")]
    pub conversation: Vec<u8>,
    #[serde(with = "base64_bytes")]
    pub idempotency_key: Vec<u8>,
    #[serde(with = "base64_bytes")]
    pub commit: Vec<u8>,
    #[serde(with = "base64_bytes")]
    pub group_info: Vec<u8>,
    /// Empty when the commit adds nobody.
    #[serde(with = "base64_bytes")]
    pub welcome: Vec<u8>,
    /// A new group's epoch-0 GroupInfo and tree (`MlsGroupInit`).
    pub create: Option<GroupInit>,
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupInit {
    #[serde(with = "base64_bytes")]
    pub group_info: Vec<u8>,
    #[serde(with = "base64_bytes")]
    pub tree: Vec<u8>,
}

/// Where a message stands in its sender's chain (docs/e2ee.md §6.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "order", rename_all = "snake_case", deny_unknown_fields)]
pub enum Order {
    InOrder,
    /// Messages from that device are missing.
    Gap {
        missing: u64,
    },
    /// The server showed different histories: an alarm, both versions kept.
    Fork,
}

/// What one stream message was.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Received {
    /// A chat message from another device.
    Message {
        seq: u64,
        sender: MemberView,
        text: String,
        /// The sender's clock, Unix milliseconds.
        sent_at: u64,
        #[serde(with = "base64_bytes")]
        idempotency_key: Vec<u8>,
        order: Order,
    },
    /// Our own message (sent by this device) coming back.
    Own {
        seq: u64,
        text: String,
        #[serde(with = "base64_bytes")]
        idempotency_key: Vec<u8>,
    },
    /// A commit was applied; the view shows the group now.
    Commit { seq: u64, view: ConversationView },
    /// Nothing to show (a typing indicator, a proposal, an envelope kind this
    /// version does not show).
    Nothing { seq: u64 },
    /// Already processed (at or before the cursor).
    Duplicate { seq: u64 },
    /// Could not be decrypted: shown as an explicit gap, never silently.
    Undecryptable { seq: u64, reason: String },
    /// A commit the worker refused (a stranger, a credential that does not
    /// verify, a changed binding): the conversation is blocked.
    Refused {
        seq: u64,
        reason: String,
        view: ConversationView,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Envelope, read_frame, write_frame};

    #[test]
    fn requests_round_trip_with_base64_bytes() {
        let request = CryptoRequest::Contact {
            server: "voice.example.org:64738".into(),
            account: vec![7; 16],
            lookup: vec![1, 2, 3],
            record: Some(KtRecordBytes {
                device_signing_certificate: vec![9; 4],
                devices: vec![vec![1], vec![2, 3]],
                revocations: Vec::new(),
                statement: vec![4],
            }),
            direct: true,
            user_id: Some(3),
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &Envelope::new(3, &request)).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("\"AQID\""));
        let back: Envelope<CryptoRequest> = read_frame(&mut bytes.as_slice()).unwrap().unwrap();
        assert!(matches!(
            back.payload,
            CryptoRequest::Contact { direct: true, .. }
        ));
        assert!(back.payload.check().is_ok());
    }

    #[test]
    fn bounds_are_checked() {
        let long = CryptoRequest::Due {
            server: "x".repeat(MAX_SERVER_KEY + 1),
            connected: true,
        };
        assert!(long.check().is_err());
        let short_account = CryptoRequest::ContactStatus {
            server: "s".into(),
            account: vec![1; 15],
        };
        assert!(short_account.check().is_err());
        let guest = CryptoRequest::Connect {
            server: CryptoServer {
                key: "s".into(),
                label: "S".into(),
            },
            descriptor: vec![0; 71],
            account: Vec::new(),
            head: vec![0; 148],
        };
        assert!(guest.check().is_ok());
        // Unknown fields and malformed base64 are refused.
        let bad = br#"{"kind":"contact_status","server":"s","account":"!!"}"#;
        assert!(serde_json::from_slice::<CryptoRequest>(bad).is_err());
        let extra = br#"{"kind":"due","server":"s","connected":true,"more":1}"#;
        assert!(serde_json::from_slice::<CryptoRequest>(extra).is_err());
    }
}
