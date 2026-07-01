//! cendb-core: shared primitives for the CenDB engine.
//!
//! This crate holds the type aliases, identifier newtypes, error model, and
//! configuration record shared by every other CenDB crate. It deliberately has
//! no dependency on any I/O or storage backend so that it can be linked into
//! both the embedded engine and the FFI shim without pulling extra symbols.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(rust_2021_compatibility)]

use core::fmt;

// ============================================================================
// Identifier newtypes — these are the "coordinates" used by every layer of the
// engine. Newtypes prevent accidentally mixing a PageId with a BlockId.
// ============================================================================

/// Identifier of a segment file within a database. Segments are the unit of
/// append-mostly immutable storage (default 64 MiB).
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(transparent)]
pub struct SegmentId(pub u64);

/// Identifier of a block within a segment. Blocks are the PAX page unit
/// (default 256 KiB). The pair `(segment_id, block_id)` uniquely identifies a
/// block on disk.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(transparent)]
pub struct BlockId(pub u32);

/// Logical page identifier. The buffer pool keys frames by `PageId`. We pack
/// `(segment_id, block_id)` plus a sub-block page index into a single u64:
/// high 32 bits = segment_id, low 32 bits = (block_id << 16 | sub_index).
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(transparent)]
pub struct PageId(pub u64);

impl PageId {
    #[inline]
    pub fn pack(segment: SegmentId, block: BlockId, sub: u16) -> Self {
        let high = (segment.0 as u64) << 32;
        let low = ((block.0 as u64) << 16) | (sub as u64 & 0xFFFF);
        Self(high | low)
    }

    #[inline]
    pub fn segment(self) -> SegmentId {
        SegmentId((self.0 >> 32) & 0xFFFF_FFFF)
    }

    #[inline]
    pub fn block(self) -> BlockId {
        BlockId(((self.0 >> 16) & 0xFFFF_FFFF) as u32)
    }

    #[inline]
    pub fn sub_index(self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }
}

/// Index of a frame inside the BufferPool's slab. Repr is u32 because we cap
/// the pool at 2^32 frames in the extreme case.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(transparent)]
pub struct FrameId(pub u32);

/// Slot within a PAX block (row position). u32 because a 256 KiB block cannot
/// hold more than ~2^31 fixed-width rows even at 1 byte each.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(transparent)]
pub struct SlotId(pub u32);

/// Physical row locator returned by indexes (ART, etc.). Identifies a row by
/// its block + slot within the block. Tight 8-byte struct so it can sit inside
/// radix-tree leaves without indirection.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct RowLocator {
    pub segment: SegmentId,
    pub block: BlockId,
    pub slot: SlotId,
}

impl RowLocator {
    #[inline]
    pub const fn new(segment: SegmentId, block: BlockId, slot: SlotId) -> Self {
        Self { segment, block, slot }
    }
}

/// Node identifier in a graph projection.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(transparent)]
pub struct NodeId(pub u64);

// ============================================================================
// Sizing constants — centralised so every layer agrees on the geometry.
// ============================================================================

/// Default page size used by the buffer pool. Must be a power of two and a
/// multiple of 4096 (the OS page size on essentially every supported platform).
pub const DEFAULT_PAGE_SIZE: u32 = 4096;

/// Default PAX block size. A block holds many pages worth of columnar data;
/// 256 KiB is the canonical value used by the spec.
pub const DEFAULT_BLOCK_SIZE: u32 = 256 * 1024;

/// Default segment size (64 MiB). Segments are append-mostly and immutable
/// once sealed.
pub const DEFAULT_SEGMENT_SIZE: u64 = 64 * 1024 * 1024;

/// Required alignment for every minipage inside a PAX block. 64 bytes matches
/// both the common cache-line width and the AVX-512 register width, so SIMD
/// scans never straddle.
pub const MINIPAGE_ALIGN: usize = 64;

/// Magic bytes written at the head of every CenDB segment file. Note the
/// rename from HexaDB: we keep an 8-byte ASCII tag that is unambiguous on disk.
pub const SEGMENT_MAGIC: [u8; 8] = *b"CENDB001";

/// Format version written into the segment header.
pub const FORMAT_VERSION: u16 = 1;

// ============================================================================
// Error model — shared between Rust API and the FFI shim.
// ============================================================================

/// C-compatible status code returned by every `extern "C"` function. The values
/// are stable across releases: never renumber existing variants.
#[repr(i32)]
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum HexStatus {
    Ok = 0,
    ErrNotFound = 1,
    ErrConstraint = 2,
    ErrConflict = 3,
    ErrIo = 4,
    ErrCorrupt = 5,
    ErrSyntax = 6,
    ErrInternal = 99,
}

impl HexStatus {
    #[inline]
    pub fn is_ok(self) -> bool {
        matches!(self, HexStatus::Ok)
    }
}

/// Owned error type used inside Rust code. Cheap to construct (no allocation
/// for the common static-message case) and convertible to `(HexStatus, &str)`
/// at the FFI boundary.
#[derive(Debug)]
pub struct HexError {
    pub status: HexStatus,
    pub message: String,
}

impl HexError {
    #[inline]
    pub fn new(status: HexStatus, msg: impl Into<String>) -> Self {
        Self { status, message: msg.into() }
    }

    #[inline]
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(HexStatus::ErrNotFound, msg)
    }

    #[inline]
    pub fn io(msg: impl Into<String>) -> Self {
        Self::new(HexStatus::ErrIo, msg)
    }

    #[inline]
    pub fn corrupt(msg: impl Into<String>) -> Self {
        Self::new(HexStatus::ErrCorrupt, msg)
    }

    #[inline]
    pub fn constraint(msg: impl Into<String>) -> Self {
        Self::new(HexStatus::ErrConstraint, msg)
    }

    #[inline]
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(HexStatus::ErrInternal, msg)
    }
}

impl fmt::Display for HexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.status, self.message)
    }
}

impl std::error::Error for HexError {}

impl From<std::io::Error> for HexError {
    fn from(e: std::io::Error) -> Self {
        Self::new(HexStatus::ErrIo, e.to_string())
    }
}

pub type HexResult<T> = core::result::Result<T, HexError>;

// ============================================================================
// Configuration record. POD so it can be embedded in a C struct via the FFI
// layer (every field has a defined layout).
// ============================================================================

/// Runtime configuration for a CenDB instance. Plain-old-data so it can be
/// constructed from C via a parallel `HexConfig` struct.
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct CenDbConfig {
    /// Page size used by the buffer pool. Must be a power of two in
    /// `[4096, 65536]`.
    pub page_size: u32,
    /// Block size for PAX blocks. Must be a multiple of `page_size`.
    pub block_size: u32,
    /// Maximum number of frames the buffer pool may allocate. The pool will
    /// pin memory of `page_size * pool_frames` bytes; this is the hard cap.
    pub pool_frames: u32,
    /// If non-zero, enable WAL group-commit with this many milliseconds of
    /// latency window. 0 = synchronous (fsync per commit).
    pub group_commit_ms: u32,
    /// Bitfield of feature flags. Reserved for future use; currently ignored.
    pub flags: u64,
}

impl Default for CenDbConfig {
    fn default() -> Self {
        Self {
            page_size: DEFAULT_PAGE_SIZE,
            block_size: DEFAULT_BLOCK_SIZE,
            pool_frames: 1024,
            group_commit_ms: 10,
            flags: 0,
        }
    }
}

impl CenDbConfig {
    /// Sanity-check a config. We refuse obviously broken values up-front so
    /// the rest of the engine can rely on its invariants.
    pub fn validate(&self) -> HexResult<()> {
        if self.page_size < 4096 || !self.page_size.is_power_of_two() {
            return Err(HexError::constraint(format!(
                "page_size {} must be a power of two >= 4096",
                self.page_size
            )));
        }
        if self.block_size < self.page_size || self.block_size % self.page_size != 0 {
            return Err(HexError::constraint(format!(
                "block_size {} must be a multiple of page_size {}",
                self.block_size, self.page_size
            )));
        }
        if self.pool_frames == 0 {
            return Err(HexError::constraint("pool_frames must be > 0"));
        }
        Ok(())
    }
}

/// Logical data model tag attached to a table/collection. Mirrors the six
/// projections in the spec.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(u8)]
pub enum Model {
    Relational = 0,
    Columnar = 1,
    Document = 2,
    KeyValue = 3,
    TimeSeries = 4,
    Graph = 5,
}

/// Column value kinds that the storage layer understands natively. Anything
/// more exotic is encoded as bytes in the variable-length heap.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(u8)]
pub enum ValueKind {
    Null = 0,
    Bool = 1,
    I64 = 2,
    U64 = 3,
    F64 = 4,
    Bytes = 5,
    /// Timestamp stored as i64 nanos since UNIX_EPOCH.
    Timestamp = 6,
}

impl ValueKind {
    /// Convert a stored `u8` kind tag back to the enum. Unknown tags map to
    /// `Null` so a corrupt directory entry cannot cause UB.
    pub fn from_u8(b: u8) -> Self {
        match b {
            0 => ValueKind::Null,
            1 => ValueKind::Bool,
            2 => ValueKind::I64,
            3 => ValueKind::U64,
            4 => ValueKind::F64,
            5 => ValueKind::Bytes,
            6 => ValueKind::Timestamp,
            _ => ValueKind::Null,
        }
    }
}

impl ValueKind {
    /// Fixed byte width of values of this kind, or 0 if variable-length.
    #[inline]
    pub const fn fixed_width(self) -> usize {
        match self {
            ValueKind::Null => 0,
            ValueKind::Bool => 1,
            ValueKind::I64 | ValueKind::U64 | ValueKind::F64 | ValueKind::Timestamp => 8,
            ValueKind::Bytes => 0,
        }
    }
}

/// A scalar value with its kind tag. Used at the API boundary; the storage
/// layer never materialises a `Value` per row on the hot path.
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    Bytes(Vec<u8>),
    Timestamp(i64),
}

impl Value {
    #[inline]
    pub fn kind(&self) -> ValueKind {
        match self {
            Value::Null => ValueKind::Null,
            Value::Bool(_) => ValueKind::Bool,
            Value::I64(_) => ValueKind::I64,
            Value::U64(_) => ValueKind::U64,
            Value::F64(_) => ValueKind::F64,
            Value::Bytes(_) => ValueKind::Bytes,
            Value::Timestamp(_) => ValueKind::Timestamp,
        }
    }
}
