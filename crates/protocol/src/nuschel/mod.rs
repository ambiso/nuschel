// SPDX-License-Identifier: EUPL-1.2
//! The Nuschel protocol extension (docs/extension.md): message types in the
//! reserved block 0x4E00–0x4EFF, the hello in field 7200 of `Version`, channel
//! properties in field 7200 of `ChannelState`, per-type limits before and after
//! decoding, and errors with fixed details. Pure data; no I/O.
use crate::{HEADER_BYTES, MAX_CONTROL_BYTES, control as proto};
use bytes::{Bytes, BytesMut};
use prost::Message as _;

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/nuschel.v1.rs"));
    mod redacted;
}

use v1::error::Code;

/// TLS ALPN identifiers. A Nuschel client offers both in one handshake; stock
/// Murmur ignores ALPN, a Nuschel server selects `nuschel/1`.
pub const ALPN: &[u8] = b"nuschel/1";
pub const ALPN_MUMBLE: &[u8] = b"mumble";
/// `Hello.protocol` for this specification.
pub const PROTOCOL: u32 = 1;

/// Capabilities this implementation knows; a capability is not a permission.
pub mod capability {
    /// Durable conversations, history pages, edits, deletions, read markers
    /// and direct conversations.
    pub const HISTORY: &str = "history/1";
    pub const SUBSCRIBE: &str = "subscribe/1";
    pub const TYPING: &str = "typing/1";
    pub const ALL: [&str; 3] = [HISTORY, SUBSCRIBE, TYPING];
}

/// Message types.
pub mod kind {
    pub const ERROR: u16 = 0x4e00;
    pub const CHAT_SEND: u16 = 0x4e01;
    pub const CHAT_ACK: u16 = 0x4e02;
    pub const CHAT_EVENT: u16 = 0x4e03;
    pub const HISTORY_REQUEST: u16 = 0x4e04;
    pub const HISTORY_PAGE: u16 = 0x4e05;
    pub const SUBSCRIBE: u16 = 0x4e06;
    pub const SUBSCRIBED: u16 = 0x4e07;
    pub const UNSUBSCRIBE: u16 = 0x4e08;
    pub const UNSUBSCRIBED: u16 = 0x4e09;
    pub const CHAT_EDIT: u16 = 0x4e0a;
    pub const CHAT_DELETE: u16 = 0x4e0b;
    pub const READ_MARKER: u16 = 0x4e0c;
    pub const TYPING: u16 = 0x4e0d;
    pub const OPEN_DIRECT: u16 = 0x4e0e;
    pub const DIRECT_OPENED: u16 = 0x4e0f;
    pub const LIST_DIRECT: u16 = 0x4e10;
    pub const DIRECT_LIST: u16 = 0x4e11;
    pub const CHANNEL_SETTINGS: u16 = 0x4e12;
    pub const DONE: u16 = 0x4e13;
}

/// Requests in flight per connection.
pub const MAX_OUTSTANDING: usize = 16;
/// Message text, in bytes of UTF-8.
pub const MAX_CONTENT_BYTES: usize = 64 * 1024;
pub const MAX_PAGE_MESSAGES: usize = 100;
/// Content bytes in one history page; a larger message comes alone.
pub const MAX_PAGE_BYTES: usize = 256 * 1024;
pub const MAX_SUBSCRIBE_PER_MESSAGE: usize = 64;
pub const MAX_SUBSCRIPTIONS: usize = 256;
pub const MAX_DIRECTS: usize = 1024;
pub const MAX_CAPABILITIES: usize = 32;
pub const MAX_CAPABILITY_BYTES: usize = 64;
pub const MAX_NAME_BYTES: usize = 512;

/// 16 bytes: conversation ids, account ids and idempotency keys.
pub type Id = [u8; 16];

pub fn id(bytes: &[u8]) -> Option<Id> {
    bytes.try_into().ok()
}

/// The capability a client-sent message type needs, or `None` if clients may
/// not send it at all (a server-to-client type, or unassigned).
pub fn required_capability(kind: u16) -> Option<&'static str> {
    use kind::*;
    match kind {
        CHAT_SEND | CHAT_EDIT | CHAT_DELETE | HISTORY_REQUEST | READ_MARKER | OPEN_DIRECT
        | LIST_DIRECT | CHANNEL_SETTINGS => Some(capability::HISTORY),
        SUBSCRIBE | UNSUBSCRIBE => Some(capability::SUBSCRIBE),
        TYPING => Some(capability::TYPING),
        _ => None,
    }
}

/// Wire-size cap for a frame from a client, checked from the header before
/// the body is buffered (after authentication; before it, every type in the
/// block closes the connection). Types clients may not send get a token
/// allowance so they can be answered with `UNSUPPORTED`.
pub fn client_limit(kind: u16) -> usize {
    use kind::*;
    match kind {
        CHAT_SEND | CHAT_EDIT => MAX_CONTENT_BYTES + 256,
        SUBSCRIBE | UNSUBSCRIBE => MAX_SUBSCRIBE_PER_MESSAGE * 18 + 32,
        HISTORY_REQUEST => 128,
        _ => 64,
    }
}

/// Wire-size cap for a frame from a server (the client's side).
pub fn server_limit(kind: u16) -> usize {
    use kind::*;
    match kind {
        // A message larger than the byte cap comes alone.
        HISTORY_PAGE => MAX_PAGE_BYTES + MAX_CONTENT_BYTES + MAX_PAGE_MESSAGES * 1024,
        CHAT_EVENT => MAX_CONTENT_BYTES + 2048,
        SUBSCRIBED => MAX_SUBSCRIBE_PER_MESSAGE * 64 + 32,
        DIRECT_LIST => MAX_DIRECTS * (MAX_NAME_BYTES + 96) + 32,
        DIRECT_OPENED => MAX_NAME_BYTES + 128,
        _ => 1024,
    }
    .min(MAX_CONTROL_BYTES)
}

/// The fixed text for an error code. Clients show it as plain text.
pub fn detail(code: Code) -> &'static str {
    match code {
        Code::Unspecified => "The request failed.",
        Code::Unsupported => "This server does not support that request.",
        Code::Malformed => "The request was malformed.",
        Code::NotFound => "No such conversation or message, or you may not use it.",
        Code::TooLarge => "The message is too large.",
        Code::RateLimited => "Too many requests; try again later.",
        Code::IdempotencyConflict => "This message key was already used for a different message.",
        Code::Busy => "The server is busy; try again later.",
        Code::DuplicateRequest => "This request is already in progress.",
        Code::TooManyRequests => "Too many requests are in progress.",
        Code::LimitReached => "This message cannot be edited any more.",
    }
}

pub fn error(request_id: u64, code: Code, retry_after_ms: u64) -> v1::Error {
    v1::Error {
        request_id,
        code: code as i32,
        detail: detail(code).into(),
        retry_after_ms,
    }
}

/// Why a hello is refused. Any refusal fails the connection closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HelloError {
    #[error("the Nuschel hello is malformed")]
    Malformed,
    #[error("the Nuschel hello has too many or invalid capabilities")]
    Capabilities,
    #[error("the peer speaks an unsupported Nuschel protocol version")]
    Protocol,
}

fn valid_capability(text: &str) -> bool {
    let Some((name, version)) = text.split_once('/') else {
        return false;
    };
    text.len() <= MAX_CAPABILITY_BYTES
        && !name.is_empty()
        && !version.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && version.bytes().all(|c| c.is_ascii_digit())
}

/// The hello in a `Version` body, if any, checked against its bounds. Field
/// 7200 may occur more than once on the wire (protobuf merges them); the
/// bounds apply to the merged result.
pub fn hello(version_body: &[u8]) -> Result<Option<v1::Hello>, HelloError> {
    let extension =
        v1::VersionExtension::decode(version_body).map_err(|_| HelloError::Malformed)?;
    let Some(hello) = extension.hello else {
        return Ok(None);
    };
    if hello.capabilities.len() > MAX_CAPABILITIES
        || !hello.capabilities.iter().all(|c| valid_capability(c))
    {
        return Err(HelloError::Capabilities);
    }
    if hello.protocol != PROTOCOL {
        return Err(HelloError::Protocol);
    }
    Ok(Some(hello))
}

/// The capabilities both sides announced, in a fixed order; unknown ones are
/// dropped.
pub fn intersect(ours: &[&'static str], theirs: &[String]) -> Vec<&'static str> {
    ours.iter()
        .copied()
        .filter(|capability| theirs.iter().any(|other| other == capability))
        .collect()
}

fn frame(kind: u16, parts: &[&dyn Fn(&mut BytesMut)], length: usize) -> Option<Bytes> {
    if length > MAX_CONTROL_BYTES {
        return None;
    }
    let mut buffer = BytesMut::with_capacity(HEADER_BYTES + length);
    buffer.extend_from_slice(&kind.to_be_bytes());
    buffer.extend_from_slice(&(length as u32).to_be_bytes());
    for part in parts {
        part(&mut buffer);
    }
    Some(buffer.freeze())
}

/// A `Version` frame, with the hello appended in field 7200 when given.
pub fn version_frame(version: &proto::Version, hello: Option<&v1::Hello>) -> Option<Bytes> {
    let extension = hello.map(|hello| v1::VersionExtension {
        hello: Some(hello.clone()),
    });
    let length = version.encoded_len() + extension.as_ref().map_or(0, |e| e.encoded_len());
    frame(
        crate::kind::VERSION,
        &[&|buffer| version.encode_raw(buffer), &|buffer| {
            if let Some(extension) = &extension {
                extension.encode_raw(buffer);
            }
        }],
        length,
    )
}

/// A `ChannelState` frame with the channel's extension properties; only for
/// connections that negotiated the extension.
pub fn channel_state_frame(state: &proto::ChannelState, info: v1::ChannelInfo) -> Option<Bytes> {
    let extension = v1::ChannelStateExtension {
        nuschel: Some(info),
    };
    frame(
        crate::kind::CHANNEL_STATE,
        &[&|buffer| state.encode_raw(buffer), &|buffer| {
            extension.encode_raw(buffer)
        }],
        state.encoded_len() + extension.encoded_len(),
    )
}

/// The extension properties in a `ChannelState` body, if valid.
pub fn channel_info(body: &[u8]) -> Option<v1::ChannelInfo> {
    let info = v1::ChannelStateExtension::decode(body).ok()?.nuschel?;
    id(&info.conversation)?;
    Some(info)
}

/// An extension message as a frame.
pub fn encode(kind: u16, message: &impl prost::Message) -> Option<Bytes> {
    crate::encode_control(kind, message)
}

/// Bounds a decoded message from a server must satisfy (a hostile server
/// cannot make the client hold more than these).
pub fn valid_message(message: &v1::Message) -> bool {
    message.content.len() <= MAX_CONTENT_BYTES
        && message.author.as_ref().is_none_or(|author| {
            author.name.len() <= MAX_NAME_BYTES
                && (author.account.is_empty() || author.account.len() == 16)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello_with(capabilities: &[&str]) -> v1::Hello {
        v1::Hello {
            protocol: PROTOCOL,
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
            policy: None,
        }
    }

    #[test]
    fn the_hello_rides_in_version_and_stock_decoders_skip_it() {
        let version = proto::Version {
            version_v2: Some(1 << 48),
            release: Some("x".into()),
            ..Default::default()
        };
        let frame = version_frame(&version, Some(&hello_with(&["history/1"]))).unwrap();
        let body = &frame[HEADER_BYTES..];
        assert_eq!(proto::Version::decode(body).unwrap(), version);
        assert_eq!(hello(body).unwrap().unwrap().capabilities, ["history/1"]);
        let plain = version_frame(&version, None).unwrap();
        assert_eq!(hello(&plain[HEADER_BYTES..]).unwrap(), None);
    }

    #[test]
    fn hello_bounds_fail_closed() {
        let encode = |hello: v1::Hello| {
            version_frame(&proto::Version::default(), Some(&hello)).unwrap()[HEADER_BYTES..]
                .to_vec()
        };
        let many: Vec<String> = (0..33).map(|n| format!("c{n}/1")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(
            hello(&encode(hello_with(&refs))),
            Err(HelloError::Capabilities)
        );
        for bad in ["History/1", "history", "/1", "a/", "a/1x", &"a".repeat(63)] {
            let bad = if bad.len() > 60 {
                format!("{bad}/1")
            } else {
                bad.into()
            };
            assert_eq!(
                hello(&encode(hello_with(&[&bad]))),
                Err(HelloError::Capabilities),
                "{bad}"
            );
        }
        let mut wrong = hello_with(&["history/1"]);
        wrong.protocol = 2;
        assert_eq!(hello(&encode(wrong)), Err(HelloError::Protocol));
        // Unknown but well-formed capabilities are fine and ignored.
        let ok = hello(&encode(hello_with(&["future-thing/7", "history/1"])))
            .unwrap()
            .unwrap();
        assert_eq!(intersect(&capability::ALL, &ok.capabilities), ["history/1"]);
    }

    #[test]
    fn channel_properties_ride_in_channel_state() {
        let state = proto::ChannelState {
            channel_id: Some(3),
            name: Some("Lobby".into()),
            ..Default::default()
        };
        let frame = channel_state_frame(
            &state,
            v1::ChannelInfo {
                conversation: vec![7; 16],
                visibility: v1::Visibility::SinceAccess as i32,
                retention_days: 0,
            },
        )
        .unwrap();
        let body = &frame[HEADER_BYTES..];
        assert_eq!(proto::ChannelState::decode(body).unwrap(), state);
        assert_eq!(channel_info(body).unwrap().conversation, vec![7; 16]);
        assert!(channel_info(&state.encode_to_vec()).is_none());
    }

    #[test]
    fn every_error_has_a_fixed_detail() {
        for code in 0..=10 {
            let code = Code::try_from(code).unwrap();
            assert!(!detail(code).is_empty());
            assert_eq!(error(1, code, 0).detail, detail(code));
        }
    }

    #[test]
    fn message_contents_are_redacted_in_debug_output() {
        let send = v1::ChatSend {
            content: "secret words".into(),
            idempotency_key: vec![0xab; 16],
            ..Default::default()
        };
        let message = v1::Message {
            content: "more secrets".into(),
            ..Default::default()
        };
        let edit = v1::ChatEdit {
            content: "edited secret".into(),
            ..Default::default()
        };
        let printed = format!("{send:?} {message:?} {edit:?}");
        for secret in ["secret words", "more secrets", "edited secret", "171, 171"] {
            assert!(!printed.contains(secret), "{secret} leaked: {printed}");
        }
    }

    #[test]
    fn only_client_requests_need_a_capability() {
        assert_eq!(
            required_capability(kind::CHAT_SEND),
            Some(capability::HISTORY)
        );
        assert_eq!(required_capability(kind::CHAT_ACK), None);
        assert_eq!(required_capability(0x4eff), None);
        assert!(client_limit(kind::CHAT_SEND) > MAX_CONTENT_BYTES);
        assert_eq!(client_limit(kind::CHAT_ACK), 64);
    }
}
