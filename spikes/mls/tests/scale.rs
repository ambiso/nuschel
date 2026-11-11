// SPDX-License-Identifier: EUPL-1.2
//! Cost at channel sizes (ignored by default; run in release):
//!   cargo test --release --test scale -- --ignored --nocapture --test-threads 1
//!
//! Only three members actually join; the others contribute key packages. That is
//! enough to measure commit, Welcome and tree sizes and per-member processing time.

use std::time::Instant;

use nuschel_mls_spike::*;
use openmls::prelude::tls_codec::Serialize as _;
use openmls::prelude::*;

const COMPACT_CREDENTIAL: usize = 168;

fn binding() -> ConversationBinding {
    ConversationBinding {
        server: [7; 32],
        conversation: [1; 16],
        generation: 0,
    }
}

fn device(name: &str, suite: Ciphersuite, compact: bool) -> Device {
    let mut device = Device::with_suite(name, suite);
    if compact {
        // The compact credential the design recommends (identity fingerprint,
        // device id, generation, leaf key, Ed25519 signature): same size, dummy
        // content. Credential verification is not exercised here.
        device.credential.credential = Credential::new(
            CredentialType::Other(DEVICE_CREDENTIAL),
            vec![0x5a; COMPACT_CREDENTIAL],
        );
    }
    device
}

fn run(n: usize, suite: Ciphersuite, compact: bool) {
    let alice = device("alice", suite, compact);
    let devices: Vec<Device> = (1..n)
        .map(|i| device(&format!("m{i}"), suite, compact))
        .collect();
    let started = Instant::now();
    let kps: Vec<KeyPackage> = devices.iter().map(|d| d.key_package()).collect();
    let kp_time = started.elapsed() / (n as u32 - 1);
    let kp_size = to_wire(&MlsMessageOut::from(kps[0].clone())).len();

    let mut group = alice.create_group(&binding());
    let started = Instant::now();
    let (commit, welcome, _) = group
        .add_members(&alice.provider, alice.signer(), &kps)
        .unwrap();
    group.merge_pending_commit(&alice.provider).unwrap();
    let add_time = started.elapsed();
    let commit = to_wire(&commit);
    let welcome = to_wire(&welcome);

    // Two members join (the Welcome carries the ratchet tree).
    let started = Instant::now();
    let mut bob = devices[0].join(&welcome);
    let join_time = started.elapsed();
    let mut carol = devices[1].join(&welcome);
    let tree = group
        .export_ratchet_tree()
        .tls_serialize_detached()
        .unwrap()
        .len();

    // A member's update commit (post-compromise security), processed by another.
    let started = Instant::now();
    let bundle = bob
        .self_update(
            &devices[0].provider,
            devices[0].signer(),
            LeafNodeParameters::default(),
        )
        .unwrap();
    bob.merge_pending_commit(&devices[0].provider).unwrap();
    let update_time = started.elapsed();
    let update = to_wire(bundle.commit());
    let started = Instant::now();
    devices[1].receive(&mut carol, &update).unwrap();
    let process_time = started.elapsed();

    // Removing one member.
    let leaf = carol.members().last().unwrap().index;
    let started = Instant::now();
    let (remove, _, _) = carol
        .remove_members(&devices[1].provider, devices[1].signer(), &[leaf])
        .unwrap();
    let remove_time = started.elapsed();
    let remove = to_wire(&remove);

    // An application message.
    let started = Instant::now();
    let message = devices[0].send(&mut bob, &"x".repeat(100));
    let encrypt_time = started.elapsed();
    let _ = group;

    println!(
        "| {n:>4} | {:<6} | {:<7} | {:>6} B {:>6.1?} | {:>8.1?} {:>7} B | {:>9} B | {:>9} B | {:>8.1?} | {:>6} B {:>7.1?} {:>7.1?} | {:>6} B {:>7.1?} | {:>3} B {:>6.1?} |",
        if suite == SUITE { "X-Wing" } else { "X25519" },
        if compact { "compact" } else { "chain" },
        kp_size,
        kp_time,
        add_time,
        commit.len(),
        welcome.len(),
        tree,
        join_time,
        update.len(),
        update_time,
        process_time,
        remove.len(),
        remove_time,
        message.len(),
        encrypt_time,
    );
}

#[test]
#[ignore]
fn scale() {
    println!(
        "|    n | suite  | cred    | key package     | add n-1: time, commit | Welcome     | tree        | join     | update: size, create, process | remove: size, time | message (100 B) |"
    );
    for n in [10, 100, 300, 1000] {
        run(n, SUITE, true);
        run(n, CLASSIC, true);
    }
    for n in [10, 100] {
        run(n, SUITE, false);
    }
}
