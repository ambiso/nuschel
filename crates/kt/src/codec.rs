// SPDX-License-Identifier: EUPL-1.2
//! A strict binary codec for key-transparency messages.
//!
//! Rules (docs/e2ee.md §5.6): big-endian integers; every variable-length field has an
//! explicit length prefix and a maximum; counts are checked against the bytes that are
//! left before anything is allocated; enum and option tags accept only their defined
//! values; nothing may follow the end of a message. Every value therefore has exactly
//! one encoding, and a parser never allocates more than its input could describe.

use thiserror::Error;

/// Why a message could not be parsed.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("the input is longer than {max} bytes")]
    InputTooLarge { max: usize },
    #[error("the input ends inside `{field}`")]
    Truncated { field: &'static str },
    #[error("{extra} bytes follow the end of the message")]
    TrailingBytes { extra: usize },
    #[error("`{field}` is {len} bytes long, more than {max}")]
    TooLong {
        field: &'static str,
        len: usize,
        max: usize,
    },
    #[error("`{field}` has {count} entries, more than {max}")]
    TooMany {
        field: &'static str,
        count: usize,
        max: usize,
    },
    #[error("`{field}` has an invalid value")]
    InvalidValue { field: &'static str },
    #[error("format version {0} is not supported")]
    UnsupportedVersion(u8),
    #[error("expected message type {expected}, got {got}")]
    UnexpectedType { expected: u8, got: u8 },
}

/// A value that cannot be encoded because a field exceeds its limit. Only reachable
/// through a bug or an unvalidated backend value: every public constructor validates.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("`{field}` is {len} bytes or entries, more than the format allows ({max})")]
pub struct EncodeError {
    pub field: &'static str,
    pub len: usize,
    pub max: usize,
}

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Starts parsing a whole message of at most `max` bytes.
    pub(crate) fn new(buf: &'a [u8], max: usize) -> Result<Self, DecodeError> {
        if buf.len() > max {
            return Err(DecodeError::InputTooLarge { max });
        }
        Ok(Self { buf, pos: 0 })
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub(crate) fn take(&mut self, n: usize, field: &'static str) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::Truncated { field });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub(crate) fn array<const N: usize>(
        &mut self,
        field: &'static str,
    ) -> Result<[u8; N], DecodeError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N, field)?);
        Ok(out)
    }

    pub(crate) fn u8(&mut self, field: &'static str) -> Result<u8, DecodeError> {
        Ok(self.array::<1>(field)?[0])
    }

    pub(crate) fn u16(&mut self, field: &'static str) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.array(field)?))
    }

    pub(crate) fn u32(&mut self, field: &'static str) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.array(field)?))
    }

    pub(crate) fn u64(&mut self, field: &'static str) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.array(field)?))
    }

    /// `opaque field<0..2^8-1>` with an upper bound.
    pub(crate) fn bytes_u8(
        &mut self,
        field: &'static str,
        max: usize,
    ) -> Result<&'a [u8], DecodeError> {
        let len = usize::from(self.u8(field)?);
        self.bounded(field, len, max)
    }

    /// `opaque field<0..2^16-1>` with an upper bound.
    pub(crate) fn bytes_u16(
        &mut self,
        field: &'static str,
        max: usize,
    ) -> Result<&'a [u8], DecodeError> {
        let len = usize::from(self.u16(field)?);
        self.bounded(field, len, max)
    }

    /// `opaque field<0..2^32-1>` with an upper bound.
    pub(crate) fn bytes_u32(
        &mut self,
        field: &'static str,
        max: usize,
    ) -> Result<&'a [u8], DecodeError> {
        let len = usize::try_from(self.u32(field)?).unwrap_or(usize::MAX);
        self.bounded(field, len, max)
    }

    fn bounded(
        &mut self,
        field: &'static str,
        len: usize,
        max: usize,
    ) -> Result<&'a [u8], DecodeError> {
        if len > max {
            return Err(DecodeError::TooLong { field, len, max });
        }
        self.take(len, field)
    }

    /// A `u8`-prefixed entry count, at most `max`, each entry at least `min_size` bytes.
    pub(crate) fn count_u8(
        &mut self,
        field: &'static str,
        max: usize,
        min_size: usize,
    ) -> Result<usize, DecodeError> {
        let count = usize::from(self.u8(field)?);
        self.check_count(field, count, max, min_size)
    }

    /// A `u16`-prefixed entry count, at most `max`, each entry at least `min_size` bytes.
    pub(crate) fn count_u16(
        &mut self,
        field: &'static str,
        max: usize,
        min_size: usize,
    ) -> Result<usize, DecodeError> {
        let count = usize::from(self.u16(field)?);
        self.check_count(field, count, max, min_size)
    }

    /// A `u32`-prefixed entry count, at most `max`, each entry at least `min_size` bytes.
    pub(crate) fn count_u32(
        &mut self,
        field: &'static str,
        max: usize,
        min_size: usize,
    ) -> Result<usize, DecodeError> {
        let count = usize::try_from(self.u32(field)?).unwrap_or(usize::MAX);
        self.check_count(field, count, max, min_size)
    }

    fn check_count(
        &self,
        field: &'static str,
        count: usize,
        max: usize,
        min_size: usize,
    ) -> Result<usize, DecodeError> {
        if count > max {
            return Err(DecodeError::TooMany { field, count, max });
        }
        // Never reserve memory for entries the remaining input cannot hold.
        if count.saturating_mul(min_size) > self.remaining() {
            return Err(DecodeError::Truncated { field });
        }
        Ok(count)
    }

    /// An option tag: 0 for absent, 1 for present.
    pub(crate) fn present(&mut self, field: &'static str) -> Result<bool, DecodeError> {
        match self.u8(field)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::InvalidValue { field }),
        }
    }

    /// Checks the two-byte header `version, type` of a top-level message.
    pub(crate) fn header(&mut self, expected_type: u8) -> Result<(), DecodeError> {
        let version = self.u8("version")?;
        if version != crate::FORMAT_VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        let got = self.u8("type")?;
        if got != expected_type {
            return Err(DecodeError::UnexpectedType {
                expected: expected_type,
                got,
            });
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<(), DecodeError> {
        match self.remaining() {
            0 => Ok(()),
            extra => Err(DecodeError::TrailingBytes { extra }),
        }
    }
}

#[derive(Default)]
pub(crate) struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn header(&mut self, message_type: u8) {
        self.u8(crate::FORMAT_VERSION);
        self.u8(message_type);
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub(crate) fn bytes_u8(
        &mut self,
        field: &'static str,
        bytes: &[u8],
        max: usize,
    ) -> Result<(), EncodeError> {
        let len = checked_len(field, bytes.len(), max.min(usize::from(u8::MAX)))?;
        self.u8(len as u8);
        self.raw(bytes);
        Ok(())
    }

    pub(crate) fn bytes_u16(
        &mut self,
        field: &'static str,
        bytes: &[u8],
        max: usize,
    ) -> Result<(), EncodeError> {
        let len = checked_len(field, bytes.len(), max.min(usize::from(u16::MAX)))?;
        self.u16(len as u16);
        self.raw(bytes);
        Ok(())
    }

    pub(crate) fn bytes_u32(
        &mut self,
        field: &'static str,
        bytes: &[u8],
        max: usize,
    ) -> Result<(), EncodeError> {
        let len = checked_len(field, bytes.len(), max.min(u32::MAX as usize))?;
        self.u32(len as u32);
        self.raw(bytes);
        Ok(())
    }

    pub(crate) fn count_u8(
        &mut self,
        field: &'static str,
        count: usize,
        max: usize,
    ) -> Result<(), EncodeError> {
        let count = checked_len(field, count, max.min(usize::from(u8::MAX)))?;
        self.u8(count as u8);
        Ok(())
    }

    pub(crate) fn count_u16(
        &mut self,
        field: &'static str,
        count: usize,
        max: usize,
    ) -> Result<(), EncodeError> {
        let count = checked_len(field, count, max.min(usize::from(u16::MAX)))?;
        self.u16(count as u16);
        Ok(())
    }

    pub(crate) fn count_u32(
        &mut self,
        field: &'static str,
        count: usize,
        max: usize,
    ) -> Result<(), EncodeError> {
        let count = checked_len(field, count, max.min(u32::MAX as usize))?;
        self.u32(count as u32);
        Ok(())
    }

    pub(crate) fn present(&mut self, present: bool) {
        self.u8(u8::from(present));
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.buf
    }

    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }
}

fn checked_len(field: &'static str, len: usize, max: usize) -> Result<usize, EncodeError> {
    if len > max {
        Err(EncodeError { field, len, max })
    } else {
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_checked_against_the_remaining_input_before_allocation() {
        // Claims 2^32-1 entries of 68 bytes in a 10-byte input.
        let input = [0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0];
        let mut r = Reader::new(&input, 64).unwrap();
        assert_eq!(
            r.count_u32("nodes", usize::MAX, 68),
            Err(DecodeError::Truncated { field: "nodes" })
        );
    }

    #[test]
    fn option_tags_and_trailing_bytes_are_strict() {
        let mut r = Reader::new(&[2], 8).unwrap();
        assert_eq!(
            r.present("opt"),
            Err(DecodeError::InvalidValue { field: "opt" })
        );
        let mut r = Reader::new(&[1, 0], 8).unwrap();
        assert!(r.present("opt").unwrap());
        assert_eq!(r.finish(), Err(DecodeError::TrailingBytes { extra: 1 }));
    }

    #[test]
    fn oversized_input_is_rejected_up_front() {
        assert_eq!(
            Reader::new(&[0; 9], 8).err(),
            Some(DecodeError::InputTooLarge { max: 8 })
        );
    }

    #[test]
    fn writer_refuses_fields_over_their_limit() {
        let mut w = Writer::new();
        assert!(w.bytes_u8("x", &[0; 256], 1000).is_err());
        assert!(w.bytes_u16("x", &[0; 10], 9).is_err());
        assert_eq!(w.len(), 0);
    }
}
