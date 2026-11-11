// SPDX-License-Identifier: EUPL-1.2
//! Device linking end to end in memory, with a mailbox the attacker controls.

use super::*;
use crate::keystore::DeviceSyncKey;
use crate::{
    DeviceKeys, DeviceKeystore, DeviceRoster, Error, ExistingDeviceLink, LinkBundle, LinkCode,
    LinkDeviceKeys, LinkPake, NewAwaitingBundle, NewDeviceKeys, NewDeviceLink, ProvisioningBundle,
    RecoveryBlob, RecoveryKey, StoreKey,
};

fn name(s: &str) -> DisplayName {
    DisplayName::new(s).unwrap()
}

/// Runs both sides up to the existing device's check of the new device's keys. The
/// mailbox passes every message through `wire`, which may replace it.
fn handshake(
    existing_code: &LinkCode,
    new_code: &LinkCode,
    new_device: &DeviceKeys,
) -> (Result<NewDeviceKeys, Error>, NewAwaitingBundle) {
    let (existing, pake_a) = ExistingDeviceLink::start(existing_code);
    let (new, pake_b) = NewDeviceLink::start(new_code, new_device, name("phone"));
    // Through the mailbox, as bytes.
    let pake_a = LinkPake::decode(&pake_a.encode()).unwrap();
    let pake_b = LinkPake::decode(&pake_b.encode()).unwrap();
    let existing = existing.receive_pake(&pake_b).unwrap();
    let (awaiting, keys) = new.receive_pake(&pake_a).unwrap();
    let keys = LinkDeviceKeys::decode(&keys.encode()).unwrap();
    (existing.receive_device_keys(&keys), awaiting)
}

fn bundle_for(f: &Fixture, new: &NewDeviceKeys) -> ProvisioningBundle {
    let (cert, cred) =
        f.id.device_signing
            .certify_device(new.device_keys(), new.display_name(), validity())
            .unwrap();
    ProvisioningBundle::new(&f.identity, cert, cred)
}

#[test]
fn linking_end_to_end() {
    let f = fixture();
    let new_device = DeviceKeys::generate().unwrap();
    let code = LinkCode::generate(42).unwrap();
    assert_eq!(code.nameplate(), 42);
    // The user reads the code off one screen and types it on the other.
    let typed = LinkCode::parse(&format!("  {} ", code.as_str())).unwrap();

    let (authenticated, awaiting) = handshake(&code, &typed, &new_device);
    let authenticated = authenticated.unwrap();
    assert_eq!(authenticated.device_keys(), &new_device.public_keys());
    assert_eq!(authenticated.display_name().as_str(), "phone");

    let recovery_key = RecoveryKey::generate().unwrap();
    let recovery_blob = RecoveryBlob::seal(&f.id.root, &recovery_key).unwrap();
    let sync_key = DeviceSyncKey::generate(3).unwrap();
    let bundle = bundle_for(&f, &authenticated)
        .with_device_signing(&f.id.device_signing)
        .unwrap()
        .with_recovery_blob(recovery_blob.clone())
        .with_sync_key(&sync_key)
        .with_application_data(b"pins and contacts")
        .unwrap();
    let sealed = authenticated.seal(&bundle).unwrap();
    let sealed = LinkBundle::decode(&sealed.encode()).unwrap();

    let provisioned = awaiting.receive_bundle(&new_device, &sealed, NOW).unwrap();
    assert_eq!(provisioned.identity.fingerprint(), f.identity.fingerprint());
    assert_eq!(
        provisioned.device_certificate.public_keys(),
        new_device.public_keys()
    );
    assert_eq!(&provisioned.application_data[..], b"pins and contacts");
    assert_eq!(provisioned.sync_key.as_ref().unwrap().epoch(), 3);
    assert_eq!(provisioned.recovery_blob.as_ref(), Some(&recovery_blob));
    // The root comes back with the recovery key, on the new device too.
    let root = provisioned
        .recovery_blob
        .as_ref()
        .unwrap()
        .open(&recovery_key)
        .unwrap();
    assert_eq!(root.fingerprint(), f.identity.fingerprint());

    // The new device can link a third one: its device-signing authority works.
    let authority = provisioned.device_signing.as_ref().unwrap();
    let third = DeviceKeys::generate().unwrap();
    let (third_cert, third_cred) = authority
        .certify_device(&third.public_keys(), &name("tablet"), validity())
        .unwrap();
    let mut roster = DeviceRoster::new(f.identity.clone());
    roster
        .add_certificate(provisioned.device_certificate.clone())
        .unwrap();
    roster.add_certificate(third_cert).unwrap();
    roster
        .check_credential(&third_cred, third.leaf_key().as_bytes(), NOW)
        .unwrap();
    roster
        .check_credential(
            &provisioned.device_credential,
            new_device.leaf_key().as_bytes(),
            NOW,
        )
        .unwrap();

    // And keeps all of it in its keystore.
    let store_key = StoreKey::generate().unwrap();
    let store = DeviceKeystore::from_provisioned(new_device, provisioned).unwrap();
    let reopened = DeviceKeystore::open(&store_key, &store.seal(&store_key).unwrap()).unwrap();
    assert!(reopened.device_signing().is_some());
    assert_eq!(reopened.sync_key().unwrap().epoch(), 3);
    assert_eq!(reopened.recovery_blob(), Some(&recovery_blob));
}

#[test]
fn a_bundle_without_the_device_signing_key() {
    let f = fixture();
    let new_device = DeviceKeys::generate().unwrap();
    let code = LinkCode::generate(7).unwrap();
    let (authenticated, awaiting) = handshake(&code, &code, &new_device);
    let authenticated = authenticated.unwrap();
    let bundle = bundle_for(&f, &authenticated);
    let sealed = authenticated.seal(&bundle).unwrap();
    let provisioned = awaiting.receive_bundle(&new_device, &sealed, NOW).unwrap();
    assert!(provisioned.device_signing.is_none());
    assert!(provisioned.sync_key.is_none());
    assert!(provisioned.application_data.is_empty());
}

#[test]
fn a_wrong_code_fails_at_the_existing_device() {
    let new_device = DeviceKeys::generate().unwrap();
    let code = LinkCode::parse("42-1234-5678").unwrap();
    for typo in ["42-1234-5679", "43-1234-5678", "42-5678-1234"] {
        let typo = LinkCode::parse(typo).unwrap();
        let (authenticated, _) = handshake(&code, &typo, &new_device);
        assert_eq!(authenticated.err(), Some(Error::LinkAuthentication));
    }
}

#[test]
fn a_server_swapping_the_new_devices_keys_is_caught() {
    let new_device = DeviceKeys::generate().unwrap();
    let attacker = DeviceKeys::generate().unwrap();
    let code = LinkCode::generate(1).unwrap();
    let (existing, pake_a) = ExistingDeviceLink::start(&code);
    let (new, pake_b) = NewDeviceLink::start(&code, &new_device, name("phone"));
    let existing = existing.receive_pake(&pake_b).unwrap();
    let (_, keys) = new.receive_pake(&pake_a).unwrap();
    let bytes = keys.encode();
    // Offsets: header 2, device id 16, epoch 4, leaf alg 2, leaf key 32, kem alg 2.
    let mut swapped_kem = bytes.clone();
    swapped_kem[58..58 + 1216].copy_from_slice(attacker.kem_key().as_bytes());
    let mut swapped_leaf = bytes.clone();
    swapped_leaf[24..56].copy_from_slice(attacker.leaf_key().as_bytes());
    let mut renamed = bytes.clone();
    let name_at = bytes.len() - 32 - 5;
    renamed[name_at] = b'P';
    let mut epoch = bytes.clone();
    epoch[21] = 2;
    for forged in [swapped_kem, swapped_leaf, renamed, epoch] {
        let forged = LinkDeviceKeys::decode(&forged).unwrap();
        assert_eq!(
            existing
                .duplicate_for_tests()
                .receive_device_keys(&forged)
                .err(),
            Some(Error::LinkAuthentication)
        );
    }
    existing
        .receive_device_keys(&LinkDeviceKeys::decode(&bytes).unwrap())
        .unwrap();
}

#[test]
fn a_man_in_the_middle_without_the_code_fails() {
    let new_device = DeviceKeys::generate().unwrap();
    let code = LinkCode::generate(5).unwrap();
    let guess = LinkCode::generate(5).unwrap();
    // The server runs SPAKE2 with each side under a guessed code.
    let (existing, _pake_a) = ExistingDeviceLink::start(&code);
    let (new, _pake_b) = NewDeviceLink::start(&code, &new_device, name("phone"));
    let (mitm_as_existing, mitm_a) = ExistingDeviceLink::start(&guess);
    let (_mitm_as_new, mitm_b) = NewDeviceLink::start(&guess, &new_device, name("phone"));
    let existing = existing.receive_pake(&mitm_b).unwrap();
    let (_, keys) = new.receive_pake(&mitm_a).unwrap();
    // The new device's confirmation is under its key with the attacker, not the existing
    // device's.
    assert_eq!(
        existing.receive_device_keys(&keys).err(),
        Some(Error::LinkAuthentication)
    );
    let _ = mitm_as_existing;
}

#[test]
fn tampered_or_replayed_bundles_fail_at_the_new_device() {
    let f = fixture();
    let new_device = DeviceKeys::generate().unwrap();

    let code1 = LinkCode::generate(1).unwrap();
    let (authenticated, awaiting1) = handshake(&code1, &code1, &new_device);
    let authenticated = authenticated.unwrap();
    let bundle = bundle_for(&f, &authenticated);
    let sealed = authenticated.seal(&bundle).unwrap();
    let bytes = sealed.encode();

    let code2 = LinkCode::generate(2).unwrap();
    let (_, awaiting2) = handshake(&code2, &code2, &new_device);
    // A bundle from another session (same device keys) is refused.
    assert_eq!(
        awaiting2.receive_bundle(&new_device, &sealed, NOW).err(),
        Some(Error::LinkAuthentication)
    );

    // Any flipped bit in the X-Wing ciphertext or the AEAD output.
    for index in [
        2,
        2 + 600,
        2 + 1119,
        2 + 1120 + 3,
        2 + 1120 + 500,
        bytes.len() - 1,
    ] {
        let mut broken = bytes.clone();
        broken[index] ^= 0x04;
        let broken = LinkBundle::decode(&broken).unwrap();
        let code = LinkCode::generate(1).unwrap();
        let (authenticated, awaiting) = handshake(&code, &code, &new_device);
        let _ = authenticated.unwrap();
        assert_eq!(
            awaiting.receive_bundle(&new_device, &broken, NOW).err(),
            Some(Error::LinkAuthentication)
        );
    }
    // The genuine bundle still opens in its own session, but not for other keys.
    let other = DeviceKeys::generate().unwrap();
    assert_eq!(
        awaiting1.receive_bundle(&other, &sealed, NOW).err(),
        Some(Error::KeyMismatch)
    );
}

#[test]
fn the_existing_device_refuses_to_seal_for_other_keys() {
    let f = fixture();
    let new_device = DeviceKeys::generate().unwrap();
    let code = LinkCode::generate(1).unwrap();
    let (authenticated, _) = handshake(&code, &code, &new_device);
    let authenticated = authenticated.unwrap();
    let elsewhere = DeviceKeys::generate().unwrap();
    let (cert, cred) =
        f.id.device_signing
            .certify_device(&elsewhere.public_keys(), &name("x"), validity())
            .unwrap();
    let bundle = ProvisioningBundle::new(&f.identity, cert, cred);
    assert_eq!(authenticated.seal(&bundle).err(), Some(Error::KeyMismatch));
}

#[test]
fn a_bundle_must_be_valid_now_and_from_the_right_identity() {
    let f = fixture();
    let new_device = DeviceKeys::generate().unwrap();
    let code = LinkCode::generate(1).unwrap();
    let (authenticated, awaiting) = handshake(&code, &code, &new_device);
    let authenticated = authenticated.unwrap();
    let bundle = bundle_for(&f, &authenticated);
    let sealed = authenticated.seal(&bundle).unwrap();
    assert_eq!(
        awaiting
            .receive_bundle(&new_device, &sealed, NOW + 5000)
            .err(),
        Some(Error::Expired)
    );
    // With a device-signing key that does not belong to the bundle's identity.
    let stranger = fixture();
    let (authenticated, _) = handshake(&code, &code, &new_device);
    let bundle = bundle_for(&f, &authenticated.unwrap());
    assert_eq!(
        bundle
            .with_device_signing(&stranger.id.device_signing)
            .err(),
        Some(Error::KeyMismatch)
    );
}

#[test]
fn pake_messages_are_checked() {
    let code = LinkCode::generate(1).unwrap();
    let device = DeviceKeys::generate().unwrap();
    // Reflection: the existing device gets its own message back.
    let (existing, pake_a) = ExistingDeviceLink::start(&code);
    assert_eq!(
        existing.receive_pake(&pake_a).err(),
        Some(Error::LinkAuthentication)
    );
    let (new, pake_b) = NewDeviceLink::start(&code, &device, name("p"));
    assert_eq!(
        new.receive_pake(&pake_b).err(),
        Some(Error::LinkAuthentication)
    );
    // A small-order element with the right side byte.
    let mut bytes = pake_a.encode();
    bytes[3..].copy_from_slice(&[0u8; 32]);
    bytes[3] = 1; // the identity point
    let identity_point = LinkPake::decode(&bytes).unwrap();
    let (new, _) = NewDeviceLink::start(&code, &device, name("p"));
    assert_eq!(
        new.receive_pake(&identity_point).err(),
        Some(Error::LinkAuthentication)
    );
    // Encoding.
    let encoded = pake_a.encode();
    assert_eq!(encoded.len(), LinkPake::LEN);
    assert_truncations_rejected(&encoded, LinkPake::decode);
    assert_trailing_rejected(&encoded, LinkPake::decode);
    assert_header_checked(&encoded, LinkPake::decode);
}

#[test]
fn link_message_encodings_are_strict() {
    let f = fixture();
    let new_device = DeviceKeys::generate().unwrap();
    let code = LinkCode::generate(1).unwrap();
    let (existing, pake_a) = ExistingDeviceLink::start(&code);
    let (new, pake_b) = NewDeviceLink::start(&code, &new_device, name("phone"));
    let existing = existing.receive_pake(&pake_b).unwrap();
    let (_, keys) = new.receive_pake(&pake_a).unwrap();
    let keys_bytes = keys.encode();
    assert_eq!(
        LinkDeviceKeys::decode(&keys_bytes).unwrap().encode(),
        keys_bytes
    );
    assert_truncations_rejected(&keys_bytes, LinkDeviceKeys::decode);
    assert_trailing_rejected(&keys_bytes, LinkDeviceKeys::decode);
    assert_header_checked(&keys_bytes, LinkDeviceKeys::decode);

    let authenticated = existing.receive_device_keys(&keys).unwrap();
    let bundle = bundle_for(&f, &authenticated);
    let sealed = authenticated.seal(&bundle).unwrap();
    let bundle_bytes = sealed.encode();
    assert_eq!(
        LinkBundle::decode(&bundle_bytes).unwrap().encode(),
        bundle_bytes
    );
    assert_trailing_rejected(&bundle_bytes, LinkBundle::decode);
    assert_header_checked(&bundle_bytes, LinkBundle::decode);
    for len in (0..bundle_bytes.len()).step_by(97) {
        assert!(LinkBundle::decode(&bundle_bytes[..len]).is_err());
    }
}

#[test]
fn link_codes_are_parsed_strictly() {
    for good in ["0-0000-0000", "42-1234-5678", "4294967295-9999-0000"] {
        let code = LinkCode::parse(good).unwrap();
        assert_eq!(code.as_str(), good);
    }
    for bad in [
        "",
        "42",
        "42-1234",
        "42-1234-567",
        "42-1234-56789",
        "42-1234-5678-1",
        "042-1234-5678",
        "4294967296-1234-5678",
        "42-12a4-5678",
        "42 1234 5678",
        "-1234-5678",
        "42-١٢٣٤-5678",
    ] {
        assert!(LinkCode::parse(bad).is_err(), "accepted {bad:?}");
    }
    let code = LinkCode::generate(9).unwrap();
    assert!(!format!("{code:?}").contains(&code.as_str()[2..]));
    // Codes are random.
    assert_ne!(
        LinkCode::generate(9).unwrap().as_str(),
        LinkCode::generate(9).unwrap().as_str()
    );
}
