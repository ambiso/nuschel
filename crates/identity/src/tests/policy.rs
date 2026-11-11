// SPDX-License-Identifier: EUPL-1.2
//! What a verifier accepts: validity, revocation, supersession, generations.

use super::*;
use crate::{DeviceKeys, DeviceRoster, Error, NewIdentity};

fn roster(f: &Fixture) -> DeviceRoster {
    let mut roster = DeviceRoster::new(f.identity.clone());
    roster
        .add_certificate(f.id.device_certificate.clone())
        .unwrap();
    roster
}

#[test]
fn a_certified_device_is_accepted() {
    let f = fixture();
    let roster = roster(&f);
    let device = roster
        .check_credential(
            &f.id.device_credential,
            f.id.device.leaf_key().as_bytes(),
            NOW,
        )
        .unwrap();
    assert_eq!(device.identity, f.id.root.fingerprint());
    assert_eq!(device.device_id, f.id.device.device_id());
    assert_eq!(device.display_name.as_str(), "laptop");
    assert_eq!(&device.kem_key, f.id.device.kem_key());
    assert_eq!(roster.active_devices(NOW), vec![device]);
}

#[test]
fn validity_window_is_inclusive_exclusive() {
    let f = fixture();
    let roster = roster(&f);
    let leaf = f.id.device.leaf_key().as_bytes();
    let check = |now| roster.check_credential(&f.id.device_credential, leaf, now);
    assert_eq!(check(NOW - 101).err(), Some(Error::NotYetValid));
    check(NOW - 100).unwrap();
    check(NOW + 999).unwrap();
    assert_eq!(check(NOW + 1000).err(), Some(Error::Expired));
    assert!(roster.active_devices(NOW + 1000).is_empty());
}

#[test]
fn wrong_leaf_key_and_unknown_devices() {
    let f = fixture();
    let leaf = f.id.device.leaf_key().as_bytes();
    let other = DeviceKeys::generate().unwrap();
    let roster_ = roster(&f);
    assert_eq!(
        roster_
            .check_credential(&f.id.device_credential, other.leaf_key().as_bytes(), NOW)
            .err(),
        Some(Error::KeyMismatch)
    );
    // Not in the identity's device list (key transparency: pending).
    let empty = DeviceRoster::new(f.identity.clone());
    assert_eq!(
        empty
            .check_credential(&f.id.device_credential, leaf, NOW)
            .err(),
        Some(Error::UnknownDevice)
    );
    // A credential of another identity, presented against this one.
    let stranger = fixture();
    assert_eq!(
        roster_
            .check_credential(
                &stranger.id.device_credential,
                stranger.id.device.leaf_key().as_bytes(),
                NOW
            )
            .err(),
        Some(Error::WrongIdentity)
    );
    // A stranger's device certificate cannot join this roster.
    let mut roster_ = roster_;
    assert_eq!(
        roster_
            .add_certificate(stranger.id.device_certificate.clone())
            .err(),
        Some(Error::WrongIdentity)
    );
    assert_eq!(
        roster_.add_revocation(stranger.revocation.clone()).err(),
        Some(Error::WrongIdentity)
    );
}

#[test]
fn revocation_covers_key_epochs() {
    let f = fixture();
    let authority = &f.id.device_signing;
    let name = DisplayName::new("laptop").unwrap();
    let epoch1 = &f.id.device;
    let epoch2 = epoch1.rotate().unwrap();
    let (cert2, cred2) = authority
        .certify_device(&epoch2.public_keys(), &name, validity())
        .unwrap();

    // Revoking epoch 1 only: the rotated keys stay valid.
    let mut roster = roster(&f);
    roster.add_certificate(cert2.clone()).unwrap();
    roster
        .add_revocation(authority.revoke_device(epoch1.device_id(), 1, NOW).unwrap())
        .unwrap();
    assert_eq!(
        roster
            .check_credential(&f.id.device_credential, epoch1.leaf_key().as_bytes(), NOW)
            .err(),
        Some(Error::Revoked)
    );
    roster
        .check_credential(&cred2, epoch2.leaf_key().as_bytes(), NOW)
        .unwrap();

    // Revoking the whole device.
    roster
        .add_revocation(
            authority
                .revoke_device(epoch1.device_id(), WHOLE_DEVICE, NOW)
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        roster
            .check_credential(&cred2, epoch2.leaf_key().as_bytes(), NOW)
            .err(),
        Some(Error::Revoked)
    );
    assert_eq!(
        roster.device(epoch1.device_id(), NOW).err(),
        Some(Error::Revoked)
    );
    // Revocations only widen: a narrower one later changes nothing.
    roster
        .add_revocation(authority.revoke_device(epoch1.device_id(), 1, NOW).unwrap())
        .unwrap();
    assert_eq!(
        roster.device(epoch1.device_id(), NOW).err(),
        Some(Error::Revoked)
    );
    assert!(roster.active_devices(NOW).is_empty());
}

#[test]
fn a_newer_key_epoch_supersedes() {
    let f = fixture();
    let authority = &f.id.device_signing;
    let name = DisplayName::new("laptop").unwrap();
    let epoch2 = f.id.device.rotate().unwrap();
    let (cert2, cred2) = authority
        .certify_device(&epoch2.public_keys(), &name, validity())
        .unwrap();

    let mut roster = roster(&f);
    // A credential for an epoch the list does not have yet.
    assert_eq!(
        roster
            .check_credential(&cred2, epoch2.leaf_key().as_bytes(), NOW)
            .err(),
        Some(Error::UnknownDevice)
    );
    roster.add_certificate(cert2.clone()).unwrap();
    roster
        .check_credential(&cred2, epoch2.leaf_key().as_bytes(), NOW)
        .unwrap();
    assert_eq!(
        roster
            .check_credential(
                &f.id.device_credential,
                f.id.device.leaf_key().as_bytes(),
                NOW
            )
            .err(),
        Some(Error::Superseded)
    );
    assert_eq!(
        roster
            .add_certificate(f.id.device_certificate.clone())
            .err(),
        Some(Error::Superseded)
    );
    // The same certificate twice is fine; a different one for the same epoch is not.
    roster.add_certificate(cert2).unwrap();
    let (renamed, _) = authority
        .certify_device(
            &epoch2.public_keys(),
            &DisplayName::new("desk").unwrap(),
            validity(),
        )
        .unwrap();
    assert_eq!(roster.add_certificate(renamed).err(), Some(Error::Conflict));
}

#[test]
fn credential_must_match_the_listed_certificate() {
    let f = fixture();
    let authority = &f.id.device_signing;
    // A credential for the same device id and epoch but another leaf key: the
    // device-signing key signed it, but the device list says otherwise.
    let impostor = DeviceKeys::generate().unwrap();
    let mut keys = impostor.public_keys();
    keys.device_id = f.id.device.device_id();
    let credential = authority.issue_credential(&keys, validity()).unwrap();
    let roster = roster(&f);
    assert_eq!(
        roster
            .check_credential(&credential, impostor.leaf_key().as_bytes(), NOW)
            .err(),
        Some(Error::KeyMismatch)
    );
}

#[test]
fn rotating_the_device_signing_key_keeps_the_identity() {
    let f = fixture();
    let root = &f.id.root;
    let rotated = root
        .rotate_device_signing_key(f.id.device_signing.certificate(), NOW)
        .unwrap();
    assert_eq!(rotated.generation(), 2);
    assert_eq!(rotated.identity(), f.identity.fingerprint());
    let identity2 = f.identity.rotate(rotated.certificate().clone()).unwrap();
    assert_eq!(identity2.fingerprint(), f.identity.fingerprint());

    // Generation 1 credentials no longer verify.
    let mut roster2 = DeviceRoster::new(identity2.clone());
    assert_eq!(
        roster2
            .add_certificate(f.id.device_certificate.clone())
            .err(),
        Some(Error::WrongGeneration {
            expected: 2,
            found: 1
        })
    );
    assert_eq!(
        roster2
            .check_credential(
                &f.id.device_credential,
                f.id.device.leaf_key().as_bytes(),
                NOW
            )
            .err(),
        Some(Error::WrongGeneration {
            expected: 2,
            found: 1
        })
    );
    assert_eq!(
        roster2.add_revocation(f.revocation.clone()).err(),
        Some(Error::WrongGeneration {
            expected: 2,
            found: 1
        })
    );
    // Re-certified under generation 2, the same device is accepted again.
    let (cert, cred) = rotated
        .certify_device(
            &f.id.device.public_keys(),
            &DisplayName::new("laptop").unwrap(),
            validity(),
        )
        .unwrap();
    roster2.add_certificate(cert).unwrap();
    roster2
        .check_credential(&cred, f.id.device.leaf_key().as_bytes(), NOW)
        .unwrap();

    // Not backwards, not sideways, not from another root.
    assert!(
        identity2
            .rotate(f.id.device_signing.certificate().clone())
            .is_err()
    );
    assert!(identity2.rotate(rotated.certificate().clone()).is_err());
    let stranger = NewIdentity::create(&DisplayName::new("x").unwrap(), validity(), NOW).unwrap();
    let foreign = stranger
        .root
        .rotate_device_signing_key(stranger.device_signing.certificate(), NOW)
        .unwrap();
    assert_eq!(
        f.identity.rotate(foreign.certificate().clone()).err(),
        Some(Error::WrongIdentity)
    );
    assert_eq!(
        root.rotate_device_signing_key(stranger.device_signing.certificate(), NOW)
            .err(),
        Some(Error::WrongIdentity)
    );
    // A device-signing certificate of another identity does not verify under this root.
    assert_eq!(
        crate::VerifiedIdentity::verify(
            root.public_key().clone(),
            stranger.device_signing.certificate().clone()
        )
        .err(),
        Some(Error::WrongIdentity)
    );
}

#[test]
fn authority_needs_the_matching_key() {
    let a = fixture();
    let b = fixture();
    let key = crate::DeviceSigningKey::generate().unwrap();
    assert_eq!(
        crate::DeviceSigningAuthority::new(key, a.id.device_signing.certificate().clone()).err(),
        Some(Error::KeyMismatch)
    );
    assert!(
        a.id.device_signing.certificate() != b.id.device_signing.certificate(),
        "fresh keys per identity"
    );
}
