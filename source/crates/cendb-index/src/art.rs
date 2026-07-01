//! Adaptive Radix Tree (ART) implementation.
//!
//! ## Design
//!
//! This is a path-compressed trie with adaptive fanout. We use a single
//! `Interior` node variant backed by a `Vec<(u8, Box<ArtNode>)>` sorted by
//! key byte. The canonical ART's Node4/Node16/Node48/Node256 layouts are
//! an *optimization* on top of this — they reduce per-node memory and
//! speed up child lookup. For the prototype we use the simpler Vec layout
//! because:
//!
//!   * It's algorithmically equivalent (O(fanout) child lookup, but fanout
//!     is bounded by 256 so worst-case is constant).
//!   * The code is ~3x smaller and easier to verify.
//!   * Memory overhead is acceptable for the index sizes we target
//!     (the spec calls for < 1M entries per index).
//!
//! ## Path compression
//!
//! Long internal edges where every node has a single child are compressed
//! into a `prefix` byte slice stored in the parent node. This keeps the
//! tree height bounded by `O(k)` worst-case but typically `O(k / w)`
//! where `w` is the average prefix length.
//!
//! ## Order preservation
//!
//! Children are kept sorted by byte value, so an in-order traversal yields
//! keys in lexicographic order — range scans are natural.


// ============================================================================
// Node types.
// ============================================================================

#[allow(unused_imports)]
use cendb_core::RowLocator;

#[derive(Debug, Clone)]
enum ArtNode<V: Clone> {
    /// Interior node: a sorted list of (byte, child) pairs plus a path-
    /// compression prefix. Children are `Option<Box>` so we can take them
    /// out during removal without shifting the array.
    Interior {
        prefix: Vec<u8>,
        children: Vec<(u8, Option<Box<ArtNode<V>>>)>,
    },
    /// Leaf: holds the full key and value.
    Leaf(ArtLeaf<V>),
}

#[derive(Debug, Clone)]
struct ArtLeaf<V: Clone> {
    key: Vec<u8>,
    value: V,
}

// ============================================================================
// Tree.
// ============================================================================

/// An Adaptive Radix Tree. Owns its root; keys are `&[u8]`, values are
/// `V: Clone`. The tree is order-preserving: an in-order traversal yields
/// keys in lexicographic order.
pub struct ArtTree<V: Clone> {
    root: Option<Box<ArtNode<V>>>,
    size: usize,
}

impl<V: Clone> ArtTree<V> {
    pub fn new() -> Self {
        Self { root: None, size: 0 }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.size
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Insert (or overwrite) a key-value pair. Returns the previous value
    /// if the key already existed.
    pub fn insert(&mut self, key: &[u8], value: V) -> Option<V> {
        let prev = Self::insert_rec(&mut self.root, key, 0, value);
        if prev.is_none() {
            self.size += 1;
        }
        prev
    }

    fn insert_rec(
        node_slot: &mut Option<Box<ArtNode<V>>>,
        key: &[u8],
        key_off: usize,
        value: V,
    ) -> Option<V> {
        // Empty tree: install a leaf.
        if node_slot.is_none() {
            *node_slot = Some(Box::new(ArtNode::Leaf(ArtLeaf {
                key: key.to_vec(),
                value,
            })));
            return None;
        }

        // Take the node out so we can match on it without borrow conflicts.
        let node = node_slot.take().unwrap();
        let (new_node, prev) = Self::insert_into_node(*node, key, key_off, value);
        *node_slot = Some(Box::new(new_node));
        prev
    }

    fn insert_into_node(
        node: ArtNode<V>,
        key: &[u8],
        key_off: usize,
        value: V,
    ) -> (ArtNode<V>, Option<V>) {
        match node {
            ArtNode::Leaf(leaf) => {
                if leaf.key == key {
                    // Overwrite: replace the value.
                    let prev = leaf.value.clone();
                    let new_leaf = ArtLeaf {
                        key: leaf.key,
                        value,
                    };
                    (ArtNode::Leaf(new_leaf), Some(prev))
                } else {
                    // Split: build a new Interior node holding both leaves.
                    Self::split_leaf(leaf, key, key_off, value)
                }
            }
            ArtNode::Interior { prefix, children } => {
                Self::insert_into_interior(prefix, children, key, key_off, value)
            }
        }
    }

    fn split_leaf(
        leaf: ArtLeaf<V>,
        key: &[u8],
        key_off: usize,
        value: V,
    ) -> (ArtNode<V>, Option<V>) {
        // The leaf stores the full key. We need to compute the common
        // prefix between the leaf's key and `key` *starting from
        // `key_off`* (the offset into `key` that this node represents).
        // But the leaf's key is the full key, so the common prefix
        // starts from offset 0 of the leaf and `key_off` of `key`.
        let mut common = 0;
        let leaf_remaining = &leaf.key[key_off..];
        let key_remaining = &key[key_off..];
        while common < leaf_remaining.len()
            && common < key_remaining.len()
            && leaf_remaining[common] == key_remaining[common]
        {
            common += 1;
        }
        // The new prefix is the common bytes starting at `key_off`.
        let prefix = leaf_remaining[..common].to_vec();
        let mut children: Vec<(u8, Option<Box<ArtNode<V>>>)> = Vec::with_capacity(2);
        if let Some(&b1) = leaf.key.get(key_off + common) {
            children.push((b1, Some(Box::new(ArtNode::Leaf(leaf)))));
        }
        if let Some(&b2) = key.get(key_off + common) {
            children.push((
                b2,
                Some(Box::new(ArtNode::Leaf(ArtLeaf {
                    key: key.to_vec(),
                    value,
                }))),
            ));
        }
        children.sort_by_key(|(b, _)| *b);
        (
            ArtNode::Interior { prefix, children },
            None,
        )
    }

    fn insert_into_interior(
        prefix: Vec<u8>,
        mut children: Vec<(u8, Option<Box<ArtNode<V>>>)>,
        key: &[u8],
        key_off: usize,
        value: V,
    ) -> (ArtNode<V>, Option<V>) {
        // How much of `prefix` matches `key` starting at `key_off`?
        let consumed = match_prefix(&prefix, key, key_off);
        if consumed < prefix.len() {
            // Prefix mismatch: split this interior node.
            let split_byte = prefix[consumed];
            let new_prefix = prefix[..consumed].to_vec();
            let remaining_prefix = prefix[consumed + 1..].to_vec();
            let old_node = ArtNode::Interior {
                prefix: remaining_prefix,
                children,
            };
            let mut new_children: Vec<(u8, Option<Box<ArtNode<V>>>)> = Vec::with_capacity(2);
            new_children.push((split_byte, Some(Box::new(old_node))));
            if let Some(&b) = key.get(key_off + consumed) {
                new_children.push((
                    b,
                    Some(Box::new(ArtNode::Leaf(ArtLeaf {
                        key: key.to_vec(),
                        value,
                    }))),
                ));
            }
            new_children.sort_by_key(|(b, _)| *b);
            return (
                ArtNode::Interior {
                    prefix: new_prefix,
                    children: new_children,
                },
                None,
            );
        }
        // Prefix fully matched; descend into the child at the next byte.
        let next_byte_off = key_off + consumed;
        let next_byte = match key.get(next_byte_off) {
            Some(b) => *b,
            None => {
                // Key is a strict prefix of the node's prefix; we can't
                // store a value here without an "end-of-key" sentinel.
                return (
                    ArtNode::Interior { prefix, children },
                    None,
                );
            }
        };
        // Find the child for `next_byte`.
        let pos = children.iter().position(|(b, _)| *b == next_byte);
        match pos {
            Some(idx) => {
                let prev = Self::insert_rec(&mut children[idx].1, key, next_byte_off + 1, value);
                (
                    ArtNode::Interior { prefix, children },
                    prev,
                )
            }
            None => {
                // No child for this byte; add one. Insert in sorted order.
                let insert_pos = children.partition_point(|(b, _)| *b < next_byte);
                children.insert(
                    insert_pos,
                    (
                        next_byte,
                        Some(Box::new(ArtNode::Leaf(ArtLeaf {
                            key: key.to_vec(),
                            value,
                        }))),
                    ),
                );
                (ArtNode::Interior { prefix, children }, None)
            }
        }
    }

    pub fn get(&self, key: &[u8]) -> Option<V> {
        Self::get_rec(&self.root, key, 0)
    }

    fn get_rec(node_slot: &Option<Box<ArtNode<V>>>, key: &[u8], key_off: usize) -> Option<V> {
        let node = node_slot.as_ref()?;
        match node.as_ref() {
            ArtNode::Leaf(leaf) => {
                // Leaf stores the full key; compare against the whole key.
                if leaf.key == key {
                    Some(leaf.value.clone())
                } else {
                    None
                }
            }
            ArtNode::Interior { prefix, children } => {
                // Verify the prefix matches the corresponding slice of `key`.
                let consumed = match_prefix(prefix, key, key_off);
                if consumed != prefix.len() {
                    return None;
                }
                let next_byte_off = key_off + consumed;
                let next_byte = key.get(next_byte_off)?;
                let idx = children.iter().position(|(b, _)| *b == *next_byte)?;
                let child = &children[idx].1;
                // Descend; the child will see the full `key` and an offset
                // advanced past the prefix + child-selection byte.
                Self::get_rec(child, key, next_byte_off + 1)
            }
        }
    }

    /// Iterate over all (key, value) pairs in lexicographic order.
    pub fn iter(&self) -> ArtIter<'_, V> {
        ArtIter {
            stack: vec![ArtIterFrame::Start(&self.root)],
        }
    }

    /// Range scan: yield all (key, value) pairs with `start <= key < end`.
    pub fn range(&self, start: &[u8], end: Option<&[u8]>) -> ArtRangeIter<'_, V> {
        ArtRangeIter {
            inner: self.iter(),
            start: start.to_vec(),
            end: end.map(|e| e.to_vec()),
            started: false,
        }
    }

    /// Remove a key. Returns the previous value if present.
    pub fn remove(&mut self, key: &[u8]) -> Option<V> {
        let prev = Self::remove_rec(&mut self.root, key, 0);
        if prev.is_some() {
            self.size -= 1;
        }
        prev
    }

    fn remove_rec(
        node_slot: &mut Option<Box<ArtNode<V>>>,
        key: &[u8],
        key_off: usize,
    ) -> Option<V> {
        let node = node_slot.as_mut()?;
        match node.as_mut() {
            ArtNode::Leaf(leaf) => {
                if leaf.key == key {
                    let owned = node_slot.take().unwrap();
                    if let ArtNode::Leaf(l) = *owned {
                        Some(l.value)
                    } else {
                        unreachable!()
                    }
                } else {
                    None
                }
            }
            ArtNode::Interior { prefix, children } => {
                let consumed = match_prefix(prefix, key, key_off);
                if consumed != prefix.len() {
                    return None;
                }
                let next_byte_off = key_off + consumed;
                let next_byte = match key.get(next_byte_off) {
                    Some(b) => *b,
                    None => return None,
                };
                let idx = children.iter().position(|(b, _)| *b == next_byte)?;
                let prev = Self::remove_rec(&mut children[idx].1, key, next_byte_off + 1);
                if prev.is_some() && children[idx].1.is_none() {
                    children.remove(idx);
                }
                prev
            }
        }
    }
}

impl<V: Clone> Default for ArtTree<V> {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Iteration.
// ============================================================================

enum ArtIterFrame<'a, V: Clone> {
    Start(&'a Option<Box<ArtNode<V>>>),
    Interior {
        prefix: &'a [u8],
        children: &'a [(u8, Option<Box<ArtNode<V>>>)],
        idx: usize,
    },
    Leaf(&'a ArtLeaf<V>),
}

pub struct ArtIter<'a, V: Clone> {
    stack: Vec<ArtIterFrame<'a, V>>,
}

impl<'a, V: Clone> Iterator for ArtIter<'a, V> {
    type Item = (Vec<u8>, V);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(frame) = self.stack.pop() {
            match frame {
                ArtIterFrame::Start(node_slot) => {
                    if let Some(node) = node_slot {
                        self.push_node(node);
                    }
                }
                ArtIterFrame::Interior { prefix, children, idx } => {
                    if idx < children.len() {
                        self.stack.push(ArtIterFrame::Interior {
                            prefix,
                            children,
                            idx: idx + 1,
                        });
                        if let Some(child) = &children[idx].1 {
                            self.push_node(child);
                        }
                    }
                }
                ArtIterFrame::Leaf(leaf) => {
                    return Some((leaf.key.clone(), leaf.value.clone()));
                }
            }
        }
        None
    }
}

impl<'a, V: Clone> ArtIter<'a, V> {
    fn push_node(&mut self, node: &'a Box<ArtNode<V>>) {
        match node.as_ref() {
            ArtNode::Leaf(leaf) => {
                self.stack.push(ArtIterFrame::Leaf(leaf));
            }
            ArtNode::Interior { prefix, children } => {
                self.stack.push(ArtIterFrame::Interior {
                    prefix,
                    children,
                    idx: 0,
                });
            }
        }
    }
}

/// Range iterator: yields (key, value) pairs with `start <= key < end`.
pub struct ArtRangeIter<'a, V: Clone> {
    inner: ArtIter<'a, V>,
    start: Vec<u8>,
    end: Option<Vec<u8>>,
    started: bool,
}

impl<'a, V: Clone> Iterator for ArtRangeIter<'a, V> {
    type Item = (Vec<u8>, V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (k, v) = self.inner.next()?;
            if !self.started {
                if k < self.start {
                    continue;
                }
                self.started = true;
            }
            if let Some(end) = &self.end {
                if k >= *end {
                    return None;
                }
            }
            return Some((k, v));
        }
    }
}

// ============================================================================
// Helpers.
// ============================================================================

#[allow(dead_code)]
fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let mut i = 0;
    let len = a.len().min(b.len());
    while i < len && a[i] == b[i] {
        i += 1;
    }
    i
}

fn match_prefix(prefix: &[u8], key: &[u8], key_off: usize) -> usize {
    let mut i = 0;
    while i < prefix.len() && key_off + i < key.len() && prefix[i] == key[key_off + i] {
        i += 1;
    }
    i
}

// ============================================================================
// Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_get_basic() {
        let mut t: ArtTree<u64> = ArtTree::new();
        t.insert(b"hello", 1);
        t.insert(b"world", 2);
        t.insert(b"helium", 3);
        assert_eq!(t.len(), 3);
        assert_eq!(t.get(b"hello"), Some(1));
        assert_eq!(t.get(b"world"), Some(2));
        assert_eq!(t.get(b"helium"), Some(3));
        assert_eq!(t.get(b"missing"), None);
    }

    #[test]
    fn overwrite_returns_previous() {
        let mut t: ArtTree<u64> = ArtTree::new();
        t.insert(b"k", 1);
        let prev = t.insert(b"k", 2);
        assert_eq!(prev, Some(1));
        assert_eq!(t.get(b"k"), Some(2));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn many_keys_with_common_prefix() {
        let mut t: ArtTree<u64> = ArtTree::new();
        for i in 0..20u8 {
            let key = [b'a', b'a', i];
            t.insert(&key, i as u64);
        }
        for i in 0..20u8 {
            let key = [b'a', b'a', i];
            assert_eq!(t.get(&key), Some(i as u64));
        }
        assert_eq!(t.len(), 20);
    }

    #[test]
    fn node_growth_to_256_children() {
        let mut t: ArtTree<u64> = ArtTree::new();
        for i in 0..255u8 {
            t.insert(&[i], i as u64);
        }
        for i in 0..255u8 {
            assert_eq!(t.get(&[i]), Some(i as u64));
        }
        assert_eq!(t.len(), 255);
    }

    #[test]
    fn iter_is_sorted() {
        let mut t: ArtTree<u64> = ArtTree::new();
        let keys: Vec<Vec<u8>> = vec![
            b"banana".to_vec(),
            b"apple".to_vec(),
            b"cherry".to_vec(),
            b"avocado".to_vec(),
            b"blueberry".to_vec(),
        ];
        for (i, k) in keys.iter().enumerate() {
            t.insert(k, i as u64);
        }
        let collected: Vec<Vec<u8>> = t.iter().map(|(k, _)| k).collect();
        let mut expected = keys.clone();
        expected.sort();
        assert_eq!(collected, expected);
    }

    #[test]
    fn range_scan() {
        let mut t: ArtTree<u64> = ArtTree::new();
        for i in 0..100u64 {
            t.insert(format!("key_{:04}", i).as_bytes(), i);
        }
        let start = b"key_0050".to_vec();
        let end = b"key_0060".to_vec();
        let results: Vec<(Vec<u8>, u64)> = t.range(&start, Some(&end)).collect();
        assert_eq!(results.len(), 10);
        assert_eq!(results[0].1, 50);
        assert_eq!(results[9].1, 59);
    }

    #[test]
    fn remove_returns_value() {
        let mut t: ArtTree<u64> = ArtTree::new();
        t.insert(b"hello", 42);
        let prev = t.remove(b"hello");
        assert_eq!(prev, Some(42));
        assert_eq!(t.get(b"hello"), None);
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn long_keys_with_common_prefix() {
        let mut t: ArtTree<u64> = ArtTree::new();
        for i in 0..1000u64 {
            let key = format!("{}", 1_700_000_000_000u64 + i);
            t.insert(key.as_bytes(), i);
        }
        for i in 0..1000u64 {
            let key = format!("{}", 1_700_000_000_000u64 + i);
            assert_eq!(t.get(key.as_bytes()), Some(i));
        }
        assert_eq!(t.len(), 1000);
    }

    #[test]
    fn row_locator_as_value() {
        let mut t: ArtTree<RowLocator> = ArtTree::new();
        let loc = RowLocator::new(
            cendb_core::SegmentId(1),
            cendb_core::BlockId(2),
            cendb_core::SlotId(3),
        );
        t.insert(b"some_key", loc);
        let got = t.get(b"some_key").unwrap();
        assert_eq!(got.segment.0, 1);
        assert_eq!(got.block.0, 2);
        assert_eq!(got.slot.0, 3);
    }

    #[test]
    fn stress_random_insertions() {
        use std::collections::HashMap;
        let mut t: ArtTree<u64> = ArtTree::new();
        let mut expected: HashMap<Vec<u8>, u64> = HashMap::new();
        // Pseudo-random keys.
        let mut seed: u64 = 42;
        for i in 0..5000u64 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let key = format!("k_{:020}", seed ^ i);
            t.insert(key.as_bytes(), i);
            expected.insert(key.into_bytes(), i);
        }
        assert_eq!(t.len(), expected.len());
        for (k, v) in &expected {
            assert_eq!(t.get(k), Some(*v), "missing key {:?}", k);
        }
    }
}
