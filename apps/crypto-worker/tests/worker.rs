// SPDX-License-Identifier: EUPL-1.2
//! The worker's own decisions: the recovery policy, verification that holds on every
//! server whose log binds the same identity, persistence across restarts, the
//! schedule of docs/e2ee.md §5.4, and refusals.
mod common;

use common::*;
use nuschel_ipc::crypto::{
    ContactState, CryptoRequest, CryptoResponse, HeadOutcome, IdentityView, RecoveryPolicy,
};
use nuschel_kt::Directory;

fn identity(client: &mut Client, request: CryptoRequest) -> IdentityView {
    match client.ask(request) {
        CryptoResponse::Identity(view) => view,
        other => panic!("{other:?}"),
    }
}

fn create(client: &mut Client, recovery: RecoveryPolicy) -> IdentityView {
    identity(
        client,
        CryptoRequest::CreateIdentity {
            device_name: "desk".into(),
            recovery,
        },
    )
}

#[test]
fn the_recovery_key_is_offered_once_and_skipping_deletes_the_root() {
    // Offered: shown until saved, and the root stays recoverable.
    let mut saved = Client::new();
    let view = create(&mut saved, RecoveryPolicy::Offer);
    assert!(view.created && view.recovery_pending && view.recoverable && view.can_link);
    assert_eq!(view.device_name, "desk");
    let CryptoResponse::RecoveryKey(key) = saved.ask(CryptoRequest::ShowRecoveryKey) else {
        panic!()
    };
    assert_eq!(key.len(), 56 + 13, "14 groups of 4");
    // Creating again changes nothing.
    let again = create(&mut saved, RecoveryPolicy::Offer);
    assert!(!again.created);
    assert_eq!(again.fingerprint, view.fingerprint);
    // The pending key survives a restart, until it is saved.
    saved.restart();
    assert!(
        matches!(saved.ask(CryptoRequest::ShowRecoveryKey), CryptoResponse::RecoveryKey(k) if k == key)
    );
    let view = identity(&mut saved, CryptoRequest::RecoveryKeySaved);
    assert!(!view.recovery_pending && view.recoverable);
    assert!(matches!(
        saved.ask(CryptoRequest::ShowRecoveryKey),
        CryptoResponse::Rejected(_)
    ));
    // Nothing secret is left readable in the worker's directory.
    for entry in std::fs::read_dir(saved.directory.path()).unwrap() {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        assert!(!bytes.windows(19).any(|w| w == &key.as_bytes()[..19]));
    }

    // Skipped: the recovery blob (and with it the root) is gone.
    let mut skipped = Client::new();
    create(&mut skipped, RecoveryPolicy::Offer);
    let view = identity(&mut skipped, CryptoRequest::SkipRecoveryKey);
    assert!(!view.recovery_pending && !view.recoverable);
    skipped.restart();
    let view = identity(&mut skipped, CryptoRequest::Identity);
    assert!(!view.recoverable && view.fingerprint.is_some());

    // Required: skipping is refused.
    let mut required = Client::new();
    create(&mut required, RecoveryPolicy::Require);
    assert!(matches!(
        required.ask(CryptoRequest::SkipRecoveryKey),
        CryptoResponse::Rejected(reason) if reason.contains("required")
    ));
    // Never: no recovery key at all.
    let mut never = Client::new();
    let view = create(&mut never, RecoveryPolicy::Never);
    assert!(!view.recovery_pending && !view.recoverable);
}

#[tokio::test(flavor = "multi_thread")]
async fn verification_holds_on_every_server_that_binds_the_same_identity() {
    // Alice uses one identity on two servers, X and Y.
    let x = directory([0x11; 32]).await;
    let y = directory([0x33; 32]).await;
    let mut alice = Client::new();
    alice.server = "x.example:64738";
    let head = x.latest_head().await.unwrap();
    alice.connect(&x, [1; 16], &head);
    let on_x = alice.publish_own(&x, T0 + 10).await;
    alice.server = "y.example:64738";
    let head = y.latest_head().await.unwrap();
    alice.connect(&y, [9; 16], &head);
    let on_y = alice.publish_own(&y, T0 + 10).await;
    // Mallory on Y has an identity of her own.
    let mallory = Person::new(&y, 5);
    mallory.publish(&y, T0 + 20).await;

    let mut bob = Client::new();
    bob.server = "x.example:64738";
    bob.sync(&x).await;
    let view = bob.contact(&x, &on_x).await;
    assert_eq!(view.state, ContactState::Unverified);
    let CryptoResponse::Contact(view) = bob.ask(CryptoRequest::MarkVerified {
        server: bob.server.into(),
        account: on_x.account.to_vec(),
    }) else {
        panic!()
    };
    assert_eq!(view.state, ContactState::Verified { elsewhere: None });

    // On Y, Alice's account is bound to the same identity: verified, by the server
    // where Bob compared, never by her (server-controlled) name.
    bob.server = "y.example:64738";
    bob.sync(&y).await;
    let there = bob.contact(&y, &on_y).await;
    assert_eq!(
        there.state,
        ContactState::Verified {
            elsewhere: Some("Label of x.example:64738".into())
        }
    );
    assert_eq!(there.identity, view.identity);
    assert_eq!(there.safety_number, view.safety_number);
    // Mallory is not.
    assert_eq!(
        bob.contact(&y, &mallory).await.state,
        ContactState::Unverified
    );
    // Y cannot present Alice's identity as another of its accounts: her statement
    // names her account on Y only.
    let copied = Person {
        account: [6; 16],
        value: on_y.value.clone(),
        record: on_y.record.clone(),
    };
    copied.publish(&y, T0 + 30).await;
    bob.sync(&y).await;
    let impostor = bob.contact(&y, &copied).await;
    assert_eq!(impostor.identity, None, "nothing pinned for the impostor");
    // Unverifying on X withdraws it everywhere, and it survives a restart.
    bob.server = "x.example:64738";
    bob.ask(CryptoRequest::UnmarkVerified {
        server: bob.server.into(),
        account: on_x.account.to_vec(),
    });
    bob.restart();
    let CryptoResponse::Contact(view) = bob.ask(CryptoRequest::ContactStatus {
        server: "y.example:64738".into(),
        account: on_y.account.to_vec(),
    }) else {
        panic!()
    };
    assert_eq!(view.state, ContactState::Unverified);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_schedule_publishes_monitors_and_rechecks_daily() {
    let dir = directory([0x11; 32]).await;
    let alice_person = Person::new(&dir, 1);
    alice_person.publish(&dir, T0 + 5).await;
    let mut bob = Client::new();
    let head = dir.latest_head().await.unwrap();
    assert!(matches!(
        bob.connect(&dir, [2; 16], &head),
        HeadOutcome::Accepted { epoch: 1 }
    ));
    let due = |bob: &mut Client, connected: bool| match bob.ask(CryptoRequest::Due {
        server: bob.server.into(),
        connected,
    }) {
        CryptoResponse::Due(due) => due,
        other => panic!("{other:?}"),
    };
    // On connect: publish and monitor; no contacts yet.
    let first = due(&mut bob, true);
    assert!(first.publish && first.monitor && first.histories.is_empty());
    let person = bob.publish_own(&dir, T0 + 10).await;
    bob.sync(&dir).await;
    let history = dir.history(&person.label()).await.unwrap();
    assert!(matches!(
        bob.ask(CryptoRequest::Monitor {
            server: bob.server.into(),
            history: history.encode().unwrap(),
        }),
        CryptoResponse::Monitor(nuschel_ipc::crypto::MonitorOutcome::Consistent {
            current: true,
            ..
        })
    ));
    // A direct contact joins the histories: due at once (never checked), then daily.
    bob.contact(&dir, &alice_person).await;
    let quiet = due(&mut bob, false);
    assert!(!quiet.publish && !quiet.monitor);
    assert_eq!(quiet.histories, vec![alice_person.account.to_vec()]);
    let history = dir.history(&alice_person.label()).await.unwrap();
    bob.history(&dir, &alice_person, history.encode().unwrap())
        .await;
    assert!(due(&mut bob, false).histories.is_empty());
    // A day later everything is due again.
    bob.at(T0 + 1000 + 25 * 3600 * 1000);
    let later = due(&mut bob, false);
    assert!(later.monitor);
    assert_eq!(later.histories, vec![alice_person.account.to_vec()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn guests_and_strangers_are_refused_and_bounds_hold() {
    let dir = directory([0x11; 32]).await;
    let mut guest = Client::new();
    let head = dir.latest_head().await.unwrap();
    guest.connect(&dir, [0; 16], &head);
    // An empty account means a guest: nothing to publish.
    guest.ask(CryptoRequest::Connect {
        server: nuschel_ipc::crypto::CryptoServer {
            key: SERVER.into(),
            label: "x".into(),
        },
        descriptor: dir.descriptor().encode(),
        account: Vec::new(),
        head: head.encode(),
    });
    assert!(matches!(
        guest.ask(CryptoRequest::PublishBundle {
            server: SERVER.into(),
            device_name: "x".into(),
            recovery: RecoveryPolicy::Never,
        }),
        CryptoResponse::Rejected(reason) if reason.contains("guest")
    ));
    // Unknown servers, oversized fields and short account ids are refused.
    assert!(matches!(
        guest.ask(CryptoRequest::Monitor {
            server: "unknown:1".into(),
            history: Vec::new(),
        }),
        CryptoResponse::Rejected(_)
    ));
    assert!(matches!(
        guest.ask(CryptoRequest::ContactStatus {
            server: SERVER.into(),
            account: vec![0; 3],
        }),
        CryptoResponse::Rejected(reason) if reason.contains("out of bounds")
    ));
    // Marking someone verified needs a pinned identity first.
    assert!(matches!(
        guest.ask(CryptoRequest::MarkVerified {
            server: SERVER.into(),
            account: vec![4; 16],
        }),
        CryptoResponse::Rejected(_)
    ));
    // A malformed descriptor is a failure, and pins nothing.
    let outcome = guest.ask(CryptoRequest::Connect {
        server: nuschel_ipc::crypto::CryptoServer {
            key: "other:1".into(),
            label: "x".into(),
        },
        descriptor: vec![1, 2, 3],
        account: Vec::new(),
        head: head.encode(),
    });
    assert!(matches!(
        outcome,
        CryptoResponse::Head(HeadOutcome::Failed { .. })
    ));
}
