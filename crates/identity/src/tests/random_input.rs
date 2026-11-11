// SPDX-License-Identifier: EUPL-1.2
//! A cheap property test: random bytes, and random mutations of valid encodings, through
//! every decoder (the same checks as the fuzz target). Nothing may panic, and whatever
//! decodes must re-encode to the same bytes.

use super::*;
use crate::fuzzing::decode_everything;
use crate::{DeviceKeys, LinkCode, RecoveryKey, SafetyNumber, SafetyQr};

/// xorshift64*: deterministic, so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn mutate(rng: &mut Rng, valid: &[u8]) -> Vec<u8> {
    let mut out = valid.to_vec();
    for _ in 0..1 + rng.below(4) {
        match rng.below(5) {
            0 if !out.is_empty() => {
                let i = rng.below(out.len());
                out[i] = rng.next() as u8;
            }
            1 if !out.is_empty() => {
                let i = rng.below(out.len());
                out.truncate(i);
            }
            2 => {
                let i = rng.below(out.len() + 1);
                out.insert(i, rng.next() as u8);
            }
            3 if !out.is_empty() => {
                let i = rng.below(out.len());
                out.remove(i);
            }
            _ => {
                // Length fields: overwrite a byte near the start with an extreme value.
                if out.len() > 2 {
                    let i = 2 + rng.below(out.len().min(64) - 2);
                    out[i] = [0x00, 0x01, 0x7f, 0x80, 0xff][rng.below(5)];
                }
            }
        }
    }
    out
}

#[test]
fn decoders_never_panic_and_stay_canonical() {
    let f = fixture();
    let device = DeviceKeys::generate().unwrap();
    let sealed = crate::SealedToDevice::seal(&device.public_keys(), b"", b"x").unwrap();
    let valid: Vec<Vec<u8>> = vec![
        f.id.root.public_key().encode(),
        f.id.device_signing.certificate().encode(),
        f.id.device_certificate.encode(),
        f.id.device_credential.encode(),
        f.revocation.encode(),
        sealed.encode(),
        SafetyQr::new(&f.id.root.fingerprint(), &f.identity.fingerprint())
            .as_bytes()
            .to_vec(),
        SafetyNumber::new(&f.id.root.fingerprint(), &f.identity.fingerprint())
            .to_string()
            .into_bytes(),
        LinkCode::generate(77).unwrap().as_str().as_bytes().to_vec(),
        RecoveryKey::generate()
            .unwrap()
            .display()
            .as_bytes()
            .to_vec(),
    ];
    for v in &valid {
        decode_everything(v);
    }
    let mut rng = Rng(0x5eed_1234_abcd_0001);
    for round in 0..1500 {
        let input = if round % 4 == 0 {
            let len = rng.below(300);
            (0..len).map(|_| rng.next() as u8).collect()
        } else {
            let which = rng.below(valid.len());
            mutate(&mut rng, &valid[which])
        };
        decode_everything(&input);
    }
}
