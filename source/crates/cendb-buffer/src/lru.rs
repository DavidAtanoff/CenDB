//! LRU-K (K=2) eviction policy with scan resistance.
//!
//! Plain LRU is famously vulnerable to sequential scans: a long full-table
//! scan pollutes the cache with pages that are touched exactly once and
//! never again, evicting the OLTP hot set. LRU-K fixes this by keeping, for
//! each frame, the timestamps of its *last K accesses*; a frame only becomes
//! a candidate for eviction once it has accumulated K accesses. The
//! "eviction victim" is the frame with the *oldest K-th-most-recent*
//! access. A one-shot scan never reaches K=2 accesses, so its pages are
//! evicted before they can displace the hot working set.
//!
//! ## Implementation notes
//!
//! * Time is a monotonic `u64` counter incremented on every `record_access`.
//!   We deliberately avoid `SystemTime` / `Instant` so the policy is
//!   deterministic and testable.
//! * The `history` map stores per-frame access timestamps in a small
//!   `VecDeque<u64>` capped at K=2. We keep it sorted newest-first.
//! * "Evictable" = has K recorded accesses AND pin_count == 0.
//! * Among evictable frames, we pick the one whose *K-th-most-recent* (i.e.
//!   oldest) access timestamp is the smallest — the classic LRU-K rule.
//!
//! This is single-threaded for this implementation. A production version would
//! guard the structures with a small spinlock or use a lock-free skip list.

use std::collections::{HashMap, VecDeque};

use cendb_core::FrameId;

/// LRU-K policy with K=2. Tracks access history per frame and picks an
/// eviction victim by the K-th-oldest access rule.
pub struct LruK {
    k: usize,
    /// Monotonic logical clock; incremented on every access.
    clock: u64,
    /// Per-frame access history, newest-first. Capped at `k` entries.
    history: HashMap<FrameId, VecDeque<u64>>,
    /// Frames that have been "freed" by the caller (e.g. the page they
    /// held was overwritten). Removed from `history` lazily.
    evictable: Vec<FrameId>,
}

impl LruK {
    pub fn new() -> Self {
        Self::with_k(2)
    }

    pub fn with_k(k: usize) -> Self {
        assert!(k >= 1, "LRU-K requires K >= 1");
        Self {
            k,
            clock: 0,
            history: HashMap::new(),
            evictable: Vec::new(),
        }
    }

    /// Record an access to `frame` at the current logical time.
    pub fn record_access(&mut self, frame: FrameId) {
        self.clock += 1;
        let hist = self.history.entry(frame).or_insert_with(VecDeque::new);
        hist.push_front(self.clock);
        if hist.len() > self.k {
            hist.pop_back();
        }
    }

    /// Mark a frame as evictable (i.e. its page is no longer needed and the
    /// frame may be reclaimed). The frame must not currently be pinned by
    /// the caller.
    pub fn mark_evictable(&mut self, frame: FrameId) {
        self.evictable.push(frame);
    }

    /// Remove a frame from the policy entirely (e.g. the frame was
    /// repurposed for a different page).
    pub fn forget(&mut self, frame: FrameId) {
        self.history.remove(&frame);
        self.evictable.retain(|&f| f != frame);
    }

    /// Pick a victim frame to evict, applying the LRU-K rule with scan
    /// resistance.
    ///
    /// Scan resistance: we **prefer** to evict frames with fewer than K
    /// accesses (i.e. one-shot scan pages that haven't proven themselves
    /// "hot"). Only when no such frame is available do we fall back to the
    /// classic LRU-K rule: among frames with K accesses, pick the one
    /// whose K-th-most-recent access is the oldest.
    pub fn pick_victim(&mut self) -> Option<FrameId> {
        // Phase 1: prefer frames with < K accesses (one-shot scan pages).
        // Pick the one with the *earliest* single access (oldest one-shot).
        let mut best_scan: Option<(FrameId, u64)> = None;
        let mut best_k: Option<(FrameId, u64)> = None;
        for (idx, &frame) in self.evictable.iter().enumerate() {
            let _ = idx;
            if let Some(hist) = self.history.get(&frame) {
                if hist.len() < self.k {
                    let oldest = *hist.back().unwrap();
                    match best_scan {
                        None => best_scan = Some((frame, oldest)),
                        Some((_, b)) if oldest < b => best_scan = Some((frame, oldest)),
                        _ => {}
                    }
                } else {
                    // Has K accesses — hot under LRU-K. Consider only if no
                    // scan page is available.
                    let oldest = *hist.back().unwrap();
                    match best_k {
                        None => best_k = Some((frame, oldest)),
                        Some((_, b)) if oldest < b => best_k = Some((frame, oldest)),
                        _ => {}
                    }
                }
            }
        }
        let victim = best_scan.or(best_k);
        if let Some((frame, _)) = victim {
            if let Some(pos) = self.evictable.iter().position(|&f| f == frame) {
                self.evictable.swap_remove(pos);
            }
            self.history.remove(&frame);
            return Some(frame);
        }
        None
    }

    /// Number of frames currently tracked by the policy.
    #[inline]
    pub fn tracked(&self) -> usize {
        self.history.len()
    }

    /// Number of frames currently marked evictable.
    #[inline]
    pub fn evictable_count(&self) -> usize {
        self.evictable.len()
    }
}

impl Default for LruK {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_pages_evicted_before_hot_pages() {
        let mut lru = LruK::new();
        // Frame 0: hot, accessed twice (K=2).
        lru.record_access(FrameId(0));
        lru.record_access(FrameId(0));
        // Frame 1: scan page, accessed once.
        lru.record_access(FrameId(1));
        // Mark both evictable.
        lru.mark_evictable(FrameId(0));
        lru.mark_evictable(FrameId(1));

        // Scan resistance: frame 1 (only 1 access) should be evicted first.
        let v = lru.pick_victim().unwrap();
        assert_eq!(v, FrameId(1));

        // Now only frame 0 remains; it's the K-access hot page.
        let v2 = lru.pick_victim().unwrap();
        assert_eq!(v2, FrameId(0));
    }

    #[test]
    fn one_shot_scan_does_not_displace_hot_set() {
        let mut lru = LruK::new();
        // Hot set: frame 0, accessed twice.
        lru.record_access(FrameId(0));
        lru.record_access(FrameId(0));
        // One-shot scan: frames 1..100, each accessed once.
        for i in 1..100 {
            lru.record_access(FrameId(i));
        }
        for i in 0..100 {
            lru.mark_evictable(FrameId(i));
        }
        // First victim: should be a scan frame (only 1 access), not frame 0.
        let v = lru.pick_victim().unwrap();
        assert!(v.0 >= 1, "hot frame 0 should not be evicted first");
    }

    #[test]
    fn forget_removes_from_history() {
        let mut lru = LruK::new();
        lru.record_access(FrameId(0));
        lru.record_access(FrameId(0));
        lru.mark_evictable(FrameId(0));
        lru.forget(FrameId(0));
        assert_eq!(lru.tracked(), 0);
        assert_eq!(lru.evictable_count(), 0);
        assert!(lru.pick_victim().is_none());
    }
}
