// SPDX-License-Identifier: EUPL-1.2
//! OCB2 decryption of arbitrary datagrams: no panic, and the nonce state
//! unchanged by rejected packets (a genuine packet still decrypts afterwards).
//! Mumble's header carries only 24 bits of the tag, so one random packet in
//! 2^24 passes without the key; at this target's speed that takes about a
//! minute (the first campaign found one after 21.6 million runs). Such a
//! packet is the protocol's, not a finding: only its length is checked.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschel_protocol::crypt::CryptState;

fuzz_target!(|data: &[u8]| {
    let (mut server, parts) = CryptState::generate_server([9; 48]);
    let mut client = CryptState::client(&parts.key, &parts.client_nonce, &parts.server_nonce)
        .expect("16-byte parts");
    let mut out = [0_u8; 4096];
    if data.len() <= out.len()
        && let Some(length) = server.decrypt_into(data, &mut out)
    {
        assert_eq!(length, data.len() - 4);
        return;
    }
    let genuine = client.encrypt(b"voice");
    assert_eq!(server.decrypt(&genuine).as_deref(), Some(&b"voice"[..]));
});
