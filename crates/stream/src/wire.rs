// SPDX-License-Identifier: EUPL-1.2
//! Bounds-checked big-endian reading; every length is checked before it is used.

use crate::Error;

pub(crate) struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    pub fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        if self.0.len() < len {
            return Err(Error::Malformed("truncated"));
        }
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(head)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.take(N)?.try_into().expect("took N bytes"))
    }

    pub fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.array::<1>()?[0])
    }

    pub fn u32(&mut self) -> Result<u32, Error> {
        self.array().map(u32::from_be_bytes)
    }

    pub fn u64(&mut self) -> Result<u64, Error> {
        self.array().map(u64::from_be_bytes)
    }

    /// `opaque value<min..2^16-1>`.
    pub fn opaque16(&mut self, min: usize) -> Result<&'a [u8], Error> {
        let len = usize::from(u16::from_be_bytes(self.array()?));
        if len < min {
            return Err(Error::Malformed("short field"));
        }
        self.take(len)
    }

    pub fn finish(self) -> Result<(), Error> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Error::Malformed("trailing bytes"))
        }
    }
}

/// Appends `opaque value<..2^16-1>`.
pub(crate) fn put_opaque16(out: &mut Vec<u8>, value: &[u8]) -> Result<(), Error> {
    let len = u16::try_from(value.len()).map_err(|_| Error::TooLong)?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value);
    Ok(())
}
