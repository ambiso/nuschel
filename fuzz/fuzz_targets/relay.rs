// SPDX-License-Identifier: EUPL-1.2
//! Voice datagrams as the server parses and re-encodes them: no panic, output
//! within the UDP plaintext limit, a protobuf re-encoding that prost accepts,
//! and server-browser ping answers never above twice the request.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschl_protocol::{
    relay::{self, Context, Datagram, Delivery, PingBudget, ServerInfo},
    udp,
};
use prost::Message;

struct Open;
impl PingBudget for Open {
    fn allow(&mut self, _: usize, _: usize) -> bool {
        true
    }
}

fuzz_target!(|data: &[u8]| {
    let mut out = [0_u8; 2048];
    for protobuf in [false, true] {
        if let Some(Datagram::Audio(audio)) = relay::parse(protobuf, data) {
            for (target, context) in [(false, Context::Normal), (true, Context::Listen)] {
                let delivery = Delivery {
                    sender: 7,
                    context,
                    volume: 0.5,
                    positional: true,
                };
                if let Some(length) = relay::encode(target, &audio, delivery, &mut out) {
                    assert!(length <= relay::MAX_PLAIN);
                    if target {
                        let decoded = udp::Audio::decode(&out[1..length]).expect("valid protobuf");
                        assert_eq!(decoded.opus_data, audio.opus);
                        assert_eq!(decoded.sender_session, 7);
                    }
                }
            }
        }
    }
    let info = ServerInfo {
        version: 1 << 48,
        users: 1,
        max_users: 2,
        max_bandwidth: 3,
    };
    let mut answer = [0_u8; 64];
    if let Some(length) = relay::answer_info_ping(data, info, &mut Open, &mut answer) {
        assert!(length <= 2 * data.len());
    }
});
