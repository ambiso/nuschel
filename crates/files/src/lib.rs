// SPDX-License-Identifier: EUPL-1.2
//! The `files/1` object format (docs/files.md).
//!
//! The 2026-11-11 Nuschel → Nuschl rename changes all application crypto
//! domain labels to `nuschl`. Vectors are regenerated under this namespace;
//! old experimental identities, proofs, MLS state and encrypted objects are
//! intentionally incompatible. The client archives old crypto stores.
//!
//! Every file attached on a Nuschl server is an *object*: the file, padded to a
//! length from [`padme`], cut into chunks and encrypted with a key of its own that
//! travels in the message (inside MLS in end-to-end conversations). Each sealed chunk
//! is a 64 KiB *piece*, and the pieces are verified by the Merkle tree of BitTorrent v2
//! (BEP 52). The same format serves files kept on the server and files relayed live
//! from the sender's device. The server checks each piece against the object's
//! [`Root`] and key [`Commitment`] as it arrives, before storing, caching or
//! forwarding it, without the key.
//!
//! # Chunks and pieces
//!
//! An object of `object_len` plaintext bytes (the file and its padding) has
//! `n = max(1, ceil(object_len / 65520))` chunks, at most 2^32, so `object_len` is at
//! most 65,520 × 2^32 bytes, just under 256 TiB ([`MAX_OBJECT`]). Chunk `i` holds the
//! plaintext bytes `[i * 65520, min((i + 1) * 65520, object_len))`. Only the last chunk
//! may be shorter. An empty object has one empty chunk.
//!
//! Chunk `i` is sealed into piece `i`: its ciphertext followed by the 16-byte tag. So
//! every piece but the last has exactly 65,536 bytes ([`PIECE`]), and the last has 16
//! to 65,536. The pieces in order are the *ciphertext object*, `object_len + 16 n`
//! bytes and never empty. [`Layout`] does this arithmetic.
//!
//! A plaintext chunk is [`CHUNK`] = 65,536 − 16 bytes so that the pieces fall exactly on
//! the 64 KiB pieces of a BitTorrent v2 torrent of the ciphertext object: each
//! BitTorrent piece is one sealed chunk, which can be verified, stored, served and
//! decrypted on its own.
//!
//! # Encryption
//!
//! Every object has its own random 32-byte [`Key`]. Chunks are encrypted with the
//! STREAM construction (Hoang, Reyhanitabar, Rogaway and Vizár, "Online
//! Authenticated-Encryption and its Nonce-Reuse Misuse-Resistance", CRYPTO 2015) over
//! ChaCha20-Poly1305 (RFC 8439), with empty associated data. The 12-byte nonce of
//! chunk `i` is seven zero bytes, `i` as a big-endian `u32`, and a flag byte: 1 for
//! the last chunk `n - 1`, otherwise 0.
//!
//! The index in the nonce stops pieces from being moved. The flag stops the object
//! from being cut short or extended at a piece boundary. The nonce has no random part
//! because a key encrypts one object only. A key must never encrypt a second object,
//! and a chunk must never be sealed again with other plaintext. Sealing is
//! deterministic, so an uploader may hash every piece first and seal each chunk again
//! while uploading, but it must compare the [`piece_hash`] of the second piece with
//! the tree before sending it: if the file changed in between, the two ciphertexts
//! share a nonce.
//!
//! # Tree
//!
//! The tree is BEP 52's Merkle tree of a file whose contents are the ciphertext
//! object. The ciphertext object is cut into 16 KiB blocks ([`BLOCK`]), the last
//! possibly shorter, never empty: `B = ceil(cipher_len / 16384)` blocks. The leaf of
//! block `j` is `L_j = SHA-256(block_j)`. The leaves are padded with 32-byte all-zero
//! values up to `P = B.next_power_of_two()`, and an inner node is
//! `SHA-256(left || right)`. The root of this perfect tree is the *pieces root*
//! ([`PiecesRoot`]); with one block it is `L_0`.
//!
//! A piece covers four blocks. With `n > 1` pieces (then `B >= 5` and `P >= 8`), the
//! hash of piece `i` is the root of the subtree of four leaves over blocks `4i` to
//! `4i + 3`, zero leaves beyond `B` included ([`piece_hash`]). These `n` hashes are
//! BEP 52's *piece layer* for a piece length of 64 KiB ([`Tree::piece_layer`]). The
//! level of the piece layer has `P / 4 = n.next_power_of_two()` nodes, and the tree
//! above it is the perfect tree over them. The nodes from `n` on cover only padding and
//! are `Z2 = SHA-256(Z1 || Z1)`, with `Z1 = SHA-256(0^32 || 0^32)`. With a single piece
//! the tree has 1, 2 or 4 leaves, and the piece's hash is the pieces root.
//!
//! The [`Proof`] for piece `i` is the sibling hashes from the piece layer up to the
//! pieces root: exactly `log2(P / 4) = ceil(log2 n)` hashes ([`Layout::proof_len`]),
//! none for a single piece, at most 32 ([`MAX_PROOF`]). The number is fixed for the
//! object and any other is rejected. A [`Tree`] keeps the piece layer and every level
//! above it, about 64 bytes per piece, and answers a proof without hashing.
//!
//! # Root
//!
//! The key's [`Commitment`] is `C = SHA-256(0x03 || "nuschl files/1 key" || key)`,
//! 32 bytes. It reveals nothing useful about a random 256-bit key, so the server may
//! hold it. The object's root is
//!
//! ```text
//! R = SHA-256(0x02 || "nuschl files/1 root" || u8(16) || u64_be(object_len) || C || pieces_root)
//! ```
//!
//! where 16 is [`PIECE_LOG2`]. The root binds the piece size and the object length,
//! so it fixes the number of pieces, the length of every piece, the number of blocks
//! and the shape of the tree.
//!
//! The root binds the key because ChaCha20-Poly1305 does not: one ciphertext can be
//! crafted to decrypt under two keys (Len, Grubbs and Ristenpart, "Partitioning Oracle
//! Attacks", USENIX Security 2021). An owner may reference one object from several
//! messages, and without `C` it could give each recipient another key, and so another
//! plaintext, for the same root. With `C` a root fixes the key and every piece, and so
//! the plaintext. The sender puts the key, the root and the file's length into the
//! same end-to-end encrypted message.
//!
//! [`Root::verify`] needs the commitment, not the key. It checks the length of the
//! piece for its index, computes the piece's hash from its blocks, checks the number
//! of hashes in the proof, folds the proof up to a pieces root (the bits of the index
//! say on which side each sibling is), and recomputes the root with the object length
//! and the commitment. [`PiecesRoot::verify`] checks a piece against a pieces root
//! alone, as a BitTorrent v2 peer does.
//!
//! # No domain separation in the tree
//!
//! RFC 9162 prefixes leaves with 0x00 and inner nodes with 0x01, because without them
//! the 64 bytes of two children hash to their parent: a verifier that let the prover
//! choose the shape of the tree would accept those 64 bytes as a block in place of the
//! parent. BEP 52 has no prefixes, and this format takes its tree unchanged so that
//! BitTorrent v2 computes the same pieces root. Here the prover chooses nothing about
//! the shape. The tree is perfect, padded to a power of two, so every leaf is at the
//! same depth `log2 P`; the root binds `object_len`, which fixes `B` and `P`; a proof
//! has one valid length; and the verifier hashes the blocks of a piece itself, at the
//! positions its index and length give. Two children presented as a block hash to
//! their parent, so they could pass only where the parent stands, a level above the
//! leaves: that takes a proof one hash shorter, which is rejected. The zero leaves are
//! no hash of anything; a block that hashed to zero would be a SHA-256 preimage. The root
//! and the commitment keep their prefixes and labels, and with inputs of 93 and 51
//! bytes neither is an inner node of 64.
//!
//! # BitTorrent v2
//!
//! A v2 torrent with a piece length of 65,536 bytes and one file whose contents are the
//! ciphertext object has [`Tree::pieces_root`] as the file's "pieces root" and, when
//! `n > 1`, the [`Tree::piece_layer`] hashes concatenated as its "piece layers" entry
//! (BEP 52 leaves it out for a file no longer than one piece). A BitTorrent v2 client
//! checks the piece layer against the pieces root and every piece it downloads
//! against the piece layer, or against the pieces root with the sibling hashes of a
//! hash request: the check of [`PiecesRoot::verify`]. So a swarm, the server and a
//! recipient verify the same bytes against the same tree. The torrent's name, info
//! dictionary and info hash are not part of this format; Nuschl names an object by
//! its root, which adds the object length, the piece size and the key commitment. A
//! pieces root learnt elsewhere is checked against a root with [`Root::new`].
//!
//! # Use
//!
//! - **Uploader:** [`Key::generate`]; [`Layout::new`] with the padded length; a
//!   [`TreeBuilder`] for the layout and [`Key::commitment`]; [`seal`] every chunk and
//!   push its [`piece_hash`]. The server gets the [`Tree::root`] and the
//!   [`Tree::commitment`] when the upload starts and every piece with its
//!   [`Tree::proof`]; the message gets the key, the root and the file's length.
//! - **Server:** the [`Layout`] of the declared length, and [`Root::verify`] with the
//!   commitment for every piece it receives.
//! - **Recipient:** [`Key::commitment`] of the key in the message, never a commitment
//!   from the server; [`Root::verify`] with it, then [`open`]. A wrong key fails the
//!   verification of the very first piece.
//! - **BitTorrent v2 peer:** the [`Tree::pieces_root`] and the ciphertext length of
//!   [`Layout::ciphertext_len`]; [`PiecesRoot::verify`] for every piece. It needs
//!   neither the key nor the commitment, but a length it trusts (a torrent's info
//!   dictionary), because the pieces root does not bind the length.
//!
//! Nothing here does I/O.

use std::fmt;

use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::digest::{Context, SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use thiserror::Error;
use zeroize::Zeroize;

/// The base-2 logarithm of [`PIECE`]. Fixed in `files/1`; the root commits to it.
pub const PIECE_LOG2: u8 = 16;
/// Bytes in every piece but the last: 64 KiB, the BitTorrent v2 piece length.
pub const PIECE: usize = 1 << PIECE_LOG2;
/// Bytes in a BEP 52 block, the data under one leaf of the tree: 16 KiB.
pub const BLOCK: usize = 1 << 14;
/// Bytes of the Poly1305 tag at the end of every piece.
pub const TAG: usize = 16;
/// Plaintext bytes in every chunk but the last: a piece without its tag, 65,520.
pub const CHUNK: usize = PIECE - TAG;
/// Bytes of a [`Key`].
pub const KEY: usize = 32;
/// Bytes of a SHA-256 [`Hash`](type@Hash).
pub const HASH: usize = 32;
/// The most chunks an object can have: the chunk index in the nonce is a `u32`.
pub const MAX_CHUNKS: u64 = 1 << 32;
/// The longest object: [`MAX_CHUNKS`] full chunks, 65,520 × 2^32 bytes. Its ciphertext
/// object has 2^48 bytes (256 TiB).
pub const MAX_OBJECT: u64 = MAX_CHUNKS * CHUNK as u64;
/// The most hashes in a [`Proof`]: the height of the tree above [`MAX_CHUNKS`] pieces.
pub const MAX_PROOF: usize = 32;

/// A SHA-256 output: a leaf, an inner node such as a piece hash, or a root.
pub type Hash = [u8; HASH];

/// Blocks, and so leaves, under one piece.
const BLOCKS_PER_PIECE: usize = PIECE / BLOCK;
const ROOT_PREFIX: u8 = 0x02;
const ROOT_LABEL: &[u8] = b"nuschl files/1 root";
const COMMITMENT_PREFIX: u8 = 0x03;
const COMMITMENT_LABEL: &[u8] = b"nuschl files/1 key";

/// Why an operation on an object failed.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum Error {
    /// The object would have more than [`MAX_CHUNKS`] chunks.
    #[error("an object of {object_len} bytes has more than 2^32 chunks")]
    TooLarge { object_len: u64 },
    /// The chunk index is not below the number of chunks.
    #[error("chunk {index} does not exist in an object of {chunks} chunks")]
    BadIndex { index: u64, chunks: u64 },
    /// A plaintext chunk or a piece has the wrong length for its index.
    #[error("the chunk or piece has {actual} bytes, expected {expected}")]
    WrongLength { expected: usize, actual: usize },
    /// A [`TreeBuilder`] got another number of piece hashes than the object has pieces.
    #[error("{actual} piece hashes for an object of {expected} pieces")]
    PieceCount { expected: u64, actual: u64 },
    /// Encoded proof bytes are not a whole number of hashes, or more than [`MAX_PROOF`].
    #[error("a proof is at most 32 hashes of 32 bytes, not {len} bytes")]
    ProofEncoding { len: usize },
    /// The piece and proof do not lead to the root: the piece is not the one at this
    /// index of this object, the proof is wrong, or the commitment is not that of the
    /// object's key.
    #[error("the piece is not part of the object with this root at this index")]
    Proof,
    /// The piece does not decrypt with this key at this index.
    #[error("the piece does not decrypt with this key at this index")]
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
    /// `SHA-256(0x03 || "nuschl files/1 key" || key)`.
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
/// It reveals nothing useful about the key, so the server holds it to verify pieces.
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

/// How an object of a given length is cut into chunks and pieces.
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

    /// The number of chunks, and so of pieces, from 1 to [`MAX_CHUNKS`].
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
        Ok(index * CHUNK as u64)
    }

    /// Plaintext bytes in chunk `index`: [`CHUNK`], or fewer for the last chunk.
    pub fn plain_len(&self, index: u64) -> Result<usize, Error> {
        let rest = self.object_len - self.offset(index)?;
        // At most CHUNK, so the cast does not truncate.
        Ok(rest.min(CHUNK as u64) as usize)
    }

    /// Bytes in piece `index`: its plaintext and the tag. [`PIECE`] for every piece but
    /// the last.
    pub fn cipher_len(&self, index: u64) -> Result<usize, Error> {
        Ok(self.plain_len(index)? + TAG)
    }

    /// Where piece `index` starts in the ciphertext object.
    pub fn cipher_offset(&self, index: u64) -> Result<u64, Error> {
        self.check(index)?;
        Ok(index << PIECE_LOG2)
    }

    /// Bytes in the ciphertext object, all pieces in order: the object and one tag
    /// per chunk. This is the file length a BitTorrent v2 torrent of it declares.
    pub fn ciphertext_len(&self) -> u64 {
        self.object_len + self.chunks * TAG as u64
    }

    /// The number of 16 KiB blocks in the ciphertext object: the leaves of the tree
    /// before padding.
    pub fn blocks(&self) -> u64 {
        self.ciphertext_len().div_ceil(BLOCK as u64)
    }

    /// The number of hashes in the [`Proof`] of every piece: `log2(P / 4)` for `P`
    /// leaves, which is `ceil(log2 n)` for `n` pieces, and 0 for a single piece.
    pub fn proof_len(&self) -> usize {
        // At most 32.
        self.chunks.next_power_of_two().trailing_zeros() as usize
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

    fn check_piece(&self, index: u64, piece: &[u8]) -> Result<(), Error> {
        let expected = self.cipher_len(index)?;
        if piece.len() == expected {
            Ok(())
        } else {
            Err(Error::WrongLength {
                expected,
                actual: piece.len(),
            })
        }
    }
}

/// Encrypts the plaintext of chunk `index`, which must have exactly
/// [`Layout::plain_len`] bytes, into piece `index`.
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
    let mut piece = Vec::with_capacity(plaintext.len() + TAG);
    piece.extend_from_slice(plaintext);
    key.aead()?
        .seal_in_place_append_tag(nonce(index, last), Aad::empty(), &mut piece)
        .map_err(|_| Error::Crypto)?;
    Ok(piece)
}

/// Decrypts piece `index`, which must have exactly [`Layout::cipher_len`] bytes, into
/// the plaintext of chunk `index`.
pub fn open(key: &Key, layout: &Layout, index: u64, piece: &[u8]) -> Result<Vec<u8>, Error> {
    layout.check_piece(index, piece)?;
    let (index, last) = layout.position(index)?;
    let mut chunk = piece.to_vec();
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

/// The hash of piece `index`, which must have exactly [`Layout::cipher_len`] bytes:
/// BEP 52's piece-layer hash for a piece length of 64 KiB.
///
/// The leaves are the SHA-256 of each 16 KiB block of the piece. In an object of
/// several pieces they are padded with zero leaves to four, and the hash is the root
/// of those four. A single piece is the whole tree: its leaves are padded to 1, 2 or 4,
/// and its hash is the [`PiecesRoot`].
pub fn piece_hash(layout: &Layout, index: u64, piece: &[u8]) -> Result<Hash, Error> {
    layout.check_piece(index, piece)?;
    let mut leaves = [[0; HASH]; BLOCKS_PER_PIECE];
    for (leaf, block) in leaves.iter_mut().zip(piece.chunks(BLOCK)) {
        *leaf = sha256(block);
    }
    let width = if layout.chunks() > 1 {
        BLOCKS_PER_PIECE
    } else {
        piece.len().div_ceil(BLOCK).next_power_of_two()
    };
    let [a, b, c, d] = &leaves;
    Ok(match width {
        1 => *a,
        2 => node(a, b),
        _ => node(&node(a, b), &node(c, d)),
    })
}

fn sha256(data: &[u8]) -> Hash {
    let mut out = [0; HASH];
    out.copy_from_slice(digest(&SHA256, data).as_ref());
    out
}

/// An inner node: `SHA-256(left || right)`, without a prefix, as in BEP 52.
fn node(left: &Hash, right: &Hash) -> Hash {
    let mut hash = Context::new(&SHA256);
    hash.update(left);
    hash.update(right);
    finish(hash)
}

/// The root of four zero leaves, which stands for a piece position beyond the object.
fn piece_pad() -> Hash {
    let zero = node(&[0; HASH], &[0; HASH]);
    node(&zero, &zero)
}

fn finish(hash: Context) -> Hash {
    let mut out = [0; HASH];
    out.copy_from_slice(hash.finish().as_ref());
    out
}

/// Lowercase hex, for [`Root`], [`PiecesRoot`] and [`Commitment`].
fn hex(hash: &Hash, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    hash.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
}

/// The pieces root from the hash of piece `index` and its proof.
fn fold_proof(
    layout: &Layout,
    index: u64,
    piece_hash: &Hash,
    proof: &Proof,
) -> Result<PiecesRoot, Error> {
    layout.check(index)?;
    if proof.0.len() != layout.proof_len() {
        return Err(Error::Proof);
    }
    // A perfect tree over 2^proof_len piece positions: bit k of the index says whether
    // the node at level k above the piece layer is a right child.
    let hash = (0..)
        .zip(&proof.0)
        .fold(*piece_hash, |hash, (level, sibling)| {
            if (index >> level) & 1 == 0 {
                node(&hash, sibling)
            } else {
                node(sibling, &hash)
            }
        });
    Ok(PiecesRoot(hash))
}

/// Collects the piece hashes of an object in piece order.
#[derive(Debug)]
pub struct TreeBuilder {
    layout: Layout,
    commitment: Commitment,
    pieces: Vec<Hash>,
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
            pieces: Vec::with_capacity(capacity),
        }
    }

    /// Adds the [`piece_hash`] of the next piece.
    pub fn push(&mut self, piece_hash: Hash) {
        self.pieces.push(piece_hash);
    }

    /// The tree, once every piece has its hash.
    pub fn finish(self) -> Result<Tree, Error> {
        let actual = self.pieces.len() as u64;
        if actual != self.layout.chunks() {
            return Err(Error::PieceCount {
                expected: self.layout.chunks(),
                actual,
            });
        }
        // Level by level from the piece layer. A level of odd length gets the node of
        // its padding: the root of an all-zero subtree of that height. No proof needs
        // another padding node, because only the last real node of a level can have a
        // sibling that covers nothing but padding.
        let mut levels = vec![self.pieces];
        let mut pad = piece_pad();
        while let Some(level) = levels.last_mut().filter(|level| level.len() > 1) {
            if level.len() % 2 == 1 {
                level.push(pad);
            }
            let parents = level
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[left, right]| node(left, right))
                .collect();
            levels.push(parents);
            pad = node(&pad, &pad);
        }
        let pieces_root = levels.last().and_then(|top| top.first()).copied();
        let pieces_root = PiecesRoot(pieces_root.ok_or(Error::PieceCount {
            expected: self.layout.chunks(),
            actual,
        })?);
        Ok(Tree {
            layout: self.layout,
            commitment: self.commitment,
            root: Root::new(&self.layout, &self.commitment, &pieces_root),
            pieces_root,
            levels,
        })
    }
}

/// The BEP 52 tree of an object: its roots, its piece layer and every proof.
pub struct Tree {
    layout: Layout,
    commitment: Commitment,
    root: Root,
    pieces_root: PiecesRoot,
    /// `levels[0]` is the piece layer; the last level is the pieces root alone. A level
    /// of odd length below the top ends with one padding node.
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

    /// The BEP 52 pieces root of the ciphertext object.
    pub fn pieces_root(&self) -> PiecesRoot {
        self.pieces_root
    }

    /// The piece hashes in piece order. With more than one piece, concatenated they
    /// are the ciphertext object's entry in a v2 torrent's "piece layers"; a single
    /// piece's hash is the pieces root, and BEP 52 has no entry for it.
    pub fn piece_layer(&self) -> &[Hash] {
        // The count was checked against the layout, so the cast does not truncate.
        &self.levels[0][..self.layout.chunks() as usize]
    }

    /// The proof of piece `index`: [`Layout::proof_len`] sibling hashes from the piece
    /// layer up.
    pub fn proof(&self, index: u64) -> Result<Proof, Error> {
        self.layout.check(index)?;
        // Below the number of pieces, which fitted in memory.
        let mut position = index as usize;
        let mut path = Vec::with_capacity(self.layout.proof_len());
        for level in &self.levels[..self.levels.len() - 1] {
            // Every level below the top has even length.
            path.push(level[position ^ 1]);
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
            .field("pieces_root", &self.pieces_root)
            .finish_non_exhaustive()
    }
}

/// The root of an object: a commitment to its length, its piece size, its key and the
/// pieces root, and so to every piece.
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

    /// The root of the object with this layout, key commitment and pieces root:
    /// `SHA-256(0x02 || "nuschl files/1 root" || u8(16) || u64_be(object_len) || C ||
    /// pieces_root)`.
    pub fn new(layout: &Layout, commitment: &Commitment, pieces_root: &PiecesRoot) -> Self {
        let mut hash = Context::new(&SHA256);
        hash.update(&[ROOT_PREFIX]);
        hash.update(ROOT_LABEL);
        hash.update(&[PIECE_LOG2]);
        hash.update(&layout.object_len().to_be_bytes());
        hash.update(commitment.as_bytes());
        hash.update(pieces_root.as_bytes());
        Self(finish(hash))
    }

    /// Checks that `piece` is piece `index` of the object with this root, layout and
    /// key commitment. Needs no key; a recipient passes [`Key::commitment`] of the key
    /// it holds.
    pub fn verify(
        &self,
        layout: &Layout,
        commitment: &Commitment,
        index: u64,
        piece: &[u8],
        proof: &Proof,
    ) -> Result<(), Error> {
        let hash = piece_hash(layout, index, piece)?;
        self.verify_piece_hash(layout, commitment, index, &hash, proof)
    }

    /// Checks that `piece_hash` is the [`piece_hash`] of piece `index`. [`Root::verify`]
    /// also checks the length of the piece; a caller of this must do that itself.
    pub fn verify_piece_hash(
        &self,
        layout: &Layout,
        commitment: &Commitment,
        index: u64,
        piece_hash: &Hash,
        proof: &Proof,
    ) -> Result<(), Error> {
        let pieces_root = fold_proof(layout, index, piece_hash, proof)?;
        if Self::new(layout, commitment, &pieces_root) == *self {
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

/// The BEP 52 pieces root of an object's ciphertext object: the root of the Merkle tree
/// over its 16 KiB blocks, as a BitTorrent v2 torrent of it names it.
///
/// It binds every piece but not the object length or the key; the [`Root`] adds those.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PiecesRoot(Hash);

impl PiecesRoot {
    /// The pieces root with these bytes.
    pub fn from_bytes(bytes: Hash) -> Self {
        Self(bytes)
    }

    /// The pieces root's bytes.
    pub fn as_bytes(&self) -> &Hash {
        &self.0
    }

    /// Checks that `piece` is piece `index` of a ciphertext object with this pieces
    /// root, cut by this layout: the check of a BitTorrent v2 peer. The layout must
    /// come from a trusted length, because the pieces root does not bind it.
    pub fn verify(
        &self,
        layout: &Layout,
        index: u64,
        piece: &[u8],
        proof: &Proof,
    ) -> Result<(), Error> {
        let hash = piece_hash(layout, index, piece)?;
        self.verify_piece_hash(layout, index, &hash, proof)
    }

    /// Checks that `piece_hash` is the [`piece_hash`] of piece `index`.
    /// [`PiecesRoot::verify`] also checks the length of the piece; a caller of this must
    /// do that itself.
    pub fn verify_piece_hash(
        &self,
        layout: &Layout,
        index: u64,
        piece_hash: &Hash,
        proof: &Proof,
    ) -> Result<(), Error> {
        if fold_proof(layout, index, piece_hash, proof)? == *self {
            Ok(())
        } else {
            Err(Error::Proof)
        }
    }
}

impl fmt::Display for PiecesRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        hex(&self.0, f)
    }
}

impl fmt::Debug for PiecesRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PiecesRoot({self})")
    }
}

/// The proof of one piece: sibling hashes from the piece layer up to the pieces root.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Proof(Vec<Hash>);

impl Proof {
    /// The sibling hashes, from the piece layer upwards.
    pub fn hashes(&self) -> &[Hash] {
        &self.0
    }

    /// The hashes concatenated: 32 bytes each.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.0.concat()
    }

    /// Parses concatenated hashes: a multiple of 32 bytes, at most [`MAX_PROOF`]
    /// hashes. Whether the number fits the object ([`Layout::proof_len`]) is checked by
    /// [`Root::verify`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let (hashes, rest) = bytes.as_chunks::<HASH>();
        if !rest.is_empty() || hashes.len() > MAX_PROOF {
            return Err(Error::ProofEncoding { len: bytes.len() });
        }
        Ok(Self(hashes.to_vec()))
    }
}

/// Encrypts a whole object held in memory: every piece and the tree.
///
/// For tests and small objects; large files go chunk by chunk through [`seal`],
/// [`piece_hash`] and a [`TreeBuilder`].
pub fn encrypt_object(key: &Key, object: &[u8]) -> Result<(Tree, Vec<Vec<u8>>), Error> {
    let layout = Layout::new(object.len() as u64)?;
    let mut tree = TreeBuilder::new(layout, key.commitment());
    let mut pieces = Vec::new();
    // An empty object still has one (empty) chunk.
    let chunks = object
        .chunks(CHUNK)
        .chain(object.is_empty().then_some(object));
    for (index, chunk) in (0..).zip(chunks) {
        let piece = seal(key, &layout, index, chunk)?;
        tree.push(piece_hash(&layout, index, &piece)?);
        pieces.push(piece);
    }
    Ok((tree.finish()?, pieces))
}

/// The padded length of a file of `len` bytes: Padmé (Nikitin, Barman, Lueks, Underwood,
/// Hubaux and Ford, "Reducing Metadata Leakage from Encrypted Files and Communication
/// with PURBs", PETS 2019).
///
/// With `E = floor(log2 len)` and `S = floor(log2 E) + 1`, the length is rounded up to
/// a multiple of `2^(E - S)`. The padded length reveals `O(log log len)` bits of the
/// length and adds less than 12 % (the most is 129 bytes padded to 144). Lengths
/// below 2 stay as they are. The result is never smaller than `len`, never decreases
/// as `len` grows, and is at most 2^48 when `len` is. It is at most [`MAX_OBJECT`] for
/// `len` up to 2^48 − 2^41 (254 TiB); a longer file of at most 2^48 bytes pads to
/// 2^48, which [`Layout::new`] rejects. Above `2^64 - 2^57`, where rounding up would overflow, the result is
/// `u64::MAX`; no object is that long.
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

    /// SHA-256 of the parts concatenated, for the reference tree.
    fn sha(parts: &[&[u8]]) -> Hash {
        let mut hash = Context::new(&SHA256);
        parts.iter().for_each(|part| hash.update(part));
        finish(hash)
    }

    fn hex_of(hash: &Hash) -> String {
        hash.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The BEP 52 tree from its definition, written apart from the code above: SHA-256
    /// leaves of 16 KiB blocks, zero leaves up to a power of two, nodes
    /// `SHA-256(left || right)`, recursively. Subtree hashes are remembered, so that
    /// every proof of 300 trees stays fast.
    struct Bep52 {
        leaves: Vec<Hash>,
        subtrees: HashMap<(usize, usize), Hash>,
    }

    impl Bep52 {
        fn from_leaves(mut leaves: Vec<Hash>) -> Self {
            leaves.resize(leaves.len().next_power_of_two(), [0; HASH]);
            let subtrees = HashMap::new();
            Self { leaves, subtrees }
        }

        fn from_file(file: &[u8]) -> Self {
            Self::from_leaves(file.chunks(BLOCK).map(|block| sha(&[block])).collect())
        }

        /// The root over the `len` leaves from `lo`.
        fn subtree(&mut self, lo: usize, len: usize) -> Hash {
            if len == 1 {
                return self.leaves[lo];
            }
            if let Some(hash) = self.subtrees.get(&(lo, len)) {
                return *hash;
            }
            let half = len / 2;
            let left = self.subtree(lo, half);
            let right = self.subtree(lo + half, half);
            let hash = sha(&[&left, &right]);
            self.subtrees.insert((lo, len), hash);
            hash
        }

        fn root(&mut self) -> Hash {
            self.subtree(0, self.leaves.len())
        }

        /// The hash of piece `i`: the subtree of its four blocks, or the whole tree if
        /// it has at most four leaves (a single piece).
        fn piece(&mut self, i: usize) -> Hash {
            if self.leaves.len() <= 4 {
                self.root()
            } else {
                self.subtree(4 * i, 4)
            }
        }

        /// The siblings of piece `i` from the piece layer up to the root.
        fn uncles(&mut self, i: usize) -> Vec<Hash> {
            let mut uncles = Vec::new();
            let mut size = 4;
            while size < self.leaves.len() {
                let sibling = (4 * i / size) ^ 1;
                uncles.push(self.subtree(sibling * size, size));
                size *= 2;
            }
            uncles
        }
    }

    /// Plaintext lengths whose ciphertext object is one piece of one to four blocks,
    /// just under, at and just over each block boundary.
    fn single_piece_lengths() -> Vec<usize> {
        (1..=4)
            .flat_map(|k| [k * BLOCK - TAG - 1, k * BLOCK - TAG, k * BLOCK - TAG + 1])
            .collect()
    }

    /// The layout of `n` pieces whose last piece has `last_blocks` blocks.
    fn layout_of(n: u64, last_blocks: u64) -> Layout {
        let last_piece = last_blocks * BLOCK as u64 - 7;
        let layout = Layout::new((n - 1) * CHUNK as u64 + last_piece - TAG as u64).unwrap();
        assert_eq!(layout.chunks(), n);
        assert_eq!(layout.blocks(), 4 * (n - 1) + last_blocks);
        layout
    }

    #[test]
    fn round_trip() {
        let key = Key::generate().unwrap();
        let mut lengths = LENGTHS.to_vec();
        lengths.extend(single_piece_lengths());
        for len in lengths {
            let data = object(len);
            let (tree, pieces) = encrypt_object(&key, &data).unwrap();
            let layout = *tree.layout();
            assert_eq!(layout, Layout::new(len as u64).unwrap());
            assert_eq!(layout.chunks(), pieces.len() as u64);
            assert_eq!(layout.chunks(), len.div_ceil(CHUNK).max(1) as u64);
            let file = pieces.concat();
            assert_eq!(file.len() as u64, layout.ciphertext_len());
            assert_eq!(layout.blocks(), file.len().div_ceil(BLOCK) as u64);

            // The tree is BEP 52's over the ciphertext object.
            let mut reference = Bep52::from_file(&file);
            assert_eq!(*tree.pieces_root().as_bytes(), reference.root());
            assert_eq!(tree.piece_layer().len(), pieces.len());
            for (i, hash) in tree.piece_layer().iter().enumerate() {
                assert_eq!(
                    *hash,
                    reference.piece(i),
                    "object of {len} bytes, piece {i}"
                );
            }

            let root = Root::from_bytes(*tree.root().as_bytes());
            let commitment = Commitment::from_bytes(*tree.commitment().as_bytes());
            assert_eq!(commitment, key.commitment());
            assert_eq!(Root::new(&layout, &commitment, &tree.pieces_root()), root);
            let pieces_root = PiecesRoot::from_bytes(*tree.pieces_root().as_bytes());
            let mut plaintext = Vec::new();
            for (index, piece) in (0..).zip(&pieces) {
                assert_eq!(piece.len(), layout.cipher_len(index).unwrap());
                assert!(piece.len() == PIECE || layout.is_last(index));
                let start = layout.cipher_offset(index).unwrap() as usize;
                assert_eq!(&file[start..start + piece.len()], piece);
                let proof = Proof::from_bytes(&tree.proof(index).unwrap().to_bytes()).unwrap();
                assert_eq!(proof.hashes().len(), layout.proof_len());
                root.verify(&layout, &commitment, index, piece, &proof)
                    .unwrap();
                pieces_root.verify(&layout, index, piece, &proof).unwrap();
                let chunk = open(&key, &layout, index, piece).unwrap();
                assert_eq!(chunk.len(), layout.plain_len(index).unwrap());
                assert_eq!(plaintext.len() as u64, layout.offset(index).unwrap());
                plaintext.extend(chunk);
            }
            assert_eq!(plaintext, data, "object of {len} bytes");
        }
    }

    #[test]
    fn single_piece_is_the_whole_tree() {
        let key = Key::from_bytes([9; KEY]);
        // The last length, a byte over 64 KiB of ciphertext, is two pieces.
        let blocks = [1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4];
        for (len, blocks) in single_piece_lengths().into_iter().zip(blocks) {
            let (tree, pieces) = encrypt_object(&key, &object(len)).unwrap();
            let layout = *tree.layout();
            assert_eq!(layout.chunks(), 1, "{len}");
            assert_eq!(layout.blocks(), blocks, "{len}");
            assert_eq!(layout.proof_len(), 0);
            assert_eq!(tree.piece_layer(), [*tree.pieces_root().as_bytes()]);
            let hash = piece_hash(&layout, 0, &pieces[0]).unwrap();
            assert_eq!(hash, *tree.pieces_root().as_bytes());
            // Leaves padded to 1, 2 or 4, not always to the four of a full piece.
            let leaves: Vec<Hash> = pieces[0].chunks(BLOCK).map(|b| sha(&[b])).collect();
            let expected = match leaves.as_slice() {
                [a] => *a,
                [a, b] => sha(&[a, b]),
                [a, b, c] => sha(&[&sha(&[a, b]), &sha(&[c, &[0; HASH]])]),
                [a, b, c, d] => sha(&[&sha(&[a, b]), &sha(&[c, d])]),
                _ => unreachable!(),
            };
            assert_eq!(hash, expected, "{len}");
            // Any proof but the empty one is refused.
            let one = Proof(vec![[0; HASH]]);
            assert_eq!(
                tree.root()
                    .verify(&layout, &key.commitment(), 0, &pieces[0], &one),
                Err(Error::Proof)
            );
            assert_eq!(
                tree.pieces_root().verify(&layout, 0, &pieces[0], &one),
                Err(Error::Proof)
            );
        }
    }

    #[test]
    fn empty_object_is_one_tag() {
        let key = Key::from_bytes([7; KEY]);
        let (tree, pieces) = encrypt_object(&key, &[]).unwrap();
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].len(), TAG);
        assert_eq!(tree.layout().blocks(), 1);
        assert!(tree.proof(0).unwrap().hashes().is_empty());
        assert_eq!(*tree.pieces_root().as_bytes(), sha(&[&pieces[0]]));
        assert_eq!(open(&key, tree.layout(), 0, &pieces[0]).unwrap(), b"");
    }

    /// Root/key bindings were independently re-recorded with Python hashlib
    /// for the Nuschl namespace on 2026-11-11; ciphertext and BEP 52 roots stay.
    /// Any change to the format changes these values. All were recomputed when frozen
    /// by a separate implementation (Python: RFC 8439 by hand, hashlib, BEP 52), and
    /// libtorrent 2.1.2 gave the same "pieces root" and "piece layers" for v2-only
    /// torrents of the same ciphertext objects with a piece length of 64 KiB.
    #[test]
    fn frozen_vector() {
        let key = Key::from_bytes(std::array::from_fn(|i| i as u8));
        assert_eq!(
            key.commitment().to_string(),
            "b4933d51d2fd989a018b0ae07dedff7f6a8f6d99e446da0a63d7e9d3f15eb9ab"
        );
        let data =
            |len: usize| -> Vec<u8> { (0..len).map(|i| (i as u8).wrapping_mul(7)).collect() };

        let (tree, pieces) = encrypt_object(&key, &data(70_000)).unwrap();
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[1].len(), 70_000 - CHUNK + TAG);
        assert_eq!(tree.layout().ciphertext_len(), 70_032);
        assert_eq!(
            tree.pieces_root().to_string(),
            "f5ec81805c832fe6e9f01bb4faa536d11092746a30a10a2a86138f2f056d35e5"
        );
        let layer: Vec<String> = tree.piece_layer().iter().map(hex_of).collect();
        assert_eq!(
            layer,
            [
                "1422bc1e21adb2401221fb0efa3e50e8de8219c028017998f8187213c1dcd38e",
                "6ca02184a1d86393ca1c379dac3bfb6f7b4928a05e6a4ee4c700687478405e9a",
            ]
        );
        assert_eq!(
            tree.root().to_string(),
            "0524dd8cedb3b73bba98ca9d24e2d3bf938a43e65fa6e6ae520dac1872c2cc41"
        );

        let (tree, _) = encrypt_object(&key, &[]).unwrap();
        assert_eq!(
            tree.pieces_root().to_string(),
            "98082ff61f1317c757770e0ee5ec26bbe8f1d4d7c3f91e4755eb0622f630b8a7"
        );
        assert_eq!(
            tree.root().to_string(),
            "2ef6576e5447757e3fcbc215722d1c844bbd4ceca4864ec4852a9214f20a7094"
        );

        // One piece of two blocks: two leaves, not four.
        let (tree, _) = encrypt_object(&key, &data(20_000)).unwrap();
        assert_eq!(
            tree.pieces_root().to_string(),
            "7637bc7a928237053f54eed6736ebd32f7671f15a09536854b8c00f601ff2579"
        );
        assert_eq!(
            tree.root().to_string(),
            "46801614b3e3bb891df3a24e12ab60d0dcbb6bd823a3af71457b7142f6036428"
        );

        // Five pieces: three padding nodes in the piece layer's level.
        let (tree, _) = encrypt_object(&key, &data(300_000)).unwrap();
        assert_eq!(tree.layout().chunks(), 5);
        assert_eq!(tree.layout().proof_len(), 3);
        assert_eq!(
            tree.pieces_root().to_string(),
            "babf8fac8beb11bc9c94087a99d14a264a3c73fd62d40e27d9346f5c31b39dd5"
        );
        assert_eq!(
            hex_of(&sha(&[&tree.piece_layer().concat()])),
            "554137285d65b3b9139e36622935a02f009e537e4373b5378fd6eca023d876f2"
        );
        assert_eq!(
            tree.root().to_string(),
            "2d98388ae06e37ffc0108e3d7a4fe3eef99609fcaf784296e2aa90a8c291a7a8"
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
        let (tree, pieces) = encrypt_object(&key, &data).unwrap();
        let layout = *tree.layout();
        let root = tree.root();
        let pieces_root = tree.pieces_root();
        let commitment = key.commitment();
        let proofs: Vec<Proof> = (0..4).map(|i| tree.proof(i).unwrap()).collect();
        assert_eq!(layout.proof_len(), 2);

        // A flipped bit, in the ciphertext and in the tag, in every block.
        for position in [0, 100, BLOCK, 2 * BLOCK + 1, PIECE - 1] {
            let mut piece = pieces[0].clone();
            piece[position] ^= 1;
            assert_eq!(
                root.verify(&layout, &commitment, 0, &piece, &proofs[0]),
                Err(Error::Proof)
            );
            assert_eq!(
                pieces_root.verify(&layout, 0, &piece, &proofs[0]),
                Err(Error::Proof)
            );
            assert_eq!(open(&key, &layout, 0, &piece), Err(Error::Authentication));
        }

        // Swapped pieces.
        for proof in [&proofs[0], &proofs[1]] {
            assert_eq!(
                root.verify(&layout, &commitment, 0, &pieces[1], proof),
                Err(Error::Proof)
            );
            assert_eq!(
                pieces_root.verify(&layout, 0, &pieces[1], proof),
                Err(Error::Proof)
            );
        }
        assert_eq!(
            open(&key, &layout, 0, &pieces[1]),
            Err(Error::Authentication)
        );
        assert_eq!(
            open(&key, &layout, 1, &pieces[0]),
            Err(Error::Authentication)
        );

        // A piece at the wrong index, with its own proof or that of the index.
        for (index, proof) in [(2, &proofs[1]), (2, &proofs[2]), (0, &proofs[1])] {
            assert_eq!(
                root.verify(&layout, &commitment, index, &pieces[1], proof),
                Err(Error::Proof)
            );
            assert_eq!(
                pieces_root.verify(&layout, index, &pieces[1], proof),
                Err(Error::Proof)
            );
        }
        let bad_index = Err(Error::BadIndex {
            index: 4,
            chunks: 4,
        });
        assert_eq!(
            root.verify(&layout, &commitment, 4, &pieces[1], &proofs[1]),
            bad_index
        );
        let hash = piece_hash(&layout, 1, &pieces[1]).unwrap();
        assert_eq!(
            pieces_root.verify_piece_hash(&layout, 4, &hash, &proofs[1]),
            bad_index
        );

        // A truncated last piece, and a full piece where the last belongs.
        let short = &pieces[3][..pieces[3].len() - 1];
        let wrong_length = Error::WrongLength {
            expected: 5 + TAG,
            actual: 4 + TAG,
        };
        assert_eq!(
            root.verify(&layout, &commitment, 3, short, &proofs[3]),
            Err(wrong_length)
        );
        assert_eq!(
            pieces_root.verify(&layout, 3, short, &proofs[3]),
            Err(wrong_length)
        );
        assert_eq!(piece_hash(&layout, 3, short), Err(wrong_length));
        assert_eq!(open(&key, &layout, 3, short), Err(wrong_length));
        assert_eq!(
            root.verify(&layout, &commitment, 3, &pieces[2], &proofs[3]),
            Err(Error::WrongLength {
                expected: 5 + TAG,
                actual: PIECE
            })
        );

        // An object cut short at a piece boundary: chunk 2 was not sealed as the last.
        let cut = Layout::new(3 * CHUNK as u64).unwrap();
        assert_eq!(open(&key, &cut, 2, &pieces[2]), Err(Error::Authentication));
        assert_eq!(
            root.verify(&cut, &commitment, 2, &pieces[2], &proofs[2]),
            Err(Error::Proof)
        );

        // A proof one hash short or long.
        for index in 0..4 {
            let piece = &pieces[index as usize];
            let mut hashes = proofs[index as usize].hashes().to_vec();
            hashes.pop();
            let short = Proof(hashes.clone());
            assert_eq!(
                root.verify(&layout, &commitment, index, piece, &short),
                Err(Error::Proof)
            );
            assert_eq!(
                pieces_root.verify(&layout, index, piece, &short),
                Err(Error::Proof)
            );
            hashes.extend([[0; HASH], [0; HASH]]);
            let long = Proof(hashes);
            assert_eq!(
                root.verify(&layout, &commitment, index, piece, &long),
                Err(Error::Proof)
            );
            assert_eq!(
                pieces_root.verify(&layout, index, piece, &long),
                Err(Error::Proof)
            );
        }

        // The root of another object length (same piece count, same piece hashes).
        let other = Layout::new(3 * CHUNK as u64 + 6).unwrap();
        assert_eq!(other.chunks(), layout.chunks());
        assert_eq!(
            root.verify(&other, &commitment, 0, &pieces[0], &proofs[0]),
            Err(Error::Proof)
        );
        let mut builder = TreeBuilder::new(other, commitment);
        tree.piece_layer()
            .iter()
            .for_each(|hash| builder.push(*hash));
        let other_tree = builder.finish().unwrap();
        assert_eq!(other_tree.pieces_root(), pieces_root);
        assert_ne!(other_tree.root(), root);
        // The pieces root alone does not bind the length: the full pieces verify under
        // the other layout. That is why the root does.
        pieces_root
            .verify(&other, 0, &pieces[0], &proofs[0])
            .unwrap();

        // Another key: it does not open the pieces, the same pieces and tree under its
        // commitment have another root, and its commitment verifies no piece.
        let other_key = Key::generate().unwrap();
        let other_commitment = other_key.commitment();
        for (index, piece) in (0..).zip(&pieces) {
            assert_eq!(
                open(&other_key, &layout, index, piece),
                Err(Error::Authentication)
            );
            assert_eq!(
                root.verify(
                    &layout,
                    &other_commitment,
                    index,
                    piece,
                    &proofs[index as usize]
                ),
                Err(Error::Proof)
            );
        }
        let mut builder = TreeBuilder::new(layout, other_commitment);
        tree.piece_layer()
            .iter()
            .for_each(|hash| builder.push(*hash));
        let other_tree = builder.finish().unwrap();
        assert_eq!(other_tree.piece_layer(), tree.piece_layer());
        assert_eq!(other_tree.pieces_root(), pieces_root);
        assert_ne!(other_tree.root(), root);
        let mut builder = TreeBuilder::new(layout, commitment);
        tree.piece_layer()
            .iter()
            .for_each(|hash| builder.push(*hash));
        assert_eq!(builder.finish().unwrap().root(), root);

        // The last-chunk flag: the last chunk sealed as not last, an earlier one as last.
        let last_plain = &data[3 * CHUNK..];
        let middle_plain = &data[CHUNK..2 * CHUNK];
        for (index, last, plain) in [(3, false, last_plain), (1, true, middle_plain)] {
            let piece = seal_at(&key, index as u32, last, plain).unwrap();
            assert_eq!(piece.len(), pieces[index as usize].len());
            assert_eq!(
                open(&key, &layout, index, &piece),
                Err(Error::Authentication)
            );
            assert_eq!(
                root.verify(&layout, &commitment, index, &piece, &proofs[index as usize]),
                Err(Error::Proof)
            );
        }
        // The honest seal is reproducible.
        assert_eq!(seal(&key, &layout, 3, last_plain).unwrap(), pieces[3]);
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
            seal(&key, &layout, 0, &[0; PIECE]),
            Err(Error::WrongLength {
                expected: CHUNK,
                actual: PIECE
            })
        );
        let bad = Error::BadIndex {
            index: 2,
            chunks: 2,
        };
        assert_eq!(seal(&key, &layout, 2, &[]), Err(bad));
        assert_eq!(open(&key, &layout, 2, &[0; TAG]), Err(bad));
        assert_eq!(piece_hash(&layout, 2, &[0; TAG]), Err(bad));
    }

    #[test]
    fn tree_shape() {
        let commitment = Key::from_bytes([3; KEY]).commitment();
        let other_commitment = Key::from_bytes([4; KEY]).commitment();
        let shapes = (1..=4)
            .map(|last_blocks| (1, last_blocks))
            .chain((2..=300).map(|n| (n, 1 + n % 4)));
        for (n, last_blocks) in shapes {
            let layout = layout_of(n, last_blocks);
            let blocks = layout.blocks() as usize;
            // Synthetic block leaves, all distinct: the tree above them does not look at
            // the blocks.
            let leaves: Vec<Hash> = (0..blocks as u64)
                .map(|j| {
                    let mut leaf = [0xa5; HASH];
                    leaf[..8].copy_from_slice(&j.to_be_bytes());
                    leaf[8..16].copy_from_slice(&n.to_be_bytes());
                    leaf
                })
                .collect();
            let mut reference = Bep52::from_leaves(leaves);
            let hashes: Vec<Hash> = (0..n as usize).map(|i| reference.piece(i)).collect();
            let mut builder = TreeBuilder::new(layout, commitment);
            hashes.iter().for_each(|hash| builder.push(*hash));
            let tree = builder.finish().unwrap();

            let pieces_root = PiecesRoot(reference.root());
            assert_eq!(tree.pieces_root(), pieces_root, "n {n}");
            assert_eq!(tree.piece_layer(), &hashes[..]);
            assert_eq!(tree.commitment(), commitment);
            let root = Root::new(&layout, &commitment, &pieces_root);
            assert_eq!(tree.root(), root);
            // log2(P / 4) hashes, P the leaves padded to a power of two.
            let proof_len = if n == 1 {
                0
            } else {
                (blocks.next_power_of_two() / 4).ilog2() as usize
            };
            assert_eq!(layout.proof_len(), proof_len, "n {n}");

            for index in 0..n {
                let i = index as usize;
                let proof = tree.proof(index).unwrap();
                assert_eq!(proof.hashes(), reference.uncles(i), "n {n}, index {index}");
                assert_eq!(proof.hashes().len(), proof_len);
                root.verify_piece_hash(&layout, &commitment, index, &hashes[i], &proof)
                    .unwrap();
                // The other checks take a sample of the pieces, to keep the test fast.
                if index % 8 != 0 && index != n - 1 {
                    continue;
                }
                pieces_root
                    .verify_piece_hash(&layout, index, &hashes[i], &proof)
                    .unwrap();
                assert_eq!(
                    root.verify_piece_hash(&layout, &other_commitment, index, &hashes[i], &proof),
                    Err(Error::Proof)
                );
                if n > 1 {
                    let next = (index + 1) % n;
                    let other_hash = &hashes[next as usize];
                    assert_eq!(
                        root.verify_piece_hash(&layout, &commitment, index, other_hash, &proof),
                        Err(Error::Proof)
                    );
                    assert_eq!(
                        pieces_root.verify_piece_hash(&layout, next, &hashes[i], &proof),
                        Err(Error::Proof)
                    );
                }
            }
            assert!(tree.proof(n).is_err());
        }
    }

    #[test]
    fn padding_nodes() {
        // A piece position beyond the object is the root of four zero leaves, and the
        // tree is the perfect tree over P / 4 such positions.
        let z1 = sha(&[&[0; HASH], &[0; HASH]]);
        assert_eq!(piece_pad(), sha(&[&z1, &z1]));
        let commitment = Key::from_bytes([6; KEY]).commitment();
        let layout = layout_of(5, 2);
        let hashes: Vec<Hash> = (0..5u8).map(|i| [i; HASH]).collect();
        let mut builder = TreeBuilder::new(layout, commitment);
        hashes.iter().for_each(|hash| builder.push(*hash));
        let tree = builder.finish().unwrap();
        let z2 = piece_pad();
        let z3 = sha(&[&z2, &z2]);
        let h = |l: &Hash, r: &Hash| sha(&[l, r]);
        let expected = h(
            &h(&h(&hashes[0], &hashes[1]), &h(&hashes[2], &hashes[3])),
            &h(&h(&hashes[4], &z2), &z3),
        );
        assert_eq!(*tree.pieces_root().as_bytes(), expected);
        assert_eq!(
            tree.proof(4).unwrap().hashes(),
            [
                z2,
                z3,
                h(&h(&hashes[0], &hashes[1]), &h(&hashes[2], &hashes[3]))
            ]
        );
    }

    #[test]
    fn proof_length_bound() {
        let len = |chunks: u64| Layout::new(chunks * CHUNK as u64).unwrap().proof_len();
        assert_eq!(Layout::new(0).unwrap().proof_len(), 0);
        for (chunks, expected) in [(1, 0), (2, 1), (3, 2), (4, 2), (5, 3), (8, 3), (9, 4)] {
            assert_eq!(len(chunks), expected, "{chunks} chunks");
        }
        assert_eq!(len(MAX_CHUNKS / 2), MAX_PROOF - 1);
        assert_eq!(len(MAX_CHUNKS / 2 + 1), MAX_PROOF);
        assert_eq!(Layout::new(MAX_OBJECT).unwrap().proof_len(), MAX_PROOF);
        // log2(P / 4), with P the blocks rounded up to a power of two.
        for object_len in [CHUNK as u64 + 1, 1 << 30, (1 << 40) + 12_345, MAX_OBJECT] {
            let layout = Layout::new(object_len).unwrap();
            let p = layout.blocks().next_power_of_two();
            assert_eq!(layout.proof_len(), (p / 4).ilog2() as usize);
        }
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
        assert_eq!(CHUNK, 65_520);
        assert_eq!(BLOCKS_PER_PIECE, 4);

        let empty = Layout::new(0).unwrap();
        assert_eq!(empty.chunks(), 1);
        assert_eq!(empty.plain_len(0), Ok(0));
        assert_eq!(empty.cipher_len(0), Ok(TAG));
        assert_eq!(empty.ciphertext_len(), TAG as u64);
        assert_eq!(empty.blocks(), 1);
        assert!(empty.is_last(0));

        let full = Layout::new(CHUNK as u64).unwrap();
        assert_eq!(full.chunks(), 1);
        assert_eq!(full.cipher_len(0), Ok(PIECE));
        assert_eq!(full.blocks(), 4);

        let layout = Layout::new(2 * CHUNK as u64 + 1).unwrap();
        assert_eq!(layout.chunks(), 3);
        assert_eq!(layout.offset(2), Ok(2 * CHUNK as u64));
        assert_eq!(layout.plain_len(1), Ok(CHUNK));
        assert_eq!(layout.cipher_len(1), Ok(PIECE));
        assert_eq!(layout.plain_len(2), Ok(1));
        assert_eq!(layout.cipher_len(2), Ok(1 + TAG));
        assert_eq!(layout.cipher_offset(2), Ok(2 * PIECE as u64));
        assert_eq!(layout.ciphertext_len(), 2 * PIECE as u64 + 1 + TAG as u64);
        assert_eq!(layout.blocks(), 9);
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
        assert_eq!(largest.cipher_len(MAX_CHUNKS - 1), Ok(PIECE));
        assert_eq!(largest.position(MAX_CHUNKS - 1), Ok((u32::MAX, true)));
        assert_eq!(largest.ciphertext_len(), 1 << 48);
        assert_eq!(largest.blocks(), 1 << 34);
        assert_eq!(
            largest.cipher_offset(MAX_CHUNKS - 1),
            Ok((1 << 48) - PIECE as u64)
        );
        for object_len in [MAX_OBJECT + 1, 1 << 48, u64::MAX] {
            assert_eq!(Layout::new(object_len), Err(Error::TooLarge { object_len }));
        }
    }

    #[test]
    fn tree_builder_counts_pieces() {
        let layout = layout_of(2, 1);
        let commitment = Key::from_bytes([5; KEY]).commitment();
        let mut builder = TreeBuilder::new(layout, commitment);
        builder.push([0; HASH]);
        assert_eq!(
            builder.finish().unwrap_err(),
            Error::PieceCount {
                expected: 2,
                actual: 1
            }
        );
        let builder = TreeBuilder::new(Layout::new(0).unwrap(), commitment);
        assert_eq!(
            builder.finish().unwrap_err(),
            Error::PieceCount {
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
            ((1 << 48) - (1 << 41), (1 << 48) - (1 << 41)),
            ((1 << 48) - (1 << 41) + 1, 1 << 48),
            (MAX_OBJECT, 1 << 48),
            (1 << 48, 1 << 48),
        ];
        for (len, padded) in known {
            assert_eq!(padme(len), padded, "padme({len})");
        }
        assert!(padme((1 << 48) - (1 << 41)) <= MAX_OBJECT);
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
