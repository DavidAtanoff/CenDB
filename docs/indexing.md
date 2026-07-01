# Adaptive Radix Tree (ART) Index

The `cendb-index` crate implements the Adaptive Radix Tree (ART), the
chosen primary in-memory index for CenDB (§4.2 of the spec).

## Why ART?

| Need | ART property |
|---|---|
| **O(k) point lookups** | Lookup time depends on key length, not key count. |
| **Range scans** | Order-preserving (lexicographic on byte representation). |
| **Space-efficient** | Adaptive node sizing (Node4 → Node16 → Node48 → Node256). |
| **No rebalancing** | Simpler concurrency than B-trees. |
| **Insert-friendly** | Path compression keeps height low. |

## Node types

The canonical ART (Leis et al. 2013) uses four node types that grow
adaptively:

| Type | Capacity | Layout | When to use |
|---|---|---|---|
| `Node4` | 4 children | Linear scan over keys. | Sparse nodes (default). |
| `Node16` | 16 children | Linear scan; SIMD-friendly. | Growing nodes. |
| `Node48` | 48 children | 256-entry indirection table → 48-slot child array. | Mid-fanout. |
| `Node256` | 256 children | Direct-indexed by byte. | Dense nodes (full byte fanout). |

**Prototype simplification**: this implementation uses a single
`Interior` variant backed by a `Vec<(u8, Option<Box<ArtNode>>)>)`
sorted by key byte. The Node4/16/48/256 layouts are an *optimization*
on top of this — they reduce per-node memory and speed up child lookup.
The algorithmic complexity is identical; we trade some memory for code
simplicity in the prototype.

## Path compression

Long internal edges where every node has a single child are compressed
into a `prefix` byte slice stored in the parent node:

```
Keys: ["hello", "help", "helium"]

Without compression:
  root → 'h' → 'e' → 'l' → 'l' → 'o' (leaf "hello")
                            ↘ 'p' (leaf "help")
                  ↘ 'i' → 'u' → 'm' (leaf "helium")

With compression:
  root (prefix="hel")
    ├─ 'l' → 'o' (leaf "hello")
    ├─ 'p' (leaf "help")
    └─ 'i' → 'u' → 'm' (leaf "helium")
```

This keeps tree height bounded by `O(k)` worst-case but typically
`O(k / w)` where `w` is the average prefix length.

## Operations

### Insert

```rust
let mut t: ArtTree<u64> = ArtTree::new();
t.insert(b"hello", 1);
t.insert(b"world", 2);
t.insert(b"helium", 3);
```

Insertion:
1. Walk down the tree, matching prefix bytes.
2. If a leaf is reached whose key differs from the insert key, split
   the leaf into an `Interior` node holding both leaves.
3. If an interior node's prefix doesn't fully match, split the node at
   the mismatch point.
4. Returns the previous value if the key already existed.

### Get

```rust
let v: Option<u64> = t.get(b"hello");  // Some(1)
```

O(k) lookup: walk down the tree, matching prefix bytes and selecting
children by the next key byte.

### Remove

```rust
let prev: Option<u64> = t.remove(b"hello");  // Some(1)
```

Removal walks down to the leaf, removes it, and removes the now-empty
parent interior node. For the prototype we don't merge under-full
nodes (a production version would).

### Iterate (sorted)

```rust
for (key, value) in t.iter() {
    println!("{:?} → {}", key, value);
}
```

In-order traversal yields keys in lexicographic order.

### Range scan

```rust
for (key, value) in t.range(b"key_0050", Some(b"key_0060")) {
    println!("{:?} → {}", key, value);
}
```

Yields keys `k` with `start <= k < end`.

## Use as primary index

ART is the primary in-memory index for KV point lookups and relational
primary keys:

```rust
use cendb_index::ArtTree;
use cendb_core::RowLocator;

let mut index: ArtTree<RowLocator> = ArtTree::new();
index.insert(b"alice", RowLocator::new(segment, block, slot));

let loc = index.get(b"alice").unwrap();
// → fetch the row at (segment, block, slot)
```

## Performance characteristics

| Operation | Complexity |
|---|---|
| Insert | O(k) |
| Get | O(k) |
| Remove | O(k) |
| Iterate (full) | O(n) |
| Range scan [start, end) | O(log n + m) where m = result size |

where `k` is the key length and `n` is the number of keys.

Memory: approximately `n * (k + 32)` bytes for random 8-byte keys (much
less than a `HashMap<Vec<u8>, V>` which is `n * (k + 64 + 32)`).

## Concurrency (future work)

The current implementation is single-threaded. A production version
would add the **ROWEX** (Read-Optimized Write-Exclusive) latch-free
concurrency protocol from the original Leis et al. paper. ROWEX uses
optimistic latch-free reads (with version checks) and exclusive latches
on writes.

## Persistence

ART is an in-memory structure. Persistence is provided by the segment
file's B-link tree (planned); ART is rebuilt on cold-start from the
segment's block directory. A background compaction merges runs into
the persistent B-link tree.

## API summary

```rust
impl<V: Clone> ArtTree<V> {
    pub fn new() -> Self;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn insert(&mut self, key: &[u8], value: V) -> Option<V>;
    pub fn get(&self, key: &[u8]) -> Option<V>;
    pub fn remove(&mut self, key: &[u8]) -> Option<V>;
    pub fn iter(&self) -> ArtIter<'_, V>;
    pub fn range(&self, start: &[u8], end: Option<&[u8]>) -> ArtRangeIter<'_, V>;
}
```
