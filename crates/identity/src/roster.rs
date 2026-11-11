// SPDX-License-Identifier: EUPL-1.2
//! Verification policy: a verified identity (root and current device-signing key) and
//! the devices it has certified and revoked (docs/e2ee.md §3.3, §3.6).
//!
//! A credential in an MLS leaf is accepted only if all of these hold:
//!
//! 1. it is signed by the credential key of the identity's *current* device-signing
//!    certificate (same identity, same generation);
//! 2. it certifies the leaf's signature key;
//! 3. `now` is inside its validity window and inside its device certificate's;
//! 4. the device has a certificate in the identity's device list with the same key
//!    epoch and leaf key (a newer epoch supersedes it; an unknown or newer epoch is
//!    "not in the list yet", which key transparency reports as pending);
//! 5. no revocation covers its key epoch.
//!
//! Freshness (which generation is current, which devices exist) comes from the
//! identity's key transparency record; this module checks what that record contains.

use std::collections::BTreeMap;

use crate::Error;
use crate::certificate::{
    DeviceCertificate, DeviceCredential, DeviceRevocation, DeviceSigningCertificate, Validity,
};
use crate::keys::{DeviceId, DisplayName, HybridPublicKey, IdentityFingerprint, XWingPublicKey};

/// An identity whose device-signing certificate verified under its root key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedIdentity {
    root: HybridPublicKey,
    fingerprint: IdentityFingerprint,
    device_signing: DeviceSigningCertificate,
}

impl VerifiedIdentity {
    /// Checks the device-signing certificate under `root`.
    pub fn verify(
        root: HybridPublicKey,
        device_signing: DeviceSigningCertificate,
    ) -> Result<Self, Error> {
        device_signing.verify(&root)?;
        Ok(Self {
            fingerprint: root.fingerprint(),
            root,
            device_signing,
        })
    }

    /// The same identity under a newer device-signing certificate (after a rotation).
    /// The generation must increase.
    pub fn rotate(&self, newer: DeviceSigningCertificate) -> Result<Self, Error> {
        if newer.generation() <= self.generation() {
            return Err(Error::WrongGeneration {
                expected: self.generation() + 1,
                found: newer.generation(),
            });
        }
        Self::verify(self.root.clone(), newer)
    }

    pub fn fingerprint(&self) -> IdentityFingerprint {
        self.fingerprint
    }

    pub fn root_key(&self) -> &HybridPublicKey {
        &self.root
    }

    pub fn device_signing_certificate(&self) -> &DeviceSigningCertificate {
        &self.device_signing
    }

    pub fn generation(&self) -> u32 {
        self.device_signing.generation()
    }
}

/// A device that passed every check, as the UI and the crypto worker see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedDevice {
    pub identity: IdentityFingerprint,
    pub device_id: DeviceId,
    pub key_epoch: u32,
    pub display_name: DisplayName,
    pub kem_key: XWingPublicKey,
    pub validity: Validity,
}

impl VerifiedDevice {
    fn from_certificate(certificate: &DeviceCertificate) -> Self {
        Self {
            identity: certificate.identity(),
            device_id: certificate.device_id(),
            key_epoch: certificate.key_epoch(),
            display_name: certificate.display_name().clone(),
            kem_key: certificate.kem_key().clone(),
            validity: certificate.validity(),
        }
    }
}

/// One identity's certified devices and revocations under its current generation.
#[derive(Clone, Debug)]
pub struct DeviceRoster {
    identity: VerifiedIdentity,
    devices: BTreeMap<DeviceId, DeviceCertificate>,
    revoked_through: BTreeMap<DeviceId, u32>,
}

impl DeviceRoster {
    pub fn new(identity: VerifiedIdentity) -> Self {
        Self {
            identity,
            devices: BTreeMap::new(),
            revoked_through: BTreeMap::new(),
        }
    }

    pub fn identity(&self) -> &VerifiedIdentity {
        &self.identity
    }

    /// Adds a verified device certificate. A newer key epoch replaces an older one; an
    /// older one is refused ([`Error::Superseded`]), and a different certificate for the
    /// same epoch is a [`Error::Conflict`] (the device-signing key signed two).
    pub fn add_certificate(&mut self, certificate: DeviceCertificate) -> Result<(), Error> {
        certificate.verify(&self.identity)?;
        match self.devices.get(&certificate.device_id()) {
            Some(known) if known.key_epoch() > certificate.key_epoch() => Err(Error::Superseded),
            Some(known) if known.key_epoch() == certificate.key_epoch() => {
                if *known == certificate {
                    Ok(())
                } else {
                    Err(Error::Conflict)
                }
            }
            _ => {
                self.devices.insert(certificate.device_id(), certificate);
                Ok(())
            }
        }
    }

    /// Adds a verified revocation; revocations only ever widen.
    pub fn add_revocation(&mut self, revocation: DeviceRevocation) -> Result<(), Error> {
        revocation.verify(&self.identity)?;
        let through = self
            .revoked_through
            .entry(revocation.device_id())
            .or_insert(0);
        *through = (*through).max(revocation.through_key_epoch());
        Ok(())
    }

    fn check_revocation(&self, device_id: DeviceId, key_epoch: u32) -> Result<(), Error> {
        match self.revoked_through.get(&device_id) {
            Some(&through) if through >= key_epoch => Err(Error::Revoked),
            _ => Ok(()),
        }
    }

    /// The device's current certificate, if it is valid at `now` and not revoked.
    pub fn device(&self, device_id: DeviceId, now: u64) -> Result<VerifiedDevice, Error> {
        let certificate = self.devices.get(&device_id).ok_or(Error::UnknownDevice)?;
        self.check_revocation(device_id, certificate.key_epoch())?;
        certificate.validity().check(now)?;
        Ok(VerifiedDevice::from_certificate(certificate))
    }

    /// Every device valid at `now` and not revoked.
    pub fn active_devices(&self, now: u64) -> Vec<VerifiedDevice> {
        self.devices
            .keys()
            .filter_map(|&id| self.device(id, now).ok())
            .collect()
    }

    /// The full check of an MLS leaf's credential (see the module documentation).
    pub fn check_credential(
        &self,
        credential: &DeviceCredential,
        leaf_key: &[u8],
        now: u64,
    ) -> Result<VerifiedDevice, Error> {
        credential.verify(&self.identity, leaf_key, now)?;
        self.check_revocation(credential.device_id(), credential.key_epoch())?;
        let certificate = self
            .devices
            .get(&credential.device_id())
            .ok_or(Error::UnknownDevice)?;
        if certificate.key_epoch() > credential.key_epoch() {
            return Err(Error::Superseded);
        }
        if certificate.key_epoch() < credential.key_epoch() {
            return Err(Error::UnknownDevice);
        }
        if !certificate.matches_credential(credential) {
            return Err(Error::KeyMismatch);
        }
        certificate.validity().check(now)?;
        Ok(VerifiedDevice::from_certificate(certificate))
    }
}
