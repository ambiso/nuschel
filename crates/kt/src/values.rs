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
//! | 3 | `acct:` | [`AccountBinding`] with the hash of the identity's [`AccountStatement`] |
//!
//! Nuschel servers publish format 3; [`verify_account_record`] requires it, because
//! without the identity's signed statement a server could bind someone else's
//! identity to one of its accounts (and make it look like the same person as on
//! another server).
//!
//! A later roster or binding format gets a new number; old proofs keep verifying, and
//! clients that do not know a format report [`ValueError::UnknownFormat`].

use crate::error::KtError;
use crate::log::LogId;
use crate::types::{AccountId, DirectoryValue};
use nuschel_identity::{
    AccountStatement, DeviceCertificate, DeviceRevocation, DeviceRoster, DeviceSigningCertificate,
    HybridPublicKey, IdentityFingerprint, VerifiedIdentity,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// [`DirectoryValue::format`] of an [`AccountBinding`], version 1.
pub const FORMAT_ACCOUNT_BINDING_V1: u16 = 1;
/// [`DirectoryValue::format`] of a [`NameBinding`], version 1.
pub const FORMAT_NAME_BINDING_V1: u16 = 2;
/// [`DirectoryValue::format`] of an [`AccountBinding`] with an account statement.
pub const FORMAT_ACCOUNT_BINDING_V2: u16 = 3;

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
/// - Format 1: `data = HybridPublicKey (1986 bytes) || opaque roster_commitment[32]`
///   (2018 bytes).
/// - Format 3: the same followed by `opaque statement_hash[32]`, the
///   [`AccountStatement::hash`] of the identity's statement for this account (2050
///   bytes).
///
/// The root key is in the value (not only its fingerprint) so that one lookup gives a
/// client everything it needs to verify the device-signing certificate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AccountBinding {
    root: HybridPublicKey,
    roster: RosterCommitment,
    statement: Option<[u8; 32]>,
}

impl AccountBinding {
    /// A format 1 binding, without an account statement.
    pub fn new(root: HybridPublicKey, roster: RosterCommitment) -> Self {
        Self {
            root,
            roster,
            statement: None,
        }
    }

    /// A format 3 binding, committing to the identity's statement for the account.
    pub fn with_statement(
        root: HybridPublicKey,
        roster: RosterCommitment,
        statement: &AccountStatement,
    ) -> Self {
        Self {
            root,
            roster,
            statement: Some(statement.hash()),
        }
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

    /// The hash of the account statement (format 3).
    pub fn statement(&self) -> Option<&[u8; 32]> {
        self.statement.as_ref()
    }

    pub fn to_value(&self) -> DirectoryValue {
        let mut data = self.root.encode();
        data.extend_from_slice(&self.roster.0);
        let format = match &self.statement {
            Some(hash) => {
                data.extend_from_slice(hash);
                FORMAT_ACCOUNT_BINDING_V2
            }
            None => FORMAT_ACCOUNT_BINDING_V1,
        };
        DirectoryValue::new(format, data).expect("2050 bytes fit a directory value")
    }

    pub fn from_value(value: &DirectoryValue) -> Result<Self, ValueError> {
        let with_statement = match value.format() {
            FORMAT_ACCOUNT_BINDING_V1 => false,
            FORMAT_ACCOUNT_BINDING_V2 => true,
            other => return Err(ValueError::UnknownFormat(other)),
        };
        let data = value.data();
        let expected = HybridPublicKey::LEN + 32 + if with_statement { 32 } else { 0 };
        if data.len() != expected {
            return Err(ValueError::Malformed);
        }
        let (key, rest) = data.split_at(HybridPublicKey::LEN);
        let root = HybridPublicKey::decode(key).map_err(|_| ValueError::Malformed)?;
        let (roster, statement) = rest.split_at(32);
        let roster = RosterCommitment(roster.try_into().map_err(|_| ValueError::Malformed)?);
        let statement = if with_statement {
            Some(statement.try_into().map_err(|_| ValueError::Malformed)?)
        } else {
            None
        };
        Ok(Self {
            root,
            roster,
            statement,
        })
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
    let identity = VerifiedIdentity::verify(binding.root().clone(), device_signing.clone())
        .map_err(invalid)?;
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

/// The certificates behind an account binding, as the server's directory API returns
/// them beside a lookup: the identity's device-signing certificate, its device
/// certificates and revocations (the roster), and the account statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityRecord {
    pub device_signing: DeviceSigningCertificate,
    pub devices: Vec<DeviceCertificate>,
    pub revocations: Vec<DeviceRevocation>,
    pub statement: AccountStatement,
}

/// What [`verify_account_record`] establishes about an account.
#[derive(Clone, Debug)]
pub struct VerifiedAccount {
    pub identity: IdentityFingerprint,
    pub roster: DeviceRoster,
    pub statement: AccountStatement,
}

/// Checks the record behind a verified `acct:` value (docs/e2ee.md §3.8, §5.4): the
/// certificates are the roster the log commits to and verify under the bound root,
/// and the identity itself said, with its device-signing key, that this account on
/// this log (`log_id`) is its own.
///
/// - [`KtError::RosterMismatch`]: the record is not what the log commits to.
/// - [`KtError::InvalidRecord`]: a format 1 binding (no statement), or a statement or
///   certificate that does not verify, names another account or another log.
pub fn verify_account_record(
    binding: &AccountBinding,
    record: &IdentityRecord,
    log_id: LogId,
    account: AccountId,
) -> Result<VerifiedAccount, KtError> {
    let roster = roster_for_binding(
        binding,
        &record.device_signing,
        &record.devices,
        &record.revocations,
    )?;
    let invalid = |reason: &str| KtError::InvalidRecord(reason.to_owned());
    let committed = binding
        .statement()
        .ok_or_else(|| invalid("the binding carries no account statement"))?;
    if record.statement.hash() != *committed {
        return Err(KtError::RosterMismatch);
    }
    record
        .statement
        .verify(roster.identity())
        .map_err(|e| KtError::InvalidRecord(format!("account statement: {e}")))?;
    if record.statement.server() != &log_id.0 {
        return Err(invalid("the account statement names another server"));
    }
    if record.statement.account() != &account.0 {
        return Err(invalid("the account statement names another account"));
    }
    Ok(VerifiedAccount {
        identity: binding.fingerprint(),
        roster,
        statement: record.statement.clone(),
    })
}
