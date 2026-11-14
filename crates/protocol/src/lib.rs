// SPDX-License-Identifier: EUPL-1.2
//! Transport-independent Mumble wire protocol. No audio decoding or network I/O.

use bytes::{Buf, Bytes, BytesMut};
use thiserror::Error;

pub mod crypt;
pub mod nuschl;
pub mod plugin;
pub mod relay;
pub mod tunnel;
pub mod voice;

pub mod control {
    include!(concat!(env!("OUT_DIR"), "/mumble_proto.rs"));
    mod redacted;
}

pub mod udp {
    include!(concat!(env!("OUT_DIR"), "/mumble_udp.rs"));
}

pub const HEADER_BYTES: usize = 6;
/// Stock Mumble rejects control frames above this body size (`Connection.cpp`).
pub const MAX_CONTROL_BYTES: usize = 0x7f_ffff;

/// Control message types: the first two bytes of a frame header.
pub mod kind {
    pub const VERSION: u16 = 0;
    pub const UDP_TUNNEL: u16 = 1;
    pub const AUTHENTICATE: u16 = 2;
    pub const PING: u16 = 3;
    pub const REJECT: u16 = 4;
    pub const SERVER_SYNC: u16 = 5;
    pub const CHANNEL_REMOVE: u16 = 6;
    pub const CHANNEL_STATE: u16 = 7;
    pub const USER_REMOVE: u16 = 8;
    pub const USER_STATE: u16 = 9;
    pub const BAN_LIST: u16 = 10;
    pub const TEXT_MESSAGE: u16 = 11;
    pub const PERMISSION_DENIED: u16 = 12;
    pub const ACL: u16 = 13;
    pub const QUERY_USERS: u16 = 14;
    pub const CRYPT_SETUP: u16 = 15;
    pub const CONTEXT_ACTION_MODIFY: u16 = 16;
    pub const CONTEXT_ACTION: u16 = 17;
    pub const USER_LIST: u16 = 18;
    pub const VOICE_TARGET: u16 = 19;
    pub const PERMISSION_QUERY: u16 = 20;
    pub const CODEC_VERSION: u16 = 21;
    pub const USER_STATS: u16 = 22;
    pub const REQUEST_BLOB: u16 = 23;
    pub const SERVER_CONFIG: u16 = 24;
    pub const SUGGEST_CONFIG: u16 = 25;
    pub const PLUGIN_DATA_TRANSMISSION: u16 = 26;
    /// Reserved for the Nuschl extension (`docs/extension.md`): 0x4E00–0x4EFF.
    pub const NUSCHL_EXTENSION: std::ops::RangeInclusive<u16> = 0x4e00..=0x4eff;
}

/// A complete control frame (header and protobuf body) in one buffer, so a
/// broadcast encodes once and shares the bytes. `None` if the body would exceed
/// what stock clients accept.
pub fn encode_control(kind: u16, message: &impl prost::Message) -> Option<Bytes> {
    let length = message.encoded_len();
    if length > MAX_CONTROL_BYTES {
        return None;
    }
    let mut buffer = BytesMut::with_capacity(HEADER_BYTES + length);
    buffer.extend_from_slice(&kind.to_be_bytes());
    buffer.extend_from_slice(&(length as u32).to_be_bytes());
    message.encode(&mut buffer).ok()?;
    Some(buffer.freeze())
}

/// A tunneled voice datagram as a `UDPTunnel` frame (raw bytes, not protobuf).
pub fn encode_tunnel(payload: &[u8]) -> Bytes {
    let mut buffer = BytesMut::with_capacity(HEADER_BYTES + payload.len());
    buffer.extend_from_slice(&kind::UDP_TUNNEL.to_be_bytes());
    buffer.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buffer.extend_from_slice(payload);
    buffer.freeze()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlFrame {
    /// Unknown types remain framed data until the session layer decides how to handle them.
    pub kind: u16,
    /// UDPTunnel payloads are raw UDP packets, not serialized UDPTunnel protobufs.
    pub payload: Bytes,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("Mumble control frame exceeds its size limit")]
    TooLarge,
    #[error("unexpected Mumble control message type {0}")]
    Unexpected(u16),
}

/// Decode incrementally without allocating the advertised payload size.
/// The transport adapter must also bound its read buffer and apply a read timeout.
pub fn decode_control(buffer: &mut BytesMut) -> Result<Option<ControlFrame>, Error> {
    decode_control_limited(buffer, |_| Some(MAX_CONTROL_BYTES))
}

/// Like [`decode_control`], with the body limit chosen per message type from
/// the header, before any of the body is waited for: `None` means the type is
/// not accepted at all (for example before authentication).
pub fn decode_control_limited(
    buffer: &mut BytesMut,
    limit: impl FnOnce(u16) -> Option<usize>,
) -> Result<Option<ControlFrame>, Error> {
    if buffer.len() < HEADER_BYTES {
        return Ok(None);
    }
    let kind = u16::from_be_bytes([buffer[0], buffer[1]]);
    let size = u32::from_be_bytes([buffer[2], buffer[3], buffer[4], buffer[5]]) as usize;
    let Some(limit) = limit(kind) else {
        return Err(Error::Unexpected(kind));
    };
    if size > limit.min(MAX_CONTROL_BYTES) {
        return Err(Error::TooLarge);
    }
    if buffer.len() < HEADER_BYTES + size {
        return Ok(None);
    }
    buffer.advance(HEADER_BYTES);
    Ok(Some(ControlFrame {
        kind,
        payload: buffer.split_to(size).freeze(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_fragmented_and_coalesced_control_frames() -> Result<(), Error> {
        let mut buffer = BytesMut::from(&[0, 1, 0, 0, 0, 2, 7][..]);
        assert_eq!(decode_control(&mut buffer)?, None);
        buffer.extend_from_slice(&[8, 0, 4, 0, 0, 0, 0]);
        assert_eq!(
            decode_control(&mut buffer)?,
            Some(ControlFrame {
                kind: 1,
                payload: Bytes::from_static(&[7, 8])
            })
        );
        assert_eq!(
            decode_control(&mut buffer)?,
            Some(ControlFrame {
                kind: 4,
                payload: Bytes::new()
            })
        );
        assert!(buffer.is_empty());
        Ok(())
    }

    #[test]
    fn rejects_an_oversized_header_without_waiting_for_its_body() {
        let mut buffer = BytesMut::from(&[0, 1, 255, 255, 255, 255][..]);
        assert_eq!(decode_control(&mut buffer), Err(Error::TooLarge));
        // 0x800000 is one byte more than stock Mumble accepts.
        let mut buffer = BytesMut::from(&[0, 1, 0, 0x80, 0, 0][..]);
        assert_eq!(decode_control(&mut buffer), Err(Error::TooLarge));
    }

    #[test]
    fn per_type_limits_apply_before_the_body_arrives() {
        let limit = |kind| match kind {
            kind::VERSION => Some(4),
            _ => None,
        };
        let mut buffer = BytesMut::from(&[0, 0, 0, 0, 0, 5][..]);
        assert_eq!(
            decode_control_limited(&mut buffer, limit),
            Err(Error::TooLarge)
        );
        let mut buffer = BytesMut::from(&[0, 13, 0, 0, 0, 0][..]);
        assert_eq!(
            decode_control_limited(&mut buffer, limit),
            Err(Error::Unexpected(13))
        );
        let mut buffer = BytesMut::from(&[0, 0, 0, 0, 0, 1, 9][..]);
        assert_eq!(
            decode_control_limited(&mut buffer, limit),
            Ok(Some(ControlFrame {
                kind: 0,
                payload: Bytes::from_static(&[9])
            }))
        );
    }

    #[test]
    fn secrets_are_redacted_in_debug_output() {
        let authenticate = control::Authenticate {
            username: Some("alice".into()),
            password: Some("hunter2".into()),
            tokens: vec!["channel-secret".into()],
            ..Default::default()
        };
        let crypt = control::CryptSetup {
            key: Some(vec![0xab; 16]),
            client_nonce: Some(vec![0xcd; 16]),
            server_nonce: Some(vec![0xef; 16]),
        };
        let state = control::UserState {
            temporary_access_tokens: vec!["temporary-secret".into()],
            comment: Some("private words".into()),
            ..Default::default()
        };
        let text = control::TextMessage {
            message: "message body".into(),
            ..Default::default()
        };
        let acl = control::acl::ChanAcl {
            group: Some("#token-secret".into()),
            ..Default::default()
        };
        let printed = format!("{authenticate:?} {crypt:?} {state:?} {text:?} {acl:?}");
        for secret in [
            "hunter2",
            "channel-secret",
            "171",
            "ab, ab",
            "temporary-secret",
            "private words",
            "message body",
            "token-secret",
        ] {
            assert!(!printed.contains(secret), "{secret} leaked: {printed}");
        }
        assert!(printed.contains("alice"));
    }
}

/// Deterministic hostile-input tests: every decoder must reject garbage without
/// panicking, and bounded inputs must never trigger large allocations.
#[cfg(test)]
mod hostile_input {
    use super::*;

    pub(crate) struct Rng(u64);
    impl Rng {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        pub(crate) fn bytes(&mut self, max: usize) -> Vec<u8> {
            let length = (self.next() as usize) % (max + 1);
            (0..length).map(|_| self.next() as u8).collect()
        }
    }

    #[test]
    fn voice_and_udp_decoders_survive_random_bytes() {
        let mut rng = Rng(0x5eed_1234_abcd_ef01);
        for _ in 0..50_000 {
            let mut bytes = rng.bytes(64);
            // Bias towards valid headers so deeper paths are exercised.
            if let Some(first) = bytes.first_mut() {
                *first = [0x00, 0x01, 0x20, 0x80, 0x9f, *first][(rng.next() % 6) as usize];
            }
            let _ = voice::decode_voice(&bytes);
            let _ = voice::decode_udp(&bytes);
        }
    }

    #[test]
    fn crypt_state_rejects_random_datagrams() {
        let mut rng = Rng(42);
        let mut state = crypt::CryptState::client(&[7; 16], &[1; 16], &[2; 16]).unwrap();
        for _ in 0..20_000 {
            assert!(state.decrypt(&rng.bytes(200)).is_none());
        }
        assert_eq!(state.stats.good, 0);
    }

    #[test]
    fn control_framing_survives_random_streams() {
        let mut rng = Rng(9);
        for _ in 0..20_000 {
            let mut buffer = BytesMut::from(rng.bytes(64).as_slice());
            while let Ok(Some(frame)) = decode_control(&mut buffer) {
                assert!(frame.payload.len() <= MAX_CONTROL_BYTES);
                use prost::Message as _;
                let _ = control::UserState::decode(frame.payload.clone());
                let _ = control::TextMessage::decode(frame.payload.clone());
                let _ = control::Acl::decode(frame.payload);
            }
        }
    }
}
