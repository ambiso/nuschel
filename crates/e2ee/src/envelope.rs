// SPDX-License-Identifier: EUPL-1.2
//! The application payload inside every MLS application message (docs/e2ee.md
//! §6.4): per-sender counters and hash chains, so a server that reorders, withholds
//! or forks history is detected.
//!
//! ```text
//! struct {
//!   uint8  version = 1;
//!   uint8  kind;                 // Kind
//!   opaque device_id[16];        // the sending device (must match the MLS sender)
//!   uint64 counter;              // per (device, conversation), +1 per durable
//!                                // envelope; 0 for typing
//!   opaque prev[32];             // SHA-256 of this device's previous durable
//!                                // envelope (zeros before the first)
//!   opaque seen[32];             // SHA-256 of the newest envelope (any sender) the
//!                                // device had processed when it sent (zeros: none)
//!   opaque idempotency_key[16];  // equals the send's: the server cannot swap it
//!   uint64 sent_at;              // sender clock, milliseconds
//!   uint8  archive_hash<0..32>;  // hash of the archive record (§6.6), empty for now
//!   uint32 body<0..2^16+1024>;
//! } Envelope;
//! ```
//!
//! The hash of an envelope is SHA-256 over a context label and its encoding.

pub const VERSION: u8 = 1;
/// A chat body (UTF-8, as typed) is at most 64 KiB; other kinds get some room.
pub const MAX_BODY: usize = 64 * 1024 + 1024;
const FIXED: usize = 1 + 1 + 16 + 8 + 32 + 32 + 16 + 8 + 1 + 4;
const HASH_CONTEXT: &[u8] = b"nuschel/envelope/v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Chat,
    Edit,
    Delete,
    Reaction,
    File,
    Typing,
    HistoryKey,
    HistoryGrant,
    KtHead,
    Generation,
}

impl Kind {
    fn code(self) -> u8 {
        match self {
            Self::Chat => 1,
            Self::Edit => 2,
            Self::Delete => 3,
            Self::Reaction => 4,
            Self::File => 5,
            Self::Typing => 6,
            Self::HistoryKey => 7,
            Self::HistoryGrant => 8,
            Self::KtHead => 9,
            Self::Generation => 10,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Chat,
            2 => Self::Edit,
            3 => Self::Delete,
            4 => Self::Reaction,
            5 => Self::File,
            6 => Self::Typing,
            7 => Self::HistoryKey,
            8 => Self::HistoryGrant,
            9 => Self::KtHead,
            10 => Self::Generation,
            _ => return None,
        })
    }

    /// Durable envelopes carry a counter and chain; typing indicators do not.
    pub fn durable(self) -> bool {
        self != Self::Typing
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    pub kind: Kind,
    pub device_id: [u8; 16],
    pub counter: u64,
    pub prev: [u8; 32],
    pub seen: [u8; 32],
    pub idempotency_key: [u8; 16],
    pub sent_at: u64,
    pub archive_hash: Vec<u8>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    #[error("malformed envelope")]
    Malformed,
    #[error("unsupported envelope version {0}")]
    Version(u8),
    #[error("unknown envelope kind {0}")]
    Kind(u8),
}

impl Envelope {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FIXED + self.archive_hash.len() + self.body.len());
        out.push(VERSION);
        out.push(self.kind.code());
        out.extend_from_slice(&self.device_id);
        out.extend_from_slice(&self.counter.to_be_bytes());
        out.extend_from_slice(&self.prev);
        out.extend_from_slice(&self.seen);
        out.extend_from_slice(&self.idempotency_key);
        out.extend_from_slice(&self.sent_at.to_be_bytes());
        out.push(self.archive_hash.len().min(32) as u8);
        out.extend_from_slice(&self.archive_hash[..self.archive_hash.len().min(32)]);
        let body = &self.body[..self.body.len().min(MAX_BODY)];
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, EnvelopeError> {
        let mut at = 0usize;
        let mut take = |n: usize| -> Result<&[u8], EnvelopeError> {
            let slice = bytes.get(at..at + n).ok_or(EnvelopeError::Malformed)?;
            at += n;
            Ok(slice)
        };
        let version = take(1)?[0];
        if version != VERSION {
            return Err(EnvelopeError::Version(version));
        }
        let code = take(1)?[0];
        let kind = Kind::from_code(code).ok_or(EnvelopeError::Kind(code))?;
        let array16 = |s: &[u8]| -> [u8; 16] { s.try_into().unwrap_or([0; 16]) };
        let array32 = |s: &[u8]| -> [u8; 32] { s.try_into().unwrap_or([0; 32]) };
        let u64_of = |s: &[u8]| -> u64 { u64::from_be_bytes(s.try_into().unwrap_or([0; 8])) };
        let device_id = array16(take(16)?);
        let counter = u64_of(take(8)?);
        let prev = array32(take(32)?);
        let seen = array32(take(32)?);
        let idempotency_key = array16(take(16)?);
        let sent_at = u64_of(take(8)?);
        let archive_len = take(1)?[0] as usize;
        if archive_len > 32 {
            return Err(EnvelopeError::Malformed);
        }
        let archive_hash = take(archive_len)?.to_vec();
        let body_len = u32::from_be_bytes(take(4)?.try_into().unwrap_or([0; 4])) as usize;
        if body_len > MAX_BODY {
            return Err(EnvelopeError::Malformed);
        }
        let body = take(body_len)?.to_vec();
        if at != bytes.len() {
            return Err(EnvelopeError::Malformed);
        }
        if !kind.durable() && (counter != 0 || prev != [0; 32]) {
            return Err(EnvelopeError::Malformed);
        }
        if kind.durable() && counter == 0 {
            return Err(EnvelopeError::Malformed);
        }
        Ok(Self {
            kind,
            device_id,
            counter,
            prev,
            seen,
            idempotency_key,
            sent_at,
            archive_hash,
            body,
        })
    }

    /// SHA-256 over a context label and the encoding: what `prev` and `seen` name.
    pub fn hash(&self) -> [u8; 32] {
        hash_encoded(&self.encode())
    }
}

/// The hash of an encoded envelope.
pub fn hash_encoded(encoded: &[u8]) -> [u8; 32] {
    let mut input = Vec::with_capacity(HASH_CONTEXT.len() + 1 + encoded.len());
    input.push(HASH_CONTEXT.len() as u8);
    input.extend_from_slice(HASH_CONTEXT);
    input.extend_from_slice(encoded);
    libcrux_sha2::sha256(&input)
}

/// Where an incoming durable envelope stands in its sender's chain (§6.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// `counter = last + 1` and `prev` matches.
    InOrder,
    /// Envelopes from this device are missing (`missing` of them).
    Gap { missing: u64 },
    /// The same counter with different content, or a `prev` that does not match a
    /// known envelope: the server showed different histories.
    Fork,
    /// Already processed (same counter and hash).
    Duplicate,
}

/// The receiver's view of one sender device in one conversation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Chain {
    pub counter: u64,
    pub hash: [u8; 32],
}

impl Chain {
    /// Classifies `envelope` (with hash `hash`) and advances the chain past it unless
    /// it is a fork or duplicate. A gap advances too: the gap is shown, and later
    /// envelopes are judged from here.
    pub fn accept(&mut self, envelope: &Envelope, hash: [u8; 32]) -> Order {
        if envelope.counter <= self.counter {
            return if envelope.counter == self.counter && hash == self.hash {
                Order::Duplicate
            } else {
                Order::Fork
            };
        }
        let order = if envelope.counter == self.counter + 1 {
            if envelope.prev == self.hash {
                Order::InOrder
            } else {
                return Order::Fork;
            }
        } else {
            Order::Gap {
                missing: envelope.counter - self.counter - 1,
            }
        };
        self.counter = envelope.counter;
        self.hash = hash;
        order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(counter: u64, prev: [u8; 32], body: &str) -> Envelope {
        Envelope {
            kind: Kind::Chat,
            device_id: [3; 16],
            counter,
            prev,
            seen: [0; 32],
            idempotency_key: [counter as u8; 16],
            sent_at: 1_800_000_000_000,
            archive_hash: Vec::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn envelopes_round_trip_and_reject_garbage() {
        let envelope = chat(1, [0; 32], "hello");
        let bytes = envelope.encode();
        assert_eq!(Envelope::decode(&bytes), Ok(envelope.clone()));
        assert!(Envelope::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(Envelope::decode(&longer).is_err());
        let mut zero = chat(0, [0; 32], "x").encode();
        assert!(Envelope::decode(&zero).is_err());
        zero[0] = 2;
        assert_eq!(Envelope::decode(&zero), Err(EnvelopeError::Version(2)));
    }

    #[test]
    fn chains_detect_gaps_forks_and_duplicates() {
        let mut chain = Chain::default();
        let first = chat(1, [0; 32], "a");
        assert_eq!(chain.accept(&first, first.hash()), Order::InOrder);
        assert_eq!(chain.accept(&first, first.hash()), Order::Duplicate);
        let other = chat(1, [0; 32], "b");
        assert_eq!(chain.accept(&other, other.hash()), Order::Fork);
        let second = chat(2, first.hash(), "c");
        let third = chat(3, second.hash(), "d");
        assert_eq!(
            chain.accept(&third, third.hash()),
            Order::Gap { missing: 1 }
        );
        let wrong_prev = chat(4, [9; 32], "e");
        assert_eq!(chain.accept(&wrong_prev, wrong_prev.hash()), Order::Fork);
        let fourth = chat(4, third.hash(), "f");
        assert_eq!(chain.accept(&fourth, fourth.hash()), Order::InOrder);
    }
}
