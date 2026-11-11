// SPDX-License-Identifier: EUPL-1.2
//! A malicious server against the worker, with nuschel-kt's directory in process as
//! the server: wrong and stale heads, a fork shown to two clients, hidden changes,
//! proofs for the wrong account, records the log does not commit to, and a binding
//! that came and went. Each must be caught by the worker's use of nuschel-kt's
//! checks, with the classification of docs/e2ee.md §5.5.
mod common;

use common::*;
use nuschel_ipc::crypto::{
    ContactState, CryptoRequest, CryptoResponse, HeadOutcome, MonitorOutcome, RecoveryPolicy,
};
use nuschel_kt::{AccountId, Directory, Ed25519HeadSigner, HeadSigner, Label, SignedHead};

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_or_stale_head_is_refused_and_never_accepted() {
    let dir = directory([0x11; 32]).await;
    let mut client = Client::new();
    let head = dir.latest_head().await.unwrap();

    // Signed by another key, under the right log id.
    let forger = Ed25519HeadSigner::from_seed(&[0x99; 32]);
    let forged =
        SignedHead::sign(&forger, head.log_id, head.epoch, [7; 32], head.issued_at_ms).unwrap();
    assert!(matches!(
        client.connect(&dir, [0; 16], &forged),
        HeadOutcome::Failed { reason } if reason.contains("signature")
    ));
    // The genuine head is then accepted: the forged one left no trace.
    assert!(matches!(
        client.connect(&dir, [0; 16], &head),
        HeadOutcome::Accepted { epoch: 0 }
    ));
    // A day and two hours later, the same head is stale.
    client.at(T0 + 26 * 3600 * 1000);
    assert!(matches!(
        client.connect(&dir, [0; 16], &head),
        HeadOutcome::Failed { reason } if reason.contains("old")
    ));
    // A head from far in the future too.
    client.at(T0 - 2 * 3600 * 1000);
    assert!(matches!(
        client.connect(&dir, [0; 16], &head),
        HeadOutcome::Failed { reason } if reason.contains("future")
    ));
    let _ = forger.public_key();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_shown_to_two_clients_is_caught_by_gossip() {
    // One log id, two histories: the operator shows each client its own.
    let honest = directory([0x11; 32]).await;
    let fork = directory([0x11; 32]).await;
    let alice = Person::new(&honest, 1);
    let mallory = Person::new(&honest, 1);
    alice.publish(&honest, T0 + 10).await;
    mallory.publish(&fork, T0 + 10).await;

    let mut bob = Client::new();
    let mut carol = Client::new();
    bob.sync(&honest).await;
    carol.sync(&fork).await;
    // Each one's view is consistent on its own.
    assert_eq!(
        bob.contact(&honest, &alice).await.state,
        ContactState::Unverified
    );
    assert_eq!(
        carol.contact(&fork, &mallory).await.state,
        ContactState::Unverified
    );
    // Carol gossips her head in a conversation with Bob: two roots for one epoch.
    let CryptoResponse::Gossip(gossip) = carol.ask(CryptoRequest::MakeGossip {
        server: SERVER.into(),
        last_gossip_ms: None,
        newest_seen_epoch: None,
    }) else {
        panic!()
    };
    assert_eq!(gossip.len(), nuschel_kt::GOSSIP_BYTES);
    let CryptoResponse::GossipChecked(outcome) = bob.ask(CryptoRequest::CheckGossip {
        server: SERVER.into(),
        gossip,
    }) else {
        panic!()
    };
    assert!(
        outcome.starts_with("misbehaviour") && outcome.contains("two different roots"),
        "{outcome}"
    );
    // Bob keeps the alarm (and the evidence) with his identity view.
    let CryptoResponse::Identity(view) = bob.ask(CryptoRequest::Identity) else {
        panic!()
    };
    assert_eq!(view.alarms.len(), 1);
    // The same head from the honest log is simply consistent.
    let CryptoResponse::Gossip(gossip) = bob.ask(CryptoRequest::MakeGossip {
        server: SERVER.into(),
        last_gossip_ms: None,
        newest_seen_epoch: None,
    }) else {
        panic!()
    };
    let mut dave = Client::new();
    dave.sync(&honest).await;
    assert!(matches!(
        dave.ask(CryptoRequest::CheckGossip { server: SERVER.into(), gossip }),
        CryptoResponse::GossipChecked(outcome) if outcome == "consistent"
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_cannot_prove_its_new_head_extends_the_old_one_is_caught() {
    let honest = directory([0x11; 32]).await;
    let fork = directory([0x11; 32]).await;
    Person::new(&honest, 1).publish(&honest, T0 + 10).await;
    let mut bob = Client::new();
    bob.sync(&honest).await;
    // The fork has its own epoch 1 and moves on to epoch 2.
    Person::new(&fork, 2).publish(&fork, T0 + 10).await;
    Person::new(&fork, 3).publish(&fork, T0 + 20).await;
    let head = fork.latest_head().await.unwrap();
    let HeadOutcome::NeedAudit { from, to } = bob.connect(&fork, [0; 16], &head) else {
        panic!()
    };
    assert_eq!((from, to), (1, 2));
    // The only audit proof the fork has starts at its own epoch 1.
    let audit = fork.audit(from, to).await.unwrap();
    let outcome = bob.ask(CryptoRequest::Advance {
        server: SERVER.into(),
        head: head.encode(),
        audit: audit.encode().unwrap(),
    });
    assert!(
        matches!(&outcome, CryptoResponse::Head(HeadOutcome::Misbehaviour { reason }) if reason.contains("cannot prove")),
        "{outcome:?}"
    );
    // The fork's head was never accepted: the honest log stays the reference.
    let honest_head = honest.latest_head().await.unwrap();
    assert!(matches!(
        bob.connect(&honest, [0; 16], &honest_head),
        HeadOutcome::Accepted { epoch: 1 }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn hidden_changes_wrong_accounts_and_foreign_records_are_not_accepted() {
    let dir = directory([0x11; 32]).await;
    let alice = Person::new(&dir, 1);
    let bob_person = Person::new(&dir, 2);
    alice.publish(&dir, T0 + 10).await;
    bob_person.publish(&dir, T0 + 20).await;
    let mut bob = Client::new();
    bob.sync(&dir).await;
    assert_eq!(
        bob.contact(&dir, &alice).await.state,
        ContactState::Unverified
    );

    // The operator binds Mallory to Alice's account, then hides that version from
    // the history it shows: the proof no longer verifies.
    let mallory = Person::new(&dir, 1);
    mallory.publish(&dir, T0 + 30).await;
    bob.sync(&dir).await;
    let history = dir.history(&alice.label()).await.unwrap();
    let mut hidden = history.clone();
    hidden.proof = nuschel_kt::__private::drop_newest_history_version(&history.proof).unwrap();
    let view = bob.history(&dir, &alice, hidden.encode().unwrap()).await;
    // A failure, first retried quietly: the pin does not move.
    assert_eq!(view.state, ContactState::Unverified);
    let again = bob.history(&dir, &alice, hidden.encode().unwrap()).await;
    assert!(
        matches!(&again.state, ContactState::Unavailable { reason, verified: false } if reason.contains("proof")),
        "{again:?}"
    );
    assert_eq!(again.identity, view.identity);

    // A lookup proof for another account is refused.
    let wrong = dir.lookup(&bob_person.label()).await.unwrap();
    let CryptoResponse::Contact(view) = bob.ask(CryptoRequest::Contact {
        server: SERVER.into(),
        account: alice.account.to_vec(),
        lookup: wrong.encode().unwrap(),
        record: Some(alice.record.clone()),
        direct: false,
    }) else {
        panic!()
    };
    assert!(matches!(view.state, ContactState::Unavailable { .. }));

    // The honest history shows the change: a key change, not silently accepted.
    let view = bob.history(&dir, &alice, history.encode().unwrap()).await;
    assert_eq!(view.state, ContactState::KeyChanged { blocking: false });

    // A record that is not the one the log commits to (Alice's certificates under
    // Mallory's binding) never verifies.
    let lookup = dir.lookup(&alice.label()).await.unwrap();
    let mut carol = Client::new();
    carol.sync(&dir).await;
    let CryptoResponse::Contact(view) = carol.ask(CryptoRequest::Contact {
        server: SERVER.into(),
        account: alice.account.to_vec(),
        lookup: lookup.encode().unwrap(),
        record: Some(alice.record.clone()),
        direct: false,
    }) else {
        panic!()
    };
    assert_eq!(
        view.state,
        ContactState::Unknown,
        "a first failure shows nothing"
    );
    assert_eq!(view.identity, None, "nothing was pinned");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_binding_that_came_and_went_is_shown_and_self_monitoring_catches_it() {
    let dir = directory([0x11; 32]).await;
    let mut alice = Client::new();
    let mut bob = Client::new();
    // Alice publishes through her own worker.
    let head = dir.latest_head().await.unwrap();
    assert!(matches!(
        alice.connect(&dir, [1; 16], &head),
        HeadOutcome::Accepted { .. }
    ));
    let CryptoResponse::Bundle(bundle) = alice.ask(CryptoRequest::PublishBundle {
        server: SERVER.into(),
        device_name: "laptop".into(),
        recovery: RecoveryPolicy::Never,
    }) else {
        panic!()
    };
    let label = Label::Account(AccountId([1; 16]));
    let theirs = Person::new(&dir, 1);
    let ours = Person {
        account: [1; 16],
        value: value_of(&bundle),
        record: bundle.record.clone(),
    };
    ours.publish(&dir, T0 + 10).await;
    bob.sync(&dir).await;
    assert_eq!(
        bob.contact(&dir, &ours).await.state,
        ContactState::Unverified
    );

    // Alice's own check: consistent.
    let monitor = |alice: &mut Client, history: Vec<u8>| match alice.ask(CryptoRequest::Monitor {
        server: SERVER.into(),
        history,
    }) {
        CryptoResponse::Monitor(outcome) => outcome,
        CryptoResponse::NeedAudit { .. } => panic!("audit first"),
        other => panic!("{other:?}"),
    };
    alice.sync(&dir).await;
    let history = dir.history(&label).await.unwrap().encode().unwrap();
    assert!(matches!(
        monitor(&mut alice, history),
        MonitorOutcome::Consistent {
            version: 1,
            current: true
        }
    ));

    // The operator binds another key for a while, then restores Alice's.
    theirs.publish(&dir, T0 + 20).await;
    ours.publish(&dir, T0 + 30).await;
    bob.sync(&dir).await;
    alice.sync(&dir).await;
    let history = dir.history(&label).await.unwrap().encode().unwrap();
    // Bob, checking her history, sees that another identity came and went.
    let view = bob.history(&dir, &ours, history.clone()).await;
    assert_eq!(view.state, ContactState::Unverified);
    assert!(view.notice.as_deref().unwrap().contains("for a while"));
    // Alice's self-monitoring raises the alarm about version 2.
    assert!(matches!(
        monitor(&mut alice, history),
        MonitorOutcome::OwnKeyAlarm { reason } if reason.contains("version 2")
    ));
}
