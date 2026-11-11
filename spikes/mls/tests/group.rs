// SPDX-License-Identifier: EUPL-1.2
//! Task 1.2: a three-party group, removal, forward secrecy, post-compromise security
//! and exporter secrets, through the delivery service model.

use std::sync::RwLock;

use nuschel_mls_spike::ds::DeliveryService;
use nuschel_mls_spike::pins::ConversationPins;
use nuschel_mls_spike::sim::Client;
use nuschel_mls_spike::*;
use openmls::prelude::*;
use openmls_memory_storage::MemoryStorage;
use openmls_traits::OpenMlsProvider;

const SERVER: [u8; 32] = [7; 32];

fn binding() -> ConversationBinding {
    ConversationBinding {
        server: SERVER,
        conversation: [1; 16],
        generation: 0,
    }
}

fn group_of(names: &[&str]) -> (DeliveryService, Vec<Client>) {
    let mut ds = DeliveryService::new();
    let mut clients: Vec<Client> = names.iter().map(|n| Client::new(Device::new(n))).collect();
    clients[0].create(&mut ds, &binding());
    let kps: Vec<_> = clients[1..]
        .iter()
        .map(|c| c.device.key_package())
        .collect();
    let welcome = clients[0].add(&mut ds, &kps).unwrap();
    for client in &mut clients[1..] {
        let mut pins = ConversationPins::new(SERVER);
        client.join_checked(&ds, &welcome, &mut pins).unwrap();
    }
    (ds, clients)
}

fn stored(ds: &DeliveryService, id: &GroupId, seq: u64) -> Vec<u8> {
    ds.after(id, seq - 1)[0].bytes.clone()
}

#[test]
fn three_party_lifecycle() {
    let (mut ds, mut c) = group_of(&["alice", "bob", "carol"]);
    let id = c[0].group_id();
    for client in &mut c {
        assert_eq!(client.group().epoch().as_u64(), 1);
        let roster = Device::verify_roster(client.group()).unwrap();
        assert_eq!(roster.len(), 3);
    }
    assert_eq!(ds.epoch(&id), Some(1));

    // Everyone derives the same SFrame base key for the epoch.
    let keys: Vec<_> = c
        .iter_mut()
        .map(|cl| {
            let group = cl.group.as_ref().unwrap();
            sframe_base_key(&cl.device, group).unwrap()
        })
        .collect();
    assert_eq!(keys[0].len(), 32);
    assert!(keys.iter().all(|k| k == &keys[0]));

    // Application messages, with the sender's verified identity.
    let seq = c[0].send(&mut ds, "hello").unwrap();
    let alice_identity = c[0].device.identity.root_public().fingerprint();
    for client in &mut c[1..] {
        let received = client.sync(&ds);
        match &received[..] {
            [
                Ok(Received::Application {
                    sender: Some(sender),
                    text,
                }),
            ] => {
                assert_eq!(text, "hello");
                assert_eq!(sender.display_name, "alice");
                assert_eq!(sender.identity, alice_identity);
            }
            other => panic!("{other:?}"),
        }
    }

    // Each message key is used once and deleted: a replay fails.
    let replay = stored(&ds, &id, seq);
    let carol = &mut c[2];
    let err = carol
        .device
        .receive(carol.group.as_mut().unwrap(), &replay)
        .unwrap_err();
    println!("replayed message: {err}");

    // Bob removes Carol.
    c[1].remove(&mut ds, "carol").unwrap();
    assert!(matches!(
        c[0].sync(&ds)[..],
        [Ok(Received::Commit {
            epoch: 2,
            removed_self: false
        })]
    ));
    assert!(matches!(
        c[2].sync(&ds)[..],
        [Ok(Received::Commit {
            removed_self: true,
            ..
        })]
    ));
    assert!(!c[2].group().is_active());
    assert!(sframe_base_key(&c[2].device, c[2].group.as_ref().unwrap()).is_err());

    let epoch2: Vec<_> = c[..2]
        .iter()
        .map(|cl| sframe_base_key(&cl.device, cl.group.as_ref().unwrap()).unwrap())
        .collect();
    assert_eq!(epoch2[0], epoch2[1]);
    assert_ne!(epoch2[0], keys[0], "a new epoch has a new exporter secret");

    // Carol cannot read what is sent after her removal.
    let seq = c[0].send(&mut ds, "after carol").unwrap();
    assert_eq!(Client::texts(c[1].sync(&ds)), vec!["after carol"]);
    let bytes = stored(&ds, &id, seq);
    let carol = &mut c[2];
    assert!(
        carol
            .device
            .receive(carol.group.as_mut().unwrap(), &bytes)
            .is_err()
    );
    assert_eq!(ds.members(&id).len(), 2);
}

/// Post-compromise security heals only once the compromised device itself commits
/// a fresh update path. A copy of Bob's state can follow commits by others (their
/// path secrets are encrypted to Bob's old leaf key), but not Bob's own update.
#[test]
fn post_compromise_security_needs_the_victims_own_update() {
    let (mut ds, mut c) = group_of(&["alice", "bob"]);
    let id = c[0].group_id();

    // The thief copies Bob's whole MLS state at epoch 1.
    let stolen = c[1]
        .device
        .provider
        .storage()
        .values
        .read()
        .unwrap()
        .clone();
    let thief = NuschelProvider::with_storage(MemoryStorage {
        values: RwLock::new(stolen),
    });
    let mut thief_group = MlsGroup::load(thief.storage(), &id).unwrap().unwrap();
    let thief_read = |group: &mut MlsGroup, bytes: &[u8]| -> Result<Option<String>, String> {
        let message = from_wire(bytes).try_into_protocol_message().unwrap();
        let processed = group
            .process_message(&thief, message)
            .map_err(|e| format!("{e:?}"))?;
        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(m) => {
                Ok(Some(String::from_utf8(m.into_bytes()).unwrap()))
            }
            ProcessedMessageContent::StagedCommitMessage(commit) => {
                group
                    .merge_staged_commit(&thief, *commit)
                    .map_err(|e| format!("{e:?}"))?;
                Ok(None)
            }
            other => Err(format!("{other:?}")),
        }
    };

    // (a) Alice updates: the thief still follows.
    let commit = c[0].update(&mut ds).unwrap();
    let seq = c[0].send(&mut ds, "the thief can read this").unwrap();
    c[1].sync(&ds);
    assert_eq!(
        thief_read(&mut thief_group, &stored(&ds, &id, commit)),
        Ok(None)
    );
    assert_eq!(
        thief_read(&mut thief_group, &stored(&ds, &id, seq)),
        Ok(Some("the thief can read this".into()))
    );

    // (b) Bob updates his own leaf: the thief is locked out.
    let commit = c[1].update(&mut ds).unwrap();
    c[0].sync(&ds);
    let seq = c[0].send(&mut ds, "healed").unwrap();
    assert_eq!(Client::texts(c[1].sync(&ds)), vec!["healed"]);
    let on_commit = thief_read(&mut thief_group, &stored(&ds, &id, commit));
    let on_message = thief_read(&mut thief_group, &stored(&ds, &id, seq));
    println!("thief on Bob's update: {on_commit:?}");
    println!("thief on the next message: {on_message:?}");
    assert!(on_message.is_err());
}

/// Forward secrecy across epochs: by default (`max_past_epochs = 0`) a member keeps
/// no secrets of past epochs, so a late message from an old epoch is lost. Keeping
/// past epochs trades forward secrecy for tolerance of reordering. The delivery
/// service avoids the trade-off by accepting application messages only for the
/// current epoch (tests/delivery.rs).
#[test]
fn forward_secrecy_across_epochs() {
    for past_epochs in [0usize, 1] {
        let alice = Device::new("alice");
        let bob = Device::new("bob");
        let mut group = alice.create_group(&binding());
        let (_, welcome, _) = group
            .add_members(&alice.provider, alice.signer(), &[bob.key_package()])
            .unwrap();
        group.merge_pending_commit(&alice.provider).unwrap();
        let config = MlsGroupJoinConfig::builder()
            .wire_format_policy(wire_policy())
            .use_ratchet_tree_extension(true)
            .max_past_epochs(past_epochs)
            .build();
        let welcome = match from_wire(&to_wire(&welcome)).extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            _ => unreachable!(),
        };
        let mut bob_group = StagedWelcome::new_from_welcome(&bob.provider, &config, welcome, None)
            .unwrap()
            .into_group(&bob.provider)
            .unwrap();

        let late = alice.send(&mut group, "sent in epoch 1");
        let bundle = group
            .self_update(
                &alice.provider,
                alice.signer(),
                LeafNodeParameters::default(),
            )
            .unwrap();
        group.merge_pending_commit(&alice.provider).unwrap();
        let commit = to_wire(bundle.commit());
        bob.receive(&mut bob_group, &commit).unwrap();
        let result = bob.receive(&mut bob_group, &late);
        let shown = match &result {
            Ok(Received::Application { text, .. }) => format!("Ok({text:?})"),
            other => format!("{other:?}"),
        };
        println!("max_past_epochs={past_epochs}: late epoch-1 message -> {shown}");
        assert_eq!(result.is_ok(), past_epochs == 1);
    }
}
