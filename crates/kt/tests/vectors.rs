// SPDX-License-Identifier: EUPL-1.2
//! Known-answer vectors in `tests/vectors/kt-v1.txt`.
//!
//! Everything is deterministic: ECVRF-TAI and Ed25519 signatures are, akd's commitments
//! are keyed by the VRF secret, and the tree's shape depends only on its labels. A
//! change in any encoding, in akd's hashing or in the configuration changes the file.
//! Regenerate deliberately with
//! `NUSCHEL_KT_WRITE_VECTORS=1 cargo test -p nuschel-kt --test vectors`.

mod common;

use common::*;
use nuschel_kt::*;
use std::collections::BTreeMap;

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/kt-v1.txt");

const HEADER: &str = "\
# nuschel-kt known-answer vectors, format version 1.
# Suite 1: akd 0.13 (ExperimentalConfiguration, domain label \"nuschel-kt/v1 akd\"),
# BLAKE3, ECVRF-EDWARDS25519-SHA512-TAI; heads signed with Ed25519.
# Values are hex unless the key ends in a number field (version, epoch).
# Log: signing key seed 11..11, VRF secret 22..22, opened at 1790000000000 ms.
# Epoch 1 (t+1000): acct 1 -> binding(root 1, roster 01..), acct 2 -> binding(root 2,
#   roster 01..), name \"alice\" -> account 1. Epoch 2 (t+2000): acct 1 -> roster 02..
#   (a device change). Epoch 3 (t+3000): acct 3 -> binding(root 3, roster 01..).
# A fork under the same keys publishes acct 1 -> root 666 in epoch 2 instead.
# Regenerate: NUSCHEL_KT_WRITE_VECTORS=1 cargo test -p nuschel-kt --test vectors
";

async fn generate() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut put = |k: &str, v: String| out.push((k.to_owned(), v));
    let dir = open().await;
    let fork = open().await;
    let descriptor = dir.descriptor().clone();
    put("signing_seed", hex::encode(SIGNER_SEED));
    put("vrf_seed", hex::encode(VRF_SEED));
    put("descriptor", hex::encode(descriptor.encode()));
    put("log_id", hex::encode(descriptor.log_id().0));
    put("label.acct1", hex::encode(acct(1).encode().unwrap()));
    put(
        "label.id",
        hex::encode(Label::Identity(root(1).fingerprint()).encode().unwrap()),
    );
    put(
        "label.alice",
        hex::encode(Label::name("alice").unwrap().encode().unwrap()),
    );
    put("value.acct1.v1", hex::encode(binding(1, 1).encode()));
    put("value.acct1.v2", hex::encode(binding(1, 2).encode()));
    put(
        "value.alice",
        hex::encode(NameBinding::Account(account(1)).to_value().encode()),
    );

    put(
        "head.0",
        hex::encode(dir.latest_head().await.unwrap().encode()),
    );
    let epoch1 = vec![
        (acct(1), binding(1, 1)),
        (acct(2), binding(2, 1)),
        (
            Label::name("alice").unwrap(),
            NameBinding::Account(account(1)).to_value(),
        ),
    ];
    dir.publish(epoch1.clone(), T0 + 1000).await.unwrap();
    fork.publish(epoch1, T0 + 1000).await.unwrap();
    dir.publish(vec![(acct(1), binding(1, 2))], T0 + 2000)
        .await
        .unwrap();
    let forked = fork
        .publish(vec![(acct(1), binding(666, 1))], T0 + 2000)
        .await
        .unwrap();
    dir.publish(vec![(acct(3), binding(3, 1))], T0 + 3000)
        .await
        .unwrap();
    for epoch in 1..=3 {
        put(
            &format!("head.{epoch}"),
            hex::encode(dir.head_at(epoch).await.unwrap().encode()),
        );
    }
    put(
        "lookup.acct1",
        hex::encode(dir.lookup(&acct(1)).await.unwrap().encode().unwrap()),
    );
    put("lookup.acct1.version", "2".into());
    put("lookup.acct1.epoch", "2".into());
    put(
        "lookup.alice",
        hex::encode(
            dir.lookup(&Label::name("alice").unwrap())
                .await
                .unwrap()
                .encode()
                .unwrap(),
        ),
    );
    put(
        "history.acct1",
        hex::encode(dir.history(&acct(1)).await.unwrap().encode().unwrap()),
    );
    put(
        "audit.0.3",
        hex::encode(dir.audit(0, 3).await.unwrap().encode().unwrap()),
    );
    let head3 = dir.latest_head().await.unwrap();
    put("gossip.3", hex::encode(HeadGossip { head: head3 }.encode()));
    let evidence = Evidence {
        kind: EvidenceKind::Equivocation,
        first: dir.head_at(2).await.unwrap(),
        second: forked,
    };
    put("evidence.equivocation.2", hex::encode(evidence.encode()));
    out
}

fn render(entries: &[(String, String)]) -> String {
    let mut s = HEADER.to_owned();
    for (k, v) in entries {
        s.push_str(&format!("{k} = {v}\n"));
    }
    s
}

fn parse(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let (k, v) = l.split_once(" = ").expect("key = value");
            (k.to_owned(), v.to_owned())
        })
        .collect()
}

#[tokio::test]
async fn vectors_are_reproducible() {
    let rendered = render(&generate().await);
    if std::env::var_os("NUSCHEL_KT_WRITE_VECTORS").is_some() {
        std::fs::write(PATH, &rendered).unwrap();
    }
    let on_disk = std::fs::read_to_string(PATH).expect("vectors file");
    assert!(
        on_disk == rendered,
        "{PATH} is out of date (see its header)"
    );
}

/// The file alone, through the client API only.
#[test]
fn vectors_verify_with_the_client_functions() {
    let v = parse(&std::fs::read_to_string(PATH).unwrap());
    let bytes = |k: &str| hex::decode(&v[k]).unwrap();
    let descriptor = LogDescriptor::decode(&bytes("descriptor")).unwrap();
    assert_eq!(descriptor.log_id().0.to_vec(), bytes("log_id"));
    assert_eq!(descriptor.suite(), Suite::AkdBlake3Ecvrf25519V1);

    let heads: Vec<VerifiedHead> = (0..=3)
        .map(|e| {
            verify_head(
                &descriptor,
                &SignedHead::decode(&bytes(&format!("head.{e}"))).unwrap(),
            )
            .unwrap()
        })
        .collect();
    let mut known = KnownHeads::new(&descriptor);
    for (e, head) in heads.iter().enumerate() {
        assert_eq!(head.epoch(), e as u64);
        known.record(head.clone()).unwrap();
    }

    let acct1 = Label::decode(&bytes("label.acct1")).unwrap();
    let lookup = LookupResponse::decode(&bytes("lookup.acct1")).unwrap();
    let (at, entry) = verify_lookup(&descriptor, &acct1, &lookup).unwrap();
    assert_eq!(&at, &heads[3]);
    assert_eq!(entry.value.encode(), bytes("value.acct1.v2"));
    assert_eq!(entry.version.to_string(), v["lookup.acct1.version"]);
    assert_eq!(entry.epoch.to_string(), v["lookup.acct1.epoch"]);
    let binding = AccountBinding::from_value(&entry.value).unwrap();
    assert_eq!(
        Label::Identity(binding.fingerprint()).encode().unwrap(),
        bytes("label.id")
    );

    let alice = Label::decode(&bytes("label.alice")).unwrap();
    let (_, entry) = verify_lookup(
        &descriptor,
        &alice,
        &LookupResponse::decode(&bytes("lookup.alice")).unwrap(),
    )
    .unwrap();
    assert_eq!(entry.value.encode(), bytes("value.alice"));

    let history = HistoryResponse::decode(&bytes("history.acct1")).unwrap();
    let history = verify_history(&descriptor, &acct1, &history).unwrap();
    let values: Vec<_> = history
        .entries
        .iter()
        .map(|e| (e.version, e.epoch, e.value.encode()))
        .collect();
    assert_eq!(
        values,
        vec![
            (2, 2, bytes("value.acct1.v2")),
            (1, 1, bytes("value.acct1.v1"))
        ]
    );

    let audit = AuditResponse::decode(&bytes("audit.0.3")).unwrap();
    verify_audit(&descriptor, &heads[0], &heads[3], &audit).unwrap();

    let gossip = HeadGossip::decode(&bytes("gossip.3")).unwrap();
    assert_eq!(
        check_gossip(&descriptor, &known, &gossip).unwrap().1,
        GossipOutcome::Known
    );

    let evidence = Evidence::decode(&bytes("evidence.equivocation.2")).unwrap();
    evidence.verify(&descriptor).unwrap();
    let forked = verify_head(&descriptor, &evidence.second).unwrap();
    assert!(matches!(
        known.classify(&forked),
        Err(KtError::Equivocation(_))
    ));
}
