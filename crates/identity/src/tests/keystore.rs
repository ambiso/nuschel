// SPDX-License-Identifier: EUPL-1.2
//! The encrypted keystore, the recovery key, recovery and the identity export.

use super::*;
use crate::keystore::{Purpose, Sealed};
use crate::{
    DeviceKeys, DeviceKeystore, DeviceRoster, DeviceSyncKey, Error, IdentityExport, RecoveryBlob,
    RecoveryKey, StoreKey,
};

fn full_keystore(f: &Fixture, recovery_key: &RecoveryKey) -> DeviceKeystore {
    f.id.keystore()
        .unwrap()
        .with_recovery_blob(RecoveryBlob::seal(&f.id.root, recovery_key).unwrap())
        .with_sync_key(DeviceSyncKey::generate(1).unwrap())
}

#[test]
fn keystore_round_trip_restores_working_keys() {
    let f = fixture();
    let recovery_key = RecoveryKey::generate().unwrap();
    let key = StoreKey::generate().unwrap();
    let store = full_keystore(&f, &recovery_key);
    let sealed = store.seal(&key).unwrap();
    let opened = DeviceKeystore::open(&key, &sealed).unwrap();

    assert_eq!(opened.identity(), &f.identity);
    assert_eq!(opened.device().public_keys(), f.id.device.public_keys());
    assert_eq!(opened.device_certificate(), &f.id.device_certificate);
    assert_eq!(opened.device_credential(), &f.id.device_credential);
    assert_eq!(opened.sync_key().unwrap().epoch(), 1);
    // The secrets work: the leaf key signs, the X-Wing key decrypts, the device-signing
    // key certifies, the recovery blob opens.
    let signature = opened.device().sign_leaf(b"mls").unwrap();
    assert_eq!(signature, f.id.device.sign_leaf(b"mls").unwrap());
    let sealed_to = crate::SealedToDevice::seal(&f.id.device.public_keys(), b"t", b"k").unwrap();
    assert_eq!(&opened.device().open(&sealed_to, b"t").unwrap()[..], b"k");
    let other = DeviceKeys::generate().unwrap();
    let (cert, cred) = opened
        .device_signing()
        .unwrap()
        .certify_device(
            &other.public_keys(),
            &DisplayName::new("x").unwrap(),
            validity(),
        )
        .unwrap();
    let mut roster = DeviceRoster::new(f.identity.clone());
    roster.add_certificate(cert).unwrap();
    roster
        .check_credential(&cred, other.leaf_key().as_bytes(), NOW)
        .unwrap();
    let root = opened.recovery_blob().unwrap().open(&recovery_key).unwrap();
    assert_eq!(root.fingerprint(), f.identity.fingerprint());

    // Two seals of the same keystore differ (fresh salt), both open.
    let again = store.seal(&key).unwrap();
    assert_ne!(again, sealed);
    DeviceKeystore::open(&key, &again).unwrap();

    // Debug output names the identity, never key material.
    let printed = format!("{opened:?}");
    assert!(printed.contains(&f.identity.fingerprint().to_hex()));
    assert!(!printed.contains("Secret32("));
}

#[test]
fn keystore_without_optional_parts() {
    let f = fixture();
    let key = StoreKey::generate().unwrap();
    let store = DeviceKeystore::new(
        f.identity.clone(),
        DeviceKeys::from_seeds(
            f.id.device.device_id(),
            f.id.device.key_epoch(),
            f.id.device.leaf_seed.duplicate(),
            f.id.device.kem_seed.duplicate(),
        )
        .unwrap(),
        f.id.device_certificate.clone(),
        f.id.device_credential.clone(),
    )
    .unwrap();
    let opened = DeviceKeystore::open(&key, &store.seal(&key).unwrap()).unwrap();
    assert!(opened.device_signing().is_none());
    assert!(opened.recovery_blob().is_none());
    assert!(opened.sync_key().is_none());
}

#[test]
fn keystore_rejects_inconsistent_parts() {
    let f = fixture();
    let other = DeviceKeys::generate().unwrap();
    assert_eq!(
        DeviceKeystore::new(
            f.identity.clone(),
            other,
            f.id.device_certificate.clone(),
            f.id.device_credential.clone()
        )
        .err(),
        Some(Error::KeyMismatch)
    );
    let stranger = fixture();
    assert!(
        f.id.keystore()
            .unwrap()
            .with_device_signing(stranger.id.device_signing.duplicate())
            .is_err()
    );
}

#[test]
fn keystore_tampering_wrong_key_truncation_and_purpose() {
    let f = fixture();
    let recovery_key = RecoveryKey::generate().unwrap();
    let key = StoreKey::generate().unwrap();
    let sealed = full_keystore(&f, &recovery_key).seal(&key).unwrap();

    assert_eq!(
        DeviceKeystore::open(&StoreKey::generate().unwrap(), &sealed).err(),
        Some(Error::DecryptionFailed)
    );
    let positions: Vec<usize> = (0..sealed.len())
        .filter(|&i| i < 64 || i % 7 == 0 || i + 64 >= sealed.len())
        .collect();
    assert_flips_rejected(&sealed, &positions, |b| DeviceKeystore::open(&key, b));
    for len in (0..sealed.len()).filter(|&l| l < 64 || l % 13 == 0) {
        assert!(DeviceKeystore::open(&key, &sealed[..len]).is_err());
    }
    assert_trailing_rejected(&sealed, |b| DeviceKeystore::open(&key, b));
    assert_header_checked(&sealed, |b| DeviceKeystore::open(&key, b));

    // The purpose byte is authenticated: relabelled, the envelope is refused by the
    // intended reader and does not decrypt for the other.
    let mut relabelled = sealed.clone();
    relabelled[2] = Purpose::RecoveryBlob as u8;
    assert!(matches!(
        DeviceKeystore::open(&key, &relabelled),
        Err(Error::Malformed(_))
    ));
    let envelope = Sealed::decode(&relabelled).unwrap();
    assert_eq!(
        envelope.open(&key.to_bytes(), Purpose::RecoveryBlob).err(),
        Some(Error::DecryptionFailed)
    );
    relabelled[2] = 9;
    assert!(Sealed::decode(&relabelled).is_err());
    // A recovery blob is not a keystore.
    let blob = RecoveryBlob::seal(&f.id.root, &recovery_key)
        .unwrap()
        .encode();
    assert!(DeviceKeystore::open(&key, &blob).is_err());
}

#[test]
fn recovery_key_display_and_parsing() {
    let key = RecoveryKey::generate().unwrap();
    let shown = key.display();
    assert_eq!(shown.len(), 56 + 13);
    let groups: Vec<&str> = shown.split('-').collect();
    assert_eq!(groups.len(), 14);
    assert!(groups.iter().all(|g| g.len() == 4));
    let parsed = RecoveryKey::parse(&shown).unwrap();
    assert_eq!(&parsed.display()[..], &shown[..]);
    // Case, separators and Crockford's look-alikes do not matter.
    let relaxed = shown.to_lowercase().replace('-', " ");
    assert_eq!(
        &RecoveryKey::parse(&relaxed).unwrap().display()[..],
        &shown[..]
    );
    let compact: String = shown.chars().filter(|c| *c != '-').collect();
    let lookalike = compact.replace('0', "O").replace('1', "l");
    assert_eq!(
        &RecoveryKey::parse(&lookalike).unwrap().display()[..],
        &shown[..]
    );
    // Typos are caught by the checksum (or the length), never accepted as another key.
    let chars: Vec<char> = compact.chars().collect();
    for i in 0..chars.len() {
        let mut typo = chars.clone();
        typo[i] = if typo[i] == 'A' { 'B' } else { 'A' };
        let typo: String = typo.into_iter().collect();
        assert!(RecoveryKey::parse(&typo).is_err(), "typo at {i} accepted");
    }
    let mut swapped = chars.clone();
    let j = (0..55).find(|&j| swapped[j] != swapped[j + 1]).unwrap();
    swapped.swap(j, j + 1);
    assert!(RecoveryKey::parse(&swapped.into_iter().collect::<String>()).is_err());
    assert!(RecoveryKey::parse(&compact[..55]).is_err());
    assert!(RecoveryKey::parse(&format!("{compact}0")).is_err());
    assert!(RecoveryKey::parse(&compact.replacen(&compact[..1], "U", 1)).is_err());
    assert!(RecoveryKey::parse(&compact.replacen(&compact[..1], "*", 1)).is_err());
    assert_eq!(format!("{key:?}"), "RecoveryKey(redacted)");
}

#[test]
fn recovery_after_losing_every_device() {
    let f = fixture();
    let recovery_key = RecoveryKey::generate().unwrap();
    // At setup: the root is sealed, the blob kept on devices and the server; the root
    // itself is deleted.
    let blob = RecoveryBlob::seal(&f.id.root, &recovery_key)
        .unwrap()
        .encode();
    let old_generation = f.identity.clone();

    // Every device is gone. A new device gets the blob from the server.
    let shown = recovery_key.display();
    let typed = RecoveryKey::parse(&shown).unwrap();
    let root = RecoveryBlob::decode(&blob).unwrap().open(&typed).unwrap();
    assert_eq!(root.fingerprint(), f.identity.fingerprint());
    let authority = root
        .rotate_device_signing_key(old_generation.device_signing_certificate(), NOW)
        .unwrap();
    let identity = old_generation
        .rotate(authority.certificate().clone())
        .unwrap();
    let device = DeviceKeys::generate().unwrap();
    let (cert, cred) = authority
        .certify_device(
            &device.public_keys(),
            &DisplayName::new("new").unwrap(),
            validity(),
        )
        .unwrap();
    let mut roster = DeviceRoster::new(identity.clone());
    roster.add_certificate(cert).unwrap();
    roster
        .check_credential(&cred, device.leaf_key().as_bytes(), NOW)
        .unwrap();
    // The lost devices' credentials are dead, and the identity is unchanged: contacts'
    // verification survives.
    assert!(
        roster
            .check_credential(
                &f.id.device_credential,
                f.id.device.leaf_key().as_bytes(),
                NOW
            )
            .is_err()
    );
    assert_eq!(identity.fingerprint(), f.identity.fingerprint());

    // A wrong recovery key, or a modified blob, fails.
    assert_eq!(
        RecoveryBlob::decode(&blob)
            .unwrap()
            .open(&RecoveryKey::generate().unwrap())
            .err(),
        Some(Error::DecryptionFailed)
    );
    assert_flips_rejected(&blob, &(0..blob.len()).collect::<Vec<_>>(), |b| {
        RecoveryBlob::decode(b)?.open(&typed)
    });
    assert_truncations_rejected(&blob, |b| RecoveryBlob::decode(b)?.open(&typed));
    assert_trailing_rejected(&blob, RecoveryBlob::decode);
}

#[test]
fn identity_export_round_trip() {
    let f = fixture();
    let recovery_key = RecoveryKey::generate().unwrap();
    let sync_key = DeviceSyncKey::generate(4).unwrap();
    let export = IdentityExport::create(
        &f.id.root,
        Some(&f.id.device_signing),
        Some(&sync_key),
        b"pins, verified contacts",
        &recovery_key,
    )
    .unwrap();
    let bytes = export.encode();
    let decoded = IdentityExport::decode(&bytes).unwrap();
    assert_eq!(decoded, export);
    assert_eq!(decoded.encode(), bytes);
    assert_eq!(decoded.identity(), f.identity.fingerprint());

    let imported = decoded.open(&recovery_key).unwrap();
    assert_eq!(imported.root.fingerprint(), f.identity.fingerprint());
    let authority = imported.device_signing.as_ref().unwrap();
    assert_eq!(authority.certificate(), f.id.device_signing.certificate());
    assert_eq!(imported.sync_key.as_ref().unwrap().epoch(), 4);
    assert_eq!(&imported.synced_data[..], b"pins, verified contacts");

    assert_eq!(
        decoded.open(&RecoveryKey::generate().unwrap()).err(),
        Some(Error::DecryptionFailed)
    );
    let positions: Vec<usize> = (0..bytes.len()).filter(|&i| i < 80 || i % 5 == 0).collect();
    assert_flips_rejected(&bytes, &positions, |b| {
        IdentityExport::decode(b)?.open(&recovery_key)
    });
    assert_truncations_rejected(&bytes, IdentityExport::decode);
    assert_trailing_rejected(&bytes, IdentityExport::decode);
    assert_header_checked(&bytes, IdentityExport::decode);

    // Root only.
    let minimal = IdentityExport::create(&f.id.root, None, None, b"", &recovery_key).unwrap();
    let imported = IdentityExport::decode(&minimal.encode())
        .unwrap()
        .open(&recovery_key)
        .unwrap();
    assert!(imported.device_signing.is_none() && imported.sync_key.is_none());
    assert!(imported.synced_data.is_empty());
    // Synced data needs the sync key; a foreign device-signing authority is refused.
    assert!(IdentityExport::create(&f.id.root, None, None, b"x", &recovery_key).is_err());
    let stranger = fixture();
    assert_eq!(
        IdentityExport::create(
            &f.id.root,
            Some(&stranger.id.device_signing),
            None,
            b"",
            &recovery_key
        )
        .err(),
        Some(Error::WrongIdentity)
    );
}

#[test]
fn device_sync_key_blobs() {
    let key = DeviceSyncKey::generate(1).unwrap();
    let sealed = key.seal(b"read markers").unwrap();
    assert_eq!(&key.open(&sealed).unwrap()[..], b"read markers");
    let next = key.rotate().unwrap();
    assert_eq!(next.epoch(), 2);
    assert_eq!(next.open(&sealed).err(), Some(Error::DecryptionFailed));
    assert!(format!("{key:?}").contains("redacted"));
}

#[test]
fn worker_state_is_sealed_under_the_store_key() {
    let key = StoreKey::generate().unwrap();
    let sealed = key.seal_state(b"pins and heads").unwrap();
    assert_eq!(&key.open_state(&sealed).unwrap()[..], b"pins and heads");
    // Another key, a keystore envelope, or a flipped byte fails.
    assert!(StoreKey::generate().unwrap().open_state(&sealed).is_err());
    let mut flipped = sealed.clone();
    *flipped.last_mut().unwrap() ^= 1;
    assert!(key.open_state(&flipped).is_err());
    let f = super::fixture();
    let keystore = f.id.keystore().unwrap().seal(&key).unwrap();
    assert!(key.open_state(&keystore).is_err());
    assert!(DeviceKeystore::open(&key, &sealed).is_err());
}

#[test]
fn a_keystore_renews_its_device_and_can_drop_the_recovery_blob() {
    let f = super::fixture();
    let recovery_key = RecoveryKey::generate().unwrap();
    let mut store = full_keystore(&f, &recovery_key);
    let later = crate::Validity::new(NOW, NOW + 10_000).unwrap();
    store.renew_device(later).unwrap();
    assert_eq!(store.device_certificate().validity(), later);
    assert_eq!(store.device_credential().validity(), later);
    assert_eq!(
        store.device_certificate().public_keys(),
        store.device().public_keys()
    );
    let key = StoreKey::generate().unwrap();
    let reopened = DeviceKeystore::open(&key, &store.seal(&key).unwrap()).unwrap();
    assert_eq!(reopened.device_certificate().validity(), later);
    let dropped = reopened.without_recovery_blob();
    assert!(dropped.recovery_blob().is_none());
    // Without the device-signing key nothing can be renewed.
    let mut plain = DeviceKeystore::open(&key, &dropped.seal(&key).unwrap()).unwrap();
    assert!(plain.device_signing().is_some());
    let f2 = super::fixture();
    let mut without = DeviceKeystore::new(
        f2.identity.clone(),
        DeviceKeys::from_seeds(
            f2.id.device.device_id(),
            f2.id.device.key_epoch(),
            f2.id.device.leaf_seed.duplicate(),
            f2.id.device.kem_seed.duplicate(),
        )
        .unwrap(),
        f2.id.device_certificate.clone(),
        f2.id.device_credential.clone(),
    )
    .unwrap();
    assert!(without.renew_device(later).is_err());
    plain.renew_device(later).unwrap();
}
