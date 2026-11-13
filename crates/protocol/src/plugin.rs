// SPDX-License-Identifier: EUPL-1.2
//! `PluginDataTransmission` (type 26): opaque bytes a client sends to chosen
//! sessions through the server. Nuschl never interprets them; they are bounded
//! here and carried as a [`Payload`] whose `Debug` shows only lengths.
//!
//! # What a stock server forwards
//!
//! Murmur 1.5.939 (`src/murmur/Messages.cpp`, `msgPluginDataTransmission`;
//! limits in `src/MumbleConstants.h`, rates in `src/murmur/Meta.cpp`), in this
//! order:
//!
//! - only from an authenticated sender; it does not count as activity;
//! - one leaky bucket per sender, `pluginmessagelimit`/`pluginmessageburst`
//!   (default 4 a second, burst 15, both configurable); excess is dropped
//!   silently, also messages that fail the checks below;
//! - dropped unless `data` and `dataID` are both present (present and empty
//!   is valid), `data` is at most 1,000 bytes and `dataID` at most 100 bytes;
//! - dropped if it names more than 20 receivers beyond the users connected;
//! - the sender's session is overwritten with the real one and the receiver
//!   list is cleared, so a receiver never learns who else got it;
//! - receivers are deduplicated, and every one that is connected and
//!   authenticated gets it, whatever its channel, permissions or relation to
//!   the sender (the sender may name itself). Unknown receivers are skipped.
//!
//! There is no acknowledgement and no error to the sender. The server reads
//! everything: TLS protects the hop to the server only. Murmur forwards what
//! the sender's protobuf held, unknown fields included, in frames of up to
//! 8 MiB, so a receiver must survive larger plugin frames than it accepts.
//!
//! Murmur's bucket drains whole tokens and restarts its timer only when one
//! drained, so at its default of 4 a second it is guaranteed to drain only
//! about 2 a second; its shipped `mumble-server.ini` suggests 1 a second with
//! a burst of 5. A sender's local limit is therefore best effort, never a
//! promise that the server keeps what it admitted.
//!
//! `nuschld` forwards the same way, with these differences: it re-encodes the
//! message (unknown fields are not forwarded), it drops what this module
//! refuses (including a `dataID` that is not UTF-8, which Murmur forwards),
//! and an end-to-end server forwards nothing.
//!
//! # Decoding
//!
//! With a limit of `n` receivers, a body larger than [`max_body`]`(n)` is
//! refused before decoding, so what decoding allocates follows from the limit.
//! It adds [`MAX_UNKNOWN_BYTES`] to the largest canonical encoding: 1,111 bytes
//! for the singular fields and seven per receiver (each alone in a packed
//! field). prost then decodes it as leniently as Murmur's parser: the last
//! value wins, a `uint32` keeps its low 32 bits, and unknown fields are
//! skipped. The data, ID and receiver limits are checked after decoding;
//! every receiver counts before anything is deduplicated.
//!
//! # The `nuschl/` namespace
//!
//! Data IDs starting with [`NUSCHL_DATA_ID_PREFIX`] are reserved for Nuschl's
//! own features. The generic interfaces neither send nor deliver them. Any
//! client on a stock server can still send such an ID: the prefix is a naming
//! convention, never evidence of who sent the data.
use crate::{MAX_CONTROL_BYTES, control::PluginDataTransmission};
use prost::Message as _;
use std::fmt;
use thiserror::Error;

/// Mumble's `Plugins::PluginMessage::MAX_DATA_LENGTH`.
pub const MAX_DATA_BYTES: usize = 1000;
/// Mumble's `Plugins::PluginMessage::MAX_DATA_ID_LENGTH`, in bytes.
pub const MAX_DATA_ID_BYTES: usize = 100;
/// Murmur accepts up to this many receivers beyond the users connected.
pub const RECEIVER_SLACK: usize = 20;
/// Room for unknown fields (tags included) in [`max_body`].
pub const MAX_UNKNOWN_BYTES: usize = 256;
/// Data IDs reserved for Nuschl's own features.
pub const NUSCHL_DATA_ID_PREFIX: &str = "nuschl/";

/// Fields 1, 3 and 4 at their largest canonical encoding.
const SINGULAR_BYTES: usize = (1 + 5) + (1 + 2 + MAX_DATA_BYTES) + (1 + 1 + MAX_DATA_ID_BYTES);
/// One receiver at its largest canonical encoding: alone in a packed field.
const RECEIVER_BYTES: usize = 1 + 1 + 5;
const _: () = assert!(SINGULAR_BYTES == 1111);

/// The largest accepted body naming at most `receivers` receivers, capped at
/// what stock Mumble accepts for any frame.
pub fn max_body(receivers: usize) -> usize {
    receivers
        .checked_mul(RECEIVER_BYTES)
        .and_then(|bytes| bytes.checked_add(SINGULAR_BYTES + MAX_UNKNOWN_BYTES))
        .map_or(MAX_CONTROL_BYTES, |bytes| bytes.min(MAX_CONTROL_BYTES))
}

/// Whether `data_id` is in the reserved namespace (exact, case-sensitive prefix).
pub fn is_reserved(data_id: &str) -> bool {
    data_id.starts_with(NUSCHL_DATA_ID_PREFIX)
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("malformed plugin data")]
    Malformed,
    #[error("plugin data without data or a data ID")]
    Missing,
    #[error("plugin data larger than 1,000 bytes")]
    DataTooLarge,
    #[error("plugin data ID longer than 100 bytes")]
    DataIdTooLong,
    #[error("plugin data for too many receivers")]
    TooManyReceivers,
}

/// Why our plugin data was not sent. Not sending is the only outcome known
/// locally: there is no delivery acknowledgement, and an admitted message may
/// still be dropped by the server without a word.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum Refusal {
    #[error("not connected")]
    NotConnected,
    #[error("plugin data is not sent on an end-to-end encrypted server")]
    EndToEnd,
    #[error("the server is too old to relay plugin data")]
    ServerTooOld,
    #[error("data IDs starting with \"nuschl/\" are reserved")]
    Reserved,
    #[error("plugin data needs 1 to {0} receivers")]
    Receivers(usize),
    #[error("a receiver left or was replaced")]
    ReceiverChanged,
    #[error("{0}")]
    Payload(Error),
    #[error("plugin data is sent too fast; retry in {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u64 },
    #[error("{0}")]
    Unavailable(String),
}

/// A data ID and its data, within Mumble's limits. Either may be empty.
#[derive(Clone, PartialEq, Eq)]
pub struct Payload {
    data_id: String,
    data: Vec<u8>,
}

impl Payload {
    pub fn new(data_id: String, data: Vec<u8>) -> Result<Self, Error> {
        if data.len() > MAX_DATA_BYTES {
            return Err(Error::DataTooLarge);
        }
        if data_id.len() > MAX_DATA_ID_BYTES {
            return Err(Error::DataIdTooLong);
        }
        Ok(Self { data_id, data })
    }
    pub fn data_id(&self) -> &str {
        &self.data_id
    }
    pub fn data(&self) -> &[u8] {
        &self.data
    }
    pub fn is_reserved(&self) -> bool {
        is_reserved(&self.data_id)
    }
    /// Bytes held (data ID and data), for budgets.
    pub fn size(&self) -> usize {
        self.data_id.len() + self.data.len()
    }
    pub fn into_parts(self) -> (String, Vec<u8>) {
        (self.data_id, self.data)
    }
}

impl fmt::Debug for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Payload")
            .field("data_id", &format_args!("<{} bytes>", self.data_id.len()))
            .field("data", &format_args!("<{} bytes>", self.data.len()))
            .field("reserved", &self.is_reserved())
            .finish()
    }
}

/// One `PluginDataTransmission`, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginData {
    pub sender: Option<u32>,
    /// As sent: possibly with duplicates.
    pub receivers: Vec<u32>,
    pub payload: Payload,
}

impl PluginData {
    /// Decode `body`, naming at most `max_receivers` receivers. A body larger
    /// than [`max_body`]`(max_receivers)` is refused before decoding.
    pub fn decode(body: &[u8], max_receivers: usize) -> Result<Self, Error> {
        if body.len() > max_body(max_receivers) {
            return Err(Error::Malformed);
        }
        let message = PluginDataTransmission::decode(body).map_err(|_| Error::Malformed)?;
        let (Some(data), Some(data_id)) = (message.data, message.data_id) else {
            return Err(Error::Missing);
        };
        if message.receiver_sessions.len() > max_receivers {
            return Err(Error::TooManyReceivers);
        }
        Ok(Self {
            sender: message.sender_session,
            receivers: message.receiver_sessions,
            payload: Payload::new(data_id, data)?,
        })
    }

    /// The wire message: data and data ID always present.
    pub fn into_message(self) -> PluginDataTransmission {
        let (data_id, data) = self.payload.into_parts();
        PluginDataTransmission {
            sender_session: self.sender,
            receiver_sessions: self.receivers,
            data: Some(data),
            data_id: Some(data_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(receivers: Vec<u32>, data_id: &str, data: &[u8]) -> PluginDataTransmission {
        PluginDataTransmission {
            sender_session: Some(7),
            receiver_sessions: receivers,
            data: Some(data.to_vec()),
            data_id: Some(data_id.into()),
        }
    }

    /// Fields 1, 3 and 4 at their largest: the 1,111-byte singular worst case.
    fn singular() -> Vec<u8> {
        let mut body = vec![0x08, 0xff, 0xff, 0xff, 0xff, 0x0f];
        body.extend([0x1a, 0xe8, 0x07]);
        body.extend([0xab; MAX_DATA_BYTES]);
        body.extend([0x22, 100]);
        body.extend([b'x'; MAX_DATA_ID_BYTES]);
        assert_eq!(body.len(), 1111);
        body
    }
    fn unpacked(receivers: usize) -> Vec<u8> {
        (0..receivers)
            .flat_map(|_| [0x10, 0xff, 0xff, 0xff, 0xff, 0x0f])
            .collect()
    }
    fn split_packed(receivers: usize) -> Vec<u8> {
        (0..receivers)
            .flat_map(|_| [0x12, 5, 0xff, 0xff, 0xff, 0xff, 0x0f])
            .collect()
    }
    fn with_singular(receivers: Vec<u8>) -> Vec<u8> {
        let mut body = singular();
        body.extend(receivers);
        body
    }

    #[test]
    fn round_trips_and_keeps_present_empty_fields() {
        for (receivers, data_id, data) in [
            (vec![1, 2, 2, 300], "positional", &b"\x00\x01payload"[..]),
            (vec![], "", &b""[..]),
            (vec![u32::MAX], "x", &[0xff; MAX_DATA_BYTES][..]),
        ] {
            let body = message(receivers.clone(), data_id, data).encode_to_vec();
            let decoded = PluginData::decode(&body, 8).unwrap();
            assert_eq!(decoded.sender, Some(7));
            assert_eq!(decoded.receivers, receivers);
            assert_eq!(decoded.payload.data_id(), data_id);
            assert_eq!(decoded.payload.data(), data);
            assert_eq!(decoded.clone().into_message().encode_to_vec(), body);
        }
    }

    #[test]
    fn missing_fields_differ_from_empty_ones() {
        let mut no_data = message(vec![1], "id", b"");
        no_data.data = None;
        let mut no_id = message(vec![1], "id", b"x");
        no_id.data_id = None;
        for body in [no_data.encode_to_vec(), no_id.encode_to_vec(), Vec::new()] {
            assert_eq!(PluginData::decode(&body, 8), Err(Error::Missing));
        }
    }

    #[test]
    fn data_and_id_are_bounded_in_bytes() {
        let decode = |id: &str, data: &[u8]| {
            PluginData::decode(&message(vec![], id, data).encode_to_vec(), 0).map(|_| ())
        };
        assert_eq!(decode("x", &[0; MAX_DATA_BYTES]), Ok(()));
        assert_eq!(
            decode("x", &[0; MAX_DATA_BYTES + 1]),
            Err(Error::DataTooLarge)
        );
        assert_eq!(decode(&"x".repeat(MAX_DATA_ID_BYTES), b""), Ok(()));
        assert_eq!(decode(&"x".repeat(101), b""), Err(Error::DataIdTooLong));
        // 34 three-byte characters are 102 bytes: bytes count, not characters.
        assert_eq!(decode(&"€".repeat(34), b""), Err(Error::DataIdTooLong));
        let mut body = vec![0x1a, 0, 0x22, 2, 0xc3, 0x28];
        assert_eq!(PluginData::decode(&body, 0), Err(Error::Malformed));
        body[5] = 0xa9;
        assert!(PluginData::decode(&body, 0).is_ok());
        assert_eq!(
            Payload::new(String::new(), vec![0; 1001]),
            Err(Error::DataTooLarge)
        );
        assert_eq!(
            Payload::new("y".repeat(101), vec![]),
            Err(Error::DataIdTooLong)
        );
    }

    #[test]
    fn worst_case_encodings_fit_the_derived_body_bound() {
        for receivers in [0, 1, 20, 128, 1020] {
            let unpacked = with_singular(unpacked(receivers));
            assert_eq!(unpacked.len(), 6 * receivers + 1111);
            let packed = with_singular(split_packed(receivers));
            assert_eq!(packed.len(), 7 * receivers + 1111);
            assert_eq!(packed.len() + MAX_UNKNOWN_BYTES, max_body(receivers));
            for body in [&unpacked, &packed] {
                let decoded = PluginData::decode(body, receivers).unwrap();
                assert_eq!(decoded.receivers, vec![u32::MAX; receivers]);
            }
        }
        // The worst case of the reviewed example sizes.
        assert_eq!(with_singular(unpacked(16_384)).len(), 99_415);
        assert_eq!(with_singular(split_packed(16_384)).len(), 115_799);
        assert_eq!(max_body(0), 1111 + MAX_UNKNOWN_BYTES);
        assert_eq!(max_body(usize::MAX), MAX_CONTROL_BYTES);
        assert_eq!(max_body(usize::MAX / RECEIVER_BYTES), MAX_CONTROL_BYTES);
        assert_eq!(max_body(1 << 21), MAX_CONTROL_BYTES);
        assert!(max_body(1 << 20) < MAX_CONTROL_BYTES);
    }

    #[test]
    fn receivers_count_across_every_segment_before_deduplication() {
        let limit = 10;
        // Five unpacked and six packed values (one segment of six) are eleven.
        let mut packed = vec![0x12, 6];
        packed.extend([3; 6]);
        let mut body = with_singular([0x10, 3].repeat(5));
        body.extend(&packed);
        assert_eq!(
            PluginData::decode(&body, limit),
            Err(Error::TooManyReceivers)
        );
        assert_eq!(PluginData::decode(&body, 11).unwrap().receivers, [3; 11]);
        // Two packed segments of six: the count does not restart per segment.
        let body = with_singular([packed.clone(), packed].concat());
        assert_eq!(
            PluginData::decode(&body, limit),
            Err(Error::TooManyReceivers)
        );
        // limit + 1 split across both forms, in the worst-case encodings.
        let body = with_singular([unpacked(5), split_packed(6)].concat());
        assert_eq!(
            PluginData::decode(&body, limit),
            Err(Error::TooManyReceivers)
        );
        for receivers in [unpacked(11), split_packed(11)] {
            assert_eq!(
                PluginData::decode(&with_singular(receivers), limit),
                Err(Error::TooManyReceivers)
            );
        }
        // A receiver limit of zero refuses any receiver.
        assert_eq!(
            PluginData::decode(&with_singular(unpacked(1)), 0),
            Err(Error::TooManyReceivers)
        );
    }

    #[test]
    fn malformed_encodings_are_refused() {
        // Without data and ID: anything decoded would be `Missing`.
        let refused: &[&[u8]] = &[
            // Truncated: a varint, a length, packed values, fixed fields.
            &[0x10, 0xff],
            &[0x1a, 5, 1, 2],
            &[0x12, 2, 0xff],
            &[0x12, 1, 0xff],
            &[0x29, 1, 2, 3],
            &[0x2d, 1],
            // A huge declared length is refused, never allocated.
            &[0x1a, 0xff, 0xff, 0xff, 0xff, 0x0f],
            &[0x3a, 0xff, 0xff, 0xff, 0xff, 0x0f],
            // Field zero, a lone end of group and the reserved wire types.
            &[0x00, 1],
            &[0x2c],
            &[0x2e],
            &[0x2f],
            // Known fields with the wrong wire type.
            &[0x0a, 0],
            &[0x15, 0, 0, 0, 0],
            &[0x18, 1],
            &[0x20, 1],
            // An unknown varint of eleven bytes, and a ten-byte one past 64 bits.
            &[
                0x28, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
            ],
            &[
                0x28, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02,
            ],
        ];
        for bad in refused {
            assert_eq!(
                PluginData::decode(bad, 4),
                Err(Error::Malformed),
                "{bad:x?}"
            );
        }
        assert!(PluginData::decode(&[0x1a, 0, 0x22, 0], 0).is_ok());
    }

    #[test]
    fn repeated_and_overlong_values_decode_as_protobuf_does() {
        // Two data fields: the last wins. A receiver of 2^32 + 1: the low 32 bits.
        let body = [
            0x1a, 1, 1, 0x1a, 1, 2, 0x22, 0, 0x10, 0x81, 0x80, 0x80, 0x80, 0x10,
        ];
        let decoded = PluginData::decode(&body, 1).unwrap();
        assert_eq!(decoded.payload.data(), [2]);
        assert_eq!(decoded.receivers, [1]);
    }

    #[test]
    fn unknown_fields_are_skipped_up_to_the_body_bound() {
        let valid = vec![0x1a, 0, 0x22, 0];
        // Varint, fixed64, length-delimited and fixed32 fields, and a group.
        let unknown: Vec<u8> = [
            &[0x28, 0x96, 0x01][..],
            &[0x31, 1, 2, 3, 4, 5, 6, 7, 8],
            &[0x3a, 2, 9, 9],
            &[0x45, 1, 2, 3, 4],
            &[0x2b, 0x28, 1, 0x2c],
        ]
        .concat();
        assert!(PluginData::decode(&[valid, unknown].concat(), 0).is_ok());
        // A field of `length` bytes behind a tag and a two-byte length.
        let field = |length: usize| {
            [
                vec![0x3a, (length as u8 & 0x7f) | 0x80, (length >> 7) as u8],
                vec![0; length],
            ]
            .concat()
        };
        // The worst case plus 256 unknown bytes is `max_body`; one more byte is
        // refused before decoding.
        for receivers in [0, 3] {
            let body = with_singular(split_packed(receivers));
            let largest = [body.clone(), field(253)].concat();
            assert_eq!(largest.len(), max_body(receivers));
            assert!(PluginData::decode(&largest, receivers).is_ok());
            assert_eq!(
                PluginData::decode(&[body, field(254)].concat(), receivers),
                Err(Error::Malformed)
            );
        }
    }

    #[test]
    fn nested_groups_at_the_server_ceiling_are_refused() {
        // nuschld's frame ceiling; prost stops at its recursion limit, so this
        // returns instead of overflowing the stack.
        let receivers = 16_384 + RECEIVER_SLACK;
        let body = vec![0x2b; max_body(receivers)];
        assert_eq!(PluginData::decode(&body, receivers), Err(Error::Malformed));
    }

    #[test]
    fn the_namespace_and_debug_output_reveal_nothing() {
        assert!(is_reserved("nuschl/rtc"));
        assert!(!is_reserved("Nuschl/rtc"));
        assert!(!is_reserved("nuschl"));
        let payload = Payload::new("nuschl/secret-route".into(), b"secret".to_vec()).unwrap();
        let data = PluginData {
            sender: Some(1),
            receivers: vec![2],
            payload,
        };
        let printed = format!("{data:?}");
        assert!(!printed.contains("secret"), "{printed}");
        assert!(printed.contains("<19 bytes>") && printed.contains("<6 bytes>"));
        assert!(printed.contains("reserved: true"));
    }

    #[test]
    fn random_bodies_never_panic() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..50_000 {
            let length = (next() % 96) as usize;
            let mut body: Vec<u8> = (0..length).map(|_| next() as u8).collect();
            if let Some(first) = body.first_mut() {
                *first = [0x08, 0x10, 0x12, 0x1a, 0x22, *first][(next() % 6) as usize];
            }
            if let Ok(data) = PluginData::decode(&body, 4) {
                assert!(data.receivers.len() <= 4);
                assert!(data.payload.size() <= MAX_DATA_BYTES + MAX_DATA_ID_BYTES);
            }
        }
    }
}
