// SPDX-License-Identifier: EUPL-1.2
//! The OpenMLS provider: libcrux for cryptography and randomness, and OpenMLS's
//! in-memory key-value storage as the working set, which the owner persists as a
//! whole ([`Provider::snapshot`]): the crypto worker in its sealed store, the server
//! per group in its database.
use crate::{BINDING_EXTENSION, CREDENTIAL_TYPE, ConversationBinding, Error, SUITE};
use openmls::prelude::*;
use openmls_libcrux_crypto::CryptoProvider;
use openmls_memory_storage::MemoryStorage;
use openmls_traits::OpenMlsProvider;
use std::collections::HashMap;

/// Records in one snapshot, and the size of one key or value.
const MAX_RECORDS: usize = 1_000_000;
const MAX_RECORD: usize = 16 * 1024 * 1024;

pub(crate) struct Provider {
    crypto: CryptoProvider,
    storage: MemoryStorage,
}

impl Provider {
    pub(crate) fn new() -> Result<Self, Error> {
        Ok(Self {
            crypto: CryptoProvider::new().map_err(|_| Error::Invalid("no system randomness"))?,
            storage: MemoryStorage::default(),
        })
    }

    /// Every record OpenMLS holds, as `count || (len key value)*` with 32-bit
    /// lengths, in key order (so equal states give equal bytes).
    pub(crate) fn snapshot(&self) -> Vec<u8> {
        let values = self
            .storage
            .values
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut entries: Vec<(&Vec<u8>, &Vec<u8>)> = values.iter().collect();
        entries.sort();
        let mut out = Vec::new();
        out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (key, value) in entries {
            out.extend_from_slice(&(key.len() as u32).to_be_bytes());
            out.extend_from_slice(key);
            out.extend_from_slice(&(value.len() as u32).to_be_bytes());
            out.extend_from_slice(value);
        }
        out
    }

    pub(crate) fn restore(bytes: &[u8]) -> Result<Self, Error> {
        let provider = Self::new()?;
        let corrupt = || Error::Storage("corrupt MLS state".into());
        let mut at = 0usize;
        let length = |at: &mut usize| -> Result<usize, Error> {
            let field = bytes.get(*at..*at + 4).ok_or_else(corrupt)?;
            *at += 4;
            Ok(u32::from_be_bytes([field[0], field[1], field[2], field[3]]) as usize)
        };
        let count = length(&mut at)?;
        if count > MAX_RECORDS {
            return Err(corrupt());
        }
        let mut values = HashMap::with_capacity(count.min(4096));
        for _ in 0..count {
            let mut pair = [Vec::new(), Vec::new()];
            for slot in &mut pair {
                let len = length(&mut at)?;
                if len > MAX_RECORD {
                    return Err(corrupt());
                }
                *slot = bytes.get(at..at + len).ok_or_else(corrupt)?.to_vec();
                at += len;
            }
            let [key, value] = pair;
            values.insert(key, value);
        }
        if at != bytes.len() {
            return Err(corrupt());
        }
        *provider
            .storage
            .values
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = values;
        Ok(provider)
    }

    /// Replaces the working set (to roll back a failed operation).
    pub(crate) fn replace_with(&self, other: Provider) {
        let values = other
            .storage
            .values
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *self
            .storage
            .values
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = values;
    }

    #[allow(dead_code)]
    pub(crate) fn records(&self) -> usize {
        self.storage
            .values
            .read()
            .map(|values| values.len())
            .unwrap_or(0)
    }
}

impl OpenMlsProvider for Provider {
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

/// What every Nuschl leaf supports: the suite, the binding extension and the
/// Nuschl credential type.
pub(crate) fn capabilities() -> Capabilities {
    Capabilities::builder()
        .ciphersuites(vec![SUITE])
        // LastResort: a last-resort key package carries it, and OpenMLS's validation
        // wants every key package extension listed in the leaf's capabilities.
        .extensions(vec![
            ExtensionType::Unknown(BINDING_EXTENSION),
            ExtensionType::LastResort,
        ])
        .credentials(vec![CredentialType::Other(CREDENTIAL_TYPE)])
        .build()
}

/// The external sender's credential: the server, named by nothing more than this
/// label (its key is what members check, against the pinned key-transparency log).
pub(crate) const SERVER_SENDER: &[u8] = b"nuschl-server";

/// The binding, required capabilities so that no member without Nuschl's
/// extension and credential type can be added, and the server as the one external
/// sender (docs/e2ee.md §6.2): its key-transparency head-signing key (Ed25519),
/// which every client already pins, signs the removals it proposes.
pub(crate) fn group_context_extensions(
    binding: &ConversationBinding,
    server_key: &[u8; 32],
) -> Result<Extensions<GroupContext>, Error> {
    Extensions::from_vec(vec![
        Extension::Unknown(BINDING_EXTENSION, UnknownExtension(binding.encode())),
        Extension::RequiredCapabilities(RequiredCapabilitiesExtension::new(
            &[ExtensionType::Unknown(BINDING_EXTENSION)],
            &[],
            &[CredentialType::Other(CREDENTIAL_TYPE)],
        )),
        Extension::ExternalSenders(vec![server_sender(server_key)]),
    ])
    .map_err(crate::mls)
}

fn server_sender(server_key: &[u8; 32]) -> ExternalSender {
    ExternalSender::new(
        server_key.to_vec().into(),
        Credential::new(CredentialType::Basic, SERVER_SENDER.to_vec()),
    )
}

/// The group's only external sender is the server with this key.
pub(crate) fn server_is_sole_sender(
    extensions: &Extensions<GroupContext>,
    server_key: &[u8; 32],
) -> bool {
    extensions.external_senders().map(Vec::as_slice) == Some(&[server_sender(server_key)][..])
}

/// The binding in a group context, if it is there and well-formed.
pub(crate) fn binding_of(context: &GroupContext) -> Option<ConversationBinding> {
    binding_in(context.extensions())
}

pub(crate) fn binding_in(extensions: &Extensions<GroupContext>) -> Option<ConversationBinding> {
    match extensions.unknown(BINDING_EXTENSION) {
        Some(extension) => ConversationBinding::decode(&extension.0),
        None => None,
    }
}

/// Handshakes as PublicMessage (the delivery service validates them), application
/// messages always as PrivateMessage.
pub(crate) fn wire_policy() -> WireFormatPolicy {
    MIXED_PLAINTEXT_WIRE_FORMAT_POLICY
}

/// Joining: no ratchet tree extension (the tree comes from the server), no past
/// epochs kept (forward secrecy, docs/e2ee.md §6.3).
pub(crate) fn join_config() -> MlsGroupJoinConfig {
    MlsGroupJoinConfig::builder()
        .wire_format_policy(wire_policy())
        .use_ratchet_tree_extension(false)
        .max_past_epochs(0)
        .build()
}

use openmls::prelude::tls_codec::Deserialize as _;

pub(crate) fn serialize(
    value: &impl openmls::prelude::tls_codec::Serialize,
) -> Result<Vec<u8>, Error> {
    value.tls_serialize_detached().map_err(crate::mls)
}

pub(crate) fn message_in(bytes: &[u8]) -> Result<MlsMessageIn, Error> {
    let mut reader = bytes;
    let message = MlsMessageIn::tls_deserialize(&mut reader).map_err(crate::mls)?;
    if !reader.is_empty() {
        return Err(Error::Invalid("trailing bytes after an MLS message"));
    }
    Ok(message)
}

pub(crate) fn tree_in(bytes: &[u8]) -> Result<RatchetTreeIn, Error> {
    let mut reader = bytes;
    let tree = RatchetTreeIn::tls_deserialize(&mut reader).map_err(crate::mls)?;
    if !reader.is_empty() {
        return Err(Error::Invalid("trailing bytes after a ratchet tree"));
    }
    Ok(tree)
}

pub(crate) fn key_package_in(bytes: &[u8]) -> Result<KeyPackageIn, Error> {
    let mut reader = bytes;
    let package = KeyPackageIn::tls_deserialize(&mut reader).map_err(crate::mls)?;
    if !reader.is_empty() {
        return Err(Error::Invalid("trailing bytes after a key package"));
    }
    Ok(package)
}

/// A member as the application sees it: leaf index, the Nuschl device credential
/// bytes and the leaf signature key, which the credential must certify.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberInfo {
    pub index: u32,
    pub credential: Vec<u8>,
    pub signature_key: Vec<u8>,
}

pub(crate) fn member_info(member: &Member) -> Result<MemberInfo, Error> {
    if member.credential.credential_type() != CredentialType::Other(CREDENTIAL_TYPE) {
        return Err(Error::Invalid("a member without a Nuschl credential"));
    }
    Ok(MemberInfo {
        index: member.index.u32(),
        credential: member.credential.serialized_content().to_vec(),
        signature_key: member.signature_key.clone(),
    })
}
