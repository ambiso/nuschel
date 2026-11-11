// SPDX-License-Identifier: EUPL-1.2
//! An honest server: publish, look up, monitor and audit across many epochs.

mod common;

use common::*;
use nuschel_identity::{DeviceKeys, DisplayName, NewIdentity, Validity};
use nuschel_kt::*;
use std::collections::BTreeMap;
use std::sync::Arc;

const ACCOUNTS: u32 = 30;
const EPOCHS: u64 = 40;

/// Deterministic pseudo-random numbers for the schedule.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[tokio::test]
async fn publish_look_up_monitor_and_audit_across_many_epochs() {
    let dir = open().await;
    let descriptor = dir.descriptor().clone();
    let mut client = Client::new(&descriptor);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);

    // Model: account → (root, roster) per version.
    let mut model: BTreeMap<u32, Vec<(u32, u8, u64)>> = BTreeMap::new();
    // Self-monitoring of account 0, contact view of account 1 from another user.
    let mut own_values = vec![];
    let mut checkpoint: Option<MonitorCheckpoint> = None;
    let mut contact = ContactTrust::default();
    let mut heads = vec![
        client
            .accept(&dir, &dir.latest_head().await.unwrap(), T0)
            .await
            .unwrap(),
    ];

    for epoch in 1..=EPOCHS {
        let now = T0 + epoch * 10_000;
        let mut updates = BTreeMap::new();
        // Account 0 changes its roster every 5 epochs (a device added: same root).
        if epoch % 5 == 1 {
            updates.insert(0, (0, epoch as u8));
        }
        // Account 1 rotates its root once, at epoch 20.
        if epoch == 1 || epoch == 20 {
            updates.insert(1, (if epoch == 1 { 1 } else { 1001 }, 1));
        }
        for _ in 0..rng.below(4) + 1 {
            let n = 2 + rng.below(u64::from(ACCOUNTS) - 2) as u32;
            updates.insert(n, (n, rng.below(250) as u8));
        }
        let batch: Vec<_> = updates
            .iter()
            .map(|(&n, &(r, s))| (acct(n), binding(r, s)))
            .collect();
        if updates.contains_key(&0) {
            own_values.push(binding(updates[&0].0, updates[&0].1));
        }
        let head = dir.publish(batch, now).await.unwrap();
        // A value equal to the current one is skipped by the directory.
        for (n, (r, s)) in updates {
            let versions = model.entry(n).or_default();
            if versions.last().map(|v| (v.0, v.1)) != Some((r, s)) {
                versions.push((r, s, head.epoch));
            }
        }
        assert_eq!(head.epoch, epoch, "every batch changes something");
        heads.push(client.accept(&dir, &head, now).await.unwrap());

        // Look up a few accounts and compare with the model.
        for _ in 0..3 {
            let n = rng.below(u64::from(ACCOUNTS)) as u32;
            match model.get(&n) {
                Some(versions) => {
                    let response = dir.lookup(&acct(n)).await.unwrap();
                    let (at, entry) = verify_lookup(&descriptor, &acct(n), &response).unwrap();
                    assert_eq!(at.epoch(), epoch);
                    let (r, s, published) = *versions.last().unwrap();
                    assert_eq!(entry.value, binding(r, s));
                    assert_eq!(entry.version, versions.len() as u64);
                    assert_eq!(entry.epoch, published);
                }
                None => assert!(matches!(
                    dir.lookup(&acct(n)).await,
                    Err(DirectoryError::NotFound)
                )),
            }
        }

        // Self-monitoring of account 0.
        let history = dir.history(&acct(0)).await.unwrap();
        let history = verify_history(&descriptor, &acct(0), &history).unwrap();
        let report =
            monitor_own(&history, checkpoint.as_ref(), |v| own_values.contains(v)).unwrap();
        assert_eq!(report.checkpoint.version, model[&0].len() as u64);
        checkpoint = Some(report.checkpoint);

        // Contact monitoring of account 1.
        let history = dir.history(&acct(1)).await.unwrap();
        let observation = Observation::from_history(
            verify_history(&descriptor, &acct(1), &history),
            contact.checked_version,
        );
        let assessment = contact.assess(observation, now);
        match epoch {
            1 => assert!(matches!(assessment, Assessment::FirstContact { .. })),
            20 => {
                assert!(matches!(
                    assessment,
                    Assessment::KeyChanged {
                        was_verified: false,
                        ..
                    }
                ));
                assert_eq!(
                    assessment.surface(ConversationKind::Direct),
                    Surface::KeyChangeNotice
                );
            }
            _ => assert_eq!(
                assessment.surface(ConversationKind::Direct),
                Surface::VerifiedAutomatically
            ),
        }
    }

    // Complete histories match the model.
    for (&n, versions) in &model {
        let history = dir.history(&acct(n)).await.unwrap();
        let history = verify_history(&descriptor, &acct(n), &history).unwrap();
        let got: Vec<_> = history
            .entries
            .iter()
            .rev()
            .map(|e| (e.value.clone(), e.epoch))
            .collect();
        let want: Vec<_> = versions
            .iter()
            .map(|&(r, s, e)| (binding(r, s), e))
            .collect();
        assert_eq!(got, want, "history of account {n}");
    }

    // Audits between distant heads, and in steps through signed intermediate heads.
    let first = &heads[1];
    let last = heads.last().unwrap();
    let audit = dir.audit(first.epoch(), last.epoch()).await.unwrap();
    verify_audit(&descriptor, first, last, &audit).unwrap();
    let middle = verify_head(&descriptor, &dir.head_at(17).await.unwrap()).unwrap();
    verify_audit(
        &descriptor,
        first,
        &middle,
        &dir.audit(1, 17).await.unwrap(),
    )
    .unwrap();
    verify_audit(
        &descriptor,
        &middle,
        last,
        &dir.audit(17, EPOCHS).await.unwrap(),
    )
    .unwrap();
    // From the empty directory at epoch 0.
    verify_audit(
        &descriptor,
        &heads[0],
        &heads[3],
        &dir.audit(0, 3).await.unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn refreshed_heads_keep_the_epoch_and_never_go_back_in_time() {
    let dir = open().await;
    let descriptor = dir.descriptor().clone();
    let first = dir
        .publish(vec![(acct(1), binding(1, 1))], T0 + 1000)
        .await
        .unwrap();
    let refreshed = dir.refresh_head(T0 + 3_600_000).await.unwrap();
    assert_eq!((refreshed.epoch, refreshed.root), (first.epoch, first.root));
    assert!(refreshed.issued_at_ms > first.issued_at_ms);
    // A clock that went back neither re-signs nor moves time backwards.
    assert_eq!(dir.refresh_head(T0).await.unwrap(), refreshed);
    let next = dir
        .publish(vec![(acct(2), binding(2, 1))], T0 + 5)
        .await
        .unwrap();
    assert!(next.issued_at_ms >= refreshed.issued_at_ms);
    // Publishing an unchanged value creates no epoch.
    assert_eq!(
        dir.publish(vec![(acct(2), binding(2, 1))], T0 + 9_000_000)
            .await
            .unwrap(),
        next
    );

    let a = verify_head(&descriptor, &first).unwrap();
    let b = verify_head(&descriptor, &refreshed).unwrap();
    let c = verify_head(&descriptor, &next).unwrap();
    assert_eq!(check_successor(&a, &b).unwrap(), Succession::Same);
    assert_eq!(check_successor(&b, &c).unwrap(), Succession::Advanced);
    // The stored head of the epoch is the refreshed one.
    assert_eq!(dir.head_at(1).await.unwrap(), refreshed);
    // Freshness follows refreshing.
    assert!(check_freshness(&a, T0 + 26 * 3_600_000, Default::default()).is_err());
    assert!(check_freshness(&b, T0 + 26 * 3_600_000, Default::default()).is_ok());
}

#[tokio::test]
async fn reopening_a_store_keeps_the_log_and_refuses_other_keys() {
    let store = Arc::new(MemoryStore::new());
    let dir = open_with(store.clone(), SIGNER_SEED, VRF_SEED).await;
    let head = dir
        .publish(vec![(acct(1), binding(1, 1))], T0 + 1)
        .await
        .unwrap();
    drop(dir);
    let dir = open_with(store.clone(), SIGNER_SEED, VRF_SEED).await;
    assert_eq!(dir.latest_head().await.unwrap(), head);
    let response = dir.lookup(&acct(1)).await.unwrap();
    verify_lookup(dir.descriptor(), &acct(1), &response).unwrap();
    for (signer, vrf) in [([9; 32], VRF_SEED), (SIGNER_SEED, [9; 32])] {
        let reopened = AkdDirectory::open(
            store.clone(),
            VrfSecret::from_bytes(vrf),
            Box::new(Ed25519HeadSigner::from_seed(&signer)),
            T0,
        )
        .await;
        assert!(matches!(reopened, Err(DirectoryError::KeyMismatch)));
    }
}

#[tokio::test]
async fn gossip_between_consistent_members() {
    let dir = open().await;
    let descriptor = dir.descriptor().clone();
    let mut alice = Client::new(&descriptor);
    let mut bob = Client::new(&descriptor);
    let mut now = T0;
    for n in 1..=3 {
        now += 10_000;
        let head = dir
            .publish(vec![(acct(n), binding(n, 1))], now)
            .await
            .unwrap();
        alice.accept(&dir, &head, now).await.unwrap();
        if n == 1 {
            bob.accept(&dir, &head, now).await.unwrap();
        }
    }
    let alice_latest = alice.known.latest().unwrap().clone();
    assert!(should_gossip(&alice_latest, Some(1), None, now));
    assert!(!should_gossip(&alice_latest, Some(3), None, now));
    assert!(!should_gossip(
        &alice_latest,
        Some(1),
        Some(now - 1000),
        now
    ));

    // Bob receives a newer head and audits up to it.
    let message = HeadGossip::decode(
        &HeadGossip {
            head: alice_latest.signed().clone(),
        }
        .encode(),
    )
    .unwrap();
    let (head, outcome) = check_gossip(&descriptor, &bob.known, &message).unwrap();
    let GossipOutcome::Newer { since: Some(since) } = outcome else {
        panic!("expected a newer head, got {outcome:?}");
    };
    verify_audit(
        &descriptor,
        &since,
        &head,
        &dir.audit(since.epoch(), head.epoch()).await.unwrap(),
    )
    .unwrap();
    bob.known.record(head).unwrap();
    // The same head again, and an older one, are consistent.
    assert_eq!(
        check_gossip(&descriptor, &bob.known, &message).unwrap().1,
        GossipOutcome::Known
    );
    let old = HeadGossip {
        head: dir.head_at(2).await.unwrap(),
    };
    assert_eq!(
        check_gossip(&descriptor, &bob.known, &old).unwrap().1,
        GossipOutcome::Older
    );
    // KnownHeads survives persistence.
    let saved: Vec<_> = bob.known.heads().map(|h| h.signed().clone()).collect();
    let restored = KnownHeads::restore(&descriptor, &saved).unwrap();
    assert_eq!(restored.latest(), bob.known.latest());
}

#[tokio::test]
async fn a_real_identity_roster_is_checked_against_the_log() {
    let now_s = T0 / 1000;
    let validity = Validity::starting_at(now_s, 86_400 * 365).unwrap();
    let identity =
        NewIdentity::create(&DisplayName::new("desktop").unwrap(), validity, now_s).unwrap();
    let laptop = DeviceKeys::generate().unwrap();
    let (laptop_cert, _) = identity
        .device_signing
        .certify_device(
            &laptop.public_keys(),
            &DisplayName::new("laptop").unwrap(),
            validity,
        )
        .unwrap();
    let signing_cert = identity.device_signing.certificate().clone();
    let devices = vec![identity.device_certificate.clone(), laptop_cert.clone()];
    let roster = RosterCommitment::compute(&signing_cert, &devices, &[]).unwrap();
    // The commitment does not depend on the order of the certificates.
    let reversed: Vec<_> = devices.iter().rev().cloned().collect();
    assert_eq!(
        RosterCommitment::compute(&signing_cert, &reversed, &[]).unwrap(),
        roster
    );
    let value = AccountBinding::new(identity.root.public_key().clone(), roster).to_value();
    assert_eq!(value.data().len(), 2018);

    let dir = open().await;
    let descriptor = dir.descriptor().clone();
    dir.publish(vec![(acct(7), value)], T0 + 1).await.unwrap();
    let (_, entry) =
        verify_lookup(&descriptor, &acct(7), &dir.lookup(&acct(7)).await.unwrap()).unwrap();
    let binding = AccountBinding::from_value(&entry.value).unwrap();
    assert_eq!(binding.fingerprint(), identity.root.fingerprint());

    let roster = roster_for_binding(&binding, &signing_cert, &devices, &[]).unwrap();
    assert_eq!(roster.active_devices(now_s + 10).len(), 2);
    // The directory API leaving a device out, or adding one, does not match the log.
    assert_eq!(
        roster_for_binding(&binding, &signing_cert, &devices[..1], &[]).unwrap_err(),
        KtError::RosterMismatch
    );
    let revocation = identity
        .device_signing
        .revoke_device(laptop.device_id(), 1, now_s)
        .unwrap();
    assert_eq!(
        roster_for_binding(
            &binding,
            &signing_cert,
            &devices,
            std::slice::from_ref(&revocation)
        )
        .unwrap_err(),
        KtError::RosterMismatch
    );
    // A revocation published with a new roster commitment is accepted and applied.
    let revoked =
        RosterCommitment::compute(&signing_cert, &devices, std::slice::from_ref(&revocation))
            .unwrap();
    let binding2 = AccountBinding::new(identity.root.public_key().clone(), revoked);
    let roster = roster_for_binding(&binding2, &signing_cert, &devices, &[revocation]).unwrap();
    assert_eq!(roster.active_devices(now_s + 10).len(), 1);
    // A certificate under another identity's root is refused although committed.
    let other = NewIdentity::create(&DisplayName::new("x").unwrap(), validity, now_s).unwrap();
    let foreign = vec![other.device_certificate.clone()];
    let commitment = RosterCommitment::compute(&signing_cert, &foreign, &[]).unwrap();
    let binding3 = AccountBinding::new(identity.root.public_key().clone(), commitment);
    assert!(matches!(
        roster_for_binding(&binding3, &signing_cert, &foreign, &[]),
        Err(KtError::InvalidRecord(_))
    ));
    // Unknown value formats are reported as such.
    assert_eq!(
        AccountBinding::from_value(&DirectoryValue::new(9, vec![0]).unwrap()),
        Err(ValueError::UnknownFormat(9))
    );
    let name = NameBinding(account(7));
    assert_eq!(NameBinding::from_value(&name.to_value()).unwrap(), name);
}

#[tokio::test]
async fn an_account_record_needs_the_identitys_statement_for_this_account_and_log() {
    let now_s = T0 / 1000;
    let validity = Validity::starting_at(now_s, 86_400 * 365).unwrap();
    let identity =
        NewIdentity::create(&DisplayName::new("desktop").unwrap(), validity, now_s).unwrap();
    let dir = open().await;
    let descriptor = dir.descriptor().clone();
    let log_id = descriptor.log_id();
    let signing_cert = identity.device_signing.certificate().clone();
    let devices = vec![identity.device_certificate.clone()];
    let roster = RosterCommitment::compute(&signing_cert, &devices, &[]).unwrap();
    let statement = identity
        .device_signing
        .sign_account(log_id.0, account(7).0, now_s)
        .unwrap();
    let binding =
        AccountBinding::with_statement(identity.root.public_key().clone(), roster, &statement);
    let value = binding.to_value();
    assert_eq!(value.format(), FORMAT_ACCOUNT_BINDING_V2);
    assert_eq!(value.data().len(), 2050);
    assert_eq!(AccountBinding::from_value(&value).unwrap(), binding);
    dir.publish(vec![(acct(7), value)], T0 + 1).await.unwrap();
    let (_, entry) =
        verify_lookup(&descriptor, &acct(7), &dir.lookup(&acct(7)).await.unwrap()).unwrap();
    let binding = AccountBinding::from_value(&entry.value).unwrap();
    let record = IdentityRecord {
        device_signing: signing_cert.clone(),
        devices: devices.clone(),
        revocations: Vec::new(),
        statement: statement.clone(),
    };
    let verified = verify_account_record(&binding, &record, log_id, account(7)).unwrap();
    assert_eq!(verified.identity, identity.root.fingerprint());
    assert_eq!(verified.roster.active_devices(now_s + 1).len(), 1);

    // The record for another account, or for another log, is refused: the server cannot
    // present this identity as one of its other accounts.
    assert!(matches!(
        verify_account_record(&binding, &record, log_id, account(8)),
        Err(KtError::InvalidRecord(_))
    ));
    let elsewhere = identity
        .device_signing
        .sign_account([0x77; 32], account(7).0, now_s)
        .unwrap();
    let foreign =
        AccountBinding::with_statement(identity.root.public_key().clone(), roster, &elsewhere);
    let mut copied = record.clone();
    copied.statement = elsewhere;
    assert!(matches!(
        verify_account_record(&foreign, &copied, log_id, account(7)),
        Err(KtError::InvalidRecord(_))
    ));
    // A statement other than the committed one does not match the log.
    let resigned = identity
        .device_signing
        .sign_account(log_id.0, account(7).0, now_s + 1)
        .unwrap();
    let mut swapped = record.clone();
    swapped.statement = resigned;
    assert_eq!(
        verify_account_record(&binding, &swapped, log_id, account(7)).unwrap_err(),
        KtError::RosterMismatch
    );
    // A statement signed by another identity under this root's binding fails.
    let other = NewIdentity::create(&DisplayName::new("x").unwrap(), validity, now_s).unwrap();
    let theirs = other
        .device_signing
        .sign_account(log_id.0, account(7).0, now_s)
        .unwrap();
    let forged =
        AccountBinding::with_statement(identity.root.public_key().clone(), roster, &theirs);
    let mut forged_record = record.clone();
    forged_record.statement = theirs;
    assert!(matches!(
        verify_account_record(&forged, &forged_record, log_id, account(7)),
        Err(KtError::InvalidRecord(_))
    ));
    // A format 1 binding has no statement, so it never establishes an account.
    let v1 = AccountBinding::new(identity.root.public_key().clone(), roster);
    assert!(matches!(
        verify_account_record(&v1, &record, log_id, account(7)),
        Err(KtError::InvalidRecord(_))
    ));
}

#[tokio::test]
async fn messages_round_trip_and_have_the_documented_sizes() {
    let dir = open().await;
    let descriptor = dir.descriptor().clone();
    dir.publish(
        vec![
            (acct(1), binding(1, 1)),
            (
                Label::name("alice").unwrap(),
                NameBinding(account(1)).to_value(),
            ),
        ],
        T0 + 1,
    )
    .await
    .unwrap();
    dir.publish(vec![(acct(1), binding(1, 2))], T0 + 2)
        .await
        .unwrap();
    let head = dir.latest_head().await.unwrap();
    assert_eq!(head.encode().len(), HEAD_BYTES);
    assert_eq!(HEAD_BYTES, 148);
    assert_eq!(SignedHead::decode(&head.encode()).unwrap(), head);
    assert_eq!(descriptor.encode().len(), 71);
    assert_eq!(
        LogDescriptor::decode(&descriptor.encode()).unwrap(),
        descriptor
    );
    let gossip = HeadGossip { head: head.clone() };
    assert_eq!(gossip.encode().len(), GOSSIP_BYTES);
    assert_eq!(GOSSIP_BYTES, 150);

    let lookup = dir.lookup(&Label::name("alice").unwrap()).await.unwrap();
    assert_eq!(
        LookupResponse::decode(&lookup.encode().unwrap()).unwrap(),
        lookup
    );
    let history = dir.history(&acct(1)).await.unwrap();
    assert_eq!(
        HistoryResponse::decode(&history.encode().unwrap()).unwrap(),
        history
    );
    let audit = dir.audit(0, 2).await.unwrap();
    assert_eq!(
        AuditResponse::decode(&audit.encode().unwrap()).unwrap(),
        audit
    );
    let (_, entry) = verify_lookup(&descriptor, &Label::name("alice").unwrap(), &lookup).unwrap();
    assert_eq!(
        NameBinding::from_value(&entry.value).unwrap(),
        NameBinding(account(1))
    );
}

/// A store whose next write of an epoch's head fails, as a crash or a lost database
/// connection after akd committed the epoch would.
struct FailingHeadWrite {
    inner: MemoryStore,
    fail_next: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl KtStore for FailingHeadWrite {
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get(key).await
    }
    async fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StoreError> {
        self.inner.scan_prefix(prefix).await
    }
    async fn write(&self, records: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), StoreError> {
        if records.iter().any(|(k, _)| k[0] == b'h')
            && self
                .fail_next
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError("connection lost".into()));
        }
        self.inner.write(records).await
    }
}

#[tokio::test]
async fn an_epoch_whose_head_was_not_stored_is_repaired() {
    let store = Arc::new(FailingHeadWrite {
        inner: MemoryStore::new(),
        fail_next: false.into(),
    });
    let dir = open_with(store.clone(), SIGNER_SEED, VRF_SEED).await;
    let descriptor = dir.descriptor().clone();
    let mut client = Client::new(&descriptor);
    let first = dir
        .publish(vec![(acct(1), binding(1, 1))], T0 + 1)
        .await
        .unwrap();
    client.accept(&dir, &first, T0 + 1).await.unwrap();
    store
        .fail_next
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        dir.publish(vec![(acct(2), binding(2, 1))], T0 + 2).await,
        Err(DirectoryError::Store(_))
    ));
    // akd committed epoch 2; until repaired, proofs for it fail instead of lying.
    assert!(matches!(
        dir.lookup(&acct(2)).await,
        Err(DirectoryError::Corrupt(_))
    ));
    // The next publish repairs epoch 2 first, then publishes epoch 3.
    let third = dir
        .publish(vec![(acct(3), binding(3, 1))], T0 + 3)
        .await
        .unwrap();
    assert_eq!(third.epoch, 3);
    client.accept(&dir, &third, T0 + 3).await.unwrap();
    verify_head(&descriptor, &dir.head_at(2).await.unwrap()).unwrap();
    for n in 1..=3 {
        verify_lookup(&descriptor, &acct(n), &dir.lookup(&acct(n)).await.unwrap()).unwrap();
    }
}
