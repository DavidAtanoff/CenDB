//! Frame: the in-memory slot that holds one page worth of bytes.
//!
//! Each frame is a fixed `page_size` byte buffer, 64-byte aligned so that
//! `ColumnView` can hand out `&[i64]` / `&[f64]` slices over the frame's
//! bytes without violating alignment invariants.
//!
//! Concurrency model: every mutable field is atomic. Pin/unpin operations
//! hit `pin_count` with `fetch_add`/`fetch_sub` and never take a lock. The
//! buffer pool's eviction path uses `compare_exchange` on `pin_count` to
//! skip frames that are currently borrowed.

use cendb_core::{FrameId, PageId};
use cendb_storage::pax::AlignedBlock;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// A single buffer-pool frame. Sized to one page and 64-byte aligned.
pub struct Frame {
    /// The bytes of the page currently cached in this frame. Owned by the
    /// frame; never re-allocated (the size is fixed for the pool's lifetime).
    data: AlignedBlock,
    /// The page id currently cached in `data`. `INVALID_PAGE_ID` if the
    /// frame is empty (freshly allocated, never filled).
    page_id: AtomicU64,
    /// Number of outstanding `PinnedPage` guards borrowing this frame.
    /// Pinned frames cannot be evicted. The eviction path spins on
    /// `compare_exchange(0, 0)` to atomically observe "still unpinned".
    pin_count: AtomicU32,
    /// True iff the page has been modified since it was loaded from disk
    /// and must be written back before eviction.
    dirty: AtomicBool,
    /// WAL LSN at the moment the page was last dirtied. The flush path
    /// must not write this page to disk until the WAL has durably
    /// persisted up to at least this LSN (the WAL invariant — see §2.2.2).
    page_lsn: AtomicU64,
    /// Frame id of this frame inside the pool slab. Cached here so a
    /// `&Frame` can be cheaply mapped back to its `FrameId` without a
    /// reverse lookup.
    frame_id: FrameId,
}

/// Sentinel value stored in `Frame::page_id` when the frame has no page.
pub const INVALID_PAGE_ID: u64 = u64::MAX;

impl Frame {
    /// Allocate a fresh frame with no page cached. The underlying buffer is
    /// zeroed; reads will see zeros until the buffer pool fills it with real
    /// page bytes from disk.
    pub fn new(frame_id: FrameId, page_size: usize) -> cendb_core::CenResult<Self> {
        let data = AlignedBlock::zeroed(page_size)?;
        Ok(Self {
            data,
            page_id: AtomicU64::new(INVALID_PAGE_ID),
            pin_count: AtomicU32::new(0),
            dirty: AtomicBool::new(false),
            page_lsn: AtomicU64::new(0),
            frame_id,
        })
    }

    /// The page id currently cached in this frame, or `None` if empty.
    #[inline]
    pub fn page_id(&self) -> Option<PageId> {
        let raw = self.page_id.load(Ordering::Acquire);
        if raw == INVALID_PAGE_ID {
            None
        } else {
            Some(PageId(raw))
        }
    }

    /// Set the cached page id. Used by the buffer pool when it loads a new
    /// page into a frame.
    #[inline]
    pub fn set_page_id(&self, page_id: PageId) {
        self.page_id.store(page_id.0, Ordering::Release);
    }

    /// Clear the cached page id (called after eviction).
    #[inline]
    pub fn clear_page_id(&self) {
        self.page_id.store(INVALID_PAGE_ID, Ordering::Release);
    }

    /// Current pin count. Pinned frames cannot be evicted.
    #[inline]
    pub fn pin_count(&self) -> u32 {
        self.pin_count.load(Ordering::Acquire)
    }

    /// Increment the pin count. Returns the previous value.
    #[inline]
    pub fn pin(&self) -> u32 {
        self.pin_count.fetch_add(1, Ordering::AcqRel)
    }

    /// Decrement the pin count. Returns the previous value.
    ///
    /// # Panics (debug)
    /// Debug builds assert that the pin count does not underflow.
    #[inline]
    pub fn unpin(&self) -> u32 {
        let prev = self.pin_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            prev > 0,
            "Frame::unpin underflow on frame {:?} (page {:?})",
            self.frame_id,
            self.page_id()
        );
        prev
    }

    /// True iff this frame has outstanding pins and must not be evicted.
    #[inline]
    pub fn is_pinned(&self) -> bool {
        self.pin_count() > 0
    }

    /// Borrow the frame's bytes for reading. Lifetime is tied to `&self`.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.data.as_slice()
    }

    /// Borrow the frame's bytes for writing. The caller is responsible for
    /// setting `dirty = true` afterwards.
    #[inline]
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        self.data.as_mut_slice()
    }

    /// Mark the page as modified. `page_lsn` is the WAL LSN of the
    /// modifying record; the flush path will defer the writeback until the
    /// WAL has persisted up to this LSN.
    #[inline]
    pub fn mark_dirty(&self, page_lsn: u64) {
        self.dirty.store(true, Ordering::Release);
        let prev = self.page_lsn.load(Ordering::Acquire);
        if page_lsn > prev {
            self.page_lsn.store(page_lsn, Ordering::Release);
        }
    }

    /// Clear the dirty flag (after a successful flush).
    #[inline]
    pub fn clear_dirty(&self) {
        self.dirty.store(false, Ordering::Release);
    }

    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    #[inline]
    pub fn page_lsn(&self) -> u64 {
        self.page_lsn.load(Ordering::Acquire)
    }

    #[inline]
    pub fn frame_id(&self) -> FrameId {
        self.frame_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_lifecycle() {
        let mut f = Frame::new(FrameId(0), 4096).unwrap();
        assert!(f.page_id().is_none());
        assert_eq!(f.pin_count(), 0);
        assert!(!f.is_dirty());

        f.set_page_id(PageId(42));
        assert_eq!(f.page_id(), Some(PageId(42)));

        f.pin();
        f.pin();
        assert_eq!(f.pin_count(), 2);
        assert!(f.is_pinned());

        f.unpin();
        f.unpin();
        assert_eq!(f.pin_count(), 0);

        f.mark_dirty(100);
        assert!(f.is_dirty());
        assert_eq!(f.page_lsn(), 100);
        f.clear_dirty();
        assert!(!f.is_dirty());

        // Mutate bytes.
        let bytes = f.as_bytes_mut();
        bytes[0] = 0xAB;
        bytes[1] = 0xCD;
        assert_eq!(f.as_bytes()[0], 0xAB);
        assert_eq!(f.as_bytes()[1], 0xCD);
    }
}
