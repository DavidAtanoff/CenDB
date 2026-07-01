# Storage Subsystem

The storage layer (`cendb-storage`) implements the unified PAX page
format shared by every CenDB projection. This document covers the on-disk
layout, alignment rules, and the encoding pipeline.

## Geometry

| Unit | Default size | Notes |
|---|---|---|
| Page | 4 KiB | OS page; the buffer pool's unit of I/O. |
| Block | 256 KiB | The PAX unit; holds a horizontal partition of rows. |
| Segment | 64 MiB | Append-mostly, immutable once sealed. |

A segment file starts with a `SegmentHeader` (64 bytes), followed by a
sequence of PAX blocks, and ends with a `BlockDirectory` written at seal
time.

## Segment header

```rust
#[repr(C)]
pub struct SegmentHeader {            // 64 bytes total
    pub magic:         [u8; 8],       // b"CENDB001"
    pub segment_id:    u64,
    pub created_lsn:   u64,
    pub sealed_lsn:    u64,           // 0 while mutable
    pub block_dir_off: u64,           // written at seal
    pub checksum:      u64,           // xxh3 of header
    pub page_size:     u32,           // 4096..=65536, power of two
    pub block_size:    u32,           // multiple of page_size
    pub block_count:   u32,
    pub format_ver:    u16,           // = 1
    pub flags:         u16,           // bit 0 = sealed, etc.
}
```

Fields are arranged in descending alignment order so the struct has no
internal padding (required by `bytemuck::Pod`).

## PAX block layout

```
┌───────────────────────────────────────────────────────────┐
│ BlockHeader (64B)                                         │
│  row_count, column_count, min_pk, max_pk, min_ts, max_ts │  ← zone map
│  tombstone_bitmap_off, null_bitmap_off                    │
│  minipages_off, varheap_off, varheap_len                  │
├───────────────────────────────────────────────────────────┤
│ ColumnDirectory[column_count]   (64B each)                │
│  col_id, kind, encoding, flags, minipage_off,             │
│  minipage_len, compressed_len, value_count,               │
│  zone_min, zone_max                                       │
├───────────────────────────────────────────────────────────┤
│ Minipage[0]   (64B-aligned)                               │
│ Minipage[1]   (64B-aligned)                               │
│ ...                                                        │
│ Minipage[n]   (64B-aligned)                               │
├───────────────────────────────────────────────────────────┤
│ Variable-length heap (grows upward)                       │
│   - string/blob bytes for Bytes columns                   │
│ ...                                                        │
│ Tombstone bitmap + Null bitmaps (grows downward)          │
└───────────────────────────────────────────────────────────┘
```

### Why PAX?

- **Pure row (NSM):** great for point lookups; terrible for scans.
- **Pure column (DSM):** great for analytics; terrible for point
  lookups (N random reads) and writes.
- **PAX:** one page holds all columns of a horizontal partition, but
  stores each column contiguously ("minipages"). Point lookups touch
  one page; scans read only the requested minipages. The
  "fractured mirror without the mirror".

### Alignment

- Minipages are **64-byte aligned** (cache line + AVX-512 register
  width) so SIMD scans never straddle.
- Blocks are **page-size aligned** within the segment.
- The variable-length heap uses **8-byte alignment** with `(offset, len)`
  slots referenced from a fixed-width minipage.

## Zone maps

Every `BlockHeader` carries a zone map (`min_pk`, `max_pk`, `min_ts`,
`max_ts`). Scans consult the zone map to skip entire blocks:

```rust
if !block.header().zone_overlaps_ts(lo, hi) {
    continue;  // skip this block
}
```

This is the workhorse of time-series predicate pushdown: a scan with
`WHERE ts > X` skips blocks whose `max_ts < X` without reading them.

## Encodings

The encoding pipeline is **two-stage**:

1. **Type-aware encoding** (keeps data SIMD-decodable; many predicates
   run on encoded data without decoding).
2. **Optional general-purpose compression** (LZ4/zstd) for cold data.

### Stage 1: type-aware encodings

| Encoding | Use case | Implemented? |
|---|---|---|
| `Raw` | Incompressible data; baseline. | ✅ |
| `BitPacked { bits }` | Integers in small range. | ✅ |
| `FrameOfReference { base, bits }` | Clustered integers. | ✅ |
| `DeltaOfDelta` | Monotonic integers (timestamps, sequential PKs). | ✅ |
| `RunLength` | Long runs of identical values (sorted/sparse). | ✅ |
| `Gorilla` | Time-series floats (XOR of consecutive). | ✅ |
| `Chimp128` | Improved TS float codec. | (stub → Raw) |
| `Dictionary { dict_id }` | Low-cardinality strings/enums. | (stub → Raw) |
| `Fsst` | Short-string compression. | (stub → Raw) |

### Auto-selection

When `Encoding::Raw` is requested for an integer column, the builder
auto-selects the best encoding based on observed values:

```rust
let enc = auto_select_encoding_i64(vals);
```

Heuristics (from §4.1.1 of the spec):
- **Monotonic integers** → `DeltaOfDelta` (best for timestamps).
- **Range fits in ≤16 bits, all non-negative** → `BitPacked`.
- **Clustered integers, range < 2^32** → `FrameOfReference`.
- **Otherwise** → `Raw`.

### Gorilla XOR float encoding

Gorilla is the canonical time-series float codec. For each value after
the first, it stores the XOR with the previous value using a compact
control-bit scheme:

- **XOR == 0**: 1 bit (`0`).
- **XOR != 0, same leading/trailing zero block as previous**: 2 bits +
  meaningful bits of XOR.
- **XOR != 0, new block**: 1 + 5 (leading) + 6 (meaningful) + meaningful
  bits.

For slowly-changing floats (temperatures, prices), Gorilla achieves
~1-2 bits per value vs 64 bits raw.

## Zero-copy reads

The on-disk minipage layout *is* the in-memory layout for fixed-width
data. `ColumnView<'a, T>` borrows a slice of a frame's bytes and
reinterprets it as `&'a [T]`:

```rust
let view: ColumnView<i64> = block.column_view(0)?;
let slice: &[i64] = view.as_slice();  // zero-copy, no allocation
```

Invariants (checked once at page-load time, not per-access):
- The byte slice's length is a multiple of `size_of::<T>()`.
- The slice is 64-byte aligned.

## Block builder

`PaxBlockBuilder` accumulates rows in columnar staging buffers in
memory, then `finalize()` lays them out into a single aligned buffer of
the requested block size:

```rust
let mut builder = PaxBlockBuilder::new(block_size, specs)?;
for row in rows {
    builder.append_row(&row)?;
}
let block: PaxBlock = builder.finalize()?;
```

The builder:
1. Encodes each column's staging buffer via `encode_minipage`.
2. Aligns each minipage to 64 bytes.
3. Concatenates the variable-length heap.
4. Writes the `BlockHeader` with zone maps populated from observed
   min/max.
5. Returns an owned `PaxBlock` with a 64-byte-aligned buffer.

## Segment I/O

`SegmentWriter` appends blocks to a segment file; `SegmentFile` reads
them back:

```rust
let mut writer = SegmentWriter::create(path, segment_id, page_size, block_size, lsn)?;
let block_id = writer.append_block(&block)?;
writer.seal(sealed_lsn)?;

let mut reader = SegmentFile::open(path)?;
let mut buf = vec![0u8; block_size as usize];
reader.read_block(block_id, &mut buf)?;
```

The block directory's zone map allows skipping blocks at the directory
level (without reading the block bytes):

```rust
let candidates: Vec<_> = reader.block_dir.blocks_overlapping_ts(lo, hi);
for entry in candidates {
    reader.read_block(entry.block_id, &mut buf)?;
    // ...
}
```
