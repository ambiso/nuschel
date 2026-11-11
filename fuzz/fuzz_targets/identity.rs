// SPDX-License-Identifier: EUPL-1.2
//! Hostile bytes through every nuschl-identity decoder: certificates, the compact MLS
//! credential, linking messages, sealed envelopes, the provisioning bundle, the safety
//! QR payload and the typed codes. Nothing may panic, whatever decodes must re-encode to
//! exactly the input, and the signature checks run on whatever decodes. Seeds:
//! `fuzz/seeds/identity` (from the crate's test vectors).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    nuschl_identity::fuzzing::decode_everything(data);
});
