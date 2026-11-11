// SPDX-License-Identifier: EUPL-1.2
//! The Nuschl protocol extension (docs/extension.md): message types in the
//! reserved block 0x4E00–0x4EFF, the hello in field 7200 of `Version`, channel
//! properties in field 7200 of `ChannelState`, per-type limits before and after
//! decoding, and errors with fixed details. Pure data; no I/O.
use crate::{HEADER_BYTES, MAX_CONTROL_BYTES, control as proto};
use bytes::{Bytes, BytesMut};
use prost::Message as _;

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/nuschl.v1.rs"));
    mod redacted;
}

pub mod version;

use v1::error::Code;

/// TLS ALPN identifiers. A Nuschl client offers both in one handshake; stock
/// Murmur ignores ALPN, a Nuschl server selects `nuschl/1`.
pub const ALPN: &[u8] = b"nuschl/1";
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
    /// Key transparency (docs/e2ee.md §5): the log descriptor in the hello,
    /// binding publication, lookups, histories, audits and heads.
    pub const KT: &str = "kt/1";
    /// MLS (docs/e2ee.md §6): the delivery service, key packages, Welcome
    /// mailboxes. Needs `kt/1`; sticky like every capability.
    pub const MLS: &str = "mls/1";
    /// End-to-end mode (docs/e2ee.md §10, answer 7): a server-level switch. Every
    /// conversation is an MLS group and the server takes no plaintext text; needs
    /// `mls/1` and `kt/1`. Sticky like every capability, so a server that offered
    /// it cannot quietly become a compatible one. Without it there is no end-to-end
    /// encryption at all, and the extension's extras are plaintext.
    pub const E2EE: &str = "e2ee/1";
    /// Witnessed key transparency; needs `kt/1` (docs/witness.md).
    pub const KT_WITNESS: &str = "kt-witness/1";
    /// Share links (docs/web.md): tokens that open a channel.
    pub const SHARE: &str = "share/1";
    pub const PRIVACY: &str = "privacy/1";
    pub const KNOCK: &str = "knock/1";
    pub const ALL: [&str; 10] = [
        HISTORY, SUBSCRIBE, TYPING, KT, MLS, E2EE, KT_WITNESS, SHARE, PRIVACY, KNOCK,
    ];
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
    pub const KT_STATUS: u16 = 0x4e14;
    pub const KT_STATUS_REPLY: u16 = 0x4e15;
    pub const KT_PUBLISH: u16 = 0x4e16;
    pub const KT_PUBLISHED: u16 = 0x4e17;
    pub const KT_LOOKUP: u16 = 0x4e18;
    pub const KT_LOOKUP_REPLY: u16 = 0x4e19;
    pub const KT_HISTORY: u16 = 0x4e1a;
    pub const KT_HISTORY_REPLY: u16 = 0x4e1b;
    pub const KT_AUDIT: u16 = 0x4e1c;
    pub const KT_AUDIT_REPLY: u16 = 0x4e1d;
    pub const KT_HEAD: u16 = 0x4e1e;
    pub const KT_HEAD_REPLY: u16 = 0x4e1f;
    pub const MLS_KEY_PACKAGE_UPLOAD: u16 = 0x4e20;
    pub const MLS_KEY_PACKAGE_STATUS: u16 = 0x4e21;
    pub const MLS_KEY_PACKAGE_CLAIM: u16 = 0x4e22;
    pub const MLS_KEY_PACKAGES: u16 = 0x4e23;
    pub const MLS_COMMIT: u16 = 0x4e24;
    pub const MLS_ACK: u16 = 0x4e25;
    pub const MLS_SEND: u16 = 0x4e26;
    pub const MLS_WELCOME_PUSH: u16 = 0x4e27;
    pub const MLS_WELCOME_FETCH: u16 = 0x4e28;
    pub const MLS_WELCOMES: u16 = 0x4e29;
    pub const MLS_WELCOME_ACK: u16 = 0x4e2a;
    pub const MLS_JOIN_INFO_REQUEST: u16 = 0x4e2b;
    pub const MLS_JOIN_INFO: u16 = 0x4e2c;
    pub const PRIVACY: u16 = 0x4e36;
    pub const KNOCK_REQUEST: u16 = 0x4e60;
    pub const KNOCK_PENDING: u16 = 0x4e61;
    pub const KNOCK_NOTICE: u16 = 0x4e62;
    pub const KNOCK_DECIDE: u16 = 0x4e63;
    pub const KNOCK_ADMISSION: u16 = 0x4e64;
    pub const KNOCK_CANCEL: u16 = 0x4e65;
    /// `share/1`, 0x4E90–0x4E97 (0x4E94–0x4E97 reserved).
    pub const SHARE_LINK_CREATE: u16 = 0x4e90;
    pub const SHARE_LINK: u16 = 0x4e91;
    pub const SHARE_LINK_REVOKE: u16 = 0x4e92;
    pub const SHARE_LINK_RESOLVE: u16 = 0x4e93;
    /// Channels (phase 4c, docs/e2ee.md §6.2): 0x4E2D–0x4E2F and 0x4E80–0x4E8F.
    pub const MLS_TASK: u16 = 0x4e2d;
    pub const MLS_OPENED: u16 = 0x4e2e;
}

/// Requests in flight per connection.
pub const MAX_OUTSTANDING: usize = 16;
/// Message text, in bytes of UTF-8.
pub const MAX_CONTENT_BYTES: usize = 64 * 1024;
/// A stored stock client's message (Mumble HTML, which may carry inline
/// images up to the server's image limit); larger ones are not stored.
pub const MAX_LEGACY_CONTENT_BYTES: usize = 1024 * 1024;
pub const MAX_PAGE_MESSAGES: usize = 100;
/// Content bytes in one history page; a larger message comes alone.
pub const MAX_PAGE_BYTES: usize = 256 * 1024;
pub const MAX_SUBSCRIBE_PER_MESSAGE: usize = 64;
pub const MAX_SUBSCRIPTIONS: usize = 256;
pub const MAX_DIRECTS: usize = 1024;
pub const MAX_CAPABILITIES: usize = 32;
pub const MAX_CAPABILITY_BYTES: usize = 64;
pub const MAX_NAME_BYTES: usize = 512;
/// An access token in a share link (Mumble's own limit is the message size).
pub const MAX_ACCESS_TOKEN_BYTES: usize = 128;
/// A web host name in a `ShareLink`.
pub const MAX_WEB_HOST_BYTES: usize = 253;
/// A key-transparency log descriptor in the hello.
pub const MAX_KT_LOG_BYTES: usize = 512;
/// Device certificates and revocations in one `KtRecord`.
pub const MAX_KT_DEVICES: usize = 16;
pub const MAX_KT_REVOCATIONS: usize = 32;
/// Any one certificate, statement or root key in a `KtRecord` (a device
/// certificate is at most 4764 bytes, a device-signing certificate 5439).
pub const MAX_KT_CERTIFICATE_BYTES: usize = 8 * 1024;
/// A `KtPublish` from a client.
pub const MAX_KT_PUBLISH_BYTES: usize = 256 * 1024;
/// A nuschl-kt `WitnessedHead` in a reply (`kt-witness/1`).
pub const MAX_KT_WITNESSED_BYTES: usize = 72 * 1024;
/// A lookup reply: up to three proofs (nuschl-kt allows 256 KiB each), the
/// record and the witnessed checkpoint.
pub const MAX_KT_LOOKUP_REPLY_BYTES: usize = 1536 * 1024;
/// History and audit replies; a server answers `TOO_LARGE` beyond this, and
/// clients audit shorter spans.
pub const MAX_KT_REPLY_BYTES: usize = 6 * 1024 * 1024;
/// `mls/1` (docs/e2ee.md §6.3). One key package (an X-Wing one is about 2.8 KB).
pub const MAX_MLS_KEY_PACKAGE_BYTES: usize = 4096;
/// One-time key packages per upload, and kept per device.
pub const MAX_MLS_UPLOAD_PACKAGES: usize = 16;
pub const MAX_MLS_DEVICE_PACKAGES: usize = 50;
/// Devices per account in one claim (the key-transparency roster's limit).
pub const MAX_MLS_CLAIM_PACKAGES: usize = MAX_KT_DEVICES;
/// A commit or proposal in the stream (a 1000-member X-Wing add is 2.8 MB: larger
/// groups than direct conversations need more, with channels).
pub const MAX_MLS_HANDSHAKE_BYTES: usize = 2 * 1024 * 1024;
/// An application message: the 64 KiB text, the envelope and MLS framing.
pub const MAX_MLS_APPLICATION_BYTES: usize = MAX_CONTENT_BYTES + 16 * 1024;
pub const MAX_MLS_WELCOME_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_MLS_GROUP_INFO_BYTES: usize = 256 * 1024;
/// A ratchet tree (1.5 MB at 1000 X-Wing members).
pub const MAX_MLS_TREE_BYTES: usize = 2 * 1024 * 1024;
/// Member accounts in one `MlsJoinInfo`.
pub const MAX_MLS_JOIN_ACCOUNTS: usize = 4096;
/// Welcomes in one `MlsWelcomes` and ids in one `MlsWelcomeAck`.
pub const MAX_MLS_WELCOMES: usize = 32;
/// `MlsCommit` from a client: commit, GroupInfo, Welcome and a new group's
/// GroupInfo and tree.
pub const MAX_MLS_COMMIT_FRAME: usize = MAX_MLS_HANDSHAKE_BYTES
    + 2 * MAX_MLS_GROUP_INFO_BYTES
    + MAX_MLS_WELCOME_BYTES
    + MAX_MLS_TREE_BYTES
    + 1024;
/// The largest stored row of any format, as it travels (a page or event with it
/// comes alone if it exceeds the page's byte cap).
pub const MAX_ROW_BYTES: usize = MAX_MLS_HANDSHAKE_BYTES;

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
        KT_STATUS | KT_PUBLISH | KT_LOOKUP | KT_HISTORY | KT_AUDIT | KT_HEAD => {
            Some(capability::KT)
        }
        MLS_KEY_PACKAGE_UPLOAD
        | MLS_KEY_PACKAGE_CLAIM
        | MLS_COMMIT
        | MLS_SEND
        | MLS_WELCOME_FETCH
        | MLS_WELCOME_ACK
        | MLS_JOIN_INFO_REQUEST
        | MLS_OPENED => Some(capability::MLS),
        SHARE_LINK_CREATE | SHARE_LINK_REVOKE | SHARE_LINK_RESOLVE => Some(capability::SHARE),
        PRIVACY => Some(capability::PRIVACY),
        KNOCK_REQUEST | KNOCK_DECIDE | KNOCK_CANCEL => Some(capability::KNOCK),
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
        PRIVACY => 16,
        CHAT_SEND | CHAT_EDIT => MAX_CONTENT_BYTES + 256,
        SUBSCRIBE | UNSUBSCRIBE => MAX_SUBSCRIBE_PER_MESSAGE * 18 + 32,
        HISTORY_REQUEST => 128,
        KT_PUBLISH => MAX_KT_PUBLISH_BYTES,
        MLS_KEY_PACKAGE_UPLOAD => {
            (MAX_MLS_UPLOAD_PACKAGES + 1) * (MAX_MLS_KEY_PACKAGE_BYTES + 8) + 64
        }
        MLS_COMMIT => MAX_MLS_COMMIT_FRAME,
        MLS_SEND => 2 * MAX_MLS_APPLICATION_BYTES + 256,
        MLS_WELCOME_ACK => MAX_MLS_WELCOMES * 20 + 64,
        SHARE_LINK_CREATE => MAX_ACCESS_TOKEN_BYTES + 64,
        _ => 64,
    }
}

/// Wire-size cap for a frame from a server (the client's side).
pub fn server_limit(kind: u16) -> usize {
    use kind::*;
    match kind {
        // A message larger than the byte cap comes alone.
        HISTORY_PAGE => MAX_PAGE_BYTES + MAX_ROW_BYTES + MAX_PAGE_MESSAGES * 1024,
        CHAT_EVENT => MAX_ROW_BYTES + 2048,
        MLS_KEY_PACKAGES => MAX_MLS_CLAIM_PACKAGES * (MAX_MLS_KEY_PACKAGE_BYTES + 8) + 64,
        MLS_WELCOME_PUSH => MAX_MLS_WELCOME_BYTES + 256,
        MLS_WELCOMES => 4 * MAX_MLS_WELCOME_BYTES,
        MLS_JOIN_INFO => {
            MAX_MLS_GROUP_INFO_BYTES + MAX_MLS_TREE_BYTES + MAX_MLS_JOIN_ACCOUNTS * 20 + 256
        }
        SUBSCRIBED => MAX_SUBSCRIBE_PER_MESSAGE * 64 + 32,
        DIRECT_LIST => MAX_DIRECTS * (MAX_NAME_BYTES + 96) + 32,
        DIRECT_OPENED => MAX_NAME_BYTES + 128,
        KT_LOOKUP_REPLY => MAX_KT_LOOKUP_REPLY_BYTES,
        KT_HISTORY_REPLY | KT_AUDIT_REPLY => MAX_KT_REPLY_BYTES,
        KT_PUBLISHED => 8 * 1024,
        KT_STATUS_REPLY => MAX_KT_WITNESSED_BYTES + 2048,
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
        Code::StaleEpoch => "The conversation moved on; catch up and try again.",
        Code::ConfirmationRequired => {
            "This change removes stored messages; confirm it to go ahead."
        }
    }
}

pub fn error(request_id: u64, code: Code, retry_after_ms: u64) -> v1::Error {
    v1::Error {
        request_id,
        code: code as i32,
        detail: detail(code).into(),
        retry_after_ms,
        epoch: 0,
        head: 0,
    }
}

/// `STALE_EPOCH` with the group's epoch and the conversation's head.
pub fn stale_epoch(request_id: u64, epoch: u64, head: u64) -> v1::Error {
    v1::Error {
        epoch,
        head,
        ..error(request_id, Code::StaleEpoch, 0)
    }
}

/// Whether a row's format carries an MLS message.
pub fn is_mls_format(format: i32) -> bool {
    format == v1::Format::MlsApplication as i32 || format == v1::Format::MlsHandshake as i32
}

/// Why a hello is refused. Any refusal fails the connection closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HelloError {
    #[error("the Nuschl hello is malformed")]
    Malformed,
    #[error("the Nuschl hello has too many or invalid capabilities")]
    Capabilities,
    #[error("the peer speaks an unsupported Nuschl protocol version")]
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
    if !valid_kt_log(&hello) || hello.client_version.len() > version::MAX_CLIENT_VERSION_BYTES {
        return Err(HelloError::Malformed);
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
    let extension = v1::ChannelStateExtension { nuschl: Some(info) };
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
    let info = v1::ChannelStateExtension::decode(body).ok()?.nuschl?;
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
    let limit = if message.format == v1::Format::LegacyHtml as i32 {
        MAX_LEGACY_CONTENT_BYTES
    } else {
        MAX_CONTENT_BYTES
    };
    let mls = if is_mls_format(message.format) {
        message.content.is_empty()
            && message.mls.len() <= MAX_MLS_HANDSHAKE_BYTES
            && message.archive.len() <= MAX_MLS_APPLICATION_BYTES
    } else {
        message.mls.is_empty() && message.archive.is_empty()
    };
    mls && message.content.len() <= limit
        && message.author.as_ref().is_none_or(|author| {
            author.name.len() <= MAX_NAME_BYTES
                && (author.account.is_empty() || author.account.len() == 16)
        })
}

/// Bounds of a `KtRecord` (from either side).
pub fn valid_kt_record(record: &v1::KtRecord) -> bool {
    let bounded = |bytes: &Vec<u8>| bytes.len() <= MAX_KT_CERTIFICATE_BYTES;
    record.devices.len() <= MAX_KT_DEVICES
        && record.revocations.len() <= MAX_KT_REVOCATIONS
        && bounded(&record.device_signing_certificate)
        && bounded(&record.statement)
        && record.devices.iter().all(bounded)
        && record.revocations.iter().all(bounded)
}

/// Bounds of a hello's log descriptor.
pub fn valid_kt_log(hello: &v1::Hello) -> bool {
    hello.kt_log.len() <= MAX_KT_LOG_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello_with(capabilities: &[&str]) -> v1::Hello {
        v1::Hello {
            protocol: PROTOCOL,
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
            policy: None,
            kt_log: Vec::new(),
            client_version: String::new(),
            relay_only: false,
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
                ..Default::default()
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
        for code in (0..=12).filter_map(|code| Code::try_from(code).ok()) {
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
        for request in [
            kind::KT_STATUS,
            kind::KT_PUBLISH,
            kind::KT_LOOKUP,
            kind::KT_HISTORY,
            kind::KT_AUDIT,
            kind::KT_HEAD,
        ] {
            assert_eq!(required_capability(request), Some(capability::KT));
        }
        assert_eq!(required_capability(kind::KT_LOOKUP_REPLY), None);
        assert_eq!(client_limit(kind::KT_LOOKUP), 64);
        assert!(server_limit(kind::KT_AUDIT_REPLY) < MAX_CONTROL_BYTES);
    }

    #[test]
    fn kt_records_are_bounded() {
        let certificate = vec![0; 4764];
        let mut record = v1::KtRecord {
            device_signing_certificate: vec![0; 5439],
            devices: vec![certificate.clone(); MAX_KT_DEVICES],
            revocations: vec![vec![0; 3439]; MAX_KT_REVOCATIONS],
            statement: vec![0; 3467],
        };
        assert!(valid_kt_record(&record));
        // The largest valid publish fits its frame limit.
        let publish = v1::KtPublish {
            request_id: u64::MAX,
            root: vec![0; 1986],
            record: Some(record.clone()),
        };
        assert!(publish.encoded_len() <= client_limit(kind::KT_PUBLISH));
        record.devices.push(certificate);
        assert!(!valid_kt_record(&record));
        record.devices.pop();
        record.statement = vec![0; MAX_KT_CERTIFICATE_BYTES + 1];
        assert!(!valid_kt_record(&record));
    }
}
