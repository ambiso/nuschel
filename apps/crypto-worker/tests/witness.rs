// SPDX-License-Identifier: EUPL-1.2
//! Witnessing in the worker (docs/e2ee.md §5.9) against a malicious server: the
//! directory in process stands in for the server, and nuschl-kt's auditor checks
//! with an ML-DSA-44 cosigner stand in for `nuschl-witness`.
mod common;

use common::*;
use nuschl_ipc::crypto::{
    ContactState, ContactView, CryptoRequest, CryptoResponse, CryptoServer, HeadOutcome,
    KeyTransparencySettings, MonitorOutcome, WitnessSetting, WitnessStatus,
};
use nuschl_kt::witness::auditor::{LogState, add_epochs};
use nuschl_kt::witness::note::{Cosigner, Note};
use nuschl_kt::witness::{Checkpoint, WitnessedHead};
use nuschl_kt::{AccountId, Directory, Label, NameBinding, SignedHead};

const ORIGIN: &str = "kt.example.org:64738/nuschl-kt/v1";
const HOUR: u64 = 3600 * 1000;

struct Auditor {
    cosigner: Cosigner,
    state: Option<LogState>,
}

impl Auditor {
    fn new() -> Self {
        Self {
            cosigner: Cosigner::from_seed("witness.example.org/nuschl-kt", &[0x77; 32]).unwrap(),
            state: None,
        }
    }

    fn settings(&self) -> KeyTransparencySettings {
        KeyTransparencySettings {
            witnesses: vec![WitnessSetting {
                key: self.cosigner.key().vkey(),
                enabled: true,
            }],
            ..Default::default()
        }
    }

    /// The server's submission, checked and cosigned at `at_ms`: the cosigned note.
    async fn cosign(&mut self, dir: &Dir, at_ms: u64) -> String {
        let old = self.state.as_ref().map_or(0, LogState::size);
        let request = dir.add_epochs(ORIGIN, old, 4096).await.unwrap();
        let accepted = add_epochs(self.state.as_ref(), &request).unwrap();
        self.state = Some(accepted.state);
        let mut note = Note::parse(&request.checkpoint).unwrap();
        note.signatures.push(
            self.cosigner
                .cosign(&accepted.checkpoint, at_ms / 1000)
                .unwrap(),
        );
        note.format()
    }
}

/// What the server attaches to a head of `epoch`.
async fn attach(dir: &Dir, note: &str, epoch: u64) -> Vec<u8> {
    let size = Checkpoint::parse(&Note::parse(note.as_bytes()).unwrap().text)
        .unwrap()
        .size;
    WitnessedHead {
        checkpoint: note.as_bytes().to_vec(),
        inclusion: if epoch < size {
            dir.inclusion(epoch, size).await.unwrap()
        } else {
            Vec::new()
        },
    }
    .encode()
    .unwrap()
}

/// The checkpoint the server signs alone (no witness reached).
async fn unwitnessed(dir: &Dir) -> String {
    dir.checkpoint(ORIGIN).await.unwrap().1
}

fn connect(
    client: &mut Client,
    dir: &Dir,
    head: &SignedHead,
    witnessed: Option<Vec<u8>>,
    settings: &KeyTransparencySettings,
) -> HeadOutcome {
    match client.ask(CryptoRequest::Connect {
        server: CryptoServer {
            key: SERVER.into(),
            label: "Witnessed".into(),
        },
        descriptor: dir.descriptor().encode(),
        account: client.account.to_vec(),
        head: head.encode(),
        witnessed,
        witnessing: settings.clone(),
        registration: None,
    }) {
        CryptoResponse::Head(outcome) => outcome,
        other => panic!("{other:?}"),
    }
}

/// A lookup of `person` with what the server attached.
async fn lookup(
    client: &mut Client,
    dir: &Dir,
    person: &Person,
    witnessed: Option<Vec<u8>>,
    names: (Option<u32>, Vec<u8>, Option<&str>, Vec<u8>),
) -> ContactView {
    let lookup = dir.lookup(&person.label()).await.unwrap();
    let (user_id, registration, name, name_lookup) = names;
    match client.ask(CryptoRequest::Contact {
        server: SERVER.into(),
        account: person.account.to_vec(),
        lookup: lookup.encode().unwrap(),
        record: Some(person.record.clone()),
        direct: false,
        user_id,
        witnessed,
        registration,
        name_lookup,
        name: name.map(str::to_owned),
    }) {
        CryptoResponse::Contact(view) => view,
        other => panic!("{other:?}"),
    }
}

fn witness(outcome: &HeadOutcome) -> &WitnessStatus {
    match outcome {
        HeadOutcome::Accepted { witness, .. } => witness,
        other => panic!("not accepted: {other:?}"),
    }
}

#[tokio::test]
async fn without_witnesses_nothing_changes_but_checkpoints_are_still_checked() {
    let dir = directory([0x11; 32]).await;
    let mut alice = Client::new();
    let head = dir.latest_head().await.unwrap();
    let off = KeyTransparencySettings::default();
    // No kt-witness/1, or a checkpoint signed by the log alone: as before.
    assert_eq!(
        witness(&connect(&mut alice, &dir, &head, None, &off)),
        &WitnessStatus::Off
    );
    let note = unwitnessed(&dir).await;
    let attached = attach(&dir, &note, head.epoch).await;
    assert_eq!(
        witness(&connect(
            &mut alice,
            &dir,
            &head,
            Some(attached.clone()),
            &off
        )),
        &WitnessStatus::Off
    );
    // A checkpoint that does not contain the head fails it, witnesses or not.
    let mut tampered = WitnessedHead::decode(&attached).unwrap();
    tampered.checkpoint = tampered
        .checkpoint
        .iter()
        .map(|b| if *b == b'1' { b'2' } else { *b })
        .collect();
    let outcome = connect(
        &mut alice,
        &dir,
        &head,
        Some(tampered.encode().unwrap()),
        &off,
    );
    assert!(matches!(outcome, HeadOutcome::Failed { .. }), "{outcome:?}");
}

#[tokio::test]
async fn invalid_keys_and_impossible_quorums_never_reduce_the_policy() {
    let dir = directory([0x11; 32]).await;
    let mut auditor = Auditor::new();
    let mut alice = Client::new();
    let note = auditor.cosign(&dir, T0).await;
    let head = dir.latest_head().await.unwrap();
    let attached = attach(&dir, &note, head.epoch).await;

    // Malformed syntax and a shaped but invalid ML-DSA verifier key both
    // remain required; neither may turn the policy into Off.
    for key in ["not a key", "witness.example.org/kt+00000000+AQ=="] {
        let settings = KeyTransparencySettings {
            default_witnesses: false,
            witnesses: vec![WitnessSetting {
                key: key.into(),
                enabled: true,
            }],
            ..Default::default()
        };
        assert!(settings.check().is_ok(), "within the IPC resource bounds");
        let outcome = connect(&mut alice, &dir, &head, Some(attached.clone()), &settings);
        assert!(
            matches!(witness(&outcome), WitnessStatus::Unavailable { .. }),
            "{outcome:?}"
        );
        assert!(alice.worker.new_devices_blocked(SERVER).is_some());
    }

    // One genuine signature cannot satisfy the requested two witnesses, even
    // when the configured set itself is incomplete.
    let settings = KeyTransparencySettings {
        required: Some(2),
        ..auditor.settings()
    };
    let outcome = connect(&mut alice, &dir, &head, Some(attached.clone()), &settings);
    assert!(
        matches!(witness(&outcome), WitnessStatus::Unavailable { .. }),
        "{outcome:?}"
    );
    assert!(alice.worker.new_devices_blocked(SERVER).is_some());

    // Default "all" includes an invalid enabled extra rather than silently
    // accepting the signature of the one key that happened to parse.
    let mut settings = auditor.settings();
    settings.witnesses.push(WitnessSetting {
        key: "invalid extra witness".into(),
        enabled: true,
    });
    let outcome = connect(&mut alice, &dir, &head, Some(attached.clone()), &settings);
    assert!(
        matches!(witness(&outcome), WitnessStatus::Unavailable { .. }),
        "{outcome:?}"
    );

    // Explicitly turning the requirement off is still the user's choice.
    settings.required = Some(0);
    let outcome = connect(&mut alice, &dir, &head, Some(attached), &settings);
    assert_eq!(witness(&outcome), &WitnessStatus::Off);
}

#[tokio::test]
async fn a_witnessed_server_is_fresh_by_the_witness_clock_not_its_own() {
    let dir = directory([0x11; 32]).await;
    let mut auditor = Auditor::new();
    let mut alice = Client::new();
    let bob = Person::new(&dir, 2);
    bob.publish(&dir, T0 + 1).await;
    // The server's head is two days old by its own clock (a server that stopped
    // re-signing, or whose clock is off), but the auditor cosigned it just now.
    let now = T0 + 48 * HOUR;
    alice.at(now);
    let note = auditor.cosign(&dir, now).await;
    let head = dir.latest_head().await.unwrap();
    let attached = attach(&dir, &note, head.epoch).await;
    let outcome = connect(&mut alice, &dir, &head, Some(attached), &auditor.settings());
    assert!(
        matches!(witness(&outcome), WitnessStatus::Witnessed { .. }),
        "{outcome:?}"
    );
    assert_eq!(alice.worker.new_devices_blocked(SERVER), None);
    // A contact on it is checked as usual, and not marked unavailable.
    let attached = attach(&dir, &note, head.epoch).await;
    let view = lookup(
        &mut alice,
        &dir,
        &bob,
        Some(attached),
        (None, vec![], None, vec![]),
    )
    .await;
    assert_eq!(view.state, ContactState::Unverified);
}

#[tokio::test]
async fn an_unreachable_witness_makes_verification_unavailable_but_first_contact_proceeds() {
    let dir = directory([0x11; 32]).await;
    let auditor = Auditor::new();
    let mut alice = Client::new();
    let bob = Person::new(&dir, 2);
    bob.publish(&dir, T0 + 1).await;
    let head = dir.latest_head().await.unwrap();
    // The server could not reach the auditor: its checkpoint has no cosignature.
    let note = unwitnessed(&dir).await;
    let attached = attach(&dir, &note, head.epoch).await;
    let outcome = connect(
        &mut alice,
        &dir,
        &head,
        Some(attached.clone()),
        &auditor.settings(),
    );
    assert!(
        matches!(witness(&outcome), WitnessStatus::Unavailable { .. }),
        "{outcome:?}"
    );
    // First contact proceeds: Bob is pinned, shown as "Automatic verification
    // unavailable", and his new devices are not added to existing conversations.
    let view = lookup(
        &mut alice,
        &dir,
        &bob,
        Some(attached),
        (None, vec![], None, vec![]),
    )
    .await;
    assert!(
        matches!(
            view.state,
            ContactState::Unavailable {
                verified: false,
                ..
            }
        ),
        "{view:?}"
    );
    assert!(view.identity.is_some(), "first contact pins the identity");
    assert!(alice.worker.new_devices_blocked(SERVER).is_some());
    // A server without kt-witness/1 at all is unavailable too.
    let outcome = connect(&mut alice, &dir, &head, None, &auditor.settings());
    assert!(matches!(
        witness(&outcome),
        WitnessStatus::Unavailable { .. }
    ));
    // Turning the setting off restores the old behaviour.
    let lenient = KeyTransparencySettings {
        block_new_devices: false,
        ..auditor.settings()
    };
    connect(&mut alice, &dir, &head, None, &lenient);
    assert_eq!(alice.worker.new_devices_blocked(SERVER), None);
}

#[tokio::test]
async fn a_fork_cosigned_by_nobody_is_never_witnessed() {
    let honest = directory([0x11; 32]).await;
    let fork = directory([0x11; 32]).await;
    let mut auditor = Auditor::new();
    let alice = Person::new(&honest, 1);
    alice.publish(&honest, T0 + 1).await;
    // The fork binds Alice's account to the operator's own identity.
    let impostor = Person::new(&fork, 1);
    impostor.publish(&fork, T0 + 1).await;
    let note = auditor.cosign(&honest, T0 + 1000).await;
    let mut bob = Client::new();
    let head = fork.latest_head().await.unwrap();
    // The fork's own checkpoint carries no cosignature: unavailable.
    let own = attach(&fork, &unwitnessed(&fork).await, head.epoch).await;
    let outcome = connect(&mut bob, &fork, &head, Some(own), &auditor.settings());
    assert!(matches!(
        witness(&outcome),
        WitnessStatus::Unavailable { .. }
    ));
    // The honest, cosigned checkpoint does not contain the fork's head: refused.
    let mut borrowed = WitnessedHead::decode(&attach(&honest, &note, head.epoch).await).unwrap();
    borrowed.inclusion = fork.inclusion(head.epoch, head.epoch + 1).await.unwrap();
    let mut bob = Client::new();
    let outcome = connect(
        &mut bob,
        &fork,
        &head,
        Some(borrowed.encode().unwrap()),
        &auditor.settings(),
    );
    assert!(matches!(outcome, HeadOutcome::Failed { .. }), "{outcome:?}");
    // The auditor itself refuses to cosign the fork.
    let request = fork
        .add_epochs(ORIGIN, auditor.state.as_ref().unwrap().size(), 4096)
        .await
        .unwrap();
    assert!(add_epochs(auditor.state.as_ref(), &request).is_err());
}

#[tokio::test]
async fn stale_cosignatures_and_re_signed_old_heads_are_not_witnessed() {
    let dir = directory([0x11; 32]).await;
    let mut auditor = Auditor::new();
    let mut alice = Client::new();
    Person::new(&dir, 1).publish(&dir, T0 + 1).await;
    let at_one = auditor.cosign(&dir, T0 + 1000).await;
    let frozen = dir.latest_head().await.unwrap();
    let attached = attach(&dir, &at_one, frozen.epoch).await;
    // A day later the cosignature is too old (the server stopped submitting).
    alice.at(T0 + 25 * HOUR);
    let fresh = dir.refresh_head(T0 + 25 * HOUR).await.unwrap();
    let outcome = connect(
        &mut alice,
        &dir,
        &fresh,
        Some(attached),
        &auditor.settings(),
    );
    assert!(
        matches!(witness(&outcome), WitnessStatus::Unavailable { reason } if reason.contains("hours old")),
        "{outcome:?}"
    );

    // The server moves on, the auditor with it, but Carol is frozen at epoch 1
    // with its head signed again at a fresh time.
    Person::new(&dir, 2).publish(&dir, T0 + 26 * HOUR).await;
    let now = T0 + 27 * HOUR;
    let at_two = auditor.cosign(&dir, now).await;
    let resigned = SignedHead::sign(
        &nuschl_kt::Ed25519HeadSigner::from_seed(&[0x11; 32]),
        frozen.log_id,
        frozen.epoch,
        frozen.root,
        now,
    )
    .unwrap();
    let mut carol = Client::new();
    carol.at(now);
    // With the newer checkpoint, the head is not its newest leaf: refused.
    let attached = attach(&dir, &at_two, resigned.epoch).await;
    let outcome = connect(
        &mut carol,
        &dir,
        &resigned,
        Some(attached),
        &auditor.settings(),
    );
    assert!(
        matches!(&outcome, HeadOutcome::Failed { reason } if reason.contains("newest")),
        "{outcome:?}"
    );
    // With the old checkpoint, its cosignature is a day old: never witnessed,
    // whatever time the server signed the head at.
    let attached = attach(&dir, &at_one, resigned.epoch).await;
    let outcome = connect(
        &mut carol,
        &dir,
        &resigned,
        Some(attached),
        &auditor.settings(),
    );
    assert!(matches!(
        witness(&outcome),
        WitnessStatus::Unavailable { .. }
    ));
}

#[tokio::test]
async fn a_head_waiting_for_cosignatures_is_pending_for_ten_minutes() {
    let dir = directory([0x11; 32]).await;
    let mut auditor = Auditor::new();
    let mut alice = Client::new();
    let note = auditor.cosign(&dir, T0 + 1000).await;
    Person::new(&dir, 1).publish(&dir, T0 + 2000).await;
    let head = dir.latest_head().await.unwrap();
    let attached = attach(&dir, &note, head.epoch).await;
    alice.at(T0 + 3000);
    let outcome = connect(
        &mut alice,
        &dir,
        &head,
        Some(attached.clone()),
        &auditor.settings(),
    );
    assert_eq!(witness(&outcome), &WitnessStatus::Pending);
    assert!(alice.worker.new_devices_blocked(SERVER).is_some());
    alice.at(T0 + 3000 + 11 * 60 * 1000);
    let outcome = connect(&mut alice, &dir, &head, Some(attached), &auditor.settings());
    assert!(matches!(
        witness(&outcome),
        WitnessStatus::Unavailable { .. }
    ));
    // Once cosigned, it is witnessed again.
    let note = auditor.cosign(&dir, T0 + 12 * 60 * 1000).await;
    let attached = attach(&dir, &note, head.epoch).await;
    let outcome = connect(&mut alice, &dir, &head, Some(attached), &auditor.settings());
    assert!(matches!(witness(&outcome), WitnessStatus::Witnessed { .. }));
}

#[tokio::test]
async fn names_and_numbers_must_name_the_account_looked_up() {
    let dir = directory([0x11; 32]).await;
    let bob = Person::new(&dir, 2);
    let mallory = Person::new(&dir, 9);
    let named = |account: [u8; 16]| NameBinding::Account(AccountId(account)).to_value();
    dir.publish(
        vec![
            (bob.label(), bob.value.clone()),
            (mallory.label(), mallory.value.clone()),
            (Label::Registration(5), named(bob.account)),
            (Label::name("bob").unwrap(), named(bob.account)),
            // Another number of Bob's.
            (Label::Registration(6), named(bob.account)),
        ],
        T0 + 1,
    )
    .await
    .unwrap();
    let mut alice = Client::new();
    let head = dir.latest_head().await.unwrap();
    let note = unwitnessed(&dir).await;
    let attached = attach(&dir, &note, head.epoch).await;
    let off = KeyTransparencySettings::default();
    connect(&mut alice, &dir, &head, Some(attached.clone()), &off);
    let proof = |label: Label| {
        let dir = &dir;
        async move { dir.lookup(&label).await.unwrap().encode().unwrap() }
    };
    // #5, shown as "Bob": both labels name Bob's account.
    let view = lookup(
        &mut alice,
        &dir,
        &bob,
        Some(attached.clone()),
        (
            Some(5),
            proof(Label::Registration(5)).await,
            Some("Bob"),
            proof(Label::name("bob").unwrap()).await,
        ),
    )
    .await;
    assert_eq!(view.state, ContactState::Unverified, "{view:?}");
    // The server answers #6 (which the log gives Bob) with Mallory's account.
    let view = lookup(
        &mut alice,
        &dir,
        &mallory,
        Some(attached.clone()),
        (Some(6), proof(Label::Registration(6)).await, None, vec![]),
    )
    .await;
    assert!(view.identity.is_none(), "{view:?}");
    // Or leaves the proof out: not accepted either.
    let view = lookup(
        &mut alice,
        &dir,
        &mallory,
        Some(attached.clone()),
        (Some(9), vec![], None, vec![]),
    )
    .await;
    assert!(view.identity.is_none(), "{view:?}");
}

/// Connects as the owner of account `[1; 16]`, registration #3 "alice", following
/// the head with an audit if it moved.
async fn connect_owner(alice: &mut Client, dir: &Dir) {
    let head = dir.latest_head().await.unwrap();
    let outcome = alice.ask(CryptoRequest::Connect {
        server: CryptoServer {
            key: SERVER.into(),
            label: "Witnessed".into(),
        },
        descriptor: dir.descriptor().encode(),
        account: vec![1; 16],
        head: head.encode(),
        witnessed: None,
        witnessing: KeyTransparencySettings::default(),
        registration: Some((3, "alice".into())),
    });
    match outcome {
        CryptoResponse::Head(HeadOutcome::Accepted { .. }) => {}
        CryptoResponse::Head(HeadOutcome::NeedAudit { from, to }) => {
            let audit = dir.audit(from, to).await.unwrap();
            let CryptoResponse::Head(HeadOutcome::Accepted { .. }) =
                alice.ask(CryptoRequest::Advance {
                    server: SERVER.into(),
                    head: head.encode(),
                    audit: audit.encode().unwrap(),
                })
            else {
                panic!("the audit failed")
            };
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn owners_monitor_their_number_and_name() {
    let dir = directory([0x11; 32]).await;
    let mut alice = Client::new();
    let mine = NameBinding::Account(AccountId([1; 16])).to_value();
    dir.publish(
        vec![
            (Label::Registration(3), mine.clone()),
            (Label::name("alice").unwrap(), mine.clone()),
        ],
        T0 + 1,
    )
    .await
    .unwrap();
    connect_owner(&mut alice, &dir).await;
    let monitor = |alice: &mut Client, name: Option<&str>, history: Vec<u8>| match alice.ask(
        CryptoRequest::MonitorLabel {
            server: SERVER.into(),
            name: name.map(str::to_owned),
            history,
            witnessed: None,
        },
    ) {
        CryptoResponse::Monitor(outcome) => outcome,
        other => panic!("{other:?}"),
    };
    let history = |label: Label| {
        let dir = &dir;
        async move { dir.history(&label).await.unwrap().encode().unwrap() }
    };
    assert!(matches!(
        monitor(&mut alice, None, history(Label::Registration(3)).await),
        MonitorOutcome::Consistent { current: true, .. }
    ));
    assert!(matches!(
        monitor(
            &mut alice,
            Some("Alice"),
            history(Label::name("alice").unwrap()).await
        ),
        MonitorOutcome::Consistent { current: true, .. }
    ));
    // Someone else's name is not ours to monitor.
    assert!(matches!(
        alice.ask(CryptoRequest::MonitorLabel {
            server: SERVER.into(),
            name: Some("bob".into()),
            history: history(Label::name("alice").unwrap()).await,
            witnessed: None,
        }),
        CryptoResponse::Rejected(_)
    ));
    // The server rebinds our name to another account.
    dir.publish(
        vec![(
            Label::name("alice").unwrap(),
            NameBinding::Account(AccountId([9; 16])).to_value(),
        )],
        T0 + 2,
    )
    .await
    .unwrap();
    connect_owner(&mut alice, &dir).await;
    let outcome = monitor(
        &mut alice,
        Some("alice"),
        history(Label::name("alice").unwrap()).await,
    );
    assert!(
        matches!(&outcome, MonitorOutcome::OwnKeyAlarm { reason } if reason.contains("your name")),
        "{outcome:?}"
    );
    // Our number is still ours.
    assert!(matches!(
        monitor(&mut alice, None, history(Label::Registration(3)).await),
        MonitorOutcome::Consistent { .. }
    ));
}
