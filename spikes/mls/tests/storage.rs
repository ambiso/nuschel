// SPDX-License-Identifier: EUPL-1.2
//! Task 1.3: OpenMLS state in our own encrypted, transactional store.

use nuschel_mls_spike::ds::DeliveryService;
use nuschel_mls_spike::pins::ConversationPins;
use nuschel_mls_spike::sim::Client;
use nuschel_mls_spike::store::{EncryptedStore, StoreError, census};
use nuschel_mls_spike::*;
use openmls::prelude::*;
use openmls_traits::OpenMlsProvider;

const SERVER: [u8; 32] = [7; 32];

fn binding() -> ConversationBinding {
    ConversationBinding {
        server: SERVER,
        conversation: [1; 16],
        generation: 0,
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn print_census(title: &str, client: &Client) {
    println!("{title}:");
    for (label, (count, bytes)) in census(client.device.provider.storage()) {
        println!("  {label:<22} {count:>3} records {bytes:>7} bytes");
    }
}

#[test]
fn encrypted_store_survives_a_restart() {
    let mut ds = DeliveryService::new();
    let mut alice = Client::new(Device::new("alice"));
    let mut bob = Client::new(Device::new("bob"));
    let mut carol = Client::new(Device::new("carol"));
    let mut bob_store = EncryptedStore::new();

    let kp = bob.device.key_package();
    println!(
        "bob after publishing a key package: {:?}",
        bob_store.commit(bob.device.provider.storage())
    );
    print_census("bob, key package only", &bob);

    alice.create(&mut ds, &binding());
    let welcome = alice
        .add(&mut ds, &[kp, carol.device.key_package()])
        .unwrap();
    bob.join_checked(&ds, &welcome, &mut ConversationPins::new(SERVER))
        .unwrap();
    carol
        .join_checked(&ds, &welcome, &mut ConversationPins::new(SERVER))
        .unwrap();
    println!(
        "bob after joining: {:?}",
        bob_store.commit(bob.device.provider.storage())
    );
    print_census("bob in a 3-member group, epoch 1", &bob);

    alice.send(&mut ds, "one").unwrap();
    assert_eq!(Client::texts(bob.sync(&ds)), vec!["one"]);
    println!(
        "bob after one message: {:?}",
        bob_store.commit(bob.device.provider.storage())
    );

    alice.remove(&mut ds, "carol").unwrap();
    bob.sync(&ds);
    println!(
        "bob after carol's removal: {:?}",
        bob_store.commit(bob.device.provider.storage())
    );
    print_census("bob in a 2-member group, epoch 2", &bob);

    // Nothing readable on disk: no labels, no group id.
    let id = bob.group_id();
    for record in bob_store.disk.values().chain(bob_store.disk.keys()) {
        for needle in [
            &b"EpochSecrets"[..],
            b"MessageSecrets",
            b"Tree",
            id.as_slice(),
        ] {
            assert!(!contains(record, needle));
        }
    }

    // Restart: the crypto worker rebuilds OpenMLS state from the encrypted store.
    let storage = bob_store.load().unwrap();
    bob.device.provider = NuschelProvider::with_storage(storage);
    bob.group = Some(
        MlsGroup::load(bob.device.provider.storage(), &id)
            .unwrap()
            .unwrap(),
    );
    alice.send(&mut ds, "after restart").unwrap();
    assert_eq!(Client::texts(bob.sync(&ds)), vec!["after restart"]);
    bob_store.commit(bob.device.provider.storage());

    // Re-keying keeps the data readable and replaces every ciphertext.
    let before: Vec<_> = bob_store.disk.values().cloned().collect();
    bob_store.rekey();
    assert!(bob_store.disk.values().all(|r| !before.contains(r)));
    bob_store.load().unwrap();

    // Tampering is detected.
    let first = bob_store.disk.keys().next().unwrap().clone();
    bob_store.disk.get_mut(&first).unwrap()[20] ^= 1;
    assert_eq!(bob_store.load().err(), Some(StoreError::Corrupt));
}

/// The leaf signature key stays with the crypto worker, outside OpenMLS's storage;
/// the HPKE private keys (leaf, init, epoch path keys) and epoch secrets are inside.
#[test]
fn what_openmls_persists() {
    let mut ds = DeliveryService::new();
    let mut alice = Client::new(Device::new("alice"));
    alice.create(&mut ds, &binding());
    let labels = census(alice.device.provider.storage());
    assert!(!labels.contains_key("SignatureKeyPair"));
    for expected in [
        "Tree",
        "GroupContext",
        "EpochSecrets",
        "MessageSecrets",
        "GroupState",
    ] {
        assert!(labels.contains_key(expected), "{expected}");
    }
    assert!(!labels.contains_key("?"), "unknown label");
}
