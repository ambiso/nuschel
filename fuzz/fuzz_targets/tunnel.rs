// SPDX-License-Identifier: EUPL-1.2
//! The web relay's tunnel frames (docs/web.md) as both ends decode them: no
//! panic, every accepted frame re-encodes to the same bytes, and the TLS shape
//! checks the relay applies to the first bytes of each direction never panic
//! and decide once six bytes are there.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschl_protocol::tunnel::{self, Frame};

fuzz_target!(|data: &[u8]| {
    if let Ok(frame) = tunnel::decode(data) {
        assert_eq!(tunnel::encode(&frame).as_deref(), Some(data));
        match frame {
            Frame::Open { host, .. } => {
                assert!(tunnel::valid_host(host) && host.len() <= tunnel::MAX_HOST);
            }
            Frame::Stream(bytes) => assert!(bytes.len() <= tunnel::MAX_STREAM),
            Frame::Datagram(bytes) => assert!(bytes.len() <= tunnel::MAX_DATAGRAM),
            Frame::Opened { .. } | Frame::Closed(_) => assert_eq!(data.len(), 2),
        }
    }
    let client = tunnel::starts_like_client_hello(data);
    let server = tunnel::starts_like_server_hello(data);
    if data.len() >= 6 {
        assert!(client.is_some() && server.is_some());
    }
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = tunnel::valid_host(text);
    }
});
