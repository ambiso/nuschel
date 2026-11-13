// SPDX-License-Identifier: EUPL-1.2
//! Key types: algorithm identifiers, public keys with their encodings, and the secret
//! halves held by the identity root, the device-signing key and each device.
//!
//! ```text
//! enum { ed25519(0x0807), mldsa65_ed25519(0xFE01), (0xFFFF) } SignatureAlgorithm;
//! enum { xwing(0x647A), (0xFFFF) } KemAlgorithm;
//!
//! struct {
//!   SignatureAlgorithm algorithm;        // mldsa65_ed25519
//!   opaque mldsa65[1952];                // FIPS 204 public key
//!   opaque ed25519[32];                  // RFC 8032 public key
//! } HybridPublicKey;                     // 1986 bytes
//!
//! opaque HybridSignature[3373];          // ML-DSA-65 signature (3309) || Ed25519 (64)
//! ```
//!
//! `0x0807` is the TLS/MLS SignatureScheme value of Ed25519, so a device's leaf algorithm
//! compares directly with its MLS leaf node; `0x647A` is the HPKE KEM id of X-Wing
//! (`MLKEM768-X25519`). The composite has no registered code point, so it takes one from
//! the private-use range. The key, not the signature, decides which algorithm applies:
//! a hybrid key accepts only a composite signature, with both halves valid.

use std::fmt;

use zeroize::Zeroizing;

use crate::codec::{Reader, Writer, label};
use crate::crypto::{self, Secret32, composite, ed25519, mldsa65, xwing};
use crate::{Error, hpke};

/// Domain-separation label of [`HybridPublicKey::fingerprint`].
pub(crate) const FINGERPRINT_LABEL: &str = "nuschl/identity-fingerprint/v1";

/// A signature algorithm identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SignatureAlgorithm {
    /// Ed25519 (RFC 8032): MLS leaf keys and the credential key.
    Ed25519,
    /// Composite ML-DSA-65 + Ed25519, `id-MLDSA65-Ed25519-SHA512` of RFC 10073: the
    /// identity root and device-signing keys.
    MlDsa65Ed25519,
}

impl SignatureAlgorithm {
    pub const fn code(self) -> u16 {
        match self {
            Self::Ed25519 => 0x0807,
            Self::MlDsa65Ed25519 => 0xFE01,
        }
    }

    pub fn from_code(code: u16) -> Result<Self, Error> {
        match code {
            0x0807 => Ok(Self::Ed25519),
            0xFE01 => Ok(Self::MlDsa65Ed25519),
            other => Err(Error::UnsupportedAlgorithm(other)),
        }
    }

    /// Reads an algorithm id that must be exactly `expected`.
    pub(crate) fn expect(reader: &mut Reader<'_>, expected: Self) -> Result<(), Error> {
        let code = reader.u16()?;
        match Self::from_code(code)? {
            found if found == expected => Ok(()),
            _ => Err(Error::UnsupportedAlgorithm(code)),
        }
    }
}

/// A key encapsulation mechanism identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KemAlgorithm {
    /// X-Wing (ML-KEM-768 + X25519).
    XWing,
}

impl KemAlgorithm {
    pub const fn code(self) -> u16 {
        match self {
            Self::XWing => 0x647A,
        }
    }

    pub(crate) fn expect(reader: &mut Reader<'_>, expected: Self) -> Result<(), Error> {
        let code = reader.u16()?;
        if code == expected.code() {
            Ok(())
        } else {
            Err(Error::UnsupportedAlgorithm(code))
        }
    }
}

/// The identity: SHA-256 over a label and the identity root's [`HybridPublicKey`]
/// encoding. Pins, safety numbers, credentials and key transparency name it.
///
/// ```text
/// identity = SHA-256(uint8(30) || "nuschl/identity-fingerprint/v1" || HybridPublicKey)
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IdentityFingerprint([u8; 32]);

impl IdentityFingerprint {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for IdentityFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IdentityFingerprint({})", self.to_hex())
    }
}

impl fmt::Display for IdentityFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A device's 128-bit random identifier. It stays the same when the device's keys
/// rotate ([`DeviceKeys::rotate`]) and is never reused.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId([u8; 16]);

impl DeviceId {
    pub fn random() -> Result<Self, Error> {
        Ok(Self(crypto::random_array()?))
    }

    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({})", hex::encode(self.0))
    }
}

/// A validated Ed25519 public key: the canonical encoding of a point of prime order.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ed25519PublicKey([u8; 32]);

impl Ed25519PublicKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, Error> {
        ed25519::validate_public(&bytes)?;
        Ok(Self(bytes))
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub(crate) fn read(reader: &mut Reader<'_>) -> Result<Self, Error> {
        Self::from_bytes(reader.array()?)
    }

    pub(crate) fn verify(&self, message: &[u8], signature: &[u8; 64]) -> Result<(), Error> {
        ed25519::verify(&self.0, message, signature)
    }
}

impl fmt::Debug for Ed25519PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ed25519PublicKey({})", hex::encode(self.0))
    }
}

/// A validated X-Wing public key (1216 bytes): a device's encryption key.
#[derive(Clone, PartialEq, Eq)]
pub struct XWingPublicKey(Box<[u8; xwing::PUBLIC_KEY_LEN]>);

impl XWingPublicKey {
    pub const LEN: usize = xwing::PUBLIC_KEY_LEN;

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let key: Box<[u8; xwing::PUBLIC_KEY_LEN]> = bytes
            .to_vec()
            .into_boxed_slice()
            .try_into()
            .map_err(|_| Error::InvalidPublicKey)?;
        xwing::validate_public(&key)?;
        Ok(Self(key))
    }

    pub fn as_bytes(&self) -> &[u8; xwing::PUBLIC_KEY_LEN] {
        &self.0
    }

    pub(crate) fn read(reader: &mut Reader<'_>) -> Result<Self, Error> {
        Self::from_bytes(reader.take(xwing::PUBLIC_KEY_LEN)?)
    }
}

impl fmt::Debug for XWingPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digest = crypto::sha256(&[&self.0[..]]);
        write!(f, "XWingPublicKey(sha256:{})", hex::encode(&digest[..8]))
    }
}

/// The public key of the identity root or of a device-signing key.
#[derive(Clone, PartialEq, Eq)]
pub struct HybridPublicKey {
    mldsa65: Box<[u8; mldsa65::PUBLIC_KEY_LEN]>,
    ed25519: Ed25519PublicKey,
}

impl HybridPublicKey {
    /// Length of the encoding.
    pub const LEN: usize = 2 + composite::PUBLIC_KEY_LEN;

    pub fn ed25519(&self) -> &Ed25519PublicKey {
        &self.ed25519
    }

    pub fn mldsa65(&self) -> &[u8; mldsa65::PUBLIC_KEY_LEN] {
        &self.mldsa65
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        self.write(&mut writer);
        writer.into_vec()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes, Self::LEN)?;
        let key = Self::read(&mut reader)?;
        reader.finish()?;
        Ok(key)
    }

    pub(crate) fn write(&self, writer: &mut Writer) {
        writer.u16(SignatureAlgorithm::MlDsa65Ed25519.code());
        writer.bytes(&self.mldsa65[..]);
        writer.bytes(self.ed25519.as_bytes());
    }

    pub(crate) fn read(reader: &mut Reader<'_>) -> Result<Self, Error> {
        SignatureAlgorithm::expect(reader, SignatureAlgorithm::MlDsa65Ed25519)?;
        // Every 1952-byte string is a well-formed ML-DSA-65 public key (FIPS 204 pkDecode).
        let mldsa65 = reader.boxed()?;
        let ed25519 = Ed25519PublicKey::read(reader)?;
        Ok(Self { mldsa65, ed25519 })
    }

    /// SHA-256 over the label and the encoding. For an identity root this is the
    /// identity ([`IdentityFingerprint`]).
    pub fn fingerprint(&self) -> IdentityFingerprint {
        IdentityFingerprint(crypto::sha256(&[&label(FINGERPRINT_LABEL), &self.encode()]))
    }

    pub(crate) fn verify(
        &self,
        ctx: &str,
        message: &[u8],
        signature: &HybridSignature,
    ) -> Result<(), Error> {
        composite::verify(
            &self.mldsa65,
            self.ed25519.as_bytes(),
            ctx.as_bytes(),
            message,
            &signature.0,
        )
    }
}

impl fmt::Debug for HybridPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HybridPublicKey({})", self.fingerprint())
    }
}

/// A composite signature: ML-DSA-65 (3309 bytes) followed by Ed25519 (64 bytes).
#[derive(Clone, PartialEq, Eq)]
pub struct HybridSignature(pub(crate) Box<[u8; composite::SIGNATURE_LEN]>);

impl HybridSignature {
    pub const LEN: usize = composite::SIGNATURE_LEN;

    pub fn as_bytes(&self) -> &[u8; composite::SIGNATURE_LEN] {
        &self.0
    }

    pub(crate) fn read(reader: &mut Reader<'_>) -> Result<Self, Error> {
        Ok(Self(reader.boxed()?))
    }
}

impl fmt::Debug for HybridSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HybridSignature({} bytes)", self.0.len())
    }
}

/// The private half of a hybrid key: the draft's composite private key
/// `mldsa65_seed[32] || ed25519_seed[32]`.
pub(crate) struct HybridSecret {
    mldsa65: Secret32,
    ed25519: Secret32,
}

impl HybridSecret {
    pub const LEN: usize = 64;

    pub fn generate() -> Result<Self, Error> {
        Ok(Self {
            mldsa65: Secret32::random()?,
            ed25519: Secret32::random()?,
        })
    }

    pub fn from_seeds(mldsa65: Secret32, ed25519: Secret32) -> Self {
        Self { mldsa65, ed25519 }
    }

    pub fn public(&self) -> HybridPublicKey {
        HybridPublicKey {
            mldsa65: mldsa65::public_from_seed(self.mldsa65.expose()),
            ed25519: Ed25519PublicKey(ed25519::public_from_seed(self.ed25519.expose())),
        }
    }

    /// Signing with the given ML-DSA randomness: callers pass fresh random bytes (hedged
    /// signing, the FIPS 204 default); all zeros is FIPS 204's deterministic variant,
    /// used for the test vectors.
    pub fn sign_with(
        &self,
        ctx: &str,
        message: &[u8],
        randomness: [u8; 32],
    ) -> Result<HybridSignature, Error> {
        composite::sign(
            self.mldsa65.expose(),
            self.ed25519.expose(),
            ctx.as_bytes(),
            message,
            randomness,
        )
        .map(HybridSignature)
    }

    #[cfg(test)]
    pub fn seeds(&self) -> (&[u8; 32], &[u8; 32]) {
        (self.mldsa65.expose(), self.ed25519.expose())
    }

    pub fn write(&self, writer: &mut Writer) {
        writer.bytes(self.mldsa65.expose());
        writer.bytes(self.ed25519.expose());
    }

    pub fn read(reader: &mut Reader<'_>) -> Result<Self, Error> {
        Ok(Self {
            mldsa65: Secret32::from_slice(reader.take(32)?)?,
            ed25519: Secret32::from_slice(reader.take(32)?)?,
        })
    }

    pub fn duplicate(&self) -> Self {
        Self {
            mldsa65: self.mldsa65.duplicate(),
            ed25519: self.ed25519.duplicate(),
        }
    }
}

/// A device name shown to the user's contacts: UTF-8, at most 64 bytes, without
/// control or bidirectional formatting characters.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct DisplayName(String);

impl DisplayName {
    pub const MAX_LEN: usize = 64;

    pub fn new(name: &str) -> Result<Self, Error> {
        if name.len() > Self::MAX_LEN {
            return Err(Error::Malformed("display name longer than 64 bytes"));
        }
        let forbidden = |c: char| {
            c.is_control()
                || matches!(c, '\u{200E}' | '\u{200F}' | '\u{061C}')
                || ('\u{202A}'..='\u{202E}').contains(&c)
                || ('\u{2066}'..='\u{2069}').contains(&c)
        };
        if name.chars().any(forbidden) {
            return Err(Error::Malformed(
                "display name contains a control character",
            ));
        }
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn write(&self, writer: &mut Writer) {
        writer.opaque8(self.0.as_bytes());
    }

    pub(crate) fn read(reader: &mut Reader<'_>) -> Result<Self, Error> {
        let bytes = reader.opaque8(0, Self::MAX_LEN)?;
        let name = std::str::from_utf8(bytes).map_err(|_| Error::Malformed("display name"))?;
        Self::new(name)
    }
}

impl fmt::Debug for DisplayName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DisplayName({:?})", self.0)
    }
}

/// The identity root key pair. It names the person on every server and signs only
/// device-signing certificates, so it is decrypted rarely: at setup, to rotate the
/// device-signing key, and to recover (docs/e2ee.md §3.5).
pub struct IdentityRoot {
    pub(crate) secret: HybridSecret,
    public: HybridPublicKey,
    fingerprint: IdentityFingerprint,
}

impl IdentityRoot {
    pub fn generate() -> Result<Self, Error> {
        Ok(Self::from_secret(HybridSecret::generate()?))
    }

    pub(crate) fn from_secret(secret: HybridSecret) -> Self {
        let public = secret.public();
        let fingerprint = public.fingerprint();
        Self {
            secret,
            public,
            fingerprint,
        }
    }

    pub fn public_key(&self) -> &HybridPublicKey {
        &self.public
    }

    pub fn fingerprint(&self) -> IdentityFingerprint {
        self.fingerprint
    }
}

impl fmt::Debug for IdentityRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IdentityRoot({})", self.fingerprint)
    }
}

/// A device-signing key: the hybrid key that certifies devices and signs revocations,
/// and the Ed25519 credential key that signs only compact device credentials (the
/// composite draft forbids using a component key outside the composite, so the
/// credential key is a key of its own, certified next to the hybrid key).
pub struct DeviceSigningKey {
    pub(crate) secret: HybridSecret,
    pub(crate) credential_seed: Secret32,
    public: HybridPublicKey,
    credential_public: Ed25519PublicKey,
}

impl DeviceSigningKey {
    /// Length of the secret encoding: hybrid secret (64) and credential seed (32).
    pub(crate) const SECRET_LEN: usize = HybridSecret::LEN + 32;

    pub fn generate() -> Result<Self, Error> {
        Ok(Self::from_secrets(
            HybridSecret::generate()?,
            Secret32::random()?,
        ))
    }

    pub(crate) fn from_secrets(secret: HybridSecret, credential_seed: Secret32) -> Self {
        let public = secret.public();
        let credential_public =
            Ed25519PublicKey(ed25519::public_from_seed(credential_seed.expose()));
        Self {
            secret,
            credential_seed,
            public,
            credential_public,
        }
    }

    pub fn public_key(&self) -> &HybridPublicKey {
        &self.public
    }

    pub fn credential_key(&self) -> &Ed25519PublicKey {
        &self.credential_public
    }

    pub(crate) fn write_secret(&self, writer: &mut Writer) {
        self.secret.write(writer);
        writer.bytes(self.credential_seed.expose());
    }

    pub(crate) fn read_secret(reader: &mut Reader<'_>) -> Result<Self, Error> {
        let secret = HybridSecret::read(reader)?;
        let credential_seed = Secret32::from_slice(reader.take(32)?)?;
        Ok(Self::from_secrets(secret, credential_seed))
    }

    pub(crate) fn duplicate(&self) -> Self {
        Self::from_secrets(self.secret.duplicate(), self.credential_seed.duplicate())
    }
}

impl fmt::Debug for DeviceSigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceSigningKey({})", self.public.fingerprint())
    }
}

/// A device's public keys, as certified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevicePublicKeys {
    pub device_id: DeviceId,
    /// Incremented every time the device's keys rotate; starts at 1.
    pub key_epoch: u32,
    /// The MLS leaf signature key.
    pub leaf_key: Ed25519PublicKey,
    /// The device's X-Wing key for linking and key delivery outside MLS.
    pub kem_key: XWingPublicKey,
}

/// One device's own keys; they never leave the device.
pub struct DeviceKeys {
    device_id: DeviceId,
    key_epoch: u32,
    pub(crate) leaf_seed: Secret32,
    pub(crate) kem_seed: Secret32,
    leaf_public: Ed25519PublicKey,
    kem_public: XWingPublicKey,
}

impl DeviceKeys {
    /// Length of the secret encoding (see the keystore).
    pub(crate) const SECRET_LEN: usize = 16 + 4 + 2 + 32 + 2 + 32;

    /// A new device: random id, key epoch 1.
    pub fn generate() -> Result<Self, Error> {
        Self::from_seeds(
            DeviceId::random()?,
            1,
            Secret32::random()?,
            Secret32::random()?,
        )
    }

    /// Fresh keys for the same device, one key epoch later (docs/e2ee.md §3.6).
    pub fn rotate(&self) -> Result<Self, Error> {
        let key_epoch = self
            .key_epoch
            .checked_add(1)
            .ok_or(Error::Malformed("key epoch exhausted"))?;
        Self::from_seeds(
            self.device_id,
            key_epoch,
            Secret32::random()?,
            Secret32::random()?,
        )
    }

    pub(crate) fn from_seeds(
        device_id: DeviceId,
        key_epoch: u32,
        leaf_seed: Secret32,
        kem_seed: Secret32,
    ) -> Result<Self, Error> {
        if key_epoch == 0 {
            return Err(Error::Malformed("key epoch 0"));
        }
        let leaf_public = Ed25519PublicKey(ed25519::public_from_seed(leaf_seed.expose()));
        let kem_public = XWingPublicKey(xwing::public_from_seed(kem_seed.expose())?);
        Ok(Self {
            device_id,
            key_epoch,
            leaf_seed,
            kem_seed,
            leaf_public,
            kem_public,
        })
    }

    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    pub fn key_epoch(&self) -> u32 {
        self.key_epoch
    }

    pub fn leaf_key(&self) -> &Ed25519PublicKey {
        &self.leaf_public
    }

    pub fn kem_key(&self) -> &XWingPublicKey {
        &self.kem_public
    }

    pub fn public_keys(&self) -> DevicePublicKeys {
        DevicePublicKeys {
            device_id: self.device_id,
            key_epoch: self.key_epoch,
            leaf_key: self.leaf_public,
            kem_key: self.kem_public.clone(),
        }
    }

    /// An Ed25519 signature with the MLS leaf key over `payload`, which MLS has already
    /// labelled (`SignWithLabel`): what an OpenMLS `Signer` returns.
    pub fn sign_leaf(&self, payload: &[u8]) -> Result<[u8; 64], Error> {
        ed25519::sign(self.leaf_seed.expose(), payload)
    }

    /// Opens a [`SealedToDevice`](crate::SealedToDevice) addressed to these keys.
    pub fn open(
        &self,
        sealed: &crate::SealedToDevice,
        info: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, Error> {
        sealed.open(self, info)
    }

    pub(crate) fn open_hpke(
        &self,
        enc: &[u8; xwing::CIPHERTEXT_LEN],
        info: &[u8],
        ciphertext: &[u8],
        psk: Option<(&[u8; 32], &[u8])>,
    ) -> Result<Zeroizing<Vec<u8>>, Error> {
        hpke::open(&self.kem_seed, enc, info, ciphertext, psk)
    }

    pub(crate) fn write_secret(&self, writer: &mut Writer) {
        writer.bytes(self.device_id.as_bytes());
        writer.u32(self.key_epoch);
        writer.u16(SignatureAlgorithm::Ed25519.code());
        writer.bytes(self.leaf_seed.expose());
        writer.u16(KemAlgorithm::XWing.code());
        writer.bytes(self.kem_seed.expose());
    }

    pub(crate) fn read_secret(reader: &mut Reader<'_>) -> Result<Self, Error> {
        let device_id = DeviceId(reader.array()?);
        let key_epoch = reader.u32()?;
        SignatureAlgorithm::expect(reader, SignatureAlgorithm::Ed25519)?;
        let leaf_seed = Secret32::from_slice(reader.take(32)?)?;
        KemAlgorithm::expect(reader, KemAlgorithm::XWing)?;
        let kem_seed = Secret32::from_slice(reader.take(32)?)?;
        Self::from_seeds(device_id, key_epoch, leaf_seed, kem_seed)
    }
}

impl fmt::Debug for DeviceKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceKeys")
            .field("device_id", &self.device_id)
            .field("key_epoch", &self.key_epoch)
            .field("leaf_key", &self.leaf_public)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hybrid_public_key_round_trips_and_is_strict() {
        let root = IdentityRoot::generate().unwrap();
        let bytes = root.public_key().encode();
        assert_eq!(bytes.len(), HybridPublicKey::LEN);
        assert_eq!(bytes.len(), 1986);
        assert_eq!(&HybridPublicKey::decode(&bytes).unwrap(), root.public_key());
        assert_eq!(
            HybridPublicKey::decode(&bytes[..bytes.len() - 1]).err(),
            Some(Error::Truncated)
        );
        let mut long = bytes.clone();
        long.push(0);
        assert_eq!(HybridPublicKey::decode(&long).err(), Some(Error::TooLong));
        let mut wrong_alg = bytes.clone();
        wrong_alg[1] = 0x07;
        wrong_alg[0] = 0x08;
        assert_eq!(
            HybridPublicKey::decode(&wrong_alg).err(),
            Some(Error::UnsupportedAlgorithm(0x0807))
        );
        // A small-order Ed25519 half is rejected.
        let mut weak = bytes;
        weak[2 + 1952..].copy_from_slice(&[0u8; 32]);
        assert_eq!(
            HybridPublicKey::decode(&weak).err(),
            Some(Error::InvalidPublicKey)
        );
    }

    #[test]
    fn fingerprints_differ_per_key_and_are_stable() {
        let a = IdentityRoot::generate().unwrap();
        let b = IdentityRoot::generate().unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint(), a.public_key().fingerprint());
    }

    #[test]
    fn display_names_are_restricted() {
        DisplayName::new("Alice's laptop").unwrap();
        DisplayName::new("").unwrap();
        DisplayName::new(&"é".repeat(32)).unwrap();
        assert!(DisplayName::new(&"é".repeat(33)).is_err());
        assert!(DisplayName::new("a\nb").is_err());
        assert!(DisplayName::new("a\u{202E}b").is_err());
        assert!(DisplayName::new("a\u{2067}b").is_err());
    }

    #[test]
    fn device_key_rotation_keeps_the_id() {
        let keys = DeviceKeys::generate().unwrap();
        assert_eq!(keys.key_epoch(), 1);
        let next = keys.rotate().unwrap();
        assert_eq!(next.device_id(), keys.device_id());
        assert_eq!(next.key_epoch(), 2);
        assert_ne!(next.leaf_key(), keys.leaf_key());
        assert_ne!(next.kem_key(), keys.kem_key());
    }

    #[test]
    fn debug_output_never_contains_secrets() {
        let keys = DeviceKeys::generate().unwrap();
        let printed = format!("{keys:?}");
        let seed_hex = hex::encode(keys.leaf_seed.expose());
        assert!(!printed.contains(&seed_hex));
        assert!(format!("{:?}", keys.kem_seed).contains("redacted"));
        let root = IdentityRoot::generate().unwrap();
        assert!(!format!("{root:?}").contains(&hex::encode(root.secret.mldsa65.expose())));
    }
}
