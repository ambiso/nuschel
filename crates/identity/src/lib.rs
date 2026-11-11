// SPDX-License-Identifier: EUPL-1.2
//! Nuschel's persistent, cross-server, multi-device identity (docs/e2ee.md §3, §4).
//!
//! ```text
//! identity root (hybrid ML-DSA-65 + Ed25519)          fingerprint = the identity
//!   └─ DeviceSigningCertificate, generation g: device-signing key (hybrid)
//!                                              + credential key (Ed25519)
//!         ├─ DeviceCertificate (hybrid): device id, key epoch, validity, MLS leaf key
//!         │                              (Ed25519), X-Wing key, display name
//!         ├─ DeviceCredential (Ed25519, 172 bytes): the same device in MLS leaves
//!         └─ DeviceRevocation (hybrid): key epochs of a device, or all of it
//! ```
//!
//! - [`keys`]: algorithms, public keys, the root, device-signing and device key pairs.
//! - [`certificate`]: the signed formats and their issuance.
//! - [`roster`]: what a verifier accepts ([`VerifiedIdentity`], [`DeviceRoster`]).
//! - [`safety`]: safety numbers and the QR payload.
//! - [`link`]: device linking (SPAKE2 short code, X-Wing-sealed bundle).
//! - [`keystore`]: the encrypted device keystore, recovery key and identity export.
//! - [`hpke`]: X-Wing HPKE and [`SealedToDevice`].
//!
//! Every format is a fixed-layout, versioned binary encoding (see [`codec`]'s rules and
//! each module's layout); decoders reject unknown versions, truncation and trailing
//! bytes, and every signature has its own context string. The test vectors in
//! `test-vectors/identity-v1.txt` pin the encodings.
//!
//! Nothing here does I/O or reads the clock: callers pass `now` (Unix seconds) and move
//! the bytes.
#![forbid(unsafe_code)]

mod codec;
mod crypto;
mod error;

pub mod certificate;
pub mod hpke;
pub mod keys;
pub mod keystore;
pub mod link;
pub mod roster;
pub mod safety;

#[cfg(test)]
mod tests;

#[doc(hidden)]
pub mod fuzzing;

pub use certificate::{
    DeviceCertificate, DeviceCredential, DeviceRevocation, DeviceSigningAuthority,
    DeviceSigningCertificate, Validity, WHOLE_DEVICE,
};
pub use error::Error;
pub use hpke::SealedToDevice;
pub use keys::{
    DeviceId, DeviceKeys, DevicePublicKeys, DeviceSigningKey, DisplayName, Ed25519PublicKey,
    HybridPublicKey, HybridSignature, IdentityFingerprint, IdentityRoot, KemAlgorithm,
    SignatureAlgorithm, XWingPublicKey,
};
pub use keystore::{
    DeviceKeystore, DeviceSyncKey, IdentityExport, ImportedIdentity, RecoveryBlob, RecoveryKey,
    StoreKey,
};
pub use link::{
    ExistingAwaitingKeys, ExistingDeviceLink, LinkBundle, LinkCode, LinkDeviceKeys, LinkPake,
    NewAwaitingBundle, NewDeviceKeys, NewDeviceLink, ProvisionedDevice, ProvisioningBundle,
};
pub use roster::{DeviceRoster, VerifiedDevice, VerifiedIdentity};
pub use safety::{QrComparison, SafetyNumber, SafetyQr};

/// The MLS credential type that carries a [`DeviceCredential`] (private-use range,
/// docs/e2ee.md §3.3).
pub const MLS_CREDENTIAL_TYPE: u16 = 0xF4E1;

/// A new identity with its first device, as created at setup (docs/e2ee.md §3.5): the
/// root (to be sealed under a recovery key or deleted), the device-signing authority of
/// generation 1, and the device's keys, certificate and credential.
pub struct NewIdentity {
    pub root: IdentityRoot,
    pub device_signing: DeviceSigningAuthority,
    pub device: DeviceKeys,
    pub device_certificate: DeviceCertificate,
    pub device_credential: DeviceCredential,
}

impl NewIdentity {
    pub fn create(display_name: &DisplayName, validity: Validity, now: u64) -> Result<Self, Error> {
        let root = IdentityRoot::generate()?;
        let device_signing = root.first_device_signing_authority(now)?;
        let device = DeviceKeys::generate()?;
        let (device_certificate, device_credential) =
            device_signing.certify_device(&device.public_keys(), display_name, validity)?;
        Ok(Self {
            root,
            device_signing,
            device,
            device_certificate,
            device_credential,
        })
    }

    /// The verified view of this identity, as any verifier builds it.
    pub fn verified_identity(&self) -> Result<VerifiedIdentity, Error> {
        VerifiedIdentity::verify(
            self.root.public_key().clone(),
            self.device_signing.certificate().clone(),
        )
    }

    /// The device keystore for the first device, with its device-signing key.
    pub fn keystore(&self) -> Result<DeviceKeystore, Error> {
        let identity = self.verified_identity()?;
        let device = DeviceKeys::from_seeds(
            self.device.device_id(),
            self.device.key_epoch(),
            self.device.leaf_seed.duplicate(),
            self.device.kem_seed.duplicate(),
        )?;
        DeviceKeystore::new(
            identity,
            device,
            self.device_certificate.clone(),
            self.device_credential.clone(),
        )?
        .with_device_signing(self.device_signing.duplicate())
    }
}

impl std::fmt::Debug for NewIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewIdentity")
            .field("root", &self.root)
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}
