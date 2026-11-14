// SPDX-License-Identifier: EUPL-1.2
//! Control framing with per-type limits, then every message type's decoder:
//! no panic, no frame above its limit. Plugin data also goes through its
//! checked decoder, which must keep its limits and round-trip what it accepts.
#![no_main]

use bytes::BytesMut;
use libfuzzer_sys::fuzz_target;
use nuschl_protocol::plugin::{MAX_DATA_BYTES, MAX_DATA_ID_BYTES, PluginData, max_body};
use nuschl_protocol::{control as proto, decode_control_limited, kind};
use prost::Message;

fn plugin_data(body: &[u8]) {
    for n in [0, 4, 512] {
        if let Ok(data) = PluginData::decode(body, n) {
            assert!(data.receivers.len() <= n && body.len() <= max_body(n));
            assert!(data.payload.size() <= MAX_DATA_BYTES + MAX_DATA_ID_BYTES);
            let encoded = data.clone().into_message().encode_to_vec();
            assert_eq!(PluginData::decode(&encoded, n), Ok(data));
        }
    }
}

fn decode_all(kind: u16, payload: bytes::Bytes) {
    macro_rules! each {
        ($($kind:literal => $type:ty),* $(,)?) => {
            match kind {
                $($kind => { let _ = <$type>::decode(payload); })*
                _ => {}
            }
        };
    }
    each!(
        0 => proto::Version, 2 => proto::Authenticate, 3 => proto::Ping, 4 => proto::Reject,
        5 => proto::ServerSync, 6 => proto::ChannelRemove, 7 => proto::ChannelState,
        8 => proto::UserRemove, 9 => proto::UserState, 10 => proto::BanList,
        11 => proto::TextMessage, 12 => proto::PermissionDenied, 13 => proto::Acl,
        14 => proto::QueryUsers, 15 => proto::CryptSetup, 16 => proto::ContextActionModify,
        17 => proto::ContextAction, 18 => proto::UserList, 19 => proto::VoiceTarget,
        20 => proto::PermissionQuery, 21 => proto::CodecVersion, 22 => proto::UserStats,
        23 => proto::RequestBlob, 24 => proto::ServerConfig, 25 => proto::SuggestConfig,
        26 => proto::PluginDataTransmission,
    );
}

fuzz_target!(|data: &[u8]| {
    let mut buffer = BytesMut::from(data);
    let limit = |kind: u16| (kind < 32).then_some(64 * 1024);
    while let Ok(Some(frame)) = decode_control_limited(&mut buffer, limit) {
        assert!(frame.payload.len() <= 64 * 1024);
        if frame.kind == kind::PLUGIN_DATA_TRANSMISSION {
            plugin_data(&frame.payload);
        }
        decode_all(frame.kind, frame.payload);
    }
});
