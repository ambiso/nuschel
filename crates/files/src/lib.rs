// SPDX-License-Identifier: EUPL-1.2
//! The `files/1` object format (docs/files.md).
//!
//! Every file attached on a Nuschel server is an *object*: the file, padded to a
//! length from [`padme`], cut into chunks of 64 KiB and encrypted with a key of its
//! own that travels in the message (inside MLS in end-to-end conversations). The same
//! format serves files kept on the server and files relayed live from the sender's
//! device. The server checks each ciphertext chunk against the object's [`Root`] and
//! key [`Commitment`] as it arrives, before storing, caching or forwarding it, without
//! the key.
//!
//! # Chunks
//!
//! An object of `object_len` bytes has `n = max(1, ceil(object_len / 65536))` chunks,
//! at most 2^32, so `object_len` is at most 2^48 bytes ([`MAX_OBJECT`]). Chunk `i`
//! holds the plaintext bytes `[i * 65536, min((i + 1) * 65536, object_len))`. Only the
//! last chunk may be shorter. An empty object has one empty chunk. [`Layout`] does
//! this arithmetic.
//!
//! # Encryption
//!
//! Every object has its own random 32-byte [`Key`]. Chunks are encrypted with the
//! STREAM construction (Hoang, Reyhanitabar, Rogaway and Vizár, "Online
//! Authenticated-Encryption and its Nonce-Reuse Misuse-Resistance", CRYPTO 2015) over
//! ChaCha20-Poly1305 (RFC 8439), with empty associated data. The 12-byte nonce of
//! chunk `i` is seven zero bytes, `i` as a big-endian `u32`, and a flag byte: 1 for
//! the last chunk `n - 1`, otherwise 0. The ciphertext chunk `c_i` is the ciphertext
//! followed by the 16-byte tag, so it is 16 bytes longer than the plaintext chunk.
//!
//! The index in the nonce stops chunks from being moved. The flag stops the object
//! from being cut short or extended at a chunk boundary. The nonce has no random part
//! because a key encrypts one object only. A key must never encrypt a second object,
//! and a chunk must never be sealed again with other plaintext. Sealing is
//! deterministic, so an uploader may hash every chunk first and seal each one again
//! while uploading, but it must compare the [`leaf`] of the second ciphertext with the
//! tree before sending it: if the file changed in between, the two ciphertexts share
//! a nonce.
//!
//! # Root
//!
//! The key's [`Commitment`] is `C = SHA-256(0x03 || "nuschel files/1 key" || key)`,
//! 32 bytes. It reveals nothing useful about a random 256-bit key, so the server may
//! hold it. The leaf hash of chunk `i` is `L_i = SHA-256(0x00 || c_i)`. The leaves
//! form the Merkle tree of RFC 9162 §2.1.1. The tree hash of one leaf is the leaf hash
//! itself. For `n > 1` leaves, with `k` the largest power of two smaller than `n`,
//! `MTH(L[0:n]) = SHA-256(0x01 || MTH(L[0:k]) || MTH(L[k:n]))`. The object's root is
//!
//! ```text
//! R = SHA-256(0x02 || "nuschel files/1 root" || u8(16) || u64_be(object_len) || C || MTH)
//! ```
//!
//! where 16 is [`CHUNK_LOG2`]. The root binds the chunk size and the object length,
//! so it fixes the number of chunks and the length of every chunk.
//!
//! The root binds the key because ChaCha20-Poly1305 does not: one ciphertext can be
//! crafted to decrypt under two keys (Len, Grubbs and Ristenpart, "Partitioning Oracle
//! Attacks", USENIX Security 2021). An owner may reference one object from several
//! messages, and without `C` it could give each recipient another key, and so another
//! plaintext, for the same root. With `C` a root fixes the key and every ciphertext
//! chunk, and so the plaintext. The sender puts the key, the root and the file's
//! length into the same end-to-end encrypted message.
//!
//! # Proofs
//!
//! The [`Proof`] for chunk `i` is its RFC 9162 §2.1.3.1 inclusion path: the sibling
//! hashes from the leaf upwards, at most 32 ([`MAX_PROOF`]). [`Root::verify`] needs
//! the commitment, not the key. It checks the length of the ciphertext chunk for its
//! index and the number of hashes in the proof for its index, walks the path as RFC
//! 9162 §2.1.3.2 does, and recomputes the root with the object length and the
//! commitment. A [`Tree`] keeps every level of the tree, about 64 bytes per chunk, and
//! answers a proof without hashing.
//!
//! # Use
//!
//! - **Uploader:** [`Key::generate`]; [`Layout::new`] with the padded length; a
//!   [`TreeBuilder`] for the layout and [`Key::commitment`]; [`seal`] every chunk and
//!   push its [`leaf`]. The server gets the [`Tree::root`] and the
//!   [`Tree::commitment`] when the upload starts and every chunk with its
//!   [`Tree::proof`]; the message gets the key, the root and the file's length.
//! - **Server:** the [`Layout`] of the declared length, and [`Root::verify`] with the
//!   commitment for every chunk it receives.
//! - **Recipient:** [`Key::commitment`] of the key in the message, never a commitment
//!   from the server; [`Root::verify`] with it, then [`open`]. A wrong key fails the
//!   verification of the very first chunk.
//!
//! Nothing here does I/O.

use std::fmt;

use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::digest::{Context, SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use thiserror::Error;
use zeroize::Zeroize;

/// The base-2 logarithm of [`CHUNK`]. Fixed in `files/1`; the root commits to it.
pub const CHUNK_LOG2: u8 = 16;
/// Plaintext bytes in every chunk but the last: 64 KiB.
pub const CHUNK: usize = 1 << CHUNK_LOG2;
/// Bytes of the Poly1305 tag at the end of every ciphertext chunk.
pub const TAG: usize = 16;
/// Bytes of a [`Key`].
pub const KEY: usize = 32;
/// Bytes of a SHA-256 [`Hash`](type@Hash).
pub const HASH: usize = 32;
/// The most chunks an object can have: the chunk index in the nonce is a `u32`.
pub const MAX_CHUNKS: u64 = 1 << 32;
/// The longest object: [`MAX_CHUNKS`] full chunks, 2^48 bytes (256 TiB).
pub const MAX_OBJECT: u64 = MAX_CHUNKS << CHUNK_LOG2;
/// The most hashes in a [`Proof`]: the height of a tree of [`MAX_CHUNKS`] leaves.
pub const MAX_PROOF: usize = 32;

/// A SHA-256 output: a leaf hash, an inner node, or a root.
pub type Hash = [u8; HASH];

const LEAF_PREFIX: u8 = 0x00;
const NODE_PREFIX: u8 = 0x01;
const ROOT_PREFIX: u8 = 0x02;
const ROOT_LABEL: &[u8] = b"nuschel files/1 root";
const COMMITMENT_PREFIX: u8 = 0x03;
const COMMITMENT_LABEL: &[u8] = b"nuschel files/1 key";

/// Why an operation on an object failed.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum Error {
    /// The object would have more than [`MAX_CHUNKS`] chunks.
    #[error("an object of {object_len} bytes has more than 2^32 chunks")]
    TooLarge { object_len: u64 },
    /// The chunk index is not below the number of chunks.
    #[error("chunk {index} does not exist in an object of {chunks} chunks")]
    BadIndex { index: u64, chunks: u64 },
    /// A plaintext or ciphertext chunk has the wrong length for its index.
    #[error("the chunk has {actual} bytes, expected {expected}")]
    WrongLength { expected: usize, actual: usize },
    /// A [`TreeBuilder`] got another number of leaves than the object has chunks.
    #[error("{actual} leaves for an object of {expected} chunks")]
    LeafCount { expected: u64, actual: u64 },
    /// Encoded proof bytes are not a whole number of hashes, or more than [`MAX_PROOF`].
    #[error("a proof is at most 32 hashes of 32 bytes, not {len} bytes")]
    ProofEncoding { len: usize },
    /// The chunk and proof do not lead to the root: the chunk is not the one at this
    /// index of this object, the proof is wrong, or the commitment is not that of the
    /// object's key.
    #[error("the chunk is not part of the object with this root at this index")]
    Proof,
    /// The ciphertext does not decrypt with this key at this index.
    #[error("the chunk does not decrypt with this key at this index")]
    Authentication,
    /// ring failed where it should not: no system random source.
    #[error("the cryptographic library failed")]
    Crypto,
}

/// The key of one object: 32 random bytes.
///
/// It is erased from memory when dropped, and `Debug` does not show it.
#[derive(Clone)]
pub struct Key([u8; KEY]);

impl Key {
    /// A new random key from the operating system.
    pub fn generate() -> Result<Self, Error> {
        let mut bytes = [0; KEY];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| Error::Crypto)?;
        Ok(Self(bytes))
    }

    /// The key with these bytes, as carried in the end-to-end message.
    pub fn from_bytes(bytes: [u8; KEY]) -> Self {
        Self(bytes)
    }

    /// The key's bytes, to put into the end-to-end message.
    pub fn as_bytes(&self) -> &[u8; KEY] {
        &self.0
    }

    /// The key's commitment, which goes into the root:
    /// `SHA-256(0x03 || "nuschel files/1 key" || key)`.
    pub fn commitment(&self) -> Commitment {
        let mut hash = Context::new(&SHA256);
        hash.update(&[COMMITMENT_PREFIX]);
        hash.update(COMMITMENT_LABEL);
        hash.update(&self.0);
        Commitment(finish(hash))
    }

    fn aead(&self) -> Result<LessSafeKey, Error> {
        let key = UnboundKey::new(&CHACHA20_POLY1305, &self.0).map_err(|_| Error::Crypto)?;
        Ok(LessSafeKey::new(key))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(..)")
    }
}

/// The commitment to an object's [`Key`], from [`Key::commitment`]. The root binds it,
/// so that one root opens under one key only.
///
/// It reveals nothing useful about the key, so the server holds it to verify chunks.
/// A recipient computes it from the key it holds.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Commitment(Hash);

impl Commitment {
    /// The commitment with these bytes, as the server receives it next to the root.
    pub fn from_bytes(bytes: Hash) -> Self {
        Self(bytes)
    }

    /// The commitment's bytes.
    pub fn as_bytes(&self) -> &Hash {
        &self.0
    }
}

impl fmt::Display for Commitment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        hex(&self.0, f)
    }
}

impl fmt::Debug for Commitment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Commitment({self})")
    }
}

/// How an object of a given length is cut into chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Layout {
    object_len: u64,
    chunks: u64,
}

impl Layout {
    /// The layout of an object of `object_len` plaintext bytes, padding included.
    pub fn new(object_len: u64) -> Result<Self, Error> {
        if object_len > MAX_OBJECT {
            return Err(Error::TooLarge { object_len });
        }
        let chunks = object_len.div_ceil(CHUNK as u64).max(1);
        Ok(Self { object_len, chunks })
    }

    /// Plaintext bytes in the object.
    pub fn object_len(&self) -> u64 {
        self.object_len
    }

    /// The number of chunks, from 1 to [`MAX_CHUNKS`].
    pub fn chunks(&self) -> u64 {
        self.chunks
    }

    /// Whether `index` is the last chunk. False for an index beyond the object.
    pub fn is_last(&self, index: u64) -> bool {
        index == self.chunks - 1
    }

    /// Where chunk `index` starts in the plaintext.
    pub fn offset(&self, index: u64) -> Result<u64, Error> {
        self.check(index)?;
        Ok(index << CHUNK_LOG2)
    }

    /// Plaintext bytes in chunk `index`: [`CHUNK`], or fewer for the last chunk.
    pub fn plain_len(&self, index: u64) -> Result<usize, Error> {
        let rest = self.object_len - self.offset(index)?;
        // At most CHUNK, so the cast does not truncate.
        Ok(rest.min(CHUNK as u64) as usize)
    }

    /// Bytes in ciphertext chunk `index`: its plaintext and the tag.
    pub fn cipher_len(&self, index: u64) -> Result<usize, Error> {
        Ok(self.plain_len(index)? + TAG)
    }

    /// Where ciphertext chunk `index` starts when all chunks are stored in order.
    pub fn cipher_offset(&self, index: u64) -> Result<u64, Error> {
        self.check(index)?;
        Ok(index * (CHUNK + TAG) as u64)
    }

    /// Bytes of all ciphertext chunks together: the object and one tag per chunk.
    pub fn ciphertext_len(&self) -> u64 {
        self.object_len + self.chunks * TAG as u64
    }

    fn check(&self, index: u64) -> Result<(), Error> {
        if index < self.chunks {
            Ok(())
        } else {
            Err(Error::BadIndex {
                index,
                chunks: self.chunks,
            })
        }
    }

    /// The index as it goes into the nonce, and the last-chunk flag.
    fn position(&self, index: u64) -> Result<(u32, bool), Error> {
        self.check(index)?;
        let bad = Error::BadIndex {
            index,
            chunks: self.chunks,
        };
        Ok((u32::try_from(index).map_err(|_| bad)?, self.is_last(index)))
    }
}

/// Encrypts the plaintext of chunk `index`, which must have exactly
/// [`Layout::plain_len`] bytes, into its ciphertext chunk.
pub fn seal(key: &Key, layout: &Layout, index: u64, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
    let expected = layout.plain_len(index)?;
    if plaintext.len() != expected {
        return Err(Error::WrongLength {
            expected,
            actual: plaintext.len(),
        });
    }
    let (index, last) = layout.position(index)?;
    seal_at(key, index, last, plaintext)
}

fn seal_at(key: &Key, index: u32, last: bool, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
    let mut chunk = Vec::with_capacity(plaintext.len() + TAG);
    chunk.extend_from_slice(plaintext);
    key.aead()?
        .seal_in_place_append_tag(nonce(index, last), Aad::empty(), &mut chunk)
        .map_err(|_| Error::Crypto)?;
    Ok(chunk)
}

/// Decrypts ciphertext chunk `index`, which must have exactly [`Layout::cipher_len`]
/// bytes, into its plaintext.
pub fn open(key: &Key, layout: &Layout, index: u64, ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
    let expected = layout.cipher_len(index)?;
    if ciphertext.len() != expected {
        return Err(Error::WrongLength {
            expected,
            actual: ciphertext.len(),
        });
    }
    let (index, last) = layout.position(index)?;
    let mut chunk = ciphertext.to_vec();
    let plain_len = key
        .aead()?
        .open_in_place(nonce(index, last), Aad::empty(), &mut chunk)
        .map_err(|_| Error::Authentication)?
        .len();
    chunk.truncate(plain_len);
    Ok(chunk)
}

/// Seven zero bytes, the chunk index as a big-endian `u32`, and the last-chunk flag.
fn nonce(index: u32, last: bool) -> Nonce {
    let mut nonce = [0; NONCE_LEN];
    nonce[7..11].copy_from_slice(&index.to_be_bytes());
    nonce[11] = u8::from(last);
    Nonce::assume_unique_for_key(nonce)
}

/// The leaf hash of a ciphertext chunk: `SHA-256(0x00 || ciphertext)`.
pub fn leaf(ciphertext: &[u8]) -> Hash {
    let mut hash = Context::new(&SHA256);
    hash.update(&[LEAF_PREFIX]);
    hash.update(ciphertext);
    finish(hash)
}

/// An inner node: `SHA-256(0x01 || left || right)`.
fn node(left: &Hash, right: &Hash) -> Hash {
    let mut hash = Context::new(&SHA256);
    hash.update(&[NODE_PREFIX]);
    hash.update(left);
    hash.update(right);
    finish(hash)
}

fn finish(hash: Context) -> Hash {
    let mut out = [0; HASH];
    out.copy_from_slice(hash.finish().as_ref());
    out
}

/// Lowercase hex, for [`Root`] and [`Commitment`].
fn hex(hash: &Hash, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    hash.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
}

/// The number of hashes in the inclusion path of leaf `index` in a tree of `n` leaves.
fn path_len(index: u64, n: u64) -> usize {
    let (mut index, mut width, mut len) = (index, n, 0);
    while width > 1 {
        // A node without a right sibling moves up unchanged and adds no hash.
        if index ^ 1 < width {
            len += 1;
        }
        index >>= 1;
        width = width.div_ceil(2);
    }
    len
}

/// The tree hash from a leaf and its inclusion path, by RFC 9162 §2.1.3.2.
fn tree_hash_from_path(index: u64, n: u64, leaf: &Hash, path: &[Hash]) -> Option<Hash> {
    if index >= n {
        return None;
    }
    let (mut fnode, mut snode) = (index, n - 1);
    let mut hash = *leaf;
    for sibling in path {
        if snode == 0 {
            return None;
        }
        if fnode & 1 == 1 || fnode == snode {
            hash = node(sibling, &hash);
            while fnode & 1 == 0 && fnode != 0 {
                fnode >>= 1;
                snode >>= 1;
            }
        } else {
            hash = node(&hash, sibling);
        }
        fnode >>= 1;
        snode >>= 1;
    }
    (snode == 0).then_some(hash)
}

/// Collects the leaf hashes of an object in chunk order.
#[derive(Debug)]
pub struct TreeBuilder {
    layout: Layout,
    commitment: Commitment,
    leaves: Vec<Hash>,
}

impl TreeBuilder {
    /// A builder for the object with this layout, encrypted with the key of this
    /// commitment.
    pub fn new(layout: Layout, commitment: Commitment) -> Self {
        // A declared length is not trusted with a large allocation up front.
        let capacity = layout.chunks().min(1 << 16) as usize;
        Self {
            layout,
            commitment,
            leaves: Vec::with_capacity(capacity),
        }
    }

    /// Adds the [`leaf`] hash of the next chunk.
    pub fn push(&mut self, leaf: Hash) {
        self.leaves.push(leaf);
    }

    /// The tree, once every chunk has its leaf.
    pub fn finish(self) -> Result<Tree, Error> {
        let actual = self.leaves.len() as u64;
        if actual != self.layout.chunks() {
            return Err(Error::LeafCount {
                expected: self.layout.chunks(),
                actual,
            });
        }
        // Level by level from the leaves: pairs become their parent, and a last node
        // without a sibling moves up unchanged. This gives the tree of RFC 9162 §2.1.1.
        let mut levels = vec![self.leaves];
        while let Some(level) = levels.last().filter(|level| level.len() > 1) {
            let parents = level
                .chunks(2)
                .map(|pair| match pair {
                    [left, right] => node(left, right),
                    _ => pair[0],
                })
                .collect();
            levels.push(parents);
        }
        let tree_hash = levels.last().and_then(|top| top.first()).copied();
        let tree_hash = tree_hash.ok_or(Error::LeafCount {
            expected: self.layout.chunks(),
            actual,
        })?;
        Ok(Tree {
            layout: self.layout,
            commitment: self.commitment,
            root: Root::compute(&self.layout, &self.commitment, &tree_hash),
            levels,
        })
    }
}

/// The Merkle tree of an object: its root and every inclusion proof.
pub struct Tree {
    layout: Layout,
    commitment: Commitment,
    root: Root,
    /// `levels[0]` are the leaves; the last level is the tree hash alone.
    levels: Vec<Vec<Hash>>,
}

impl Tree {
    /// The object's layout.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The commitment to the object's key, which the server gets with the root.
    pub fn commitment(&self) -> Commitment {
        self.commitment
    }

    /// The object's root.
    pub fn root(&self) -> Root {
        self.root
    }

    /// The leaf hashes in chunk order.
    pub fn leaves(&self) -> &[Hash] {
        &self.levels[0]
    }

    /// The inclusion proof of chunk `index`.
    pub fn proof(&self, index: u64) -> Result<Proof, Error> {
        self.layout.check(index)?;
        let mut position = usize::try_from(index).map_err(|_| Error::BadIndex {
            index,
            chunks: self.layout.chunks(),
        })?;
        let mut path = Vec::with_capacity(MAX_PROOF);
        for level in &self.levels[..self.levels.len() - 1] {
            if let Some(sibling) = level.get(position ^ 1) {
                path.push(*sibling);
            }
            position >>= 1;
        }
        Ok(Proof(path))
    }
}

impl fmt::Debug for Tree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tree")
            .field("layout", &self.layout)
            .field("commitment", &self.commitment)
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

/// The root of an object: a commitment to its length, its chunk size, its key and
/// every ciphertext chunk.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Root(Hash);

impl Root {
    /// The root with these bytes, as carried in the end-to-end message.
    pub fn from_bytes(bytes: Hash) -> Self {
        Self(bytes)
    }

    /// The root's bytes.
    pub fn as_bytes(&self) -> &Hash {
        &self.0
    }

    fn compute(layout: &Layout, commitment: &Commitment, tree_hash: &Hash) -> Self {
        let mut hash = Context::new(&SHA256);
        hash.update(&[ROOT_PREFIX]);
        hash.update(ROOT_LABEL);
        hash.update(&[CHUNK_LOG2]);
        hash.update(&layout.object_len().to_be_bytes());
        hash.update(commitment.as_bytes());
        hash.update(tree_hash);
        Self(finish(hash))
    }

    /// Checks that `ciphertext` is chunk `index` of the object with this root, layout
    /// and key commitment. Needs no key; a recipient passes [`Key::commitment`] of the
    /// key it holds.
    pub fn verify(
        &self,
        layout: &Layout,
        commitment: &Commitment,
        index: u64,
        ciphertext: &[u8],
        proof: &Proof,
    ) -> Result<(), Error> {
        let expected = layout.cipher_len(index)?;
        if ciphertext.len() != expected {
            return Err(Error::WrongLength {
                expected,
                actual: ciphertext.len(),
            });
        }
        self.verify_leaf(layout, commitment, index, &leaf(ciphertext), proof)
    }

    /// Checks that `leaf` is the leaf hash of chunk `index`. [`Root::verify`] also
    /// checks the length of the chunk; a caller of this must do that itself.
    pub fn verify_leaf(
        &self,
        layout: &Layout,
        commitment: &Commitment,
        index: u64,
        leaf: &Hash,
        proof: &Proof,
    ) -> Result<(), Error> {
        layout.check(index)?;
        if proof.0.len() != path_len(index, layout.chunks()) {
            return Err(Error::Proof);
        }
        let tree_hash =
            tree_hash_from_path(index, layout.chunks(), leaf, &proof.0).ok_or(Error::Proof)?;
        if Self::compute(layout, commitment, &tree_hash) == *self {
            Ok(())
        } else {
            Err(Error::Proof)
        }
    }
}

impl fmt::Display for Root {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        hex(&self.0, f)
    }
}

impl fmt::Debug for Root {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Root({self})")
    }
}

/// The inclusion proof of one chunk: sibling hashes from the leaf upwards.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Proof(Vec<Hash>);

impl Proof {
    /// The sibling hashes, from the leaf upwards.
    pub fn hashes(&self) -> &[Hash] {
        &self.0
    }

    /// The hashes concatenated: 32 bytes each.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.0.concat()
    }

    /// Parses concatenated hashes: a multiple of 32 bytes, at most [`MAX_PROOF`]
    /// hashes. Whether the number fits the chunk is checked by [`Root::verify`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let (hashes, rest) = bytes.as_chunks::<HASH>();
        if !rest.is_empty() || hashes.len() > MAX_PROOF {
            return Err(Error::ProofEncoding { len: bytes.len() });
        }
        Ok(Self(hashes.to_vec()))
    }
}

/// Encrypts a whole object held in memory: every chunk and its tree.
///
/// For tests and small objects; large files go chunk by chunk through [`seal`] and a
/// [`TreeBuilder`].
pub fn encrypt_object(key: &Key, object: &[u8]) -> Result<(Tree, Vec<Vec<u8>>), Error> {
    let layout = Layout::new(object.len() as u64)?;
    let mut tree = TreeBuilder::new(layout, key.commitment());
    let mut chunks = Vec::new();
    // An empty object still has one (empty) chunk.
    let pieces = object
        .chunks(CHUNK)
        .chain(object.is_empty().then_some(object));
    for (index, piece) in (0..).zip(pieces) {
        let chunk = seal(key, &layout, index, piece)?;
        tree.push(leaf(&chunk));
        chunks.push(chunk);
    }
    Ok((tree.finish()?, chunks))
}

/// The padded length of a file of `len` bytes: Padmé (Nikitin, Barman, Lueks, Underwood,
/// Hubaux and Ford, "Reducing Metadata Leakage from Encrypted Files and Communication
/// with PURBs", PETS 2019).
///
/// With `E = floor(log2 len)` and `S = floor(log2 E) + 1`, the length is rounded up to
/// a multiple of `2^(E - S)`. The padded length reveals `O(log log len)` bits of the
/// length and adds less than 12 % (the most is 129 bytes padded to 144). Lengths
/// below 2 stay as they are. The result is never smaller than `len`, never decreases
/// as `len` grows, and is at most [`MAX_OBJECT`] when `len` is. Above `2^64 - 2^57`,
/// where rounding up would overflow, the result is `u64::MAX`; no object is that long.
pub fn padme(len: u64) -> u64 {
    if len < 2 {
        return len;
    }
    let e = len.ilog2();
    let s = e.ilog2() + 1;
    let mask = (1u64 << (e - s)) - 1;
    len.checked_add(mask).map_or(u64::MAX, |len| len & !mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const LENGTHS: [usize; 7] = [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK, 3 * CHUNK + 5];

    /// Bytes that differ from chunk to chunk.
    fn object(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect()
    }

    /// RFC 9162 §2.1.1 MTH and §2.1.3.1 PATH, from the recursive definitions. Subtree
    /// hashes are remembered, so that every path of 260 trees stays fast.
    struct Naive {
        leaves: Vec<Hash>,
        subtrees: HashMap<(usize, usize), Hash>,
    }

    impl Naive {
        fn new(leaves: Vec<Hash>) -> Self {
            let subtrees = HashMap::new();
            Self { leaves, subtrees }
        }

        /// The largest power of two smaller than `n`.
        fn split(n: usize) -> usize {
            1 << (n - 1).ilog2()
        }

        /// MTH(D[lo:hi]).
        fn mth(&mut self, lo: usize, hi: usize) -> Hash {
            if hi - lo == 1 {
                return self.leaves[lo];
            }
            if let Some(hash) = self.subtrees.get(&(lo, hi)) {
                return *hash;
            }
            let k = Self::split(hi - lo);
            let hash = node(&self.mth(lo, lo + k), &self.mth(lo + k, hi));
            self.subtrees.insert((lo, hi), hash);
            hash
        }

        /// PATH(m, D[lo:hi]).
        fn path(&mut self, m: usize, lo: usize, hi: usize) -> Vec<Hash> {
            if hi - lo == 1 {
                return Vec::new();
            }
            let k = Self::split(hi - lo);
            let (mut path, sibling) = if m < k {
                (self.path(m, lo, lo + k), self.mth(lo + k, hi))
            } else {
                (self.path(m - k, lo + k, hi), self.mth(lo, lo + k))
            };
            path.push(sibling);
            path
        }
    }

    fn layout_of_chunks(n: u64) -> Layout {
        Layout::new(n * CHUNK as u64).unwrap()
    }

    #[test]
    fn round_trip() {
        let key = Key::generate().unwrap();
        for len in LENGTHS {
            let data = object(len);
            let (tree, chunks) = encrypt_object(&key, &data).unwrap();
            let layout = *tree.layout();
            assert_eq!(layout, Layout::new(len as u64).unwrap());
            assert_eq!(layout.chunks(), chunks.len() as u64);
            assert_eq!(layout.chunks(), len.div_ceil(CHUNK).max(1) as u64);
            let total: usize = chunks.iter().map(Vec::len).sum();
            assert_eq!(total as u64, layout.ciphertext_len());

            let root = Root::from_bytes(*tree.root().as_bytes());
            let commitment = Commitment::from_bytes(*tree.commitment().as_bytes());
            assert_eq!(commitment, key.commitment());
            let mut plaintext = Vec::new();
            for (index, chunk) in (0..).zip(&chunks) {
                assert_eq!(chunk.len(), layout.cipher_len(index).unwrap());
                let proof = Proof::from_bytes(&tree.proof(index).unwrap().to_bytes()).unwrap();
                root.verify(&layout, &commitment, index, chunk, &proof)
                    .unwrap();
                let piece = open(&key, &layout, index, chunk).unwrap();
                assert_eq!(piece.len(), layout.plain_len(index).unwrap());
                assert_eq!(plaintext.len() as u64, layout.offset(index).unwrap());
                plaintext.extend(piece);
            }
            assert_eq!(plaintext, data, "object of {len} bytes");
        }
    }

    #[test]
    fn empty_object_is_one_tag() {
        let key = Key::from_bytes([7; KEY]);
        let (tree, chunks) = encrypt_object(&key, &[]).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), TAG);
        assert!(tree.proof(0).unwrap().hashes().is_empty());
        assert_eq!(open(&key, tree.layout(), 0, &chunks[0]).unwrap(), b"");
    }

    /// Any change to the format changes these values. All were recomputed when frozen
    /// by a separate implementation (Python: RFC 8439 by hand, hashlib, RFC 9162).
    #[test]
    fn frozen_vector() {
        let key = Key::from_bytes(std::array::from_fn(|i| i as u8));
        assert_eq!(
            key.commitment().to_string(),
            "375c22f2ecb5e9dd79610fe9d636315bc967300f8661508efd3fd51216e1b517"
        );

        let data: Vec<u8> = (0..70_000).map(|i| (i as u8).wrapping_mul(7)).collect();
        let (tree, chunks) = encrypt_object(&key, &data).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].len(), 70_000 - CHUNK + TAG);
        assert_eq!(
            tree.root().to_string(),
            "77d05554ed33b44d5c2d9ffe5789dd6c5e9036cb2d0399caa2ec3e8261a75eb6"
        );

        let (tree, _) = encrypt_object(&key, &[]).unwrap();
        assert_eq!(
            tree.root().to_string(),
            "2dfdfaa1ef96182688a43097f2843445c605155e8644f7b7a87c2935937f2269"
        );
    }

    #[test]
    fn key_commitment() {
        let key = Key::from_bytes([0x11; KEY]);
        assert_eq!(key.commitment(), Key::from_bytes([0x11; KEY]).commitment());
        assert_eq!(key.commitment(), key.clone().commitment());
        let mut other = [0x11; KEY];
        other[KEY - 1] ^= 1;
        assert_ne!(key.commitment(), Key::from_bytes(other).commitment());
        assert_ne!(
            Key::generate().unwrap().commitment(),
            Key::generate().unwrap().commitment()
        );
        // Not the key itself, and printed like a root.
        let commitment = key.commitment();
        assert_ne!(commitment.as_bytes(), key.as_bytes());
        assert_eq!(Commitment::from_bytes(*commitment.as_bytes()), commitment);
        assert_eq!(commitment.to_string().len(), 2 * HASH);
        assert_eq!(
            format!("{commitment:?}"),
            format!("Commitment({commitment})")
        );
    }

    #[test]
    fn tampering() {
        let key = Key::generate().unwrap();
        let data = object(3 * CHUNK + 5);
        let (tree, chunks) = encrypt_object(&key, &data).unwrap();
        let layout = *tree.layout();
        let root = tree.root();
        let commitment = key.commitment();
        let proofs: Vec<Proof> = (0..4).map(|i| tree.proof(i).unwrap()).collect();

        // A flipped bit, in the ciphertext and in the tag.
        for position in [0, 100, CHUNK + TAG - 1] {
            let mut chunk = chunks[0].clone();
            chunk[position] ^= 1;
            assert_eq!(
                root.verify(&layout, &commitment, 0, &chunk, &proofs[0]),
                Err(Error::Proof)
            );
            assert_eq!(open(&key, &layout, 0, &chunk), Err(Error::Authentication));
        }

        // Swapped chunks.
        for proof in [&proofs[0], &proofs[1]] {
            assert_eq!(
                root.verify(&layout, &commitment, 0, &chunks[1], proof),
                Err(Error::Proof)
            );
        }
        assert_eq!(
            open(&key, &layout, 0, &chunks[1]),
            Err(Error::Authentication)
        );
        assert_eq!(
            open(&key, &layout, 1, &chunks[0]),
            Err(Error::Authentication)
        );

        // A chunk at the wrong index, with its own proof or that of the index.
        for (index, proof) in [(2, &proofs[1]), (2, &proofs[2]), (0, &proofs[1])] {
            assert_eq!(
                root.verify(&layout, &commitment, index, &chunks[1], proof),
                Err(Error::Proof)
            );
        }
        assert_eq!(
            root.verify(&layout, &commitment, 4, &chunks[1], &proofs[1]),
            Err(Error::BadIndex {
                index: 4,
                chunks: 4
            })
        );

        // A truncated last chunk.
        let short = &chunks[3][..chunks[3].len() - 1];
        let wrong_length = Error::WrongLength {
            expected: 5 + TAG,
            actual: 4 + TAG,
        };
        assert_eq!(
            root.verify(&layout, &commitment, 3, short, &proofs[3]),
            Err(wrong_length)
        );
        assert_eq!(open(&key, &layout, 3, short), Err(wrong_length));

        // An object cut short at a chunk boundary: chunk 2 was not sealed as the last.
        let cut = Layout::new(3 * CHUNK as u64).unwrap();
        assert_eq!(open(&key, &cut, 2, &chunks[2]), Err(Error::Authentication));

        // A proof one hash short or long.
        for index in 0..4 {
            let mut hashes = proofs[index as usize].hashes().to_vec();
            hashes.pop();
            let short = Proof(hashes.clone());
            assert_eq!(
                root.verify(&layout, &commitment, index, &chunks[index as usize], &short),
                Err(Error::Proof)
            );
            hashes.extend([[0; HASH], [0; HASH]]);
            let long = Proof(hashes);
            assert_eq!(
                root.verify(&layout, &commitment, index, &chunks[index as usize], &long),
                Err(Error::Proof)
            );
        }

        // The root of another object length (same chunk count, same leaves).
        let other = Layout::new(3 * CHUNK as u64 + 6).unwrap();
        assert_eq!(other.chunks(), layout.chunks());
        assert_eq!(
            root.verify(&other, &commitment, 0, &chunks[0], &proofs[0]),
            Err(Error::Proof)
        );
        let mut builder = TreeBuilder::new(other, commitment);
        tree.leaves().iter().for_each(|leaf| builder.push(*leaf));
        assert_ne!(builder.finish().unwrap().root(), root);

        // Another key: it does not open the chunks, the same chunks and tree under its
        // commitment have another root, and its commitment verifies no chunk.
        let other_key = Key::generate().unwrap();
        let other_commitment = other_key.commitment();
        for (index, chunk) in (0..).zip(&chunks) {
            assert_eq!(
                open(&other_key, &layout, index, chunk),
                Err(Error::Authentication)
            );
            assert_eq!(
                root.verify(
                    &layout,
                    &other_commitment,
                    index,
                    chunk,
                    &proofs[index as usize]
                ),
                Err(Error::Proof)
            );
        }
        let mut builder = TreeBuilder::new(layout, other_commitment);
        tree.leaves().iter().for_each(|leaf| builder.push(*leaf));
        let other_tree = builder.finish().unwrap();
        assert_eq!(other_tree.leaves(), tree.leaves());
        assert_eq!(other_tree.levels.last(), tree.levels.last());
        assert_ne!(other_tree.root(), root);
        let mut builder = TreeBuilder::new(layout, commitment);
        tree.leaves().iter().for_each(|leaf| builder.push(*leaf));
        assert_eq!(builder.finish().unwrap().root(), root);

        // The last-chunk flag: the last chunk sealed as not last, an earlier one as last.
        let last_plain = &data[3 * CHUNK..];
        let first_plain = &data[CHUNK..2 * CHUNK];
        for (index, last, plain) in [(3, false, last_plain), (1, true, first_plain)] {
            let chunk = seal_at(&key, index as u32, last, plain).unwrap();
            assert_eq!(chunk.len(), chunks[index as usize].len());
            assert_eq!(
                open(&key, &layout, index, &chunk),
                Err(Error::Authentication)
            );
            assert_eq!(
                root.verify(&layout, &commitment, index, &chunk, &proofs[index as usize]),
                Err(Error::Proof)
            );
        }
        // The honest seal is reproducible.
        assert_eq!(seal(&key, &layout, 3, last_plain).unwrap(), chunks[3]);
    }

    #[test]
    fn seal_checks_length_and_index() {
        let key = Key::from_bytes([1; KEY]);
        let layout = Layout::new(CHUNK as u64 + 5).unwrap();
        assert_eq!(
            seal(&key, &layout, 1, &[0; 6]),
            Err(Error::WrongLength {
                expected: 5,
                actual: 6
            })
        );
        assert_eq!(
            seal(&key, &layout, 0, &[0; 5]),
            Err(Error::WrongLength {
                expected: CHUNK,
                actual: 5
            })
        );
        assert_eq!(
            seal(&key, &layout, 2, &[]),
            Err(Error::BadIndex {
                index: 2,
                chunks: 2
            })
        );
        assert_eq!(
            open(&key, &layout, 2, &[0; TAG]),
            Err(Error::BadIndex {
                index: 2,
                chunks: 2
            })
        );
    }

    #[test]
    fn tree_shape() {
        let commitment = Key::from_bytes([3; KEY]).commitment();
        let other_commitment = Key::from_bytes([4; KEY]).commitment();
        for n in 1..=260u64 {
            let layout = layout_of_chunks(n);
            let leaves: Vec<Hash> = (0..n).map(|i| leaf(&i.to_be_bytes())).collect();
            let mut builder = TreeBuilder::new(layout, commitment);
            leaves.iter().for_each(|leaf| builder.push(*leaf));
            let tree = builder.finish().unwrap();
            let mut naive = Naive::new(leaves.clone());
            let mth = naive.mth(0, n as usize);
            assert_eq!(tree.levels.last().unwrap(), &[mth]);
            assert_eq!(tree.root(), Root::compute(&layout, &commitment, &mth));
            assert_eq!(tree.commitment(), commitment);
            assert_eq!(tree.leaves(), &leaves[..]);

            let root = tree.root();
            for index in 0..n {
                let i = index as usize;
                let proof = tree.proof(index).unwrap();
                let expected = naive.path(i, 0, n as usize);
                assert_eq!(proof.hashes(), expected, "n {n}, index {index}");
                assert_eq!(proof.hashes().len(), path_len(index, n));
                root.verify_leaf(&layout, &commitment, index, &leaves[i], &proof)
                    .unwrap();
                assert_eq!(
                    root.verify_leaf(&layout, &other_commitment, index, &leaves[i], &proof),
                    Err(Error::Proof)
                );
                if n > 1 {
                    let next = (index + 1) % n;
                    let other_leaf = &leaves[next as usize];
                    assert_eq!(
                        root.verify_leaf(&layout, &commitment, index, other_leaf, &proof),
                        Err(Error::Proof)
                    );
                    assert_eq!(
                        root.verify_leaf(&layout, &commitment, next, &leaves[i], &proof),
                        Err(Error::Proof)
                    );
                }
            }
            assert!(tree.proof(n).is_err());
        }
    }

    #[test]
    fn proof_length_bound() {
        assert_eq!(path_len(0, 1), 0);
        for index in [0, 1, 12_345_678, MAX_CHUNKS / 2, MAX_CHUNKS - 1] {
            assert_eq!(path_len(index, MAX_CHUNKS), MAX_PROOF);
        }
        for n in [MAX_CHUNKS - 1, MAX_CHUNKS / 2 + 1, 1 << 20, 1_000_003] {
            for index in [0, 1, n / 2, n - 2, n - 1] {
                assert!(path_len(index, n) <= MAX_PROOF);
            }
        }
        // The last leaf of 2^k + 1 leaves sits right below the root.
        assert_eq!(path_len(MAX_CHUNKS / 2, MAX_CHUNKS / 2 + 1), 1);
    }

    #[test]
    fn proof_encoding() {
        assert_eq!(Proof::from_bytes(&[]).unwrap(), Proof::default());
        let hashes: Vec<Hash> = (0..MAX_PROOF as u8).map(|i| [i; HASH]).collect();
        let proof = Proof(hashes);
        let bytes = proof.to_bytes();
        assert_eq!(bytes.len(), MAX_PROOF * HASH);
        assert_eq!(Proof::from_bytes(&bytes).unwrap(), proof);
        for len in [1, HASH - 1, HASH + 1, (MAX_PROOF + 1) * HASH] {
            assert_eq!(
                Proof::from_bytes(&vec![0; len]),
                Err(Error::ProofEncoding { len })
            );
        }
    }

    #[test]
    fn layout() {
        let empty = Layout::new(0).unwrap();
        assert_eq!(empty.chunks(), 1);
        assert_eq!(empty.plain_len(0), Ok(0));
        assert_eq!(empty.cipher_len(0), Ok(TAG));
        assert!(empty.is_last(0));

        let layout = Layout::new(2 * CHUNK as u64 + 1).unwrap();
        assert_eq!(layout.chunks(), 3);
        assert_eq!(layout.offset(2), Ok(2 * CHUNK as u64));
        assert_eq!(layout.plain_len(1), Ok(CHUNK));
        assert_eq!(layout.plain_len(2), Ok(1));
        assert_eq!(layout.cipher_offset(2), Ok(2 * (CHUNK + TAG) as u64));
        assert_eq!(
            layout.ciphertext_len(),
            2 * CHUNK as u64 + 1 + 3 * TAG as u64
        );
        assert!(!layout.is_last(1));
        assert!(layout.is_last(2));
        assert!(!layout.is_last(3));
        let bad = Error::BadIndex {
            index: 3,
            chunks: 3,
        };
        assert_eq!(layout.plain_len(3), Err(bad));
        assert_eq!(layout.cipher_len(3), Err(bad));
        assert_eq!(layout.offset(3), Err(bad));
        assert_eq!(layout.cipher_offset(3), Err(bad));

        let largest = Layout::new(MAX_OBJECT).unwrap();
        assert_eq!(largest.chunks(), MAX_CHUNKS);
        assert_eq!(largest.plain_len(MAX_CHUNKS - 1), Ok(CHUNK));
        assert_eq!(largest.position(MAX_CHUNKS - 1), Ok((u32::MAX, true)));
        for object_len in [MAX_OBJECT + 1, u64::MAX] {
            assert_eq!(Layout::new(object_len), Err(Error::TooLarge { object_len }));
        }
    }

    #[test]
    fn tree_builder_counts_leaves() {
        let layout = layout_of_chunks(2);
        let commitment = Key::from_bytes([5; KEY]).commitment();
        let mut builder = TreeBuilder::new(layout, commitment);
        builder.push([0; HASH]);
        assert_eq!(
            builder.finish().unwrap_err(),
            Error::LeafCount {
                expected: 2,
                actual: 1
            }
        );
        let builder = TreeBuilder::new(Layout::new(0).unwrap(), commitment);
        assert_eq!(
            builder.finish().unwrap_err(),
            Error::LeafCount {
                expected: 1,
                actual: 0
            }
        );
    }

    #[test]
    fn key_is_not_printed() {
        let key = Key::from_bytes([0xab; KEY]);
        let printed = format!("{key:?}");
        assert!(
            !printed.contains("171") && !printed.contains("ab"),
            "{printed}"
        );
    }

    #[test]
    fn padme_values() {
        let known = [
            (0, 0),
            (1, 1),
            (2, 2),
            (3, 3),
            (4, 4),
            (7, 7),
            (8, 8),
            (9, 10),
            (11, 12),
            (15, 16),
            (17, 18),
            (33, 36),
            (129, 144),
            (1000, 1024),
            (1025, 1088),
            (65_537, 67_584),
            (1_000_000, 1_015_808),
            (1_000_000_000, 1_006_632_960),
            ((1 << 32) + 1, (1 << 32) + (1 << 26)),
            (1 << 63, 1 << 63),
            ((1 << 63) + 1, (1 << 63) + (1 << 57)),
            (u64::MAX - (1 << 57) + 1, u64::MAX - (1 << 57) + 1),
            (u64::MAX - (1 << 57) + 2, u64::MAX),
            (u64::MAX, u64::MAX),
            (MAX_OBJECT - 1, MAX_OBJECT),
            (MAX_OBJECT, MAX_OBJECT),
        ];
        for (len, padded) in known {
            assert_eq!(padme(len), padded, "padme({len})");
        }
    }

    #[test]
    fn padme_properties() {
        let check = |len: u64, previous: u64| {
            let padded = padme(len);
            assert!(padded >= len, "padme({len}) = {padded}");
            assert!(padded >= previous, "padme is not monotonic at {len}");
            if len >= 2 && padded != u64::MAX {
                // At most 12 % overhead.
                assert!(
                    u128::from(padded - len) * 100 <= u128::from(len) * 12,
                    "padme({len}) = {padded}"
                );
            }
            padded
        };
        let mut previous = 0;
        for len in 0..=1 << 20 {
            previous = check(len, previous);
        }
        for e in 2..64 {
            let base = 1u64 << e;
            let mut previous = padme(base - 2);
            for len in [
                base - 1,
                base,
                base + 1,
                base + base / 2 + 1,
                base + (base - 1),
            ] {
                previous = check(len, previous);
            }
        }
        let mut previous = padme(u64::MAX - 1_000_000);
        for len in u64::MAX - 1_000_000..=u64::MAX {
            previous = check(len, previous);
        }
        let mut previous = padme(u64::MAX - (1 << 57) - 1000);
        for len in u64::MAX - (1 << 57) - 1000..u64::MAX - (1 << 57) + 1000 {
            previous = check(len, previous);
        }
    }
}
