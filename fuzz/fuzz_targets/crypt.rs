// SPDX-License-Identifier: EUPL-1.2
//! OCB2 decryption of arbitrary datagrams: no panic, nothing accepted without
//! the key, and the nonce state unchanged by rejected packets (a genuine packet
//! still decrypts afterwards).
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschel_protocol::crypt::CryptState;

fuzz_target!(|data: &[u8]| {
    let (mut server, parts) = CryptState::generate_server([9; 48]);
    let mut client = CryptState::client(&parts.key, &parts.client_nonce, &parts.server_nonce)
        .expect("16-byte parts");
    let mut out = [0_u8; 4096];
    if data.len() <= out.len() {
        assert!(server.decrypt_into(data, &mut out).is_none());
    }
    let genuine = client.encrypt(b"voice");
    assert_eq!(server.decrypt(&genuine).as_deref(), Some(&b"voice"[..]));
});
