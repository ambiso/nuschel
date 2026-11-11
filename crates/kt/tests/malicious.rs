// SPDX-License-Identifier: EUPL-1.2
//! A malicious server. Every attack must be caught by the client functions and mapped
//! to the right error and error class.

mod common;

use common::*;
use nuschel_kt::*;
use std::sync::Arc;

fn signer() -> Ed25519HeadSigner {
    Ed25519HeadSigner::from_seed(&SIGNER_SEED)
}

/// Alice's own value, and the attacker's key for her account.
fn alice_value() -> DirectoryValue {
    binding(1, 1)
}
fn attacker_value() -> DirectoryValue {
    binding(666, 1)
}

#[tokio::test]
async fn forked_heads_for_two_clients_are_equivocation() {
    // The server keeps two trees under the same keys: one for Alice, one for Bob.
    let for_alice = open().await;
    let for_bob = open().await;
    let descriptor = for_alice.descriptor().clone();
    assert_eq!(&descriptor, for_bob.descriptor());
    for dir in [&for_alice, &for_bob] {
        dir.publish(
            vec![(acct(1), alice_value()), (acct(2), binding(2, 1))],
            T0 + 10,
        )
        .await
        .unwrap();
    }
    // Epoch 2: Alice's tree changes Bob's roster; Bob's tree rebinds Alice's account.
    for_alice
        .publish(vec![(acct(2), binding(2, 2))], T0 + 20)
        .await
        .unwrap();
    for_bob
        .publish(vec![(acct(1), attacker_value())], T0 + 20)
        .await
        .unwrap();

    // Each view verifies on its own: Alice's history has no second version...
    let mut alice = Client::new(&descriptor);
    let history = for_alice.history(&acct(1)).await.unwrap();
    let history = verify_history(&descriptor, &acct(1), &history).unwrap();
    alice
        .accept(&for_alice, &history.head.signed().clone(), T0 + 30)
        .await
        .unwrap();
    let report = monitor_own(&history, None, |v| *v == alice_value()).unwrap();
    assert_eq!(report.checkpoint.version, 1);
    // ...while Bob is shown the attacker's key for Alice.
    let mut bob = Client::new(&descriptor);
    let lookup = for_bob.lookup(&acct(1)).await.unwrap();
    let (bob_head, entry) = verify_lookup(&descriptor, &acct(1), &lookup).unwrap();
    assert_eq!(entry.value, attacker_value());
    bob.accept(&for_bob, bob_head.signed(), T0 + 30)
        .await
        .unwrap();

    // Bob's head reaches Alice inside an MLS message.
    let gossip = HeadGossip::decode(
        &HeadGossip {
            head: bob_head.signed().clone(),
        }
        .encode(),
    )
    .unwrap();
    let error = check_gossip(&descriptor, &alice.known, &gossip).unwrap_err();
    assert!(matches!(error, KtError::Equivocation(_)), "{error:?}");
    assert_eq!(error.class(), ErrorClass::ServerMisbehaviour);
    // The evidence convinces anyone holding the descriptor, and survives encoding.
    let evidence = error.evidence().unwrap();
    let evidence = Evidence::decode(&evidence.encode()).unwrap();
    assert_eq!(evidence.kind, EvidenceKind::Equivocation);
    evidence.verify(&descriptor).unwrap();
    assert_eq!(evidence.encode().len(), 299);
    // The TOFU layer raises the alarm.
    let mut trust = ContactTrust::default();
    let assessment = trust.assess(Observation::Failed(error), T0);
    assert_eq!(
        assessment.surface(ConversationKind::Channel),
        Surface::ServerMisbehaviourAlarm
    );
}

#[tokio::test]
async fn a_fork_at_a_later_epoch_fails_the_audit() {
    let for_alice = open().await;
    let for_bob = open().await;
    let descriptor = for_alice.descriptor().clone();
    for dir in [&for_alice, &for_bob] {
        dir.publish(vec![(acct(1), alice_value())], T0 + 10)
            .await
            .unwrap();
    }
    for_alice
        .publish(vec![(acct(2), binding(2, 1))], T0 + 20)
        .await
        .unwrap();
    for_bob
        .publish(vec![(acct(3), binding(3, 1))], T0 + 20)
        .await
        .unwrap();
    for_bob
        .publish(vec![(acct(1), attacker_value())], T0 + 30)
        .await
        .unwrap();

    let mut alice = Client::new(&descriptor);
    alice
        .accept(&for_alice, &for_alice.latest_head().await.unwrap(), T0 + 40)
        .await
        .unwrap();
    // Bob gossips epoch 3; Alice has epoch 2 from another tree, so heads do not clash.
    let gossip = HeadGossip {
        head: for_bob.latest_head().await.unwrap(),
    };
    let (head, outcome) = check_gossip(&descriptor, &alice.known, &gossip).unwrap();
    let GossipOutcome::Newer { since: Some(since) } = outcome else {
        panic!("{outcome:?}");
    };
    // Neither tree can connect Alice's epoch 2 to Bob's epoch 3.
    let from_bob_tree = for_bob.audit(2, 3).await.unwrap();
    let error = verify_audit(&descriptor, &since, &head, &from_bob_tree).unwrap_err();
    assert!(
        matches!(error, KtError::AuditFailed { from: 2, to: 3, .. }),
        "{error:?}"
    );
    assert_eq!(error.class(), ErrorClass::ServerMisbehaviour);
    for_alice
        .publish(vec![(acct(4), binding(4, 1))], T0 + 30)
        .await
        .unwrap();
    let from_alice_tree = for_alice.audit(2, 3).await.unwrap();
    assert!(matches!(
        verify_audit(&descriptor, &since, &head, &from_alice_tree),
        Err(KtError::AuditFailed { .. })
    ));
    // Replacing the end root with Bob's does not help either.
    let mut spliced = from_alice_tree.clone();
    spliced.roots = vec![*head.root()];
    assert!(matches!(
        verify_audit(&descriptor, &since, &head, &spliced),
        Err(KtError::AuditFailed { .. })
    ));
}

#[tokio::test]
async fn a_changed_key_without_a_history_entry_is_caught() {
    let dir = open().await;
    let descriptor = dir.descriptor().clone();
    let mine = |v: &DirectoryValue| *v == alice_value();
    dir.publish(vec![(acct(1), alice_value())], T0 + 10)
        .await
        .unwrap();
    let before =
        verify_history(&descriptor, &acct(1), &dir.history(&acct(1)).await.unwrap()).unwrap();
    let checkpoint = monitor_own(&before, None, mine).unwrap().checkpoint;
    let stale_response = dir.history(&acct(1)).await.unwrap();

    // The server binds the attacker's key to Alice's account and shows it to Bob.
    dir.publish(vec![(acct(1), attacker_value())], T0 + 20)
        .await
        .unwrap();
    let (_, seen_by_bob) =
        verify_lookup(&descriptor, &acct(1), &dir.lookup(&acct(1)).await.unwrap()).unwrap();
    assert_eq!(seen_by_bob.value, attacker_value());

    // Shown honestly, Alice's history carries the attacker's version: the own-key alarm.
    let honest = dir.history(&acct(1)).await.unwrap();
    let history = verify_history(&descriptor, &acct(1), &honest).unwrap();
    let error = monitor_own(&history, Some(&checkpoint), mine).unwrap_err();
    assert_eq!(
        error,
        KtError::UnexpectedOwnValue {
            version: 2,
            epoch: 2
        }
    );
    assert_eq!(error.class(), ErrorClass::OwnKeyAlarm);
    assert_eq!(
        Assessment::OwnKeyAlarm(error).surface(ConversationKind::Direct),
        Surface::OwnKeyAlarm
    );

    // Hiding the entry from the proof fails verification.
    let mut hidden = honest.clone();
    hidden.proof = nuschel_kt::__private::drop_newest_history_version(&honest.proof).unwrap();
    let error = verify_history(&descriptor, &acct(1), &hidden).unwrap_err();
    assert!(
        matches!(
            error,
            KtError::ProofInvalid {
                proof: ProofKind::History,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(error.class(), ErrorClass::Failure);

    // Replaying the old history (from before the change) is a rollback once Alice has
    // seen a newer head, and stale once the old head ages past 25 hours.
    let mut alice = Client::new(&descriptor);
    alice
        .accept(&dir, &history.head.signed().clone(), T0 + 30)
        .await
        .unwrap();
    let replayed = verify_history(&descriptor, &acct(1), &stale_response).unwrap();
    assert_eq!(
        alice
            .known
            .classify_from_server(&replayed.head)
            .unwrap_err(),
        KtError::Rollback { latest: 2, got: 1 }
    );
    let mut fresh_client = Client::new(&descriptor);
    let error = fresh_client
        .accept(&dir, &stale_response.head, T0 + 26 * 3_600_000)
        .await
        .unwrap_err();
    assert!(matches!(error, KtError::StaleHead { .. }), "{error:?}");

    // A binding that came and went between lookups shows in a contact's history.
    dir.publish(vec![(acct(1), alice_value())], T0 + 40)
        .await
        .unwrap();
    let mut trust = ContactTrust::default();
    trust.assess(
        Observation::Lookup {
            root: root(1).fingerprint(),
            version: 1,
        },
        T0,
    );
    let history = dir.history(&acct(1)).await.unwrap();
    let observation = Observation::from_history(
        verify_history(&descriptor, &acct(1), &history),
        trust.checked_version,
    );
    let assessment = trust.assess(observation, T0 + 50);
    assert_eq!(
        assessment,
        Assessment::TransientBinding {
            seen: vec![root(666).fingerprint()]
        }
    );
    assert_eq!(
        assessment.surface(ConversationKind::Direct),
        Surface::KeyChangeNotice
    );
}

#[tokio::test]
async fn a_rolled_back_epoch_is_caught() {
    let dir = open().await;
    let descriptor = dir.descriptor().clone();
    let mut client = Client::new(&descriptor);
    let mut old = None;
    for n in 1..=5u32 {
        let head = dir
            .publish(vec![(acct(n), binding(n, 1))], T0 + u64::from(n) * 1000)
            .await
            .unwrap();
        client.accept(&dir, &head, T0 + 10_000).await.unwrap();
        if n == 3 {
            old = Some(head);
        }
    }
    let old = old.unwrap();
    // The old head as it was signed: a replay.
    let error = client.accept(&dir, &old, T0 + 10_000).await.unwrap_err();
    assert_eq!(error, KtError::Rollback { latest: 5, got: 3 });
    assert_eq!(error.class(), ErrorClass::Failure);
    let a = verify_head(&descriptor, client.known.latest().unwrap().signed()).unwrap();
    let b = verify_head(&descriptor, &old).unwrap();
    assert_eq!(
        check_successor(&a, &b).unwrap_err(),
        KtError::Rollback { latest: 5, got: 3 }
    );

    // The old epoch re-signed now, to look fresh: signed proof of a stale head.
    let resigned =
        SignedHead::sign(&signer(), old.log_id, old.epoch, old.root, T0 + 9_000).unwrap();
    let error = client
        .accept(&dir, &resigned, T0 + 10_000)
        .await
        .unwrap_err();
    assert!(matches!(error, KtError::StaleSigning(_)), "{error:?}");
    assert_eq!(error.class(), ErrorClass::ServerMisbehaviour);
    let evidence = error.evidence().unwrap();
    evidence.verify(&descriptor).unwrap();
    // Epoch 3 signed after a newer epoch (the first newer head the client holds).
    assert_eq!(evidence.first.epoch, 3);
    assert!(
        evidence.second.epoch > 3 && evidence.second.issued_at_ms < evidence.first.issued_at_ms
    );
    // The same holds for a client that has not seen epoch 5 yet but receives both
    // heads by gossip.
    let mut other = Client::new(&descriptor);
    other.accept(&dir, &resigned, T0 + 10_000).await.unwrap();
    let error = check_gossip(
        &descriptor,
        &other.known,
        &HeadGossip {
            head: dir.latest_head().await.unwrap(),
        },
    )
    .unwrap_err();
    assert!(matches!(error, KtError::StaleSigning(_)), "{error:?}");
}

#[tokio::test]
async fn a_wrong_head_signature_is_caught() {
    let dir = open().await;
    let descriptor = dir.descriptor().clone();
    dir.publish(vec![(acct(1), alice_value())], T0 + 10)
        .await
        .unwrap();
    let head = dir.latest_head().await.unwrap();

    // Signed by another key under this log's id.
    let impostor = Ed25519HeadSigner::from_seed(&[0x99; 32]);
    let forged = SignedHead::sign(
        &impostor,
        head.log_id,
        head.epoch,
        head.root,
        head.issued_at_ms,
    )
    .unwrap();
    assert_eq!(
        verify_head(&descriptor, &forged).unwrap_err(),
        KtError::BadSignature
    );
    // A changed root, epoch or time under the original signature.
    let tampers: [fn(&mut SignedHead); 4] = [
        |h: &mut SignedHead| h.root[0] ^= 1,
        |h: &mut SignedHead| h.epoch += 1,
        |h: &mut SignedHead| h.issued_at_ms += 1,
        |h: &mut SignedHead| h.signature[0] ^= 1,
    ];
    for tamper in tampers {
        let mut bad = head.clone();
        tamper(&mut bad);
        let error = verify_head(&descriptor, &bad).unwrap_err();
        assert_eq!(error, KtError::BadSignature);
        assert_eq!(error.class(), ErrorClass::Failure);
    }
    // A lookup carrying a forged head.
    let mut lookup = dir.lookup(&acct(1)).await.unwrap();
    lookup.head = forged;
    assert_eq!(
        verify_lookup(&descriptor, &acct(1), &lookup).unwrap_err(),
        KtError::BadSignature
    );
    // A head of another log (other signing key, so another log id).
    let other = open_with(Arc::new(MemoryStore::new()), [0x99; 32], VRF_SEED).await;
    let error = verify_head(&descriptor, &other.latest_head().await.unwrap()).unwrap_err();
    assert!(matches!(error, KtError::WrongLog { .. }), "{error:?}");
    // Two failures in a row: automatic verification becomes unavailable, a manual
    // verification stays.
    let mut trust = ContactTrust {
        pinned: Some(root(1).fingerprint()),
        manually_verified: true,
        ..Default::default()
    };
    let first = trust.assess(Observation::Failed(KtError::BadSignature), T0);
    assert_eq!(first.surface(ConversationKind::Direct), Surface::Nothing);
    let second = trust.assess(Observation::Failed(KtError::BadSignature), T0 + 3_600_000);
    assert_eq!(
        second.surface(ConversationKind::Direct),
        Surface::AutomaticVerificationUnavailable {
            reason: KtError::BadSignature.to_string(),
            still_manually_verified: true,
        }
    );
    assert_eq!(trust.pinned, Some(root(1).fingerprint()));
}

#[tokio::test]
async fn a_proof_for_the_wrong_account_is_caught() {
    let dir = open().await;
    let descriptor = dir.descriptor().clone();
    dir.publish(
        vec![(acct(1), alice_value()), (acct(2), attacker_value())],
        T0 + 10,
    )
    .await
    .unwrap();
    // Asked for Alice, the server answers with the attacker's entry.
    let mut lookup = dir.lookup(&acct(2)).await.unwrap();
    let error = verify_lookup(&descriptor, &acct(1), &lookup).unwrap_err();
    assert_eq!(error, KtError::LabelMismatch);
    assert_eq!(error.class(), ErrorClass::Failure);
    // ...and with the label field rewritten: the VRF proof does not match Alice's label.
    lookup.label = acct(1);
    let error = verify_lookup(&descriptor, &acct(1), &lookup).unwrap_err();
    assert!(
        matches!(
            error,
            KtError::ProofInvalid {
                proof: ProofKind::Lookup,
                ..
            }
        ),
        "{error:?}"
    );
    let mut history = dir.history(&acct(2)).await.unwrap();
    assert_eq!(
        verify_history(&descriptor, &acct(1), &history).unwrap_err(),
        KtError::LabelMismatch
    );
    history.label = acct(1);
    assert!(matches!(
        verify_history(&descriptor, &acct(1), &history),
        Err(KtError::ProofInvalid {
            proof: ProofKind::History,
            ..
        })
    ));
    // A proof made under another VRF key (the server swapped the VRF key).
    let other_vrf = open_with(Arc::new(MemoryStore::new()), SIGNER_SEED, [0x77; 32]).await;
    other_vrf
        .publish(vec![(acct(1), attacker_value())], T0 + 10)
        .await
        .unwrap();
    let mut swapped = other_vrf.lookup(&acct(1)).await.unwrap();
    swapped.head = dir.latest_head().await.unwrap();
    assert!(matches!(
        verify_lookup(&descriptor, &acct(1), &swapped),
        Err(KtError::ProofInvalid { .. })
    ));
}

/// Deterministic xorshift for the mutation test.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[tokio::test]
async fn every_tampered_response_is_rejected_without_panicking() {
    let dir = open().await;
    let descriptor = dir.descriptor().clone();
    for n in 2..=13u32 {
        dir.publish(
            vec![(acct(n), binding(n, 1)), (acct(1), binding(1, n as u8))],
            T0 + u64::from(n),
        )
        .await
        .unwrap();
    }
    let lookup = dir.lookup(&acct(5)).await.unwrap().encode().unwrap();
    let history = dir.history(&acct(1)).await.unwrap().encode().unwrap();
    let audit = dir.audit(9, 12).await.unwrap().encode().unwrap();
    let from = verify_head(&descriptor, &dir.head_at(9).await.unwrap()).unwrap();
    let to = verify_head(&descriptor, &dir.head_at(12).await.unwrap()).unwrap();

    let lookup_ok = |b: &[u8]| {
        LookupResponse::decode(b)
            .map_err(KtError::from)
            .and_then(|r| verify_lookup(&descriptor, &acct(5), &r).map(|_| ()))
    };
    let history_ok = |b: &[u8]| {
        HistoryResponse::decode(b)
            .map_err(KtError::from)
            .and_then(|r| verify_history(&descriptor, &acct(1), &r).map(|_| ()))
    };
    let audit_ok = |b: &[u8]| {
        AuditResponse::decode(b)
            .map_err(KtError::from)
            .and_then(|r| verify_audit(&descriptor, &from, &to, &r))
    };
    lookup_ok(&lookup).unwrap();
    history_ok(&history).unwrap();
    audit_ok(&audit).unwrap();

    let mut rng = Rng(0x1234_5678_9abc_def1);
    type Check<'a> = &'a dyn Fn(&[u8]) -> Result<(), KtError>;
    let cases: [(&str, &[u8], Check, usize); 3] = [
        ("lookup", &lookup, &lookup_ok, 600),
        ("history", &history, &history_ok, 300),
        ("audit", &audit, &audit_ok, 150),
    ];
    for (name, original, check, rounds) in cases {
        for round in 0..rounds {
            let mut bytes = original.to_vec();
            match round % 4 {
                // Flip one bit anywhere.
                0 | 1 => {
                    let i = rng.next() as usize % bytes.len();
                    bytes[i] ^= 1 << (rng.next() % 8);
                }
                // Truncate.
                2 => bytes.truncate(rng.next() as usize % bytes.len()),
                // Append garbage.
                _ => bytes.push(rng.next() as u8),
            }
            assert!(
                check(&bytes).is_err(),
                "{name}: mutation {round} was accepted"
            );
        }
    }
}

#[tokio::test]
async fn unknown_versions_types_and_trailing_bytes_are_refused() {
    let dir = open().await;
    let descriptor = dir.descriptor().clone();
    dir.publish(vec![(acct(1), alice_value())], T0 + 1)
        .await
        .unwrap();
    let head = dir.latest_head().await.unwrap().encode();
    let mut v2 = head.clone();
    v2[0] = 2;
    assert_eq!(
        SignedHead::decode(&v2).unwrap_err(),
        DecodeError::UnsupportedVersion(2)
    );
    assert!(matches!(
        HeadGossip::decode(&head).unwrap_err(),
        DecodeError::UnexpectedType {
            expected: 6,
            got: 2
        }
    ));
    let mut long = head.clone();
    long.push(0);
    assert!(SignedHead::decode(&long).is_err());
    let mut d = descriptor.encode();
    d[2] = 9; // unknown suite
    assert!(LogDescriptor::decode(&d).is_err());
    // A head epoch beyond 2^48 is refused by the parser.
    let mut far = dir.latest_head().await.unwrap();
    far.epoch = MAX_EPOCH + 1;
    assert!(SignedHead::decode(&far.encode()).is_err());
}
