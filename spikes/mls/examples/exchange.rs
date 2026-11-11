// SPDX-License-Identifier: EUPL-1.2
//! Size probe: two devices, a group, one message each way, hybrid identities.
use nuschl_mls_spike::*;

fn main() {
    let binding = ConversationBinding {
        server: [1; 32],
        conversation: [2; 16],
        generation: 0,
    };
    let alice = Device::new("alice");
    let bob = Device::new("bob");
    let mut group = alice.create_group(&binding);
    let (_, welcome, _) = group
        .add_members(&alice.provider, alice.signer(), &[bob.key_package()])
        .unwrap();
    group.merge_pending_commit(&alice.provider).unwrap();
    let mut bob_group = bob.join(&to_wire(&welcome));
    let hello = alice.send(&mut group, "hello");
    println!("{:?}", bob.receive(&mut bob_group, &hello).unwrap());
    println!(
        "roster: {}",
        Device::verify_roster(&bob_group).unwrap().len()
    );
}
