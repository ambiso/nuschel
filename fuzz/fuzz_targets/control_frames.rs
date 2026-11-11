// SPDX-License-Identifier: EUPL-1.2
//! Control framing with per-type limits, then every message type's decoder:
//! no panic, no frame above its limit.
#![no_main]

use bytes::BytesMut;
use libfuzzer_sys::fuzz_target;
use nuschel_protocol::{control as proto, decode_control_limited};
use prost::Message;

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
        decode_all(frame.kind, frame.payload);
    }
});
