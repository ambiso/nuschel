// SPDX-License-Identifier: EUPL-1.2
//! Typed directory values over the identity crate's formats.
//!
//! The directory itself commits to versioned [`DirectoryValue`]s. These types give the
//! formats Nuschel publishes:
//!
//! | Format | Label | Value |
//! | --- | --- | --- |
//! | 1 | `acct:` | [`AccountBinding`]: identity root public key ‖ roster commitment |
//! | 2 | `name:` | [`NameBinding`]: the account id |
//!
//! A later roster or binding format gets a new number; old proofs keep verifying, and
//! clients that do not know a format report [`ValueError::UnknownFormat`].

use crate::error::KtError;
use crate::types::{AccountId, DirectoryValue};
use nuschel_identity::{
    DeviceCertificate, DeviceRevocation, DeviceRoster, DeviceSigningCertificate, HybridPublicKey,
    IdentityFingerprint, VerifiedIdentity,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// [`DirectoryValue::format`] of an [`AccountBinding`], version 1.
pub const FORMAT_ACCOUNT_BINDING_V1: u16 = 1;
/// [`DirectoryValue::format`] of a [`NameBinding`], version 1.
pub const FORMAT_NAME_BINDING_V1: u16 = 2;

const ROSTER_CONTEXT: &[u8] = b"nuschel-kt/v1 roster\0";
/// Most device certificates or revocations in one roster commitment.
pub const MAX_ROSTER_ENTRIES: usize = 4096;

/// A directory value that is not the expected typed value.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ValueError {
    #[error("value format {0} is not known to this client")]
    UnknownFormat(u16),
    #[error("malformed value")]
    Malformed,
}

/// A commitment to an identity's device roster under its current device-signing key:
///
/// ```text
/// SHA-256("nuschel-kt/v1 roster\0"
///         || SHA-256(DeviceSigningCertificate)
///         || uint16 n || SHA-256(DeviceCertificate)[n]     // DeviceCertificate::hash, sorted
///         || uint16 m || SHA-256(DeviceRevocation)[m])     // sorted
/// ```
///
/// Sorting makes it independent of the order in which certificates are fetched;
/// duplicates are refused. The certificates themselves come from the server's directory
/// API; [`roster_for_binding`] checks them against this commitment.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RosterCommitment(pub [u8; 32]);

impl RosterCommitment {
    pub fn compute(
        device_signing: &DeviceSigningCertificate,
        devices: &[DeviceCertificate],
        revocations: &[DeviceRevocation],
    ) -> Result<Self, ValueError> {
        let sorted = |mut hashes: Vec<[u8; 32]>| -> Result<Vec<[u8; 32]>, ValueError> {
            if hashes.len() > MAX_ROSTER_ENTRIES {
                return Err(ValueError::Malformed);
            }
            hashes.sort_unstable();
            if hashes.windows(2).any(|w| w[0] == w[1]) {
                return Err(ValueError::Malformed);
            }
            Ok(hashes)
        };
        let devices = sorted(devices.iter().map(DeviceCertificate::hash).collect())?;
        let revocations = sorted(
            revocations
                .iter()
                .map(|r| Sha256::digest(r.encode()).into())
                .collect(),
        )?;
        let mut h = Sha256::new();
        h.update(ROSTER_CONTEXT);
        h.update(Sha256::digest(device_signing.encode()));
        for list in [&devices, &revocations] {
            h.update((list.len() as u16).to_be_bytes());
            for hash in list {
                h.update(hash);
            }
        }
        Ok(Self(h.finalize().into()))
    }
}

/// The value of an `acct:` label: the identity an account is bound to, and that
/// identity's current device roster.
///
/// `data = HybridPublicKey (1986 bytes) || opaque roster_commitment[32]` (2018 bytes).
/// The root key is in the value (not only its fingerprint) so that one lookup gives a
/// client everything it needs to verify the device-signing certificate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AccountBinding {
    root: HybridPublicKey,
    roster: RosterCommitment,
}

impl AccountBinding {
    pub fn new(root: HybridPublicKey, roster: RosterCommitment) -> Self {
        Self { root, roster }
    }

    pub fn root(&self) -> &HybridPublicKey {
        &self.root
    }

    /// The identity (what TOFU pins and safety numbers name).
    pub fn fingerprint(&self) -> IdentityFingerprint {
        self.root.fingerprint()
    }

    pub fn roster(&self) -> RosterCommitment {
        self.roster
    }

    pub fn to_value(&self) -> DirectoryValue {
        let mut data = self.root.encode();
        data.extend_from_slice(&self.roster.0);
        DirectoryValue::new(FORMAT_ACCOUNT_BINDING_V1, data)
            .expect("2018 bytes fit a directory value")
    }

    pub fn from_value(value: &DirectoryValue) -> Result<Self, ValueError> {
        if value.format() != FORMAT_ACCOUNT_BINDING_V1 {
            return Err(ValueError::UnknownFormat(value.format()));
        }
        let data = value.data();
        if data.len() != HybridPublicKey::LEN + 32 {
            return Err(ValueError::Malformed);
        }
        let (key, roster) = data.split_at(HybridPublicKey::LEN);
        let root = HybridPublicKey::decode(key).map_err(|_| ValueError::Malformed)?;
        let roster = RosterCommitment(roster.try_into().map_err(|_| ValueError::Malformed)?);
        Ok(Self { root, roster })
    }
}

/// The value of a `name:` label: the account behind a registered user name.
/// `data = opaque account_id[16]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NameBinding(pub AccountId);

impl NameBinding {
    pub fn to_value(&self) -> DirectoryValue {
        DirectoryValue::new(FORMAT_NAME_BINDING_V1, self.0.0.to_vec()).expect("16 bytes")
    }

    pub fn from_value(value: &DirectoryValue) -> Result<Self, ValueError> {
        if value.format() != FORMAT_NAME_BINDING_V1 {
            return Err(ValueError::UnknownFormat(value.format()));
        }
        let id = value.data().try_into().map_err(|_| ValueError::Malformed)?;
        Ok(Self(AccountId(id)))
    }
}

/// Builds the device roster of a verified account binding from the certificates the
/// server's directory API returned (docs/e2ee.md §5.4).
///
/// - [`KtError::RosterMismatch`]: the certificates are not the ones the log commits to
///   (the directory API and the log disagree).
/// - [`KtError::InvalidRecord`]: the committed certificates do not verify under the
///   bound root (the identity's own devices published something broken, or the server
///   bound a key it cannot back with certificates).
pub fn roster_for_binding(
    binding: &AccountBinding,
    device_signing: &DeviceSigningCertificate,
    devices: &[DeviceCertificate],
    revocations: &[DeviceRevocation],
) -> Result<DeviceRoster, KtError> {
    let commitment = RosterCommitment::compute(device_signing, devices, revocations)
        .map_err(|_| KtError::RosterMismatch)?;
    if commitment != binding.roster() {
        return Err(KtError::RosterMismatch);
    }
    let invalid = |e: nuschel_identity::Error| KtError::InvalidRecord(e.to_string());
    // One current certificate per device: a superseded key epoch does not belong in the
    // roster, and accepting it would make the result depend on the order.
    let mut ids: Vec<_> = devices.iter().map(DeviceCertificate::device_id).collect();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        return Err(KtError::InvalidRecord(
            "two certificates for the same device".into(),
        ));
    }
    let identity =
        VerifiedIdentity::verify(binding.root().clone(), device_signing.clone()).map_err(invalid)?;
    let mut roster = DeviceRoster::new(identity);
    for certificate in devices {
        roster
            .add_certificate(certificate.clone())
            .map_err(invalid)?;
    }
    for revocation in revocations {
        roster.add_revocation(revocation.clone()).map_err(invalid)?;
    }
    Ok(roster)
}
