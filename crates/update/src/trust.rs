// SPDX-License-Identifier: EUPL-1.2
use crate::auth_format::Error;
use crate::auth_format::{FORMAT_VERSION, PUBLIC_BYTES, PublicKey};
use serde::{Deserialize, Serialize};

pub const ALGORITHM_NAME: &str = "ed25519+mldsa44";
pub const MAX_TRUST_BYTES: usize = 32 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub format: u16,
    pub algorithm: String,
    pub keys: Vec<Entry>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub key_id: String,
    pub ed25519: String,
    pub mldsa44: String,
}

fn decode(text: &str, output: &mut [u8]) -> Result<(), Error> {
    if text.len() != output.len() * 2
        || !text
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(Error::KeyDocument);
    }
    for (pair, byte) in text.as_bytes().chunks_exact(2).zip(output.iter_mut()) {
        let digit = |c: u8| if c <= b'9' { c - b'0' } else { c - b'a' + 10 };
        *byte = digit(pair[0]) * 16 + digit(pair[1]);
    }
    Ok(())
}
pub fn parse(bytes: &[u8]) -> Result<Vec<PublicKey>, Error> {
    if bytes.len() > MAX_TRUST_BYTES {
        return Err(Error::KeyDocument);
    }
    let parsed: Document = serde_json::from_slice(bytes).map_err(|_| Error::KeyDocument)?;
    if parsed.format != FORMAT_VERSION
        || parsed.algorithm != ALGORITHM_NAME
        || parsed.keys.len() > 8
    {
        return Err(Error::KeyDocument);
    }
    let mut keys = Vec::new();
    for entry in parsed.keys {
        let mut bytes = [0; PUBLIC_BYTES];
        decode(&entry.ed25519, &mut bytes[..32])?;
        decode(&entry.mldsa44, &mut bytes[32..])?;
        let key = PublicKey::from_bytes(&bytes)?;
        if entry.key_id != crate::hex(&key.id())
            || keys.iter().any(|other: &PublicKey| other.id() == key.id())
        {
            return Err(Error::KeyDocument);
        }
        keys.push(key);
    }
    Ok(keys)
}
pub fn encode(key: &PublicKey) -> Result<Vec<u8>, Error> {
    serde_json::to_vec_pretty(&Document {
        format: FORMAT_VERSION,
        algorithm: ALGORITHM_NAME.into(),
        keys: vec![Entry {
            key_id: crate::hex(&key.id()),
            ed25519: crate::hex(key.ed25519()),
            mldsa44: crate::hex(key.mldsa44()),
        }],
    })
    .map_err(|_| Error::KeyDocument)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trust_metadata_rejects_unknown_duplicate_and_wrong_pair_identifiers() {
        let key = PublicKey::from_bytes(&[1; PUBLIC_BYTES]).unwrap();
        let bytes = encode(&key).unwrap();
        assert_eq!(parse(&bytes).unwrap(), vec![key.clone()]);
        let mut parsed: Document = serde_json::from_slice(&bytes).unwrap();
        parsed.keys[0].key_id = "00".repeat(32);
        assert!(parse(&serde_json::to_vec(&parsed).unwrap()).is_err());
        let mut bytes = bytes;
        bytes.splice(1..1, br#""format":2,"#.iter().copied());
        assert!(parse(&bytes).is_err());
        assert!(parse(br#"{"format":2,"algorithm":"ed25519","keys":[]}"#).is_err());
    }
}
