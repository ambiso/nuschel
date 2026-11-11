// SPDX-License-Identifier: EUPL-1.2
//! Voice and ping datagrams from a hostile server as the client decodes them
//! (`nuschel_protocol::voice`, both the legacy and the protobuf format, after
//! decryption or from the TCP tunnel): nothing may panic, and only bounded,
//! non-empty Opus with a finite volume reaches the codec worker.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschel_protocol::voice::{MAX_VOICE_BYTES, UdpMessage, decode_udp, decode_voice};

fuzz_target!(|data: &[u8]| {
    let voice = decode_voice(data);
    match decode_udp(data) {
        Ok(UdpMessage::Audio(packet)) => {
            assert_eq!(voice.ok().as_ref(), Some(&packet));
            assert!(!packet.opus.is_empty() && packet.opus.len() <= MAX_VOICE_BYTES);
            assert!(packet.volume.is_finite() && (0.0..=4.0).contains(&packet.volume));
        }
        Ok(UdpMessage::Ping { .. }) | Err(_) => {}
    }
});
