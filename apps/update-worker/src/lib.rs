// SPDX-License-Identifier: EUPL-1.2
#![forbid(unsafe_code)]
//! Cryptographic engines belong only in the isolated verifier executables and
//! explicit offline/CI signing tool, never linked into the UI/updater parent.

use curve25519_dalek::{edwards::CompressedEdwardsY, scalar::Scalar};
use nuschl_update::auth_format::{self, ML_CONTEXT, PublicKey, Signature, signed_bytes};

#[cfg(feature = "signer")]
pub mod key_file;
#[cfg(any(feature = "signer", test))]
pub mod signing;
pub use nuschl_update::trust;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid or noncanonical update signature")]
    Signature,
    #[error(transparent)]
    Format(#[from] auth_format::Error),
    #[error("invalid update key document")]
    KeyDocument,
    #[error("update key operation failed")]
    Io(#[from] std::io::Error),
    #[error("secure random generation failed")]
    Randomness,
    #[error("refusing to overwrite an existing update key file")]
    ExistingFile,
}

fn strong_point(bytes: &[u8; 32]) -> bool {
    CompressedEdwardsY(*bytes).decompress().is_some_and(|p| {
        p.compress().as_bytes() == bytes && p.is_torsion_free() && !p.is_small_order()
    })
}

/// Strong encoding checks precede dalek's strict verifier. In particular AKD
/// can unify legacy_compatibility for the whole binary: our explicit canonical
/// scalar check still rejects S >= the group order under that feature.
pub fn verify_ed25519(
    key: &PublicKey,
    signature: &Signature,
    manifest: &[u8],
) -> Result<(), Error> {
    signature.check_key(key)?;
    let encoded = signature.ed25519();
    let r: &[u8; 32] = encoded[..32].try_into().map_err(|_| Error::Signature)?;
    let s: [u8; 32] = encoded[32..].try_into().map_err(|_| Error::Signature)?;
    if !strong_point(key.ed25519())
        || !strong_point(r)
        || !bool::from(Scalar::from_canonical_bytes(s).is_some())
    {
        return Err(Error::Signature);
    }
    let public =
        ed25519_dalek::VerifyingKey::from_bytes(key.ed25519()).map_err(|_| Error::Signature)?;
    public
        .verify_strict(
            &signed_bytes(key, manifest)?,
            &ed25519_dalek::Signature::from_bytes(encoded),
        )
        .map_err(|_| Error::Signature)
}

pub fn verify_mldsa44(
    key: &PublicKey,
    signature: &Signature,
    manifest: &[u8],
) -> Result<(), Error> {
    signature.check_key(key)?;
    let encoded =
        ml_dsa::EncodedVerifyingKey::<ml_dsa::MlDsa44>::try_from(key.mldsa44().as_slice())
            .map_err(|_| Error::Signature)?;
    let public = ml_dsa::VerifyingKey::<ml_dsa::MlDsa44>::decode(&encoded);
    let signature = ml_dsa::Signature::<ml_dsa::MlDsa44>::try_from(signature.mldsa44().as_slice())
        .map_err(|_| Error::Signature)?;
    if public.verify_with_context(&signed_bytes(key, manifest)?, ML_CONTEXT, &signature) {
        Ok(())
    } else {
        Err(Error::Signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MESSAGE: &[u8] = br#"{"format":2,"version":"0.1.0"}"#;

    #[test]
    fn independent_components_reject_either_corruption_wrong_pair_and_raw_byte_changes() {
        let (key, signature) = signing::deterministic_fixture(&[1; 32], &[2; 32], MESSAGE).unwrap();
        verify_ed25519(&key, &signature, MESSAGE).unwrap();
        verify_mldsa44(&key, &signature, MESSAGE).unwrap();
        let mut ed = *signature.ed25519();
        ed[0] ^= 1;
        let bad = Signature::from_components(&key, &ed, signature.mldsa44()).unwrap();
        assert!(verify_ed25519(&key, &bad, MESSAGE).is_err());
        assert!(verify_mldsa44(&key, &bad, MESSAGE).is_ok());
        let mut ml = *signature.mldsa44();
        ml[0] ^= 1;
        let bad = Signature::from_components(&key, signature.ed25519(), &ml).unwrap();
        assert!(verify_ed25519(&key, &bad, MESSAGE).is_ok());
        assert!(verify_mldsa44(&key, &bad, MESSAGE).is_err());
        let (other, _) = signing::deterministic_fixture(&[3; 32], &[4; 32], MESSAGE).unwrap();
        assert!(verify_ed25519(&other, &signature, MESSAGE).is_err());
        assert!(verify_mldsa44(&other, &signature, MESSAGE).is_err());
        assert!(verify_ed25519(&key, &signature, b"other protocol bytes").is_err());
        assert!(verify_mldsa44(&key, &signature, b"other protocol bytes").is_err());
    }

    #[test]
    fn canonical_scalar_and_small_order_point_checks_do_not_depend_on_feature_unification() {
        let (key, signature) = signing::deterministic_fixture(&[1; 32], &[2; 32], MESSAGE).unwrap();
        let order: [u8; 32] = [
            0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9,
            0xde, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
        ];
        let mut ed = *signature.ed25519();
        ed[32..].copy_from_slice(&order);
        let bad = Signature::from_components(&key, &ed, signature.mldsa44()).unwrap();
        assert!(verify_ed25519(&key, &bad, MESSAGE).is_err());
        let mut encoded = *key.as_bytes();
        encoded[..32].fill(0);
        encoded[0] = 1;
        let weak = PublicKey::from_bytes(&encoded).unwrap();
        let weak_signature =
            Signature::from_components(&weak, signature.ed25519(), signature.mldsa44()).unwrap();
        assert!(verify_ed25519(&weak, &weak_signature, MESSAGE).is_err());
    }
}
