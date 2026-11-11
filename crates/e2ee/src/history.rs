// SPDX-License-Identifier: EUPL-1.2
//! History keys and archive records (docs/e2ee.md §6.6): how a member who joins
//! later reads what was said before, without anyone keeping old MLS epoch secrets.
//!
//! Every durable message is sent twice: live, as the MLS application message, and as
//! an **archive record** the server stores beside it, sealed under the
//! conversation's current history key and signed by the sender's leaf key:
//!
//! ```text
//! struct {
//!   uint8  version = 1;
//!   uint32 key_index;           // which history key
//!   opaque nonce[12];
//!   opaque credential[172];     // the sender's Nuschel device credential
//!   uint32 ciphertext<0..2^17>; // ChaCha20-Poly1305(key, nonce, envelope; aad)
//!   opaque signature[64];       // leaf key over 0xff || label || everything above
//! } ArchiveRecord;
//! ```
//!
//! The AEAD's associated data is the label, the conversation id, the key index and
//! the credential, so a record cannot move to another conversation or claim another
//! sender. The signed payload starts with 0xff, which no MLS `SignContent` can (a
//! TLS variable-length integer never starts with two set bits), so a leaf signature
//! of one is never a signature of the other. The live envelope carries the record's
//! hash, so a sender cannot show members one text and the archive another.
//!
//! A **grant** hands history keys to the group, inside MLS: to a member who joined
//! (`members` history policy), and after a removal with a fresh key that the removed
//! device never gets (rotation).
//!
//! ```text
//! struct {
//!   uint8  version = 3;
//!   uint32 current;              // the key to seal with from now on
//!   uint16 count;                // at most MAX_KEYS
//!   uint8  more;                 // more bounded grant pages follow
//!   uint64 total;                // keys across all pages, including current
//!   struct { uint32 index; opaque key[32]; } keys[count];
//! } HistoryGrant;
//! ```
use crate::provider::Provider;
use crate::{Error, guard, mls};
use nuschel_identity::{DeviceCredential, DeviceKeys};
use openmls::prelude::{AeadType, SignatureScheme};
use openmls_traits::OpenMlsProvider;
use openmls_traits::crypto::OpenMlsCrypto;
use openmls_traits::random::OpenMlsRand;

const VERSION: u8 = 1;
const LABEL: &[u8] = b"nuschel/archive/v1";
const NONCE: usize = 12;
const CREDENTIAL: usize = 172;
const SIGNATURE: usize = 64;
/// An envelope with a 64 KiB body and its framing, sealed.
pub const MAX_CIPHERTEXT: usize = 128 * 1024;
/// History keys one grant carries; the retained conversation total is uncapped.
pub const MAX_KEYS: usize = 256;

/// Hash binding the separately signed archive record to its live MLS envelope.
pub fn archive_hash(bytes: &[u8]) -> [u8; 32] {
    let mut input = Vec::with_capacity(LABEL.len() + bytes.len());
    input.extend_from_slice(LABEL);
    input.extend_from_slice(bytes);
    libcrux_sha2::sha256(&input)
}

/// One history key.
#[derive(Clone, PartialEq, Eq)]
pub struct HistoryKey {
    pub index: u32,
    pub key: [u8; 32],
}

impl std::fmt::Debug for HistoryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HistoryKey({})", self.index)
    }
}

impl HistoryKey {
    /// A fresh key with this index.
    pub fn generate(index: u32) -> Result<Self, Error> {
        let provider = Provider::new()?;
        let bytes = provider.rand().random_vec(32).map_err(mls)?;
        Ok(Self {
            index,
            key: bytes.try_into().map_err(|_| Error::Invalid("randomness"))?,
        })
    }
}

fn aad(conversation: &[u8; 16], index: u32, credential: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(LABEL.len() + 16 + 4 + credential.len());
    out.extend_from_slice(LABEL);
    out.extend_from_slice(conversation);
    out.extend_from_slice(&index.to_be_bytes());
    out.extend_from_slice(credential);
    out
}

fn signed_payload(unsigned: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + LABEL.len() + unsigned.len());
    out.push(0xff);
    out.extend_from_slice(LABEL);
    out.extend_from_slice(unsigned);
    out
}

/// Seals an encoded envelope as an archive record under `key`.
pub fn seal_archive(
    key: &HistoryKey,
    conversation: &[u8; 16],
    envelope: &[u8],
    device: &DeviceKeys,
    credential: &DeviceCredential,
) -> Result<Vec<u8>, Error> {
    let credential = credential.encode();
    if credential.len() != CREDENTIAL {
        return Err(Error::Invalid("not a Nuschel device credential"));
    }
    guard(|| {
        let provider = Provider::new()?;
        let nonce = provider.rand().random_vec(NONCE).map_err(mls)?;
        let ciphertext = provider
            .crypto()
            .aead_encrypt(
                AeadType::ChaCha20Poly1305,
                &key.key,
                envelope,
                &nonce,
                &aad(conversation, key.index, &credential),
            )
            .map_err(mls)?;
        if ciphertext.len() > MAX_CIPHERTEXT {
            return Err(Error::Invalid("the message is too large to archive"));
        }
        let mut out = Vec::with_capacity(1 + 4 + NONCE + CREDENTIAL + 4 + ciphertext.len() + 64);
        out.push(VERSION);
        out.extend_from_slice(&key.index.to_be_bytes());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&credential);
        out.extend_from_slice(&(ciphertext.len() as u32).to_be_bytes());
        out.extend_from_slice(&ciphertext);
        let signature = device
            .sign_leaf(&signed_payload(&out))
            .map_err(|_| Error::Invalid("cannot sign the archive record"))?;
        out.extend_from_slice(&signature);
        Ok(out)
    })
}

/// An archive record's fields, before anything is decrypted.
struct Parsed<'a> {
    index: u32,
    nonce: &'a [u8],
    credential: &'a [u8],
    ciphertext: &'a [u8],
    unsigned: &'a [u8],
    signature: &'a [u8],
}

fn parse(bytes: &[u8]) -> Result<Parsed<'_>, Error> {
    let malformed = Error::Invalid("malformed archive record");
    let fixed = 1 + 4 + NONCE + CREDENTIAL + 4;
    if bytes.len() < fixed + SIGNATURE || bytes[0] != VERSION {
        return Err(malformed);
    }
    let index = u32::from_be_bytes(
        bytes[1..5]
            .try_into()
            .map_err(|_| Error::Invalid("index"))?,
    );
    let length = u32::from_be_bytes(
        bytes[fixed - 4..fixed]
            .try_into()
            .map_err(|_| Error::Invalid("length"))?,
    ) as usize;
    if length > MAX_CIPHERTEXT || bytes.len() != fixed + length + SIGNATURE {
        return Err(malformed);
    }
    Ok(Parsed {
        index,
        nonce: &bytes[5..5 + NONCE],
        credential: &bytes[5 + NONCE..5 + NONCE + CREDENTIAL],
        ciphertext: &bytes[fixed..fixed + length],
        unsigned: &bytes[..fixed + length],
        signature: &bytes[fixed + length..],
    })
}

/// The history key an archive record needs.
pub fn archive_key_index(bytes: &[u8]) -> Option<u32> {
    parse(bytes).ok().map(|parsed| parsed.index)
}

/// An opened archive record: the envelope, and the sender's credential (which the
/// caller checks against key transparency; the signature already verified under
/// its leaf key).
#[derive(Clone, Debug)]
pub struct Archived {
    pub envelope: Vec<u8>,
    pub credential: Vec<u8>,
    pub signature_key: Vec<u8>,
}

/// Verifies and opens an archive record of `conversation` with `key` (the one its
/// index names).
pub fn open_archive(
    key: &HistoryKey,
    conversation: &[u8; 16],
    bytes: &[u8],
) -> Result<Archived, Error> {
    let parsed = parse(bytes)?;
    if parsed.index != key.index {
        return Err(Error::Invalid("another history key"));
    }
    let credential = DeviceCredential::decode(parsed.credential)
        .map_err(|_| Error::Invalid("not a Nuschel device credential"))?;
    let signature_key = credential.leaf_key().as_bytes().to_vec();
    guard(|| {
        let provider = Provider::new()?;
        provider
            .crypto()
            .verify_signature(
                SignatureScheme::ED25519,
                &signed_payload(parsed.unsigned),
                &signature_key,
                parsed.signature,
            )
            .map_err(|_| Error::Invalid("the archive record's signature"))?;
        let envelope = provider
            .crypto()
            .aead_decrypt(
                AeadType::ChaCha20Poly1305,
                &key.key,
                parsed.ciphertext,
                parsed.nonce,
                &aad(conversation, key.index, parsed.credential),
            )
            .map_err(|_| Error::Invalid("the archive record does not open with its key"))?;
        Ok(Archived {
            envelope,
            credential: parsed.credential.to_vec(),
            signature_key,
        })
    })
}

/// Select one grant page from keys in increasing index order. The cap applies
/// only to this message; the cursor makes every retained key reachable.
pub fn grant_page(
    keys: impl Iterator<Item = HistoryKey>,
    current: HistoryKey,
    after: Option<u32>,
) -> (Vec<HistoryKey>, Option<u32>) {
    let mut page: Vec<_> = keys
        .filter(|key| key.index != current.index && after.is_none_or(|after| key.index > after))
        .take(MAX_KEYS)
        .collect();
    let next = if page.len() == MAX_KEYS {
        page.pop();
        page.last().map(|key| key.index)
    } else {
        None
    };
    page.push(current);
    page.sort_by_key(|key| key.index);
    (page, next)
}

/// A grant's body: every key, and the one to seal with from now on.
pub fn encode_grant(keys: &[HistoryKey], current: u32) -> Result<Vec<u8>, Error> {
    encode_grant_page(keys, current, false)
}

/// One bounded page; `more` keeps a new member waiting for subsequent pages.
pub fn encode_grant_page(keys: &[HistoryKey], current: u32, more: bool) -> Result<Vec<u8>, Error> {
    encode_grant_page_with_total(keys, current, more, keys.len() as u64)
}

pub fn encode_grant_page_with_total(
    keys: &[HistoryKey],
    current: u32,
    more: bool,
    total: u64,
) -> Result<Vec<u8>, Error> {
    if total < keys.len() as u64 || total > u64::from(current) + 1 {
        return Err(Error::Invalid("invalid history grant coverage"));
    }
    if keys.len() > MAX_KEYS {
        return Err(Error::Invalid("too many keys in one history grant"));
    }
    let mut out = Vec::with_capacity(16 + keys.len() * 36);
    out.push(3);
    out.extend_from_slice(&current.to_be_bytes());
    out.extend_from_slice(&(keys.len() as u16).to_be_bytes());
    out.push(u8::from(more));
    out.extend_from_slice(&total.to_be_bytes());
    for key in keys {
        out.extend_from_slice(&key.index.to_be_bytes());
        out.extend_from_slice(&key.key);
    }
    Ok(out)
}

/// The keys and the current index of a grant.
pub fn decode_grant(bytes: &[u8]) -> Result<(Vec<HistoryKey>, u32, bool, u64), Error> {
    let malformed = Error::Invalid("malformed history grant");
    if bytes.len() < 16 || bytes[0] != 3 || bytes[7] > 1 {
        return Err(malformed);
    }
    let current = u32::from_be_bytes(
        bytes[1..5]
            .try_into()
            .map_err(|_| Error::Invalid("grant"))?,
    );
    let count = usize::from(u16::from_be_bytes(
        bytes[5..7]
            .try_into()
            .map_err(|_| Error::Invalid("grant"))?,
    ));
    let total = u64::from_be_bytes(
        bytes[8..16]
            .try_into()
            .map_err(|_| Error::Invalid("grant coverage"))?,
    );
    if total < count as u64 || total > u64::from(current) + 1 {
        return Err(malformed);
    }
    if count > MAX_KEYS || bytes.len() != 16 + count * 36 {
        return Err(malformed);
    }
    let keys = bytes[16..]
        .chunks(36)
        .map(|entry| {
            Ok(HistoryKey {
                index: u32::from_be_bytes(
                    entry[..4].try_into().map_err(|_| Error::Invalid("grant"))?,
                ),
                key: entry[4..].try_into().map_err(|_| Error::Invalid("grant"))?,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    if keys.windows(2).any(|pair| pair[0].index >= pair[1].index) {
        return Err(Error::Invalid("history keys are not strictly ordered"));
    }
    if !keys.iter().any(|key| key.index == current) {
        return Err(Error::Invalid("the grant's current key is not in it"));
    }
    Ok((keys, current, bytes[7] == 1, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuschel_identity::{DisplayName, NewIdentity, Validity};

    #[test]
    fn bounded_grants_reach_every_retained_key_without_truncation() {
        let keys: Vec<_> = (0..700)
            .map(|index| HistoryKey {
                index,
                key: [(index % 251) as u8; 32],
            })
            .collect();
        let mut after = None;
        let mut received = std::collections::BTreeMap::new();
        let mut pages = 0;
        loop {
            let (page, next) = grant_page(keys.iter().cloned(), keys[699].clone(), after);
            assert!(page.len() <= MAX_KEYS);
            let encoded = encode_grant_page_with_total(&page, 699, next.is_some(), 700).unwrap();
            let (decoded, current, more, total) = decode_grant(&encoded).unwrap();
            assert_eq!(total, 700);
            assert_eq!(current, 699);
            assert_eq!(more, next.is_some());
            for key in decoded {
                received.insert(key.index, key.key);
            }
            pages += 1;
            let Some(next) = next else {
                break;
            };
            assert!(after.is_none_or(|before| next > before));
            after = Some(next);
        }
        assert_eq!(pages, 3);
        assert_eq!(received.len(), keys.len());
        for key in keys {
            assert_eq!(received[&key.index], key.key);
        }
        assert!(
            encode_grant(
                &vec![
                    HistoryKey {
                        index: 1,
                        key: [0; 32]
                    };
                    MAX_KEYS + 1
                ],
                1
            )
            .is_err()
        );
    }

    #[test]
    fn archive_records_open_only_where_they_belong() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let validity = Validity::starting_at(now - 3600, 86_400).unwrap();
        let alice =
            NewIdentity::create(&DisplayName::new("alice").unwrap(), validity, now).unwrap();
        let key = HistoryKey::generate(3).unwrap();
        let record = seal_archive(
            &key,
            &[1; 16],
            b"an envelope",
            &alice.device,
            &alice.device_credential,
        )
        .unwrap();
        assert_eq!(archive_key_index(&record), Some(3));
        let opened = open_archive(&key, &[1; 16], &record).unwrap();
        assert_eq!(opened.envelope, b"an envelope");
        assert_eq!(opened.credential, alice.device_credential.encode());
        // Another conversation, another key, a changed byte: refused.
        assert!(open_archive(&key, &[2; 16], &record).is_err());
        assert!(open_archive(&HistoryKey::generate(3).unwrap(), &[1; 16], &record).is_err());
        for at in [0, 3, 10, 30, record.len() - 70, record.len() - 1] {
            let mut bad = record.clone();
            bad[at] ^= 1;
            assert!(open_archive(&key, &[1; 16], &bad).is_err(), "byte {at}");
        }
        assert!(open_archive(&key, &[1; 16], &record[..record.len() - 1]).is_err());
        // Grants round-trip and must name a key they carry.
        let keys = vec![HistoryKey::generate(0).unwrap(), key.clone()];
        let grant = encode_grant(&keys, 3).unwrap();
        assert_eq!(
            decode_grant(&grant).unwrap(),
            (keys.clone(), 3, false, keys.len() as u64)
        );
        assert!(decode_grant(&encode_grant(&keys, 9).unwrap()).is_err());
        assert!(decode_grant(&grant[..grant.len() - 1]).is_err());
    }
}
