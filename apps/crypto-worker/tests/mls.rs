// SPDX-License-Identifier: EUPL-1.2
//! End-to-end-encrypted direct conversations through the worker (docs/e2ee.md §6,
//! §7), in process: nuschel-kt's directory stands in for the server's log, the
//! test plays the delivery service with nuschel-e2ee's `DeliveryGroup` and assigns
//! seqs itself, and "raw" devices (nuschel-e2ee's client driven directly) play
//! members that misbehave or are not workers.
mod common;

use common::*;
use nuschel_crypto_worker::{KeySource, Store};
use nuschel_e2ee::client::{Client as RawClient, Incoming, ValidKeyPackage};
use nuschel_e2ee::ds::{DeliveryGroup, DsError};
use nuschel_e2ee::envelope::{Envelope, Kind};
use nuschel_e2ee::{ConversationBinding, SUITE_KEM_PUBLIC_KEY_LEN};
use nuschel_identity::{
    DeviceCertificate, DeviceCredential, DeviceKeys, DeviceKeystore, DisplayName, NewIdentity,
    Validity,
};
use nuschel_ipc::crypto::{
    ConversationView, CryptoRequest, CryptoResponse, KtRecordBytes, Lock, MlsOutgoing, Order,
    Received,
};
use nuschel_kt::{AccountBinding, Directory, RosterCommitment};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

const CONVERSATION: [u8; 16] = [0xc0; 16];

fn conversation() -> Vec<u8> {
    CONVERSATION.to_vec()
}

// ---- the delivery service ----

struct Ds {
    group: Option<DeliveryGroup>,
    /// seq - 1 → message
    stream: Vec<Vec<u8>>,
    log_id: [u8; 32],
}

impl Ds {
    fn new(dir: &Dir) -> Self {
        Self {
            group: None,
            stream: Vec::new(),
            log_id: dir.descriptor().log_id().0,
        }
    }

    fn commit(&mut self, outgoing: &MlsOutgoing) -> Result<u64, DsError> {
        if let Some(init) = &outgoing.create {
            let (group, _) =
                DeliveryGroup::create(&init.group_info, &init.tree, &self.log_id, &CONVERSATION)
                    .map_err(DsError::from)?;
            self.group = Some(group);
        }
        let welcome = (!outgoing.welcome.is_empty()).then_some(outgoing.welcome.as_slice());
        self.group
            .as_mut()
            .unwrap()
            .commit(&outgoing.commit, &outgoing.group_info, welcome)?;
        self.stream.push(outgoing.commit.clone());
        Ok(self.stream.len() as u64)
    }

    fn raw_commit(&mut self, commit: &[u8], info: &[u8], welcome: Option<&[u8]>) -> u64 {
        self.group
            .as_mut()
            .unwrap()
            .commit(commit, info, welcome)
            .unwrap();
        self.stream.push(commit.to_vec());
        self.stream.len() as u64
    }

    fn send(&mut self, message: &[u8]) -> Result<u64, DsError> {
        self.group.as_ref().unwrap().fence(message)?;
        self.stream.push(message.to_vec());
        Ok(self.stream.len() as u64)
    }

    fn tree(&self) -> Vec<u8> {
        self.group.as_ref().unwrap().join_info().unwrap().1
    }

    fn at(&self, seq: u64) -> Vec<u8> {
        self.stream[seq as usize - 1].clone()
    }
}

// ---- workers ----

fn view(response: CryptoResponse) -> ConversationView {
    match response {
        CryptoResponse::MlsConversation(view) => view,
        other => panic!("expected a conversation: {other:?}"),
    }
}

fn outgoing(response: CryptoResponse) -> MlsOutgoing {
    match response {
        CryptoResponse::MlsOutgoing(outgoing) => outgoing,
        other => panic!("expected a commit: {other:?}"),
    }
}

fn received(response: CryptoResponse) -> Received {
    match response {
        CryptoResponse::MlsReceived(received) => received,
        other => panic!("expected a received message: {other:?}"),
    }
}

impl Client {
    fn key_packages(&mut self, count: u32) -> Vec<Vec<u8>> {
        match self.ask(CryptoRequest::MlsKeyPackages {
            server: SERVER.into(),
            one_time: count,
            last_resort: false,
        }) {
            CryptoResponse::MlsKeyPackages(batch) => {
                assert_eq!(batch.device.len(), 16);
                batch.one_time
            }
            other => panic!("{other:?}"),
        }
    }

    fn status(&mut self) -> ConversationView {
        view(self.ask(CryptoRequest::MlsStatus {
            server: SERVER.into(),
            conversation: conversation(),
        }))
    }

    fn create(&mut self, peer: [u8; 16], packages: Vec<Vec<u8>>) -> CryptoResponse {
        self.ask(CryptoRequest::MlsCreateDirect {
            server: SERVER.into(),
            conversation: conversation(),
            peer: peer.to_vec(),
            key_packages: packages,
        })
    }

    fn done(&mut self, accepted: bool, seq: u64) -> ConversationView {
        view(self.ask(CryptoRequest::MlsCommitDone {
            server: SERVER.into(),
            conversation: conversation(),
            accepted,
            seq,
        }))
    }

    fn welcome(&mut self, peer: [u8; 16], welcome: &[u8], tree: &[u8], seq: u64) -> CryptoResponse {
        self.ask(CryptoRequest::MlsWelcome {
            server: SERVER.into(),
            conversation: conversation(),
            peer: peer.to_vec(),
            welcome: welcome.to_vec(),
            tree: tree.to_vec(),
            seq,
        })
    }

    fn receive(&mut self, ds: &Ds, seq: u64) -> Received {
        received(self.ask(CryptoRequest::MlsReceive {
            server: SERVER.into(),
            conversation: conversation(),
            seq,
            message: ds.at(seq),
        }))
    }

    fn receive_bytes(&mut self, seq: u64, message: Vec<u8>) -> Received {
        received(self.ask(CryptoRequest::MlsReceive {
            server: SERVER.into(),
            conversation: conversation(),
            seq,
            message,
        }))
    }

    fn encrypt(&mut self, key: [u8; 16], text: &str) -> Result<Vec<u8>, String> {
        match self.ask(CryptoRequest::MlsEncrypt {
            server: SERVER.into(),
            conversation: conversation(),
            idempotency_key: key.to_vec(),
            text: text.into(),
        }) {
            CryptoResponse::MlsCiphertext { message, .. } => Ok(message),
            CryptoResponse::Rejected(reason) => Err(reason),
            other => panic!("{other:?}"),
        }
    }

    fn sent(&mut self, key: [u8; 16], seq: u64) {
        assert!(matches!(
            self.ask(CryptoRequest::MlsSent {
                server: SERVER.into(),
                conversation: conversation(),
                idempotency_key: key.to_vec(),
                seq,
            }),
            CryptoResponse::Done
        ));
    }

    /// Sends `text` through the delivery service; returns its seq.
    fn say(&mut self, ds: &mut Ds, key: u8, text: &str) -> u64 {
        let message = self.encrypt([key; 16], text).unwrap();
        let seq = ds.send(&message).unwrap();
        self.sent([key; 16], seq);
        seq
    }
}

fn text_of(received: &Received) -> (&str, Order) {
    match received {
        Received::Message { text, order, .. } => (text, *order),
        other => panic!("expected a message: {other:?}"),
    }
}

/// A worker connected to the directory as `account`, its binding published.
async fn worker(dir: &Dir, account: u8, at: u64) -> (Client, Person) {
    let mut client = Client::new();
    let head = dir.latest_head().await.unwrap();
    client.connect(dir, [account; 16], &head);
    let person = client.publish_own(dir, at).await;
    (client, person)
}

/// Alice and Bob, both workers, know each other through key transparency.
async fn pair() -> (Dir, Client, Person, Client, Person) {
    let dir = directory([0x11; 32]).await;
    let (mut alice, alice_person) = worker(&dir, 1, T0 + 10).await;
    let (mut bob, bob_person) = worker(&dir, 2, T0 + 20).await;
    alice.sync(&dir).await;
    bob.sync(&dir).await;
    alice.contact(&dir, &bob_person).await;
    bob.contact(&dir, &alice_person).await;
    (dir, alice, alice_person, bob, bob_person)
}

/// Alice creates the group with Bob's devices; every `joiner` joins with the Welcome.
fn establish(ds: &mut Ds, alice: &mut Client, alice_account: [u8; 16], bob: &mut Client) -> u64 {
    let peer = bob.account;
    let packages = bob.key_packages(2);
    let commit = outgoing(alice.create(peer, packages[..1].to_vec()));
    assert!(commit.create.is_some());
    assert!(alice.status().pending_commit);
    let seq = ds.commit(&commit).unwrap();
    let view = alice.done(true, seq);
    assert!(view.pinned && view.active);
    assert_eq!(view.members.len(), 2);
    assert_eq!(view.lock, Lock::Encrypted);
    let joined = view_of(bob.welcome(alice_account, &commit.welcome, &ds.tree(), seq));
    assert!(joined.pinned);
    assert_eq!(joined.cursor, seq);
    assert_eq!(joined.lock, Lock::Encrypted);
    seq
}

fn view_of(response: CryptoResponse) -> ConversationView {
    view(response)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_conversation_between_two_workers() {
    let (dir, mut alice, alice_person, mut bob, _) = pair().await;
    let mut ds = Ds::new(&dir);
    // No group yet: pending (the session shows red without mls/1).
    assert!(matches!(alice.status().lock, Lock::Pending { .. }));
    assert!(!alice.status().pinned);
    assert!(alice.encrypt([9; 16], "too early").is_err());
    establish(&mut ds, &mut alice, alice_person.account, &mut bob);

    let seq = alice.say(&mut ds, 1, "hello bob");
    let message = bob.receive(&ds, seq);
    assert_eq!(text_of(&message), ("hello bob", Order::InOrder));
    if let Received::Message { sender, .. } = &message {
        assert!(sender.verified && !sender.own);
        assert_eq!(sender.device_name, "laptop");
    }
    // Our own message coming back is shown from what we sent.
    assert!(matches!(
        alice.receive(&ds, seq),
        Received::Own { text, .. } if text == "hello bob"
    ));
    let reply = bob.say(&mut ds, 2, "hi alice");
    assert_eq!(
        text_of(&alice.receive(&ds, reply)),
        ("hi alice", Order::InOrder)
    );
    let again = bob.say(&mut ds, 3, "second");
    assert_eq!(
        text_of(&alice.receive(&ds, again)),
        ("second", Order::InOrder)
    );
    // A replay of a processed seq is a duplicate.
    assert!(matches!(
        alice.receive(&ds, again),
        Received::Duplicate { .. }
    ));
    // The server replays the same ciphertext under a new seq: MLS refuses it.
    let replayed = ds.stream.len() as u64 + 1;
    assert!(matches!(
        alice.receive_bytes(replayed, ds.at(again)),
        Received::Undecryptable { .. } | Received::Duplicate { .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn state_survives_a_worker_restart() {
    let (dir, mut alice, alice_person, mut bob, _) = pair().await;
    let mut ds = Ds::new(&dir);
    establish(&mut ds, &mut alice, alice_person.account, &mut bob);
    let seq = bob.say(&mut ds, 1, "before");
    assert_eq!(text_of(&alice.receive(&ds, seq)).0, "before");
    alice.restart();
    bob.restart();
    let view = alice.status();
    assert!(view.pinned);
    assert_eq!(view.cursor, seq);
    let seq = bob.say(&mut ds, 2, "after restart");
    assert_eq!(
        text_of(&alice.receive(&ds, seq)),
        ("after restart", Order::InOrder)
    );
    let seq = alice.say(&mut ds, 3, "and back");
    assert_eq!(text_of(&bob.receive(&ds, seq)).0, "and back");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malicious_server_that_drops_reorders_and_forks_is_noticed() {
    let (dir, mut alice, alice_person, mut bob, _) = pair().await;
    let mut ds = Ds::new(&dir);
    establish(&mut ds, &mut alice, alice_person.account, &mut bob);
    let one = alice.say(&mut ds, 1, "one");
    let two = alice.say(&mut ds, 2, "two");
    let three = alice.say(&mut ds, 3, "three");
    let four = alice.say(&mut ds, 4, "four");
    // Dropped: Bob never gets "two"; "three" shows the gap.
    assert_eq!(text_of(&bob.receive(&ds, one)).1, Order::InOrder);
    assert_eq!(
        text_of(&bob.receive(&ds, three)).1,
        Order::Gap { missing: 1 }
    );
    let notices = bob.status().notices;
    assert!(
        notices.iter().any(|n| n.contains("not delivered")),
        "{notices:?}"
    );
    // Reordered: "two" comes later under a new seq; it fills its gap.
    let late = ds.stream.len() as u64 + 1;
    assert_eq!(
        text_of(&bob.receive_bytes(late, ds.at(two))),
        ("two", Order::InOrder)
    );
    let _ = four;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hostile_key_package_is_refused_and_the_worker_keeps_answering() {
    let (dir, mut alice, _, _bob, bob_person) = pair().await;
    let mallory = RawDevice::new("mallory");
    let poisoned = short_init_key_package(&mallory);
    let response = alice.create(bob_person.account, vec![poisoned]);
    assert!(
        matches!(&response, CryptoResponse::Rejected(reason) if reason.contains("none of the contact's devices")),
        "{response:?}"
    );
    // Still alive, nothing pinned.
    assert!(!alice.status().pinned);
    let _ = dir;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_epoch_resend_reuses_the_envelope() {
    let dir = directory([0x11; 32]).await;
    let (mut alice, alice_person) = worker(&dir, 1, T0 + 10).await;
    let bob = RawDevice::new("bob");
    let bob_person = bob.person(&dir, 2, &[]);
    bob_person.publish(&dir, T0 + 20).await;
    alice.sync(&dir).await;
    alice.contact(&dir, &bob_person).await;
    let mut ds = Ds::new(&dir);
    let commit = outgoing(alice.create(bob_person.account, vec![bob.key_package()]));
    let seq = ds.commit(&commit).unwrap();
    alice.done(true, seq);
    let group = bob.join(&commit.welcome, &ds.tree());
    let _ = alice_person;

    // Alice encrypts for epoch 1; Bob's update moves the group to epoch 2 first.
    let stale = alice.encrypt([5; 16], "resend me").unwrap();
    let update = bob.client.self_update(bob.device(), &group).unwrap();
    let update_seq = ds.raw_commit(&update.commit, &update.group_info, None);
    bob.client.merge_pending(&group).unwrap();
    assert!(matches!(
        ds.send(&stale),
        Err(DsError::StaleEpoch { current: 2 })
    ));
    // Alice catches up and resends with the same key: the same envelope.
    assert!(matches!(
        alice.receive(&ds, update_seq),
        Received::Commit { .. }
    ));
    let fresh = alice.encrypt([5; 16], "resend me").unwrap();
    assert_ne!(fresh, stale);
    let seq = ds.send(&fresh).unwrap();
    alice.sent([5; 16], seq);
    let envelope = bob.open(&group, &fresh);
    assert_eq!(envelope.counter, 1);
    assert_eq!(envelope.idempotency_key, [5; 16]);
    let next = alice.encrypt([6; 16], "next").unwrap();
    ds.send(&next).unwrap();
    let envelope = bob.open(&group, &next);
    assert_eq!(envelope.counter, 2);

    // A fork: Bob shows the same counter with two different texts.
    let first = bob.envelope(1, [0; 32], "original", [7; 16]);
    let forked = bob.envelope(1, [0; 32], "forged", [8; 16]);
    let a = ds
        .send(
            &bob.client
                .encrypt(bob.device(), &group, &first.encode())
                .unwrap(),
        )
        .unwrap();
    let b = ds
        .send(
            &bob.client
                .encrypt(bob.device(), &group, &forked.encode())
                .unwrap(),
        )
        .unwrap();
    assert_eq!(text_of(&alice.receive(&ds, a)).1, Order::InOrder);
    assert_eq!(text_of(&alice.receive(&ds, b)).1, Order::Fork);
    assert!(matches!(alice.status().lock, Lock::Attention { .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_commit_adding_a_stranger_is_refused_and_blocks_the_conversation() {
    let dir = directory([0x11; 32]).await;
    let (mut alice, _) = worker(&dir, 1, T0 + 10).await;
    let bob = RawDevice::new("bob");
    let bob_person = bob.person(&dir, 2, &[]);
    bob_person.publish(&dir, T0 + 20).await;
    alice.sync(&dir).await;
    alice.contact(&dir, &bob_person).await;
    let mut ds = Ds::new(&dir);
    let commit = outgoing(alice.create(bob_person.account, vec![bob.key_package()]));
    let seq = ds.commit(&commit).unwrap();
    alice.done(true, seq);
    let group = bob.join(&commit.welcome, &ds.tree());

    let mallory = RawDevice::new("mallory");
    let package = ValidKeyPackage::check(&mallory.key_package()).unwrap();
    let add = bob.client.add(bob.device(), &group, &[package]).unwrap();
    let seq = ds.raw_commit(&add.commit, &add.group_info, add.welcome.as_deref());
    match alice.receive(&ds, seq) {
        Received::Refused { reason, view, .. } => {
            assert!(reason.contains("another identity"), "{reason}");
            assert!(view.blocked.is_some());
            assert!(matches!(view.lock, Lock::Attention { .. }));
        }
        other => panic!("{other:?}"),
    }
    assert!(alice.encrypt([1; 16], "to mallory?").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_key_transparency_does_not_list_blocks_sending_until_it_does() {
    let dir = directory([0x11; 32]).await;
    let (mut alice, _) = worker(&dir, 1, T0 + 10).await;
    let bob = RawDevice::new("bob");
    let bob_person = bob.person(&dir, 2, &[]);
    bob_person.publish(&dir, T0 + 20).await;
    alice.sync(&dir).await;
    alice.contact(&dir, &bob_person).await;
    let mut ds = Ds::new(&dir);
    let commit = outgoing(alice.create(bob_person.account, vec![bob.key_package()]));
    let seq = ds.commit(&commit).unwrap();
    alice.done(true, seq);
    let group = bob.join(&commit.welcome, &ds.tree());

    // Bob's second device, certified by his device-signing key but not yet in the log.
    let phone = bob.second_device("phone");
    let package = ValidKeyPackage::check(&phone.key_package()).unwrap();
    let add = bob.client.add(bob.device(), &group, &[package]).unwrap();
    let seq = ds.raw_commit(&add.commit, &add.group_info, add.welcome.as_deref());
    bob.client.merge_pending(&group).unwrap();
    match alice.receive(&ds, seq) {
        Received::Commit { view, .. } => {
            assert_eq!(view.members.len(), 3);
            assert!(view.blocked.is_some(), "{view:?}");
            assert!(matches!(view.lock, Lock::Pending { .. }), "{view:?}");
            assert!(view.members.iter().any(|m| !m.verified));
        }
        other => panic!("{other:?}"),
    }
    assert!(alice.encrypt([1; 16], "wait").is_err());
    // Bob publishes the phone; Alice's next lookup confirms it and sending resumes.
    let both = bob.person(&dir, 2, std::slice::from_ref(&phone.certificate));
    both.publish(&dir, T0 + 30).await;
    alice.sync(&dir).await;
    alice.contact(&dir, &both).await;
    let view = alice.status();
    assert_eq!(view.blocked, None, "{view:?}");
    assert_eq!(view.lock, Lock::Encrypted);
    assert!(view.members.iter().all(|m| m.verified));
    assert!(alice.encrypt([1; 16], "now").is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_welcome_for_a_pinned_conversation_is_refused() {
    let (dir, mut alice, alice_person, mut bob, bob_person) = pair().await;
    let mut ds = Ds::new(&dir);
    establish(&mut ds, &mut alice, alice_person.account, &mut bob);
    // The server builds another group for the same conversation with a raw device
    // and offers Alice a Welcome into it.
    let mallory = RawDevice::new("mallory");
    let package = ValidKeyPackage::check(&alice.key_packages(1)[0]).unwrap();
    let created = mallory
        .client
        .create(
            mallory.device(),
            mallory.credential(),
            &ConversationBinding::direct(ds.log_id, CONVERSATION),
            &[package],
        )
        .unwrap();
    let mut other = Ds::new(&dir);
    let (group, _) = DeliveryGroup::create(
        &created.init_group_info,
        &created.init_tree,
        &other.log_id,
        &CONVERSATION,
    )
    .unwrap();
    other.group = Some(group);
    let seq = other.raw_commit(
        &created.first.commit,
        &created.first.group_info,
        created.first.welcome.as_deref(),
    );
    let response = alice.welcome(
        bob_person.account,
        created.first.welcome.as_deref().unwrap(),
        &other.tree(),
        seq,
    );
    assert!(
        matches!(&response, CryptoResponse::Rejected(reason) if reason.contains("replace")),
        "{response:?}"
    );
    // The real conversation still works.
    let seq = bob.say(&mut ds, 1, "still here");
    assert_eq!(text_of(&alice.receive(&ds, seq)).0, "still here");
}

#[tokio::test(flavor = "multi_thread")]
async fn three_devices_two_identities() {
    let dir = directory([0x11; 32]).await;
    let (mut alice, alice_person) = worker(&dir, 1, T0 + 10).await;
    // Bob has two devices of one identity, each with its own worker.
    let bob_identity = new_identity("bob laptop");
    let (laptop_keystore, phone_keystore, phone_certificate) = two_keystores(&bob_identity);
    let mut laptop = worker_with(laptop_keystore);
    let mut phone = worker_with(phone_keystore);
    let bob_account = [2; 16];
    let bob_person = person_for(
        &dir,
        &bob_identity,
        bob_account,
        &[bob_identity.device_certificate.clone(), phone_certificate],
    );
    bob_person.publish(&dir, T0 + 20).await;
    for device in [&mut laptop, &mut phone] {
        let head = dir.latest_head().await.unwrap();
        device.connect(&dir, bob_account, &head);
        device.sync(&dir).await;
        device.contact(&dir, &alice_person).await;
        // Their own account: the other device is in its roster.
        device.contact(&dir, &bob_person).await;
    }
    alice.sync(&dir).await;
    alice.contact(&dir, &bob_person).await;

    let mut ds = Ds::new(&dir);
    let mut packages = laptop.key_packages(1);
    packages.extend(phone.key_packages(1));
    let commit = outgoing(alice.create(bob_account, packages));
    let seq = ds.commit(&commit).unwrap();
    let created = alice.done(true, seq);
    assert_eq!(created.members.len(), 3);
    assert_eq!(created.lock, Lock::Encrypted);
    for device in [&mut laptop, &mut phone] {
        let joined = view(device.welcome(alice_person.account, &commit.welcome, &ds.tree(), seq));
        assert_eq!(joined.lock, Lock::Encrypted, "{joined:?}");
        assert_eq!(joined.members.iter().filter(|m| m.own).count(), 2);
    }
    let hello = alice.say(&mut ds, 1, "hello both");
    assert_eq!(text_of(&laptop.receive(&ds, hello)).0, "hello both");
    // The phone is offline for a while; the laptop answers.
    let answer = laptop.say(&mut ds, 2, "from the laptop");
    assert_eq!(text_of(&alice.receive(&ds, answer)).0, "from the laptop");
    // The phone catches up from the stream, its own identity's message included.
    assert_eq!(text_of(&phone.receive(&ds, hello)).0, "hello both");
    match phone.receive(&ds, answer) {
        Received::Message { sender, text, .. } => {
            assert_eq!(text, "from the laptop");
            assert!(sender.own && sender.verified);
        }
        other => panic!("{other:?}"),
    }
    let from_phone = phone.say(&mut ds, 3, "from the phone");
    assert_eq!(text_of(&alice.receive(&ds, from_phone)).0, "from the phone");
    assert_eq!(
        text_of(&laptop.receive(&ds, from_phone)).0,
        "from the phone"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_creations_one_wins_and_the_other_joins() {
    let (dir, mut alice, alice_person, mut bob, bob_person) = pair().await;
    let mut ds = Ds::new(&dir);
    let from_alice = outgoing(alice.create(bob_person.account, bob.key_packages(1)));
    let from_bob = outgoing(bob.create(alice_person.account, alice.key_packages(1)));
    let seq = ds.commit(&from_alice).unwrap();
    // The server already has a group: Bob's creation is refused (STALE_EPOCH).
    assert!(ds.group.as_ref().unwrap().epoch() == 1);
    let refused = bob.done(false, 0);
    assert!(!refused.pinned && !refused.pending_commit);
    alice.done(true, seq);
    let joined = view(bob.welcome(alice_person.account, &from_alice.welcome, &ds.tree(), seq));
    assert!(joined.pinned);
    let _ = from_bob;
    let hello = bob.say(&mut ds, 1, "joined after all");
    assert_eq!(text_of(&alice.receive(&ds, hello)).0, "joined after all");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_acknowledgement_is_settled_by_the_stream() {
    let (dir, mut alice, alice_person, mut bob, _) = pair().await;
    let mut ds = Ds::new(&dir);
    let commit = outgoing(alice.create(bob.account, bob.key_packages(1)));
    let seq = ds.commit(&commit).unwrap();
    // The ack never arrives; after reconnecting, the page has our commit.
    assert!(alice.status().pending_commit);
    match alice.receive(&ds, seq) {
        Received::Commit { view, .. } => {
            assert!(view.pinned && !view.pending_commit);
            assert_eq!(view.epoch, 1);
        }
        other => panic!("{other:?}"),
    }
    // A late acknowledgement changes nothing.
    assert!(alice.done(true, seq).pinned);
    view(bob.welcome(alice_person.account, &commit.welcome, &ds.tree(), seq));
    let hello = alice.say(&mut ds, 1, "settled");
    assert_eq!(text_of(&bob.receive(&ds, hello)).0, "settled");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_device_stops_sending() {
    let dir = directory([0x11; 32]).await;
    let (mut alice, _) = worker(&dir, 1, T0 + 10).await;
    let bob = RawDevice::new("bob");
    let bob_person = bob.person(&dir, 2, &[]);
    bob_person.publish(&dir, T0 + 20).await;
    alice.sync(&dir).await;
    alice.contact(&dir, &bob_person).await;
    let mut ds = Ds::new(&dir);
    let commit = outgoing(alice.create(bob_person.account, vec![bob.key_package()]));
    let seq = ds.commit(&commit).unwrap();
    alice.done(true, seq);
    let group = bob.join(&commit.welcome, &ds.tree());
    let alice_leaf = 0;
    let remove = bob
        .client
        .remove(bob.device(), &group, &[alice_leaf])
        .unwrap();
    let seq = ds.raw_commit(&remove.commit, &remove.group_info, None);
    match alice.receive(&ds, seq) {
        Received::Commit { view, .. } => {
            assert!(!view.active);
            assert!(view.blocked.as_deref().unwrap().contains("removed"));
        }
        other => panic!("{other:?}"),
    }
    assert!(alice.encrypt([1; 16], "anyone?").is_err());
}

// ---- raw devices and identities ----

fn now_s() -> u64 {
    T0 / 1000
}

fn new_identity(name: &str) -> NewIdentity {
    let validity = Validity::starting_at(now_s() - 3600, 86_400 * 365).unwrap();
    NewIdentity::create(&DisplayName::new(name).unwrap(), validity, now_s()).unwrap()
}

/// The identity's first device's keystore, and a second device certified by the
/// device-signing key (what linking produces).
fn two_keystores(identity: &NewIdentity) -> (DeviceKeystore, DeviceKeystore, DeviceCertificate) {
    let first = identity.keystore().unwrap();
    let keys = DeviceKeys::generate().unwrap();
    let validity = Validity::starting_at(now_s() - 3600, 86_400 * 365).unwrap();
    let (certificate, credential) = identity
        .device_signing
        .certify_device(
            &keys.public_keys(),
            &DisplayName::new("bob phone").unwrap(),
            validity,
        )
        .unwrap();
    let second = DeviceKeystore::new(
        identity.verified_identity().unwrap(),
        keys,
        certificate.clone(),
        credential,
    )
    .unwrap();
    (first, second, certificate)
}

fn worker_with(keystore: DeviceKeystore) -> Client {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        KeySource::File(directory.path().join("store.key")),
    )
    .unwrap();
    store.save_keystore(&keystore).unwrap();
    let clock = Arc::new(AtomicU64::new(T0 + 1000));
    Client {
        worker: open_worker(directory.path(), clock.clone()),
        clock,
        account: [0; 16],
        server: SERVER,
        directory,
    }
}

/// A binding for `account` listing `devices`.
fn person_for(
    dir: &Dir,
    identity: &NewIdentity,
    account: [u8; 16],
    devices: &[DeviceCertificate],
) -> Person {
    let statement = identity
        .device_signing
        .sign_account(dir.descriptor().log_id().0, account, now_s())
        .unwrap();
    let signing = identity.device_signing.certificate().clone();
    let roster = RosterCommitment::compute(&signing, devices, &[]).unwrap();
    let value =
        AccountBinding::with_statement(identity.root.public_key().clone(), roster, &statement)
            .to_value();
    Person {
        account,
        value,
        record: KtRecordBytes {
            device_signing_certificate: signing.encode(),
            devices: devices.iter().map(|d| d.encode()).collect(),
            revocations: Vec::new(),
            statement: statement.encode(),
        },
    }
}

/// A device driven with nuschel-e2ee directly.
struct RawDevice {
    /// A first device holds its identity (and the device-signing key).
    identity: Option<NewIdentity>,
    /// A second device: its own keys and credential.
    extra: Option<(DeviceKeys, DeviceCredential)>,
    certificate: DeviceCertificate,
    client: RawClient,
}

impl RawDevice {
    fn new(name: &str) -> Self {
        let identity = new_identity(name);
        Self {
            certificate: identity.device_certificate.clone(),
            identity: Some(identity),
            extra: None,
            client: RawClient::new().unwrap(),
        }
    }

    fn identity(&self) -> &NewIdentity {
        self.identity.as_ref().expect("a first device")
    }

    /// Another device of the same identity, certified but not published.
    fn second_device(&self, name: &str) -> RawDevice {
        let keys = DeviceKeys::generate().unwrap();
        let validity = Validity::starting_at(now_s() - 3600, 86_400 * 365).unwrap();
        let (certificate, credential) = self
            .identity()
            .device_signing
            .certify_device(
                &keys.public_keys(),
                &DisplayName::new(name).unwrap(),
                validity,
            )
            .unwrap();
        RawDevice {
            identity: None,
            extra: Some((keys, credential)),
            certificate,
            client: RawClient::new().unwrap(),
        }
    }

    fn device(&self) -> &DeviceKeys {
        match &self.extra {
            Some((keys, _)) => keys,
            None => &self.identity().device,
        }
    }

    fn credential(&self) -> &DeviceCredential {
        match &self.extra {
            Some((_, credential)) => credential,
            None => &self.identity().device_credential,
        }
    }

    fn key_package(&self) -> Vec<u8> {
        self.client
            .key_package(self.device(), self.credential(), false)
            .unwrap()
    }

    /// This identity bound to `account`, with the first device and `more`.
    fn person(&self, dir: &Dir, account: u8, more: &[DeviceCertificate]) -> Person {
        let mut devices = vec![self.certificate.clone()];
        devices.extend_from_slice(more);
        person_for(dir, self.identity(), [account; 16], &devices)
    }

    fn join(&self, welcome: &[u8], tree: &[u8]) -> Vec<u8> {
        let pending = self.client.stage_welcome(welcome, tree).unwrap();
        self.client.join(pending).unwrap()
    }

    fn open(&self, group: &[u8], message: &[u8]) -> Envelope {
        match self.client.process(group, message).unwrap() {
            Incoming::Application { data, .. } => Envelope::decode(&data).unwrap(),
            _ => panic!("expected application data"),
        }
    }

    fn envelope(&self, counter: u64, prev: [u8; 32], text: &str, key: [u8; 16]) -> Envelope {
        Envelope {
            kind: Kind::Chat,
            device_id: *self.device().device_id().as_bytes(),
            counter,
            prev,
            seen: [0; 32],
            idempotency_key: key,
            sent_at: T0,
            archive_hash: Vec::new(),
            body: text.as_bytes().to_vec(),
        }
    }
}

/// A key package re-signed with a 32-byte X-Wing init key (crates/e2ee/tests/flow.rs).
fn short_init_key_package(device: &RawDevice) -> Vec<u8> {
    let valid = device.key_package();
    assert_eq!(&valid[4..6], &[0x44, 0xc0]);
    let rest = &valid[6 + SUITE_KEM_PUBLIC_KEY_LEN..];
    assert_eq!(&rest[rest.len() - 66..rest.len() - 64], &[0x40, 0x40]);
    let unsigned_rest = &rest[..rest.len() - 66];
    let mut tbs = valid[..4].to_vec();
    tbs.push(32);
    tbs.extend_from_slice(&[0x55; 32]);
    tbs.extend_from_slice(unsigned_rest);
    let label = b"MLS 1.0 KeyPackageTBS";
    let mut content = vec![label.len() as u8];
    content.extend_from_slice(label);
    let length = tbs.len();
    content.extend_from_slice(&[0x40 | (length >> 8) as u8, length as u8]);
    content.extend_from_slice(&tbs);
    let signature = device.device().sign_leaf(&content).unwrap();
    let mut package = tbs;
    package.extend_from_slice(&[0x40, 0x40]);
    package.extend_from_slice(&signature);
    package
}
