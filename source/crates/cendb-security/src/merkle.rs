//! Merkle tree for cryptographic data provenance.
//!
//! Builds a binary hash tree over database blocks. The root hash is
//! stored securely (e.g. in a separate file or external key server).
//! Any tampering with a block changes its hash, which propagates up to
//! the root, making the tampering detectable.

use blake3;

/// A 32-byte BLAKE3 hash.
pub type Hash = [u8; 32];

/// A Merkle tree over a set of data blocks.
#[derive(Clone, Debug)]
pub struct MerkleTree {
    /// Leaf hashes (one per data block).
    leaves: Vec<Hash>,
    /// All internal node hashes, level by level.
    /// `levels[0]` = leaves, `levels[1]` = parents of leaves, etc.
    /// `levels.last()` = the root (single hash).
    levels: Vec<Vec<Hash>>,
}

impl MerkleTree {
    /// Build a Merkle tree from data blocks.
    pub fn build(blocks: &[&[u8]]) -> Self {
        if blocks.is_empty() {
            return Self {
                leaves: Vec::new(),
                levels: vec![Vec::new()],
            };
        }

        // Compute leaf hashes.
        let leaves: Vec<Hash> = blocks
            .iter()
            .map(|block| {
                let h = blake3::hash(block);
                *h.as_bytes()
            })
            .collect();

        // Build internal levels.
        let mut levels = vec![leaves.clone()];
        let mut current = leaves.clone();

        while current.len() > 1 {
            let mut next = Vec::with_capacity((current.len() + 1) / 2);
            let mut i = 0;
            while i < current.len() {
                let left = current[i];
                let right = if i + 1 < current.len() {
                    current[i + 1]
                } else {
                    // Odd node: duplicate the last hash.
                    current[i]
                };
                next.push(hash_pair(&left, &right));
                i += 2;
            }
            levels.push(next.clone());
            current = next;
        }

        Self { leaves, levels }
    }

    /// Get the root hash. Returns `None` if the tree is empty.
    pub fn root(&self) -> Option<Hash> {
        self.levels.last().and_then(|l| l.first().copied())
    }

    /// Number of leaves (data blocks).
    pub fn leaf_count(&self) -> usize {
        self.leaves.len()
    }

    /// Verify that a specific block matches the Merkle tree.
    pub fn verify_block(&self, block_index: usize, block_data: &[u8]) -> bool {
        if block_index >= self.leaves.len() {
            return false;
        }
        let computed = blake3::hash(block_data);
        computed.as_bytes() == &self.leaves[block_index]
    }

    /// Generate a Merkle proof for a specific block. The proof allows
    /// verifying the block's inclusion without downloading the entire tree.
    pub fn proof(&self, block_index: usize) -> Option<MerkleProof> {
        if block_index >= self.leaves.len() {
            return None;
        }

        let mut path = Vec::new();
        let mut idx = block_index;

        for level in &self.levels[..self.levels.len() - 1] {
            let sibling_idx = if idx % 2 == 0 {
                idx + 1
            } else {
                idx.saturating_sub(1)
            };

            let sibling = if sibling_idx < level.len() {
                level[sibling_idx]
            } else {
                // Odd node: the sibling is itself (duplicated).
                level[idx]
            };

            // `true` if the sibling is on the LEFT (idx is odd).
            let sibling_is_left = idx % 2 == 1;
            path.push((sibling, sibling_is_left));
            idx /= 2;
        }

        Some(MerkleProof {
            leaf_hash: self.leaves[block_index],
            path,
            root: self.root()?,
        })
    }

    /// Verify the entire tree against a known root hash.
    pub fn verify_root(&self, expected_root: Hash) -> bool {
        self.root() == Some(expected_root)
    }
}

/// A Merkle proof: allows verifying a single block's inclusion.
#[derive(Clone, Debug)]
pub struct MerkleProof {
    /// The leaf hash of the block being proven.
    pub leaf_hash: Hash,
    /// Path of (sibling_hash, is_left_sibling) from leaf to root.
    pub path: Vec<(Hash, bool)>,
    /// The expected root hash.
    pub root: Hash,
}

impl MerkleProof {
    /// Verify this proof against a block's data.
    pub fn verify(&self, block_data: &[u8]) -> bool {
        let computed_leaf = blake3::hash(block_data);
        if computed_leaf.as_bytes() != &self.leaf_hash {
            return false;
        }

        let mut current = self.leaf_hash;
        for (sibling, is_left_sibling) in &self.path {
            if *is_left_sibling {
                // Sibling is on the left; we're on the right.
                current = hash_pair(sibling, &current);
            } else {
                // Sibling is on the right; we're on the left.
                current = hash_pair(&current, sibling);
            }
        }

        current == self.root
    }
}

/// Hash a pair of child hashes into a parent hash.
fn hash_pair(left: &Hash, right: &Hash) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(left);
    hasher.update(right);
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_verify_tree() {
        let blocks: Vec<&[u8]> = vec![b"block0", b"block1", b"block2", b"block3"];
        let tree = MerkleTree::build(&blocks);

        assert_eq!(tree.leaf_count(), 4);
        assert!(tree.root().is_some());

        // Verify each block.
        for (i, block) in blocks.iter().enumerate() {
            assert!(tree.verify_block(i, block));
        }

        // Tampered block should fail.
        assert!(!tree.verify_block(0, b"TAMPERED"));
    }

    #[test]
    fn merkle_proof_verification() {
        let blocks: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d", b"e"];
        let tree = MerkleTree::build(&blocks);

        for (i, block) in blocks.iter().enumerate() {
            let proof = tree.proof(i).unwrap();
            assert!(proof.verify(block), "proof for block {} failed", i);
            assert!(!proof.verify(b"wrong data"), "proof for block {} should reject wrong data", i);
        }
    }

    #[test]
    fn tamper_detection() {
        let blocks: Vec<&[u8]> = vec![b"data1", b"data2", b"data3"];
        let tree = MerkleTree::build(&blocks);
        let root = tree.root().unwrap();

        // Build a new tree with tampered data.
        let tampered: Vec<&[u8]> = vec![b"data1", b"TAMPERED", b"data3"];
        let tampered_tree = MerkleTree::build(&tampered);

        assert_ne!(tampered_tree.root(), Some(root));
    }

    #[test]
    fn single_block_tree() {
        let tree = MerkleTree::build(&[b"only block" as &[u8]]);
        assert_eq!(tree.leaf_count(), 1);
        assert!(tree.root().is_some());
        assert!(tree.verify_block(0, b"only block"));
    }

    #[test]
    fn empty_tree() {
        let tree = MerkleTree::build(&[] as &[&[u8]]);
        assert_eq!(tree.leaf_count(), 0);
        assert!(tree.root().is_none());
    }
}
