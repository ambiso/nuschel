// SPDX-License-Identifier: EUPL-1.2
//! Spike for docs/e2ee.md: OpenMLS 0.9 with the libcrux crypto provider.
//!
//! This is not production code. It exists to check, with tests, the claims the
//! design relies on: the hybrid post-quantum ciphersuite, a three-party group with
//! removal, forward secrecy and post-compromise security as far as OpenMLS exposes
//! them, a server-side delivery service that validates handshake messages without
//! being a member, binding a group to a Nuschl conversation, and an encrypted,
//! transactional store behind OpenMLS's storage interface.
//!
//! Signing uses libcrux's Ed25519 through [`DeviceSigner`], so the spike does not
//! need `openmls_basic_credential` (which would pull in the RustCrypto signature
//! stack: ed25519-dalek, p256, p384, ml-dsa).
#![forbid(unsafe_code)]

pub mod ds;
pub mod identity;
pub mod pins;
pub mod sim;
pub mod store;

pub use openmls;
pub use openmls_libcrux_crypto;
pub use openmls_memory_storage;
pub use openmls_traits;

use openmls::prelude::tls_codec::{Deserialize as _, Serialize as _};
use openmls::prelude::*;
use openmls_libcrux_crypto::CryptoProvider;
use openmls_memory_storage::MemoryStorage;
use openmls_traits::OpenMlsProvider;
use openmls_traits::crypto::OpenMlsCrypto;
use openmls_traits::signatures::{Signer, SignerError};
use std::rc::Rc;
use zeroize::Zeroizing;

use identity::{DeviceKeys, UserIdentity, VerifiedDevice, verify_chain};

/// The suite the design recommends: X-Wing (ML-KEM-768 + X25519) for HPKE,
/// ChaCha20-Poly1305, SHA-256, Ed25519 signatures. OpenMLS code point 0x004D,
/// behind the `draft-ietf-mls-pq-ciphersuites` feature.
pub const SUITE: Ciphersuite = Ciphersuite::MLS_256_XWING_CHACHA20POLY1305_SHA256_Ed25519;

/// The classic RFC 9420 baseline, for size and timing comparisons.
pub const CLASSIC: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_CHACHA20POLY1305_SHA256_Ed25519;

/// Group context extension (private-use range, RFC 9420 section 17.3) that binds an
/// MLS group to one Nuschl server conversation.
pub const CONVERSATION_EXTENSION: u16 = 0xF4E0;

/// Credential type (private-use range) for a Nuschl device credential: the device's
/// leaf signature key certified by the user's identity key.
pub const DEVICE_CREDENTIAL: u16 = 0xF4E1;

/// The OpenMLS provider a Nuschl crypto worker would use: libcrux for all
/// cryptography and randomness, and a storage the worker controls (here OpenMLS's
/// in-memory map, persisted encrypted by [`store::EncryptedStore`]).
pub struct NuschlProvider {
    crypto: CryptoProvider,
    storage: MemoryStorage,
}

impl NuschlProvider {
    pub fn new() -> Self {
        Self::with_storage(MemoryStorage::default())
    }

    pub fn with_storage(storage: MemoryStorage) -> Self {
        Self {
            crypto: CryptoProvider::new().expect("operating system randomness"),
            storage,
        }
    }
}

impl Default for NuschlProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenMlsProvider for NuschlProvider {
    type CryptoProvider = CryptoProvider;
    type RandProvider = CryptoProvider;
    type StorageProvider = MemoryStorage;

    fn storage(&self) -> &Self::StorageProvider {
        &self.storage
    }
    fn crypto(&self) -> &Self::CryptoProvider {
        &self.crypto
    }
    fn rand(&self) -> &Self::RandProvider {
        &self.crypto
    }
}

/// A device's leaf signature key, held by the crypto worker (OpenMLS never needs
/// it in its own storage). Signing goes through libcrux.
pub struct DeviceSigner {
    scheme: SignatureScheme,
    private: Zeroizing<Vec<u8>>,
    public: Vec<u8>,
    crypto: CryptoProvider,
}

impl DeviceSigner {
    pub fn generate(scheme: SignatureScheme) -> Self {
        let crypto = CryptoProvider::new().expect("operating system randomness");
        let (private, public) = crypto
            .signature_key_gen(scheme)
            .expect("libcrux supports the scheme");
        Self {
            scheme,
            private: Zeroizing::new(private),
            public,
            crypto,
        }
    }

    pub fn public(&self) -> &[u8] {
        &self.public
    }
}

impl Signer for DeviceSigner {
    fn sign(&self, payload: &[u8]) -> Result<Vec<u8>, SignerError> {
        self.crypto
            .sign(self.scheme, payload, &self.private)
            .map_err(SignerError::CryptoError)
    }
    fn signature_scheme(&self) -> SignatureScheme {
        self.scheme
    }
}

/// The conversation binding carried in [`CONVERSATION_EXTENSION`]. A client checks it
/// before accepting a Welcome or an external commit, and pins it per conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConversationBinding {
    /// Hash of the server's long-term signing key (its key-transparency log key).
    pub server: [u8; 32],
    /// The server-assigned 128-bit ConversationId (docs/extension.md section 3).
    pub conversation: [u8; 16],
    /// Incremented when the conversation's group is replaced (re-initialised).
    pub generation: u32,
}

impl ConversationBinding {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 32 + 16 + 4);
        out.push(1); // format version
        out.extend_from_slice(&self.server);
        out.extend_from_slice(&self.conversation);
        out.extend_from_slice(&self.generation.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 53 || bytes[0] != 1 {
            return None;
        }
        Some(Self {
            server: bytes[1..33].try_into().ok()?,
            conversation: bytes[33..49].try_into().ok()?,
            generation: u32::from_be_bytes(bytes[49..53].try_into().ok()?),
        })
    }

    /// The MLS group id: the conversation id and generation, so a replacement group
    /// never reuses an id.
    pub fn group_id(&self) -> GroupId {
        let mut id = self.conversation.to_vec();
        id.extend_from_slice(&self.generation.to_be_bytes());
        GroupId::from_slice(&id)
    }

    pub fn from_extensions(extensions: &Extensions<GroupContext>) -> Option<Self> {
        extensions
            .unknown(CONVERSATION_EXTENSION)
            .and_then(|ext| Self::decode(&ext.0))
    }
}

/// Capabilities every Nuschl leaf announces.
pub fn capabilities(suite: Ciphersuite) -> Capabilities {
    Capabilities::builder()
        .ciphersuites(vec![suite])
        .extensions(vec![ExtensionType::Unknown(CONVERSATION_EXTENSION)])
        .credentials(vec![CredentialType::Other(DEVICE_CREDENTIAL)])
        .build()
}

/// The group context extensions of a Nuschl group: the binding, and required
/// capabilities so that no member without Nuschl's extension and credential type
/// can be added.
pub fn group_context_extensions(binding: &ConversationBinding) -> Extensions<GroupContext> {
    Extensions::from_vec(vec![
        Extension::Unknown(CONVERSATION_EXTENSION, UnknownExtension(binding.encode())),
        Extension::RequiredCapabilities(RequiredCapabilitiesExtension::new(
            &[ExtensionType::Unknown(CONVERSATION_EXTENSION)],
            &[],
            &[CredentialType::Other(DEVICE_CREDENTIAL)],
        )),
    ])
    .expect("valid group context extensions")
}

/// Handshake messages as PublicMessage (signed, with a membership tag) so the
/// delivery service can validate them; application messages are always
/// PrivateMessage regardless of this policy.
pub fn wire_policy() -> WireFormatPolicy {
    MIXED_PLAINTEXT_WIRE_FORMAT_POLICY
}

/// One device: its provider (crypto worker state), keys and credential.
pub struct Device {
    pub name: String,
    pub suite: Ciphersuite,
    pub provider: NuschlProvider,
    pub identity: Rc<UserIdentity>,
    pub keys: DeviceKeys,
    pub credential: CredentialWithKey,
}

impl Device {
    /// A new user with a new identity and this as its first device.
    pub fn new(name: &str) -> Self {
        Self::with_suite(name, SUITE)
    }

    pub fn with_suite(name: &str, suite: Ciphersuite) -> Self {
        Self::linked(name, Rc::new(UserIdentity::generate()), suite)
    }

    /// Another device of an existing identity (after linking).
    pub fn linked(name: &str, identity: Rc<UserIdentity>, suite: Ciphersuite) -> Self {
        let keys = DeviceKeys::generate(suite.signature_algorithm());
        let cert = identity.certify_device(&keys, name);
        let credential = Credential::new(
            CredentialType::Other(DEVICE_CREDENTIAL),
            identity.credential_chain(&cert),
        );
        Self {
            name: name.into(),
            suite,
            provider: NuschlProvider::new(),
            credential: CredentialWithKey {
                credential,
                signature_key: keys.signer.public().into(),
            },
            identity,
            keys,
        }
    }

    pub fn signer(&self) -> &DeviceSigner {
        &self.keys.signer
    }

    /// A fresh key package, as published to the server's directory.
    pub fn key_package(&self) -> KeyPackage {
        KeyPackage::builder()
            .leaf_node_capabilities(capabilities(self.suite))
            .build(
                self.suite,
                &self.provider,
                self.signer(),
                self.credential.clone(),
            )
            .expect("key package")
            .key_package()
            .clone()
    }

    pub fn create_group(&self, binding: &ConversationBinding) -> MlsGroup {
        MlsGroup::builder()
            .with_group_id(binding.group_id())
            .ciphersuite(self.suite)
            .with_capabilities(capabilities(self.suite))
            .with_group_context_extensions(group_context_extensions(binding))
            .with_wire_format_policy(wire_policy())
            .use_ratchet_tree_extension(true)
            .build(&self.provider, self.signer(), self.credential.clone())
            .expect("create group")
    }

    pub fn join_config() -> MlsGroupJoinConfig {
        MlsGroupJoinConfig::builder()
            .wire_format_policy(wire_policy())
            .use_ratchet_tree_extension(true)
            .build()
    }

    /// Stages a Welcome without joining yet, so the caller can check the binding.
    pub fn stage_welcome(&self, welcome: &[u8]) -> Result<StagedWelcome, String> {
        let welcome = match from_wire(welcome).extract() {
            MlsMessageBodyIn::Welcome(welcome) => welcome,
            other => return Err(format!("not a welcome: {other:?}")),
        };
        StagedWelcome::new_from_welcome(&self.provider, &Self::join_config(), welcome, None)
            .map_err(|e| format!("{e:?}"))
    }

    pub fn join(&self, welcome: &[u8]) -> MlsGroup {
        self.stage_welcome(welcome)
            .expect("stage welcome")
            .into_group(&self.provider)
            .expect("join group")
    }

    pub fn send(&self, group: &mut MlsGroup, text: &str) -> Vec<u8> {
        to_wire(
            &group
                .create_message(&self.provider, self.signer(), text.as_bytes())
                .expect("encrypt"),
        )
    }

    /// Processes one message from the delivery service. Commits are merged; the
    /// returned value says what happened.
    pub fn receive(&self, group: &mut MlsGroup, bytes: &[u8]) -> Result<Received, String> {
        let message = from_wire(bytes)
            .try_into_protocol_message()
            .map_err(|e| format!("{e:?}"))?;
        let processed = group
            .process_message(&self.provider, message)
            .map_err(|e| format!("{e:?}"))?;
        let sender = processed.credential().clone();
        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => Ok(Received::Application {
                sender: group
                    .members()
                    .find(|m| m.credential == sender)
                    .and_then(|m| member_device(&m).ok()),
                text: String::from_utf8_lossy(&app.into_bytes()).into_owned(),
            }),
            ProcessedMessageContent::StagedCommitMessage(commit) => {
                let removed_self = commit.self_removed();
                group
                    .merge_staged_commit(&self.provider, *commit)
                    .map_err(|e| format!("{e:?}"))?;
                Ok(Received::Commit {
                    epoch: group.epoch().as_u64(),
                    removed_self,
                })
            }
            ProcessedMessageContent::ProposalMessage(proposal) => {
                group
                    .store_pending_proposal(self.provider.storage(), *proposal)
                    .map_err(|e| format!("{e:?}"))?;
                Ok(Received::Proposal)
            }
            other => Ok(Received::Other(format!("{other:?}"))),
        }
    }

    pub fn leaf_of(group: &MlsGroup, name: &str) -> LeafNodeIndex {
        group
            .members()
            .find(|m| member_device(m).is_ok_and(|d| d.display_name == name))
            .map(|m| m.index)
            .expect("member present")
    }

    /// Verifies every member's credential chain against its leaf key: what a
    /// client does on every Welcome and every commit that adds or updates leaves.
    pub fn verify_roster(group: &MlsGroup) -> Result<Vec<VerifiedDevice>, String> {
        group
            .members()
            .map(|m| member_device(&m).map_err(|e| format!("leaf {}: {e}", m.index)))
            .collect()
    }
}

/// Verifies one member's credential chain against its MLS leaf signature key.
pub fn member_device(member: &Member) -> Result<VerifiedDevice, &'static str> {
    if member.credential.credential_type() != CredentialType::Other(DEVICE_CREDENTIAL) {
        return Err("not a Nuschl device credential");
    }
    verify_chain(
        member.credential.serialized_content(),
        &member.signature_key,
    )
}

#[derive(Debug, PartialEq, Eq)]
pub enum Received {
    Application {
        sender: Option<VerifiedDevice>,
        text: String,
    },
    Commit {
        epoch: u64,
        removed_self: bool,
    },
    Proposal,
    Other(String),
}

pub fn to_wire(message: &MlsMessageOut) -> Vec<u8> {
    message.tls_serialize_detached().expect("serialise")
}

pub fn from_wire(mut bytes: &[u8]) -> MlsMessageIn {
    MlsMessageIn::tls_deserialize(&mut bytes).expect("deserialise")
}

/// The SFrame base key for the current epoch (RFC 9605 section 5.2).
pub fn sframe_base_key(device: &Device, group: &MlsGroup) -> Result<Vec<u8>, String> {
    let len = group.ciphersuite().aead_key_length();
    group
        .export_secret(device.provider.crypto(), "SFrame 1.0 Base Key", b"", len)
        .map_err(|e| format!("{e:?}"))
}
