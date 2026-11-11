// SPDX-License-Identifier: EUPL-1.2
//! Labels and values of the directory.

use crate::codec::{DecodeError, EncodeError, Reader, Writer};
use nuschel_identity::IdentityFingerprint;
use std::fmt;

/// Largest opaque payload of a [`DirectoryValue`]: a hybrid identity root public key
/// (Ed25519 + ML-DSA-65, 1984 bytes) and a device-list commitment fit with room to spare.
pub const MAX_VALUE_DATA: usize = 4096;
/// Largest encoded [`DirectoryValue`] (format, length prefix and data).
pub const MAX_VALUE_ENCODED: usize = 4 + MAX_VALUE_DATA;
/// Largest registered user name in a [`Label::Name`], in UTF-8 bytes.
pub const MAX_NAME_BYTES: usize = 128;
/// Largest encoded [`Label`].
pub const MAX_LABEL_ENCODED: usize = 2 + MAX_NAME_BYTES;

/// The immutable 128-bit account id of a registered user (docs/extension.md §3). Never
/// Murmur's reusable registration number.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountId(pub [u8; 16]);

impl fmt::Debug for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AccountId(")?;
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        write!(f, ")")
    }
}

/// What a directory entry is about (docs/e2ee.md §5.1). The label is hidden by the VRF:
/// the tree contains only pseudorandom node labels derived from it.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Label {
    /// `acct:` — the identity bound to an account on this server.
    Account(AccountId),
    /// `id:` — the identity record (device-signing key generation, devices,
    /// revocations, Mumble bindings) of an identity root.
    Identity(IdentityFingerprint),
    /// `name:` — the account behind a registered user name, as the server normalises it.
    Name(String),
}

const LABEL_ACCOUNT: u8 = 1;
const LABEL_IDENTITY: u8 = 2;
const LABEL_NAME: u8 = 3;

impl Label {
    /// A name label; `None` if the name is empty or longer than [`MAX_NAME_BYTES`].
    pub fn name(name: impl Into<String>) -> Option<Self> {
        let name = name.into();
        (!name.is_empty() && name.len() <= MAX_NAME_BYTES).then_some(Self::Name(name))
    }

    /// The canonical encoding, which is also the label the backend commits to:
    /// `kind (1 = account, 2 = identity, 3 = name) || data`, where a name is
    /// `u8 length || UTF-8`.
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let mut w = Writer::new();
        self.write(&mut w)?;
        Ok(w.finish())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes, MAX_LABEL_ENCODED)?;
        let label = Self::read(&mut r)?;
        r.finish()?;
        Ok(label)
    }

    pub(crate) fn write(&self, w: &mut Writer) -> Result<(), EncodeError> {
        match self {
            Self::Account(id) => {
                w.u8(LABEL_ACCOUNT);
                w.raw(&id.0);
            }
            Self::Identity(fp) => {
                w.u8(LABEL_IDENTITY);
                w.raw(fp.as_bytes());
            }
            Self::Name(name) => {
                if name.is_empty() {
                    return Err(EncodeError {
                        field: "label.name",
                        len: 0,
                        max: MAX_NAME_BYTES,
                    });
                }
                w.u8(LABEL_NAME);
                w.bytes_u8("label.name", name.as_bytes(), MAX_NAME_BYTES)?;
            }
        }
        Ok(())
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        match r.u8("label.kind")? {
            LABEL_ACCOUNT => Ok(Self::Account(AccountId(r.array("label.account")?))),
            LABEL_IDENTITY => Ok(Self::Identity(IdentityFingerprint::from_bytes(
                r.array("label.identity")?,
            ))),
            LABEL_NAME => {
                let bytes = r.bytes_u8("label.name", MAX_NAME_BYTES)?;
                if bytes.is_empty() {
                    return Err(DecodeError::InvalidValue {
                        field: "label.name",
                    });
                }
                let name = std::str::from_utf8(bytes).map_err(|_| DecodeError::InvalidValue {
                    field: "label.name",
                })?;
                Ok(Self::Name(name.to_owned()))
            }
            _ => Err(DecodeError::InvalidValue {
                field: "label.kind",
            }),
        }
    }
}

/// The value of a directory entry: versioned bytes. The backend never interprets them;
/// [`crate::AccountBinding`] and [`crate::NameBinding`] are the typed formats Nuschel
/// publishes.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct DirectoryValue {
    format: u16,
    data: Vec<u8>,
}

impl fmt::Debug for DirectoryValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectoryValue")
            .field("format", &self.format)
            .field("len", &self.data.len())
            .finish()
    }
}

impl DirectoryValue {
    /// `format` names the layout of `data` and is chosen by the producer (the identity
    /// crate); 0 is reserved. `None` if the format is 0 or the data is longer than
    /// [`MAX_VALUE_DATA`].
    pub fn new(format: u16, data: impl Into<Vec<u8>>) -> Option<Self> {
        let data = data.into();
        (format != 0 && data.len() <= MAX_VALUE_DATA).then_some(Self { format, data })
    }

    pub fn format(&self) -> u16 {
        self.format
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// `u16 format || opaque data<0..4096>` (u16 length).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16(self.format);
        // Cannot fail: `new` and `decode` enforce the limit.
        let _ = w.bytes_u16("value.data", &self.data, MAX_VALUE_DATA);
        w.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes, MAX_VALUE_ENCODED)?;
        let format = r.u16("value.format")?;
        if format == 0 {
            return Err(DecodeError::InvalidValue {
                field: "value.format",
            });
        }
        let data = r.bytes_u16("value.data", MAX_VALUE_DATA)?.to_vec();
        r.finish()?;
        Ok(Self { format, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_round_trip_and_are_distinct_by_kind() {
        let labels = [
            Label::Account(AccountId([7; 16])),
            Label::Identity(IdentityFingerprint::from_bytes([7; 32])),
            Label::name("alice").unwrap(),
        ];
        let mut encodings = Vec::new();
        for label in &labels {
            let bytes = label.encode().unwrap();
            assert_eq!(&Label::decode(&bytes).unwrap(), label);
            encodings.push(bytes);
        }
        encodings.dedup();
        assert_eq!(encodings.len(), 3);
    }

    #[test]
    fn label_parsing_is_strict() {
        assert!(Label::decode(&[9; 17]).is_err());
        assert!(Label::decode(&[LABEL_NAME, 0]).is_err());
        assert!(Label::decode(&[LABEL_NAME, 2, 0xff, 0xfe]).is_err());
        let mut account = vec![LABEL_ACCOUNT];
        account.extend([0; 17]);
        assert!(Label::decode(&account).is_err());
        assert!(Label::name("").is_none());
        assert!(Label::name("x".repeat(MAX_NAME_BYTES + 1)).is_none());
    }

    #[test]
    fn values_are_bounded_and_strict() {
        assert!(DirectoryValue::new(0, vec![1]).is_none());
        assert!(DirectoryValue::new(1, vec![0; MAX_VALUE_DATA + 1]).is_none());
        let v = DirectoryValue::new(1, vec![1, 2, 3]).unwrap();
        assert_eq!(DirectoryValue::decode(&v.encode()).unwrap(), v);
        let mut long = v.encode();
        long.push(0);
        assert!(DirectoryValue::decode(&long).is_err());
        assert!(DirectoryValue::decode(&[0, 0, 0, 0]).is_err());
    }
}
