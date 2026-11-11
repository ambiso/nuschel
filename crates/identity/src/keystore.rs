// SPDX-License-Identifier: EUPL-1.2
//! The encrypted keystore, the recovery key and the identity export (docs/e2ee.md §3.5,
//! §3.7).
//!
//! Every encrypted blob is a [`Sealed`] envelope under a 256-bit key the caller provides
//! (the store key from the OS credential store, the recovery key, or the device-sync
//! key):
//!
//! ```text
//! struct {
//!   uint8  kind = 0x20; uint8 version = 1;
//!   uint8  purpose;            // 1 device keystore, 2 recovery blob,
//!                              // 3 export secrets, 4 synced data,
//!                              // 5 the crypto worker's own state
//!   opaque salt[32];           // fresh for every seal
//!   opaque ciphertext<16..2^24-1>;
//! } Sealed;
//!
//! key        = HKDF-SHA-256(salt, caller_key, uint8(19) || "nuschel/keystore/v1" || purpose)
//! ciphertext = ChaCha20-Poly1305(key, nonce = 0^12, aad = kind || version || purpose || salt,
//!                                plaintext)
//! ```
//!
//! Each derived key encrypts exactly one message, so the zero nonce is never reused. A
//! wrong key, a different purpose, or any modified byte fails authentication.
//!
//! Plaintexts:
//!
//! ```text
//! struct {                                     // purpose 1, under the store key
//!   uint8 kind = 0x21; uint8 version = 1;
//!   HybridPublicKey root_public_key;
//!   opaque device_signing_certificate<1..2^16-1>;
//!   opaque device_id[16]; uint32 key_epoch;
//!   SignatureAlgorithm leaf_algorithm; opaque leaf_seed[32];   // Ed25519 seed
//!   KemAlgorithm kem_algorithm; opaque kem_seed[32];           // X-Wing seed
//!   opaque device_certificate<1..2^16-1>;
//!   opaque device_credential<1..2^16-1>;
//!   uint8 has_device_signing_secret; [opaque device_signing_secret[96]]
//!   uint8 has_recovery_blob; [opaque recovery_blob<1..2^16-1>]
//!   uint8 has_sync_key; [uint32 sync_key_epoch; opaque sync_key[32]]
//! } DeviceKeystore;
//!
//! struct {                                     // purpose 2, under the recovery key
//!   uint8 kind = 0x22; uint8 version = 1;
//!   opaque identity[32];
//!   opaque root_secret[64];                    // mldsa65_seed || ed25519_seed
//! } RootSecret;
//!
//! struct {                                     // not encrypted itself
//!   uint8 kind = 0x23; uint8 version = 1;
//!   opaque identity[32];
//!   opaque secrets<1..2^16-1>;                 // Sealed, purpose 3, recovery key: ExportSecrets
//!   opaque synced_data<0..2^24-1>;             // Sealed, purpose 4, device-sync key; or empty
//! } IdentityExport;
//!
//! struct {
//!   uint8 kind = 0x24; uint8 version = 1;
//!   opaque identity[32];
//!   opaque root_secret[64];
//!   uint8 has_device_signing; [opaque device_signing_certificate<1..2^16-1>;
//!                              opaque device_signing_secret[96]]
//!   uint8 has_sync_key; [uint32 sync_key_epoch; opaque sync_key[32]]
//! } ExportSecrets;
//! ```
//!
//! `device_signing_secret` is the hybrid secret (`mldsa65_seed || ed25519_seed`) followed
//! by the credential key's Ed25519 seed. Seeds are stored, never expanded keys; loading
//! re-derives every public key and checks it against the certificates.
//!
//! The recovery key is 256 random bits, shown as 56 Crockford base32 characters in 14
//! groups of 4: the key and a 24-bit checksum
//! (`SHA-256(uint8(23) || "nuschel/recovery-key/v1" || key)[0..3]`). Whether a recovery
//! key is required is a policy for the caller (docs/e2ee.md §10, question 1).

use std::fmt;

use zeroize::Zeroizing;

use crate::certificate::{DeviceCertificate, DeviceCredential, DeviceSigningAuthority};
use crate::codec::{Reader, Writer, kind, label};
use crate::crypto::{self, Secret32};
use crate::keys::{
    DeviceKeys, DeviceSigningKey, HybridPublicKey, HybridSecret, IdentityFingerprint, IdentityRoot,
};
use crate::roster::VerifiedIdentity;
use crate::{DeviceSigningCertificate, Error};

const KEYSTORE_LABEL: &str = "nuschel/keystore/v1";
const RECOVERY_CHECKSUM_LABEL: &str = "nuschel/recovery-key/v1";
const MAX_CIPHERTEXT: usize = (1 << 24) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Purpose {
    DeviceKeystore = 1,
    RecoveryBlob = 2,
    ExportSecrets = 3,
    SyncedData = 4,
    WorkerState = 5,
}

impl Purpose {
    fn from_byte(byte: u8) -> Result<Self, Error> {
        match byte {
            1 => Ok(Self::DeviceKeystore),
            2 => Ok(Self::RecoveryBlob),
            3 => Ok(Self::ExportSecrets),
            4 => Ok(Self::SyncedData),
            5 => Ok(Self::WorkerState),
            _ => Err(Error::Malformed("unknown sealed purpose")),
        }
    }
}

/// An authenticated, encrypted envelope (see the module documentation).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Sealed {
    purpose: Purpose,
    salt: [u8; 32],
    ciphertext: Vec<u8>,
}

impl Sealed {
    pub const MAX_LEN: usize = 2 + 1 + 32 + 3 + MAX_CIPHERTEXT;

    fn header(purpose: Purpose, salt: &[u8; 32]) -> [u8; 35] {
        let mut aad = [0u8; 35];
        aad[0] = kind::SEALED;
        aad[1] = crate::codec::VERSION;
        aad[2] = purpose as u8;
        aad[3..].copy_from_slice(salt);
        aad
    }

    fn key(caller_key: &[u8; 32], purpose: Purpose, salt: &[u8; 32]) -> Zeroizing<[u8; 32]> {
        let prk = crypto::hkdf_extract(salt, caller_key);
        let mut info = label(KEYSTORE_LABEL);
        info.push(purpose as u8);
        crypto::hkdf_expand(&prk, &info)
    }

    pub fn seal(caller_key: &[u8; 32], purpose: Purpose, plaintext: &[u8]) -> Result<Self, Error> {
        Self::seal_with_salt(caller_key, purpose, plaintext, crypto::random_array()?)
    }

    pub fn seal_with_salt(
        caller_key: &[u8; 32],
        purpose: Purpose,
        plaintext: &[u8],
        salt: [u8; 32],
    ) -> Result<Self, Error> {
        if plaintext.len() + 16 > MAX_CIPHERTEXT {
            return Err(Error::TooLong);
        }
        let key = Self::key(caller_key, purpose, &salt);
        let ciphertext = crypto::seal_once(&key, &Self::header(purpose, &salt), plaintext);
        Ok(Self {
            purpose,
            salt,
            ciphertext,
        })
    }

    pub fn open(
        &self,
        caller_key: &[u8; 32],
        purpose: Purpose,
    ) -> Result<Zeroizing<Vec<u8>>, Error> {
        if self.purpose != purpose {
            return Err(Error::Malformed("sealed for another purpose"));
        }
        let key = Self::key(caller_key, purpose, &self.salt);
        crypto::open_once(&key, &Self::header(purpose, &self.salt), &self.ciphertext)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.bytes(&Self::header(self.purpose, &self.salt));
        writer.opaque24(&self.ciphertext);
        writer.into_vec()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes, Self::MAX_LEN)?;
        reader.header(kind::SEALED)?;
        let purpose = Purpose::from_byte(reader.u8()?)?;
        let salt = reader.array()?;
        let ciphertext = reader.opaque24(16, MAX_CIPHERTEXT)?.to_vec();
        reader.finish()?;
        Ok(Self {
            purpose,
            salt,
            ciphertext,
        })
    }
}

/// The key that protects the device keystore, kept in the OS credential store.
pub struct StoreKey(Secret32);

impl StoreKey {
    pub fn generate() -> Result<Self, Error> {
        Ok(Self(Secret32::random()?))
    }

    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        Self(Secret32::from_slice(bytes).expect("32 bytes"))
    }

    /// The raw key, to hand to the OS credential store.
    pub fn to_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(*self.0.expose())
    }

    /// Encrypts the crypto worker's own state (pins, verified contacts, key-transparency
    /// heads) under the store key (purpose 5). The plaintext format is the caller's.
    pub fn seal_state(&self, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(Sealed::seal(self.0.expose(), Purpose::WorkerState, plaintext)?.encode())
    }

    pub fn open_state(&self, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
        Sealed::decode(sealed)?.open(self.0.expose(), Purpose::WorkerState)
    }
}

impl fmt::Debug for StoreKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StoreKey(redacted)")
    }
}

/// The symmetric key the user's devices share for per-user blobs on servers
/// (docs/e2ee.md §3.7). Rotated, with a higher epoch, when a device is revoked.
pub struct DeviceSyncKey {
    epoch: u32,
    key: Secret32,
}

impl DeviceSyncKey {
    pub fn generate(epoch: u32) -> Result<Self, Error> {
        Ok(Self {
            epoch,
            key: Secret32::random()?,
        })
    }

    /// A fresh key one epoch later.
    pub fn rotate(&self) -> Result<Self, Error> {
        Self::generate(
            self.epoch
                .checked_add(1)
                .ok_or(Error::Malformed("sync key epoch exhausted"))?,
        )
    }

    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Encrypts a per-user blob.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(Sealed::seal(self.key.expose(), Purpose::SyncedData, plaintext)?.encode())
    }

    pub fn open(&self, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
        Sealed::decode(sealed)?.open(self.key.expose(), Purpose::SyncedData)
    }

    pub(crate) fn duplicate(&self) -> Self {
        Self {
            epoch: self.epoch,
            key: self.key.duplicate(),
        }
    }

    pub(crate) fn write(&self, writer: &mut Writer) {
        writer.u32(self.epoch);
        writer.bytes(self.key.expose());
    }

    pub(crate) fn read(reader: &mut Reader<'_>) -> Result<Self, Error> {
        Ok(Self {
            epoch: reader.u32()?,
            key: Secret32::from_slice(reader.take(32)?)?,
        })
    }
}

impl fmt::Debug for DeviceSyncKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceSyncKey(epoch {}, redacted)", self.epoch)
    }
}

// ---------------------------------------------------------------------------------
// Recovery key

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The recovery key: 256 random bits, shown once (docs/e2ee.md §3.5).
pub struct RecoveryKey(Secret32);

impl RecoveryKey {
    /// Number of characters, without separators.
    pub const CHARS: usize = 56;

    pub fn generate() -> Result<Self, Error> {
        Ok(Self(Secret32::random()?))
    }

    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: &[u8; 32]) -> Self {
        Self(Secret32::from_slice(bytes).expect("32 bytes"))
    }

    fn checksum(key: &[u8; 32]) -> [u8; 3] {
        let digest = crypto::sha256(&[&label(RECOVERY_CHECKSUM_LABEL), key]);
        [digest[0], digest[1], digest[2]]
    }

    /// 14 groups of 4 Crockford base32 characters, separated by `-`.
    pub fn display(&self) -> Zeroizing<String> {
        let mut bytes = Zeroizing::new([0u8; 35]);
        bytes[..32].copy_from_slice(self.0.expose());
        bytes[32..].copy_from_slice(&Self::checksum(self.0.expose()));
        let mut out = Zeroizing::new(String::with_capacity(Self::CHARS + 13));
        let (mut buffer, mut bits) = (0u16, 0u32);
        let mut count = 0;
        for &byte in bytes.iter() {
            buffer = (buffer << 8) | u16::from(byte);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                if count > 0 && count % 4 == 0 {
                    out.push('-');
                }
                out.push(char::from(CROCKFORD[usize::from((buffer >> bits) & 31)]));
                count += 1;
            }
        }
        debug_assert_eq!(bits, 0);
        out
    }

    /// Parses a typed recovery key. Case, `-` and spaces do not matter, and `O`, `I`, `L`
    /// read as `0`, `1`, `1` (Crockford); anything else, a wrong length or a wrong
    /// checksum is rejected.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let mut bytes = Zeroizing::new([0u8; 35]);
        let (mut buffer, mut bits, mut count, mut out) = (0u16, 0u32, 0usize, 0usize);
        for c in text.chars() {
            let value = match c.to_ascii_uppercase() {
                '-' | ' ' => continue,
                'O' => 0,
                'I' | 'L' => 1,
                c => CROCKFORD
                    .iter()
                    .position(|&a| char::from(a) == c)
                    .ok_or(Error::Malformed("recovery key character"))? as u16,
            };
            count += 1;
            if count > Self::CHARS {
                return Err(Error::Malformed("recovery key length"));
            }
            buffer = (buffer << 5) | value;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                bytes[out] = (buffer >> bits) as u8;
                out += 1;
            }
        }
        if count != Self::CHARS {
            return Err(Error::Malformed("recovery key length"));
        }
        let key: [u8; 32] = bytes[..32].try_into().expect("32 bytes");
        let key = Secret32::from_slice(&key)?;
        if !crypto::ct_eq(&Self::checksum(key.expose()), &bytes[32..]) {
            return Err(Error::Checksum);
        }
        Ok(Self(key))
    }

    fn expose(&self) -> &[u8; 32] {
        self.0.expose()
    }
}

impl fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryKey(redacted)")
    }
}

/// The identity root encrypted under the recovery key: kept on every linked device and,
/// optionally, as an opaque backup on the user's servers.
#[derive(Clone, PartialEq, Eq)]
pub struct RecoveryBlob(Sealed);

impl RecoveryBlob {
    const PLAINTEXT_LEN: usize = 2 + 32 + HybridSecret::LEN;
    pub const MAX_LEN: usize = 2 + 1 + 32 + 3 + Self::PLAINTEXT_LEN + 16;

    pub fn seal(root: &IdentityRoot, key: &RecoveryKey) -> Result<Self, Error> {
        Self::seal_with_salt(root, key, crypto::random_array()?)
    }

    pub(crate) fn seal_with_salt(
        root: &IdentityRoot,
        key: &RecoveryKey,
        salt: [u8; 32],
    ) -> Result<Self, Error> {
        let mut writer = Writer::secret(Self::PLAINTEXT_LEN);
        writer.header(kind::ROOT_SECRET);
        writer.bytes(root.fingerprint().as_bytes());
        root.secret.write(&mut writer);
        let plaintext = writer.finish();
        Ok(Self(Sealed::seal_with_salt(
            key.expose(),
            Purpose::RecoveryBlob,
            &plaintext,
            salt,
        )?))
    }

    /// Decrypts the root and checks it against the identity it was sealed for.
    pub fn open(&self, key: &RecoveryKey) -> Result<IdentityRoot, Error> {
        let plaintext = self.0.open(key.expose(), Purpose::RecoveryBlob)?;
        let mut reader = Reader::new(&plaintext, Self::PLAINTEXT_LEN)?;
        reader.header(kind::ROOT_SECRET)?;
        let identity = IdentityFingerprint::from_bytes(reader.array()?);
        let root = IdentityRoot::from_secret(HybridSecret::read(&mut reader)?);
        reader.finish()?;
        if root.fingerprint() != identity {
            return Err(Error::KeyMismatch);
        }
        Ok(root)
    }

    pub fn encode(&self) -> Vec<u8> {
        self.0.encode()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > Self::MAX_LEN {
            return Err(Error::TooLong);
        }
        let sealed = Sealed::decode(bytes)?;
        if sealed.purpose != Purpose::RecoveryBlob {
            return Err(Error::Malformed("not a recovery blob"));
        }
        Ok(Self(sealed))
    }
}

impl fmt::Debug for RecoveryBlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryBlob")
    }
}

// ---------------------------------------------------------------------------------
// Device keystore

/// A device's identity and key material, as stored encrypted under the store key.
pub struct DeviceKeystore {
    identity: VerifiedIdentity,
    device: DeviceKeys,
    device_certificate: DeviceCertificate,
    device_credential: DeviceCredential,
    device_signing: Option<DeviceSigningAuthority>,
    recovery_blob: Option<RecoveryBlob>,
    sync_key: Option<DeviceSyncKey>,
}

impl DeviceKeystore {
    const PLAINTEXT_CAPACITY: usize = 2
        + HybridPublicKey::LEN
        + 2
        + DeviceSigningCertificate::LEN
        + DeviceKeys::SECRET_LEN
        + 2
        + DeviceCertificate::MAX_LEN
        + 2
        + DeviceCredential::LEN
        + 1
        + DeviceSigningKey::SECRET_LEN
        + 1
        + 2
        + RecoveryBlob::MAX_LEN
        + 1
        + 4
        + 32;

    /// Checks that the certificate and credential belong to `identity` and certify
    /// exactly `device`'s keys.
    pub fn new(
        identity: VerifiedIdentity,
        device: DeviceKeys,
        device_certificate: DeviceCertificate,
        device_credential: DeviceCredential,
    ) -> Result<Self, Error> {
        device_certificate.verify(&identity)?;
        device_credential.verify_signature(&identity)?;
        if device_certificate.public_keys() != device.public_keys()
            || !device_certificate.matches_credential(&device_credential)
        {
            return Err(Error::KeyMismatch);
        }
        Ok(Self {
            identity,
            device,
            device_certificate,
            device_credential,
            device_signing: None,
            recovery_blob: None,
            sync_key: None,
        })
    }

    /// The keystore of a newly linked device.
    pub fn from_provisioned(
        device: DeviceKeys,
        provisioned: crate::ProvisionedDevice,
    ) -> Result<Self, Error> {
        let mut store = Self::new(
            provisioned.identity,
            device,
            provisioned.device_certificate,
            provisioned.device_credential,
        )?;
        if let Some(authority) = provisioned.device_signing {
            store = store.with_device_signing(authority)?;
        }
        store.recovery_blob = provisioned.recovery_blob;
        store.sync_key = provisioned.sync_key;
        Ok(store)
    }

    pub fn with_device_signing(mut self, authority: DeviceSigningAuthority) -> Result<Self, Error> {
        if authority.certificate() != self.identity.device_signing_certificate() {
            return Err(Error::KeyMismatch);
        }
        self.device_signing = Some(authority);
        Ok(self)
    }

    pub fn with_recovery_blob(mut self, blob: RecoveryBlob) -> Self {
        self.recovery_blob = Some(blob);
        self
    }

    pub fn with_sync_key(mut self, key: DeviceSyncKey) -> Self {
        self.sync_key = Some(key);
        self
    }

    /// Drops the recovery blob: the user declined a recovery key, so the root is gone
    /// once this keystore replaces the old one.
    pub fn without_recovery_blob(mut self) -> Self {
        self.recovery_blob = None;
        self
    }

    /// Certifies this device's keys again with a new validity (the device-signing key
    /// is needed); the certificate and credential are replaced, the keys stay.
    pub fn renew_device(&mut self, validity: crate::Validity) -> Result<(), Error> {
        let authority = self
            .device_signing
            .as_ref()
            .ok_or(Error::Malformed("no device-signing key on this device"))?;
        let (certificate, credential) = authority.certify_device(
            &self.device.public_keys(),
            self.device_certificate.display_name(),
            validity,
        )?;
        self.device_certificate = certificate;
        self.device_credential = credential;
        Ok(())
    }

    pub fn identity(&self) -> &VerifiedIdentity {
        &self.identity
    }

    pub fn device(&self) -> &DeviceKeys {
        &self.device
    }

    pub fn device_certificate(&self) -> &DeviceCertificate {
        &self.device_certificate
    }

    pub fn device_credential(&self) -> &DeviceCredential {
        &self.device_credential
    }

    pub fn device_signing(&self) -> Option<&DeviceSigningAuthority> {
        self.device_signing.as_ref()
    }

    pub fn recovery_blob(&self) -> Option<&RecoveryBlob> {
        self.recovery_blob.as_ref()
    }

    pub fn sync_key(&self) -> Option<&DeviceSyncKey> {
        self.sync_key.as_ref()
    }

    fn plaintext(&self) -> Zeroizing<Vec<u8>> {
        let mut writer = Writer::secret(Self::PLAINTEXT_CAPACITY);
        writer.header(kind::DEVICE_KEYSTORE);
        self.identity.root_key().write(&mut writer);
        writer.opaque16(&self.identity.device_signing_certificate().encode());
        self.device.write_secret(&mut writer);
        writer.opaque16(&self.device_certificate.encode());
        writer.opaque16(&self.device_credential.encode());
        writer.presence(self.device_signing.is_some());
        if let Some(authority) = &self.device_signing {
            authority.key().write_secret(&mut writer);
        }
        writer.presence(self.recovery_blob.is_some());
        if let Some(blob) = &self.recovery_blob {
            writer.opaque16(&blob.encode());
        }
        writer.presence(self.sync_key.is_some());
        if let Some(key) = &self.sync_key {
            key.write(&mut writer);
        }
        writer.finish()
    }

    /// Encrypts the keystore under `key`.
    pub fn seal(&self, key: &StoreKey) -> Result<Vec<u8>, Error> {
        Ok(Sealed::seal(key.0.expose(), Purpose::DeviceKeystore, &self.plaintext())?.encode())
    }

    #[cfg(test)]
    pub(crate) fn seal_with_salt(&self, key: &StoreKey, salt: [u8; 32]) -> Result<Vec<u8>, Error> {
        Ok(Sealed::seal_with_salt(
            key.0.expose(),
            Purpose::DeviceKeystore,
            &self.plaintext(),
            salt,
        )?
        .encode())
    }

    /// Decrypts and checks a keystore: authenticity, then every key against its
    /// certificate and every certificate against the root.
    pub fn open(key: &StoreKey, bytes: &[u8]) -> Result<Self, Error> {
        let plaintext = Sealed::decode(bytes)?.open(key.0.expose(), Purpose::DeviceKeystore)?;
        let mut reader = Reader::new(&plaintext, Self::PLAINTEXT_CAPACITY)?;
        reader.header(kind::DEVICE_KEYSTORE)?;
        let root = HybridPublicKey::read(&mut reader)?;
        let device_signing_certificate =
            DeviceSigningCertificate::decode(reader.opaque16(1, usize::from(u16::MAX))?)?;
        let device = DeviceKeys::read_secret(&mut reader)?;
        let device_certificate =
            DeviceCertificate::decode(reader.opaque16(1, usize::from(u16::MAX))?)?;
        let device_credential =
            DeviceCredential::decode(reader.opaque16(1, usize::from(u16::MAX))?)?;
        let device_signing_key = if reader.presence()? {
            Some(DeviceSigningKey::read_secret(&mut reader)?)
        } else {
            None
        };
        let recovery_blob = if reader.presence()? {
            Some(RecoveryBlob::decode(
                reader.opaque16(1, usize::from(u16::MAX))?,
            )?)
        } else {
            None
        };
        let sync_key = if reader.presence()? {
            Some(DeviceSyncKey::read(&mut reader)?)
        } else {
            None
        };
        reader.finish()?;

        let identity = VerifiedIdentity::verify(root, device_signing_certificate)?;
        let mut store = Self::new(identity, device, device_certificate, device_credential)?;
        if let Some(key) = device_signing_key {
            let authority = DeviceSigningAuthority::new(
                key,
                store.identity.device_signing_certificate().clone(),
            )?;
            store = store.with_device_signing(authority)?;
        }
        store.recovery_blob = recovery_blob;
        store.sync_key = sync_key;
        Ok(store)
    }
}

impl fmt::Debug for DeviceKeystore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceKeystore")
            .field("identity", &self.identity.fingerprint())
            .field("device", &self.device)
            .field("device_signing", &self.device_signing.is_some())
            .field("recovery_blob", &self.recovery_blob.is_some())
            .field("sync_key", &self.sync_key)
            .finish()
    }
}

// ---------------------------------------------------------------------------------
// Identity export

/// An encrypted identity file for moving to a new device without another device online
/// (docs/e2ee.md §3.7): the root and device-signing key under the recovery key, and the
/// crypto worker's synced data (pins, verified contacts) under the device-sync key.
#[derive(Clone, PartialEq, Eq)]
pub struct IdentityExport {
    identity: IdentityFingerprint,
    secrets: Sealed,
    synced_data: Option<Sealed>,
}

/// What [`IdentityExport::open`] returns.
pub struct ImportedIdentity {
    pub root: IdentityRoot,
    pub device_signing: Option<DeviceSigningAuthority>,
    pub sync_key: Option<DeviceSyncKey>,
    pub synced_data: Zeroizing<Vec<u8>>,
}

impl fmt::Debug for ImportedIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImportedIdentity")
            .field("root", &self.root)
            .field("device_signing", &self.device_signing)
            .field("sync_key", &self.sync_key)
            .finish_non_exhaustive()
    }
}

impl IdentityExport {
    const SECRETS_CAPACITY: usize = 2
        + 32
        + HybridSecret::LEN
        + 1
        + 2
        + DeviceSigningCertificate::LEN
        + DeviceSigningKey::SECRET_LEN
        + 1
        + 4
        + 32;
    pub const MAX_LEN: usize = 2 + 32 + 2 + u16::MAX as usize + 3 + MAX_CIPHERTEXT;
    /// The largest synced payload: its envelope must fit the 24-bit length.
    pub const MAX_SYNCED_DATA: usize = MAX_CIPHERTEXT - (2 + 1 + 32 + 3) - 16;

    /// Creates the export. Synced data needs the device-sync key.
    pub fn create(
        root: &IdentityRoot,
        device_signing: Option<&DeviceSigningAuthority>,
        sync_key: Option<&DeviceSyncKey>,
        synced_data: &[u8],
        recovery_key: &RecoveryKey,
    ) -> Result<Self, Error> {
        if let Some(authority) = device_signing {
            authority.certificate().verify(root.public_key())?;
        }
        if synced_data.len() > Self::MAX_SYNCED_DATA {
            return Err(Error::TooLong);
        }
        let mut writer = Writer::secret(Self::SECRETS_CAPACITY);
        writer.header(kind::EXPORT_SECRETS);
        writer.bytes(root.fingerprint().as_bytes());
        root.secret.write(&mut writer);
        writer.presence(device_signing.is_some());
        if let Some(authority) = device_signing {
            writer.opaque16(&authority.certificate().encode());
            authority.key().write_secret(&mut writer);
        }
        writer.presence(sync_key.is_some());
        if let Some(key) = sync_key {
            key.write(&mut writer);
        }
        let secrets = Sealed::seal(
            recovery_key.expose(),
            Purpose::ExportSecrets,
            &writer.finish(),
        )?;
        let synced_data = match (sync_key, synced_data.is_empty()) {
            (_, true) => None,
            (Some(key), false) => Some(Sealed::seal(
                key.key.expose(),
                Purpose::SyncedData,
                synced_data,
            )?),
            (None, false) => return Err(Error::Malformed("synced data needs the device-sync key")),
        };
        Ok(Self {
            identity: root.fingerprint(),
            secrets,
            synced_data,
        })
    }

    /// The identity the file belongs to, readable before the recovery key is entered.
    pub fn identity(&self) -> IdentityFingerprint {
        self.identity
    }

    pub fn open(&self, recovery_key: &RecoveryKey) -> Result<ImportedIdentity, Error> {
        let plaintext = self
            .secrets
            .open(recovery_key.expose(), Purpose::ExportSecrets)?;
        let mut reader = Reader::new(&plaintext, Self::SECRETS_CAPACITY)?;
        reader.header(kind::EXPORT_SECRETS)?;
        let identity = IdentityFingerprint::from_bytes(reader.array()?);
        let root = IdentityRoot::from_secret(HybridSecret::read(&mut reader)?);
        let device_signing = if reader.presence()? {
            let certificate =
                DeviceSigningCertificate::decode(reader.opaque16(1, usize::from(u16::MAX))?)?;
            let key = DeviceSigningKey::read_secret(&mut reader)?;
            Some((certificate, key))
        } else {
            None
        };
        let sync_key = if reader.presence()? {
            Some(DeviceSyncKey::read(&mut reader)?)
        } else {
            None
        };
        reader.finish()?;
        if root.fingerprint() != identity || identity != self.identity {
            return Err(Error::KeyMismatch);
        }
        let device_signing = match device_signing {
            Some((certificate, key)) => {
                certificate.verify(root.public_key())?;
                Some(DeviceSigningAuthority::new(key, certificate)?)
            }
            None => None,
        };
        let synced_data = match (&self.synced_data, &sync_key) {
            (None, _) => Zeroizing::new(Vec::new()),
            (Some(sealed), Some(key)) => sealed.open(key.key.expose(), Purpose::SyncedData)?,
            (Some(_), None) => return Err(Error::Malformed("synced data without a sync key")),
        };
        Ok(ImportedIdentity {
            root,
            device_signing,
            sync_key,
            synced_data,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.header(kind::IDENTITY_EXPORT);
        writer.bytes(self.identity.as_bytes());
        writer.opaque16(&self.secrets.encode());
        writer.opaque24(
            &self
                .synced_data
                .as_ref()
                .map(Sealed::encode)
                .unwrap_or_default(),
        );
        writer.into_vec()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader::new(bytes, Self::MAX_LEN)?;
        reader.header(kind::IDENTITY_EXPORT)?;
        let identity = IdentityFingerprint::from_bytes(reader.array()?);
        let secrets = Sealed::decode(reader.opaque16(1, usize::from(u16::MAX))?)?;
        if secrets.purpose != Purpose::ExportSecrets {
            return Err(Error::Malformed("export secrets purpose"));
        }
        let synced = reader.opaque24(0, MAX_CIPHERTEXT)?;
        let synced_data = if synced.is_empty() {
            None
        } else {
            let sealed = Sealed::decode(synced)?;
            if sealed.purpose != Purpose::SyncedData {
                return Err(Error::Malformed("synced data purpose"));
            }
            Some(sealed)
        };
        reader.finish()?;
        Ok(Self {
            identity,
            secrets,
            synced_data,
        })
    }
}

impl fmt::Debug for IdentityExport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdentityExport")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}
