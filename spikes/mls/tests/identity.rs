// SPDX-License-Identifier: EUPL-1.2
//! Cross-server, multi-device identity: certificate chains, hybrid signatures,
//! device linking over X-Wing, safety numbers, and the Mumble certificate binding.

use std::rc::Rc;

use nuschl_mls_spike::ds::DeliveryService;
use nuschl_mls_spike::identity::*;
use nuschl_mls_spike::pins::ConversationPins;
use nuschl_mls_spike::sim::Client;
use nuschl_mls_spike::*;
use openmls_traits::types::SignatureScheme;

const SERVER: [u8; 32] = [7; 32];

#[test]
fn credential_chain_verifies_and_binds_the_leaf_key() {
    let identity = UserIdentity::generate();
    let keys = DeviceKeys::generate(SignatureScheme::ED25519);
    let cert = identity.certify_device(&keys, "laptop");
    let chain = identity.credential_chain(&cert);
    let device = verify_chain(&chain, keys.signer.public()).unwrap();
    assert_eq!(device.identity, identity.root_public().fingerprint());
    assert_eq!(device.kem_key.len(), 1216, "the device's X-Wing key");

    let other = DeviceKeys::generate(SignatureScheme::ED25519);
    assert!(verify_chain(&chain, other.signer.public()).is_err());

    // A device certified by another identity's device-signing key does not chain.
    let stranger = UserIdentity::generate();
    let forged = stranger.certify_device(&keys, "laptop");
    assert!(verify_chain(&identity.credential_chain(&forged), keys.signer.public()).is_err());

    println!(
        "sizes: root key {} B, device-signing cert {} B, device cert {} B, MLS credential {} B",
        identity.root_public().encode().len(),
        identity.device_signing_cert.encode().len(),
        cert.encode().len(),
        chain.len()
    );
}

#[cfg(feature = "mldsa")]
#[test]
fn hybrid_signatures_need_both_halves() {
    let identity = UserIdentity::generate();
    assert_eq!(identity.root_public().mldsa65.len(), 1952);
    let cert = identity.device_signing_cert.clone();
    assert_eq!(cert.signature.mldsa65.len(), 3309);
    cert.verify().unwrap();

    let mut stripped = cert.clone();
    stripped.signature.mldsa65.clear();
    assert!(
        stripped.verify().is_err(),
        "dropping the ML-DSA half is a downgrade"
    );

    let mut broken = cert.clone();
    broken.signature.ed25519[0] ^= 1;
    assert!(broken.verify().is_err());

    let mut broken = cert;
    broken.signature.mldsa65[0] ^= 1;
    assert!(broken.verify().is_err());
}

/// Two devices of one identity in a group: the other member sees one person with
/// two devices, and one safety number for that person.
#[test]
fn one_identity_two_devices() {
    let alice = Rc::new(UserIdentity::generate());
    let laptop = Device::linked("alice-laptop", alice.clone(), SUITE);
    let desktop = Device::linked("alice-desktop", alice.clone(), SUITE);
    let bob = Device::new("bob");
    let bob_identity = bob.identity.root_public().clone();

    let mut ds = DeliveryService::new();
    let mut a = Client::new(laptop);
    a.create(
        &mut ds,
        &ConversationBinding {
            server: SERVER,
            conversation: [3; 16],
            generation: 0,
        },
    );
    let welcome = a
        .add(&mut ds, &[desktop.key_package(), bob.key_package()])
        .unwrap();
    let mut b = Client::new(bob);
    b.join_checked(&ds, &welcome, &mut ConversationPins::new(SERVER))
        .unwrap();
    let roster = Device::verify_roster(b.group()).unwrap();
    let alice_devices: Vec<_> = roster
        .iter()
        .filter(|d| d.identity == alice.root_public().fingerprint())
        .collect();
    assert_eq!(alice_devices.len(), 2);

    let number = safety_number(alice.root_public(), &bob_identity);
    assert_eq!(number, safety_number(&bob_identity, alice.root_public()));
    assert_eq!(number.len(), 60);
    assert!(number.bytes().all(|c| c.is_ascii_digit()));
    assert_eq!(
        safety_qr(alice.root_public(), &bob_identity),
        safety_qr(&bob_identity, alice.root_public())
    );
    let carol = UserIdentity::generate();
    assert_ne!(number, safety_number(carol.root_public(), &bob_identity));
    println!(
        "safety number: {}",
        number
            .as_bytes()
            .chunks(5)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join(" ")
    );
}

/// Linking a new device: QR code with a rendezvous id and key hash; keys and the
/// sealed provisioning bundle travel through the server's mailbox, sealed with
/// HPKE over X-Wing to the new device.
#[test]
fn device_linking_over_xwing() {
    let identity = UserIdentity::generate();
    let new_device = DeviceKeys::generate(SignatureScheme::ED25519);

    // New device: shows the QR code, uploads its keys to the mailbox.
    let offer = LinkOffer::new(&new_device);
    let qr = offer.qr();
    assert_eq!(qr.len(), 52);

    // Existing device: scans the QR, fetches the keys, checks them against it.
    let fetched = LinkOffer {
        rendezvous: offer.rendezvous,
        kem_key: offer.kem_key.clone(),
        leaf_key: offer.leaf_key.clone(),
    };
    assert!(fetched.matches_qr(&qr));
    let swapped = LinkOffer {
        kem_key: DeviceKeys::generate(SignatureScheme::ED25519).kem.public,
        ..fetched
    };
    assert!(
        !swapped.matches_qr(&qr),
        "a server swapping the key is caught by the QR"
    );

    let fetched = LinkOffer {
        rendezvous: offer.rendezvous,
        kem_key: offer.kem_key.clone(),
        leaf_key: offer.leaf_key.clone(),
    };
    let cert = identity.certify_device(&new_device, "phone");
    let payload = identity.credential_chain(&cert);
    let sealed = seal_provisioning(&fetched, &payload);
    println!(
        "provisioning: payload {} B, X-Wing encapsulation {} B, ciphertext {} B",
        payload.len(),
        sealed.kem_output.as_slice().len(),
        sealed.ciphertext.as_slice().len()
    );
    assert_eq!(
        sealed.kem_output.as_slice().len(),
        1120,
        "X-Wing ciphertext"
    );

    // New device: opens the bundle and checks that the chain names its own key.
    let opened = open_provisioning(&new_device, &offer.rendezvous, &sealed).unwrap();
    verify_chain(&opened, new_device.signer.public()).unwrap();
    assert!(open_provisioning(&new_device, &[0; 16], &sealed).is_err());
    let other = DeviceKeys::generate(SignatureScheme::ED25519);
    assert!(open_provisioning(&other, &offer.rendezvous, &sealed).is_err());
}

/// Losing a device that held the device-signing key: rotate it with the root key
/// (recovery). The identity, and every contact's verification of it, is unchanged.
#[test]
fn rotating_the_device_signing_key_keeps_the_identity() {
    let mut identity = UserIdentity::generate();
    let before = identity.root_public().fingerprint();
    let keys = DeviceKeys::generate(SignatureScheme::ED25519);
    let old_chain = identity.credential_chain(&identity.certify_device(&keys, "desktop"));
    identity.rotate_device_signing_key();
    assert_eq!(identity.root_public().fingerprint(), before);
    let new_chain = identity.credential_chain(&identity.certify_device(&keys, "desktop"));
    verify_chain(&new_chain, keys.signer.public()).unwrap();
    // The old chain is still well formed: freshness comes from the KT log, which
    // names the current device-signing key generation and revoked devices.
    verify_chain(&old_chain, keys.signer.public()).unwrap();
    let revocation = identity.sign_statement(Statement::RevokeDevice {
        device_id: keys.device_id,
    });
    revocation.verify().unwrap();
    // The account binding published in a server's KT log survives the rotation of
    // the device-signing key only if re-signed; the root still names the identity.
    let binding = identity.sign_statement(Statement::Account {
        server: [7; 32],
        account: [0x42; 16],
    });
    assert_eq!(Cert::decode(&binding.encode()), Some(binding));
}

/// What the Mumble certificate binding proves, and what it does not. A stock
/// server only *claims* each user's certificate hash, so it can attach Alice's
/// binding (a public blob) to anyone it likes. The binding proves "this identity
/// uses certificate H", never "the person talking is H".
#[test]
fn mumble_binding_is_a_claim_not_authentication() {
    let alice = UserIdentity::generate();
    let alice_hash = [0xAA; 20];
    let binding = alice.sign_statement(Statement::MumbleCertificate { sha1: alice_hash });
    let blob = binding.encode();

    // The stock server presents "mallory" with Alice's hash and Alice's blob.
    let replayed = Cert::decode(&blob).unwrap();
    replayed.verify().unwrap();
    assert_eq!(
        replayed.statement,
        Statement::MumbleCertificate { sha1: alice_hash }
    );
    // The client can only say: the server claims this user has H, and H belongs to
    // Alice's identity. On a stock server, TLS between that user and the server is
    // invisible to us, so this is never shown as verified.
}
