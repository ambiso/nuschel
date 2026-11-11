// SPDX-License-Identifier: EUPL-1.2
//! The delivery service: ordering, epochs, validation, external joins, and what a
//! server can and cannot do to a group.

use nuschel_mls_spike::ds::{DeliveryService, DsError};
use nuschel_mls_spike::pins::ConversationPins;
use nuschel_mls_spike::sim::Client;
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
        client
            .join_checked(&ds, &welcome, &mut ConversationPins::new(SERVER))
            .unwrap();
    }
    (ds, clients)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn server_sees_metadata_not_content() {
    let (mut ds, mut c) = group_of(&["alice", "bob"]);
    let id = c[0].group_id();
    let secret = b"the eagle lands at midnight";
    let seq = c[0]
        .send(&mut ds, std::str::from_utf8(secret).unwrap())
        .unwrap();
    let stored = &ds.after(&id, seq - 1)[0];
    assert!(!stored.handshake);
    assert!(!contains(&stored.bytes, secret));
    let message = from_wire(&stored.bytes)
        .try_into_protocol_message()
        .unwrap();
    assert!(matches!(message, ProtocolMessage::PrivateMessage(_)));
    // Visible: group id, epoch, content type, size, timing, and the roster.
    assert_eq!(message.epoch().as_u64(), 1);
    assert_eq!(ds.members(&id).len(), 2);
    // Commits travel as PublicMessage so the server can validate them.
    let commit = &ds.after(&id, 0)[0];
    assert!(commit.handshake);
    assert!(matches!(
        from_wire(&commit.bytes)
            .try_into_protocol_message()
            .unwrap(),
        ProtocolMessage::PublicMessage(_)
    ));
    println!(
        "application message for {} plaintext bytes: {} bytes on the wire",
        secret.len(),
        stored.bytes.len()
    );
}

#[test]
fn concurrent_commits_one_wins() {
    let (mut ds, mut c) = group_of(&["alice", "bob", "carol"]);
    let (bob_commit, bob_info) = c[1].stage_update();
    let (carol_commit, carol_info) = c[2].stage_update();
    c[1].submit_commit(&mut ds, bob_commit, bob_info).unwrap();
    assert_eq!(
        c[2].submit_commit(&mut ds, carol_commit, carol_info),
        Err(DsError::StaleEpoch { current: 2 })
    );
    // Carol catches up and retries.
    assert!(matches!(
        c[2].sync(&ds)[..],
        [Ok(Received::Commit { epoch: 2, .. })]
    ));
    c[2].update(&mut ds).unwrap();
    c[0].sync(&ds);
    c[1].sync(&ds);
    let auth: Vec<_> = c
        .iter_mut()
        .map(|cl| cl.group().epoch_authenticator().as_slice().to_vec())
        .collect();
    assert!(auth.iter().all(|a| a == &auth[0]));
    assert!(c.iter_mut().all(|cl| cl.group().epoch().as_u64() == 3));
}

#[test]
fn server_rejects_tampered_and_stale_messages() {
    let (mut ds, mut c) = group_of(&["alice", "bob"]);

    // A commit ends with the signature (VLBytes, 64 bytes), the confirmation tag and
    // the membership tag (VLBytes, 32 bytes each). The server verifies the signature
    // against the sender's leaf, so a flipped signature byte is rejected before
    // fan-out...
    let (commit, info) = c[1].stage_update();
    let good = to_wire(&commit);
    let info = info.map(|i| to_wire(&MlsMessageOut::from(i)));
    let mut bytes = good.clone();
    let at = bytes.len() - 33 - 33 - 10;
    bytes[at] ^= 1;
    assert!(matches!(
        ds.submit(&bytes, info.as_deref()),
        Err(DsError::Invalid(_))
    ));
    // ...but the membership and confirmation tags are MACs under epoch secrets the
    // server does not have. A flipped tag passes the server; members reject it.
    let mut bytes = good;
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    let tagged = ds.submit(&bytes, info.as_deref());
    println!("server on a bad membership tag: {tagged:?}");
    let alice = &mut c[0];
    let member = alice.device.receive(alice.group.as_mut().unwrap(), &bytes);
    println!("member on a bad membership tag: {member:?}");
    assert!(member.is_err());
    assert!(tagged.is_ok(), "the server cannot check membership tags");
    // Roll the model server back (a real server never stores what members reject;
    // here it shows the limit). Start over with a fresh group for the next check.
    let (mut ds, mut c) = group_of(&["alice", "bob"]);
    let id = c[0].group_id();
    let bob = &mut c[1];
    bob.group
        .as_mut()
        .unwrap()
        .clear_pending_commit(bob.device.provider.storage())
        .unwrap();
    assert_eq!(ds.epoch(&id), Some(1));

    // An application message for an old epoch is refused; the sender catches up
    // and resends (the idempotency key makes the retry safe).
    let stale = {
        let alice = &mut c[0];
        alice.device.send(alice.group.as_mut().unwrap(), "late")
    };
    c[1].update(&mut ds).unwrap();
    assert_eq!(
        ds.submit(&stale, None),
        Err(DsError::StaleApplicationEpoch { current: 2 })
    );
    c[0].sync(&ds);
    c[0].send(&mut ds, "late").unwrap();
    assert_eq!(Client::texts(c[1].sync(&ds)), vec!["late"]);
}

fn external_join(
    ds: &mut DeliveryService,
    joiner: &mut Client,
    id: &GroupId,
) -> Result<u64, String> {
    let (info, tree) = ds.join_info(id).unwrap();
    let info = match from_wire(&info).extract() {
        MlsMessageBodyIn::GroupInfo(info) => info,
        _ => unreachable!(),
    };
    let device = &joiner.device;
    let (group, bundle) = MlsGroup::external_commit_builder()
        .with_config(Device::join_config())
        .with_ratchet_tree(tree.into())
        .build_group(&device.provider, info, device.credential.clone())
        .map_err(|e| format!("{e:?}"))?
        .leaf_node_parameters(
            LeafNodeParameters::builder()
                .with_capabilities(capabilities(device.suite))
                .build(),
        )
        .load_psks(device.provider.storage())
        .map_err(|e| format!("{e:?}"))?
        .build(
            device.provider.rand(),
            device.provider.crypto(),
            device.signer(),
            |_| true,
        )
        .map_err(|e| format!("{e:?}"))?
        .finalize(&device.provider)
        .map_err(|e| format!("{e:?}"))?;
    // The joiner checks the binding before using the group.
    assert_eq!(
        ConversationBinding::from_extensions(group.extensions()),
        Some(binding())
    );
    joiner.group = Some(group);
    let (commit, _, info) = bundle.into_contents();
    let seq = joiner
        .submit_commit(ds, commit, info)
        .map_err(|e| format!("{e:?}"))?;
    joiner.cursor = seq;
    Ok(seq)
}

/// An open channel: a new device joins by external commit from the server's
/// GroupInfo and tree. It reads messages from its join on, not before.
#[test]
fn external_join_reads_from_its_join_on() {
    let (mut ds, mut c) = group_of(&["alice", "bob"]);
    let id = c[0].group_id();
    let before = c[0].send(&mut ds, "before dave").unwrap();
    c[1].sync(&ds);

    let mut dave = Client::new(Device::new("dave"));
    external_join(&mut ds, &mut dave, &id).unwrap();
    for client in &mut c {
        assert!(matches!(
            client.sync(&ds)[..],
            [Ok(Received::Commit { epoch: 2, .. })]
        ));
        let roster = Device::verify_roster(client.group()).unwrap();
        assert!(roster.iter().any(|d| d.display_name == "dave"));
    }
    let old = ds.after(&id, before - 1)[0].bytes.clone();
    let result = dave.device.receive(dave.group.as_mut().unwrap(), &old);
    println!("dave on a message from before his join: {result:?}");
    assert!(result.is_err());
    c[0].send(&mut ds, "after dave").unwrap();
    assert_eq!(Client::texts(dave.sync(&ds)), vec!["after dave"]);
}

/// The server holds no member key. It cannot forge a commit, and anything it can
/// do (an external join with a credential it controls, or a copied credential) is
/// visible in the roster and fails credential verification or shows a stranger.
#[test]
fn server_cannot_add_members_silently() {
    let (mut ds, mut c) = group_of(&["alice", "bob"]);
    let id = c[0].group_id();

    // A forged group with the same id and binding, advanced to the same epoch: its
    // commits don't verify against the real group's leaves.
    let mallory = Device::new("mallory");
    let mut fake = mallory.create_group(&binding());
    fake.self_update(
        &mallory.provider,
        mallory.signer(),
        LeafNodeParameters::default(),
    )
    .unwrap();
    fake.merge_pending_commit(&mallory.provider).unwrap();
    let bundle = fake
        .self_update(
            &mallory.provider,
            mallory.signer(),
            LeafNodeParameters::default(),
        )
        .unwrap();
    let forged = to_wire(bundle.commit());
    let verdict = ds.submit(&forged, None);
    println!("server on a forged commit: {verdict:?}");
    assert!(matches!(verdict, Err(DsError::Invalid(_))));
    // A malicious server would skip its own check; the members reject it anyway.
    let alice = &mut c[0];
    assert!(
        alice
            .device
            .receive(alice.group.as_mut().unwrap(), &forged)
            .is_err()
    );

    // Mallory joins externally with her own identity: visible as a stranger.
    let mut mallory = Client::new(Device::new("mallory"));
    external_join(&mut ds, &mut mallory, &id).unwrap();
    c[0].sync(&ds);
    let roster = Device::verify_roster(c[0].group()).unwrap();
    let stranger = roster.iter().find(|d| d.display_name == "mallory").unwrap();
    assert_ne!(
        stranger.identity,
        c[1].device.identity.root_public().fingerprint()
    );

    // Eve copies Bob's credential onto her own leaf key: verification fails.
    let mut eve = Device::new("eve");
    eve.credential.credential = c[1].device.credential.credential.clone();
    let mut eve = Client::new(eve);
    external_join(&mut ds, &mut eve, &id).unwrap();
    c[0].sync(&ds);
    let err = Device::verify_roster(c[0].group()).unwrap_err();
    println!("copied credential: {err}");
    assert!(err.contains("does not match the MLS leaf key"));
}

/// Per-conversation pins: the client's own state, not the server, decides.
#[test]
fn pinned_conversation_cannot_be_replaced_or_downgraded() {
    let (mut ds, mut c) = group_of(&["alice", "bob"]);
    let mut bob_pins = ConversationPins::new(SERVER);
    bob_pins.pin(&binding());

    // The server offers the conversation as plaintext: refused.
    assert!(bob_pins.check_plaintext(&binding().conversation).is_err());

    // The server (or Mallory) creates a fresh group with the same binding and
    // welcomes Bob into it: refused while his group is active.
    let mut mallory = Client::new(Device::new("mallory"));
    let mut other_ds = DeliveryService::new();
    mallory.create(&mut other_ds, &binding());
    let bob_device = Device::new("bob-second-device");
    let welcome = mallory
        .add(&mut other_ds, &[bob_device.key_package()])
        .unwrap();
    let mut bob2 = Client::new(bob_device);
    let err = bob2
        .join_checked(&other_ds, &welcome, &mut bob_pins)
        .unwrap_err();
    assert!(err.contains("silently replace"));

    // A replacement generation is accepted once the current group announced it.
    let next = ConversationBinding {
        generation: 1,
        ..binding()
    };
    let mut mallory2 = Client::new(Device::new("alice-again"));
    let mut ds2 = DeliveryService::new();
    mallory2.create(&mut ds2, &next);
    let device = Device::new("bob-third");
    let welcome = mallory2.add(&mut ds2, &[device.key_package()]).unwrap();
    bob_pins.announce_replacement(&next.conversation, 1);
    Client::new(device)
        .join_checked(&ds2, &welcome, &mut bob_pins)
        .unwrap();

    // A binding for another server is refused.
    let foreign = ConversationBinding {
        server: [9; 32],
        ..binding()
    };
    assert!(
        ConversationPins::new(SERVER)
            .check_welcome(&foreign)
            .is_err()
    );
    let _ = c[0].send(&mut ds, "still fine");
}
