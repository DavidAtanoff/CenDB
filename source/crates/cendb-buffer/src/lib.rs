//! cendb-buffer: user-space buffer pool with scan-resistant LRU-2 eviction.
//!
//! This crate implements the in-memory cache layer that sits between the
//! segment files on disk and the PAX block readers in [`cendb_storage`]. It
//! deliberately avoids `mmap` for the reasons set out in §3.1 of the spec:
//!
//!   * Explicit I/O lets us prefetch, batch, and propagate errors as
//!     `Result<>` instead of `SIGBUS`.
//!   * We control eviction so a sequential analytical scan cannot evict the
//!     OLTP working set (scan resistance).
//!   * We can pin a hard memory budget (`pool_frames * page_size` bytes).
//!
//! ## Architecture
//!
//! The pool owns a slab of [`Frame`]s, each `page_size` bytes and 64-byte
//! aligned so SIMD views over their contents are sound. A hash map
//! (`page_table`) maps `PageId -> FrameId` for O(1) lookup. The eviction
//! policy is LRU-2: a frame becomes evictable only after its *second* recent
//! access, which makes one-shot sequential scans unable to dislodge the
//! hot working set.
//!
//! ## Pin safety
//!
//! [`PinnedPage`] is an RAII guard: it holds a `&Frame` borrowed for `'pool`
//! and increments the frame's `pin_count` on construction, decrementing on
//! `Drop`. The borrow checker guarantees that any `ColumnView` derived from
//! a pinned page cannot outlive the pin — compile-time prevention of
//! use-after-evict.

pub mod frame;
pub mod lru;
pub mod pool;

#[cfg(feature = "mmap")]
pub mod mmap;

pub use cendb_core::FrameId;
pub use frame::Frame;
pub use lru::LruK;
pub use pool::{BufferPool, InMemoryPageSource, PageSource, PinnedPage, PoolStats, ReadHint};

#[cfg(feature = "mmap")]
pub use mmap::MmapPageSource;
