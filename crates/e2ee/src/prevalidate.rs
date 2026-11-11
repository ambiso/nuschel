// SPDX-License-Identifier: EUPL-1.2
//! Structural checks on MLS bytes before OpenMLS or libcrux see them (docs/e2ee.md
//! §6.3, "Hostile input").
//!
//! libcrux-kem 0.0.9 (pinned by OpenMLS 0.9.2) panicked when it decoded an X-Wing
//! public key shorter than 1216 bytes (RUSTSEC-2026-0330/-0331); the workspace now
//! patches it to 0.0.10, which returns an error (vendor/hpke-rs-libcrux). Nobody
//! decodes a key on receipt: the failure comes later, in whoever encapsulates to it
//! (a member adding a key package's owner, or committing to a tree with a planted
//! leaf or parent key), and would break that member's commit. So every HPKE key that
//! can end up in a key package or a ratchet tree is checked here, by a small parser of
//! RFC 9420's TLS encoding that knows nothing of OpenMLS:
//!
//! - [`key_package`]: version, suite, the init key, the leaf node (encryption and
//!   signature key lengths, a Nuschel credential, capabilities, the key-package
//!   lifetime), extensions, signature; nothing may trail.
//! - [`ratchet_tree`]: every leaf and parent node of a tree (as served by the
//!   delivery service, or exported after a commit).
//! - [`message`]: the framing of an `MLSMessage` (wire format, group, epoch, content
//!   type), which also lets the server fence epochs without decrypting anything.
//!
//! The parser is strict where RFC 9420 is (minimal variable-length integers, defined
//! enum values, no trailing bytes) and bounds every count by the remaining input.
use crate::{CREDENTIAL_TYPE, SUITE_ID, SUITE_KEM_PUBLIC_KEY_LEN};

/// Ed25519 leaf signature keys and signatures.
const SIGNATURE_KEY_LEN: usize = 32;
const SIGNATURE_LEN: usize = 64;
/// A Nuschel device credential (nuschel-identity `DeviceCredential`).
const DEVICE_CREDENTIAL_LEN: usize = 172;
/// Elements in a capabilities list or extension list (RFC 9420 has no limit; a
/// Nuschel leaf lists a handful).
const MAX_LIST: usize = 64;
/// One extension's data.
const MAX_EXTENSION: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Malformed {
    #[error("truncated MLS data")]
    Truncated,
    #[error("trailing bytes after MLS data")]
    Trailing,
    #[error("non-minimal or invalid variable-length integer")]
    VarInt,
    #[error("unsupported protocol version")]
    Version,
    #[error("unexpected ciphersuite {0:#06x}")]
    Suite(u16),
    #[error("{what} is {got} bytes, expected {expected}")]
    Length {
        what: &'static str,
        got: usize,
        expected: usize,
    },
    #[error("not a Nuschel device credential")]
    Credential,
    #[error("invalid {0}")]
    Value(&'static str),
    #[error("too many {0}")]
    TooMany(&'static str),
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        if self.bytes.len() < n {
            return Err(Malformed::Truncated);
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Malformed> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, Malformed> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, Malformed> {
        let b = self.take(8)?;
        let mut a = [0; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }

    /// RFC 9420 §2.1.2: a QUIC-style length of 1, 2 or 4 bytes, minimally encoded.
    fn varint(&mut self) -> Result<usize, Malformed> {
        let first = self.u8()?;
        let (value, minimum) = match first >> 6 {
            0 => (u32::from(first & 0x3f), 0),
            1 => {
                let second = self.u8()?;
                (u32::from(first & 0x3f) << 8 | u32::from(second), 64)
            }
            2 => {
                let rest = self.take(3)?;
                (
                    u32::from(first & 0x3f) << 24
                        | u32::from(rest[0]) << 16
                        | u32::from(rest[1]) << 8
                        | u32::from(rest[2]),
                    16384,
                )
            }
            _ => return Err(Malformed::VarInt),
        };
        if value < minimum {
            return Err(Malformed::VarInt);
        }
        Ok(value as usize)
    }

    /// `opaque data<V>`.
    fn opaque(&mut self) -> Result<&'a [u8], Malformed> {
        let length = self.varint()?;
        self.take(length)
    }

    /// `opaque data<V>` of exactly `expected` bytes.
    fn exact(&mut self, what: &'static str, expected: usize) -> Result<&'a [u8], Malformed> {
        let data = self.opaque()?;
        if data.len() != expected {
            return Err(Malformed::Length {
                what,
                got: data.len(),
                expected,
            });
        }
        Ok(data)
    }

    /// A `<V>` vector, parsed element by element from its own bytes.
    fn vector(&mut self) -> Result<Reader<'a>, Malformed> {
        Ok(Reader::new(self.opaque()?))
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn finish(&self) -> Result<(), Malformed> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(Malformed::Trailing)
        }
    }
}

/// What the parser learned from a leaf node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafFacts {
    pub signature_key: [u8; 32],
    /// The Nuschel device credential (172 bytes).
    pub credential: Vec<u8>,
    /// 1 key package, 2 update, 3 commit.
    pub source: u8,
    /// The lifetime of a key-package leaf: not before, not after (Unix seconds).
    pub lifetime: Option<(u64, u64)>,
}

fn u16_list(reader: &mut Reader<'_>, what: &'static str) -> Result<Vec<u16>, Malformed> {
    let mut list = reader.vector()?;
    let mut out = Vec::new();
    while !list.is_empty() {
        if out.len() >= MAX_LIST {
            return Err(Malformed::TooMany(what));
        }
        out.push(list.u16()?);
    }
    Ok(out)
}

fn extensions(reader: &mut Reader<'_>) -> Result<Vec<(u16, usize)>, Malformed> {
    let mut list = reader.vector()?;
    let mut out = Vec::new();
    while !list.is_empty() {
        if out.len() >= MAX_LIST {
            return Err(Malformed::TooMany("extensions"));
        }
        let kind = list.u16()?;
        let data = list.opaque()?;
        if data.len() > MAX_EXTENSION {
            return Err(Malformed::Value("extension size"));
        }
        out.push((kind, data.len()));
    }
    Ok(out)
}

/// RFC 9420 §7.2 `LeafNode`, with Nuschel's constraints.
fn leaf_node(reader: &mut Reader<'_>) -> Result<LeafFacts, Malformed> {
    reader.exact("leaf encryption key", SUITE_KEM_PUBLIC_KEY_LEN)?;
    let signature_key: [u8; 32] = reader
        .exact("leaf signature key", SIGNATURE_KEY_LEN)?
        .try_into()
        .map_err(|_| Malformed::Truncated)?;
    if reader.u16()? != CREDENTIAL_TYPE {
        return Err(Malformed::Credential);
    }
    let credential = reader.opaque()?;
    if credential.len() != DEVICE_CREDENTIAL_LEN {
        return Err(Malformed::Credential);
    }
    // Capabilities: versions, suites, extensions, proposals, credentials.
    let versions = u16_list(reader, "versions")?;
    let suites = u16_list(reader, "suites")?;
    u16_list(reader, "extension types")?;
    u16_list(reader, "proposal types")?;
    let credentials = u16_list(reader, "credential types")?;
    if !versions.is_empty() && !versions.contains(&1) {
        return Err(Malformed::Version);
    }
    if !suites.contains(&SUITE_ID) {
        return Err(Malformed::Suite(suites.first().copied().unwrap_or(0)));
    }
    if !credentials.contains(&CREDENTIAL_TYPE) {
        return Err(Malformed::Credential);
    }
    let source = reader.u8()?;
    let lifetime = match source {
        1 => Some((reader.u64()?, reader.u64()?)),
        2 => None,
        3 => {
            reader.opaque()?;
            None
        }
        _ => return Err(Malformed::Value("leaf node source")),
    };
    extensions(reader)?;
    reader.exact("leaf signature", SIGNATURE_LEN)?;
    Ok(LeafFacts {
        signature_key,
        credential: credential.to_vec(),
        source,
        lifetime,
    })
}

/// What a valid-looking key package carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPackageFacts {
    pub leaf: LeafFacts,
    /// It carries the `last_resort` extension (RFC 9420 §17.3 / RFC 9750: 0x000a).
    pub last_resort: bool,
}

/// The last-resort key package extension type.
pub const LAST_RESORT_EXTENSION: u16 = 0x000a;

/// A `KeyPackage` (RFC 9420 §10) in its TLS encoding, as uploaded to the directory
/// and claimed from it.
pub fn key_package(bytes: &[u8]) -> Result<KeyPackageFacts, Malformed> {
    let mut reader = Reader::new(bytes);
    if reader.u16()? != 1 {
        return Err(Malformed::Version);
    }
    let suite = reader.u16()?;
    if suite != SUITE_ID {
        return Err(Malformed::Suite(suite));
    }
    reader.exact("init key", SUITE_KEM_PUBLIC_KEY_LEN)?;
    let leaf = leaf_node(&mut reader)?;
    if leaf.source != 1 {
        return Err(Malformed::Value("key package leaf source"));
    }
    let extensions = extensions(&mut reader)?;
    reader.exact("key package signature", SIGNATURE_LEN)?;
    reader.finish()?;
    Ok(KeyPackageFacts {
        leaf,
        last_resort: extensions
            .iter()
            .any(|(kind, _)| *kind == LAST_RESORT_EXTENSION),
    })
}

/// The leaves of a ratchet tree: how many leaf positions it has, and the present
/// (non-blank) ones with their index, in index order. Blank leaves are not stored, so
/// what the parser keeps is never larger than the input: a present leaf takes more
/// than a kilobyte of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeLeaves {
    pub width: u32,
    pub present: Vec<(u32, LeafFacts)>,
}

impl TreeLeaves {
    /// Present leaves.
    pub fn len(&self) -> usize {
        self.present.len()
    }

    pub fn is_empty(&self) -> bool {
        self.present.is_empty()
    }
}

/// A ratchet tree (RFC 9420 §12.4.3.3 `optional<Node> ratchet_tree<V>`): every leaf
/// and parent node, so no short key can be planted in a tree a member will encrypt
/// to. Leaves sit at even positions, parents at odd ones.
///
/// There is no member cap (docs/e2ee.md §6.7, the user's answer to open question
/// 6): the input's size bounds the work. Every node takes at least one byte (its
/// presence flag), so the walk is linear in the input, and only present leaves are
/// kept. What bounds a hostile tree is the message size limit of whoever hands it
/// over (`nuschel_protocol::nuschel::MAX_MLS_TREE_BYTES`) and the process's limits.
pub fn ratchet_tree(bytes: &[u8]) -> Result<TreeLeaves, Malformed> {
    let mut outer = Reader::new(bytes);
    let mut nodes = outer.vector()?;
    outer.finish()?;
    let mut leaves = TreeLeaves::default();
    let mut position = 0usize;
    while !nodes.is_empty() {
        let present = nodes.u8()?;
        let is_leaf = position.is_multiple_of(2);
        let leaf_index =
            u32::try_from(position / 2).map_err(|_| Malformed::TooMany("tree nodes"))?;
        match present {
            0 => {}
            1 => {
                let node_type = nodes.u8()?;
                match (node_type, is_leaf) {
                    (1, true) => leaves.present.push((leaf_index, leaf_node(&mut nodes)?)),
                    (2, false) => {
                        nodes.exact("parent encryption key", SUITE_KEM_PUBLIC_KEY_LEN)?;
                        nodes.opaque()?; // parent hash
                        let mut unmerged = nodes.vector()?;
                        while !unmerged.is_empty() {
                            unmerged.u32()?;
                        }
                    }
                    _ => return Err(Malformed::Value("node type")),
                }
            }
            _ => return Err(Malformed::Value("optional")),
        }
        if is_leaf {
            leaves.width = leaf_index + 1;
        }
        position += 1;
    }
    Ok(leaves)
}

/// MLS wire formats (RFC 9420 §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireFormat {
    PublicMessage,
    PrivateMessage,
    Welcome,
    GroupInfo,
    KeyPackage,
}

/// Content types (RFC 9420 §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentType {
    Application,
    Proposal,
    Commit,
}

/// The framing of a public or private message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageFacts {
    pub wire_format: WireFormat,
    pub group_id: Vec<u8>,
    pub epoch: u64,
    pub content_type: ContentType,
    /// For public messages: the sender's type (1 member, 2 external, 3 new member
    /// proposal, 4 new member commit) and index.
    pub sender: Option<(u8, Option<u32>)>,
}

fn content_type(value: u8) -> Result<ContentType, Malformed> {
    match value {
        1 => Ok(ContentType::Application),
        2 => Ok(ContentType::Proposal),
        3 => Ok(ContentType::Commit),
        _ => Err(Malformed::Value("content type")),
    }
}

/// The header of an `MLSMessage` carrying a public or private message. A private
/// message is parsed completely (its fields are opaque); a public message up to its
/// content type, the rest being OpenMLS's to validate.
pub fn message(bytes: &[u8]) -> Result<MessageFacts, Malformed> {
    let mut reader = Reader::new(bytes);
    if reader.u16()? != 1 {
        return Err(Malformed::Version);
    }
    match reader.u16()? {
        1 => {
            let group_id = reader.opaque()?.to_vec();
            let epoch = reader.u64()?;
            let sender_type = reader.u8()?;
            let index = match sender_type {
                1 | 2 => Some(reader.u32()?),
                3 | 4 => None,
                _ => return Err(Malformed::Value("sender type")),
            };
            reader.opaque()?; // authenticated data
            let content_type = content_type(reader.u8()?)?;
            Ok(MessageFacts {
                wire_format: WireFormat::PublicMessage,
                group_id,
                epoch,
                content_type,
                sender: Some((sender_type, index)),
            })
        }
        2 => {
            let group_id = reader.opaque()?.to_vec();
            let epoch = reader.u64()?;
            let content_type = content_type(reader.u8()?)?;
            reader.opaque()?; // authenticated data
            reader.opaque()?; // encrypted sender data
            reader.opaque()?; // ciphertext
            reader.finish()?;
            Ok(MessageFacts {
                wire_format: WireFormat::PrivateMessage,
                group_id,
                epoch,
                content_type,
                sender: None,
            })
        }
        3 => Err(Malformed::Value("a Welcome where a message was expected")),
        4 => Err(Malformed::Value("a GroupInfo where a message was expected")),
        5 => Err(Malformed::Value(
            "a KeyPackage where a message was expected",
        )),
        _ => Err(Malformed::Value("wire format")),
    }
}

/// The wire format of an `MLSMessage`, without parsing its body.
pub fn wire_format(bytes: &[u8]) -> Result<WireFormat, Malformed> {
    let mut reader = Reader::new(bytes);
    if reader.u16()? != 1 {
        return Err(Malformed::Version);
    }
    match reader.u16()? {
        1 => Ok(WireFormat::PublicMessage),
        2 => Ok(WireFormat::PrivateMessage),
        3 => Ok(WireFormat::Welcome),
        4 => Ok(WireFormat::GroupInfo),
        5 => Ok(WireFormat::KeyPackage),
        _ => Err(Malformed::Value("wire format")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_are_minimal() {
        assert_eq!(Reader::new(&[0x25]).varint(), Ok(0x25));
        assert_eq!(Reader::new(&[0x40, 0x25]).varint(), Err(Malformed::VarInt));
        assert_eq!(Reader::new(&[0x7b, 0xbd]).varint(), Ok(15293));
        assert_eq!(Reader::new(&[0x80, 0x00, 0x40, 0x00]).varint(), Ok(16384));
        assert_eq!(
            Reader::new(&[0x80, 0x00, 0x3f, 0xff]).varint(),
            Err(Malformed::VarInt)
        );
        assert_eq!(Reader::new(&[0xc0; 8]).varint(), Err(Malformed::VarInt));
        assert_eq!(Reader::new(&[0x41]).varint(), Err(Malformed::Truncated));
    }

    #[test]
    fn a_tree_costs_what_its_input_does() {
        // 100,000 blank nodes (a varint length, then one byte each): no member cap,
        // and nothing kept for blank leaves.
        let count = 100_000usize;
        let mut tree = vec![
            0x80 | (count >> 24) as u8,
            (count >> 16) as u8,
            (count >> 8) as u8,
            count as u8,
        ];
        tree.extend(std::iter::repeat_n(0u8, count));
        let leaves = ratchet_tree(&tree).unwrap();
        assert!(leaves.is_empty());
        assert_eq!(leaves.width as usize, count / 2);
        // A node type in a blank's place is refused, not skipped.
        let last = tree.len() - 1;
        tree[last] = 1;
        assert!(ratchet_tree(&tree).is_err());
    }

    #[test]
    fn garbage_is_refused_without_panicking() {
        for bytes in [&[][..], &[0, 1], &[0, 1, 0, 0x4f, 0x10], &[0xff; 64]] {
            assert!(key_package(bytes).is_err());
            assert!(message(bytes).is_err());
            assert!(ratchet_tree(bytes).is_err() || bytes.is_empty());
        }
    }
}
