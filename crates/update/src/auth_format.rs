// SPDX-License-Identifier: EUPL-1.2
//! Versioned update-authentication transport and exact signed bytes.
//!
//! This module performs no asymmetric cryptography. Public-key and signature
//! encodings here are size-checked opaque bytes; the isolated verifier must also
//! validate their mathematical encodings and require BOTH standard signatures.
//! This is an application protocol, not a claimed new signature algorithm.
#![forbid(unsafe_code)]

use sha2::{Digest, Sha256};

pub const FORMAT_VERSION: u16 = 2;
pub const ALGORITHM: u16 = 1; // exactly Ed25519 AND ML-DSA-44
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
pub const ED_PUBLIC_BYTES: usize = 32;
pub const ML_PUBLIC_BYTES: usize = 1312;
pub const PUBLIC_BYTES: usize = ED_PUBLIC_BYTES + ML_PUBLIC_BYTES;
pub const ED_SIGNATURE_BYTES: usize = 64;
pub const ML_SIGNATURE_BYTES: usize = 2420;
pub const SIGNATURE_BYTES: usize = 8 + 2 + 2 + 32 + ED_SIGNATURE_BYTES + ML_SIGNATURE_BYTES;
const MAGIC: &[u8; 8] = b"NUSUPD\0\0";
const KEY_DOMAIN: &[u8] = b"nuschl.update.key-pair.v2\0";
pub const MESSAGE_DOMAIN: &[u8] = b"nuschl.update.manifest.v2\0";
pub const ML_CONTEXT: &[u8] = b"nuschl.update.signature.v2";
pub const MAX_SIGNED_BYTES: usize = MESSAGE_DOMAIN.len() + 40 + PUBLIC_BYTES + MAX_MANIFEST_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid update key document")]
    KeyDocument,
    #[error("incorrect update authentication object size")]
    Size,
    #[error("unsupported update authentication format")]
    Version,
    #[error("update authentication must require Ed25519 and ML-DSA-44")]
    Algorithm,
    #[error("signature does not name the selected update key pair")]
    KeyId,
}

/// Opaque public pair. Construction does not authenticate a key or establish
/// trust; only explicitly enrolled pairs may be passed to the verifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    bytes: Box<[u8; PUBLIC_BYTES]>,
}

impl PublicKey {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != PUBLIC_BYTES {
            return Err(Error::Size);
        }
        Ok(Self {
            bytes: bytes
                .to_vec()
                .into_boxed_slice()
                .try_into()
                .map_err(|_| Error::Size)?,
        })
    }
    pub fn as_bytes(&self) -> &[u8; PUBLIC_BYTES] {
        &self.bytes
    }
    pub fn ed25519(&self) -> &[u8; ED_PUBLIC_BYTES] {
        self.bytes[..ED_PUBLIC_BYTES]
            .try_into()
            .expect("fixed public key encoding")
    }
    pub fn mldsa44(&self) -> &[u8; ML_PUBLIC_BYTES] {
        self.bytes[ED_PUBLIC_BYTES..]
            .try_into()
            .expect("fixed public key encoding")
    }
    /// Stable routing identifier. The signed message also includes the complete
    /// public pair, so hash collisions cannot substitute another component key.
    pub fn id(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(KEY_DOMAIN);
        hash.update(FORMAT_VERSION.to_be_bytes());
        hash.update(ALGORITHM.to_be_bytes());
        hash.update(&self.bytes[..]);
        hash.finalize().into()
    }
}

/// Fixed binary envelope: magic8 || version:u16be || algorithm:u16be || key_id32
/// || ed25519_signature64 || mldsa44_signature2420. No optional signature fields,
/// alternate algorithms, trailing data or legacy format are accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    bytes: Box<[u8; SIGNATURE_BYTES]>,
}
impl Signature {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != SIGNATURE_BYTES {
            return Err(Error::Size);
        }
        if &bytes[..8] != MAGIC || bytes[8..10] != FORMAT_VERSION.to_be_bytes() {
            return Err(Error::Version);
        }
        if bytes[10..12] != ALGORITHM.to_be_bytes() {
            return Err(Error::Algorithm);
        }
        Ok(Self {
            bytes: bytes
                .to_vec()
                .into_boxed_slice()
                .try_into()
                .map_err(|_| Error::Size)?,
        })
    }
    pub fn from_components(key: &PublicKey, ed25519: &[u8], mldsa44: &[u8]) -> Result<Self, Error> {
        if ed25519.len() != ED_SIGNATURE_BYTES || mldsa44.len() != ML_SIGNATURE_BYTES {
            return Err(Error::Size);
        }
        let mut bytes = Vec::with_capacity(SIGNATURE_BYTES);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
        bytes.extend_from_slice(&ALGORITHM.to_be_bytes());
        bytes.extend_from_slice(&key.id());
        bytes.extend_from_slice(ed25519);
        bytes.extend_from_slice(mldsa44);
        Self::from_bytes(&bytes)
    }
    pub fn as_bytes(&self) -> &[u8; SIGNATURE_BYTES] {
        &self.bytes
    }
    pub fn key_id(&self) -> &[u8; 32] {
        self.bytes[12..44]
            .try_into()
            .expect("fixed signature encoding")
    }
    pub fn check_key(&self, key: &PublicKey) -> Result<(), Error> {
        if self.key_id() == &key.id() {
            Ok(())
        } else {
            Err(Error::KeyId)
        }
    }
    pub fn ed25519(&self) -> &[u8; ED_SIGNATURE_BYTES] {
        self.bytes[44..44 + ED_SIGNATURE_BYTES]
            .try_into()
            .expect("fixed signature encoding")
    }
    pub fn mldsa44(&self) -> &[u8; ML_SIGNATURE_BYTES] {
        self.bytes[44 + ED_SIGNATURE_BYTES..]
            .try_into()
            .expect("fixed signature encoding")
    }
}

/// Both algorithms sign exactly these bytes. JSON is never parsed/re-emitted to
/// construct them: whitespace, ordering and every raw manifest byte are bound.
/// The full pair, algorithm and version prevent cross-protocol/component splices.
pub fn signed_bytes(key: &PublicKey, manifest: &[u8]) -> Result<Vec<u8>, Error> {
    if manifest.is_empty() || manifest.len() > MAX_MANIFEST_BYTES {
        return Err(Error::Size);
    }
    let mut bytes = Vec::with_capacity(MESSAGE_DOMAIN.len() + PUBLIC_BYTES + manifest.len() + 40);
    bytes.extend_from_slice(MESSAGE_DOMAIN);
    bytes.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    bytes.extend_from_slice(&ALGORITHM.to_be_bytes());
    bytes.extend_from_slice(&key.id());
    bytes.extend_from_slice(key.as_bytes());
    bytes.extend_from_slice(&(manifest.len() as u32).to_be_bytes());
    bytes.extend_from_slice(manifest);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair(byte: u8) -> PublicKey {
        PublicKey::from_bytes(&[byte; PUBLIC_BYTES]).unwrap()
    }
    #[test]
    fn packet_refuses_one_missing_half_trailing_bytes_and_algorithm_downgrade() {
        let signature = Signature::from_components(
            &pair(1),
            &[2; ED_SIGNATURE_BYTES],
            &[3; ML_SIGNATURE_BYTES],
        )
        .unwrap();
        assert_eq!(
            Signature::from_bytes(signature.as_bytes()).unwrap(),
            signature
        );
        for size in [0, 44 + ED_SIGNATURE_BYTES, SIGNATURE_BYTES - 1] {
            assert_eq!(
                Signature::from_bytes(&signature.as_bytes()[..size]),
                Err(Error::Size)
            );
        }
        let mut bytes = signature.as_bytes().to_vec();
        bytes.push(0);
        assert_eq!(Signature::from_bytes(&bytes), Err(Error::Size));
        let mut bytes = signature.as_bytes().to_vec();
        bytes[9] = 1;
        assert_eq!(Signature::from_bytes(&bytes), Err(Error::Version));
        let mut bytes = signature.as_bytes().to_vec();
        bytes[11] = 0;
        assert_eq!(Signature::from_bytes(&bytes), Err(Error::Algorithm));
    }
    #[test]
    fn signed_bytes_bind_raw_json_and_both_component_keys_without_normalization() {
        let key = pair(1);
        let message = br#"{"version":"1.0.0"}"#;
        let bytes = signed_bytes(&key, message).unwrap();
        assert_ne!(
            bytes,
            signed_bytes(&key, br#"{ "version":"1.0.0"}"#).unwrap()
        );
        let mut changed = *key.as_bytes();
        changed[0] ^= 1;
        assert_ne!(
            bytes,
            signed_bytes(&PublicKey::from_bytes(&changed).unwrap(), message).unwrap()
        );
        changed = *key.as_bytes();
        changed[ED_PUBLIC_BYTES] ^= 1;
        assert_ne!(
            bytes,
            signed_bytes(&PublicKey::from_bytes(&changed).unwrap(), message).unwrap()
        );
        let signature =
            Signature::from_components(&key, &[0; ED_SIGNATURE_BYTES], &[0; ML_SIGNATURE_BYTES])
                .unwrap();
        assert_eq!(signature.check_key(&pair(2)), Err(Error::KeyId));
        assert!(bytes.ends_with(message));
        assert!(signed_bytes(&key, &[]).is_err());
        assert!(signed_bytes(&key, &vec![0; MAX_MANIFEST_BYTES + 1]).is_err());
    }
}
