//! Optional mmap-backed read-only page source.
//!
//! Enabled with the `mmap` cargo feature. Useful for tiny, read-mostly KV
//! deployments where the OS page cache is sufficient and the binary size
//! cost of the custom buffer pool is unjustified.
//!
//! ## When to use
//!
//!   * The dataset fits comfortably in RAM.
//!   * The workload is dominated by point lookups (no scans).
//!   * Cold-start latency matters more than eviction control.
//!
//! ## When NOT to use
//!
//!   * Mixed OLTP/OLAP workloads (scan resistance requires the custom pool).
//!   * Large datasets that don't fit in RAM (mmap page faults cause
//!     unpredictable tail latency).
//!   * Embedded deployments with hard memory caps (mmap is invisible to
//!     the process's RSS accounting).

use std::fs::File;
use std::path::Path;

use cendb_core::{CenError, CenResult, PageId};

use crate::pool::PageSource;

/// Mmap-backed read-only page source. Owns the underlying file and the
/// memory map. All reads are zero-copy from the OS page cache.
pub struct MmapPageSource {
    _file: File,
    map: memmap2::Mmap,
    page_size: usize,
}

impl MmapPageSource {
    /// Open a file and map it read-only. The file must already exist and
    /// be at least `page_size` bytes long.
    pub fn open(path: impl AsRef<Path>, page_size: usize) -> CenResult<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(path.as_ref())?;
        let map = unsafe { memmap2::Mmap::map(&file) }
            .map_err(|e| CenError::io(format!("mmap failed: {}", e)))?;
        if map.len() < page_size {
            return Err(CenError::corrupt(format!(
                "mmap file too short: {} < page_size {}",
                map.len(),
                page_size
            )));
        }
        Ok(Self {
            _file: file,
            map,
            page_size,
        })
    }

    /// The mapped byte slice.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.map[..]
    }

    /// Total bytes mapped.
    #[inline]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl PageSource for MmapPageSource {
    fn read_page(&mut self, page_id: PageId, buf: &mut [u8]) -> CenResult<()> {
        // Compute the byte offset of this page within the mapped file.
        // Use checked arithmetic to prevent overflow on large page IDs.
        let off = (page_id.0 as usize)
            .checked_mul(self.page_size)
            .ok_or_else(|| CenError::corrupt(format!(
                "mmap read: page {} offset overflow ({} * {})",
                page_id.0, page_id.0, self.page_size
            )))?;
        let end = off.checked_add(self.page_size)
            .ok_or_else(|| CenError::corrupt("mmap read: end offset overflow"))?;
        if end > self.map.len() {
            return Err(CenError::corrupt(format!(
                "mmap read: page {} out of bounds ({}..{} > {})",
                page_id.0, off, end, self.map.len()
            )));
        }
        buf[..self.page_size].copy_from_slice(&self.map[off..end]);
        Ok(())
    }

    fn write_page(&mut self, _page_id: PageId, _buf: &[u8]) -> CenResult<()> {
        Err(CenError::constraint(
            "MmapPageSource is read-only; use InMemoryPageSource or SegmentFile for writes",
        ))
    }

    fn page_size(&self) -> usize {
        self.page_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::InMemoryPageSource;
    use std::io::Write;

    #[test]
    fn mmap_read_only_works() {
        // Create a temp file with 4 pages of 4096 bytes each.
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let page_size = 4096;
        for page_idx in 0..4u64 {
            let mut page = vec![0u8; page_size];
            page[0] = page_idx as u8;
            tmp.as_file_mut().write_all(&page).unwrap();
        }
        tmp.as_file_mut().sync_all().unwrap();
        let path = tmp.path().to_path_buf();
        let mut src = MmapPageSource::open(&path, page_size).unwrap();
        // Read page 2.
        let mut buf = vec![0u8; page_size];
        src.read_page(PageId(2), &mut buf).unwrap();
        assert_eq!(buf[0], 2);
    }

    #[test]
    fn mmap_writes_fail() {
        // Create a temp file with at least one page of bytes so the mmap
        // length check passes.
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.as_file_mut().write_all(&[0u8; 4096]).unwrap();
        tmp.as_file_mut().sync_all().unwrap();
        let path = tmp.path().to_path_buf();
        let mut src = MmapPageSource::open(&path, 4096).unwrap();
        let result = src.write_page(PageId(0), &[0u8; 4096]);
        assert!(result.is_err());
    }

    // Ensure InMemoryPageSource still works alongside the mmap source.
    #[test]
    fn in_memory_source_still_works() {
        let mut src = InMemoryPageSource::new(4096);
        let mut buf = vec![0u8; 4096];
        src.read_page(PageId(0), &mut buf).unwrap();
    }

    #[test]
    fn mmap_zero_copy_read() {
        // Verify zero-copy: the bytes returned from as_bytes() are the
        // same bytes that were written to the file.
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let page_size = 4096;
        let mut page = vec![0u8; page_size];
        for i in 0..page_size {
            page[i] = (i % 256) as u8;
        }
        tmp.as_file_mut().write_all(&page).unwrap();
        tmp.as_file_mut().sync_all().unwrap();
        let path = tmp.path().to_path_buf();
        let src = MmapPageSource::open(&path, page_size).unwrap();
        let bytes = src.as_bytes();
        assert_eq!(bytes.len(), page_size);
        // Verify content matches — this is zero-copy from the OS page cache.
        assert_eq!(&bytes[..], &page[..]);
    }

    #[test]
    fn mmap_large_file() {
        // Verify mmap works with larger files (multiple pages).
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let page_size = 4096;
        let num_pages = 100;
        for page_idx in 0..num_pages {
            let mut page = vec![0u8; page_size];
            page[0] = page_idx as u8;
            page[1] = (page_idx >> 8) as u8;
            tmp.as_file_mut().write_all(&page).unwrap();
        }
        tmp.as_file_mut().sync_all().unwrap();
        let path = tmp.path().to_path_buf();
        let mut src = MmapPageSource::open(&path, page_size).unwrap();
        // Read each page and verify.
        for page_idx in 0..num_pages as u64 {
            let mut buf = vec![0u8; page_size];
            src.read_page(PageId(page_idx), &mut buf).unwrap();
            assert_eq!(buf[0], page_idx as u8);
            assert_eq!(buf[1], (page_idx >> 8) as u8);
        }
    }
}

/// Zero-copy segment reader backed by mmap. For read-mostly workloads
/// where the dataset fits in RAM, this bypasses the buffer pool entirely
/// and reads directly from the OS page cache via memory mapping.
///
/// ## Usage
///
/// ```rust,ignore
/// use cendb_buffer::MmapSegmentReader;
///
/// let reader = MmapSegmentReader::open("data.seg", 4096)?;
/// let page_bytes = reader.read_page_zero_copy(PageId(42))?;
/// ```
pub struct MmapSegmentReader {
    source: MmapPageSource,
}

impl MmapSegmentReader {
    /// Open a segment file for zero-copy read access via mmap.
    pub fn open(path: impl AsRef<Path>, page_size: usize) -> CenResult<Self> {
        Ok(Self {
            source: MmapPageSource::open(path, page_size)?,
        })
    }

    /// Read a page zero-copy — returns a reference to the mapped bytes.
    /// No allocation, no copy. The reference is valid for the lifetime
    /// of this reader.
    #[inline]
    pub fn read_page_zero_copy(&self, page_id: PageId) -> CenResult<&[u8]> {
        let off = (page_id.0 as usize)
            .checked_mul(self.source.page_size)
            .ok_or_else(|| CenError::corrupt(format!(
                "mmap zero-copy: page {} offset overflow", page_id.0
            )))?;
        let end = off.checked_add(self.source.page_size)
            .ok_or_else(|| CenError::corrupt("mmap zero-copy: end offset overflow"))?;
        if end > self.source.len() {
            return Err(CenError::corrupt(format!(
                "mmap zero-copy read: page {} out of bounds ({}..{} > {})",
                page_id.0, off, end, self.source.len()
            )));
        }
        Ok(&self.source.as_bytes()[off..end])
    }

    /// Total bytes mapped.
    #[inline]
    pub fn len(&self) -> usize {
        self.source.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    /// Page size.
    #[inline]
    pub fn page_size(&self) -> usize {
        self.source.page_size
    }

    /// Number of pages in the mapped file.
    #[inline]
    pub fn page_count(&self) -> u64 {
        (self.source.len() / self.source.page_size) as u64
    }
}
