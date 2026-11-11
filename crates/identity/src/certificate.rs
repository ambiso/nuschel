// SPDX-License-Identifier: EUPL-1.2
//! Certificates (docs/e2ee.md §3.1, §3.3, §3.6).
//!
//! Every certificate is a body (starting with `kind` and `version`) followed by a
//! signature over the body's encoding. Hybrid signatures use the composite construction
//! with a per-structure context string as `ctx`; the compact credential is signed with
//! the device-signing key's Ed25519 credential key over
//! `uint8 len(ctx) || ctx || body`.
//!
//! ```text
//! struct {
//!   uint8  kind = 0x01; uint8 version = 1;
//!   opaque identity[32];                    // fingerprint of the issuing root
//!   uint32 generation;                      // >= 1, +1 on every rotation
//!   uint64 created_at;                      // Unix seconds, informational
//!   HybridPublicKey device_signing_key;     // 1986 bytes
//!   SignatureAlgorithm credential_algorithm;// ed25519
//!   opaque credential_key[32];              // signs compact credentials only
//!   opaque signature[3373];                 // root, ctx "nuschel/device-signing-certificate/v1"
//! } DeviceSigningCertificate;               // 5439 bytes
//!
//! struct {
//!   uint8  kind = 0x02; uint8 version = 1;
//!   opaque identity[32];
//!   uint32 signing_generation;              // generation of the issuing device-signing key
//!   opaque device_id[16];
//!   uint32 key_epoch;                       // >= 1, +1 when the device's keys rotate
//!   uint64 not_before;                      // Unix seconds, inclusive
//!   uint64 not_after;                       // Unix seconds, exclusive, > not_before
//!   SignatureAlgorithm leaf_algorithm;      // ed25519
//!   opaque leaf_key[32];                    // the MLS leaf signature key
//!   KemAlgorithm kem_algorithm;             // xwing
//!   opaque kem_key[1216];
//!   opaque display_name<0..64>;             // UTF-8, no control characters
//!   opaque signature[3373];                 // device-signing key, ctx "nuschel/device-certificate/v1"
//! } DeviceCertificate;                      // 4700 to 4764 bytes
//!
//! struct {
//!   uint8  kind = 0x03; uint8 version = 1;
//!   opaque identity[32];
//!   uint32 signing_generation;
//!   opaque device_id[16];
//!   uint32 key_epoch;
//!   uint64 not_before;
//!   uint64 not_after;
//!   SignatureAlgorithm leaf_algorithm;      // ed25519: decides the key length and that
//!   opaque leaf_key[32];                    //   the signature is Ed25519 by the credential key
//!   opaque signature[64];                   // ctx "nuschel/device-credential/v1"
//! } DeviceCredential;                       // 172 bytes: the MLS credential (type 0xF4E1)
//!
//! struct {
//!   uint8  kind = 0x04; uint8 version = 1;
//!   opaque identity[32];
//!   uint32 signing_generation;
//!   opaque device_id[16];
//!   uint32 through_key_epoch;               // revokes key epochs <= this; 0xFFFFFFFF: the device
//!   uint64 revoked_at;                      // Unix seconds, informational
//!   opaque signature[3373];                 // device-signing key, ctx "nuschel/device-revocation/v1"
//! } DeviceRevocation;                       // 3439 bytes
//!
//! struct {
//!   uint8  kind = 0x06; uint8 version = 1;
//!   opaque identity[32];
//!   uint32 signing_generation;
//!   opaque server[32];                      // the server's key-transparency log id
//!   opaque account[16];                     // the immutable account id on that server
//!   uint64 created_at;                      // Unix seconds, informational
//!   opaque signature[3373];                 // device-signing key, ctx "nuschel/account-statement/v1"
//! } AccountStatement;                       // 3467 bytes
//! ```

use std::fmt;

use crate::Error;
use crate::codec::{Reader, Writer, kind};
use crate::crypto::{self, ed25519};
use crate::keys::{
    DeviceId, DevicePublicKeys, DeviceSigningKey, DisplayName, Ed25519PublicKey, HybridPublicKey,
    HybridSignature, IdentityFingerprint, IdentityRoot, KemAlgorithm, SignatureAlgorithm,
    XWingPublicKey,
};
use crate::roster::VerifiedIdentity;

pub(crate) const DEVICE_SIGNING_CERTIFICATE_CTX: &str = "nuschel/device-signing-certificate/v1";
pub(crate) const DEVICE_CERTIFICATE_CTX: &str = "nuschel/device-certificate/v1";
pub(crate) const DEVICE_CREDENTIAL_CTX: &str = "nuschel/device-credential/v1";
pub(crate) const DEVICE_REVOCATION_CTX: &str = "nuschel/device-revocation/v1";
pub(crate) const ACCOUNT_STATEMENT_CTX: &str = "nuschel/account-statement/v1";

/// `through_key_epoch` of a revocation that retires the device itself.
pub const WHOLE_DEVICE: u32 = u32::MAX;

/// The Ed25519 message for a context: `uint8 len(ctx) || ctx || body`.
fn ed25519_message(ctx: &str, body: &[u8]) -> Vec<u8> {
    let mut message = crate::codec::label(ctx);
    message.extend_from_slice(body);
    message
}

/// A validity window in Unix seconds: `not_before <= now < not_after`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Validity {
    not_before: u64,
    not_after: u64,
}

impl Validity {
    pub fn new(not_before: u64, not_after: u64) -> Result<Self, Error> {
        if not_before >= not_after {
            return Err(Error::Malformed("not_before must be before not_after"));
        }
        Ok(Self {
            not_before,
            not_after,
        })
    }

    /// From `now` for `seconds`, saturating at the end of time.
    pub fn starting_at(now: u64, seconds: u64) -> Result<Self, Error> {
        Self::new(now, now.saturating_add(seconds))
    }

    pub fn not_before(&self) -> u64 {
        self.not_before
    }

    pub fn not_after(&self) -> u64 {
        self.not_after
    }

    pub fn check(&self, now: u64) -> Result<(), Error> {
        if now < self.not_before {
            Err(Error::NotYetValid)
        } else if now >= self.not_after {
            Err(Error::Expired)
        } else {
            Ok(())
        }
    }

    fn write(&self, writer: &mut Writer) {
        writer.u64(self.not_before);
        writer.u64(self.not_after);
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, Error> {
        let not_before = reader.u64()?;
        let not_after = reader.u64()?;
        Self::new(not_before, not_after)
    }
}

fn read_positive(reader: &mut Reader<'_>, what: &'static str) -> Result<u32, Error> {
    match reader.u32()? {
        0 => Err(Error::Malformed(what)),
        value => Ok(value),
    }
}

fn check_issuer(
    identity: &VerifiedIdentity,
    claimed: IdentityFingerprint,
    generation: u32,
) -> Result<(), Error> {
    if claimed != identity.fingerprint() {
        return Err(Error::WrongIdentity);
    }
    if generation != identity.generation() {
        return Err(Error::WrongGeneration {
            expected: identity.generation(),
            found: generation,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------------
// Root -> device-signing key

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeviceSigningBody {
    identity: IdentityFingerprint,
    generation: u32,
    created_at: u64,
    device_signing_key: HybridPublicKey,
    credential_key: Ed25519PublicKey,
}

impl DeviceSigningBody {
    fn write(&self, writer: &mut Writer) {
        writer.header(kind::DEVICE_SIGNING_CERTIFICATE);
        writer.bytes(self.identity.as_bytes());
        writer.u32(self.generation);
        writer.u64(self.created_at);
        self.device_signing_key.write(writer);
        writer.u16(SignatureAlgorithm::Ed25519.code());
        writer.bytes(self.credential_key.as_bytes());
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, Error> {
        reader.header(kind::DEVICE_SIGNING_CERTIFICATE)?;
        let identity = IdentityFingerprint::from_bytes(reader.array()?);
        let generation = read_positive(reader, "generation 0")?;
        let created_at = reader.u64()?;
        let device_signing_key = HybridPublicKey::read(reader)?;
        SignatureAlgorithm::expect(reader, SignatureAlgorithm::Ed25519)?;
        let credential_key = Ed25519PublicKey::read(reader)?;
        Ok(Self {
            identity,
            generation,
            created_at,
            device_signing_key,
            credential_key,
        })
    }

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        self.write(&mut writer);
        writer.into_vec()
    }
}

/// The root's certification of a device-signing key, generation `g`.
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceSigningCertificate {
    body: DeviceSigningBody,
    signature: HybridSignature,
}

impl DeviceSigningCertificate {
    pub const LEN: usize = 2 + 32 + 4 + 8 + HybridPublicKey::LEN + 2 + 32 + HybridSignature::LEN;

    pub fn identity(&self) -> IdentityFingerprint {
        self.body.identity
    }

    pub fn generation(&self) -> u32 {
        self.body.generation
    }

    pub fn created_at(&self) -> u64 {
        self.body.created_at
    }

    pub fn device_signing_key(&self) -> &HybridPublicKey {
        &self.body.device_signing_key
    }

    pub fn credential_key(&self) -> &Ed25519PublicKey {
        &self.body.credential_key
    }

    /// Verifies the certificate under `root`, which must be the identity it names.
    pub fn verify(&self, root: &HybridPublicKey) -> Result<(), Error> {
        if root.fingerprint() != self.body.identity {
            return Err(Error::WrongIdentity);
        }
        root.verify(
            DEVICE_SIGNING_CERTIFICATE_CTX,
            &self.body.encode(),
            &self.signature,
        )
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        self.body.write(&mut writer);
        writer.bytes(self.signature.as_bytes());
        writer.into_vec()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes, Self::LEN)?;
        let body = DeviceSigningBody::read(&mut reader)?;
        let signature = HybridSignature::read(&mut reader)?;
        reader.finish()?;
        Ok(Self { body, signature })
    }
}

impl fmt::Debug for DeviceSigningCertificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceSigningCertificate")
            .field("identity", &self.body.identity)
            .field("generation", &self.body.generation)
            .field("device_signing_key", &self.body.device_signing_key)
            .finish_non_exhaustive()
    }
}

impl IdentityRoot {
    /// Certifies `key` as the device-signing key of generation `generation` (>= 1).
    pub fn certify_device_signing_key(
        &self,
        key: &DeviceSigningKey,
        generation: u32,
        created_at: u64,
    ) -> Result<DeviceSigningCertificate, Error> {
        self.certify_device_signing_key_with(key, generation, created_at, crypto::random_array()?)
    }

    pub(crate) fn certify_device_signing_key_with(
        &self,
        key: &DeviceSigningKey,
        generation: u32,
        created_at: u64,
        randomness: [u8; 32],
    ) -> Result<DeviceSigningCertificate, Error> {
        if generation == 0 {
            return Err(Error::Malformed("generation 0"));
        }
        let body = DeviceSigningBody {
            identity: self.fingerprint(),
            generation,
            created_at,
            device_signing_key: key.public_key().clone(),
            credential_key: *key.credential_key(),
        };
        let signature =
            self.secret
                .sign_with(DEVICE_SIGNING_CERTIFICATE_CTX, &body.encode(), randomness)?;
        Ok(DeviceSigningCertificate { body, signature })
    }

    /// A fresh device-signing key of generation 1, for a new identity.
    pub fn first_device_signing_authority(
        &self,
        created_at: u64,
    ) -> Result<DeviceSigningAuthority, Error> {
        let key = DeviceSigningKey::generate()?;
        let certificate = self.certify_device_signing_key(&key, 1, created_at)?;
        DeviceSigningAuthority::new(key, certificate)
    }

    /// Replaces the device-signing key (a device holding it was lost): a new key, one
    /// generation after `current`. The identity, and every verification of it, stays.
    pub fn rotate_device_signing_key(
        &self,
        current: &DeviceSigningCertificate,
        created_at: u64,
    ) -> Result<DeviceSigningAuthority, Error> {
        if current.identity() != self.fingerprint() {
            return Err(Error::WrongIdentity);
        }
        let generation = current
            .generation()
            .checked_add(1)
            .ok_or(Error::Malformed("generation exhausted"))?;
        let key = DeviceSigningKey::generate()?;
        let certificate = self.certify_device_signing_key(&key, generation, created_at)?;
        DeviceSigningAuthority::new(key, certificate)
    }
}

// ---------------------------------------------------------------------------------
// Device-signing key -> device (full certificate)

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeviceBody {
    identity: IdentityFingerprint,
    signing_generation: u32,
    device_id: DeviceId,
    key_epoch: u32,
    validity: Validity,
    leaf_key: Ed25519PublicKey,
    kem_key: XWingPublicKey,
    display_name: DisplayName,
}

impl DeviceBody {
    fn write(&self, writer: &mut Writer) {
        writer.header(kind::DEVICE_CERTIFICATE);
        writer.bytes(self.identity.as_bytes());
        writer.u32(self.signing_generation);
        writer.bytes(self.device_id.as_bytes());
        writer.u32(self.key_epoch);
        self.validity.write(writer);
        writer.u16(SignatureAlgorithm::Ed25519.code());
        writer.bytes(self.leaf_key.as_bytes());
        writer.u16(KemAlgorithm::XWing.code());
        writer.bytes(self.kem_key.as_bytes());
        self.display_name.write(writer);
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, Error> {
        reader.header(kind::DEVICE_CERTIFICATE)?;
        let identity = IdentityFingerprint::from_bytes(reader.array()?);
        let signing_generation = read_positive(reader, "generation 0")?;
        let device_id = DeviceId::from_bytes(reader.array()?);
        let key_epoch = read_positive(reader, "key epoch 0")?;
        let validity = Validity::read(reader)?;
        SignatureAlgorithm::expect(reader, SignatureAlgorithm::Ed25519)?;
        let leaf_key = Ed25519PublicKey::read(reader)?;
        KemAlgorithm::expect(reader, KemAlgorithm::XWing)?;
        let kem_key = XWingPublicKey::read(reader)?;
        let display_name = DisplayName::read(reader)?;
        Ok(Self {
            identity,
            signing_generation,
            device_id,
            key_epoch,
            validity,
            leaf_key,
            kem_key,
            display_name,
        })
    }

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        self.write(&mut writer);
        writer.into_vec()
    }
}

/// A device's full certificate, published in the server's directory: its leaf and
/// X-Wing keys and display name, signed (hybrid) by the device-signing key.
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceCertificate {
    body: DeviceBody,
    signature: HybridSignature,
}

impl DeviceCertificate {
    pub const MIN_LEN: usize =
        2 + 32 + 4 + 16 + 4 + 16 + 2 + 32 + 2 + XWingPublicKey::LEN + 1 + HybridSignature::LEN;
    pub const MAX_LEN: usize = Self::MIN_LEN + DisplayName::MAX_LEN;

    pub fn identity(&self) -> IdentityFingerprint {
        self.body.identity
    }

    pub fn signing_generation(&self) -> u32 {
        self.body.signing_generation
    }

    pub fn device_id(&self) -> DeviceId {
        self.body.device_id
    }

    pub fn key_epoch(&self) -> u32 {
        self.body.key_epoch
    }

    pub fn validity(&self) -> Validity {
        self.body.validity
    }

    pub fn leaf_key(&self) -> &Ed25519PublicKey {
        &self.body.leaf_key
    }

    pub fn kem_key(&self) -> &XWingPublicKey {
        &self.body.kem_key
    }

    pub fn display_name(&self) -> &DisplayName {
        &self.body.display_name
    }

    pub fn public_keys(&self) -> DevicePublicKeys {
        DevicePublicKeys {
            device_id: self.body.device_id,
            key_epoch: self.body.key_epoch,
            leaf_key: self.body.leaf_key,
            kem_key: self.body.kem_key.clone(),
        }
    }

    /// Verifies the signature under the identity's current device-signing key. Does not
    /// check validity times or revocation; [`crate::DeviceRoster`] does.
    pub fn verify(&self, identity: &VerifiedIdentity) -> Result<(), Error> {
        check_issuer(identity, self.body.identity, self.body.signing_generation)?;
        identity
            .device_signing_certificate()
            .device_signing_key()
            .verify(DEVICE_CERTIFICATE_CTX, &self.body.encode(), &self.signature)
    }

    /// Whether `credential` names the same identity, generation, device, key epoch and
    /// leaf key.
    pub fn matches_credential(&self, credential: &DeviceCredential) -> bool {
        let c = &credential.body;
        self.body.identity == c.identity
            && self.body.signing_generation == c.signing_generation
            && self.body.device_id == c.device_id
            && self.body.key_epoch == c.key_epoch
            && self.body.leaf_key == c.leaf_key
    }

    /// SHA-256 of the encoding: what an identity's key transparency record lists.
    pub fn hash(&self) -> [u8; 32] {
        crypto::sha256(&[&self.encode()])
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        self.body.write(&mut writer);
        writer.bytes(self.signature.as_bytes());
        writer.into_vec()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes, Self::MAX_LEN)?;
        let body = DeviceBody::read(&mut reader)?;
        let signature = HybridSignature::read(&mut reader)?;
        reader.finish()?;
        Ok(Self { body, signature })
    }
}

impl fmt::Debug for DeviceCertificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCertificate")
            .field("identity", &self.body.identity)
            .field("signing_generation", &self.body.signing_generation)
            .field("device_id", &self.body.device_id)
            .field("key_epoch", &self.body.key_epoch)
            .field("validity", &self.body.validity)
            .field("display_name", &self.body.display_name)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------
// Device-signing key -> device (compact MLS credential)

#[derive(Clone, Debug, PartialEq, Eq)]
struct CredentialBody {
    identity: IdentityFingerprint,
    signing_generation: u32,
    device_id: DeviceId,
    key_epoch: u32,
    validity: Validity,
    leaf_key: Ed25519PublicKey,
}

impl CredentialBody {
    fn write(&self, writer: &mut Writer) {
        writer.header(kind::DEVICE_CREDENTIAL);
        writer.bytes(self.identity.as_bytes());
        writer.u32(self.signing_generation);
        writer.bytes(self.device_id.as_bytes());
        writer.u32(self.key_epoch);
        self.validity.write(writer);
        writer.u16(SignatureAlgorithm::Ed25519.code());
        writer.bytes(self.leaf_key.as_bytes());
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, Error> {
        reader.header(kind::DEVICE_CREDENTIAL)?;
        let identity = IdentityFingerprint::from_bytes(reader.array()?);
        let signing_generation = read_positive(reader, "generation 0")?;
        let device_id = DeviceId::from_bytes(reader.array()?);
        let key_epoch = read_positive(reader, "key epoch 0")?;
        let validity = Validity::read(reader)?;
        SignatureAlgorithm::expect(reader, SignatureAlgorithm::Ed25519)?;
        let leaf_key = Ed25519PublicKey::read(reader)?;
        Ok(Self {
            identity,
            signing_generation,
            device_id,
            key_epoch,
            validity,
            leaf_key,
        })
    }

    fn message(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        self.write(&mut writer);
        ed25519_message(DEVICE_CREDENTIAL_CTX, &writer.into_vec())
    }
}

/// The compact device credential carried in MLS leaves (credential type 0xF4E1,
/// docs/e2ee.md §3.3). The per-person part (root key, device-signing certificate) comes
/// from the directory; the credential is checked against it.
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceCredential {
    body: CredentialBody,
    signature: [u8; ed25519::SIGNATURE_LEN],
}

impl DeviceCredential {
    pub const LEN: usize = 2 + 32 + 4 + 16 + 4 + 16 + 2 + 32 + ed25519::SIGNATURE_LEN;

    pub fn identity(&self) -> IdentityFingerprint {
        self.body.identity
    }

    pub fn signing_generation(&self) -> u32 {
        self.body.signing_generation
    }

    pub fn device_id(&self) -> DeviceId {
        self.body.device_id
    }

    pub fn key_epoch(&self) -> u32 {
        self.body.key_epoch
    }

    pub fn validity(&self) -> Validity {
        self.body.validity
    }

    pub fn leaf_key(&self) -> &Ed25519PublicKey {
        &self.body.leaf_key
    }

    /// Verifies the credential for an MLS leaf: the signature under the identity's
    /// current credential key, that it certifies `leaf_key` (the leaf's signature key),
    /// and the validity window at `now`. Revocation and the device list are checked by
    /// [`crate::DeviceRoster::check_credential`].
    pub fn verify(
        &self,
        identity: &VerifiedIdentity,
        leaf_key: &[u8],
        now: u64,
    ) -> Result<(), Error> {
        self.verify_signature(identity)?;
        if leaf_key != self.body.leaf_key.as_bytes() {
            return Err(Error::KeyMismatch);
        }
        self.body.validity.check(now)
    }

    /// The issuer and signature checks alone.
    pub(crate) fn verify_signature(&self, identity: &VerifiedIdentity) -> Result<(), Error> {
        check_issuer(identity, self.body.identity, self.body.signing_generation)?;
        identity
            .device_signing_certificate()
            .credential_key()
            .verify(&self.body.message(), &self.signature)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        self.body.write(&mut writer);
        writer.bytes(&self.signature);
        writer.into_vec()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes, Self::LEN)?;
        let body = CredentialBody::read(&mut reader)?;
        let signature = reader.array()?;
        reader.finish()?;
        Ok(Self { body, signature })
    }
}

impl fmt::Debug for DeviceCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCredential")
            .field("identity", &self.body.identity)
            .field("signing_generation", &self.body.signing_generation)
            .field("device_id", &self.body.device_id)
            .field("key_epoch", &self.body.key_epoch)
            .field("validity", &self.body.validity)
            .field("leaf_key", &self.body.leaf_key)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------
// Device-signing key -> revocation

#[derive(Clone, Debug, PartialEq, Eq)]
struct RevocationBody {
    identity: IdentityFingerprint,
    signing_generation: u32,
    device_id: DeviceId,
    through_key_epoch: u32,
    revoked_at: u64,
}

impl RevocationBody {
    fn write(&self, writer: &mut Writer) {
        writer.header(kind::DEVICE_REVOCATION);
        writer.bytes(self.identity.as_bytes());
        writer.u32(self.signing_generation);
        writer.bytes(self.device_id.as_bytes());
        writer.u32(self.through_key_epoch);
        writer.u64(self.revoked_at);
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, Error> {
        reader.header(kind::DEVICE_REVOCATION)?;
        Ok(Self {
            identity: IdentityFingerprint::from_bytes(reader.array()?),
            signing_generation: read_positive(reader, "generation 0")?,
            device_id: DeviceId::from_bytes(reader.array()?),
            through_key_epoch: read_positive(reader, "key epoch 0")?,
            revoked_at: reader.u64()?,
        })
    }

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        self.write(&mut writer);
        writer.into_vec()
    }
}

/// Revokes key epochs `1..=through_key_epoch` of a device, or with [`WHOLE_DEVICE`] the
/// device itself (docs/e2ee.md §3.6). Valid under the generation that signed it;
/// after a rotation, revoked devices are simply not certified again.
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceRevocation {
    body: RevocationBody,
    signature: HybridSignature,
}

impl DeviceRevocation {
    pub const LEN: usize = 2 + 32 + 4 + 16 + 4 + 8 + HybridSignature::LEN;

    pub fn identity(&self) -> IdentityFingerprint {
        self.body.identity
    }

    pub fn signing_generation(&self) -> u32 {
        self.body.signing_generation
    }

    pub fn device_id(&self) -> DeviceId {
        self.body.device_id
    }

    pub fn through_key_epoch(&self) -> u32 {
        self.body.through_key_epoch
    }

    pub fn revoked_at(&self) -> u64 {
        self.body.revoked_at
    }

    pub fn verify(&self, identity: &VerifiedIdentity) -> Result<(), Error> {
        check_issuer(identity, self.body.identity, self.body.signing_generation)?;
        identity
            .device_signing_certificate()
            .device_signing_key()
            .verify(DEVICE_REVOCATION_CTX, &self.body.encode(), &self.signature)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        self.body.write(&mut writer);
        writer.bytes(self.signature.as_bytes());
        writer.into_vec()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes, Self::LEN)?;
        let body = RevocationBody::read(&mut reader)?;
        let signature = HybridSignature::read(&mut reader)?;
        reader.finish()?;
        Ok(Self { body, signature })
    }
}

impl fmt::Debug for DeviceRevocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceRevocation")
            .field("identity", &self.body.identity)
            .field("signing_generation", &self.body.signing_generation)
            .field("device_id", &self.body.device_id)
            .field("through_key_epoch", &self.body.through_key_epoch)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------
// Device-signing key -> account statement

#[derive(Clone, Debug, PartialEq, Eq)]
struct AccountBody {
    identity: IdentityFingerprint,
    signing_generation: u32,
    server: [u8; 32],
    account: [u8; 16],
    created_at: u64,
}

impl AccountBody {
    fn write(&self, writer: &mut Writer) {
        writer.header(kind::ACCOUNT_STATEMENT);
        writer.bytes(self.identity.as_bytes());
        writer.u32(self.signing_generation);
        writer.bytes(&self.server);
        writer.bytes(&self.account);
        writer.u64(self.created_at);
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, Error> {
        reader.header(kind::ACCOUNT_STATEMENT)?;
        Ok(Self {
            identity: IdentityFingerprint::from_bytes(reader.array()?),
            signing_generation: read_positive(reader, "generation 0")?,
            server: reader.array()?,
            account: reader.array()?,
            created_at: reader.u64()?,
        })
    }

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        self.write(&mut writer);
        writer.into_vec()
    }
}

/// "Account `account` on server `server` is mine", signed by the identity's
/// device-signing key (docs/e2ee.md §3.8). A server publishes it in its key-transparency
/// log beside the account binding, so it can claim an account for an identity only with
/// that identity's consent: a server cannot present someone else's identity as one of
/// its accounts. `server` is the server's log id, which names its signing key.
///
/// Like device certificates it is valid only under the generation that signed it; after
/// a rotation of the device-signing key the account is bound again.
#[derive(Clone, PartialEq, Eq)]
pub struct AccountStatement {
    body: AccountBody,
    signature: HybridSignature,
}

impl AccountStatement {
    pub const LEN: usize = 2 + 32 + 4 + 32 + 16 + 8 + HybridSignature::LEN;

    pub fn identity(&self) -> IdentityFingerprint {
        self.body.identity
    }

    pub fn signing_generation(&self) -> u32 {
        self.body.signing_generation
    }

    pub fn server(&self) -> &[u8; 32] {
        &self.body.server
    }

    pub fn account(&self) -> &[u8; 16] {
        &self.body.account
    }

    pub fn created_at(&self) -> u64 {
        self.body.created_at
    }

    /// Checks the signature under the identity's current device-signing key.
    pub fn verify(&self, identity: &VerifiedIdentity) -> Result<(), Error> {
        check_issuer(identity, self.body.identity, self.body.signing_generation)?;
        identity
            .device_signing_certificate()
            .device_signing_key()
            .verify(ACCOUNT_STATEMENT_CTX, &self.body.encode(), &self.signature)
    }

    /// SHA-256 of the encoding: what an account binding commits to.
    pub fn hash(&self) -> [u8; 32] {
        crypto::sha256(&[&self.encode()])
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        self.body.write(&mut writer);
        writer.bytes(self.signature.as_bytes());
        writer.into_vec()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes, Self::LEN)?;
        let body = AccountBody::read(&mut reader)?;
        let signature = HybridSignature::read(&mut reader)?;
        reader.finish()?;
        Ok(Self { body, signature })
    }
}

impl fmt::Debug for AccountStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountStatement")
            .field("identity", &self.body.identity)
            .field("signing_generation", &self.body.signing_generation)
            .field("created_at", &self.body.created_at)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------
// The device-signing key at work

/// A device-signing key with its certificate: what a device that may link other devices
/// holds. It certifies devices and signs revocations.
pub struct DeviceSigningAuthority {
    key: DeviceSigningKey,
    certificate: DeviceSigningCertificate,
}

impl DeviceSigningAuthority {
    /// Pairs a key with its certificate; they must belong together.
    pub fn new(
        key: DeviceSigningKey,
        certificate: DeviceSigningCertificate,
    ) -> Result<Self, Error> {
        if key.public_key() != certificate.device_signing_key()
            || key.credential_key() != certificate.credential_key()
        {
            return Err(Error::KeyMismatch);
        }
        Ok(Self { key, certificate })
    }

    pub fn certificate(&self) -> &DeviceSigningCertificate {
        &self.certificate
    }

    pub fn identity(&self) -> IdentityFingerprint {
        self.certificate.identity()
    }

    pub fn generation(&self) -> u32 {
        self.certificate.generation()
    }

    pub(crate) fn key(&self) -> &DeviceSigningKey {
        &self.key
    }

    pub(crate) fn duplicate(&self) -> Self {
        Self {
            key: self.key.duplicate(),
            certificate: self.certificate.clone(),
        }
    }

    /// Certifies a device's keys: the full certificate for the directory and the
    /// compact credential for its MLS leaves, with the same validity.
    pub fn certify_device(
        &self,
        device: &DevicePublicKeys,
        display_name: &DisplayName,
        validity: Validity,
    ) -> Result<(DeviceCertificate, DeviceCredential), Error> {
        self.certify_device_with(device, display_name, validity, crypto::random_array()?)
    }

    pub(crate) fn certify_device_with(
        &self,
        device: &DevicePublicKeys,
        display_name: &DisplayName,
        validity: Validity,
        randomness: [u8; 32],
    ) -> Result<(DeviceCertificate, DeviceCredential), Error> {
        if device.key_epoch == 0 {
            return Err(Error::Malformed("key epoch 0"));
        }
        let body = DeviceBody {
            identity: self.identity(),
            signing_generation: self.generation(),
            device_id: device.device_id,
            key_epoch: device.key_epoch,
            validity,
            leaf_key: device.leaf_key,
            kem_key: device.kem_key.clone(),
            display_name: display_name.clone(),
        };
        let signature =
            self.key
                .secret
                .sign_with(DEVICE_CERTIFICATE_CTX, &body.encode(), randomness)?;
        let certificate = DeviceCertificate { body, signature };
        let credential = self.issue_credential(device, validity)?;
        Ok((certificate, credential))
    }

    /// A compact credential alone, for example to renew its validity.
    pub fn issue_credential(
        &self,
        device: &DevicePublicKeys,
        validity: Validity,
    ) -> Result<DeviceCredential, Error> {
        if device.key_epoch == 0 {
            return Err(Error::Malformed("key epoch 0"));
        }
        let body = CredentialBody {
            identity: self.identity(),
            signing_generation: self.generation(),
            device_id: device.device_id,
            key_epoch: device.key_epoch,
            validity,
            leaf_key: device.leaf_key,
        };
        let signature = ed25519::sign(self.key.credential_seed.expose(), &body.message())?;
        Ok(DeviceCredential { body, signature })
    }

    /// Revokes key epochs `1..=through_key_epoch` of `device_id` ([`WHOLE_DEVICE`]: all).
    pub fn revoke_device(
        &self,
        device_id: DeviceId,
        through_key_epoch: u32,
        revoked_at: u64,
    ) -> Result<DeviceRevocation, Error> {
        self.revoke_device_with(
            device_id,
            through_key_epoch,
            revoked_at,
            crypto::random_array()?,
        )
    }

    pub(crate) fn revoke_device_with(
        &self,
        device_id: DeviceId,
        through_key_epoch: u32,
        revoked_at: u64,
        randomness: [u8; 32],
    ) -> Result<DeviceRevocation, Error> {
        if through_key_epoch == 0 {
            return Err(Error::Malformed("key epoch 0"));
        }
        let body = RevocationBody {
            identity: self.identity(),
            signing_generation: self.generation(),
            device_id,
            through_key_epoch,
            revoked_at,
        };
        let signature =
            self.key
                .secret
                .sign_with(DEVICE_REVOCATION_CTX, &body.encode(), randomness)?;
        Ok(DeviceRevocation { body, signature })
    }

    /// Signs "account `account` on the server with log id `server` belongs to this
    /// identity" (docs/e2ee.md §3.8).
    pub fn sign_account(
        &self,
        server: [u8; 32],
        account: [u8; 16],
        created_at: u64,
    ) -> Result<AccountStatement, Error> {
        self.sign_account_with(server, account, created_at, crypto::random_array()?)
    }

    pub(crate) fn sign_account_with(
        &self,
        server: [u8; 32],
        account: [u8; 16],
        created_at: u64,
        randomness: [u8; 32],
    ) -> Result<AccountStatement, Error> {
        let body = AccountBody {
            identity: self.identity(),
            signing_generation: self.generation(),
            server,
            account,
            created_at,
        };
        let signature =
            self.key
                .secret
                .sign_with(ACCOUNT_STATEMENT_CTX, &body.encode(), randomness)?;
        Ok(AccountStatement { body, signature })
    }
}

impl fmt::Debug for DeviceSigningAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceSigningAuthority")
            .field("certificate", &self.certificate)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{composite, mldsa65};
    use crate::tests::fixture;

    #[test]
    fn signatures_do_not_verify_under_another_context() {
        let f = fixture();
        let root = &f.id.root;
        let authority = &f.id.device_signing;

        let cert = authority.certificate();
        let body = cert.body.encode();
        for (ctx, ok) in [
            (DEVICE_SIGNING_CERTIFICATE_CTX, true),
            (DEVICE_CERTIFICATE_CTX, false),
            (DEVICE_REVOCATION_CTX, false),
            ("", false),
        ] {
            let resigned = DeviceSigningCertificate {
                body: cert.body.clone(),
                signature: root.secret.sign_with(ctx, &body, [0; 32]).unwrap(),
            };
            assert_eq!(
                resigned.verify(root.public_key()).is_ok(),
                ok,
                "ctx {ctx:?}"
            );
        }

        let device = &f.id.device_certificate;
        let body = device.body.encode();
        for (ctx, ok) in [
            (DEVICE_CERTIFICATE_CTX, true),
            (DEVICE_SIGNING_CERTIFICATE_CTX, false),
            (DEVICE_REVOCATION_CTX, false),
        ] {
            let resigned = DeviceCertificate {
                body: device.body.clone(),
                signature: authority
                    .key()
                    .secret
                    .sign_with(ctx, &body, [0; 32])
                    .unwrap(),
            };
            assert_eq!(resigned.verify(&f.identity).is_ok(), ok, "ctx {ctx:?}");
        }

        let revocation = &f.revocation;
        let body = revocation.body.encode();
        for (ctx, ok) in [
            (DEVICE_REVOCATION_CTX, true),
            (DEVICE_CERTIFICATE_CTX, false),
        ] {
            let resigned = DeviceRevocation {
                body: revocation.body.clone(),
                signature: authority
                    .key()
                    .secret
                    .sign_with(ctx, &body, [0; 32])
                    .unwrap(),
            };
            assert_eq!(resigned.verify(&f.identity).is_ok(), ok, "ctx {ctx:?}");
        }

        // The credential: right key and body, wrong framing.
        let credential = &f.id.device_credential;
        let mut writer = Writer::new();
        credential.body.write(&mut writer);
        let body = writer.into_vec();
        let credential_seed = authority.key().credential_seed.expose();
        for message in [
            ed25519_message(DEVICE_CERTIFICATE_CTX, &body),
            body.clone(),
            composite::message_representative(DEVICE_CREDENTIAL_CTX.as_bytes(), &body),
        ] {
            let resigned = DeviceCredential {
                body: credential.body.clone(),
                signature: ed25519::sign(credential_seed, &message).unwrap(),
            };
            assert_eq!(
                resigned.verify_signature(&f.identity),
                Err(Error::BadSignature)
            );
        }
        // The hybrid key's Ed25519 half cannot stand in for the credential key.
        let (_, hybrid_ed25519) = authority.key().secret.seeds();
        let resigned = DeviceCredential {
            body: credential.body.clone(),
            signature: ed25519::sign(hybrid_ed25519, &credential.body.message()).unwrap(),
        };
        assert_eq!(
            resigned.verify_signature(&f.identity),
            Err(Error::BadSignature)
        );
    }

    #[test]
    fn a_wrong_signature_in_either_half_fails() {
        let f = fixture();
        let root = &f.id.root;
        let cert = f.id.device_signing.certificate();
        let body = cert.body.encode();
        let (mldsa_seed, ed_seed) = root.secret.seeds();
        let good = cert.signature.0.clone();

        let with = |signature: Box<[u8; HybridSignature::LEN]>| DeviceSigningCertificate {
            body: cert.body.clone(),
            signature: HybridSignature(signature),
        };
        with(good.clone()).verify(root.public_key()).unwrap();

        // Ed25519 half: a valid Ed25519 signature by the right key, but over the body
        // instead of the composite message representative.
        let mut s = good.clone();
        s[mldsa65::SIGNATURE_LEN..].copy_from_slice(&ed25519::sign(ed_seed, &body).unwrap());
        assert_eq!(with(s).verify(root.public_key()), Err(Error::BadSignature));
        // ML-DSA half: a valid ML-DSA-65 signature by the right key over the body with
        // the structure's context, not over M' with the composite label.
        let mut s = good.clone();
        let plain = mldsa65::sign(
            mldsa_seed,
            &body,
            DEVICE_SIGNING_CERTIFICATE_CTX.as_bytes(),
            [0; 32],
        )
        .unwrap();
        s[..mldsa65::SIGNATURE_LEN].copy_from_slice(&plain[..]);
        assert_eq!(with(s).verify(root.public_key()), Err(Error::BadSignature));
        // Halves from another identity's valid signature over the same body.
        let stranger = IdentityRoot::generate().unwrap();
        let other = stranger
            .secret
            .sign_with(DEVICE_SIGNING_CERTIFICATE_CTX, &body, [0; 32])
            .unwrap();
        for range in [
            0..mldsa65::SIGNATURE_LEN,
            mldsa65::SIGNATURE_LEN..HybridSignature::LEN,
        ] {
            let mut s = good.clone();
            s[range.clone()].copy_from_slice(&other.0[range]);
            assert_eq!(with(s).verify(root.public_key()), Err(Error::BadSignature));
        }
        // One flipped bit in each half.
        for index in [
            0,
            mldsa65::SIGNATURE_LEN - 1,
            mldsa65::SIGNATURE_LEN,
            HybridSignature::LEN - 1,
        ] {
            let mut s = good.clone();
            s[index] ^= 0x10;
            assert_eq!(with(s).verify(root.public_key()), Err(Error::BadSignature));
        }
    }
}
