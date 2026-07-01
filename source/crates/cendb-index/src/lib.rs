//! cendb-index: Adaptive Radix Tree (ART) primary index.
//!
//! ART is the chosen primary in-memory index for CenDB (§4.2 of the spec).
//! Properties:
//!
//!   * **O(k) lookup** on key length k — does not depend on the number of
//!     keys in the tree.
//!   * **Order-preserving** (lexicographic on the byte representation) so
//!     range queries walk the tree in sorted order.
//!   * **Adaptive node sizing** (Node4 → Node16 → Node48 → Node256) keeps
//!     memory near-optimal vs a fixed-fanout B-tree.
//!   * **No rebalancing** on insert — simpler concurrency than B-trees.
//!
//! ## Implementation notes
//!
//! This is a single-threaded, owned-tree implementation. A production
//! version would add the ROWEX latch-free concurrency protocol; we leave
//! that as future work and route concurrent access through a `Mutex` at
//! the call site.
//!
//! Keys are byte slices (`&[u8]`). Values are `V: Clone` (typically a
//! `RowLocator`).

pub mod art;

pub use art::{ArtIter, ArtRangeIter, ArtTree};
