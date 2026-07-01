//! cendb-storage: the unified PAX storage substrate.
//!
//! This crate implements the on-disk layout shared by every CenDB projection.
//! The design follows the architectural spec:
//!
//!   * `SegmentHeader` is a 64-byte POD written once at segment creation.
//!   * Each segment contains many `Block`s. A `Block` is the PAX unit: it
//!     holds a horizontal partition of rows but stores each column
//!     contiguously in a 64-byte-aligned *minipage*.
//!   * The block header carries a *zone map* (`min/max` per partitioning key
//!     and timestamp) that powers predicate pushdown for scans.
//!   * Variable-length data lives in a per-block *var-heap*; minipages for
//!     string/blob columns store `(offset, len)` slots pointing into the heap
//!     so the column remains scannable as a fixed-width array.
//!
//! Zero-copy reads are the default: `ColumnView<'a, T>` borrows a slice of a
//! frame's bytes and reinterprets it as `&'a [T]` after a single load-time
//! alignment + length check.

pub mod encoding;
pub mod header;
pub mod pax;
pub mod segment;
pub mod zerocopy;

pub use encoding::{Encoding, EncodingCodec};
pub use header::{BlockHeader, ColumnDirectory, ColumnSpec, SegmentHeader};
pub use pax::{ColumnView, PaxBlock, PaxBlockBuilder, RowId};
pub use segment::{BlockDirectory, SegmentFile, SegmentReader, SegmentWriter};
pub use zerocopy::{cast_slice_bytes, cast_slice_mut_bytes, pod_read_at, pod_write_at};

// Re-export the geometry constants so consumers don't need a second dependency
// just to know the page size.
pub use cendb_core::{DEFAULT_BLOCK_SIZE, DEFAULT_PAGE_SIZE, DEFAULT_SEGMENT_SIZE, MINIPAGE_ALIGN};
