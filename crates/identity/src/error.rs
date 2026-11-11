// SPDX-License-Identifier: EUPL-1.2
//! The one error type of the crate.

/// Why a structure was rejected or an operation failed.
///
/// Parsing errors (`Truncated` … `InvalidPublicKey`) say what was wrong with the bytes;
/// verification errors say which check failed. None of them carries secret data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("input ended before the structure was complete")]
    Truncated,
    #[error("trailing bytes after the structure")]
    TrailingBytes,
    #[error("input longer than the format allows")]
    TooLong,
    #[error("unexpected structure kind {0:#04x}")]
    WrongKind(u8),
    #[error("unsupported format version {0}")]
    UnsupportedVersion(u8),
    #[error("unsupported algorithm {0:#06x}")]
    UnsupportedAlgorithm(u16),
    #[error("malformed field: {0}")]
    Malformed(&'static str),
    #[error("invalid public key")]
    InvalidPublicKey,
    #[error("signature verification failed")]
    BadSignature,
    #[error("issued for another identity")]
    WrongIdentity,
    #[error("issued under device-signing generation {found}, expected {expected}")]
    WrongGeneration { expected: u32, found: u32 },
    #[error("not valid yet")]
    NotYetValid,
    #[error("expired")]
    Expired,
    #[error("device revoked")]
    Revoked,
    #[error("superseded by a newer key epoch of the same device")]
    Superseded,
    #[error("two different certificates for the same device and key epoch")]
    Conflict,
    #[error("the credential does not certify this key")]
    KeyMismatch,
    #[error("device not in the identity's device list")]
    UnknownDevice,
    #[error("decryption failed: wrong key, or the data was modified")]
    DecryptionFailed,
    #[error("device linking failed: wrong code, or the messages were modified")]
    LinkAuthentication,
    #[error("checksum mismatch")]
    Checksum,
    #[error("operating system randomness unavailable")]
    Randomness,
    #[error("cryptographic operation failed")]
    Crypto,
}
