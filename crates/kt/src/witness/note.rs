// SPDX-License-Identifier: EUPL-1.2
//! C2SP signed notes (c2sp.org/signed-note), the Ed25519 log signature on a
//! checkpoint, and ML-DSA-44 checkpoint cosignatures (c2sp.org/tlog-cosignature,
//! signature type 0x06), as read from the C2SP editor's copies of 2026-11.
//!
//! A note is UTF-8 text without control characters other than newline, ending in a
//! newline, then an empty line, then signature lines
//! `— <key name> <base64(key id || signature)>\n`. Key ids are
//! `SHA-256(name || "\n" || type || public key)[:4]`. Base64 is RFC 4648 with
//! padding and canonical (non-canonical input is rejected).
//!
//! Verifiers ignore signatures from keys they do not know; a known key whose
//! signature does not verify rejects the note.

use crate::codec::DecodeError;
use crate::log::{LogSigningKey, SignatureScheme};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use libcrux_ml_dsa::ml_dsa_44;
use sha2::{Digest, Sha256};
use std::fmt;
use zeroize::Zeroizing;

/// Largest note this crate parses: 16 ML-DSA-44 cosignatures are about 52 KiB.
pub const MAX_NOTE_BYTES: usize = 64 * 1024;
/// Most signature lines in a note (C2SP: at least 16 must be accepted).
pub const MAX_SIGNATURES: usize = 32;
/// Longest key name (C2SP cosigner names: at most 255 bytes).
pub const MAX_NAME_BYTES: usize = 255;
/// Signature type of Ed25519 note signatures (the log's).
pub const TYPE_ED25519: u8 = 0x01;
/// Signature type of timestamped ML-DSA-44 checkpoint cosignatures.
pub const TYPE_MLDSA44_COSIGNATURE: u8 = 0x06;
/// ML-DSA-44 sizes (FIPS 204).
pub const MLDSA44_PUBLIC_KEY_BYTES: usize = 1312;
pub const MLDSA44_SIGNATURE_BYTES: usize = 2420;
/// A cosignature: an 8-byte timestamp and the signature.
pub const COSIGNATURE_BYTES: usize = 8 + MLDSA44_SIGNATURE_BYTES;
/// The domain-separation label of the signed `CosignedSubtree` structure.
const SUBTREE_LABEL: &[u8; 12] = b"subtree/v1\n\0";
const DASH: &str = "\u{2014} ";

/// Whether `name` is a valid key or cosigner name: 1 to 255 bytes of UTF-8 without
/// spaces, `+` or control characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name
            .chars()
            .any(|c| c.is_whitespace() || c == '+' || c.is_control())
}

fn invalid(field: &'static str) -> DecodeError {
    DecodeError::InvalidValue { field }
}

/// Canonical base64 (with padding).
pub fn base64_decode(text: &str, field: &'static str) -> Result<Vec<u8>, DecodeError> {
    let bytes = BASE64.decode(text).map_err(|_| invalid(field))?;
    // The engine rejects non-zero trailing bits and bad padding; this rejects what
    // remains (none known), so that each value has one encoding.
    if BASE64.encode(&bytes) != text {
        return Err(invalid(field));
    }
    Ok(bytes)
}

pub fn base64_encode(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

/// `SHA-256(name || "\n" || type || public key)[:4]`.
pub fn key_id(name: &str, signature_type: u8, public_key: &[u8]) -> [u8; 4] {
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    h.update(b"\n");
    h.update([signature_type]);
    h.update(public_key);
    let digest = h.finalize();
    [digest[0], digest[1], digest[2], digest[3]]
}

/// One signature line.
#[derive(Clone, PartialEq, Eq)]
pub struct NoteSignature {
    pub name: String,
    pub key_id: [u8; 4],
    pub signature: Vec<u8>,
}

impl fmt::Debug for NoteSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "NoteSignature({}, {:02x?}, {} bytes)",
            self.name,
            self.key_id,
            self.signature.len()
        )
    }
}

impl NoteSignature {
    /// `— <name> <base64(key id || signature)>\n`.
    pub fn line(&self) -> String {
        let mut bytes = self.key_id.to_vec();
        bytes.extend_from_slice(&self.signature);
        format!("{DASH}{} {}\n", self.name, base64_encode(&bytes))
    }

    /// Parses one line without its newline.
    pub fn parse_line(line: &str) -> Result<Self, DecodeError> {
        let rest = line
            .strip_prefix(DASH)
            .ok_or_else(|| invalid("note.signature"))?;
        let (name, encoded) = rest
            .split_once(' ')
            .ok_or_else(|| invalid("note.signature"))?;
        if !valid_name(name) {
            return Err(invalid("note.signature.name"));
        }
        let bytes = base64_decode(encoded, "note.signature.base64")?;
        if bytes.len() < 5 {
            return Err(invalid("note.signature.length"));
        }
        Ok(Self {
            name: name.to_owned(),
            key_id: [bytes[0], bytes[1], bytes[2], bytes[3]],
            signature: bytes[4..].to_vec(),
        })
    }
}

/// A signed note: its text and signature lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// The text, ending in a newline.
    pub text: String,
    pub signatures: Vec<NoteSignature>,
}

impl Note {
    pub fn parse(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() > MAX_NOTE_BYTES {
            return Err(DecodeError::InputTooLarge {
                max: MAX_NOTE_BYTES,
            });
        }
        let note = std::str::from_utf8(bytes).map_err(|_| invalid("note.utf8"))?;
        if note.chars().any(|c| c.is_control() && c != '\n') {
            return Err(invalid("note.control"));
        }
        // The separator is the last empty line; signature lines are never empty.
        let split = note
            .rfind("\n\n")
            .ok_or_else(|| invalid("note.separator"))?;
        let text = &note[..=split];
        let lines = &note[split + 2..];
        let body = lines
            .strip_suffix('\n')
            .ok_or_else(|| invalid("note.signatures"))?;
        let mut signatures = Vec::new();
        for line in body.split('\n') {
            if signatures.len() == MAX_SIGNATURES {
                return Err(DecodeError::TooMany {
                    field: "note.signatures",
                    count: MAX_SIGNATURES + 1,
                    max: MAX_SIGNATURES,
                });
            }
            signatures.push(NoteSignature::parse_line(line)?);
        }
        Ok(Self {
            text: text.to_owned(),
            signatures,
        })
    }

    pub fn format(&self) -> String {
        let mut out = self.text.clone();
        out.push('\n');
        for signature in &self.signatures {
            out.push_str(&signature.line());
        }
        out
    }

    /// The signatures by a key with this name and id.
    pub fn by(&self, name: &str, key_id: [u8; 4]) -> impl Iterator<Item = &NoteSignature> {
        self.signatures
            .iter()
            .filter(move |s| s.name == name && s.key_id == key_id)
    }
}

/// The log's note-signing key: the server's identity key (the head-signing key), under
/// the checkpoint's origin as key name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogNoteKey {
    pub name: String,
    pub key: LogSigningKey,
}

impl LogNoteKey {
    pub fn key_id(&self) -> [u8; 4] {
        match self.key.scheme() {
            SignatureScheme::Ed25519 => key_id(&self.name, TYPE_ED25519, &self.key.to_bytes()),
        }
    }

    /// Whether the note carries a valid signature by this key. Any signature line with
    /// this key's name and id that fails rejects the note.
    pub fn verify(&self, note: &Note) -> bool {
        let mut valid = false;
        for signature in note.by(&self.name, self.key_id()) {
            if !self.key.verify(note.text.as_bytes(), &signature.signature) {
                return false;
            }
            valid = true;
        }
        valid
    }
}

/// A witness's public key: an ML-DSA-44 checkpoint cosigner.
#[derive(Clone, PartialEq, Eq)]
pub struct CosignerKey {
    name: String,
    public: Box<[u8; MLDSA44_PUBLIC_KEY_BYTES]>,
}

impl fmt::Debug for CosignerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CosignerKey({}+{})", self.name, hex4(self.key_id()))
    }
}

fn hex4(id: [u8; 4]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// A cosignature that verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cosigned {
    /// The cosigner's time of signing, seconds since the Unix epoch.
    pub timestamp: u64,
}

impl CosignerKey {
    pub fn new(name: &str, public: [u8; MLDSA44_PUBLIC_KEY_BYTES]) -> Option<Self> {
        valid_name(name).then(|| Self {
            name: name.to_owned(),
            public: Box::new(public),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn public_key(&self) -> &[u8; MLDSA44_PUBLIC_KEY_BYTES] {
        &self.public
    }

    pub fn key_id(&self) -> [u8; 4] {
        key_id(&self.name, TYPE_MLDSA44_COSIGNATURE, &self.public[..])
    }

    /// `<name>+<hex key id>+<base64(0x06 || public key)>`.
    pub fn vkey(&self) -> String {
        let mut bytes = vec![TYPE_MLDSA44_COSIGNATURE];
        bytes.extend_from_slice(&self.public[..]);
        format!(
            "{}+{}+{}",
            self.name,
            hex4(self.key_id()),
            base64_encode(&bytes)
        )
    }

    /// Parses a verifier key; only ML-DSA-44 cosigner keys (type 0x06) are accepted,
    /// and the key id must be the one the key implies.
    pub fn from_vkey(vkey: &str) -> Result<Self, DecodeError> {
        if vkey.len() > 4096 {
            return Err(DecodeError::InputTooLarge { max: 4096 });
        }
        // The base64 part may itself contain `+`.
        let mut parts = vkey.trim().splitn(3, '+');
        let (Some(name), Some(id), Some(key)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(invalid("vkey"));
        };
        if !valid_name(name) {
            return Err(invalid("vkey.name"));
        }
        let bytes = base64_decode(key, "vkey.key")?;
        let (&kind, public) = bytes.split_first().ok_or_else(|| invalid("vkey.key"))?;
        if kind != TYPE_MLDSA44_COSIGNATURE {
            return Err(invalid("vkey.type"));
        }
        let public: [u8; MLDSA44_PUBLIC_KEY_BYTES] =
            public.try_into().map_err(|_| invalid("vkey.key"))?;
        let parsed = Self::new(name, public).ok_or_else(|| invalid("vkey.name"))?;
        if id != hex4(parsed.key_id()) {
            return Err(invalid("vkey.id"));
        }
        Ok(parsed)
    }

    /// Verifies this cosigner's signature lines on a checkpoint note: `Ok(None)` if it
    /// did not sign, `Ok(Some(newest))` for its newest valid cosignature, `Err` if any
    /// of its lines fails (the note is then rejected).
    pub fn verify(
        &self,
        note: &Note,
        checkpoint: &super::Checkpoint,
    ) -> Result<Option<Cosigned>, DecodeError> {
        let mut newest: Option<Cosigned> = None;
        for line in note.by(&self.name, self.key_id()) {
            if line.signature.len() != COSIGNATURE_BYTES {
                return Err(invalid("cosignature.length"));
            }
            let timestamp = u64::from_be_bytes(line.signature[..8].try_into().expect("8 bytes"));
            let signature: [u8; MLDSA44_SIGNATURE_BYTES] =
                line.signature[8..].try_into().expect("checked length");
            let message = cosigned_subtree(&self.name, timestamp, checkpoint)?;
            let key = ml_dsa_44::MLDSA44VerificationKey::new(*self.public);
            let signature = ml_dsa_44::MLDSA44Signature::new(signature);
            if ml_dsa_44::verify(&key, &message, b"", &signature).is_err() {
                return Err(invalid("cosignature"));
            }
            if newest.is_none_or(|n| n.timestamp < timestamp) {
                newest = Some(Cosigned { timestamp });
            }
        }
        Ok(newest)
    }
}

/// The signed `CosignedSubtree` of a checkpoint (start 0, end the tree size):
///
/// ```text
/// struct {
///   uint8  label[12] = "subtree/v1\n\0";
///   opaque cosigner_name<1..2^8-1>;
///   uint64 timestamp;
///   opaque log_origin<1..2^8-1>;
///   uint64 start;
///   uint64 end;
///   uint8  hash[32];
/// } CosignedSubtree;
/// ```
///
/// ML-DSA-44 signs it with an empty FIPS 204 context.
fn cosigned_subtree(
    name: &str,
    timestamp: u64,
    checkpoint: &super::Checkpoint,
) -> Result<Vec<u8>, DecodeError> {
    if !valid_name(name) || !valid_name(&checkpoint.origin) {
        return Err(invalid("cosignature.name"));
    }
    let mut out = Vec::with_capacity(12 + 1 + name.len() + 8 + 1 + checkpoint.origin.len() + 48);
    out.extend_from_slice(SUBTREE_LABEL);
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.push(checkpoint.origin.len() as u8);
    out.extend_from_slice(checkpoint.origin.as_bytes());
    out.extend_from_slice(&0u64.to_be_bytes());
    out.extend_from_slice(&checkpoint.size.to_be_bytes());
    out.extend_from_slice(&checkpoint.root);
    Ok(out)
}

/// A witness's signing key: ML-DSA-44 from a 32-byte seed (FIPS 204
/// `ML-DSA.KeyGen_internal(ξ)`). The expanded signing key is zeroised on drop.
pub struct Cosigner {
    public: CosignerKey,
    signing: SigningKey,
}

struct SigningKey(Box<ml_dsa_44::MLDSA44SigningKey>);

impl Drop for SigningKey {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(self.0.as_ref_mut().as_mut_slice());
    }
}

impl Cosigner {
    /// `None` if the name is not a valid cosigner name.
    pub fn from_seed(name: &str, seed: &[u8; 32]) -> Option<Self> {
        let seed = Zeroizing::new(*seed);
        let pair = ml_dsa_44::generate_key_pair(*seed);
        let public = CosignerKey::new(name, *pair.verification_key.as_ref())?;
        Some(Self {
            public,
            signing: SigningKey(Box::new(pair.signing_key)),
        })
    }

    pub fn key(&self) -> &CosignerKey {
        &self.public
    }

    /// A hedged ML-DSA-44 cosignature on `checkpoint` at `timestamp` (seconds).
    pub fn cosign(
        &self,
        checkpoint: &super::Checkpoint,
        timestamp: u64,
    ) -> Result<NoteSignature, String> {
        let message = cosigned_subtree(self.public.name(), timestamp, checkpoint)
            .map_err(|e| e.to_string())?;
        let mut randomness = [0u8; 32];
        getrandom::fill(&mut randomness).map_err(|e| format!("randomness: {e}"))?;
        let signature = ml_dsa_44::sign(&self.signing.0, &message, b"", randomness)
            .map_err(|e| format!("ML-DSA-44: {e:?}"))?;
        let mut bytes = timestamp.to_be_bytes().to_vec();
        bytes.extend_from_slice(signature.as_ref());
        Ok(NoteSignature {
            name: self.public.name().to_owned(),
            key_id: self.public.key_id(),
            signature: bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{Ed25519HeadSigner, HeadSigner};
    use crate::witness::Checkpoint;

    fn checkpoint() -> Checkpoint {
        Checkpoint {
            origin: "chat.example.org:64738/nuschl-kt/v1".into(),
            size: 5,
            root: [7; 32],
        }
    }

    #[test]
    fn c2sp_signed_note_example_verifies() {
        // c2sp.org/signed-note's Ed25519 example (Go's sumdb/note test key).
        let vkey = "PeterNeumann+c74f20a3+ARpc2QcUPDhMQegwxbzhKqiBfsVkmqq/LDE4izWy10TW";
        let text = "If you think cryptography is the answer to your problem,\n\
                    then you don't know what your problem is.\n";
        let note = format!(
            "{text}\n\u{2014} PeterNeumann x08go/ZJkuBS9UG/SffcvIAQxVBtiFupLLr8pAcElZInNIuGUgYN1FFYC2pZSNXgKvqfqdngotpRZb6KE6RyyBwJnAM=\n"
        );
        let parsed = Note::parse(note.as_bytes()).unwrap();
        assert_eq!(parsed.text, text);
        assert_eq!(parsed.format(), note);
        let mut parts = vkey.split('+');
        let name = parts.next().unwrap();
        let id = parts.next().unwrap();
        let key = base64_decode(parts.next().unwrap(), "k").unwrap();
        assert_eq!(key[0], TYPE_ED25519);
        let log = LogNoteKey {
            name: name.into(),
            key: LogSigningKey::ed25519(key[1..].try_into().unwrap()).unwrap(),
        };
        assert_eq!(hex4(log.key_id()), id);
        assert!(log.verify(&parsed));
        // Another text with the same signature fails; an unknown key is ignored.
        let mut other = parsed.clone();
        other.text = "If you think\n".into();
        assert!(!log.verify(&other));
        let stranger = LogNoteKey {
            name: "someone".into(),
            key: log.key.clone(),
        };
        assert!(!stranger.verify(&parsed));
    }

    #[test]
    fn notes_are_parsed_strictly() {
        let good = "a\n\n\u{2014} k AAAAAAE=\n";
        assert!(Note::parse(good.as_bytes()).is_ok());
        for bad in [
            "a\n\u{2014} k AAAAAAE=\n",     // no separator
            "a\n\n\u{2014} k AAAAAAE=",     // no final newline
            "a\n\n- k AAAAAAE=\n",          // not an em dash
            "a\n\n\u{2014} k+x AAAAAAE=\n", // a plus in the name
            "a\n\n\u{2014} k AAAAAAF=\n",   // non-canonical base64
            "a\n\n\u{2014} k AAAA\n",       // shorter than a key id and one byte
            "a\r\n\n\u{2014} k AAAAAAE=\n", // a control character
            "a\n\n\u{2014} k AAAAAAE=\n\n", // an empty signature line
        ] {
            assert!(Note::parse(bad.as_bytes()).is_err(), "{bad:?}");
        }
        let many = format!(
            "a\n\n{}",
            "\u{2014} k AAAAAAE=\n".repeat(MAX_SIGNATURES + 1)
        );
        assert!(Note::parse(many.as_bytes()).is_err());
        assert!(Note::parse(&vec![b'a'; MAX_NOTE_BYTES + 1]).is_err());
    }

    #[test]
    fn log_signatures_and_cosignatures_round_trip() {
        let signer = Ed25519HeadSigner::from_seed(&[3; 32]);
        let cp = checkpoint();
        let log = LogNoteKey {
            name: cp.origin.clone(),
            key: signer.public_key(),
        };
        let text = cp.body();
        let mut note = Note {
            text: text.clone(),
            signatures: vec![NoteSignature {
                name: log.name.clone(),
                key_id: log.key_id(),
                signature: signer.sign(text.as_bytes()).unwrap(),
            }],
        };
        assert!(log.verify(&note));

        let cosigner = Cosigner::from_seed("witness.example.org/kt", &[9; 32]).unwrap();
        let key = CosignerKey::from_vkey(&cosigner.key().vkey()).unwrap();
        assert_eq!(&key, cosigner.key());
        note.signatures
            .push(cosigner.cosign(&cp, 1_800_000_000).unwrap());
        let reparsed = Note::parse(note.format().as_bytes()).unwrap();
        assert_eq!(reparsed, note);
        assert_eq!(
            key.verify(&reparsed, &cp).unwrap(),
            Some(Cosigned {
                timestamp: 1_800_000_000
            })
        );
        assert!(log.verify(&reparsed));
        // The same cosignature on another checkpoint, or with another timestamp, fails.
        let mut other = cp.clone();
        other.size = 6;
        assert!(key.verify(&reparsed, &other).is_err());
        let mut retimed = reparsed.clone();
        retimed.signatures[1].signature[7] ^= 1;
        assert!(key.verify(&retimed, &cp).is_err());
        // Another witness's key does not see it.
        let stranger = Cosigner::from_seed("other.example.org", &[9; 32]).unwrap();
        assert_eq!(stranger.key().verify(&reparsed, &cp).unwrap(), None);
        // The key id is part of the vkey.
        let vkey = key.vkey();
        let (name, rest) = vkey.split_once('+').unwrap();
        let tampered = format!("{name}+00000000{}", &rest[8..]);
        assert!(CosignerKey::from_vkey(&tampered).is_err());
        assert!(CosignerKey::from_vkey("a+00000000+AQ==").is_err());
        assert_eq!(COSIGNATURE_BYTES + 4, 2432);
    }
}
