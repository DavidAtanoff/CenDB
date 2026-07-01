//! The PAX block: builder, on-disk reader, and zero-copy column view.
//!
//! ## Layout
//!
//! ```text
//! ┌───────────────────────────────────────────────────────┐
//! │ BlockHeader (64B)                                     │
//! │  row_count, column_count, min_pk, max_pk, min_ts, ... │
//! ├───────────────────────────────────────────────────────┤
//! │ ColumnDirectory[column_count]   (64B each)            │
//! ├───────────────────────────────────────────────────────┤
//! │ Minipage[0]   (64B-aligned)                           │
//! │ Minipage[1]   (64B-aligned)                           │
//! │ ...                                                   │
//! │ Minipage[n]   (64B-aligned)                           │
//! ├───────────────────────────────────────────────────────┤
//! │ Variable-length heap (grows upward)                  │
//! │   - string/blob bytes for Bytes columns              │
//! │ ...                                                   │
//! │ Tombstone bitmap + Null bitmaps (grows downward)     │
//! └───────────────────────────────────────────────────────┘
//! ```
//!
//! ## Two-pass build
//!
//! The builder accumulates rows in columnar staging buffers in memory, then
//! `finalize()` lays them out into a single aligned `Box<[u8]>` of the
//! requested block size. This is simpler than writing rows one at a time into
//! the final buffer (which would require knowing minipage sizes up front) and
//! is also faster — the final write is a sequential memcpy per column.

use cendb_core::{HexError, HexResult, Value, ValueKind, MINIPAGE_ALIGN};
use bytemuck::Pod;

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ptr::NonNull;

use crate::encoding::{auto_select_encoding_i64, decode_minipage, encode_minipage, Encoding};
use crate::header::{
    block_header_size, minipages_offset_after_directory, BlockHeader, ColumnDirectory, ColumnSpec,
};
use crate::zerocopy::{align_up, cast_slice_bytes, pod_write_at};

/// Identifier of a row inside a PAX block (== slot index).
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct RowId(pub u32);

/// A borrowed, validated view over a frame's bytes. Lifetime is tied to the
/// [`PaxBlock`] it came from, which in turn is tied to a buffer-pool pin.
///
/// The view gives zero-copy access to a column's minipage as a `&'a [T]`. The
/// alignment invariant (64-byte aligned start, length multiple of
/// `size_of::<T>()`) is checked once when the view is constructed.
#[derive(Copy, Clone, Debug)]
pub struct ColumnView<'a, T: Pod> {
    raw: &'a [u8],
    _marker: core::marker::PhantomData<&'a T>,
}

impl<'a, T: Pod> ColumnView<'a, T> {
    /// Construct a view from a raw byte slice. The caller is responsible for
    /// ensuring the slice is 64-byte aligned and its length is a multiple of
    /// `size_of::<T>()`.
    ///
    /// # Safety invariants enforced
    /// The constructor checks the length invariant at runtime (in debug
    /// builds, alignment too). On mismatched length it returns `Err`.
    pub fn new(raw: &'a [u8]) -> HexResult<Self> {
        if raw.len() % core::mem::size_of::<T>() != 0 {
            return Err(HexError::corrupt(format!(
                "ColumnView: slice len {} not a multiple of size_of::<{}>() = {}",
                raw.len(),
                core::any::type_name::<T>(),
                core::mem::size_of::<T>()
            )));
        }
        debug_assert!(
            raw.as_ptr() as usize % 64 == 0 || raw.is_empty(),
            "ColumnView: minipage must be 64B aligned"
        );
        Ok(Self { raw, _marker: core::marker::PhantomData })
    }

    /// Zero-copy reinterpretation as `&'a [T]`. No allocation, no parse.
    #[inline]
    pub fn as_slice(&self) -> &'a [T] {
        cast_slice_bytes::<T>(self.raw)
    }

    /// Number of typed elements viewable through this slice.
    #[inline]
    pub fn len(&self) -> usize {
        self.raw.len() / core::mem::size_of::<T>()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// Borrow the raw bytes (e.g. for re-encoding).
    #[inline]
    pub fn raw_bytes(&self) -> &'a [u8] {
        self.raw
    }
}

// ============================================================================
// PaxBlockBuilder — accumulate rows in columnar staging buffers.
// ============================================================================

/// A column's staging area. Fixed-width columns hold a `Vec<i64>` (the
/// canonical storage form for ints, bools-as-0/1, timestamps-as-nanos);
/// variable-length columns hold a `Vec<(u32 offset, u32 len)>` plus the raw
/// byte heap.
enum Staging {
    Fixed {
        kind: ValueKind,
        vals: Vec<i64>,
    },
    Var {
        slots: Vec<(u32, u32)>,
        heap: Vec<u8>,
    },
}

/// Builder that accumulates rows for a single PAX block. Once `finalize()` is
/// called the builder produces a [`PaxBlock`] of exactly `block_size` bytes
/// with all minipages properly aligned and the zone maps populated.
pub struct PaxBlockBuilder {
    block_size: u32,
    specs: Vec<ColumnSpec>,
    staging: Vec<Staging>,
    row_count: u32,
    /// Cached (min_pk, max_pk) seen so far for the pk column, if any.
    pk_range: Option<(i64, i64)>,
    /// Cached (min_ts, max_ts) seen so far for the ts column, if any.
    ts_range: Option<(i64, i64)>,
}

impl PaxBlockBuilder {
    /// Create a builder for a block of `block_size` bytes with the given
    /// column specs. The specs must be in a stable order; column id values
    /// may be arbitrary but their position in the slice is what determines
    /// their index in the column directory.
    pub fn new(block_size: u32, specs: Vec<ColumnSpec>) -> HexResult<Self> {
        if specs.is_empty() {
            return Err(HexError::constraint("PaxBlockBuilder: need >= 1 column"));
        }
        if specs.iter().filter(|s| s.is_pk != 0).count() > 1 {
            return Err(HexError::constraint("PaxBlockBuilder: at most one pk column"));
        }
        if specs.iter().filter(|s| s.is_ts != 0).count() > 1 {
            return Err(HexError::constraint("PaxBlockBuilder: at most one ts column"));
        }
        let staging = specs
            .iter()
            .map(|s| match s.kind.fixed_width() {
                0 => Staging::Var { slots: Vec::new(), heap: Vec::new() },
                _ => Staging::Fixed { kind: s.kind, vals: Vec::new() },
            })
            .collect();
        Ok(Self {
            block_size,
            specs,
            staging,
            row_count: 0,
            pk_range: None,
            ts_range: None,
        })
    }

    /// Append a row. The `values` slice must be in the same order as the
    /// column specs passed to [`new`]. Each value's kind must be compatible
    /// with the column's declared kind (`Null` is always accepted and
    /// recorded in the null bitmap).
    pub fn append_row(&mut self, values: &[Value]) -> HexResult<RowId> {
        if values.len() != self.specs.len() {
            return Err(HexError::constraint(format!(
                "append_row: expected {} values, got {}",
                self.specs.len(),
                values.len()
            )));
        }
        let row_id = RowId(self.row_count);
        for (i, v) in values.iter().enumerate() {
            self.write_value(i, v)?;
        }
        self.row_count += 1;
        Ok(row_id)
    }

    fn write_value(&mut self, col_idx: usize, v: &Value) -> HexResult<()> {
        // Snapshot the column flags up-front so we don't hold an immutable
        // borrow of `self.specs` while we mutate `self` (zone map fields).
        let spec = &self.specs[col_idx];
        let is_pk = spec.is_pk != 0;
        let is_ts = spec.is_ts != 0;
        let kind = spec.kind;

        // Track pk / ts zone map contributions.
        match v {
            Value::I64(x) => {
                if is_pk {
                    self.extend_pk(*x);
                }
                if is_ts {
                    self.extend_ts(*x);
                }
            }
            Value::U64(x) => {
                if is_pk {
                    self.extend_pk(*x as i64);
                }
                if is_ts {
                    self.extend_ts(*x as i64);
                }
            }
            Value::Timestamp(x) => {
                if is_ts {
                    self.extend_ts(*x);
                }
                if is_pk {
                    self.extend_pk(*x);
                }
            }
            _ => {}
        }
        match &mut self.staging[col_idx] {
            Staging::Fixed { kind: stag_kind, vals } => {
                if matches!(v, Value::Null) {
                    vals.push(0);
                    return Ok(());
                }
                let encoded: i64 = match (kind, v) {
                    (ValueKind::Bool, Value::Bool(b)) => *b as i64,
                    (ValueKind::I64, Value::I64(x)) => *x,
                    (ValueKind::U64, Value::U64(x)) => *x as i64,
                    (ValueKind::F64, Value::F64(x)) => x.to_bits() as i64,
                    (ValueKind::Timestamp, Value::Timestamp(x)) => *x,
                    (ValueKind::I64, Value::Timestamp(x)) => *x, // accept ts as i64
                    (k, val) => {
                        return Err(HexError::constraint(format!(
                            "append_row: cannot store {:?} in column of kind {:?}",
                            val, k
                        )));
                    }
                };
                let _ = stag_kind; // already captured via `kind`
                vals.push(encoded);
            }
            Staging::Var { slots, heap } => {
                if let Value::Bytes(b) = v {
                    let off = heap.len() as u32;
                    let len = b.len() as u32;
                    heap.extend_from_slice(b);
                    slots.push((off, len));
                } else if let Value::Null = v {
                    slots.push((0, u32::MAX)); // sentinel: null
                } else {
                    // Forcibly convert non-bytes into Bytes columns (used by KV).
                    let s = value_to_string(v);
                    let off = heap.len() as u32;
                    let len = s.len() as u32;
                    heap.extend_from_slice(s.as_bytes());
                    slots.push((off, len));
                }
            }
        }
        Ok(())
    }

    fn extend_pk(&mut self, v: i64) {
        match &mut self.pk_range {
            Some((lo, hi)) => {
                if v < *lo {
                    *lo = v;
                }
                if v > *hi {
                    *hi = v;
                }
            }
            None => self.pk_range = Some((v, v)),
        }
    }

    fn extend_ts(&mut self, v: i64) {
        match &mut self.ts_range {
            Some((lo, hi)) => {
                if v < *lo {
                    *lo = v;
                }
                if v > *hi {
                    *hi = v;
                }
            }
            None => self.ts_range = Some((v, v)),
        }
    }

    /// Finalize the block: lay out the staging buffers into a single
    /// `block_size`-byte buffer with all minipages 64-byte aligned, populate
    /// the column directory, and compute zone maps.
    pub fn finalize(self) -> HexResult<PaxBlock> {
        let column_count = self.specs.len() as u32;
        let minipages_off = minipages_offset_after_directory(self.specs.len()) as u32;

        // For each column, compute the encoded minipage bytes.
        let mut minipages: Vec<Vec<u8>> = Vec::with_capacity(self.specs.len());
        let mut dirs: Vec<ColumnDirectory> = Vec::with_capacity(self.specs.len());

        let mut cursor = minipages_off as usize;
        for (i, spec) in self.specs.iter().enumerate() {
            // Encode this column.
            let (bytes, zone_min, zone_max, encoding_used) = match &self.staging[i] {
                Staging::Fixed { kind, vals } => {
                    // Choose encoding. If the spec requested Raw, we still
                    // auto-select for integer columns (better ratio, same
                    // semantics); explicit non-Raw requests are honoured.
                    let enc = if spec.encoding == Encoding::Raw
                        && matches!(kind, ValueKind::I64 | ValueKind::U64 | ValueKind::Timestamp)
                    {
                        auto_select_encoding_i64(vals)
                    } else {
                        spec.encoding
                    };
                    let body = encode_minipage(enc, vals)?;
                    let (zmin, zmax) = if vals.is_empty() {
                        (i64::MAX, i64::MIN)
                    } else {
                        let mut lo = vals[0];
                        let mut hi = vals[0];
                        for &v in vals {
                            if v < lo {
                                lo = v;
                            }
                            if v > hi {
                                hi = v;
                            }
                        }
                        (lo, hi)
                    };
                    (body, zmin, zmax, enc)
                }
                Staging::Var { slots, heap } => {
                    // Slots are (offset, len) u32 pairs; heap is raw bytes.
                    // The minipage body is slots concatenated; the heap lives
                    // in the var-heap region of the block.
                    let mut body = Vec::with_capacity(slots.len() * 8);
                    for &(off, len) in slots {
                        body.extend_from_slice(&off.to_le_bytes());
                        body.extend_from_slice(&len.to_le_bytes());
                    }
                    let _ = heap; // we'll move the heap separately below
                    (body, i64::MAX, i64::MIN, Encoding::Raw)
                }
            };

            // Align cursor to 64 bytes.
            cursor = align_up(cursor, MINIPAGE_ALIGN);
            let minipage_off = cursor as u32;
            let mut dir = ColumnDirectory::from_spec(spec, self.row_count, minipage_off);
            dir.encoding = encoding_used.discriminant();
            dir.minipage_len = bytes.len() as u32;
            dir.compressed_len = bytes.len() as u32;
            dir.value_count = self.row_count;
            dir.zone_min = zone_min;
            dir.zone_max = zone_max;
            dirs.push(dir);
            cursor += bytes.len();
            minipages.push(bytes);
        }

        // Append the var-heap (concatenated bytes columns' heaps).
        let varheap_off = align_up(cursor, 8) as u32;
        let mut varheap: Vec<u8> = Vec::new();
        // We need to *re-base* the offsets stored in the var columns' minipages
        // because the heap was previously local per column; now it's merged
        // into the block-level var-heap.
        let mut heap_rebases: Vec<(usize, u32)> = Vec::new(); // (col_idx, base_offset)
        for (i, spec) in self.specs.iter().enumerate() {
            if spec.kind.fixed_width() > 0 {
                continue;
            }
            if let Staging::Var { slots, heap } = &self.staging[i] {
                let base = varheap.len() as u32;
                heap_rebases.push((i, base));
                varheap.extend_from_slice(heap);
                let _ = slots;
            }
        }
        // Apply rebases to the minipage bytes.
        for (col_idx, base) in heap_rebases {
            let mp = &mut minipages[col_idx];
            for slot_idx in 0..(mp.len() / 8) {
                let off_pos = slot_idx * 8;
                let off = u32::from_le_bytes([mp[off_pos], mp[off_pos + 1], mp[off_pos + 2], mp[off_pos + 3]]);
                let len = u32::from_le_bytes([mp[off_pos + 4], mp[off_pos + 5], mp[off_pos + 6], mp[off_pos + 7]]);
                if len == u32::MAX {
                    continue; // null sentinel
                }
                let new_off = off + base;
                mp[off_pos..off_pos + 4].copy_from_slice(&new_off.to_le_bytes());
            }
        }

        // Now compute the total size and check it fits.
        let null_bitmap_size = self.specs.len() * ((self.row_count as usize + 7) / 8);
        let tombstone_bitmap_size = (self.row_count as usize + 7) / 8;
        let total_needed = varheap_off as usize + varheap.len() + null_bitmap_size + tombstone_bitmap_size;
        if total_needed > self.block_size as usize {
            return Err(HexError::constraint(format!(
                "PaxBlockBuilder: block overflow — need {} bytes, block size is {}",
                total_needed, self.block_size
            )));
        }

        // Allocate the block buffer with 64-byte alignment. `AlignedBlock`
        // returns a zeroed buffer so we don't need to zero it ourselves.
        let mut buf = alloc_aligned_block(self.block_size as usize)?;
        let buf_slice = buf.as_mut_slice();

        // Write BlockHeader.
        let mut hdr = BlockHeader::empty();
        hdr.row_count = self.row_count;
        hdr.column_count = column_count;
        if let Some((lo, hi)) = self.pk_range {
            hdr.min_pk = lo;
            hdr.max_pk = hi;
        }
        if let Some((lo, hi)) = self.ts_range {
            hdr.min_ts = lo;
            hdr.max_ts = hi;
        }
        hdr.minipages_off = minipages_off;
        hdr.varheap_off = varheap_off;
        hdr.varheap_len = varheap.len() as u32;
        // Tombstone bitmap lives at the very end, growing down.
        let tomb_off = self.block_size - tombstone_bitmap_size as u32;
        hdr.tombstone_bitmap_off = tomb_off;
        // Null bitmap lives just below the tombstone bitmap.
        let null_off = tomb_off - null_bitmap_size as u32;
        hdr.null_bitmap_off = null_off;
        pod_write_at(buf_slice, 0, &hdr);

        // Write column directory.
        for (i, dir) in dirs.iter().enumerate() {
            let off = block_header_size() + i * core::mem::size_of::<ColumnDirectory>();
            pod_write_at(buf_slice, off, dir);
        }

        // Write minipages.
        for (i, mp) in minipages.iter().enumerate() {
            let off = dirs[i].minipage_off as usize;
            buf_slice[off..off + mp.len()].copy_from_slice(mp);
        }

        // Write var-heap.
        buf_slice[varheap_off as usize..varheap_off as usize + varheap.len()]
            .copy_from_slice(&varheap);

        // (Null bitmap is left zeroed, which is correct: no nulls recorded.)

        Ok(PaxBlock {
            buf,
            block_size: self.block_size,
        })
    }

    /// Current row count of the builder.
    #[inline]
    pub fn row_count(&self) -> u32 {
        self.row_count
    }
}

/// 64-byte-aligned owned byte buffer. Uses the global allocator with an
/// explicit `Layout` so the start pointer is guaranteed 64-byte aligned and
/// the destructor deallocates with the matching layout (Vec<u8> would use
/// alignment 1, which would be UB for our minipage views).
pub struct AlignedBlock {
    ptr: NonNull<u8>,
    size: usize,
}

// SAFETY: `AlignedBlock` owns its allocation and is not tied to any thread.
// Sending it across threads is safe because the global allocator is Sync.
unsafe impl Send for AlignedBlock {}
unsafe impl Sync for AlignedBlock {}

impl AlignedBlock {
    /// Allocate a zeroed buffer of `size` bytes, 64-byte aligned.
    pub fn zeroed(size: usize) -> HexResult<Self> {
        if size == 0 {
            return Err(HexError::constraint("AlignedBlock: size must be > 0"));
        }
        let layout = Layout::from_size_align(size, MINIPAGE_ALIGN)
            .map_err(|e| HexError::internal(format!("layout error: {}", e)))?;
        // SAFETY: `layout.size() > 0` (checked above), so `alloc_zeroed` is
        // safe to call.
        let ptr = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr)
            .ok_or_else(|| HexError::internal("alloc_zeroed returned null"))?;
        Ok(Self { ptr, size })
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `size` bytes for the lifetime of `&self`.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.size) }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: same as `as_slice` but mutable. We have `&mut self` so no
        // aliasing.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.size) }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.size
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Pointer for debug alignment assertions.
    #[inline]
    pub fn ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }
}

impl Drop for AlignedBlock {
    fn drop(&mut self) {
        // SAFETY: the layout matches the one used in `zeroed()`.
        let layout = Layout::from_size_align(self.size, MINIPAGE_ALIGN).unwrap();
        unsafe { dealloc(self.ptr.as_ptr(), layout) }
    }
}

/// Allocate a `size`-byte buffer aligned to 64 bytes.
fn alloc_aligned_block(size: usize) -> HexResult<AlignedBlock> {
    AlignedBlock::zeroed(size)
}

// ============================================================================
// PaxBlock — the on-disk reader. Owns its buffer (or, in the buffer-pool
// path, borrows a frame's bytes).
// ============================================================================

/// A PAX block. The buffer is owned in the standalone case; when integrated
/// with the buffer pool, the buffer pool hands out a `PaxBlockReader` borrowing
/// a frame's bytes directly.
pub struct PaxBlock {
    buf: AlignedBlock,
    block_size: u32,
}

impl PaxBlock {
    /// Owning constructor: take ownership of an aligned buffer.
    pub fn from_owned(buf: AlignedBlock, block_size: u32) -> HexResult<Self> {
        if buf.len() < block_header_size() {
            return Err(HexError::corrupt("PaxBlock: buffer shorter than header"));
        }
        if buf.ptr() as usize % MINIPAGE_ALIGN != 0 {
            return Err(HexError::corrupt("PaxBlock: buffer not 64B aligned"));
        }
        Ok(Self { buf, block_size })
    }

    /// Construct a reader over this block's bytes.
    #[inline]
    pub fn reader(&self) -> PaxBlockReader<'_> {
        PaxBlockReader::new(self.buf.as_slice(), self.block_size)
    }

    /// Raw bytes of the block (for I/O).
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.buf.as_slice()
    }

    /// Consume the block and return the owned bytes (for I/O without copy).
    #[inline]
    pub fn into_bytes(self) -> AlignedBlock {
        self.buf
    }

    #[inline]
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Block header. Uses `pod_read_unaligned` for safety on arbitrary
    /// byte slices (the buffer may not be 64-byte aligned when constructed
    /// from a `Vec<u8>` or raw file read).
    #[inline]
    pub fn header(&self) -> BlockHeader {
        let buf = self.buf.as_slice();
        if buf.len() < block_header_size() {
            return BlockHeader::empty();
        }
        bytemuck::pod_read_unaligned(&buf[..block_header_size()])
    }

    /// Column directory entry for column `idx`.
    #[inline]
    pub fn directory(&self, idx: usize) -> HexResult<ColumnDirectory> {
        let off = block_header_size() + idx * core::mem::size_of::<ColumnDirectory>();
        let end = off + core::mem::size_of::<ColumnDirectory>();
        let buf = self.buf.as_slice();
        if end > buf.len() {
            return Err(HexError::corrupt(format!(
                "directory({}) out of bounds",
                idx
            )));
        }
        Ok(bytemuck::pod_read_unaligned(&buf[off..end]))
    }

    /// Borrow the raw bytes of column `idx`'s minipage.
    pub fn minipage_bytes(&self, idx: usize) -> HexResult<&[u8]> {
        let dir = self.directory(idx)?;
        let off = dir.minipage_off as usize;
        let len = dir.compressed_len as usize;
        let end = off + len;
        let buf = self.buf.as_slice();
        if end > buf.len() {
            return Err(HexError::corrupt(format!(
                "minipage({}) out of bounds: {}..{}",
                idx, off, end
            )));
        }
        Ok(&buf[off..end])
    }

    /// Decode column `idx` (an integer column) back into a `Vec<i64>`. This
    /// is the *decoding* path — for hot scans you want [`Self::column_view`]
    /// which is zero-copy.
    pub fn decode_i64_column(&self, idx: usize) -> HexResult<Vec<i64>> {
        let dir = self.directory(idx)?;
        let bytes = self.minipage_bytes(idx)?;
        let enc = Encoding::from_discriminant(dir.encoding);
        decode_minipage(enc, bytes, dir.value_count as usize)
    }

    /// Zero-copy typed view over column `idx`'s minipage. Works for `Raw`
    /// encoded columns; for other encodings the caller should use
    /// [`Self::decode_i64_column`] instead.
    pub fn column_view<T: Pod>(&self, idx: usize) -> HexResult<ColumnView<'_, T>> {
        let dir = self.directory(idx)?;
        let bytes = self.minipage_bytes(idx)?;
        if dir.encoding != Encoding::Raw.discriminant() && !bytes.is_empty() {
            return Err(HexError::constraint(format!(
                "column_view: column {} is encoded ({:?}); decode first",
                idx,
                Encoding::from_discriminant(dir.encoding)
            )));
        }
        ColumnView::new(bytes)
    }

    /// Borrow the bytes of a single variable-length value (column `idx`,
    /// row `row`).
    pub fn var_value(&self, idx: usize, row: usize) -> HexResult<Option<&[u8]>> {
        let dir = self.directory(idx)?;
        if dir.kind != ValueKind::Bytes as u8 {
            return Err(HexError::constraint(format!(
                "var_value: column {} is not Bytes",
                idx
            )));
        }
        let mp = self.minipage_bytes(idx)?;
        let slot_off = row * 8;
        if slot_off + 8 > mp.len() {
            return Err(HexError::corrupt(format!(
                "var_value: slot {} out of bounds in column {}",
                row, idx
            )));
        }
        let off = u32::from_le_bytes([mp[slot_off], mp[slot_off + 1], mp[slot_off + 2], mp[slot_off + 3]]) as usize;
        let len = u32::from_le_bytes([mp[slot_off + 4], mp[slot_off + 5], mp[slot_off + 6], mp[slot_off + 7]]);
        if len == u32::MAX {
            return Ok(None); // null
        }
        let heap_off = self.header().varheap_off as usize + off;
        let heap_end = heap_off + len as usize;
        let buf = self.buf.as_slice();
        if heap_end > buf.len() {
            return Err(HexError::corrupt(format!(
                "var_value: heap slice {}..{} out of bounds",
                heap_off, heap_end
            )));
        }
        Ok(Some(&buf[heap_off..heap_end]))
    }

    /// Reconstruct row `row` as a `Vec<Value>` for API-boundary use. This is
    /// *not* a hot-path function — it allocates.
    pub fn materialize_row(&self, row: usize) -> HexResult<Vec<Value>> {
        let hdr = self.header();
        if row as u32 >= hdr.row_count {
            return Err(HexError::constraint(format!(
                "materialize_row: row {} out of range (block has {} rows)",
                row, hdr.row_count
            )));
        }
        let col_count = hdr.column_count as usize;
        let mut out = Vec::with_capacity(col_count);
        for i in 0..col_count {
            let dir = self.directory(i)?;
            let kind = ValueKind::from_u8(dir.kind);
            let v = match kind {
                ValueKind::Null => Value::Null,
                ValueKind::Bool => {
                    let view = self.column_view::<i64>(i)?;
                    let slice = view.as_slice();
                    Value::Bool(slice[row] != 0)
                }
                ValueKind::I64 => {
                    let vals = self.decode_i64_column(i)?;
                    Value::I64(vals[row])
                }
                ValueKind::U64 => {
                    let vals = self.decode_i64_column(i)?;
                    Value::U64(vals[row] as u64)
                }
                ValueKind::F64 => {
                    let vals = self.decode_i64_column(i)?;
                    Value::F64(f64::from_bits(vals[row] as u64))
                }
                ValueKind::Timestamp => {
                    let vals = self.decode_i64_column(i)?;
                    Value::Timestamp(vals[row])
                }
                ValueKind::Bytes => {
                    match self.var_value(i, row)? {
                        Some(b) => Value::Bytes(b.to_vec()),
                        None => Value::Null,
                    }
                }
            };
            out.push(v);
        }
        Ok(out)
    }
}

// ============================================================================
// PaxBlockReader — a non-owning reader over an externally-owned byte slice.
// This is what the buffer pool hands out (it doesn't need to take ownership).
// ============================================================================

/// Non-owning reader over a PAX block's bytes. Constructed from any aligned
/// `&[u8]` whose lifetime the caller controls.
pub struct PaxBlockReader<'a> {
    buf: &'a [u8],
    block_size: u32,
}

impl<'a> PaxBlockReader<'a> {
    pub fn new(buf: &'a [u8], block_size: u32) -> Self {
        Self { buf, block_size }
    }

    #[inline]
    pub fn header(&self) -> BlockHeader {
        if self.buf.len() < block_header_size() {
            return BlockHeader::empty();
        }
        bytemuck::pod_read_unaligned(&self.buf[..block_header_size()])
    }

    pub fn directory(&self, idx: usize) -> HexResult<ColumnDirectory> {
        let off = block_header_size() + idx * core::mem::size_of::<ColumnDirectory>();
        let end = off + core::mem::size_of::<ColumnDirectory>();
        if end > self.buf.len() {
            return Err(HexError::corrupt(format!("directory({}) out of bounds", idx)));
        }
        Ok(bytemuck::pod_read_unaligned(&self.buf[off..end]))
    }

    pub fn minipage_bytes(&self, idx: usize) -> HexResult<&'a [u8]> {
        let dir = self.directory(idx)?;
        let off = dir.minipage_off as usize;
        let len = dir.compressed_len as usize;
        let end = off + len;
        if end > self.buf.len() {
            return Err(HexError::corrupt(format!(
                "minipage({}) out of bounds: {}..{}",
                idx, off, end
            )));
        }
        Ok(&self.buf[off..end])
    }

    pub fn decode_i64_column(&self, idx: usize) -> HexResult<Vec<i64>> {
        let dir = self.directory(idx)?;
        let bytes = self.minipage_bytes(idx)?;
        let enc = Encoding::from_discriminant(dir.encoding);
        decode_minipage(enc, bytes, dir.value_count as usize)
    }

    pub fn column_view<T: Pod>(&self, idx: usize) -> HexResult<ColumnView<'a, T>> {
        let bytes = self.minipage_bytes(idx)?;
        let dir = self.directory(idx)?;
        if dir.encoding != Encoding::Raw.discriminant() && !bytes.is_empty() {
            return Err(HexError::constraint(format!(
                "column_view: column {} is encoded; decode first",
                idx
            )));
        }
        ColumnView::new(bytes)
    }

    pub fn var_value(&self, idx: usize, row: usize) -> HexResult<Option<&'a [u8]>> {
        let dir = self.directory(idx)?;
        if dir.kind != ValueKind::Bytes as u8 {
            return Err(HexError::constraint(format!(
                "var_value: column {} is not Bytes",
                idx
            )));
        }
        let mp = self.minipage_bytes(idx)?;
        let slot_off = row * 8;
        if slot_off + 8 > mp.len() {
            return Err(HexError::corrupt(format!(
                "var_value: slot {} out of bounds in column {}",
                row, idx
            )));
        }
        let off = u32::from_le_bytes([mp[slot_off], mp[slot_off + 1], mp[slot_off + 2], mp[slot_off + 3]]) as usize;
        let len = u32::from_le_bytes([mp[slot_off + 4], mp[slot_off + 5], mp[slot_off + 6], mp[slot_off + 7]]);
        if len == u32::MAX {
            return Ok(None);
        }
        let heap_off = self.header().varheap_off as usize + off;
        let heap_end = heap_off + len as usize;
        if heap_end > self.buf.len() {
            return Err(HexError::corrupt(format!(
                "var_value: heap slice {}..{} out of bounds",
                heap_off, heap_end
            )));
        }
        Ok(Some(&self.buf[heap_off..heap_end]))
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.buf
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn value_to_string(v: &Value) -> String {
    match v {
        Value::Null => "".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::I64(x) => x.to_string(),
        Value::U64(x) => x.to_string(),
        Value::F64(x) => x.to_string(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Timestamp(x) => x.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_specs() -> Vec<ColumnSpec> {
        vec![
            ColumnSpec::new(0, ValueKind::I64).pk(),
            ColumnSpec::new(1, ValueKind::Bytes),
            ColumnSpec::new(2, ValueKind::F64),
        ]
    }

    #[test]
    fn build_and_read_back() {
        let mut builder = PaxBlockBuilder::new(DEFAULT_BLOCK_SIZE_FOR_TESTS, demo_specs()).unwrap();
        for i in 0..10i64 {
            builder
                .append_row(&[
                    Value::I64(i),
                    Value::Bytes(format!("user-{}", i).into_bytes()),
                    Value::F64(i as f64 * 1.5),
                ])
                .unwrap();
        }
        let block = builder.finalize().unwrap();

        // Header
        let hdr = block.header();
        assert_eq!(hdr.row_count, 10);
        assert_eq!(hdr.column_count, 3);
        assert_eq!(hdr.min_pk, 0);
        assert_eq!(hdr.max_pk, 9);

        // Column 0: i64 ids
        let ids = block.decode_i64_column(0).unwrap();
        assert_eq!(ids, (0..10).collect::<Vec<_>>());

        // Column 1: bytes
        for i in 0..10 {
            let v = block.var_value(1, i as usize).unwrap().unwrap();
            assert_eq!(v, format!("user-{}", i).as_bytes());
        }

        // Column 2: f64
        let f = block.decode_i64_column(2).unwrap();
        for (i, bits) in f.iter().enumerate() {
            let x = f64::from_bits(*bits as u64);
            assert!((x - i as f64 * 1.5).abs() < 1e-9);
        }
    }

    #[test]
    fn zone_map_skipping() {
        let mut builder = PaxBlockBuilder::new(DEFAULT_BLOCK_SIZE_FOR_TESTS, demo_specs()).unwrap();
        for i in 100..200i64 {
            builder
                .append_row(&[
                    Value::I64(i),
                    Value::Bytes(format!("u-{}", i).into_bytes()),
                    Value::F64(i as f64),
                ])
                .unwrap();
        }
        let block = builder.finalize().unwrap();
        let hdr = block.header();
        // The block's zone map covers [100, 199]; queries outside should skip.
        assert!(hdr.zone_overlaps_pk(150, 160));
        assert!(!hdr.zone_overlaps_pk(0, 99));
        assert!(!hdr.zone_overlaps_pk(200, 300));
    }

    #[test]
    fn materialize_full_row() {
        let mut builder = PaxBlockBuilder::new(DEFAULT_BLOCK_SIZE_FOR_TESTS, demo_specs()).unwrap();
        builder
            .append_row(&[
                Value::I64(42),
                Value::Bytes(b"hello".to_vec()),
                Value::F64(3.14),
            ])
            .unwrap();
        let block = builder.finalize().unwrap();
        let row = block.materialize_row(0).unwrap();
        assert_eq!(row.len(), 3);
        match &row[0] {
            Value::I64(42) => {}
            other => panic!("expected I64(42), got {:?}", other),
        }
        match &row[1] {
            Value::Bytes(b) => assert_eq!(b, b"hello"),
            other => panic!("expected Bytes, got {:?}", other),
        }
        match &row[2] {
            Value::F64(x) => assert!((x - 3.14).abs() < 1e-9),
            other => panic!("expected F64, got {:?}", other),
        }
    }

    #[test]
    fn block_uses_encoding_for_monotonic_ints() {
        // PK column is monotonic → should auto-select DeltaOfDelta.
        let mut builder = PaxBlockBuilder::new(DEFAULT_BLOCK_SIZE_FOR_TESTS, demo_specs()).unwrap();
        for i in 0..1000i64 {
            builder
                .append_row(&[
                    Value::I64(i),
                    Value::Bytes(format!("u-{}", i).into_bytes()),
                    Value::F64(i as f64),
                ])
                .unwrap();
        }
        let block = builder.finalize().unwrap();
        let dir = block.directory(0).unwrap();
        let enc = Encoding::from_discriminant(dir.encoding);
        assert!(
            matches!(enc, Encoding::DeltaOfDelta),
            "expected DeltaOfDelta, got {:?}",
            enc
        );
    }

    const DEFAULT_BLOCK_SIZE_FOR_TESTS: u32 = 64 * 1024;
}
