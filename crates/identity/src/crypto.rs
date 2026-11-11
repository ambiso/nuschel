// SPDX-License-Identifier: EUPL-1.2
//! Thin wrappers over the primitives: libcrux for every cryptographic operation, and
//! curve25519-dalek only to validate Edwards points (it is already in the tree
//! through `spake2`).
//!
//! libcrux takes keys by value and keeps expanded keys in its own locals, which it does
//! not zeroize. Seeds and keys owned by this crate are zeroized; copies inside the
//! primitives are a known residue that the crypto worker's process isolation has to
//! cover.

use std::fmt;

use curve25519_dalek::edwards::CompressedEdwardsY;
use libcrux_sha2::Digest as _;
use subtle::ConstantTimeEq as _;
use zeroize::{Zeroize, Zeroizing};

use crate::Error;

/// Fills `buf` from the operating system's random number generator.
pub(crate) fn fill_random(buf: &mut [u8]) -> Result<(), Error> {
    getrandom::fill(buf).map_err(|_| Error::Randomness)
}

pub(crate) fn random_array<const N: usize>() -> Result<[u8; N], Error> {
    let mut out = [0u8; N];
    fill_random(&mut out)?;
    Ok(out)
}

/// 32 secret bytes in one heap allocation: zeroized on drop, never printed, copied only
/// on purpose ([`Secret32::duplicate`]).
pub(crate) struct Secret32(Box<[u8; 32]>);

impl Secret32 {
    pub fn random() -> Result<Self, Error> {
        let mut secret = Self(Box::new([0u8; 32]));
        fill_random(&mut secret.0[..])?;
        Ok(secret)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, Error> {
        let mut secret = Self(Box::new([0u8; 32]));
        if bytes.len() != 32 {
            return Err(Error::Malformed("secret length"));
        }
        secret.0.copy_from_slice(bytes);
        Ok(secret)
    }

    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn duplicate(&self) -> Self {
        Self(Box::new(*self.0))
    }
}

impl Drop for Secret32 {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Secret32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret32(redacted)")
    }
}

/// Constant-time equality of two byte strings (the lengths are not secret).
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

pub(crate) fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = libcrux_sha2::Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    let mut out = [0u8; 32];
    hasher.finish(&mut out);
    out
}

pub(crate) fn sha512(parts: &[&[u8]]) -> [u8; 64] {
    let mut hasher = libcrux_sha2::Sha512::new();
    for part in parts {
        hasher.update(part);
    }
    let mut out = [0u8; 64];
    hasher.finish(&mut out);
    out
}

/// HKDF-SHA-256 extract (RFC 5869).
pub(crate) fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut prk = Zeroizing::new([0u8; 32]);
    libcrux_hkdf::extract(libcrux_hkdf::Algorithm::Sha256, &mut prk[..], salt, ikm)
        .expect("inputs are far below the HKDF limits");
    prk
}

/// HKDF-SHA-256 expand to 32 bytes.
pub(crate) fn hkdf_expand(prk: &[u8; 32], info: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut okm = Zeroizing::new([0u8; 32]);
    libcrux_hkdf::expand(libcrux_hkdf::Algorithm::Sha256, &mut okm[..], prk, info)
        .expect("inputs are far below the HKDF limits");
    okm
}

pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut tag = [0u8; 32];
    libcrux_hmac::hmac(libcrux_hmac::Algorithm::Sha256, key, data, &mut tag);
    tag
}

/// ChaCha20-Poly1305 under a key that is used for exactly one message, so the nonce is
/// fixed at zero. Callers derive the key from a fresh random salt for every message.
pub(crate) fn seal_once(key: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; plaintext.len() + libcrux_chacha20poly1305::TAG_LEN];
    libcrux_chacha20poly1305::encrypt(key, plaintext, &mut out, aad, &[0u8; 12])
        .expect("buffer sized for plaintext and tag; inputs below 4 GiB");
    out
}

pub(crate) fn open_once(
    key: &[u8; 32],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Zeroizing<Vec<u8>>, Error> {
    let len = ciphertext
        .len()
        .checked_sub(libcrux_chacha20poly1305::TAG_LEN)
        .ok_or(Error::DecryptionFailed)?;
    let mut plaintext = Zeroizing::new(vec![0u8; len]);
    libcrux_chacha20poly1305::decrypt(key, &mut plaintext, ciphertext, aad, &[0u8; 12])
        .map_err(|_| Error::DecryptionFailed)?;
    Ok(plaintext)
}

/// Decompresses an Edwards point only if its encoding is the canonical one.
fn canonical_point(bytes: &[u8; 32]) -> Option<curve25519_dalek::EdwardsPoint> {
    let point = CompressedEdwardsY(*bytes).decompress()?;
    (point.compress().as_bytes() == bytes).then_some(point)
}

/// A SPAKE2 element from the peer: canonical, in the prime-order subgroup, not the
/// identity. (`spake2` 0.4 only decompresses.)
pub(crate) fn valid_spake2_element(bytes: &[u8; 32]) -> bool {
    canonical_point(bytes).is_some_and(|p| p.is_torsion_free() && !p.is_small_order())
}

pub(crate) mod ed25519 {
    use super::*;

    pub const PUBLIC_KEY_LEN: usize = 32;
    pub const SIGNATURE_LEN: usize = 64;

    pub fn public_from_seed(seed: &[u8; 32]) -> [u8; 32] {
        let mut public = [0u8; 32];
        libcrux_ed25519::secret_to_public(&mut public, seed);
        public
    }

    /// Accepts only canonical encodings of points of prime order: no small-order or
    /// mixed-order keys, so that cofactored and cofactorless verifiers agree and no
    /// signature verifies under two keys.
    pub fn validate_public(public: &[u8; 32]) -> Result<(), Error> {
        match canonical_point(public) {
            Some(point) if point.is_torsion_free() && !point.is_small_order() => Ok(()),
            _ => Err(Error::InvalidPublicKey),
        }
    }

    pub fn sign(seed: &[u8; 32], message: &[u8]) -> Result<[u8; 64], Error> {
        libcrux_ed25519::sign(message, seed).map_err(|_| Error::Crypto)
    }

    /// RFC 8032 verification (libcrux/HACL*, which rejects S ≥ L) plus a canonical R,
    /// so that signatures are not malleable. `public` must have passed
    /// [`validate_public`].
    pub fn verify(public: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> Result<(), Error> {
        let r: &[u8; 32] = signature[..32].try_into().expect("64 bytes");
        if canonical_point(r).is_none() {
            return Err(Error::BadSignature);
        }
        libcrux_ed25519::verify(message, public, signature).map_err(|_| Error::BadSignature)
    }
}

pub(crate) mod mldsa65 {
    use libcrux_ml_dsa::ml_dsa_65;

    use super::*;

    pub const PUBLIC_KEY_LEN: usize = 1952;
    pub const SIGNATURE_LEN: usize = 3309;

    /// An expanded key pair whose signing key is zeroized on drop.
    struct KeyPair(ml_dsa_65::MLDSA65KeyPair);

    impl KeyPair {
        /// FIPS 204 ML-DSA.KeyGen_internal(ξ) from the 32-byte seed.
        fn from_seed(seed: &[u8; 32]) -> Self {
            Self(ml_dsa_65::generate_key_pair(*seed))
        }
    }

    impl Drop for KeyPair {
        fn drop(&mut self) {
            self.0.signing_key.as_mut_slice().zeroize();
        }
    }

    pub fn public_from_seed(seed: &[u8; 32]) -> Box<[u8; PUBLIC_KEY_LEN]> {
        Box::new(*KeyPair::from_seed(seed).0.verification_key.as_ref())
    }

    /// FIPS 204 ML-DSA.Sign with context `ctx` and the given 32 bytes of signing
    /// randomness (random for hedged signing, zero for the deterministic variant).
    pub fn sign(
        seed: &[u8; 32],
        message: &[u8],
        ctx: &[u8],
        randomness: [u8; 32],
    ) -> Result<Box<[u8; SIGNATURE_LEN]>, Error> {
        let pair = KeyPair::from_seed(seed);
        let signature = ml_dsa_65::sign(&pair.0.signing_key, message, ctx, randomness)
            .map_err(|_| Error::Crypto)?;
        Ok(Box::new(*signature.as_ref()))
    }

    pub fn verify(
        public: &[u8; PUBLIC_KEY_LEN],
        message: &[u8],
        ctx: &[u8],
        signature: &[u8; SIGNATURE_LEN],
    ) -> Result<(), Error> {
        let key = ml_dsa_65::MLDSA65VerificationKey::new(*public);
        let signature = ml_dsa_65::MLDSA65Signature::new(*signature);
        ml_dsa_65::verify(&key, message, ctx, &signature).map_err(|_| Error::BadSignature)
    }
}

/// Composite ML-DSA-65 + Ed25519, `id-MLDSA65-Ed25519-SHA512` (OID 1.3.6.1.5.5.7.6.48) of
/// RFC 10073 (formerly draft-ietf-lamps-pq-composite-sigs): both components sign
///
/// ```text
/// M' = Prefix || Label || uint8 len(ctx) || ctx || SHA-512(M)
/// ```
///
/// where the ML-DSA component also gets `Label` as its FIPS 204 context. A signature is
/// valid only if both components are; because both sign `M'`, which names the composite
/// algorithm, neither component verifies on its own as a signature over `M`.
pub(crate) mod composite {
    use super::*;

    pub const PREFIX: &[u8; 32] = b"CompositeAlgorithmSignatures2025";
    pub const LABEL: &[u8] = b"COMPSIG-MLDSA65-Ed25519-SHA512";
    pub const PUBLIC_KEY_LEN: usize = mldsa65::PUBLIC_KEY_LEN + ed25519::PUBLIC_KEY_LEN;
    pub const SIGNATURE_LEN: usize = mldsa65::SIGNATURE_LEN + ed25519::SIGNATURE_LEN;

    pub fn message_representative(ctx: &[u8], message: &[u8]) -> Vec<u8> {
        let ctx_len = u8::try_from(ctx.len()).expect("contexts are at most 255 bytes");
        let digest = sha512(&[message]);
        let mut out = Vec::with_capacity(PREFIX.len() + LABEL.len() + 1 + ctx.len() + 64);
        out.extend_from_slice(PREFIX);
        out.extend_from_slice(LABEL);
        out.push(ctx_len);
        out.extend_from_slice(ctx);
        out.extend_from_slice(&digest);
        out
    }

    /// Signs with the composite private key `mldsa_seed || ed25519_seed`.
    pub fn sign(
        mldsa_seed: &[u8; 32],
        ed25519_seed: &[u8; 32],
        ctx: &[u8],
        message: &[u8],
        mldsa_randomness: [u8; 32],
    ) -> Result<Box<[u8; SIGNATURE_LEN]>, Error> {
        let representative = message_representative(ctx, message);
        let mldsa = mldsa65::sign(mldsa_seed, &representative, LABEL, mldsa_randomness)?;
        let ed = ed25519::sign(ed25519_seed, &representative)?;
        let mut out = Box::new([0u8; SIGNATURE_LEN]);
        out[..mldsa65::SIGNATURE_LEN].copy_from_slice(&mldsa[..]);
        out[mldsa65::SIGNATURE_LEN..].copy_from_slice(&ed);
        Ok(out)
    }

    /// Both components are checked; the result is valid only if both verify.
    pub fn verify(
        mldsa_public: &[u8; mldsa65::PUBLIC_KEY_LEN],
        ed25519_public: &[u8; 32],
        ctx: &[u8],
        message: &[u8],
        signature: &[u8; SIGNATURE_LEN],
    ) -> Result<(), Error> {
        let representative = message_representative(ctx, message);
        let (mldsa_sig, ed_sig) = signature.split_at(mldsa65::SIGNATURE_LEN);
        let mldsa = mldsa65::verify(
            mldsa_public,
            &representative,
            LABEL,
            mldsa_sig.try_into().expect("split at the ML-DSA length"),
        );
        let ed = ed25519::verify(
            ed25519_public,
            &representative,
            ed_sig.try_into().expect("64 bytes"),
        );
        mldsa.and(ed)
    }
}

/// X-Wing (draft-connolly-cfrg-xwing-kem-06 and later; `MLKEM768-X25519` of
/// draft-irtf-cfrg-concrete-hybrid-kems): 32-byte seed, 1216-byte public key.
pub(crate) mod xwing {
    use super::*;

    pub const PUBLIC_KEY_LEN: usize = 1216;
    pub const CIPHERTEXT_LEN: usize = 1120;

    pub fn public_from_seed(seed: &[u8; 32]) -> Result<Box<[u8; PUBLIC_KEY_LEN]>, Error> {
        let (_private, public) =
            libcrux_kem::key_gen_derand(libcrux_kem::Algorithm::XWingKemDraft06, seed)
                .map_err(|_| Error::Crypto)?;
        public
            .encode()
            .into_boxed_slice()
            .try_into()
            .map_err(|_| Error::Crypto)
    }

    /// Rejects ML-KEM encapsulation keys that fail the FIPS 203 modulus check.
    pub fn validate_public(public: &[u8; PUBLIC_KEY_LEN]) -> Result<(), Error> {
        libcrux_kem::PublicKey::decode(libcrux_kem::Algorithm::XWingKemDraft06, public)
            .map(|_| ())
            .map_err(|_| Error::InvalidPublicKey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use curve25519_dalek::constants::{ED25519_BASEPOINT_POINT, EIGHT_TORSION};
    use curve25519_dalek::scalar::Scalar;

    #[test]
    fn ed25519_rejects_small_order_mixed_order_and_noncanonical_keys() {
        let good = ed25519::public_from_seed(&[1; 32]);
        ed25519::validate_public(&good).unwrap();

        // The eight points of small order, and each with its sign bit flipped.
        for point in EIGHT_TORSION {
            let mut bytes = point.compress().to_bytes();
            assert!(ed25519::validate_public(&bytes).is_err());
            bytes[31] ^= 0x80;
            assert!(ed25519::validate_public(&bytes).is_err());
        }
        // A valid key plus a torsion component: on the curve, but not of prime order.
        let a = ED25519_BASEPOINT_POINT * Scalar::from(12345u64);
        let mixed = (a + EIGHT_TORSION[1]).compress().to_bytes();
        assert!(ed25519::validate_public(&mixed).is_err());
        ed25519::validate_public(&a.compress().to_bytes()).unwrap();
        // Non-canonical encodings: y + p for small y.
        let p_plus_one = {
            let mut b = [0xffu8; 32];
            b[0] = 0xee;
            b[31] = 0x7f;
            b
        };
        assert!(ed25519::validate_public(&p_plus_one).is_err());
        // Not on the curve.
        let mut off = [0u8; 32];
        off[0] = 2;
        assert!(
            CompressedEdwardsY(off).decompress().is_none()
                || ed25519::validate_public(&off).is_err()
        );
    }

    #[test]
    fn ed25519_rejects_a_non_canonical_r() {
        let seed = [3u8; 32];
        let public = ed25519::public_from_seed(&seed);
        let signature = ed25519::sign(&seed, b"m").unwrap();
        ed25519::verify(&public, b"m", &signature).unwrap();
        let mut flipped = signature;
        flipped[0] ^= 1;
        assert_eq!(
            ed25519::verify(&public, b"m", &flipped),
            Err(Error::BadSignature)
        );
        // S + L is the textbook malleability; HACL* rejects S >= L.
        let s = Scalar::from_canonical_bytes(signature[32..].try_into().unwrap()).unwrap();
        let mut big = s.to_bytes();
        // Add the group order L to S (little-endian), which stays below 2^256.
        let order: [u8; 32] = [
            0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9,
            0xde, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
        ];
        let mut carry = 0u16;
        for (b, o) in big.iter_mut().zip(order) {
            let sum = u16::from(*b) + u16::from(o) + carry;
            *b = sum as u8;
            carry = sum >> 8;
        }
        let mut malleated = signature;
        malleated[32..].copy_from_slice(&big);
        assert_eq!(
            ed25519::verify(&public, b"m", &malleated),
            Err(Error::BadSignature)
        );
    }

    #[test]
    fn composite_needs_both_halves() {
        let (m, e) = ([5u8; 32], [6u8; 32]);
        let mldsa_public = mldsa65::public_from_seed(&m);
        let ed_public = ed25519::public_from_seed(&e);
        let signature = composite::sign(&m, &e, b"ctx", b"message", [0; 32]).unwrap();
        composite::verify(&mldsa_public, &ed_public, b"ctx", b"message", &signature).unwrap();
        assert!(
            composite::verify(&mldsa_public, &ed_public, b"ctx2", b"message", &signature).is_err()
        );
        assert!(
            composite::verify(&mldsa_public, &ed_public, b"ctx", b"messagf", &signature).is_err()
        );
        for index in [
            0,
            1000,
            mldsa65::SIGNATURE_LEN - 1,
            mldsa65::SIGNATURE_LEN,
            composite::SIGNATURE_LEN - 1,
        ] {
            let mut broken = signature.clone();
            broken[index] ^= 0x01;
            assert!(
                composite::verify(&mldsa_public, &ed_public, b"ctx", b"message", &broken).is_err(),
                "a flipped byte at {index} must fail"
            );
        }
        // A plain Ed25519 signature over the message is not the Ed25519 half.
        let mut spliced = signature.clone();
        spliced[mldsa65::SIGNATURE_LEN..].copy_from_slice(&ed25519::sign(&e, b"message").unwrap());
        assert!(
            composite::verify(&mldsa_public, &ed_public, b"ctx", b"message", &spliced).is_err()
        );
        // An ML-DSA half from a signature over another message does not fit either.
        let other = composite::sign(&m, &e, b"ctx", b"other", [0; 32]).unwrap();
        let mut spliced = signature.clone();
        spliced[..mldsa65::SIGNATURE_LEN].copy_from_slice(&other[..mldsa65::SIGNATURE_LEN]);
        assert!(
            composite::verify(&mldsa_public, &ed_public, b"ctx", b"message", &spliced).is_err()
        );
    }

    #[test]
    fn one_shot_aead_detects_any_change() {
        let key = [9u8; 32];
        let sealed = seal_once(&key, b"aad", b"plaintext");
        assert_eq!(&open_once(&key, b"aad", &sealed).unwrap()[..], b"plaintext");
        assert!(open_once(&key, b"aaD", &sealed).is_err());
        assert!(open_once(&[8; 32], b"aad", &sealed).is_err());
        for i in 0..sealed.len() {
            let mut broken = sealed.clone();
            broken[i] ^= 0x80;
            assert!(open_once(&key, b"aad", &broken).is_err());
        }
        assert!(open_once(&key, b"aad", &sealed[..15]).is_err());
    }

    #[test]
    fn spake2_elements_must_be_prime_order() {
        let good = (ED25519_BASEPOINT_POINT * Scalar::from(7u64))
            .compress()
            .to_bytes();
        assert!(valid_spake2_element(&good));
        for point in EIGHT_TORSION {
            assert!(!valid_spake2_element(&point.compress().to_bytes()));
        }
        let mixed = (ED25519_BASEPOINT_POINT * Scalar::from(7u64) + EIGHT_TORSION[3])
            .compress()
            .to_bytes();
        assert!(!valid_spake2_element(&mixed));
    }
}
