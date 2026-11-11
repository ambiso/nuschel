// SPDX-License-Identifier: EUPL-1.2
//! Key-transparency messages from a hostile server or member: every decoder, then the
//! client verifiers. The first byte picks the message. Decoding must be canonical (a
//! parsed message re-encodes to the same bytes), and nothing may panic. Heads are also
//! re-signed with the fixture key so that proofs reach akd's verifiers.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschel_kt::*;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, input)) = data.split_first() else {
        return;
    };
    let signer = Ed25519HeadSigner::from_seed(&[0x11; 32]);
    let descriptor = akd_descriptor(&signer, &VrfSecret::from_bytes([0x22; 32]));
    let sign = |epoch: u64, root: [u8; 32], at: u64| {
        SignedHead::sign(&signer, descriptor.log_id(), epoch, root, at).expect("software signer")
    };
    let resign = |h: &SignedHead| sign(h.epoch, h.root, h.issued_at_ms);
    match selector % 8 {
        0 => {
            if let Ok(d) = LogDescriptor::decode(input) {
                assert_eq!(d.encode(), input);
            }
        }
        1 => {
            if let Ok(h) = SignedHead::decode(input) {
                assert_eq!(h.encode(), input);
                let _ = verify_head(&descriptor, &h);
                let head = verify_head(&descriptor, &resign(&h)).expect("re-signed head");
                let _ = check_freshness(&head, 1_790_000_000_000, FreshnessPolicy::default());
            }
        }
        2 => {
            if let Ok(mut r) = LookupResponse::decode(input) {
                assert_eq!(r.encode().expect("re-encode"), input);
                let label = r.label.clone();
                let _ = verify_lookup(&descriptor, &label, &r);
                r.head = resign(&r.head);
                if let Ok((_, entry)) = verify_lookup(&descriptor, &label, &r) {
                    let _ = AccountBinding::from_value(&entry.value);
                }
            }
        }
        3 => {
            if let Ok(mut r) = HistoryResponse::decode(input) {
                assert_eq!(r.encode().expect("re-encode"), input);
                let label = r.label.clone();
                r.head = resign(&r.head);
                if let Ok(history) = verify_history(&descriptor, &label, &r) {
                    let _ = monitor_own(&history, None, |_| false);
                    let _ = Observation::from_history(Ok(history), 0);
                }
            }
        }
        4 => {
            if let Ok(r) = AuditResponse::decode(input) {
                assert_eq!(r.encode().expect("re-encode"), input);
                // From the empty tree (root 0) to the response's last root.
                let from = verify_head(&descriptor, &sign(r.from_epoch, [0; 32], 1)).unwrap();
                let last = *r.roots.last().expect("at least one root");
                let to = verify_head(&descriptor, &sign(r.to_epoch, last, 2)).unwrap();
                let _ = verify_audit(&descriptor, &from, &to, &r);
            }
        }
        5 => {
            if let Ok(g) = HeadGossip::decode(input) {
                assert_eq!(g.encode(), input);
                let mut known = KnownHeads::new(&descriptor);
                let _ = check_gossip(&descriptor, &known, &g);
                let resigned = HeadGossip { head: resign(&g.head) };
                if let Ok((head, _)) = check_gossip(&descriptor, &known, &resigned) {
                    known.record(head).expect("first head");
                    let other = sign(g.head.epoch, [0xee; 32], g.head.issued_at_ms);
                    let _ = check_gossip(&descriptor, &known, &HeadGossip { head: other });
                }
            }
        }
        6 => {
            if let Ok(e) = Evidence::decode(input) {
                assert_eq!(e.encode(), input);
                let _ = e.verify(&descriptor);
            }
        }
        _ => {
            if let Ok(label) = Label::decode(input) {
                assert_eq!(label.encode().expect("re-encode"), input);
            }
            if let Ok(value) = DirectoryValue::decode(input) {
                assert_eq!(value.encode(), input);
                let _ = AccountBinding::from_value(&value);
                let _ = NameBinding::from_value(&value);
            }
        }
    }
});
