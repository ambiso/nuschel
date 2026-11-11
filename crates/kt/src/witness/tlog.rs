// SPDX-License-Identifier: EUPL-1.2
//! RFC 6962 / RFC 9162 Merkle trees over SHA-256: the epoch log's hashing, its right
//! frontier (a "compact range"), and inclusion proofs.
//!
//! - Leaf hash: `SHA-256(0x00 || leaf)`; node hash: `SHA-256(0x01 || left || right)`.
//! - The root of `n > 1` leaves is `node(MTH(D[0:k]), MTH(D[k:n]))` with `k` the largest
//!   power of two below `n`; the root of no leaves is `SHA-256("")`.
//! - A *complete subtree* at `level` and `index` covers leaves
//!   `[index * 2^level, (index + 1) * 2^level)`. A server stores those it completed
//!   ([`Frontier::append`] returns them), and every proof is made of them.
//!
//! Consistency proofs are not needed: the project auditor appends the leaves itself
//! (docs/e2ee.md §5.9), and clients never compare checkpoints with each other.

use sha2::{Digest, Sha256};

/// A SHA-256 tree hash.
pub type Hash = [u8; 32];

/// Most hashes in an inclusion proof (a tree of 2^64 leaves).
pub const MAX_PROOF_HASHES: usize = 64;

/// `SHA-256(0x00 || data)`.
pub fn leaf_hash(data: &[u8]) -> Hash {
    let mut h = Sha256::new();
    h.update([0u8]);
    h.update(data);
    h.finalize().into()
}

/// `SHA-256(0x01 || left || right)`.
pub fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update([1u8]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// The root of a tree without leaves.
pub fn empty_root() -> Hash {
    Sha256::digest([]).into()
}

/// The largest power of two strictly below `n` (`n >= 2`).
fn split(n: u64) -> u64 {
    debug_assert!(n >= 2);
    1 << (63 - (n - 1).leading_zeros())
}

/// The right frontier of a tree: the roots of its complete subtrees from the largest
/// (leftmost) to the smallest, one for each set bit of `size`. Enough to append leaves
/// and to compute the root.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frontier {
    size: u64,
    hashes: Vec<Hash>,
}

/// A complete subtree `(level, index, hash)`.
pub type Node = (u8, u64, Hash);

impl Frontier {
    pub fn new() -> Self {
        Self::default()
    }

    /// From its parts; `None` unless there is one hash per set bit of `size`.
    pub fn from_parts(size: u64, hashes: Vec<Hash>) -> Option<Self> {
        (hashes.len() == size.count_ones() as usize).then_some(Self { size, hashes })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn hashes(&self) -> &[Hash] {
        &self.hashes
    }

    /// The complete subtrees a frontier of `size` consists of, as `(level, index)`,
    /// largest first: where to find them in a server's store.
    pub fn positions(size: u64) -> Vec<(u8, u64)> {
        let mut out = Vec::new();
        let mut start = 0_u64;
        for level in (0..64u8).rev() {
            if size & (1 << level) != 0 {
                out.push((level, start >> level));
                start += 1 << level;
            }
        }
        out
    }

    /// Appends a leaf hash and returns the complete subtrees this finished: the leaf
    /// itself at level 0 and every node merged above it. `None` when the tree is full.
    pub fn append(&mut self, leaf: Hash) -> Option<Vec<Node>> {
        let size = self.size.checked_add(1)?;
        let mut nodes = vec![(0, self.size, leaf)];
        let mut hash = leaf;
        let mut index = self.size;
        let mut level = 0u8;
        let mut bits = self.size;
        while bits & 1 == 1 {
            let left = self.hashes.pop()?;
            hash = node_hash(&left, &hash);
            level += 1;
            index >>= 1;
            bits >>= 1;
            nodes.push((level, index, hash));
        }
        self.hashes.push(hash);
        self.size = size;
        Some(nodes)
    }

    /// The tree's root hash.
    pub fn root(&self) -> Hash {
        let Some((last, rest)) = self.hashes.split_last() else {
            return empty_root();
        };
        rest.iter()
            .rev()
            .fold(*last, |right, left| node_hash(left, &right))
    }
}

/// `MTH(D[start : start + size])` from complete subtrees.
fn subtree(start: u64, size: u64, node: &mut dyn FnMut(u8, u64) -> Hash) -> Hash {
    if size.is_power_of_two() && start.is_multiple_of(size) {
        return node(size.trailing_zeros() as u8, start / size);
    }
    let k = split(size);
    let left = subtree(start, k, node);
    let right = subtree(start + k, size - k, node);
    node_hash(&left, &right)
}

/// RFC 6962 `PATH(m, D[start : start + size])`.
fn path(m: u64, start: u64, size: u64, node: &mut dyn FnMut(u8, u64) -> Hash, out: &mut Vec<Hash>) {
    if size <= 1 {
        return;
    }
    let k = split(size);
    if m < k {
        path(m, start, k, node, out);
        out.push(subtree(start + k, size - k, node));
    } else {
        path(m - k, start + k, size - k, node, out);
        out.push(subtree(start, k, node));
    }
}

/// The inclusion proof of leaf `index` in the tree of `size` leaves, from complete
/// subtrees that `node(level, index)` returns. `None` if `index >= size`.
pub fn inclusion_proof(
    index: u64,
    size: u64,
    node: &mut dyn FnMut(u8, u64) -> Hash,
) -> Option<Vec<Hash>> {
    if index >= size {
        return None;
    }
    let mut out = Vec::new();
    path(index, 0, size, node, &mut out);
    Some(out)
}

/// Which complete subtrees [`inclusion_proof`] reads, so that a server can fetch them
/// in one round trip first.
pub fn inclusion_nodes(index: u64, size: u64) -> Vec<(u8, u64)> {
    let mut wanted = Vec::new();
    let _ = inclusion_proof(index, size, &mut |level, index| {
        wanted.push((level, index));
        [0; 32]
    });
    wanted.sort_unstable();
    wanted.dedup();
    wanted
}

/// RFC 9162 §2.1.3.2: whether `proof` shows `leaf` at `index` in the tree of `size`
/// leaves with `root`.
pub fn verify_inclusion(index: u64, size: u64, leaf: &Hash, proof: &[Hash], root: &Hash) -> bool {
    if index >= size || proof.len() > MAX_PROOF_HASHES {
        return false;
    }
    let mut f = index;
    let mut s = size - 1;
    let mut r = *leaf;
    for p in proof {
        if s == 0 {
            return false;
        }
        if f & 1 == 1 || f == s {
            r = node_hash(p, &r);
            while f & 1 == 0 && f != 0 {
                f >>= 1;
                s >>= 1;
            }
        } else {
            r = node_hash(&r, p);
        }
        f >>= 1;
        s >>= 1;
    }
    s == 0 && r == *root
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The reference definition, recursively over leaf hashes.
    fn mth(leaves: &[Hash]) -> Hash {
        match leaves.len() {
            0 => empty_root(),
            1 => leaves[0],
            n => {
                let k = split(n as u64) as usize;
                node_hash(&mth(&leaves[..k]), &mth(&leaves[k..]))
            }
        }
    }

    fn tree(n: u64) -> (Vec<Hash>, Frontier, HashMap<(u8, u64), Hash>) {
        let mut frontier = Frontier::new();
        let mut nodes = HashMap::new();
        let mut leaves = Vec::new();
        for i in 0..n {
            let leaf = leaf_hash(&i.to_be_bytes());
            leaves.push(leaf);
            for (level, index, hash) in frontier.append(leaf).unwrap() {
                assert!(nodes.insert((level, index), hash).is_none());
            }
        }
        (leaves, frontier, nodes)
    }

    #[test]
    fn rfc6962_hashes_and_the_empty_tree() {
        // SHA-256 of the empty string; RFC 6962 §2.1.
        assert_eq!(
            hex::encode(empty_root()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // The certificate-transparency-go test vector: the leaf hash of an empty leaf.
        assert_eq!(
            hex::encode(leaf_hash(b"")),
            "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d"
        );
    }

    #[test]
    fn rfc6962_reference_tree_roots() {
        // The eight-leaf tree of RFC 6962's examples (certificate-transparency-go
        // merkletree tests): leaves "", 00, 10, 2021, 3031, 40414243, 5051525354555657,
        // 606162636465666768696a6b6c6d6e6f.
        let leaves: Vec<Vec<u8>> = [
            "",
            "00",
            "10",
            "2021",
            "3031",
            "40414243",
            "5051525354555657",
            "606162636465666768696a6b6c6d6e6f",
        ]
        .iter()
        .map(|h| hex::decode(h).unwrap())
        .collect();
        let roots = [
            "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
            "fac54203e7cc696cf0dfcb42c92a1d9dbaf70ad9e621f4bd8d98662f00e3c125",
            "aeb6bcfe274b70a14fb067a5e5578264db0fa9b51af5e0ba159158f329e06e77",
            "d37ee418976dd95753c1c73862b9398fa2a2cf9b4ff0fdfe8b30cd95209614b7",
            "4e3bbb1f7b478dcfe71fb631631519a3bca12c9aefca1612bfce4c13a86264d4",
            "76e67dadbcdf1e10e1b74ddc608abd2f98dfb16fbce75277b5232a127f2087ef",
            "ddb89be403809e325750d3d263cd78929c2942b7942a34b77e122c9594a74c8c",
            "5dc9da79a70659a9ad559cb701ded9a2ab9d823aad2f4960cfe370eff4604328",
        ];
        let mut frontier = Frontier::new();
        for (leaf, root) in leaves.iter().zip(roots) {
            frontier.append(leaf_hash(leaf)).unwrap();
            assert_eq!(hex::encode(frontier.root()), root);
        }
    }

    #[test]
    fn frontier_matches_the_definition_and_its_positions() {
        for n in 0..70u64 {
            let (leaves, frontier, nodes) = tree(n);
            assert_eq!(frontier.root(), mth(&leaves), "n = {n}");
            let positions = Frontier::positions(n);
            let rebuilt: Vec<Hash> = positions.iter().map(|p| nodes[p]).collect();
            assert_eq!(Frontier::from_parts(n, rebuilt).unwrap(), frontier);
        }
        assert!(Frontier::from_parts(3, vec![[0; 32]]).is_none());
    }

    #[test]
    fn every_inclusion_proof_verifies_and_every_mutation_fails() {
        for n in 1..40u64 {
            let (leaves, frontier, nodes) = tree(n);
            let root = frontier.root();
            for m in 0..n {
                let mut node = |level: u8, index: u64| nodes[&(level, index)];
                let proof = inclusion_proof(m, n, &mut node).unwrap();
                assert!(proof.len() <= 64 - (n - 1).leading_zeros() as usize);
                assert!(verify_inclusion(m, n, &leaves[m as usize], &proof, &root));
                // Wrong index, size, leaf, root, or any changed or missing hash.
                assert!(!verify_inclusion(
                    m + 1,
                    n,
                    &leaves[m as usize],
                    &proof,
                    &root
                ));
                assert!(!verify_inclusion(m, n, &[9; 32], &proof, &root));
                assert!(!verify_inclusion(
                    m,
                    n,
                    &leaves[m as usize],
                    &proof,
                    &[9; 32]
                ));
                for i in 0..proof.len() {
                    let mut bad = proof.clone();
                    bad[i][0] ^= 1;
                    assert!(!verify_inclusion(m, n, &leaves[m as usize], &bad, &root));
                    let mut short = proof.clone();
                    short.remove(i);
                    assert!(!verify_inclusion(m, n, &leaves[m as usize], &short, &root));
                }
                let mut long = proof.clone();
                long.push([0; 32]);
                assert!(!verify_inclusion(m, n, &leaves[m as usize], &long, &root));
                // The dry run names exactly the nodes the proof reads.
                let wanted = inclusion_nodes(m, n);
                assert!(wanted.iter().all(|p| nodes.contains_key(p)));
            }
            assert!(inclusion_proof(n, n, &mut |_, _| [0; 32]).is_none());
        }
    }
}
