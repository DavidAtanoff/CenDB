//! On-disk headers and directory structures.
//!
//! Every struct here is `#[repr(C)]` and `Pod`, so it can be written to disk
//! with a single `write_all(bytemuck::bytes_of(&hdr))` and read back with
//! `bytemuck::from_bytes`. The wire format is therefore stable across
//! architectures (assuming little-endian, which the spec targets).

use cendb_core::{SegmentId, FORMAT_VERSION, SEGMENT_MAGIC, MINIPAGE_ALIGN};
use bytemuck::{Pod, Zeroable};

use crate::encoding::Encoding;
use crate::zerocopy::align_up;

// ============================================================================
// SegmentHeader — 64 bytes, written once at segment creation.
// ============================================================================

/// Header of a CenDB segment file. Always 64 bytes — exactly one cache line —
/// so it can be parsed with a single load.
///
/// The layout matches the spec verbatim with the renamed magic bytes
/// `b"CENDB001"`. Fields are arranged in descending alignment order so the
/// struct has no internal padding (required by `bytemuck::Pod`).
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
#[repr(C)]
pub struct SegmentHeader {
    /// Magic bytes; must equal [`SEGMENT_MAGIC`] (`b"CENDB001"`).
    pub magic: [u8; 8],
    /// Unique segment identifier.
    pub segment_id: u64,
    /// WAL LSN at the moment of segment creation.
    pub created_lsn: u64,
    /// WAL LSN at the moment the segment was sealed; 0 while mutable.
    pub sealed_lsn: u64,
    /// Byte offset of the `BlockDirectory` within the segment file. Written at
    /// seal time; 0 while the segment is still being appended to.
    pub block_dir_off: u64,
    /// xxh3-style checksum of the preceding bytes of this header. Storage
    /// layers must verify this on load and refuse a segment whose header
    /// checksum fails.
    pub checksum: u64,
    /// Page size used by the buffer pool (4096..=65536, power of two).
    pub page_size: u32,
    /// Block size for PAX blocks (multiple of `page_size`).
    pub block_size: u32,
    /// Number of blocks currently written into the segment.
    pub block_count: u32,
    /// Format version; must equal [`FORMAT_VERSION`].
    pub format_ver: u16,
    /// Bitfield: bit 0 = sealed, bit 1 = encrypted, bit 2 = page-checksums-on.
    pub flags: u16,
}

impl SegmentHeader {
    /// Bit positions inside `flags`.
    pub const FLAG_SEALED: u16 = 1 << 0;
    pub const FLAG_ENCRYPTED: u16 = 1 << 1;
    pub const FLAG_CHECKSUMS: u16 = 1 << 2;

    /// Construct a fresh header for a new segment.
    pub fn new(segment_id: SegmentId, page_size: u32, block_size: u32, created_lsn: u64) -> Self {
        Self {
            magic: SEGMENT_MAGIC,
            segment_id: segment_id.0,
            created_lsn,
            sealed_lsn: 0,
            block_dir_off: 0,
            checksum: 0,
            page_size,
            block_size,
            block_count: 0,
            format_ver: FORMAT_VERSION,
            flags: 0,
        }
    }

    #[inline]
    pub fn is_sealed(&self) -> bool {
        self.flags & Self::FLAG_SEALED != 0
    }

    /// Validate the magic + version + checksum-feasibility of a header read
    /// from disk. Does not re-verify the checksum (we keep that as a separate
    /// step so callers can choose whether to enforce it).
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.magic != SEGMENT_MAGIC {
            return Err("segment magic mismatch — not a CenDB file");
        }
        if self.format_ver != FORMAT_VERSION {
            return Err("unsupported segment format version");
        }
        if self.page_size < 4096 || !self.page_size.is_power_of_two() {
            return Err("invalid page_size in segment header");
        }
        if self.block_size < self.page_size || self.block_size % self.page_size != 0 {
            return Err("invalid block_size in segment header");
        }
        Ok(())
    }
}

// ============================================================================
// BlockHeader — 64 bytes, prefix of every PAX block.
// ============================================================================

/// Header of a single PAX block. Lives at the very start of the block buffer
/// and is exactly 64 bytes — one cache line — so the zone map can be checked
/// without pulling the rest of the block into cache.
///
/// Fields are arranged in descending alignment order so the struct has no
/// internal padding (required by `bytemuck::Pod`).
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
#[repr(C)]
pub struct BlockHeader {
    /// Zone map: minimum partitioning-key value (i64). For KV this is the
    /// hash of the smallest key; for time-series it is `min(ts)`.
    pub min_pk: i64,
    /// Zone map: maximum partitioning-key value.
    pub max_pk: i64,
    /// Zone map: minimum timestamp in this block (nanos since UNIX_EPOCH).
    /// 0 if the block has no timestamp column.
    pub min_ts: i64,
    /// Zone map: maximum timestamp in this block.
    pub max_ts: i64,
    /// Byte offset (relative to block start) of the tombstone bitmap. The
    /// bitmap grows downward from the end of the block; this points at its
    /// current base. 0 if no tombstones.
    pub tombstone_bitmap_off: u32,
    /// Byte offset of the null bitmap (one bit per row per column, packed).
    pub null_bitmap_off: u32,
    /// Byte offset of the first minipage (immediately after the column
    /// directory). Stored explicitly so the reader can locate minipages
    /// without re-deriving alignment.
    pub minipages_off: u32,
    /// Byte offset where the variable-length heap begins.
    pub varheap_off: u32,
    /// Current size of the var-heap (bytes). Used to allocate new (offset, len)
    /// slots without re-scanning.
    pub varheap_len: u32,
    /// Number of rows currently stored in this block.
    pub row_count: u32,
    /// Number of columns (minipages) in this block.
    pub column_count: u32,
    /// Block-level flags: bit 0 = sealed (no more appends), bit 1 = sorted.
    pub flags: u32,
}

impl BlockHeader {
    pub const FLAG_SEALED: u32 = 1 << 0;
    pub const FLAG_SORTED: u32 = 1 << 1;

    /// Header for a freshly initialised block; everything zeroed except the
    /// zone map sentinels which we set so that `min > max` indicates "empty".
    pub const fn empty() -> Self {
        Self {
            min_pk: i64::MAX,
            max_pk: i64::MIN,
            min_ts: i64::MAX,
            max_ts: i64::MIN,
            tombstone_bitmap_off: 0,
            null_bitmap_off: 0,
            minipages_off: 0,
            varheap_off: 0,
            varheap_len: 0,
            row_count: 0,
            column_count: 0,
            flags: 0,
        }
    }

    #[inline]
    pub fn is_sealed(&self) -> bool {
        self.flags & Self::FLAG_SEALED != 0
    }

    /// True iff the block's zone map *could* contain a row whose partitioning
    /// key lies in `[lo, hi]`. Used for predicate pushdown.
    #[inline]
    pub fn zone_overlaps_pk(&self, lo: i64, hi: i64) -> bool {
        !(self.max_pk < lo || self.min_pk > hi)
    }

    /// True iff the block's zone map *could* contain a row whose timestamp
    /// lies in `[lo, hi]`. Returns true if the block has no timestamp column
    /// (`min_ts > max_ts`) — predicate pushdown cannot exclude such blocks.
    #[inline]
    pub fn zone_overlaps_ts(&self, lo: i64, hi: i64) -> bool {
        if self.min_ts > self.max_ts {
            return true;
        }
        !(self.max_ts < lo || self.min_ts > hi)
    }
}

/// Compute the size of a `BlockHeader` in bytes. Always 64.
#[inline]
pub const fn block_header_size() -> usize {
    core::mem::size_of::<BlockHeader>()
}

// ============================================================================
// ColumnDirectory — per-column metadata, one entry per column.
// ============================================================================

/// Specification of a column as declared by the schema.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ColumnSpec {
    /// Numeric column id; stable across the lifetime of the table.
    pub col_id: u32,
    /// Value kind (I64, F64, Bytes, ...).
    pub kind: cendb_core::ValueKind,
    /// Requested encoding (Raw, BitPacked, ...). The storage layer may
    /// override at seal time if a better encoding is detected.
    pub encoding: Encoding,
    /// Optional: 1 if this column is the partitioning key (used for the
    /// `min_pk`/`max_pk` zone map). 0 otherwise.
    pub is_pk: u8,
    /// Optional: 1 if this column is the timestamp (used for `min_ts`/`max_ts`).
    pub is_ts: u8,
}

impl ColumnSpec {
    pub fn new(col_id: u32, kind: cendb_core::ValueKind) -> Self {
        Self {
            col_id,
            kind,
            encoding: Encoding::Raw,
            is_pk: 0,
            is_ts: 0,
        }
    }

    pub fn with_encoding(mut self, enc: Encoding) -> Self {
        self.encoding = enc;
        self
    }

    pub fn pk(mut self) -> Self {
        self.is_pk = 1;
        self
    }

    pub fn ts(mut self) -> Self {
        self.is_ts = 1;
        self
    }

    /// Fixed width of each slot in this column's minipage. For variable-length
    /// columns (`Bytes`) the slot holds an `(offset, len)` pair → 8 bytes.
    #[inline]
    pub const fn slot_width(&self) -> usize {
        if self.kind.fixed_width() > 0 {
            self.kind.fixed_width()
        } else {
            // (offset: u32, len: u32) into the var-heap
            8
        }
    }
}

/// On-disk entry of the column directory. The directory is an array of these
/// structs laid out contiguously immediately after the `BlockHeader`.
///
/// Size: 64 bytes (one cache line) so a single load fetches a column's
/// complete metadata including zone min/max.
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
#[repr(C)]
pub struct ColumnDirectory {
    /// Column id (matches `ColumnSpec::col_id`).
    pub col_id: u32,
    /// Value kind (mirrors `cendb_core::ValueKind`).
    pub kind: u8,
    /// Encoding tag (mirrors `Encoding` discriminant).
    pub encoding: u8,
    /// `is_pk` + `is_ts` flags packed into a single byte.
    pub flags: u8,
    /// Reserved for alignment.
    pub _reserved: u8,
    /// Byte offset of this column's minipage, relative to block start.
    /// 64-byte aligned.
    pub minipage_off: u32,
    /// Logical length of the minipage in bytes (slot_width * row_count).
    pub minipage_len: u32,
    /// Compressed length (<= minipage_len). Equal to `minipage_len` if Raw.
    pub compressed_len: u32,
    /// Number of values currently stored (== block row_count for fixed-width
    /// columns).
    pub value_count: u32,
    /// Zone map: minimum value in this column (i64 cast for storage).
    pub zone_min: i64,
    /// Zone map: maximum value in this column.
    pub zone_max: i64,
    /// Per-column padding so the struct is exactly 64 bytes.
    pub _pad: [u8; 24],
}

impl ColumnDirectory {
    pub const FLAG_IS_PK: u8 = 1 << 0;
    pub const FLAG_IS_TS: u8 = 1 << 1;

    pub fn from_spec(spec: &ColumnSpec, row_count: u32, minipage_off: u32) -> Self {
        let mut flags = 0u8;
        if spec.is_pk != 0 {
            flags |= Self::FLAG_IS_PK;
        }
        if spec.is_ts != 0 {
            flags |= Self::FLAG_IS_TS;
        }
        let slot_w = spec.slot_width() as u32;
        Self {
            col_id: spec.col_id,
            kind: spec.kind as u8,
            encoding: spec.encoding.discriminant(),
            flags,
            _reserved: 0,
            minipage_off,
            minipage_len: slot_w * row_count,
            compressed_len: slot_w * row_count,
            value_count: row_count,
            zone_min: i64::MAX,
            zone_max: i64::MIN,
            _pad: [0; 24],
        }
    }

    #[inline]
    pub fn is_pk(&self) -> bool {
        self.flags & Self::FLAG_IS_PK != 0
    }

    #[inline]
    pub fn is_ts(&self) -> bool {
        self.flags & Self::FLAG_IS_TS != 0
    }

    /// True iff this column's zone map *could* contain a value in `[lo, hi]`.
    /// Used for predicate pushdown on a per-minipage basis.
    #[inline]
    pub fn zone_overlaps(&self, lo: i64, hi: i64) -> bool {
        if self.zone_min > self.zone_max {
            return true; // no zone info for this column
        }
        !(self.zone_max < lo || self.zone_min > hi)
    }
}

/// Compute the byte offset at which the column directory ends and minipages
/// may begin (aligned up to `MINIPAGE_ALIGN`).
#[inline]
pub fn minipages_offset_after_directory(column_count: usize) -> usize {
    let after_hdr = core::mem::size_of::<BlockHeader>();
    let dir_bytes = column_count * core::mem::size_of::<ColumnDirectory>();
    align_up(after_hdr + dir_bytes, MINIPAGE_ALIGN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_sizes_match_spec() {
        assert_eq!(core::mem::size_of::<SegmentHeader>(), 64);
        assert_eq!(core::mem::size_of::<BlockHeader>(), 64);
        assert_eq!(core::mem::size_of::<ColumnDirectory>(), 64);
    }

    #[test]
    fn zone_overlaps_pk_basic() {
        let mut h = BlockHeader::empty();
        h.min_pk = 10;
        h.max_pk = 20;
        assert!(h.zone_overlaps_pk(5, 15));
        assert!(h.zone_overlaps_pk(15, 25));
        assert!(h.zone_overlaps_pk(10, 20));
        assert!(!h.zone_overlaps_pk(0, 9));
        assert!(!h.zone_overlaps_pk(21, 30));
    }

    #[test]
    fn zone_overlaps_ts_handles_missing_ts() {
        let h = BlockHeader::empty();
        // min_ts > max_ts (i64::MAX > i64::MIN) means "no ts column"
        assert!(h.zone_overlaps_ts(0, 0));
    }

    #[test]
    fn column_directory_layout() {
        let spec = ColumnSpec::new(7, cendb_core::ValueKind::I64).pk();
        let dir = ColumnDirectory::from_spec(&spec, 100, 128);
        assert_eq!(dir.col_id, 7);
        assert_eq!(dir.kind, cendb_core::ValueKind::I64 as u8);
        assert!(dir.is_pk());
        assert!(!dir.is_ts());
        assert_eq!(dir.minipage_off, 128);
        assert_eq!(dir.minipage_len, 8 * 100);
        assert_eq!(dir.value_count, 100);
    }

    #[test]
    fn minipages_offset_aligned() {
        let off = minipages_offset_after_directory(4);
        assert_eq!(off % MINIPAGE_ALIGN, 0);
        // header (64) + dir (4*64=256) = 320; already 64-aligned, stays 320.
        assert_eq!(off, 320);
    }
}
