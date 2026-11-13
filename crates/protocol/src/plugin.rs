// SPDX-License-Identifier: EUPL-1.2
//! `PluginDataTransmission` (type 26): opaque bytes a client sends to chosen
//! sessions through the server. Nuschl never interprets them; they are bounded
//! here before anything is allocated, and carried as a [`Payload`] whose
//! `Debug` shows only lengths.
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
//! # The encodings accepted here
//!
//! Stricter than protobuf, so that a size limit follows from a receiver limit:
//!
//! - varints are minimal (no redundant trailing zero group); tags, lengths and
//!   the `uint32` fields take at most five bytes and must fit 32 bits, other
//!   varints at most ten;
//! - fields 1, 3 and 4 appear at most once; `dataID` is UTF-8;
//! - receivers may be packed, unpacked or both, in any number of fields, but
//!   a packed field is never empty, and every value counts towards the limit
//!   before anything is deduplicated;
//! - unknown fields are skipped, at most [`MAX_UNKNOWN_FIELDS`] of them in
//!   [`MAX_UNKNOWN_BYTES`] (tags included); groups and wire types 6 and 7 are
//!   refused.
//!
//! The largest body with `n` receivers is therefore [`max_body`]`(n)`: 1,111
//! bytes for the singular fields (a five-byte session, 1,000 bytes of data
//! behind a two-byte length, 100 bytes of ID), seven bytes per receiver (each
//! alone in a packed field; unpacked takes six) and the unknown-field budget.
//! Stock encoders never produce what this refuses; such a message is dropped,
//! not forwarded, which is stricter than Murmur's parser.
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
/// Unknown fields skipped per message: bytes (tags included) and fields.
pub const MAX_UNKNOWN_BYTES: usize = 256;
pub const MAX_UNKNOWN_FIELDS: usize = 16;
/// Data IDs reserved for Nuschl's own features.
pub const NUSCHL_DATA_ID_PREFIX: &str = "nuschl/";

/// Fields 1, 3 and 4 at their largest accepted encoding.
const SINGULAR_BYTES: usize = (1 + 5) + (1 + 2 + MAX_DATA_BYTES) + (1 + 1 + MAX_DATA_ID_BYTES);
/// One receiver at its largest accepted encoding: alone in a packed field.
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
    /// Check `body` against the accepted encodings and at most
    /// `max_receivers` receivers without allocating, then decode it.
    pub fn decode(body: &[u8], max_receivers: usize) -> Result<Self, Error> {
        scan(body, max_receivers)?;
        let message = PluginDataTransmission::decode(body).map_err(|_| Error::Malformed)?;
        let (Some(data), Some(data_id)) = (message.data, message.data_id) else {
            return Err(Error::Missing);
        };
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

/// A minimal varint of at most `max` bytes (at most ten).
fn varint(input: &mut &[u8], max: usize) -> Result<u64, Error> {
    let mut value = 0_u64;
    for index in 0..max.min(10) {
        let (&byte, rest) = input.split_first().ok_or(Error::Malformed)?;
        *input = rest;
        if index == 9 && byte > 1 {
            return Err(Error::Malformed);
        }
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            if byte == 0 && index > 0 {
                return Err(Error::Malformed);
            }
            return Ok(value);
        }
    }
    Err(Error::Malformed)
}

fn uint32(input: &mut &[u8]) -> Result<u32, Error> {
    u32::try_from(varint(input, 5)?).map_err(|_| Error::Malformed)
}

/// A length-delimited value, checked against what remains (as `u64`, so
/// that no length is truncated on 32-bit targets) before advancing.
fn delimited<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], Error> {
    let length = uint32(input)?;
    if u64::from(length) > input.len() as u64 {
        return Err(Error::Malformed);
    }
    let length = usize::try_from(length).map_err(|_| Error::Malformed)?;
    let (value, rest) = input.split_at(length);
    *input = rest;
    Ok(value)
}

fn skip(input: &mut &[u8], bytes: usize) -> Result<(), Error> {
    if bytes > input.len() {
        return Err(Error::Malformed);
    }
    *input = &input[bytes..];
    Ok(())
}

/// The accepted encodings (module docs), without allocating.
fn scan(mut body: &[u8], max_receivers: usize) -> Result<(), Error> {
    let (mut sender, mut data, mut data_id) = (false, false, false);
    let mut receivers = 0_usize;
    let (mut unknown_fields, mut unknown_bytes) = (0_usize, 0_usize);
    let once = |seen: &mut bool| {
        if std::mem::replace(seen, true) {
            Err(Error::Malformed)
        } else {
            Ok(())
        }
    };
    while !body.is_empty() {
        let before = body.len();
        let key = uint32(&mut body)?;
        let (field, wire) = (key >> 3, key & 7);
        match (field, wire) {
            (0, _) => return Err(Error::Malformed),
            (1, 0) => {
                once(&mut sender)?;
                uint32(&mut body)?;
            }
            (2, 0) => {
                uint32(&mut body)?;
                receivers += 1;
            }
            (2, 2) => {
                let mut values = delimited(&mut body)?;
                if values.is_empty() {
                    return Err(Error::Malformed);
                }
                while !values.is_empty() {
                    uint32(&mut values)?;
                    receivers += 1;
                    if receivers > max_receivers {
                        return Err(Error::TooManyReceivers);
                    }
                }
            }
            (3, 2) => {
                once(&mut data)?;
                if delimited(&mut body)?.len() > MAX_DATA_BYTES {
                    return Err(Error::DataTooLarge);
                }
            }
            (4, 2) => {
                once(&mut data_id)?;
                let value = delimited(&mut body)?;
                if value.len() > MAX_DATA_ID_BYTES {
                    return Err(Error::DataIdTooLong);
                }
                std::str::from_utf8(value).map_err(|_| Error::Malformed)?;
            }
            (1..=4, _) => return Err(Error::Malformed),
            (_, 0) => {
                varint(&mut body, 10)?;
            }
            (_, 1) => skip(&mut body, 8)?,
            (_, 2) => {
                delimited(&mut body)?;
            }
            (_, 5) => skip(&mut body, 4)?,
            // Groups (3, 4) and the reserved wire types.
            _ => return Err(Error::Malformed),
        }
        if field > 4 {
            unknown_fields += 1;
            unknown_bytes += before - body.len();
            if unknown_fields > MAX_UNKNOWN_FIELDS || unknown_bytes > MAX_UNKNOWN_BYTES {
                return Err(Error::Malformed);
            }
        }
        if receivers > max_receivers {
            return Err(Error::TooManyReceivers);
        }
    }
    if !data || !data_id {
        return Err(Error::Missing);
    }
    Ok(())
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
    fn singular_fields_appear_once() {
        // Each copy is within its limit, together they are not one value.
        let data = [0x1a, 3, 1, 2, 3];
        let id = [0x22, 1, b'a'];
        let sender = [0x08, 1];
        for duplicate in [&data[..], &id[..], &sender[..]] {
            let mut body = [data.as_slice(), id.as_slice(), sender.as_slice()].concat();
            body.extend(duplicate);
            assert_eq!(PluginData::decode(&body, 0), Err(Error::Malformed));
        }
        let mut body = vec![];
        for _ in 0..2 {
            body.extend([0x1a, 0xe8, 0x07]);
            body.extend([0; MAX_DATA_BYTES]);
        }
        body.extend(id);
        assert_eq!(PluginData::decode(&body, 0), Err(Error::Malformed));
    }

    #[test]
    fn malformed_encodings_are_refused() {
        // Without data and ID: anything the scan let through would be `Missing`.
        let refused: &[&[u8]] = &[
            // An empty packed field, once and many times.
            &[0x12, 0],
            &[0x12, 0, 0x12, 0, 0x12, 0],
            // Non-minimal varints: a value, a tag, a length.
            &[0x10, 0x81, 0x00],
            &[0x90, 0x00, 1],
            &[0x1a, 0x80, 0x00],
            // A six-byte uint32, and a five-byte one above 32 bits.
            &[0x10, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
            &[0x10, 0xff, 0xff, 0xff, 0xff, 0x1f],
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
            // Field zero, groups and the reserved wire types.
            &[0x00, 1],
            &[0x2b, 0x2c],
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
    fn unknown_fields_are_skipped_within_their_budget() {
        let valid = vec![0x1a, 0, 0x22, 0];
        // Varint, fixed64, length-delimited and fixed32 fields.
        let known: Vec<u8> = [
            &[0x28, 0x96, 0x01][..],
            &[0x31, 1, 2, 3, 4, 5, 6, 7, 8],
            &[0x3a, 2, 9, 9],
            &[0x45, 1, 2, 3, 4],
            &[
                0x28, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
            ],
        ]
        .concat();
        assert!(PluginData::decode(&[valid.clone(), known].concat(), 0).is_ok());
        // A field of 256 bytes (tag, two-byte length, 253 bytes) is within the
        // budget, one of 257 is not.
        let field = |length: usize| {
            [
                vec![0x3a, (length as u8 & 0x7f) | 0x80, (length >> 7) as u8],
                vec![0; length],
            ]
            .concat()
        };
        assert_eq!(field(253).len(), MAX_UNKNOWN_BYTES);
        assert!(PluginData::decode(&[valid.clone(), field(253)].concat(), 0).is_ok());
        assert_eq!(
            PluginData::decode(&[valid.clone(), field(254)].concat(), 0),
            Err(Error::Malformed)
        );
        // Sixteen tiny unknown fields are fine, seventeen are a flood.
        let flood = |count: usize| [0x28, 0].repeat(count);
        assert!(PluginData::decode(&[valid.clone(), flood(16)].concat(), 0).is_ok());
        assert_eq!(
            PluginData::decode(&[valid, flood(17)].concat(), 0),
            Err(Error::Malformed)
        );
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
