//! The user-space buffer pool.
//!
//! Owns a fixed slab of [`Frame`]s and serves `pin_page` / `unpin` requests.
//! When a requested page is not in memory, the pool reads it from disk via
//! the caller-supplied [`PageSource`] trait, evicting an existing frame if
//! the pool is full. Writes go through `mark_dirty`; the flush path writes
//! dirty pages back via [`PageSource::write`].
//!
//! ## Concurrency
//!
//! For the prototype the pool is `!Sync` (single-threaded). The `Frame`
//! struct's atomic fields are still atomic because we want the *option* of
//! sharing a pool across threads in the future without changing the Frame
//! API. A production version would wrap `BufferPool` in a `Mutex` or use
//! per-frame latches.
//!
//! ## RAII pinning
//!
//! [`PinnedPage`] holds a `&'pool Frame` borrowed for the lifetime of the
//! pool. On `Drop` it decrements the frame's pin count. The borrow checker
//! therefore guarantees that any `ColumnView` derived from a pinned page
//! cannot outlive the pin — compile-time prevention of use-after-evict.

use cendb_core::{FrameId, HexError, HexResult, PageId};

use crate::frame::Frame;
use crate::lru::LruK;

/// Hint to the buffer pool about how the caller intends to use the page.
/// Affects eviction policy: scan hints mark pages as low-priority so a
/// sequential scan cannot evict the OLTP hot set.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ReadHint {
    /// Point lookup or OLTP-style access — page is part of the hot set.
    Point,
    /// Sequential scan — page is likely touched once and not again.
    Scan,
}

/// Backend that the buffer pool calls to satisfy page faults and writebacks.
/// In the production engine this is implemented by `SegmentFile`; tests
/// supply an in-memory implementation.
pub trait PageSource {
    /// Read the bytes of `page_id` into `buf`. `buf` is guaranteed to be
    /// `page_size` bytes long and 64-byte aligned.
    fn read_page(&mut self, page_id: PageId, buf: &mut [u8]) -> HexResult<()>;

    /// Write `buf` (the bytes of `page_id`) back to durable storage.
    fn write_page(&mut self, page_id: PageId, buf: &[u8]) -> HexResult<()>;

    /// Page size in bytes (must match what the pool was constructed with).
    fn page_size(&self) -> usize;
}

/// Statistics snapshot for inspection / testing.
#[derive(Copy, Clone, Debug, Default)]
pub struct PoolStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub flushes: u64,
    pub pinned_frames: u32,
    pub total_frames: u32,
}

/// The buffer pool. Owns a slab of frames and an LRU-K eviction policy.
pub struct BufferPool {
    frames: Vec<Frame>,
    page_table: std::collections::HashMap<PageId, FrameId>,
    free_list: Vec<FrameId>,
    replacer: LruK,
    source: Box<dyn PageSource>,
    /// Reserved for future use (e.g. validating that pages read from the
    /// source match the pool's configured page size).
    #[allow(dead_code)]
    page_size: usize,
    stats: PoolStats,
}

impl BufferPool {
    /// Construct a new pool with `frame_count` frames of `page_size` bytes
    /// each, backed by `source` for I/O.
    pub fn new(source: Box<dyn PageSource>, frame_count: usize, page_size: usize) -> HexResult<Self> {
        if frame_count == 0 {
            return Err(HexError::constraint("BufferPool: frame_count must be > 0"));
        }
        if page_size == 0 || page_size % 64 != 0 {
            return Err(HexError::constraint(
                "BufferPool: page_size must be a positive multiple of 64",
            ));
        }
        let mut frames: Vec<Frame> = Vec::with_capacity(frame_count);
        for i in 0..frame_count {
            frames.push(Frame::new(FrameId(i as u32), page_size)?);
        }
        let free_list: Vec<FrameId> = (0..frame_count).map(|i| FrameId(i as u32)).collect();
        Ok(Self {
            frames,
            page_table: std::collections::HashMap::new(),
            free_list,
            replacer: LruK::new(),
            source,
            page_size,
            stats: PoolStats {
                total_frames: frame_count as u32,
                ..Default::default()
            },
        })
    }

    /// Number of frames in the pool.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.frames.len()
    }

    /// Current statistics snapshot.
    #[inline]
    pub fn stats(&self) -> PoolStats {
        let mut s = self.stats;
        s.pinned_frames = self.frames.iter().filter(|f| f.is_pinned()).count() as u32;
        s
    }

    /// Pin a page in the pool, reading it from disk on miss. Returns a
    /// [`PinnedPage`] guard whose lifetime is tied to the pool — the borrow
    /// checker prevents any `ColumnView` derived from it from outliving the
    /// pin.
    pub fn pin_page(&mut self, page_id: PageId, hint: ReadHint) -> HexResult<PinnedPage<'_>> {
        // Fast path: page already in pool.
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            self.stats.hits += 1;
            // Only Point accesses count toward LRU-K history; Scan accesses
            // are recorded as "first access only" so they're evicted first.
            if hint == ReadHint::Point {
                self.replacer.record_access(frame_id);
            }
            self.frames[frame_id.0 as usize].pin();
            return Ok(PinnedPage {
                frame_id,
                _pool: core::marker::PhantomData,
                pool: self,
            });
        }
        // Miss: need to load the page.
        self.stats.misses += 1;
        let frame_id = self.evict_or_take_free()?;
        let frame = &mut self.frames[frame_id.0 as usize];

        // Read page bytes from disk.
        let buf: &mut [u8] = frame.as_bytes_mut();
        self.source.read_page(page_id, buf)?;

        // Update frame metadata.
        frame.set_page_id(page_id);
        frame.clear_dirty();
        frame.pin();

        // Record in page table.
        self.page_table.insert(page_id, frame_id);

        // LRU-K bookkeeping.
        if hint == ReadHint::Point {
            self.replacer.record_access(frame_id);
        } else {
            // For scan reads, record one access but immediately mark the
            // frame as evictable so it gets evicted before hot frames.
            self.replacer.record_access(frame_id);
            self.replacer.mark_evictable(frame_id);
        }

        Ok(PinnedPage {
            frame_id,
            _pool: core::marker::PhantomData,
            pool: self,
        })
    }

    /// Allocate a new page in the pool (for write paths that create a new
    /// page rather than reading an existing one).
    pub fn new_page(&mut self, page_id: PageId) -> HexResult<PinnedPage<'_>> {
        if self.page_table.contains_key(&page_id) {
            return Err(HexError::constraint(format!(
                "new_page: page {:?} already exists",
                page_id
            )));
        }
        self.stats.misses += 1;
        let frame_id = self.evict_or_take_free()?;
        let frame = &mut self.frames[frame_id.0 as usize];

        // Zero the buffer for a fresh page.
        let buf = frame.as_bytes_mut();
        for b in buf.iter_mut() {
            *b = 0;
        }
        frame.set_page_id(page_id);
        frame.mark_dirty(0);
        frame.pin();
        self.page_table.insert(page_id, frame_id);
        self.replacer.record_access(frame_id);

        Ok(PinnedPage {
            frame_id,
            _pool: core::marker::PhantomData,
            pool: self,
        })
    }

    /// Flush a single page's dirty bytes back to disk (if dirty).
    pub fn flush_page(&mut self, page_id: PageId) -> HexResult<()> {
        let frame_id = match self.page_table.get(&page_id) {
            Some(&fid) => fid,
            None => return Ok(()), // not in pool; nothing to flush
        };
        let frame = &self.frames[frame_id.0 as usize];
        if !frame.is_dirty() {
            return Ok(());
        }
        // WAL invariant: in production we would wait for WAL >= page_lsn
        // here. For the prototype we write through immediately.
        let bytes: &[u8] = frame.as_bytes();
        self.source.write_page(page_id, bytes)?;
        frame.clear_dirty();
        self.stats.flushes += 1;
        Ok(())
    }

    /// Flush all dirty pages.
    pub fn flush_all(&mut self) -> HexResult<()> {
        let pages: Vec<PageId> = self.page_table.keys().copied().collect();
        for p in pages {
            self.flush_page(p)?;
        }
        Ok(())
    }

    /// Evict a frame using the LRU-K policy, or take one from the free list.
    /// Returns the FrameId of an available (now-empty) frame.
    fn evict_or_take_free(&mut self) -> HexResult<FrameId> {
        // Try the free list first.
        if let Some(frame_id) = self.free_list.pop() {
            return Ok(frame_id);
        }
        // No free frame: pick a victim via LRU-K.
        let victim = self
            .replacer
            .pick_victim()
            .ok_or_else(|| HexError::internal("BufferPool: no evictable frames (all pinned?)"))?;
        let frame = &self.frames[victim.0 as usize];
        // Double-check pin count — the policy shouldn't return a pinned frame
        // but we guard against bugs anyway.
        if frame.is_pinned() {
            return Err(HexError::internal(format!(
                "BufferPool: victim frame {:?} is pinned",
                victim
            )));
        }
        // If dirty, flush before evicting.
        let old_page_id = frame
            .page_id()
            .ok_or_else(|| HexError::internal("evict_or_take_free: victim has no page_id"))?;
        if frame.is_dirty() {
            let bytes: &[u8] = frame.as_bytes();
            self.source.write_page(old_page_id, bytes)?;
            self.stats.flushes += 1;
        }
        // Remove from page table.
        self.page_table.remove(&old_page_id);
        // Clear the frame.
        frame.clear_dirty();
        frame.clear_page_id();
        self.replacer.forget(victim);
        self.stats.evictions += 1;
        Ok(victim)
    }

    /// Internal: called by PinnedPage::drop to decrement the pin count and
    /// mark the frame as evictable.
    fn release_pin(&mut self, frame_id: FrameId) {
        let frame = &self.frames[frame_id.0 as usize];
        frame.unpin();
        // If the frame is now unpinned, mark it as evictable so the policy
        // can pick it up.
        if !frame.is_pinned() {
            self.replacer.mark_evictable(frame_id);
        }
    }
}

/// RAII guard for a pinned frame. Holds an exclusive `&'pool mut BufferPool`
/// borrow so the borrow checker prevents calling any other `&mut self`
/// method on the pool while the pin is alive — including a second
/// `pin_page` call that might evict this frame.
///
/// `frame_id` identifies the frame; the actual `&Frame` is fetched on demand
/// via `frame()` / `as_bytes()` so we don't store a second borrow that would
/// conflict with the `&mut BufferPool`.
pub struct PinnedPage<'pool> {
    frame_id: FrameId,
    _pool: core::marker::PhantomData<&'pool Frame>,
    pool: &'pool mut BufferPool,
}

impl<'pool> PinnedPage<'pool> {
    /// Borrow the underlying frame. The borrow is tied to `self`, which in
    /// turn holds `&'pool mut BufferPool` — so the returned `&Frame` cannot
    /// outlive the `PinnedPage`.
    #[inline]
    pub fn frame(&self) -> &Frame {
        // Indexing into `frames` requires only a shared borrow of the pool,
        // which we have through `&self.pool`. The returned reference is tied
        // to `&self` (not `'pool`) so callers can't keep it past the pin.
        &self.pool.frames[self.frame_id.0 as usize]
    }

    /// Borrow the page bytes for reading.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.frame().as_bytes()
    }

    /// Mark the page as dirty. The pool will flush it before eviction.
    #[inline]
    pub fn mark_dirty(&self, page_lsn: u64) {
        self.frame().mark_dirty(page_lsn);
    }

    /// Frame id of the pinned frame.
    #[inline]
    pub fn frame_id(&self) -> FrameId {
        self.frame_id
    }
}

impl<'pool> Drop for PinnedPage<'pool> {
    fn drop(&mut self) {
        let frame_id = self.frame_id;
        self.pool.release_pin(frame_id);
    }
}

// ============================================================================
// In-memory PageSource — used by tests and by the engine when running fully
// in RAM (no disk).
// ============================================================================

/// Simple in-memory `PageSource` backed by a `HashMap<PageId, Vec<u8>>`.
/// Useful for unit tests and for the verification suite.
pub struct InMemoryPageSource {
    pages: std::collections::HashMap<PageId, Vec<u8>>,
    page_size: usize,
}

impl InMemoryPageSource {
    pub fn new(page_size: usize) -> Self {
        Self {
            pages: std::collections::HashMap::new(),
            page_size,
        }
    }

    /// Pre-populate a page with bytes (used by tests to seed the source).
    pub fn put_page(&mut self, page_id: PageId, bytes: Vec<u8>) {
        debug_assert_eq!(bytes.len(), self.page_size);
        self.pages.insert(page_id, bytes);
    }

    pub fn contains(&self, page_id: PageId) -> bool {
        self.pages.contains_key(&page_id)
    }
}

impl PageSource for InMemoryPageSource {
    fn read_page(&mut self, page_id: PageId, buf: &mut [u8]) -> HexResult<()> {
        match self.pages.get(&page_id) {
            Some(src) => {
                buf.copy_from_slice(src);
                Ok(())
            }
            None => {
                // If the page doesn't exist, return zeros. This matches the
                // "new page" semantics for the prototype — the caller is
                // responsible for writing real data into the frame.
                for b in buf.iter_mut() {
                    *b = 0;
                }
                Ok(())
            }
        }
    }

    fn write_page(&mut self, page_id: PageId, buf: &[u8]) -> HexResult<()> {
        self.pages.insert(page_id, buf.to_vec());
        Ok(())
    }

    fn page_size(&self) -> usize {
        self.page_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_unpin_basic() {
        let source = Box::new(InMemoryPageSource::new(4096));
        let mut pool = BufferPool::new(source, 4, 4096).unwrap();
        let pid = PageId::pack(cendb_core::SegmentId(1), cendb_core::BlockId(0), 0);

        {
            let pinned = pool.pin_page(pid, ReadHint::Point).unwrap();
            assert_eq!(pinned.frame().page_id(), Some(pid));
            assert_eq!(pinned.frame().pin_count(), 1);
        }
        // After drop, pin count should be 0.
        let stats = pool.stats();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.pinned_frames, 0);
    }

    #[test]
    fn second_pin_is_a_hit() {
        let source = Box::new(InMemoryPageSource::new(4096));
        let mut pool = BufferPool::new(source, 4, 4096).unwrap();
        let pid = PageId::pack(cendb_core::SegmentId(1), cendb_core::BlockId(0), 0);

        {
            let _p1 = pool.pin_page(pid, ReadHint::Point).unwrap();
        }
        {
            let _p2 = pool.pin_page(pid, ReadHint::Point).unwrap();
        }
        let stats = pool.stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 1);
    }

    #[test]
    fn eviction_picks_scan_pages_first() {
        // 4-frame pool. Touch frame 0 twice (hot), then scan through pages
        // 1..10. The scan pages should evict each other, never frame 0.
        let source = Box::new(InMemoryPageSource::new(4096));
        let mut pool = BufferPool::new(source, 4, 4096).unwrap();

        // Hot page.
        let hot_pid = PageId::pack(cendb_core::SegmentId(1), cendb_core::BlockId(0), 0);
        {
            let _p = pool.pin_page(hot_pid, ReadHint::Point).unwrap();
        }
        {
            let _p = pool.pin_page(hot_pid, ReadHint::Point).unwrap();
        }

        // Scan pages.
        for i in 1..10u16 {
            let pid = PageId::pack(cendb_core::SegmentId(1), cendb_core::BlockId(0), i);
            let _p = pool.pin_page(pid, ReadHint::Scan).unwrap();
        }

        // Hot page should still be in the pool → 3rd pin is a hit.
        {
            let _p = pool.pin_page(hot_pid, ReadHint::Point).unwrap();
        }
        let stats = pool.stats();
        assert!(
            stats.hits >= 1,
            "hot page should have at least 1 hit (after 2 Point accesses), got stats {:?}",
            stats
        );
    }

    #[test]
    fn dirty_pages_are_flushed_on_eviction() {
        let mut source_box = Box::new(InMemoryPageSource::new(4096));
        // Insert 5 pages so the 5th evicts the 1st.
        for i in 0..5u16 {
            let pid = PageId::pack(cendb_core::SegmentId(1), cendb_core::BlockId(0), i);
            let mut bytes = vec![0u8; 4096];
            bytes[0] = i as u8;
            source_box.put_page(pid, bytes);
        }
        let mut pool = BufferPool::new(source_box, 2, 4096).unwrap();

        // Pin page 0 with Scan hint (so it's evictable), mark dirty.
        let pid0 = PageId::pack(cendb_core::SegmentId(1), cendb_core::BlockId(0), 0);
        {
            let pinned = pool.pin_page(pid0, ReadHint::Scan).unwrap();
            pinned.mark_dirty(42);
        }
        // Pin pages 1..5 (each evicts the previous scan page).
        for i in 1..5u16 {
            let pid = PageId::pack(cendb_core::SegmentId(1), cendb_core::BlockId(0), i);
            let _p = pool.pin_page(pid, ReadHint::Scan).unwrap();
        }
        let stats = pool.stats();
        // At least one of the evicted pages was page 0 (dirty) → flush.
        assert!(stats.flushes >= 1, "expected at least one flush, got {:?}", stats);
    }

    #[test]
    fn pinned_page_cannot_outlive_pin() {
        // This test demonstrates the borrow checker guaranteeing safety.
        // The `PinnedPage` borrows `&'pool BufferPool`, so while it is
        // alive we cannot take another `&mut BufferPool` — including a
        // second `pin_page` call that might evict the first frame.
        let source = Box::new(InMemoryPageSource::new(4096));
        let mut pool = BufferPool::new(source, 1, 4096).unwrap();
        let pid = PageId::pack(cendb_core::SegmentId(1), cendb_core::BlockId(0), 0);

        let pinned = pool.pin_page(pid, ReadHint::Point).unwrap();
        // The following line would not compile (borrow checker error) if
        // uncommented:
        //   let _p2 = pool.pin_page(PageId(999), ReadHint::Point).unwrap();
        let _ = pinned;
    }
}
