// SPDX-License-Identifier: EUPL-1.2
//! The client and delivery-service halves together, in process: a direct
//! conversation between two identities through the `DeliveryGroup`, epoch fencing,
//! hostile key packages, and a channel with external joins and the server's
//! removals (docs/e2ee.md §6.2).
use nuschel_e2ee::client::{Client, Incoming, ValidKeyPackage};
use nuschel_e2ee::ds::{DeliveryGroup, DsError, ServerSigner, check_key_package};
use nuschel_e2ee::prevalidate::{self, Malformed};
use nuschel_e2ee::{ConversationBinding, Error, SUITE_KEM_PUBLIC_KEY_LEN};
use nuschel_identity::{DisplayName, NewIdentity, Validity};
use std::sync::LazyLock;

const SERVER: [u8; 32] = [7; 32];
const CONVERSATION: [u8; 16] = [1; 16];

/// The server's external-sender key (its head-signing key in nuscheld).
static SIGNER: LazyLock<ServerSigner> = LazyLock::new(|| ServerSigner::generate().unwrap());

fn server_key() -> [u8; 32] {
    *SIGNER.public_key()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

struct Device {
    identity: NewIdentity,
    client: Client,
}

impl Device {
    fn new(name: &str) -> Self {
        let validity = Validity::starting_at(now() - 3600, 365 * 86_400).unwrap();
        Self {
            identity: NewIdentity::create(&DisplayName::new(name).unwrap(), validity, now())
                .unwrap(),
            client: Client::new().unwrap(),
        }
    }

    fn key_package(&self) -> Vec<u8> {
        self.client
            .key_package(
                &self.identity.device,
                &self.identity.device_credential,
                false,
            )
            .unwrap()
    }
}

fn binding() -> ConversationBinding {
    ConversationBinding::direct(SERVER, CONVERSATION)
}

/// Alice creates the group with Bob; the DS registers and accepts it; Bob joins
/// with the server's tree.
fn pair() -> (Device, Device, DeliveryGroup, Vec<u8>) {
    let alice = Device::new("alice");
    let bob = Device::new("bob");
    let package = bob.key_package();
    let info = check_key_package(&package).unwrap();
    assert_eq!(info.credential, bob.identity.device_credential.encode());
    let valid = ValidKeyPackage::check(&package).unwrap();
    let created = alice
        .client
        .create(
            &alice.identity.device,
            &alice.identity.device_credential,
            &binding(),
            &server_key(),
            &[valid],
        )
        .unwrap();
    let (mut ds, members) = DeliveryGroup::create(
        &created.init_group_info,
        &created.init_tree,
        &SERVER,
        &CONVERSATION,
        &server_key(),
    )
    .unwrap();
    assert_eq!(members.len(), 1);
    let outcome = ds
        .commit(
            &created.first.commit,
            &created.first.group_info,
            created.first.welcome.as_deref(),
        )
        .unwrap();
    assert_eq!(outcome.epoch, 1);
    assert_eq!(outcome.welcome_for, vec![info.reference.clone()]);
    assert_eq!(outcome.added.len(), 1);
    assert_eq!(alice.client.merge_pending(&created.group_id).unwrap(), 1);
    let (_, tree) = ds.join_info().unwrap();
    let pending = bob
        .client
        .stage_welcome(created.first.welcome.as_deref().unwrap(), &tree)
        .unwrap();
    assert_eq!(pending.binding, binding());
    assert_eq!(pending.members.len(), 2);
    let group = bob.client.join(pending).unwrap();
    assert_eq!(group, created.group_id);
    (alice, bob, ds, group)
}

#[test]
fn a_direct_conversation_through_the_delivery_service() {
    let (alice, bob, mut ds, group) = pair();
    let message = alice
        .client
        .encrypt(&alice.identity.device, &group, b"hello bob")
        .unwrap();
    ds.fence(&message).unwrap();
    match bob.client.process(&group, &message).unwrap() {
        Incoming::Application { sender, data } => {
            assert_eq!(data, b"hello bob");
            assert_eq!(sender.credential, alice.identity.device_credential.encode());
        }
        _ => panic!("expected application data"),
    }
    // Bob updates; Alice's message of the old epoch is then stale at the server.
    let stale = alice
        .client
        .encrypt(&alice.identity.device, &group, b"late")
        .unwrap();
    let update = bob
        .client
        .self_update(&bob.identity.device, &group)
        .unwrap();
    ds.commit(&update.commit, &update.group_info, None).unwrap();
    bob.client.merge_pending(&group).unwrap();
    assert_eq!(ds.fence(&stale), Err(DsError::StaleEpoch { current: 2 }));
    // A concurrent commit for the old epoch loses.
    let racing = alice
        .client
        .self_update(&alice.identity.device, &group)
        .unwrap();
    assert!(matches!(
        ds.commit(&racing.commit, &racing.group_info, None),
        Err(DsError::StaleEpoch { current: 2 })
    ));
    alice.client.clear_pending(&group).unwrap();
    match alice.client.process(&group, &update.commit).unwrap() {
        Incoming::Commit(commit) => {
            assert_eq!(commit.members.len(), 2);
            assert_eq!(alice.client.merge(&group, commit).unwrap(), 2);
        }
        _ => panic!("expected a commit"),
    }
    assert_eq!(
        alice.client.epoch_authenticator(&group).unwrap(),
        bob.client.epoch_authenticator(&group).unwrap()
    );
    // State survives a snapshot (the worker's restart) and the DS's restore.
    let bob_again = Client::restore(&bob.client.snapshot()).unwrap();
    let ds_again = DeliveryGroup::restore(&group, &ds.state()).unwrap();
    assert_eq!(ds_again.epoch(), 2);
    let message = alice
        .client
        .encrypt(&alice.identity.device, &group, b"after restart")
        .unwrap();
    ds_again.fence(&message).unwrap();
    assert!(matches!(
        bob_again.process(&group, &message).unwrap(),
        Incoming::Application { data, .. } if data == b"after restart"
    ));
    // A replay fails: message keys are gone.
    assert!(bob_again.process(&group, &message).is_err());
}

#[test]
fn the_delivery_service_refuses_what_does_not_fit() {
    let (alice, _bob, mut ds, group) = pair();
    // Application data as a "handshake", garbage, and another group's binding.
    let message = alice
        .client
        .encrypt(&alice.identity.device, &group, b"x")
        .unwrap();
    assert!(ds.commit(&message, &message, None).is_err());
    assert!(ds.fence(&[0, 1, 0, 2, 0xff]).is_err());
    let update = alice
        .client
        .self_update(&alice.identity.device, &group)
        .unwrap();
    // A GroupInfo for the wrong epoch (the epoch-1 one) rolls the commit back.
    let (old_info, _) = ds.join_info().unwrap();
    assert!(ds.commit(&update.commit, &old_info, None).is_err());
    assert_eq!(ds.epoch(), 1);
    ds.commit(&update.commit, &update.group_info, None).unwrap();
    assert_eq!(ds.epoch(), 2);
    let other = Device::new("mallory");
    let created = other
        .client
        .create(
            &other.identity.device,
            &other.identity.device_credential,
            &ConversationBinding::direct([8; 32], CONVERSATION),
            &server_key(),
            &[],
        )
        .unwrap();
    assert!(
        DeliveryGroup::create(
            &created.init_group_info,
            &created.init_tree,
            &SERVER,
            &CONVERSATION,
            &server_key(),
        )
        .is_err()
    );
    // A group naming another external sender (someone who could forge the
    // server's removals) is refused too.
    let created = other
        .client
        .create(
            &other.identity.device,
            &other.identity.device_credential,
            &ConversationBinding::direct(SERVER, [2; 16]),
            &[9; 32],
            &[],
        )
        .unwrap();
    assert!(
        DeliveryGroup::create(
            &created.init_group_info,
            &created.init_tree,
            &SERVER,
            &[2; 16],
            &server_key(),
        )
        .is_err()
    );
}

/// Re-signs a key package with a 32-byte init key: everything OpenMLS checks is
/// valid, but encapsulating to it fails (libcrux-kem 0.0.9 panicked; 0.0.10 errs).
fn short_init_key_package(device: &Device) -> Vec<u8> {
    let valid = device.key_package();
    // version (2) || suite (2) || init_key<V> (2-byte length 1216) || rest
    assert_eq!(&valid[4..6], &[0x44, 0xc0]);
    let rest = &valid[6 + SUITE_KEM_PUBLIC_KEY_LEN..];
    // The leaf node, extensions and signature: drop the old signature (a 2-byte
    // length, 64 being too large for the 1-byte form, and 64 bytes).
    assert_eq!(&rest[rest.len() - 66..rest.len() - 64], &[0x40, 0x40]);
    let unsigned_rest = &rest[..rest.len() - 66];
    let mut tbs = valid[..4].to_vec();
    tbs.push(32);
    tbs.extend_from_slice(&[0x55; 32]);
    tbs.extend_from_slice(unsigned_rest);
    // SignWithLabel: opaque label<V> || opaque content<V>.
    let label = b"MLS 1.0 KeyPackageTBS";
    let mut content = vec![label.len() as u8];
    content.extend_from_slice(label);
    let length = tbs.len();
    assert!(length < 16384);
    content.extend_from_slice(&[0x40 | (length >> 8) as u8, length as u8]);
    content.extend_from_slice(&tbs);
    let signature = device.identity.device.sign_leaf(&content).unwrap();
    let mut package = tbs;
    package.extend_from_slice(&[0x40, 0x40]);
    package.extend_from_slice(&signature);
    package
}

#[test]
fn a_short_xwing_key_in_a_key_package_is_refused_before_libcrux() {
    let alice = Device::new("alice");
    let mallory = Device::new("mallory");
    let poisoned = short_init_key_package(&mallory);
    assert_eq!(
        prevalidate::key_package(&poisoned),
        Err(Malformed::Length {
            what: "init key",
            got: 32,
            expected: SUITE_KEM_PUBLIC_KEY_LEN
        })
    );
    assert!(matches!(
        check_key_package(&poisoned),
        Err(Error::Malformed(Malformed::Length { .. }))
    ));
    assert!(ValidKeyPackage::check(&poisoned).is_err());
    // Without the check, OpenMLS accepts it; encapsulating to it fails (with the
    // patched libcrux-kem; 0.0.9 panicked).
    {
        use openmls::prelude::tls_codec::Deserialize as _;
        use openmls::prelude::*;
        let provider = openmls_libcrux_crypto::Provider::new().unwrap();
        let package = KeyPackageIn::tls_deserialize(&mut poisoned.as_slice())
            .unwrap()
            .validate(
                openmls_traits::OpenMlsProvider::crypto(&provider),
                ProtocolVersion::Mls10,
            )
            .expect("OpenMLS 0.9.2 accepts a short X-Wing init key");
        let init = package.hpke_init_key().as_slice().len();
        assert_eq!(init, 32);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let crypto = openmls_traits::OpenMlsProvider::crypto(&provider);
            openmls_traits::crypto::OpenMlsCrypto::hpke_seal(
                crypto,
                nuschel_e2ee::SUITE.hpke_config(),
                package.hpke_init_key().as_slice(),
                b"info",
                b"aad",
                b"secret",
            )
            .is_ok()
        }));
        // libcrux-kem 0.0.9 (RUSTSEC-2026-0330) panicked here; the patched 0.0.10
        // (vendor/hpke-rs-libcrux) refuses the key with an error.
        assert!(
            matches!(outcome, Ok(false)),
            "a 32-byte X-Wing key must be refused with an error, not a panic or a ciphertext"
        );
    }
    // The real flow never gets there: a group with Alice still works.
    let _ = alice.key_package();
}

#[test]
fn trees_with_short_keys_are_refused() {
    let (_alice, _bob, ds, _group) = pair();
    let (_, tree) = ds.join_info().unwrap();
    let leaves = prevalidate::ratchet_tree(&tree).unwrap();
    assert_eq!((leaves.len(), leaves.width), (2, 2));
    // The first leaf's encryption key: after the tree's length prefix (2 or 4
    // bytes), the presence byte and the node type, its own 2-byte length (1216).
    let mut broken = tree.clone();
    let prefix = match broken[0] >> 6 {
        1 => 2,
        2 => 4,
        _ => panic!("unexpected tree length prefix"),
    };
    assert_eq!(&broken[prefix..prefix + 4], &[1, 1, 0x44, 0xc0]);
    broken[prefix + 3] = 0xbf;
    assert!(prevalidate::ratchet_tree(&broken).is_err());
}

/// A channel (docs/e2ee.md §6.2): its first device creates the group alone; others
/// join by external commit from the server's GroupInfo and tree; members cannot
/// add anyone; the server proposes a removal, a member commits it, and the removed
/// device learns it is out.
#[test]
fn a_channel_with_external_joins_and_the_servers_removals() {
    let alice = Device::new("alice");
    let bob = Device::new("bob");
    let carol = Device::new("carol");
    let binding = ConversationBinding::channel(SERVER, [3; 16]);
    let created = alice
        .client
        .create(
            &alice.identity.device,
            &alice.identity.device_credential,
            &binding,
            &server_key(),
            &[],
        )
        .unwrap();
    let (mut ds, _) = DeliveryGroup::create(
        &created.init_group_info,
        &created.init_tree,
        &SERVER,
        &[3; 16],
        &server_key(),
    )
    .unwrap();
    // The first commit is a self-update: the GroupInfo external joiners need.
    let outcome = ds
        .commit(&created.first.commit, &created.first.group_info, None)
        .unwrap();
    assert_eq!((outcome.epoch, outcome.members.len()), (1, 1));
    alice.client.merge_pending(&created.group_id).unwrap();
    let group = created.group_id;

    // Bob and Carol join by themselves.
    let mut join = |device: &Device| {
        let (info, tree) = ds.join_info().unwrap();
        let joined = device
            .client
            .join_external(
                &device.identity.device,
                &device.identity.device_credential,
                &info,
                &tree,
                &server_key(),
            )
            .unwrap();
        assert_eq!(joined.binding, binding);
        let outcome = ds
            .commit(&joined.outgoing.commit, &joined.outgoing.group_info, None)
            .unwrap();
        assert!(outcome.external_join);
        assert_eq!(outcome.added.len(), 1);
        joined.outgoing.commit
    };
    let bob_join = join(&bob);
    match alice.client.process(&group, &bob_join).unwrap() {
        Incoming::Commit(commit) => {
            assert!(commit.external_join && commit.adds == 0);
            alice.client.merge(&group, commit).unwrap();
        }
        _ => panic!("expected the join"),
    }
    let carol_join = join(&carol);
    for member in [&alice, &bob] {
        match member.client.process(&group, &carol_join).unwrap() {
            Incoming::Commit(commit) => {
                assert_eq!(commit.members.len(), 3);
                member.client.merge(&group, commit).unwrap();
            }
            _ => panic!("expected the join"),
        }
    }
    let message = carol
        .client
        .encrypt(&carol.identity.device, &group, b"hello channel")
        .unwrap();
    ds.fence(&message).unwrap();
    for member in [&alice, &bob] {
        assert!(matches!(
            member.client.process(&group, &message).unwrap(),
            Incoming::Application { data, .. } if data == b"hello channel"
        ));
    }

    // Members do not add devices to a channel.
    let dave = Device::new("dave");
    let package = ValidKeyPackage::check(&dave.key_package()).unwrap();
    let add = alice
        .client
        .add(&alice.identity.device, &group, &[package])
        .unwrap();
    assert!(
        ds.commit(&add.commit, &add.group_info, add.welcome.as_deref())
            .is_err()
    );
    alice.client.clear_pending(&group).unwrap();
    // Nor remove anyone the server did not ask to remove.
    let bob_leaf = bob.client.own_index(&group).unwrap();
    let removal = alice
        .client
        .remove(&alice.identity.device, &group, &[bob_leaf])
        .unwrap();
    assert!(
        ds.commit(&removal.commit, &removal.group_info, None)
            .is_err()
    );
    alice.client.clear_pending(&group).unwrap();

    // The server proposes Bob's removal; Carol commits it.
    let proposal = ds.propose_removal(bob_leaf, &SIGNER).unwrap();
    for member in [&alice, &bob, &carol] {
        assert!(matches!(
            member.client.process(&group, &proposal).unwrap(),
            Incoming::Proposal { from_server: true, removes: Some(leaf) } if leaf == bob_leaf
        ));
    }
    assert_eq!(
        carol.client.pending_removals(&group).unwrap(),
        vec![bob_leaf]
    );
    // Bob cannot commit his own removal.
    assert!(
        bob.client
            .commit_proposals(&bob.identity.device, &group)
            .is_err()
    );
    let commit = carol
        .client
        .commit_proposals(&carol.identity.device, &group)
        .unwrap();
    let outcome = ds.commit(&commit.commit, &commit.group_info, None).unwrap();
    assert_eq!(outcome.removed.len(), 1);
    assert_eq!(outcome.members.len(), 2);
    carol.client.merge_pending(&group).unwrap();
    match alice.client.process(&group, &commit.commit).unwrap() {
        Incoming::Commit(commit) => {
            assert!(commit.removals_by_server && commit.removed.len() == 1);
            alice.client.merge(&group, commit).unwrap();
        }
        _ => panic!("expected the removal"),
    }
    match bob.client.process(&group, &commit.commit).unwrap() {
        Incoming::Commit(commit) => {
            assert!(commit.self_removed);
            bob.client.merge(&group, commit).unwrap();
            assert!(!bob.client.is_active(&group));
        }
        _ => panic!("expected the removal"),
    }
    assert_eq!(
        alice.client.epoch_authenticator(&group).unwrap(),
        carol.client.epoch_authenticator(&group).unwrap()
    );
    // A proposal for someone who is not there, or forged by another key, is refused.
    assert!(ds.propose_removal(99, &SIGNER).is_err());
    let forger = ServerSigner::generate().unwrap();
    assert!(ds.propose_removal(0, &forger).is_err());
}

#[test]
fn archive_exporters_are_shared_only_in_the_same_conversation_and_epoch() {
    let (alice, bob, mut ds, group) = pair();
    let before = alice.client.history_key(&group).unwrap();
    assert_eq!(before, bob.client.history_key(&group).unwrap());
    let other = ConversationBinding::channel(SERVER, [9; 16]);
    let created = alice
        .client
        .create(
            &alice.identity.device,
            &alice.identity.device_credential,
            &other,
            &server_key(),
            &[],
        )
        .unwrap();
    assert_ne!(
        before.key,
        alice.client.history_key(&created.group_id).unwrap().key
    );
    let update = bob
        .client
        .self_update(&bob.identity.device, &group)
        .unwrap();
    ds.commit(&update.commit, &update.group_info, None).unwrap();
    bob.client.merge_pending(&group).unwrap();
    match alice.client.process(&group, &update.commit).unwrap() {
        Incoming::Commit(commit) => {
            alice.client.merge(&group, commit).unwrap();
        }
        _ => panic!("expected a commit"),
    }
    let after = alice.client.history_key(&group).unwrap();
    assert_eq!(after, bob.client.history_key(&group).unwrap());
    assert_ne!(before.key, after.key);
    assert_eq!(after.index, before.index + 1);
}
