// SPDX-License-Identifier: EUPL-1.2
//! A strict, canonical binary codec in the style of the TLS presentation language
//! (RFC 8446 §3).
//!
//! - Integers are big-endian (`uint8`, `uint16`, `uint32`, `uint64`).
//! - `opaque x[N]` is exactly N bytes; N is fixed by the format or by an algorithm id
//!   that precedes the field.
//! - `opaque x<a..b>` is a length-prefixed byte string; the prefix is 1, 2, 3 or 4 bytes
//!   (the smallest that holds `b`), and lengths outside `a..=b` are rejected before
//!   anything is allocated.
//! - Optional fields are a presence byte, exactly 0 or 1, followed by the field.
//! - Every top-level structure starts with `uint8 kind; uint8 version;`, and decoders
//!   reject other kinds, unknown versions, truncation and trailing bytes.
//!
//! There is exactly one encoding of every value, so decoding then encoding returns the
//! input (the fuzz target checks this), and signatures are computed over re-encoded
//! fields without keeping the received bytes.

use zeroize::Zeroizing;

use crate::Error;

/// The format version of every structure this crate defines.
pub(crate) const VERSION: u8 = 1;

/// Structure kinds: the first byte of every top-level encoding.
pub(crate) mod kind {
    pub const DEVICE_SIGNING_CERTIFICATE: u8 = 0x01;
    pub const DEVICE_CERTIFICATE: u8 = 0x02;
    pub const DEVICE_CREDENTIAL: u8 = 0x03;
    pub const DEVICE_REVOCATION: u8 = 0x04;
    pub const SEALED_TO_DEVICE: u8 = 0x05;
    pub const ACCOUNT_STATEMENT: u8 = 0x06;
    pub const LINK_PAKE: u8 = 0x10;
    pub const LINK_DEVICE_KEYS: u8 = 0x11;
    pub const LINK_BUNDLE: u8 = 0x12;
    pub const PROVISIONING_BUNDLE: u8 = 0x13;
    pub const SEALED: u8 = 0x20;
    pub const DEVICE_KEYSTORE: u8 = 0x21;
    pub const ROOT_SECRET: u8 = 0x22;
    pub const IDENTITY_EXPORT: u8 = 0x23;
    pub const EXPORT_SECRETS: u8 = 0x24;
}

/// Writes an encoding. Lengths are checked by the types that call it (their
/// constructors and decoders enforce the limits), so a violation is a bug.
pub(crate) struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// A writer for secret material: the buffer is allocated once with `capacity`, so
    /// no reallocation leaves copies behind, and handed out zeroizing.
    pub fn secret(capacity: usize) -> SecretWriter {
        SecretWriter {
            inner: Writer {
                buf: Vec::with_capacity(capacity),
            },
            capacity,
        }
    }

    pub fn header(&mut self, kind: u8) {
        self.u8(kind);
        self.u8(VERSION);
    }

    pub fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    pub fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// A fixed-length field.
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn opaque8(&mut self, bytes: &[u8]) {
        let len = u8::try_from(bytes.len()).expect("checked by the caller's type");
        self.u8(len);
        self.bytes(bytes);
    }

    pub fn opaque16(&mut self, bytes: &[u8]) {
        let len = u16::try_from(bytes.len()).expect("checked by the caller's type");
        self.u16(len);
        self.bytes(bytes);
    }

    pub fn opaque24(&mut self, bytes: &[u8]) {
        assert!(bytes.len() < 1 << 24, "checked by the caller's type");
        self.bytes(&(bytes.len() as u32).to_be_bytes()[1..]);
        self.bytes(bytes);
    }

    pub fn presence(&mut self, present: bool) {
        self.u8(u8::from(present));
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// See [`Writer::secret`].
pub(crate) struct SecretWriter {
    inner: Writer,
    capacity: usize,
}

impl std::ops::Deref for SecretWriter {
    type Target = Writer;
    fn deref(&self) -> &Writer {
        &self.inner
    }
}

impl std::ops::DerefMut for SecretWriter {
    fn deref_mut(&mut self) -> &mut Writer {
        &mut self.inner
    }
}

impl SecretWriter {
    pub fn finish(mut self) -> Zeroizing<Vec<u8>> {
        debug_assert!(
            self.inner.buf.capacity() == self.capacity,
            "a secret buffer was reallocated; raise its capacity"
        );
        Zeroizing::new(std::mem::take(&mut self.inner.buf))
    }
}

impl Drop for SecretWriter {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.inner.buf.zeroize();
    }
}

/// Reads an encoding, failing on the first violation.
pub(crate) struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    /// Starts a top-level decoder: rejects inputs above `max_len` before parsing.
    pub fn new(bytes: &'a [u8], max_len: usize) -> Result<Self, Error> {
        if bytes.len() > max_len {
            return Err(Error::TooLong);
        }
        Ok(Self { rest: bytes })
    }

    /// `uint8 kind; uint8 version;` with the expected kind and version 1.
    pub fn header(&mut self, kind: u8) -> Result<(), Error> {
        let found = self.u8()?;
        if found != kind {
            return Err(Error::WrongKind(found));
        }
        match self.u8()? {
            VERSION => Ok(()),
            other => Err(Error::UnsupportedVersion(other)),
        }
    }

    pub fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        if self.rest.len() < len {
            return Err(Error::Truncated);
        }
        let (head, rest) = self.rest.split_at(len);
        self.rest = rest;
        Ok(head)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }

    pub fn boxed<const N: usize>(&mut self) -> Result<Box<[u8; N]>, Error> {
        Ok(self
            .take(N)?
            .to_vec()
            .into_boxed_slice()
            .try_into()
            .expect("length checked"))
    }

    pub fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.array::<1>()?[0])
    }

    pub fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn bounded(&mut self, len: usize, min: usize, max: usize) -> Result<&'a [u8], Error> {
        if len < min || len > max {
            return Err(Error::Malformed("length out of range"));
        }
        self.take(len)
    }

    pub fn opaque8(&mut self, min: usize, max: usize) -> Result<&'a [u8], Error> {
        let len = usize::from(self.u8()?);
        self.bounded(len, min, max)
    }

    pub fn opaque16(&mut self, min: usize, max: usize) -> Result<&'a [u8], Error> {
        let len = usize::from(self.u16()?);
        self.bounded(len, min, max)
    }

    pub fn opaque24(&mut self, min: usize, max: usize) -> Result<&'a [u8], Error> {
        let [a, b, c] = self.array()?;
        let len = u32::from_be_bytes([0, a, b, c]) as usize;
        self.bounded(len, min, max)
    }

    /// A presence byte: exactly 0 or 1.
    pub fn presence(&mut self) -> Result<bool, Error> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::Malformed("presence byte not 0 or 1")),
        }
    }

    /// Ends a top-level decoder: nothing may follow.
    pub fn finish(self) -> Result<(), Error> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(Error::TrailingBytes)
        }
    }
}

/// A domain-separation label as it is hashed: `uint8 length || ASCII`, so that no
/// label followed by data can equal another label.
pub(crate) fn label(label: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + label.len());
    out.push(u8::try_from(label.len()).expect("labels are short"));
    out.extend_from_slice(label.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_are_big_endian_and_round_trip() {
        let mut w = Writer::new();
        w.header(kind::DEVICE_CREDENTIAL);
        w.u16(0x0102);
        w.u32(0x0304_0506);
        w.u64(0x0708_090a_0b0c_0d0e);
        w.opaque8(b"ab");
        w.opaque16(b"cd");
        w.opaque24(b"ef");
        w.presence(true);
        let bytes = w.into_vec();
        assert_eq!(
            bytes,
            [
                3, 1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 2, b'a', b'b', 0, 2, b'c',
                b'd', 0, 0, 2, b'e', b'f', 1
            ]
        );
        let mut r = Reader::new(&bytes, 100).unwrap();
        r.header(kind::DEVICE_CREDENTIAL).unwrap();
        assert_eq!(r.u16().unwrap(), 0x0102);
        assert_eq!(r.u32().unwrap(), 0x0304_0506);
        assert_eq!(r.u64().unwrap(), 0x0708_090a_0b0c_0d0e);
        assert_eq!(r.opaque8(0, 2).unwrap(), b"ab");
        assert_eq!(r.opaque16(0, 2).unwrap(), b"cd");
        assert_eq!(r.opaque24(0, 2).unwrap(), b"ef");
        assert!(r.presence().unwrap());
        r.finish().unwrap();
    }

    #[test]
    fn violations_are_rejected() {
        assert_eq!(Reader::new(&[0; 5], 4).err(), Some(Error::TooLong));
        let mut r = Reader::new(&[3, 2], 10).unwrap();
        assert_eq!(
            r.header(kind::DEVICE_CREDENTIAL),
            Err(Error::UnsupportedVersion(2))
        );
        let mut r = Reader::new(&[4, 1], 10).unwrap();
        assert_eq!(r.header(kind::DEVICE_CREDENTIAL), Err(Error::WrongKind(4)));
        let mut r = Reader::new(&[0, 0, 0], 10).unwrap();
        assert_eq!(r.u32(), Err(Error::Truncated));
        let mut r = Reader::new(&[3, b'a', b'b', b'c'], 10).unwrap();
        assert!(matches!(r.opaque8(0, 2), Err(Error::Malformed(_))));
        let mut r = Reader::new(&[1, b'a'], 10).unwrap();
        assert!(matches!(r.opaque8(2, 4), Err(Error::Malformed(_))));
        let mut r = Reader::new(&[5, b'a'], 10).unwrap();
        assert_eq!(r.opaque8(0, 10), Err(Error::Truncated));
        let mut r = Reader::new(&[2], 10).unwrap();
        assert!(matches!(r.presence(), Err(Error::Malformed(_))));
        let r = Reader::new(&[0], 10).unwrap();
        assert_eq!(r.finish(), Err(Error::TrailingBytes));
    }

    #[test]
    fn labels_are_length_prefixed() {
        assert_eq!(label("ab"), [2, b'a', b'b']);
    }
}
