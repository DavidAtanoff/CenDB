//! Segment file I/O: writing and reading 64 MiB append-mostly segment files.
//!
//! A segment file starts with a [`SegmentHeader`], followed by a sequence of
//! PAX blocks (each `block_size` bytes), and ends with a [`BlockDirectory`]
//! written at seal time.
//!
//! For the prototype we use synchronous `pread`/`pwrite`-style I/O via
//! `std::fs::File` + `seek`/`read_exact`/`write_all`. The buffer pool wraps
//! these primitives with caching and pinning.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use cendb_core::{BlockId, HexError, HexResult, SegmentId, FORMAT_VERSION, SEGMENT_MAGIC};

use crate::header::SegmentHeader;
use crate::pax::PaxBlock;

/// Directory of blocks within a sealed segment. Written at the end of the
/// segment file at offset `block_dir_off` (recorded in the segment header).
/// Each entry maps a `BlockId` to its byte offset and recorded row count.
#[derive(Clone, Debug)]
pub struct BlockDirectory {
    pub entries: Vec<BlockDirEntry>,
}

#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct BlockDirEntry {
    /// Byte offset of the block within the segment file.
    pub byte_off: u64,
    /// Zone map: minimum partitioning key in this block.
    pub min_pk: i64,
    /// Zone map: maximum partitioning key in this block.
    pub max_pk: i64,
    /// Zone map: minimum timestamp in this block.
    pub min_ts: i64,
    /// Zone map: maximum timestamp in this block.
    pub max_ts: i64,
    /// Block id (index within the segment).
    pub block_id: u32,
    /// Number of rows in this block.
    pub row_count: u32,
    /// Reserved for future use; must be zero.
    pub _reserved: u32,
    /// Padding to bring the struct to exactly 56 bytes (no internal padding).
    pub _pad: u32,
}

impl BlockDirectory {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    pub fn push(&mut self, entry: BlockDirEntry) {
        self.entries.push(entry);
    }

    /// Find blocks whose zone map overlaps `[pk_lo, pk_hi]`. Used by the
    /// time-series and KV projections to skip blocks at the directory level.
    pub fn blocks_overlapping_pk(&self, pk_lo: i64, pk_hi: i64) -> Vec<&BlockDirEntry> {
        self.entries
            .iter()
            .filter(|e| !(e.max_pk < pk_lo || e.min_pk > pk_hi))
            .collect()
    }

    /// Find blocks whose zone map overlaps `[ts_lo, ts_hi]`.
    pub fn blocks_overlapping_ts(&self, ts_lo: i64, ts_hi: i64) -> Vec<&BlockDirEntry> {
        self.entries
            .iter()
            .filter(|e| !(e.max_ts < ts_lo || e.min_ts > ts_hi))
            .collect()
    }

    /// Serialise to bytes (length-prefixed).
    pub fn to_bytes(&self) -> Vec<u8> {
        let count = self.entries.len() as u32;
        let mut out = Vec::with_capacity(4 + self.entries.len() * core::mem::size_of::<BlockDirEntry>());
        out.extend_from_slice(&count.to_le_bytes());
        for e in &self.entries {
            out.extend_from_slice(bytemuck::bytes_of(e));
        }
        out
    }

    /// Deserialise from bytes.
    pub fn from_bytes(bytes: &[u8]) -> HexResult<Self> {
        if bytes.len() < 4 {
            return Err(HexError::corrupt("BlockDirectory: short header"));
        }
        let count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let body = &bytes[4..];
        let entry_size = core::mem::size_of::<BlockDirEntry>();
        if body.len() < count * entry_size {
            return Err(HexError::corrupt(format!(
                "BlockDirectory: expected {} entries × {} bytes, got {}",
                count,
                entry_size,
                body.len()
            )));
        }
        // The input slice may not be aligned for &BlockDirEntry (Vec<u8> has
        // alignment 1). We use `pod_read_unaligned` to copy each entry out
        // — the directory is small so the copy is fine.
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let off = i * entry_size;
            let end = off + entry_size;
            let e: BlockDirEntry = bytemuck::pod_read_unaligned(&body[off..end]);
            entries.push(e);
        }
        Ok(Self { entries })
    }
}

// ============================================================================
// SegmentWriter — append blocks to a new segment file.
// ============================================================================

/// Append-only writer for a segment file. Owns the underlying `File` and
/// tracks the current write offset. Call `seal()` to finalise.
pub struct SegmentWriter {
    file: File,
    header: SegmentHeader,
    block_size: u32,
    next_block_id: u32,
    block_dir: BlockDirectory,
    current_offset: u64,
}

impl SegmentWriter {
    /// Create a new segment file at `path`. Overwrites if the file exists.
    pub fn create(
        path: impl AsRef<Path>,
        segment_id: SegmentId,
        page_size: u32,
        block_size: u32,
        created_lsn: u64,
    ) -> HexResult<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path.as_ref())?;
        let header = SegmentHeader::new(segment_id, page_size, block_size, created_lsn);
        let mut me = Self {
            file,
            header,
            block_size,
            next_block_id: 0,
            block_dir: BlockDirectory::new(),
            current_offset: core::mem::size_of::<SegmentHeader>() as u64,
        };
        me.write_header()?;
        Ok(me)
    }

    fn write_header(&mut self) -> HexResult<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(bytemuck::bytes_of(&self.header))?;
        self.file.flush()?;
        Ok(())
    }

    /// Append a finished PAX block to the segment. Returns the assigned
    /// `BlockId`.
    pub fn append_block(&mut self, block: &PaxBlock) -> HexResult<BlockId> {
        let bytes = block.as_bytes();
        if bytes.len() != self.block_size as usize {
            return Err(HexError::constraint(format!(
                "append_block: block is {} bytes, segment expects {}",
                bytes.len(),
                self.block_size
            )));
        }
        let block_id = self.next_block_id;
        let byte_off = self.current_offset;

        self.file.seek(SeekFrom::Start(byte_off))?;
        self.file.write_all(bytes)?;
        self.current_offset += bytes.len() as u64;

        // Record directory entry (zone map copied from the block header).
        let bh = block.header();
        self.block_dir.push(BlockDirEntry {
            byte_off,
            min_pk: bh.min_pk,
            max_pk: bh.max_pk,
            min_ts: bh.min_ts,
            max_ts: bh.max_ts,
            block_id,
            row_count: bh.row_count,
            _reserved: 0,
            _pad: 0,
        });

        self.next_block_id += 1;
        self.header.block_count = self.next_block_id;
        self.write_header()?;
        Ok(BlockId(block_id))
    }

    /// Seal the segment: write the block directory and mark the header as
    /// sealed. After this call the segment is immutable.
    pub fn seal(&mut self, sealed_lsn: u64) -> HexResult<()> {
        let dir_bytes = self.block_dir.to_bytes();
        let dir_off = self.current_offset;
        self.file.seek(SeekFrom::Start(dir_off))?;
        self.file.write_all(&dir_bytes)?;

        self.header.sealed_lsn = sealed_lsn;
        self.header.flags |= SegmentHeader::FLAG_SEALED;
        self.header.block_dir_off = dir_off;
        self.write_header()?;
        self.file.flush()?;
        Ok(())
    }

    pub fn block_count(&self) -> u32 {
        self.next_block_id
    }
}

// ============================================================================
// SegmentReader / SegmentFile — random access to a sealed segment.
// ============================================================================

/// Random-access reader for a segment file. Keeps the file handle open and
/// caches the segment header + block directory in memory for cheap block
/// lookups.
pub struct SegmentFile {
    file: File,
    pub header: SegmentHeader,
    pub block_dir: BlockDirectory,
    block_size: u32,
}

impl SegmentFile {
    /// Open an existing segment file and load its header + block directory.
    pub fn open(path: impl AsRef<Path>) -> HexResult<Self> {
        let mut file = OpenOptions::new().read(true).open(path.as_ref())?;
        // Read header.
        let mut hdr_bytes = [0u8; core::mem::size_of::<SegmentHeader>()];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut hdr_bytes)?;
        // `hdr_bytes` is a stack array with alignment 1, so we use
        // `pod_read_unaligned` (copies the value) instead of `from_bytes`
        // (which would require 8-byte alignment).
        let header: SegmentHeader = bytemuck::pod_read_unaligned(&hdr_bytes);
        if header.magic != SEGMENT_MAGIC {
            return Err(HexError::corrupt("segment magic mismatch"));
        }
        if header.format_ver != FORMAT_VERSION {
            return Err(HexError::corrupt(format!(
                "unsupported format version {}",
                header.format_ver
            )));
        }

        // Read block directory.
        let mut dir = BlockDirectory::new();
        if header.block_dir_off != 0 {
            let dir_len = file.metadata()?.len() - header.block_dir_off;
            let mut dir_bytes = vec![0u8; dir_len as usize];
            file.seek(SeekFrom::Start(header.block_dir_off))?;
            file.read_exact(&mut dir_bytes)?;
            dir = BlockDirectory::from_bytes(&dir_bytes)?;
        }

        Ok(Self {
            file,
            header,
            block_dir: dir,
            block_size: header.block_size,
        })
    }

    /// Read a single block's bytes into `buf`. `buf` must be at least
    /// `block_size` bytes long and 64-byte aligned for SIMD-safe access.
    pub fn read_block(&mut self, block_id: BlockId, buf: &mut [u8]) -> HexResult<()> {
        let entry = self
            .block_dir
            .entries
            .iter()
            .find(|e| e.block_id == block_id.0)
            .ok_or_else(|| HexError::not_found(format!("block {} not in directory", block_id.0)))?;
        if buf.len() < self.block_size as usize {
            return Err(HexError::constraint(format!(
                "read_block: buf is {} bytes, need {}",
                buf.len(),
                self.block_size
            )));
        }
        self.file.seek(SeekFrom::Start(entry.byte_off))?;
        self.file.read_exact(&mut buf[..self.block_size as usize])?;
        Ok(())
    }

    /// Convenience: read a block into a freshly allocated 64-byte-aligned
    /// buffer.
    pub fn read_block_alloc(&mut self, block_id: BlockId) -> HexResult<Vec<u8>> {
        let mut buf = vec![0u8; self.block_size as usize];
        // Note: Vec<u8>'s allocation alignment may be < 64B. For the prototype
        // we accept this; the buffer pool path uses properly-aligned frames
        // for hot reads. We still succeed here for cold paths where zero-copy
        // SIMD is not required.
        self.read_block(block_id, &mut buf)?;
        Ok(buf)
    }

    #[inline]
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    #[inline]
    pub fn segment_id(&self) -> SegmentId {
        SegmentId(self.header.segment_id)
    }
}

/// Lighter-weight reader with no caching. Used for one-off reads during
/// recovery / debugging.
pub struct SegmentReader {
    inner: SegmentFile,
}

impl SegmentReader {
    pub fn open(path: impl AsRef<Path>) -> HexResult<Self> {
        Ok(Self { inner: SegmentFile::open(path)? })
    }

    pub fn read_block_alloc(&mut self, block_id: BlockId) -> HexResult<Vec<u8>> {
        self.inner.read_block_alloc(block_id)
    }

    pub fn header(&self) -> &SegmentHeader {
        &self.inner.header
    }

    pub fn block_dir(&self) -> &BlockDirectory {
        &self.inner.block_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::ColumnSpec;
    use crate::pax::PaxBlockBuilder;
    use cendb_core::{Value, ValueKind};
    use tempfile::tempdir;

    #[test]
    fn segment_write_read_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("seg.cdb");

        let specs = vec![
            ColumnSpec::new(0, ValueKind::I64).pk(),
            ColumnSpec::new(1, ValueKind::Bytes),
        ];
        let block_size: u32 = 16 * 1024;

        // Write 3 blocks.
        let block_ids: Vec<BlockId>;
        {
            let mut writer = SegmentWriter::create(
                &path,
                SegmentId(42),
                4096,
                block_size,
                100,
            )
            .unwrap();
            block_ids = (0..3)
                .map(|i| {
                    let mut b = PaxBlockBuilder::new(block_size, specs.clone()).unwrap();
                    for j in 0..50 {
                        let pk = (i * 50 + j) as i64;
                        b.append_row(&[
                            Value::I64(pk),
                            Value::Bytes(format!("v-{}", pk).into_bytes()),
                        ])
                        .unwrap();
                    }
                    let block = b.finalize().unwrap();
                    writer.append_block(&block).unwrap()
                })
                .collect();
            writer.seal(200).unwrap();
        }

        // Read back.
        let mut reader = SegmentFile::open(&path).unwrap();
        assert_eq!(reader.header.segment_id, 42);
        assert!(reader.header.is_sealed());
        assert_eq!(reader.block_dir.entries.len(), 3);

        for (i, &bid) in block_ids.iter().enumerate() {
            let buf = reader.read_block_alloc(bid).unwrap();
            assert_eq!(buf.len(), block_size as usize);
            // Parse via PaxBlockReader.
            let r = crate::pax::PaxBlockReader::new(&buf, block_size);
            assert_eq!(r.header().row_count, 50);
            let vals = r.decode_i64_column(0).unwrap();
            assert_eq!(vals[0], (i * 50) as i64);
        }
    }

    #[test]
    fn block_directory_zone_map_filter() {
        let mut dir = BlockDirectory::new();
        dir.push(BlockDirEntry {
            byte_off: 64,
            min_pk: 0, max_pk: 99, min_ts: 1000, max_ts: 1999,
            block_id: 0, row_count: 100, _reserved: 0, _pad: 0,
        });
        dir.push(BlockDirEntry {
            byte_off: 64 + 256 * 1024,
            min_pk: 100, max_pk: 199, min_ts: 2000, max_ts: 2999,
            block_id: 1, row_count: 100, _reserved: 0, _pad: 0,
        });
        dir.push(BlockDirEntry {
            byte_off: 64 + 2 * 256 * 1024,
            min_pk: 200, max_pk: 299, min_ts: 3000, max_ts: 3999,
            block_id: 2, row_count: 100, _reserved: 0, _pad: 0,
        });

        let hits = dir.blocks_overlapping_pk(150, 250);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].block_id, 1);
        assert_eq!(hits[1].block_id, 2);

        let hits = dir.blocks_overlapping_ts(0, 1500);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].block_id, 0);
    }

    #[test]
    fn block_directory_roundtrip() {
        let mut dir = BlockDirectory::new();
        for i in 0..10u32 {
            dir.push(BlockDirEntry {
                byte_off: 64 + i as u64 * 256 * 1024,
                min_pk: i as i64 * 100,
                max_pk: i as i64 * 100 + 99,
                min_ts: 0, max_ts: 0,
                block_id: i, row_count: 100, _reserved: 0, _pad: 0,
            });
        }
        let bytes = dir.to_bytes();
        let back = BlockDirectory::from_bytes(&bytes).unwrap();
        assert_eq!(back.entries.len(), 10);
        for (a, b) in dir.entries.iter().zip(back.entries.iter()) {
            assert_eq!(a.block_id, b.block_id);
            assert_eq!(a.byte_off, b.byte_off);
        }
    }
}
