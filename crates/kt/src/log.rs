// SPDX-License-Identifier: EUPL-1.2
//! The log descriptor clients pin, and the keys that sign heads.

use crate::codec::{DecodeError, EncodeError, Reader, Writer};
use crate::{MSG_DESCRIPTOR, akd_backend};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use std::fmt;
use thiserror::Error;

/// Domain separation for the log id.
const LOG_ID_CONTEXT: &[u8] = b"nuschl-kt/v1 log id\0";
/// Largest encoded [`LogDescriptor`].
pub const MAX_DESCRIPTOR_BYTES: usize = 512;
const MAX_SIGNING_KEY: usize = 64;
const MAX_SUITE_PARAMS: usize = 256;

/// The construction behind a log: tree, VRF, hash and proof formats. A future IETF
/// KEYTRANS backend is a new suite; heads, gossip and the client-facing results stay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Suite {
    /// `akd` 0.13 with its current ("experimental") configuration under the domain label
    /// `nuschl-kt/v1 akd`: BLAKE3, ECVRF-EDWARDS25519-SHA512-TAI (RFC 9381), 256-bit
    /// labels. Suite parameter: the 32-byte VRF public key.
    AkdBlake3Ecvrf25519V1 = 1,
}

impl Suite {
    fn from_u8(v: u8) -> Result<Self, DecodeError> {
        match v {
            1 => Ok(Self::AkdBlake3Ecvrf25519V1),
            _ => Err(DecodeError::InvalidValue { field: "suite" }),
        }
    }
}

/// How heads are signed. A hybrid Ed25519 + ML-DSA-65 scheme can be added as a new
/// value without changing the head format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SignatureScheme {
    /// Ed25519 (RFC 8032) with strict verification: canonical `s`, no small-order keys
    /// or `R`. 32-byte keys, 64-byte signatures.
    Ed25519 = 1,
}

impl SignatureScheme {
    fn from_u8(v: u8) -> Result<Self, DecodeError> {
        match v {
            1 => Ok(Self::Ed25519),
            _ => Err(DecodeError::InvalidValue {
                field: "signature_scheme",
            }),
        }
    }

    /// Exact signature length in bytes.
    pub fn signature_len(self) -> usize {
        match self {
            Self::Ed25519 => 64,
        }
    }
}

/// The public key that signs a log's heads: the server's identity key
/// (docs/extension.md §3).
#[derive(Clone, PartialEq, Eq)]
pub struct LogSigningKey {
    scheme: SignatureScheme,
    key: VerifyingKey,
}

impl fmt::Debug for LogSigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LogSigningKey({:?}, ", self.scheme)?;
        for b in self.key.as_bytes() {
            write!(f, "{b:02x}")?;
        }
        write!(f, ")")
    }
}

impl LogSigningKey {
    pub fn ed25519(bytes: &[u8; 32]) -> Option<Self> {
        let key = VerifyingKey::from_bytes(bytes).ok()?;
        (!key.is_weak()).then_some(Self {
            scheme: SignatureScheme::Ed25519,
            key,
        })
    }

    pub fn scheme(&self) -> SignatureScheme {
        self.scheme
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.key.as_bytes().to_vec()
    }

    /// Strict verification. ed25519-dalek's `legacy_compatibility` feature (turned on
    /// by `akd_core` for every crate in the build) accepts non-canonical `s`, so the
    /// scalar is checked here as well: a head has exactly one valid signature.
    pub(crate) fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        match self.scheme {
            SignatureScheme::Ed25519 => {
                let Ok(bytes) = <[u8; 64]>::try_from(signature) else {
                    return false;
                };
                let mut s = [0u8; 32];
                s.copy_from_slice(&bytes[32..]);
                if !scalar_is_canonical(&s) {
                    return false;
                }
                let signature = ed25519_dalek::Signature::from_bytes(&bytes);
                self.key.verify_strict(message, &signature).is_ok()
            }
        }
    }
}

/// The group order ℓ = 2^252 + 27742317777372353535851937790883648493, little-endian.
const ED25519_ORDER: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
];

/// `s < ℓ` for a little-endian scalar.
fn scalar_is_canonical(s: &[u8; 32]) -> bool {
    for i in (0..32).rev() {
        match s[i].cmp(&ED25519_ORDER[i]) {
            std::cmp::Ordering::Less => return true,
            std::cmp::Ordering::Greater => return false,
            std::cmp::Ordering::Equal => {}
        }
    }
    false
}

/// A failure of the signing backend (for example an HSM or a key in another process).
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("head signing failed: {0}")]
pub struct SignerError(pub String);

/// Signs heads on the server. The server's identity key implements this; an Ed25519
/// software key is provided.
pub trait HeadSigner: Send + Sync {
    fn public_key(&self) -> LogSigningKey;
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, SignerError>;
}

/// An Ed25519 head signer holding the private key in memory.
pub struct Ed25519HeadSigner {
    key: SigningKey,
}

impl Ed25519HeadSigner {
    /// From a 32-byte secret seed (RFC 8032 private key). Generate it with a CSPRNG and
    /// keep it with the server's other secrets.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(seed),
        }
    }
}

impl HeadSigner for Ed25519HeadSigner {
    fn public_key(&self) -> LogSigningKey {
        LogSigningKey {
            scheme: SignatureScheme::Ed25519,
            key: self.key.verifying_key(),
        }
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, SignerError> {
        Ok(self.key.sign(message).to_bytes().to_vec())
    }
}

/// Identifies a log: SHA-256 over `"nuschl-kt/v1 log id\0" || descriptor`. Every head
/// names it, so a head binds the suite, the signing key and the VRF key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogId(pub [u8; 32]);

impl fmt::Debug for LogId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LogId(")?;
        for b in &self.0[..8] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "…)")
    }
}

/// Everything a client pins about a server's log: the suite, the head-signing key and
/// the suite's public parameters (for akd, the VRF public key). Sent in the
/// `NuschlHello` and pinned with the server (sticky); invites can carry its [`LogId`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogDescriptor {
    suite: Suite,
    signing_key: LogSigningKey,
    suite_params: Vec<u8>,
}

impl LogDescriptor {
    /// Checks the suite parameters (for akd: a valid, non-small-order VRF public key).
    pub fn new(
        suite: Suite,
        signing_key: LogSigningKey,
        suite_params: Vec<u8>,
    ) -> Result<Self, DecodeError> {
        match suite {
            Suite::AkdBlake3Ecvrf25519V1 => akd_backend::check_suite_params(&suite_params)?,
        }
        Ok(Self {
            suite,
            signing_key,
            suite_params,
        })
    }

    pub fn suite(&self) -> Suite {
        self.suite
    }

    pub fn signing_key(&self) -> &LogSigningKey {
        &self.signing_key
    }

    pub fn suite_params(&self) -> &[u8] {
        &self.suite_params
    }

    pub fn log_id(&self) -> LogId {
        let mut h = Sha256::new();
        h.update(LOG_ID_CONTEXT);
        h.update(self.encode());
        LogId(h.finalize().into())
    }

    /// `version=1, type=1, u8 suite, u8 signature_scheme,
    /// opaque signing_key<0..64> (u8 length), opaque suite_params<0..256> (u16 length)`.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.header(MSG_DESCRIPTOR);
        w.u8(self.suite as u8);
        w.u8(self.signing_key.scheme as u8);
        // Lengths are fixed by the scheme and suite, both checked on construction.
        let _: Result<(), EncodeError> =
            w.bytes_u8("signing_key", &self.signing_key.to_bytes(), MAX_SIGNING_KEY);
        let _: Result<(), EncodeError> =
            w.bytes_u16("suite_params", &self.suite_params, MAX_SUITE_PARAMS);
        w.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes, MAX_DESCRIPTOR_BYTES)?;
        r.header(MSG_DESCRIPTOR)?;
        let suite = Suite::from_u8(r.u8("suite")?)?;
        let scheme = SignatureScheme::from_u8(r.u8("signature_scheme")?)?;
        let key = r.bytes_u8("signing_key", MAX_SIGNING_KEY)?;
        let signing_key = match scheme {
            SignatureScheme::Ed25519 => <&[u8; 32]>::try_from(key)
                .ok()
                .and_then(LogSigningKey::ed25519)
                .ok_or(DecodeError::InvalidValue {
                    field: "signing_key",
                })?,
        };
        let params = r.bytes_u16("suite_params", MAX_SUITE_PARAMS)?.to_vec();
        r.finish()?;
        Self::new(suite, signing_key, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_canonical_signatures_are_rejected() {
        let signer = Ed25519HeadSigner::from_seed(&[3; 32]);
        let key = signer.public_key();
        let sig = signer.sign(b"head").unwrap();
        assert!(key.verify(b"head", &sig));
        // s + ℓ verifies under legacy rules but is a second encoding.
        let mut malleated = sig.clone();
        let mut carry = 0u16;
        for i in 0..32 {
            let sum = u16::from(malleated[32 + i]) + u16::from(ED25519_ORDER[i]) + carry;
            malleated[32 + i] = sum as u8;
            carry = sum >> 8;
        }
        assert_ne!(malleated, sig);
        assert!(!key.verify(b"head", &malleated));
        assert!(!key.verify(b"head", &sig[..63]));
        assert!(!key.verify(b"other", &sig));
    }

    #[test]
    fn scalar_bound_is_exact() {
        assert!(!scalar_is_canonical(&ED25519_ORDER));
        let mut below = ED25519_ORDER;
        below[0] -= 1;
        assert!(scalar_is_canonical(&below));
        assert!(scalar_is_canonical(&[0; 32]));
    }

    #[test]
    fn weak_keys_are_refused() {
        // The identity point.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert!(LogSigningKey::ed25519(&identity).is_none());
    }
}
