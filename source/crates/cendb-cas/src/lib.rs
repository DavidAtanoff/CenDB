//! cendb-cas: Content-Addressable Storage with deduplication.
//!
//! ## Overview
//!
//! CAS solves the problem of storing large binary files (4K images, video
//! frames, documents) without bloating the database's PAX pages. Files are
//! stored in a separate **blob store** keyed by their BLAKE3 hash, while
//! tables store only the 32-byte hash reference.
//!
//! ## Architecture
//!
//! ```text
//! User uploads Image (4K, 8MB)
//!       │
//!       ▼
//! ┌──────────────┐
//! │ BLAKE3 Hash  │ ──► Compute 32-byte hash
//! └──────────────┘
//!       │
//!       ├──────────────────────────────┐ (If hash already exists)
//!       ▼ (If hash is new)             ▼
//! ┌──────────────┐               ┌──────────────────────────────┐
//! │ Compress     │               │   Skip storage (deduplicate) │
//! │ (Zstd)       │               │   Increment refcount         │
//! └──────────────┘               └──────────────────────────────┘
//!       │                                      │
//!       ▼                                      ▼
//! ┌──────────────┐               ┌──────────────────────────────┐
//! │ Write to     │               │ Write ONLY the 32-byte hash  │
//! │ Blob Segment │               │ to the user's table          │
//! └──────────────┘               └──────────────────────────────┘
//! ```
//!
//! ## Components
//!
//!   * [`BlobStore`] — owns the on-disk blob files and the in-memory hash
//!     → location index (backed by ART).
//!   * [`Hash`] — a 32-byte BLAKE3 digest, stored as `[u8; 32]`.
//!   * Reference counting — each blob tracks how many tables reference it;
//!     when the count hits zero, the blob is garbage-collected.
//!   * Optional zstd compression — blobs are compressed on write and
//!     decompressed on read.

pub mod blob;
pub mod hash;

pub use blob::{BlobId, BlobStore, BlobStoreStats, CompressionKind};
pub use hash::Hash;
