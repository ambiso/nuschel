// SPDX-License-Identifier: EUPL-1.2
//! Witnessing's wire formats (docs/e2ee.md §5.9) from hostile peers: signed notes,
//! checkpoints and verifier keys (C2SP text formats), the `WitnessedHead` a server
//! attaches to heads, and the auditor's `AddEpochs` submissions and state files. The
//! first byte picks the format. Binary formats must be canonical (a parsed message
//! re-encodes to the same bytes), text formats must re-format to the same text, and
//! nothing may panic. Witnessed heads and submissions also run through the client's
//! and the auditor's checks.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschl_kt::witness::auditor::{AddEpochs, LogState, add_epochs};
use nuschl_kt::witness::note::{Cosigner, CosignerKey, Note};
use nuschl_kt::witness::{Checkpoint, EpochLeaf};
use nuschl_kt::*;
use std::sync::OnceLock;

fn auditor() -> &'static Cosigner {
    static AUDITOR: OnceLock<Cosigner> = OnceLock::new();
    AUDITOR.get_or_init(|| Cosigner::from_seed("witness.example.org", &[0x77; 32]).unwrap())
}

fuzz_target!(|data: &[u8]| {
    let Some((&selector, input)) = data.split_first() else {
        return;
    };
    let signer = Ed25519HeadSigner::from_seed(&[0x11; 32]);
    let descriptor = akd_descriptor(&signer, &VrfSecret::from_bytes([0x22; 32]));
    match selector % 7 {
        0 => {
            if let Ok(note) = Note::parse(input) {
                assert_eq!(note.format().as_bytes(), input);
                if let Ok(checkpoint) = Checkpoint::parse(&note.text) {
                    assert_eq!(checkpoint.body(), note.text);
                    let _ = auditor().key().verify(&note, &checkpoint);
                }
            }
        }
        1 => {
            if let Ok(text) = std::str::from_utf8(input) {
                if let Ok(checkpoint) = Checkpoint::parse(text) {
                    assert_eq!(checkpoint.body(), text);
                }
                if let Ok(key) = CosignerKey::from_vkey(text) {
                    assert_eq!(key.vkey(), text.trim());
                }
            }
        }
        2 => {
            if let Ok(witnessed) = WitnessedHead::decode(input) {
                assert_eq!(witnessed.encode().expect("re-encode"), input);
                // Against a head of epoch 0 and 3, with and without the auditor.
                for epoch in [0, 3] {
                    let head = SignedHead::sign(&signer, descriptor.log_id(), epoch, [5; 32], 1)
                        .expect("software signer");
                    let head = verify_head(&descriptor, &head).expect("own head");
                    let required =
                        WitnessPolicy::all(vec![auditor().key().clone()], 24 * 3600 * 1000);
                    for policy in [WitnessPolicy::off(), required] {
                        let _ = check_witnessed(
                            &descriptor,
                            "chat.example.org:64738/nuschl-kt/v1",
                            &head,
                            &witnessed,
                            &policy,
                            1_790_000_000_000,
                        );
                    }
                }
            }
        }
        3 => {
            if let Ok(request) = AddEpochs::decode(input) {
                assert_eq!(request.encode().expect("re-encode"), input);
                let _ = add_epochs(None, &request);
            }
        }
        4 => {
            if let Ok(state) = LogState::decode(input) {
                assert_eq!(state.encode().expect("re-encode"), input);
                let _ = state.checkpoint();
            }
        }
        5 => {
            if let Ok(leaf) = EpochLeaf::decode(input) {
                assert_eq!(leaf.encode(), input);
            }
        }
        _ => {
            // Inclusion proofs: any bytes as a proof against a small tree.
            let hashes: Vec<[u8; 32]> = input
                .chunks_exact(32)
                .map(|c| c.try_into().unwrap())
                .collect();
            if let Some((root, proof)) = hashes.split_first() {
                let index = u64::from(input.len() as u8 % 9);
                let _ = witness::tlog::verify_inclusion(index, 9, &[1; 32], proof, root);
            }
        }
    }
});
