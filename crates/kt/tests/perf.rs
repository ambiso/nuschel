// SPDX-License-Identifier: EUPL-1.2
//! Proof sizes and timings at 10k and 100k accounts. Ignored by default; run with
//! `cargo test -p nuschl-kt --release --test perf -- --ignored --nocapture
//! --test-threads 1`. Uses a 4-thread runtime (the machine is shared).

mod common;

use common::*;
use nuschl_kt::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn ms(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1000.0)
}

fn value(n: u32) -> DirectoryValue {
    // Realistic size (2018 bytes: the hybrid root key and a roster commitment); the
    // roots repeat, which does not matter to the tree.
    binding(n % 997, (n % 251) as u8)
}

async fn measure(accounts: u32) {
    println!("\n## {accounts} accounts");
    let store = Arc::new(MemoryStore::new());
    let dir = open_with(store.clone(), SIGNER_SEED, VRF_SEED).await;
    let descriptor = dir.descriptor().clone();
    let mut now = T0;

    let start = Instant::now();
    let batch: Vec<_> = (0..accounts).map(|n| (acct(n), value(n))).collect();
    now += 10_000;
    dir.publish(batch, now).await.unwrap();
    println!(
        "initial publish of {accounts} accounts: {}",
        ms(start.elapsed())
    );
    println!(
        "store: {} records, {:.1} MB",
        store.len(),
        store.bytes() as f64 / 1e6
    );

    // Account 0 gets three versions (two device changes).
    for roster in [200u8, 201] {
        now += 10_000;
        dir.publish(vec![(acct(0), binding(0, roster))], now)
            .await
            .unwrap();
    }
    let single_from = dir.latest_head().await.unwrap().epoch;
    now += 10_000;
    let start = Instant::now();
    dir.publish(vec![(acct(1), binding(1, 99))], now)
        .await
        .unwrap();
    println!("publish 1 change: {}", ms(start.elapsed()));
    let hundred_from = single_from + 1;
    let batch: Vec<_> = (0..100u32)
        .map(|i| {
            if i < 50 {
                (acct(accounts + i), value(i))
            } else {
                (acct(i * 7 % accounts + 2), binding(i, 77))
            }
        })
        .collect();
    now += 10_000;
    let start = Instant::now();
    dir.publish(batch, now).await.unwrap();
    println!(
        "publish 100 updates (50 new, 50 changed): {}",
        ms(start.elapsed())
    );

    // Lookups.
    let samples = 50u32;
    let (mut generate, mut verify, mut size) = (Duration::ZERO, Duration::ZERO, 0usize);
    let mut max = 0;
    for i in 0..samples {
        let label = acct(i * 7919 % accounts);
        let t = Instant::now();
        let response = dir.lookup(&label).await.unwrap();
        generate += t.elapsed();
        let bytes = response.encode().unwrap();
        size += bytes.len();
        max = max.max(bytes.len());
        let t = Instant::now();
        let response = LookupResponse::decode(&bytes).unwrap();
        verify_lookup(&descriptor, &label, &response).unwrap();
        verify += t.elapsed();
    }
    println!(
        "lookup: response {} bytes avg ({} max; {} of it the value and head), generate {}, verify {}",
        size / samples as usize,
        max,
        2022 + HEAD_BYTES,
        ms(generate / samples),
        ms(verify / samples)
    );

    // History of a label with three versions.
    let t = Instant::now();
    let history = dir.history(&acct(0)).await.unwrap();
    let generate = t.elapsed();
    let bytes = history.encode().unwrap();
    let t = Instant::now();
    let checked = verify_history(
        &descriptor,
        &acct(0),
        &HistoryResponse::decode(&bytes).unwrap(),
    )
    .unwrap();
    let verify = t.elapsed();
    assert_eq!(checked.entries.len(), 3);
    println!(
        "history (3 versions): response {} bytes, generate {}, verify {}",
        bytes.len(),
        ms(generate),
        ms(verify)
    );

    // Audits.
    for (what, from, to) in [
        ("1 change", single_from, single_from + 1),
        ("100 updates", hundred_from, hundred_from + 1),
        ("4 epochs", single_from - 2, hundred_from + 1),
        ("of the initial import", 0, 1),
    ] {
        let a = verify_head(&descriptor, &dir.head_at(from).await.unwrap()).unwrap();
        let b = verify_head(&descriptor, &dir.head_at(to).await.unwrap()).unwrap();
        let t = Instant::now();
        let audit = dir.audit(from, to).await.unwrap();
        let generate = t.elapsed();
        let bytes = audit.encode().unwrap();
        let t = Instant::now();
        verify_audit(&descriptor, &a, &b, &AuditResponse::decode(&bytes).unwrap()).unwrap();
        println!(
            "audit {what} ({from}->{to}): response {} bytes, generate {}, verify {}",
            bytes.len(),
            ms(generate),
            ms(t.elapsed())
        );
    }

    let head = dir.latest_head().await.unwrap();
    let t = Instant::now();
    for _ in 0..100 {
        verify_head(&descriptor, &head).unwrap();
    }
    println!(
        "head: {} bytes, verify {}; gossip {} bytes",
        HEAD_BYTES,
        ms(t.elapsed() / 100),
        GOSSIP_BYTES
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark: run in release with --ignored --nocapture"]
async fn proof_sizes_and_timings() {
    for accounts in [10_000, 100_000] {
        measure(accounts).await;
    }
}
