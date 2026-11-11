// SPDX-License-Identifier: EUPL-1.2
//! End-to-end-encrypted conversations (docs/e2ee.md §6, §7): the worker's MLS state
//! and the `Mls*` requests of `nuschel_ipc::crypto`. Direct conversations are groups
//! of the two identities' devices; channels (phase 4c) are groups of the devices that
//! opened them, which join by external commit and are removed when the server
//! proposes it (lost access, inactive, not opened for long).
//!
//! - **State.** One `nuschel_e2ee::client::Client` (OpenMLS's records, private keys
//!   included) and, per server and conversation, a [`Conversation`]: its group, pin
//!   (§7.2), cursor, pending commit, per-sender chains (§6.4), our own chain, the
//!   envelopes waiting for the server and the texts we sent. Both live in one sealed
//!   file, `mls` (`u32 length || JSON || OpenMLS snapshot`), written atomically after
//!   every request that changed anything and before the answer goes out, so a crash
//!   never leaves a cursor that disagrees with the group.
//! - **Credentials.** Every member of a group we create, join or a commit leads to
//!   must verify against its identity's device roster as key transparency last
//!   confirmed it (`Contact::roster`, kept by `Contact` lookups). In a direct
//!   conversation it must also be one of our own devices or the peer's, by
//!   identity; in a channel anyone the server admits may be a member (it follows
//!   its ACLs), and an identity nobody looked up yet is unconfirmed, not refused. A
//!   stranger in a direct conversation, a forged or copied credential, or a revoked
//!   device is refused: nothing is merged and the conversation is blocked. A device
//!   key transparency does not confirm (yet) is merged, to stay in sync, but blocks
//!   sending until a lookup confirms it.
//! - **The server's part** (docs/e2ee.md §6.2). Each group names the server's
//!   key-transparency signing key (from the pinned log) as its only external
//!   sender. Its proposals are removals only, and a commit of them happens when the
//!   server asks this device to (`MlsCommitProposals`). In a channel, members never
//!   add anyone and remove only what the server proposed; a commit that does
//!   otherwise is refused.
//! - **Pins.** A conversation with a group is pinned: the session refuses plaintext
//!   for it, and a Welcome for a pinned, active conversation is refused (a server
//!   cannot replace a group). Bindings must name this server's log and the
//!   conversation, generation 0.
//! - **Envelopes.** Every chat message is an `Envelope` with our counter, the hash of
//!   our previous envelope and of the newest one we saw; receivers classify each
//!   sender's chain (in order, gap, fork, duplicate).
use crate::state::{Contact, ServerState};
use crate::store::Store;
use crate::worker::Worker;
use nuschel_e2ee::client::{Client, Incoming, MemberInfo, ValidKeyPackage};
use nuschel_e2ee::envelope::{self, Chain, Envelope, Kind};
use nuschel_e2ee::{ConversationBinding, ConversationKind};
use nuschel_identity::{
    DeviceCertificate, DeviceCredential, DeviceId, DeviceRevocation, DeviceRoster,
    DeviceSigningCertificate, Error as IdentityError, HybridPublicKey, IdentityFingerprint,
    VerifiedDevice, VerifiedIdentity,
};
use nuschel_ipc::crypto::{
    ContactState, ConversationView, CryptoRequest, CryptoResponse, GroupInit, KeyPackageBatch,
    Lock, MemberView, MlsOutgoing, Order, Received, base64_bytes,
};
use nuschel_kt::LogDescriptor;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use zeroize::Zeroizing;

const MLS_FILE: &str = "mls";
/// Texts of our own messages kept per conversation, to show them when they come
/// back in the stream (a device cannot decrypt its own MLS messages).
const MAX_SENT: usize = 256;
/// Envelope hashes remembered per conversation for `seen` checks.
const MAX_KNOWN: usize = 1024;
/// Envelopes encrypted but not yet stored by the server.
const MAX_OUTGOING: usize = 64;
const MAX_NOTICES: usize = 32;
/// Conversations per server.
const MAX_CONVERSATIONS: usize = 4096;

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digit = |c: u8| (c as char).to_digit(16);
            Some((digit(pair[0])? * 16 + digit(pair[1])?) as u8)
        })
        .collect()
}

fn array16(bytes: &[u8], what: &str) -> Result<[u8; 16], String> {
    bytes.try_into().map_err(|_| format!("{what} is 16 bytes"))
}

/// Our commit waiting for the server's answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingCommit {
    #[serde(with = "base64_bytes")]
    pub commit: Vec<u8>,
    #[serde(with = "base64_bytes")]
    pub idempotency_key: Vec<u8>,
    /// It creates the group (refused: the group is forgotten).
    pub create: bool,
    pub epoch: u64,
}

/// An envelope we encrypted, until the server stored it: a resend after
/// `STALE_EPOCH` encrypts the same envelope again.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutgoingEnvelope {
    #[serde(with = "base64_bytes")]
    pub envelope: Vec<u8>,
}

/// A message we sent: the ciphertext's hash (to recognise it in the stream).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SentText {
    #[serde(with = "base64_bytes")]
    pub ciphertext_hash: Vec<u8>,
    #[serde(with = "base64_bytes")]
    pub idempotency_key: Vec<u8>,
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainState {
    pub counter: u64,
    #[serde(with = "base64_bytes32")]
    pub hash: [u8; 32],
    /// Counters a gap skipped: a message that arrives late fills one.
    #[serde(default)]
    pub missing: BTreeSet<u64>,
}

/// Skipped counters remembered per sender.
const MAX_MISSING: usize = 1024;

mod base64_bytes32 {
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error> {
        nuschel_ipc::crypto::base64_bytes::serialize(bytes, serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 32], D::Error> {
        let bytes = nuschel_ipc::crypto::base64_bytes::deserialize(deserializer)?;
        bytes
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

/// One conversation's end-to-end state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conversation {
    /// Empty: no group (yet, or the creation was refused).
    #[serde(with = "base64_bytes")]
    pub group_id: Vec<u8>,
    /// The other account of the direct conversation; empty for a channel.
    #[serde(with = "base64_bytes")]
    pub peer: Vec<u8>,
    /// A channel's group (server-driven membership).
    #[serde(default)]
    pub channel: bool,
    /// Pinned (§7.2): the server accepted our group, or we joined one.
    pub pinned: bool,
    pub generation: u32,
    pub active: bool,
    /// The last processed seq.
    pub cursor: u64,
    pub pending: Option<PendingCommit>,
    /// Per sender device (hex): its chain.
    pub chains: BTreeMap<String, ChainState>,
    pub own: ChainState,
    /// The newest envelope we processed or sent.
    #[serde(with = "base64_bytes32")]
    pub seen: [u8; 32],
    /// Envelope hashes we know (hex), oldest first.
    pub known: VecDeque<String>,
    /// By idempotency key (hex).
    pub outgoing: BTreeMap<String, OutgoingEnvelope>,
    pub sent: VecDeque<SentText>,
    pub notices: Vec<String>,
    /// Sending is blocked for good (a refused commit, removal).
    pub blocked: Option<String>,
    /// Sending waits for key transparency to confirm a member device.
    pub unverified: Option<String>,
    /// Something needs attention (a fork, a gap the server would not fill).
    pub alarm: Option<String>,
}

impl Conversation {
    fn notice(&mut self, text: String) {
        if self.notices.len() < MAX_NOTICES {
            self.notices.push(text);
        }
    }

    fn know(&mut self, hash: [u8; 32]) {
        self.known.push_back(hex(&hash));
        while self.known.len() > MAX_KNOWN {
            self.known.pop_front();
        }
        self.seen = hash;
    }

    fn reset_group(&mut self) {
        let peer = std::mem::take(&mut self.peer);
        *self = Self {
            peer,
            channel: self.channel,
            ..Self::default()
        };
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MlsState {
    pub version: u32,
    /// Server (sticky key) → conversation (hex) → state.
    pub servers: BTreeMap<String, BTreeMap<String, Conversation>>,
}

/// The worker's MLS half: OpenMLS's state and the conversations.
pub struct Mls {
    pub client: Client,
    pub state: MlsState,
}

impl Mls {
    pub fn load(store: &Store) -> io::Result<Self> {
        match store.load(MLS_FILE)? {
            None => Ok(Self {
                client: Client::new().map_err(io::Error::other)?,
                state: MlsState {
                    version: 1,
                    ..MlsState::default()
                },
            }),
            Some(bytes) => {
                let corrupt = || io::Error::other("the MLS state is corrupt");
                let length = bytes.get(..4).ok_or_else(corrupt)?;
                let length =
                    u32::from_be_bytes([length[0], length[1], length[2], length[3]]) as usize;
                let json = bytes.get(4..4 + length).ok_or_else(corrupt)?;
                let snapshot = bytes.get(4 + length..).ok_or_else(corrupt)?;
                let state: MlsState = serde_json::from_slice(json)
                    .map_err(|error| io::Error::other(format!("the MLS state: {error}")))?;
                Ok(Self {
                    client: Client::restore(snapshot).map_err(io::Error::other)?,
                    state,
                })
            }
        }
    }

    pub fn save(&self, store: &Store) -> Result<(), String> {
        let json = Zeroizing::new(
            serde_json::to_vec(&self.state).map_err(|error| format!("MLS state: {error}"))?,
        );
        let snapshot = Zeroizing::new(self.client.snapshot());
        let mut plain = Zeroizing::new(Vec::with_capacity(4 + json.len() + snapshot.len()));
        plain.extend_from_slice(&(json.len() as u32).to_be_bytes());
        plain.extend_from_slice(&json);
        plain.extend_from_slice(&snapshot);
        store
            .save(MLS_FILE, &plain)
            .map_err(|error| format!("cannot save the MLS state: {error}"))
    }
}

/// How one member's credential checked out.
enum Verdict {
    Verified {
        device: VerifiedDevice,
        own: bool,
    },
    /// The right identity, but key transparency does not list this device (yet).
    Unverified {
        identity: IdentityFingerprint,
        device: [u8; 16],
        own: bool,
    },
    Refused(String),
}

/// The rosters a conversation's members are checked against.
struct Rosters {
    own: DeviceRoster,
    /// Other identities with their rosters (when a lookup confirmed one): the
    /// peer of a direct conversation, or everyone looked up on this server for a
    /// channel.
    others: Vec<(IdentityFingerprint, Option<DeviceRoster>)>,
    /// A direct conversation: no identity but ours and `others` may be in it.
    closed: bool,
}

fn roster_from(contact: &Contact) -> Option<DeviceRoster> {
    let record = contact.roster.as_ref()?;
    let root = HybridPublicKey::decode(&contact.roster_root).ok()?;
    let signing = DeviceSigningCertificate::decode(&record.device_signing_certificate).ok()?;
    let identity = VerifiedIdentity::verify(root, signing).ok()?;
    if contact.pinned.as_deref() != Some(identity.fingerprint().to_hex().as_str()) {
        return None;
    }
    let mut roster = DeviceRoster::new(identity);
    for bytes in &record.devices {
        let certificate = DeviceCertificate::decode(bytes).ok()?;
        roster.add_certificate(certificate).ok()?;
    }
    for bytes in &record.revocations {
        let revocation = DeviceRevocation::decode(bytes).ok()?;
        roster.add_revocation(revocation).ok()?;
    }
    Some(roster)
}

fn fingerprint_of(text: &str) -> Option<IdentityFingerprint> {
    let bytes: [u8; 32] = unhex(text)?.try_into().ok()?;
    Some(IdentityFingerprint::from_bytes(bytes))
}

impl Rosters {
    fn check(&self, member: &MemberInfo, now_s: u64) -> Verdict {
        let Ok(credential) = DeviceCredential::decode(&member.credential) else {
            return Verdict::Refused("a member without a valid Nuschel credential".into());
        };
        let identity = credential.identity();
        let (roster, own) = if identity == self.own.identity().fingerprint() {
            (Some(&self.own), true)
        } else {
            match self.others.iter().find(|(other, _)| *other == identity) {
                Some((_, roster)) => (roster.as_ref(), false),
                None if self.closed => {
                    return Verdict::Refused(format!(
                        "a device of another identity ({}) was added to a direct conversation",
                        &identity.to_hex()[..16]
                    ));
                }
                // A channel member nobody looked up yet: unconfirmed until a lookup.
                None => (None, false),
            }
        };
        let device = *credential.device_id().as_bytes();
        let Some(roster) = roster else {
            return Verdict::Unverified {
                identity,
                device,
                own,
            };
        };
        match roster.check_credential(&credential, &member.signature_key, now_s) {
            Ok(verified) => Verdict::Verified {
                device: verified,
                own,
            },
            Err(
                IdentityError::BadSignature
                | IdentityError::KeyMismatch
                | IdentityError::WrongIdentity
                | IdentityError::Revoked,
            ) => Verdict::Refused(format!(
                "a member's credential does not verify ({})",
                roster
                    .check_credential(&credential, &member.signature_key, now_s)
                    .err()
                    .map_or_else(String::new, |error| error.to_string())
            )),
            // Not in the roster, a newer key epoch or generation than the record
            // shows, outside its validity: key transparency cannot confirm it now.
            Err(_) => Verdict::Unverified {
                identity,
                device,
                own,
            },
        }
    }
}

/// The members' verdicts: refused (with the first reason), the views, and the
/// first unverified device's reason.
struct Assessment {
    refused: Option<String>,
    unverified: Option<String>,
    members: Vec<MemberView>,
}

fn assess(rosters: &Rosters, members: &[MemberInfo], now_s: u64) -> Assessment {
    let mut assessment = Assessment {
        refused: None,
        unverified: None,
        members: Vec::new(),
    };
    for member in members {
        match rosters.check(member, now_s) {
            Verdict::Verified { device, own } => assessment.members.push(MemberView {
                identity: device.identity.to_hex(),
                device: hex(device.device_id.as_bytes()),
                device_name: device.display_name.as_str().to_owned(),
                own,
                verified: true,
            }),
            Verdict::Unverified {
                identity,
                device,
                own,
            } => {
                assessment.unverified.get_or_insert_with(|| {
                    if own {
                        "One of your devices in this conversation is not confirmed by key \
                         transparency yet; sending waits until it is."
                            .into()
                    } else if rosters.closed {
                        "Your contact has a device that key transparency has not confirmed \
                         yet; sending waits until it is."
                            .into()
                    } else {
                        "A member's device is not confirmed by key transparency yet; \
                         sending waits until it is."
                            .into()
                    }
                });
                assessment.members.push(MemberView {
                    identity: identity.to_hex(),
                    device: hex(&device),
                    device_name: "unconfirmed device".into(),
                    own,
                    verified: false,
                });
            }
            Verdict::Refused(reason) => {
                assessment.refused.get_or_insert(reason);
            }
        }
    }
    assessment
}

fn key_of(bytes: &[u8]) -> Result<String, String> {
    Ok(hex(&array16(bytes, "an idempotency key")?))
}

fn random_key() -> Result<Vec<u8>, String> {
    DeviceId::random()
        .map(|id| id.as_bytes().to_vec())
        .map_err(|error| error.to_string())
}

impl Worker {
    /// The `Mls*` requests.
    pub(crate) fn mls_request(&mut self, request: CryptoRequest) -> Result<CryptoResponse, String> {
        let response = match request {
            CryptoRequest::MlsKeyPackages {
                server,
                one_time,
                last_resort,
            } => {
                let _ = self.server(&server)?;
                let keystore = self.keystore.as_ref().ok_or("no identity yet")?;
                let mut batch = KeyPackageBatch::default();
                for _ in 0..one_time {
                    batch.one_time.push(
                        self.mls
                            .client
                            .key_package(keystore.device(), keystore.device_credential(), false)
                            .map_err(|error| error.to_string())?,
                    );
                }
                if last_resort {
                    batch.last_resort = self
                        .mls
                        .client
                        .key_package(keystore.device(), keystore.device_credential(), true)
                        .map_err(|error| error.to_string())?;
                }
                batch.device = keystore.device().device_id().as_bytes().to_vec();
                self.mls.save(&self.store)?;
                CryptoResponse::MlsKeyPackages(batch)
            }
            CryptoRequest::MlsStatus {
                server,
                conversation,
            } => {
                let conversation = array16(&conversation, "a conversation id")?;
                self.recheck(&server, &conversation)?;
                CryptoResponse::MlsConversation(self.conversation_view(&server, &conversation)?)
            }
            CryptoRequest::MlsCreateDirect {
                server,
                conversation,
                peer,
                key_packages,
            } => {
                let conversation = array16(&conversation, "a conversation id")?;
                let peer = array16(&peer, "an account id")?;
                CryptoResponse::MlsOutgoing(self.create_direct(
                    &server,
                    &conversation,
                    &peer,
                    &key_packages,
                )?)
            }
            CryptoRequest::MlsAddDevices {
                server,
                conversation,
                key_packages,
            } => {
                let conversation = array16(&conversation, "a conversation id")?;
                CryptoResponse::MlsOutgoing(self.add_devices(
                    &server,
                    &conversation,
                    &key_packages,
                )?)
            }
            CryptoRequest::MlsCommitDone {
                server,
                conversation,
                accepted,
                seq,
            } => {
                let conversation = array16(&conversation, "a conversation id")?;
                self.commit_done(&server, &conversation, accepted, seq)?;
                CryptoResponse::MlsConversation(self.conversation_view(&server, &conversation)?)
            }
            CryptoRequest::MlsWelcome {
                server,
                conversation,
                peer,
                welcome,
                tree,
                seq,
            } => {
                let conversation = array16(&conversation, "a conversation id")?;
                let peer = array16(&peer, "an account id")?;
                self.welcome(&server, &conversation, &peer, &welcome, &tree, seq)?;
                CryptoResponse::MlsConversation(self.conversation_view(&server, &conversation)?)
            }
            CryptoRequest::MlsReceive {
                server,
                conversation,
                seq,
                message,
            } => {
                let conversation = array16(&conversation, "a conversation id")?;
                let received = self.receive(&server, &conversation, seq, &message)?;
                CryptoResponse::MlsReceived(received)
            }
            CryptoRequest::MlsEncrypt {
                server,
                conversation,
                idempotency_key,
                text,
            } => {
                let conversation = array16(&conversation, "a conversation id")?;
                let (message, epoch) =
                    self.encrypt(&server, &conversation, &idempotency_key, &text)?;
                CryptoResponse::MlsCiphertext { message, epoch }
            }
            CryptoRequest::MlsSent {
                server,
                conversation,
                idempotency_key,
                seq,
            } => {
                let conversation = array16(&conversation, "a conversation id")?;
                let key = key_of(&idempotency_key)?;
                let entry = self.conversation_mut(&server, &conversation)?;
                entry.outgoing.remove(&key);
                let _ = seq;
                self.mls.save(&self.store)?;
                CryptoResponse::Done
            }
            CryptoRequest::MlsCreateChannel {
                server,
                conversation,
            } => {
                let conversation = array16(&conversation, "a conversation id")?;
                CryptoResponse::MlsOutgoing(self.create_channel(&server, &conversation)?)
            }
            CryptoRequest::MlsJoinChannel {
                server,
                conversation,
                group_info,
                tree,
            } => {
                let conversation = array16(&conversation, "a conversation id")?;
                CryptoResponse::MlsOutgoing(self.join_channel(
                    &server,
                    &conversation,
                    &group_info,
                    &tree,
                )?)
            }
            CryptoRequest::MlsCommitProposals {
                server,
                conversation,
            } => {
                let conversation = array16(&conversation, "a conversation id")?;
                CryptoResponse::MlsOutgoing(self.commit_proposals(&server, &conversation)?)
            }
            _ => return Err("unsupported request".into()),
        };
        Ok(response)
    }

    // ---- state ----

    fn conversation(&self, server: &str, conversation: &[u8; 16]) -> Option<&Conversation> {
        self.mls
            .state
            .servers
            .get(server)
            .and_then(|conversations| conversations.get(&hex(conversation)))
    }

    fn conversation_mut(
        &mut self,
        server: &str,
        conversation: &[u8; 16],
    ) -> Result<&mut Conversation, String> {
        let conversations = self.mls.state.servers.entry(server.to_owned()).or_default();
        let key = hex(conversation);
        if !conversations.contains_key(&key) && conversations.len() >= MAX_CONVERSATIONS {
            return Err("too many end-to-end conversations on this server".into());
        }
        Ok(conversations.entry(key).or_default())
    }

    /// This server's key-transparency log id: what bindings must name.
    fn log_id(&self, server: &str) -> Result<[u8; 32], String> {
        let entry: &ServerState = self.server(server)?;
        Ok(LogDescriptor::decode(&entry.descriptor)
            .map_err(|error| error.to_string())?
            .log_id()
            .0)
    }

    /// The rosters for a conversation's members: its peer's (direct), or every
    /// identity looked up on this server (a channel).
    fn rosters_for(&self, server: &str, entry: &Conversation) -> Result<Rosters, String> {
        if entry.channel {
            return self.channel_rosters(server);
        }
        let peer = array16(&entry.peer, "an account id")?;
        self.rosters(server, &peer)
    }

    fn channel_rosters(&self, server: &str) -> Result<Rosters, String> {
        let mut rosters = self.rosters(server, &[0; 16])?;
        let own = rosters.own.identity().fingerprint();
        let entry = self.server(server)?;
        for contact in entry.contacts.values() {
            let Some(pinned) = contact.pinned.as_deref().and_then(fingerprint_of) else {
                continue;
            };
            if pinned == own || rosters.others.iter().any(|(other, _)| *other == pinned) {
                continue;
            }
            rosters.others.push((pinned, roster_from(contact)));
        }
        rosters.closed = false;
        Ok(rosters)
    }

    fn rosters(&self, server: &str, peer: &[u8; 16]) -> Result<Rosters, String> {
        let keystore = self.keystore.as_ref().ok_or("no identity yet")?;
        let mut own = DeviceRoster::new(keystore.identity().clone());
        own.add_certificate(keystore.device_certificate().clone())
            .map_err(|error| error.to_string())?;
        let entry = self.server(server)?;
        // Our other devices: what a lookup of our own account confirmed.
        if let Some(contact) = entry.contacts.get(&hex(&entry.account))
            && let Some(record) = roster_from(contact)
            && record.identity().fingerprint() == keystore.identity().fingerprint()
            && let Some(stored) = &contact.roster
        {
            // This device's own certificate stays the keystore's (it may be newer).
            for bytes in &stored.devices {
                if let Ok(certificate) = DeviceCertificate::decode(bytes)
                    && certificate.device_id() != keystore.device().device_id()
                {
                    let _ = own.add_certificate(certificate);
                }
            }
            for bytes in &stored.revocations {
                if let Ok(revocation) = DeviceRevocation::decode(bytes) {
                    let _ = own.add_revocation(revocation);
                }
            }
        }
        let others = entry
            .contacts
            .get(&hex(peer))
            .and_then(|contact| {
                let pinned = fingerprint_of(contact.pinned.as_deref()?)?;
                Some((pinned, roster_from(contact)))
            })
            .into_iter()
            .collect();
        Ok(Rosters {
            own,
            others,
            closed: true,
        })
    }

    /// This server's key-transparency signing key: the groups' external sender.
    fn server_key(&self, server: &str) -> Result<[u8; 32], String> {
        let entry: &ServerState = self.server(server)?;
        LogDescriptor::decode(&entry.descriptor)
            .map_err(|error| error.to_string())?
            .signing_key()
            .to_bytes()
            .try_into()
            .map_err(|_| "the log's signing key is not an Ed25519 key".to_owned())
    }

    fn peer_contact(&self, server: &str, peer: &[u8]) -> Option<&Contact> {
        self.state
            .servers
            .get(server)
            .and_then(|entry| entry.contacts.get(&hex(peer)))
    }

    /// Re-checks every member against the rosters (after a lookup refreshed them).
    fn recheck(&mut self, server: &str, conversation: &[u8; 16]) -> Result<(), String> {
        let Some(entry) = self.conversation(server, conversation) else {
            return Ok(());
        };
        if entry.group_id.is_empty() || !self.mls.client.has_group(&entry.group_id) {
            return Ok(());
        }
        let group_id = entry.group_id.clone();
        let rosters = self.rosters_for(server, &entry.clone())?;
        let members = self
            .mls
            .client
            .members(&group_id)
            .map_err(|error| error.to_string())?;
        let assessment = assess(&rosters, &members, self.now() / 1000);
        let entry = self.conversation_mut(server, conversation)?;
        let before = entry.unverified.clone();
        entry.unverified = assessment.unverified;
        if before.is_some() && entry.unverified.is_none() {
            entry.notice("Key transparency confirmed every device in this conversation.".into());
        }
        if let Some(reason) = assessment.refused {
            entry.blocked.get_or_insert(reason);
        }
        self.mls.save(&self.store)
    }

    pub(crate) fn conversation_view(
        &mut self,
        server: &str,
        conversation: &[u8; 16],
    ) -> Result<ConversationView, String> {
        let entry = self
            .conversation(server, conversation)
            .cloned()
            .unwrap_or_default();
        let has_group = !entry.group_id.is_empty() && self.mls.client.has_group(&entry.group_id);
        let mut members = Vec::new();
        let mut epoch = 0;
        let mut missing = Vec::new();
        let mut removals = 0;
        if has_group && (entry.channel || entry.peer.len() == 16) {
            let rosters = self.rosters_for(server, &entry)?;
            let infos = self
                .mls
                .client
                .members(&entry.group_id)
                .map_err(|error| error.to_string())?;
            members = assess(&rosters, &infos, self.now() / 1000).members;
            epoch = self
                .mls
                .client
                .epoch(&entry.group_id)
                .map_err(|error| error.to_string())?;
            if let Ok(peer) = array16(&entry.peer, "an account id")
                && !entry.channel
            {
                missing = self.missing_accounts(server, &peer, &rosters, &members);
            }
            removals = self
                .mls
                .client
                .pending_removals(&entry.group_id)
                .map_or(0, |leaves| leaves.len() as u32);
        }
        let peer_contact = self.peer_contact(server, &entry.peer).cloned();
        let key_change = peer_contact.as_ref().is_some_and(|contact| {
            matches!(contact.state, ContactState::KeyChanged { blocking: true })
        });
        let blocked = if !has_group {
            None
        } else if !entry.active && entry.channel {
            Some(
                "This device left the channel's encrypted group; open the channel to join again."
                    .to_owned(),
            )
        } else if !entry.active {
            Some("You were removed from this conversation.".to_owned())
        } else if let Some(reason) = &entry.blocked {
            Some(reason.clone())
        } else if key_change {
            Some(
                "Your contact's identity changed. Compare safety numbers and accept the new \
                 identity before sending."
                    .to_owned(),
            )
        } else {
            entry.unverified.clone()
        };
        let peer_verified = peer_contact
            .as_ref()
            .and_then(|contact| contact.pinned.as_deref())
            .is_some_and(|root| self.is_verified(root));
        let lock = if !has_group {
            Lock::Pending {
                reason: "no end-to-end group yet".into(),
            }
        } else if let Some(reason) = entry
            .blocked
            .as_ref()
            .or(blocked.as_ref().filter(|_| !entry.active || key_change))
        {
            Lock::Attention {
                reason: reason.clone(),
            }
        } else if let Some(reason) = &entry.alarm {
            Lock::Attention {
                reason: reason.clone(),
            }
        } else if let Some(reason) = &entry.unverified {
            Lock::Pending {
                reason: reason.clone(),
            }
        } else if let Some(reason) =
            peer_contact
                .as_ref()
                .and_then(|contact| match &contact.state {
                    ContactState::Unavailable { reason, .. } => {
                        Some(format!("Automatic verification unavailable: {reason}"))
                    }
                    ContactState::Misbehaviour { reason } => Some(reason.clone()),
                    _ => None,
                })
        {
            Lock::Attention { reason }
        } else if peer_verified {
            Lock::Verified
        } else {
            Lock::Encrypted
        };
        // Notices are shown once.
        let notices = if has_group || !entry.notices.is_empty() {
            match self.conversation_mut(server, conversation) {
                Ok(stored) => {
                    let notices = std::mem::take(&mut stored.notices);
                    if !notices.is_empty() {
                        self.mls.save(&self.store)?;
                    }
                    notices
                }
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };
        Ok(ConversationView {
            server: server.to_owned(),
            conversation: conversation.to_vec(),
            pinned: entry.pinned,
            active: entry.active,
            epoch,
            cursor: entry.cursor,
            lock,
            members,
            blocked,
            notices,
            pending_commit: entry.pending.is_some(),
            missing,
            channel: entry.channel,
            removals,
        })
    }

    /// Accounts whose roster lists active devices the group does not have.
    fn missing_accounts(
        &self,
        server: &str,
        peer: &[u8; 16],
        rosters: &Rosters,
        members: &[MemberView],
    ) -> Vec<Vec<u8>> {
        let now_s = self.now() / 1000;
        let present: BTreeSet<&str> = members
            .iter()
            .map(|member| member.device.as_str())
            .collect();
        let lacks = |roster: &DeviceRoster| {
            roster
                .active_devices(now_s)
                .iter()
                .any(|device| !present.contains(hex(device.device_id.as_bytes()).as_str()))
        };
        let mut missing = Vec::new();
        if let Some((_, Some(roster))) = rosters.others.first()
            && lacks(roster)
        {
            missing.push(peer.to_vec());
        }
        if let Ok(entry) = self.server(server)
            && entry.account.len() == 16
            && lacks(&rosters.own)
        {
            missing.push(entry.account.clone());
        }
        missing
    }

    // ---- creating and adding ----

    /// Checks claimed key packages: valid, not this device, not already present,
    /// verified against the rosters. Returns the packages to add and notes.
    fn admit_packages(
        &self,
        rosters: &Rosters,
        packages: &[Vec<u8>],
        present: &BTreeSet<[u8; 16]>,
    ) -> Result<(Vec<ValidKeyPackage>, Vec<String>, bool), String> {
        let keystore = self.keystore.as_ref().ok_or("no identity yet")?;
        let own_device = *keystore.device().device_id().as_bytes();
        let now_s = self.now() / 1000;
        let mut admitted = Vec::new();
        let mut notes = Vec::new();
        let mut seen = present.clone();
        let mut peer_device = false;
        for bytes in packages {
            let package = match ValidKeyPackage::check(bytes) {
                Ok(package) => package,
                Err(error) => {
                    notes.push(format!(
                        "A key package from the server was refused: {error}."
                    ));
                    continue;
                }
            };
            let Ok(credential) = DeviceCredential::decode(&package.credential) else {
                notes.push("A key package without a Nuschel credential was refused.".into());
                continue;
            };
            let device = *credential.device_id().as_bytes();
            if device == own_device || !seen.insert(device) {
                continue;
            }
            let member = MemberInfo {
                index: 0,
                credential: package.credential.clone(),
                signature_key: package.signature_key.clone(),
            };
            match rosters.check(&member, now_s) {
                Verdict::Verified { own, .. } => {
                    peer_device |= !own;
                    admitted.push(package);
                }
                Verdict::Unverified { .. } => notes
                    .push("A device key transparency has not confirmed was left out.".to_owned()),
                Verdict::Refused(reason) => {
                    notes.push(format!("A key package was refused: {reason}."))
                }
            }
        }
        Ok((admitted, notes, peer_device))
    }

    fn create_direct(
        &mut self,
        server: &str,
        conversation: &[u8; 16],
        peer: &[u8; 16],
        packages: &[Vec<u8>],
    ) -> Result<MlsOutgoing, String> {
        if let Some(entry) = self.conversation(server, conversation)
            && entry.pinned
            && entry.active
        {
            return Err("this conversation already has an end-to-end group".into());
        }
        let log_id = self.log_id(server)?;
        let contact = self
            .peer_contact(server, peer)
            .ok_or("look up the contact first")?;
        if contact.pinned.is_none() {
            return Err("the contact has no identity on this server".into());
        }
        if matches!(contact.state, ContactState::KeyChanged { blocking: true }) {
            return Err("the contact's identity changed; accept it first".into());
        }
        let rosters = self.rosters(server, peer)?;
        let (admitted, notes, peer_device) =
            self.admit_packages(&rosters, packages, &BTreeSet::new())?;
        if !peer_device {
            return Err(format!(
                "none of the contact's devices could be added{}",
                notes
                    .first()
                    .map(|note| format!(": {note}"))
                    .unwrap_or_default()
            ));
        }
        // A group from an earlier attempt that the server never accepted.
        if let Some(entry) = self.conversation(server, conversation)
            && !entry.group_id.is_empty()
            && self.mls.client.has_group(&entry.group_id)
        {
            let _ = self.mls.client.delete_group(&entry.group_id.clone());
        }
        let server_key = self.server_key(server)?;
        let keystore = self.keystore.as_ref().ok_or("no identity yet")?;
        let binding = ConversationBinding::direct(log_id, *conversation);
        let created = self
            .mls
            .client
            .create(
                keystore.device(),
                keystore.device_credential(),
                &binding,
                &server_key,
                &admitted,
            )
            .map_err(|error| error.to_string())?;
        let idempotency_key = random_key()?;
        let entry = self.conversation_mut(server, conversation)?;
        entry.reset_group();
        entry.peer = peer.to_vec();
        entry.group_id = created.group_id.clone();
        entry.active = true;
        entry.pending = Some(PendingCommit {
            commit: created.first.commit.clone(),
            idempotency_key: idempotency_key.clone(),
            create: true,
            epoch: created.first.epoch,
        });
        for note in notes {
            entry.notice(note);
        }
        self.mls.save(&self.store)?;
        Ok(MlsOutgoing {
            conversation: conversation.to_vec(),
            idempotency_key,
            commit: created.first.commit,
            group_info: created.first.group_info,
            welcome: created.first.welcome.unwrap_or_default(),
            create: Some(GroupInit {
                group_info: created.init_group_info,
                tree: created.init_tree,
            }),
            epoch: created.first.epoch,
        })
    }

    fn add_devices(
        &mut self,
        server: &str,
        conversation: &[u8; 16],
        packages: &[Vec<u8>],
    ) -> Result<MlsOutgoing, String> {
        let entry = self
            .conversation(server, conversation)
            .cloned()
            .ok_or("no end-to-end group for this conversation")?;
        if !entry.pinned || !entry.active || entry.group_id.is_empty() {
            return Err("no end-to-end group for this conversation".into());
        }
        if entry.pending.is_some() {
            return Err("a commit is waiting for the server".into());
        }
        if let Some(reason) = &entry.blocked {
            return Err(reason.clone());
        }
        // Key transparency without fresh witness cosignatures (crate::witness).
        if let Some(reason) = self.new_devices_blocked(server) {
            return Err(reason);
        }
        if entry.channel {
            return Err("devices join a channel by themselves".into());
        }
        let peer = array16(&entry.peer, "an account id")?;
        let rosters = self.rosters(server, &peer)?;
        let present: BTreeSet<[u8; 16]> = self
            .mls
            .client
            .members(&entry.group_id)
            .map_err(|error| error.to_string())?
            .iter()
            .filter_map(|member| DeviceCredential::decode(&member.credential).ok())
            .map(|credential| *credential.device_id().as_bytes())
            .collect();
        let (admitted, notes, _) = self.admit_packages(&rosters, packages, &present)?;
        if admitted.is_empty() {
            return Err("nothing to add".into());
        }
        let keystore = self.keystore.as_ref().ok_or("no identity yet")?;
        let outgoing = self
            .mls
            .client
            .add(keystore.device(), &entry.group_id, &admitted)
            .map_err(|error| error.to_string())?;
        let idempotency_key = random_key()?;
        let stored = self.conversation_mut(server, conversation)?;
        stored.pending = Some(PendingCommit {
            commit: outgoing.commit.clone(),
            idempotency_key: idempotency_key.clone(),
            create: false,
            epoch: outgoing.epoch,
        });
        for note in notes {
            stored.notice(note);
        }
        self.mls.save(&self.store)?;
        Ok(MlsOutgoing {
            conversation: conversation.to_vec(),
            idempotency_key,
            commit: outgoing.commit,
            group_info: outgoing.group_info,
            welcome: outgoing.welcome.unwrap_or_default(),
            create: None,
            epoch: outgoing.epoch,
        })
    }

    fn commit_done(
        &mut self,
        server: &str,
        conversation: &[u8; 16],
        accepted: bool,
        seq: u64,
    ) -> Result<(), String> {
        let entry = self
            .conversation(server, conversation)
            .cloned()
            .ok_or("no end-to-end group for this conversation")?;
        let Some(pending) = entry.pending.clone() else {
            // Already settled (the commit came back in the stream first).
            return Ok(());
        };
        if accepted {
            if self.mls.client.has_pending_commit(&entry.group_id) {
                self.mls
                    .client
                    .merge_pending(&entry.group_id)
                    .map_err(|error| error.to_string())?;
            }
            let stored = self.conversation_mut(server, conversation)?;
            stored.pending = None;
            stored.pinned = true;
            stored.active = true;
            stored.cursor = stored.cursor.max(seq);
            self.mls.save(&self.store)?;
            self.recheck(server, conversation)?;
        } else {
            if pending.create {
                let _ = self.mls.client.delete_group(&entry.group_id);
                let stored = self.conversation_mut(server, conversation)?;
                stored.reset_group();
            } else {
                if self.mls.client.has_pending_commit(&entry.group_id) {
                    self.mls
                        .client
                        .clear_pending(&entry.group_id)
                        .map_err(|error| error.to_string())?;
                }
                self.conversation_mut(server, conversation)?.pending = None;
            }
            self.mls.save(&self.store)?;
        }
        Ok(())
    }

    // ---- channels (docs/e2ee.md §6.2) ----

    fn create_channel(
        &mut self,
        server: &str,
        conversation: &[u8; 16],
    ) -> Result<MlsOutgoing, String> {
        if let Some(entry) = self.conversation(server, conversation)
            && entry.pinned
            && entry.active
        {
            return Err("this channel already has an end-to-end group".into());
        }
        let log_id = self.log_id(server)?;
        let server_key = self.server_key(server)?;
        self.forget_group(server, conversation);
        let keystore = self.keystore.as_ref().ok_or("no identity yet")?;
        let binding = ConversationBinding::channel(log_id, *conversation);
        let created = self
            .mls
            .client
            .create(
                keystore.device(),
                keystore.device_credential(),
                &binding,
                &server_key,
                &[],
            )
            .map_err(|error| error.to_string())?;
        let idempotency_key = random_key()?;
        let entry = self.conversation_mut(server, conversation)?;
        entry.reset_group();
        entry.channel = true;
        entry.peer.clear();
        entry.group_id = created.group_id.clone();
        entry.active = true;
        entry.pending = Some(PendingCommit {
            commit: created.first.commit.clone(),
            idempotency_key: idempotency_key.clone(),
            create: true,
            epoch: created.first.epoch,
        });
        self.mls.save(&self.store)?;
        Ok(MlsOutgoing {
            conversation: conversation.to_vec(),
            idempotency_key,
            commit: created.first.commit,
            group_info: created.first.group_info,
            welcome: Vec::new(),
            create: Some(GroupInit {
                group_info: created.init_group_info,
                tree: created.init_tree,
            }),
            epoch: created.first.epoch,
        })
    }

    /// A group of ours for this conversation that is not (or no longer) the one
    /// to keep: an unaccepted creation, or the one we were removed from.
    fn forget_group(&mut self, server: &str, conversation: &[u8; 16]) {
        if let Some(entry) = self.conversation(server, conversation)
            && !entry.group_id.is_empty()
            && self.mls.client.has_group(&entry.group_id)
        {
            let _ = self.mls.client.delete_group(&entry.group_id.clone());
        }
    }

    fn join_channel(
        &mut self,
        server: &str,
        conversation: &[u8; 16],
        group_info: &[u8],
        tree: &[u8],
    ) -> Result<MlsOutgoing, String> {
        let existing = self
            .conversation(server, conversation)
            .cloned()
            .unwrap_or_default();
        if existing.pinned && existing.active {
            return Err("this device is already in the channel's group".into());
        }
        if !existing.peer.is_empty() {
            return Err("this is a direct conversation".into());
        }
        let log_id = self.log_id(server)?;
        let server_key = self.server_key(server)?;
        self.forget_group(server, conversation);
        let rosters = self.channel_rosters(server)?;
        let keystore = self.keystore.as_ref().ok_or("no identity yet")?;
        let joined = self
            .mls
            .client
            .join_external(
                keystore.device(),
                keystore.device_credential(),
                group_info,
                tree,
                &server_key,
            )
            .map_err(|error| format!("cannot join the channel's group: {error}"))?;
        let binding = joined.binding;
        let mismatch = if binding.server != log_id {
            Some("the channel's group is bound to another server")
        } else if binding.conversation != *conversation {
            Some("the group is bound to another conversation")
        } else if binding.generation != 0
            || binding.kind != ConversationKind::Channel
            || binding.membership != nuschel_e2ee::Membership::Server
        {
            Some("the group is not this channel's")
        } else {
            None
        };
        let assessment = assess(&rosters, &joined.members, self.now() / 1000);
        if let Some(reason) = mismatch.map(str::to_owned).or(assessment.refused) {
            let _ = self.mls.client.delete_group(&binding.group_id());
            self.mls.save(&self.store)?;
            return Err(format!("refused the channel's group: {reason}"));
        }
        let rejoined = existing.pinned && !existing.active;
        let idempotency_key = random_key()?;
        let entry = self.conversation_mut(server, conversation)?;
        entry.reset_group();
        entry.channel = true;
        entry.group_id = binding.group_id();
        entry.active = true;
        entry.cursor = existing.cursor;
        entry.unverified = assessment.unverified;
        // OpenMLS merged the external commit as it made it: a refusal forgets the
        // group, as for a creation.
        entry.pending = Some(PendingCommit {
            commit: joined.outgoing.commit.clone(),
            idempotency_key: idempotency_key.clone(),
            create: true,
            epoch: joined.outgoing.epoch,
        });
        if rejoined {
            entry.notice("This device joined the channel's encrypted group again.".into());
        }
        self.mls.save(&self.store)?;
        Ok(MlsOutgoing {
            conversation: conversation.to_vec(),
            idempotency_key,
            commit: joined.outgoing.commit,
            group_info: joined.outgoing.group_info,
            welcome: Vec::new(),
            create: None,
            epoch: joined.outgoing.epoch,
        })
    }

    fn commit_proposals(
        &mut self,
        server: &str,
        conversation: &[u8; 16],
    ) -> Result<MlsOutgoing, String> {
        let entry = self
            .conversation(server, conversation)
            .cloned()
            .ok_or("no end-to-end group for this conversation")?;
        if !entry.pinned || !entry.active || entry.group_id.is_empty() {
            return Err("no end-to-end group for this conversation".into());
        }
        if entry.pending.is_some() {
            return Err("a commit is waiting for the server".into());
        }
        if let Some(reason) = &entry.blocked {
            return Err(reason.clone());
        }
        let keystore = self.keystore.as_ref().ok_or("no identity yet")?;
        let outgoing = self
            .mls
            .client
            .commit_proposals(keystore.device(), &entry.group_id)
            .map_err(|error| error.to_string())?;
        let idempotency_key = random_key()?;
        let stored = self.conversation_mut(server, conversation)?;
        stored.pending = Some(PendingCommit {
            commit: outgoing.commit.clone(),
            idempotency_key: idempotency_key.clone(),
            create: false,
            epoch: outgoing.epoch,
        });
        self.mls.save(&self.store)?;
        Ok(MlsOutgoing {
            conversation: conversation.to_vec(),
            idempotency_key,
            commit: outgoing.commit,
            group_info: outgoing.group_info,
            welcome: outgoing.welcome.unwrap_or_default(),
            create: None,
            epoch: outgoing.epoch,
        })
    }

    // ---- joining ----

    fn welcome(
        &mut self,
        server: &str,
        conversation: &[u8; 16],
        peer: &[u8; 16],
        welcome: &[u8],
        tree: &[u8],
        seq: u64,
    ) -> Result<(), String> {
        let existing = self
            .conversation(server, conversation)
            .cloned()
            .unwrap_or_default();
        if existing.pinned && existing.active {
            return Err(
                "refused a Welcome that would replace this conversation's active end-to-end \
                 group"
                    .into(),
            );
        }
        let log_id = self.log_id(server)?;
        let contact = self
            .peer_contact(server, peer)
            .ok_or("look up the contact first")?;
        if contact.pinned.is_none() {
            return Err("the contact has no identity on this server".into());
        }
        let rosters = self.rosters(server, peer)?;
        // An unaccepted group of our own creation lost to this one.
        if !existing.group_id.is_empty() && self.mls.client.has_group(&existing.group_id) {
            let _ = self.mls.client.delete_group(&existing.group_id);
        }
        let staged = self.mls.client.stage_welcome(welcome, tree);
        // Staging uses up the key package either way.
        self.mls.save(&self.store)?;
        let staged = staged.map_err(|error| format!("the Welcome: {error}"))?;
        let binding = staged.binding;
        if binding.server != log_id {
            return Err("the Welcome's group is bound to another server".into());
        }
        if binding.conversation != *conversation {
            return Err("the Welcome's group is bound to another conversation".into());
        }
        if binding.generation != 0 || binding.kind != ConversationKind::Direct {
            return Err("the Welcome's group is not this direct conversation's".into());
        }
        let assessment = assess(&rosters, &staged.members, self.now() / 1000);
        if let Some(reason) = assessment.refused {
            return Err(format!("refused the Welcome: {reason}"));
        }
        let group_id = self
            .mls
            .client
            .join(staged)
            .map_err(|error| error.to_string())?;
        let rejoined = existing.pinned && !existing.active;
        let entry = self.conversation_mut(server, conversation)?;
        entry.reset_group();
        entry.peer = peer.to_vec();
        entry.group_id = group_id;
        entry.pinned = true;
        entry.active = true;
        entry.cursor = seq;
        entry.unverified = assessment.unverified;
        if rejoined {
            entry.notice("You were added back to this conversation.".into());
        }
        self.mls.save(&self.store)
    }

    // ---- the stream ----

    fn receive(
        &mut self,
        server: &str,
        conversation: &[u8; 16],
        seq: u64,
        message: &[u8],
    ) -> Result<Received, String> {
        let Some(entry) = self.conversation(server, conversation).cloned() else {
            return Ok(Received::Undecryptable {
                seq,
                reason: "no end-to-end group for this conversation".into(),
            });
        };
        if seq <= entry.cursor {
            return Ok(Received::Duplicate { seq });
        }
        if entry.group_id.is_empty() || !self.mls.client.has_group(&entry.group_id) {
            return Ok(Received::Undecryptable {
                seq,
                reason: "no end-to-end group for this conversation".into(),
            });
        }
        let group_id = entry.group_id.clone();
        // Our pending commit, stored by the server (its ack was lost).
        if let Some(pending) = &entry.pending
            && pending.commit == message
        {
            if self.mls.client.has_pending_commit(&group_id) {
                self.mls
                    .client
                    .merge_pending(&group_id)
                    .map_err(|error| error.to_string())?;
            }
            let stored = self.conversation_mut(server, conversation)?;
            stored.pending = None;
            stored.pinned = true;
            stored.cursor = seq;
            self.mls.save(&self.store)?;
            self.recheck(server, conversation)?;
            let view = self.conversation_view(server, conversation)?;
            return Ok(Received::Commit { seq, view });
        }
        let hash = envelope::hash_encoded(message);
        if let Some(sent) = entry
            .sent
            .iter()
            .find(|sent| sent.ciphertext_hash == hash)
            .cloned()
        {
            let stored = self.conversation_mut(server, conversation)?;
            stored.cursor = seq;
            stored.outgoing.remove(&hex(&sent.idempotency_key));
            self.mls.save(&self.store)?;
            return Ok(Received::Own {
                seq,
                text: sent.text,
                idempotency_key: sent.idempotency_key,
            });
        }
        let handshake = nuschel_e2ee::prevalidate::wire_format(message)
            .is_ok_and(|format| format == nuschel_e2ee::prevalidate::WireFormat::PublicMessage);
        if handshake && let Some(pending) = &entry.pending {
            // Someone else's commit won the epoch: ours is void.
            if pending.create {
                let _ = self.mls.client.delete_group(&group_id);
                let stored = self.conversation_mut(server, conversation)?;
                stored.reset_group();
                self.mls.save(&self.store)?;
                return Ok(Received::Nothing { seq });
            }
            if self.mls.client.has_pending_commit(&group_id) {
                self.mls
                    .client
                    .clear_pending(&group_id)
                    .map_err(|error| error.to_string())?;
            }
            self.conversation_mut(server, conversation)?.pending = None;
        }
        let incoming = self.mls.client.process(&group_id, message);
        let result = match incoming {
            Err(error) => {
                let stored = self.conversation_mut(server, conversation)?;
                stored.cursor = seq;
                if handshake {
                    stored.alarm = Some(format!(
                        "A change to this conversation's group could not be processed ({error})."
                    ));
                }
                Received::Undecryptable {
                    seq,
                    reason: error.to_string(),
                }
            }
            Ok(Incoming::Proposal { .. }) => {
                self.conversation_mut(server, conversation)?.cursor = seq;
                Received::Nothing { seq }
            }
            Ok(Incoming::Commit(commit)) => {
                let rosters = self.rosters_for(server, &entry)?;
                let assessment = assess(&rosters, &commit.members, self.now() / 1000);
                // Server-driven membership (docs/e2ee.md §6.2): devices join by
                // themselves, and members remove only what the server proposed.
                let policy = (entry.channel && (commit.adds > 0 || !commit.removals_by_server))
                    .then(|| "a member changed the channel's membership on its own".to_owned());
                if let Some(reason) = assessment.refused.or(policy) {
                    let stored = self.conversation_mut(server, conversation)?;
                    stored.cursor = seq;
                    stored.blocked = Some(format!(
                        "A change to this conversation was refused: {reason}"
                    ));
                    self.mls.save(&self.store)?;
                    let view = self.conversation_view(server, conversation)?;
                    return Ok(Received::Refused { seq, reason, view });
                }
                let self_removed = commit.self_removed;
                let removed: Vec<MemberView> =
                    assess(&rosters, &commit.removed, self.now() / 1000).members;
                let joined = commit.external_join;
                self.mls
                    .client
                    .merge(&group_id, commit)
                    .map_err(|error| error.to_string())?;
                let stored = self.conversation_mut(server, conversation)?;
                stored.cursor = seq;
                if !self_removed {
                    for member in removed.iter().take(4) {
                        stored.notice(format!(
                            "{} left the encrypted group (the server removed it).",
                            member.device_name
                        ));
                    }
                }
                if joined && !stored.channel {
                    stored.notice("A device joined this conversation by itself.".into());
                }
                let had = stored.unverified.is_some();
                stored.unverified = assessment.unverified;
                if !had && stored.unverified.is_some() {
                    stored.notice(
                        "A device that key transparency has not confirmed yet joined.".into(),
                    );
                }
                if self_removed {
                    stored.active = false;
                }
                self.mls.save(&self.store)?;
                let view = self.conversation_view(server, conversation)?;
                return Ok(Received::Commit { seq, view });
            }
            Ok(Incoming::Application { sender, data }) => {
                self.application(server, conversation, seq, &sender, &data)?
            }
        };
        self.mls.save(&self.store)?;
        Ok(result)
    }

    fn application(
        &mut self,
        server: &str,
        conversation: &[u8; 16],
        seq: u64,
        sender: &MemberInfo,
        data: &[u8],
    ) -> Result<Received, String> {
        let undecryptable = |reason: &str| Received::Undecryptable {
            seq,
            reason: reason.to_owned(),
        };
        let entry = self
            .conversation(server, conversation)
            .cloned()
            .unwrap_or_default();
        let rosters = self.rosters_for(server, &entry)?;
        let view = assess(&rosters, std::slice::from_ref(sender), self.now() / 1000)
            .members
            .pop();
        self.conversation_mut(server, conversation)?.cursor = seq;
        let Ok(credential) = DeviceCredential::decode(&sender.credential) else {
            return Ok(undecryptable("the sender has no Nuschel credential"));
        };
        let Some(view) = view else {
            return Ok(undecryptable("the sender's credential does not verify"));
        };
        let envelope = match Envelope::decode(data) {
            Ok(envelope) => envelope,
            Err(error) => return Ok(undecryptable(&format!("malformed envelope: {error}"))),
        };
        if envelope.device_id != *credential.device_id().as_bytes() {
            return Ok(undecryptable(
                "the envelope names another device than its sender",
            ));
        }
        if !envelope.kind.durable() {
            return Ok(Received::Nothing { seq });
        }
        let hash = envelope.hash();
        let stored = self.conversation_mut(server, conversation)?;
        let device = hex(&envelope.device_id);
        let mut state = stored.chains.get(&device).cloned().unwrap_or_default();
        let mut chain = Chain {
            counter: state.counter,
            hash: state.hash,
        };
        let late = envelope.counter < state.counter && state.missing.remove(&envelope.counter);
        let order = match chain.accept(&envelope, hash) {
            // A message the server delivered after later ones: it fills its gap.
            _ if late => {
                stored.notice(format!(
                    "A message from {} arrived late (out of order).",
                    view.device_name
                ));
                chain = Chain {
                    counter: state.counter,
                    hash: state.hash,
                };
                Order::InOrder
            }
            envelope::Order::Duplicate => return Ok(Received::Duplicate { seq }),
            envelope::Order::InOrder => Order::InOrder,
            envelope::Order::Gap { missing } => {
                stored.notice(format!(
                    "{missing} message(s) from {} were not delivered by the server.",
                    view.device_name
                ));
                for counter in (state.counter + 1..envelope.counter).take(MAX_MISSING) {
                    if state.missing.len() < MAX_MISSING {
                        state.missing.insert(counter);
                    }
                }
                Order::Gap { missing }
            }
            envelope::Order::Fork => {
                stored.alarm = Some(format!(
                    "The server showed different histories of this conversation (a message \
                     from {} contradicts an earlier one). Both are kept.",
                    view.device_name
                ));
                Order::Fork
            }
        };
        stored.chains.insert(
            device,
            ChainState {
                counter: chain.counter,
                hash: chain.hash,
                missing: state.missing,
            },
        );
        if envelope.seen != [0; 32] && !stored.known.contains(&hex(&envelope.seen)) {
            stored.notice(format!(
                "{} saw a message this device never received; the server may be withholding \
                 it.",
                view.device_name
            ));
        }
        stored.know(hash);
        if envelope.kind != Kind::Chat {
            return Ok(Received::Nothing { seq });
        }
        let Ok(text) = String::from_utf8(envelope.body) else {
            return Ok(undecryptable("the message is not text"));
        };
        Ok(Received::Message {
            seq,
            sender: view,
            text,
            sent_at: envelope.sent_at,
            idempotency_key: envelope.idempotency_key.to_vec(),
            order,
        })
    }

    // ---- sending ----

    fn encrypt(
        &mut self,
        server: &str,
        conversation: &[u8; 16],
        idempotency_key: &[u8],
        text: &str,
    ) -> Result<(Vec<u8>, u64), String> {
        let key = key_of(idempotency_key)?;
        let view = self.conversation_view(server, conversation)?;
        if !view.pinned {
            return Err("this conversation has no end-to-end group yet".into());
        }
        if let Some(reason) = view.blocked {
            return Err(reason);
        }
        if view.pending_commit {
            return Err("a commit is waiting for the server".into());
        }
        let device = *self
            .keystore
            .as_ref()
            .ok_or("no identity yet")?
            .device()
            .device_id()
            .as_bytes();
        let now = self.now();
        let entry = self.conversation_mut(server, conversation)?;
        let encoded = match entry.outgoing.get(&key) {
            // A resend: the same envelope, counter and chain, for the new epoch.
            Some(outgoing) => outgoing.envelope.clone(),
            None => {
                if entry.outgoing.len() >= MAX_OUTGOING {
                    return Err("too many messages wait for the server".into());
                }
                let envelope = Envelope {
                    kind: Kind::Chat,
                    device_id: device,
                    counter: entry.own.counter + 1,
                    prev: entry.own.hash,
                    seen: entry.seen,
                    idempotency_key: array16(idempotency_key, "an idempotency key")?,
                    sent_at: now,
                    archive_hash: Vec::new(),
                    body: text.as_bytes().to_vec(),
                };
                let encoded = envelope.encode();
                let hash = envelope::hash_encoded(&encoded);
                entry.own = ChainState {
                    counter: envelope.counter,
                    hash,
                    missing: BTreeSet::new(),
                };
                entry.know(hash);
                entry.outgoing.insert(
                    key.clone(),
                    OutgoingEnvelope {
                        envelope: encoded.clone(),
                    },
                );
                encoded
            }
        };
        let group_id = entry.group_id.clone();
        let keystore = self.keystore.as_ref().ok_or("no identity yet")?;
        let message = self
            .mls
            .client
            .encrypt(keystore.device(), &group_id, &encoded)
            .map_err(|error| error.to_string())?;
        let epoch = self
            .mls
            .client
            .epoch(&group_id)
            .map_err(|error| error.to_string())?;
        let body = Envelope::decode(&encoded)
            .map(|envelope| String::from_utf8_lossy(&envelope.body).into_owned())
            .unwrap_or_default();
        let entry = self.conversation_mut(server, conversation)?;
        entry.sent.push_back(SentText {
            ciphertext_hash: envelope::hash_encoded(&message).to_vec(),
            idempotency_key: idempotency_key.to_vec(),
            text: body,
        });
        while entry.sent.len() > MAX_SENT {
            entry.sent.pop_front();
        }
        self.mls.save(&self.store)?;
        Ok((message, epoch))
    }
}
