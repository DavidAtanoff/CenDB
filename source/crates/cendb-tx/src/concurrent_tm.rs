//! Concurrent TransactionManager — thread-safe MVCC + OCC.
//!
//! ## Design
//!
//! The original `TransactionManager` takes `&mut self` on every call,
//! forcing callers to wrap it in a `Mutex`. This module provides a
//! natively thread-safe alternative using interior mutability:
//!
//!   * **Timestamp oracle** — `AtomicU64`, wait-free (unchanged).
//!   * **Transaction table** — `RwLock<HashMap>`. Reads (visibility
//!     checks, `is_committed`, `read_ts`) take a read lock; writes
//!     (`begin`, `record_write`, `commit`, `abort`) take a write lock
//!     on the specific txn entry. The table itself uses `RwLock` so
//!     multiple readers can traverse it concurrently.
//!   * **Committed timestamps** — `RwLock<HashSet>`. The visibility
//!     check `is_committed(ts)` is on the hot read path; using a
//!     `RwLock` lets multiple readers check concurrently.
//!   * **Latest writes (OCC validation)** — `RwLock<HashMap>`. Read
//!     during commit validation; written on commit.
//!
//! ## Contention model
//!
//! The write lock on the txn table is held only for the duration of
//! the individual operation (begin, record_write, commit, abort) —
//! NOT across the entire transaction lifetime. A transaction is
//! "active" between calls, holding no lock. This means:
//!
//!   * N concurrent transactions can all be active simultaneously.
//!   * The lock is contended only on the TM bookkeeping operations,
//!     each of which is O(1) or O(write_set_size).
//!   * OCC validation at commit is the only potentially slow path,
//!     because it iterates the write set. For small write sets (<100
//!     keys) this is sub-microsecond.
//!
//! ## MVCC garbage collection
//!
//! `ConcurrentTransactionManager` wires in `MvccGarbageCollector`.
//! On every `commit`, after publishing the commit timestamp, the TM
//! checks whether enough versions have accumulated and runs GC if
//! needed. GC removes version-chain entries whose `end_ts` is older
//! than the minimum active `read_ts`.

use crate::mvcc::{
    IsolationLevel, MvccError, MvccResult, MvccGarbageCollector, TimestampOracle,
    Transaction, TransactionState, VersionHeader,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicU64, Ordering};

/// Thread-safe transaction manager. All methods take `&self`.
pub struct ConcurrentTransactionManager {
    oracle: TimestampOracle,
    /// Txn table: txn_id → Transaction. RwLock so multiple readers
    /// (visibility checks) can traverse concurrently.
    txns: RwLock<HashMap<u64, Transaction>>,
    /// Set of committed timestamps. RwLock for concurrent reads.
    committed_ts: RwLock<HashSet<u64>>,
    /// Key → latest committed begin_ts that wrote it. Used by OCC.
    latest_writes: RwLock<HashMap<Vec<u8>, u64>>,
    next_txn_id: AtomicU64,
    /// MVCC garbage collector. Runs periodically on commit.
    gc: RwLock<MvccGarbageCollector>,
    /// Number of commits since last GC run.
    commits_since_gc: AtomicU64,
    /// GC interval (commits between runs).
    gc_interval: u64,
    /// Optional GC callback: called for each version collected.
    /// The callback receives (key, end_ts) so the storage layer can
    /// physically remove the version.
    gc_callback: RwLock<Option<Box<dyn Fn(&[u8], u64) + Send + Sync>>>,
}

// Manual Clone-like impl — we can't derive Clone because of the callback.
impl Clone for ConcurrentTransactionManager {
    fn clone(&self) -> Self {
        Self {
            oracle: TimestampOracle::new(self.oracle.current()),
            txns: RwLock::new(self.txns.read().unwrap().clone()),
            committed_ts: RwLock::new(self.committed_ts.read().unwrap().clone()),
            latest_writes: RwLock::new(self.latest_writes.read().unwrap().clone()),
            next_txn_id: AtomicU64::new(self.next_txn_id.load(Ordering::Relaxed)),
            gc: RwLock::new(MvccGarbageCollector::new()),
            commits_since_gc: AtomicU64::new(0),
            gc_interval: self.gc_interval,
            gc_callback: RwLock::new(None),
        }
    }
}

impl ConcurrentTransactionManager {
    /// Create a new concurrent TM with default GC interval (100 commits).
    pub fn new() -> Self {
        Self::with_gc_interval(100)
    }

    /// Create with a custom GC interval.
    pub fn with_gc_interval(gc_interval: u64) -> Self {
        Self {
            oracle: TimestampOracle::new(1),
            txns: RwLock::new(HashMap::new()),
            committed_ts: RwLock::new(HashSet::new()),
            latest_writes: RwLock::new(HashMap::new()),
            next_txn_id: AtomicU64::new(1),
            gc: RwLock::new(MvccGarbageCollector::new()),
            commits_since_gc: AtomicU64::new(0),
            gc_interval,
            gc_callback: RwLock::new(None),
        }
    }

    /// Set a GC callback. Called for each version that is safe to
    /// physically remove (its `end_ts` < min active read_ts).
    pub fn set_gc_callback<F>(&self, callback: F)
    where
        F: Fn(&[u8], u64) + Send + Sync + 'static,
    {
        *self.gc_callback.write().unwrap() = Some(Box::new(callback));
    }

    /// Begin a new transaction. Returns the txn_id.
    pub fn begin(&self, isolation: IsolationLevel) -> u64 {
        let txn_id = self.next_txn_id.fetch_add(1, Ordering::AcqRel);
        let read_ts = self.oracle.current();
        let txn = Transaction::new(txn_id, read_ts, isolation);
        self.txns.write().unwrap().insert(txn_id, txn);
        txn_id
    }

    /// Record a write in the transaction's write set.
    pub fn record_write(&self, txn_id: u64, key: &[u8]) -> MvccResult<()> {
        let mut txns = self.txns.write().unwrap();
        let txn = txns.get_mut(&txn_id).ok_or(MvccError::NotFound)?;
        if txn.state != TransactionState::Active {
            return Err(MvccError::Aborted);
        }
        txn.write_set.push(key.to_vec());
        Ok(())
    }

    /// Commit a transaction. Performs OCC validation under locks.
    ///
    /// Lock ordering: latest_writes (write) → txns (write) →
    /// committed_ts (write). This is a consistent global order that
    /// prevents deadlock.
    ///
    /// BUG FIX: The original implementation validated under a read lock
    /// on latest_writes, then dropped it, then took a write lock later.
    /// This created a TOCTOU race where another transaction could commit
    /// a conflicting write between validation and publication. The fix
    /// is to hold the latest_writes write lock for the entire
    /// validate-and-publish critical section.
    pub fn commit(&self, txn_id: u64) -> MvccResult<u64> {
        // Phase 1: read the txn's write_set and read_ts under a read lock.
        let (read_ts, write_set, state) = {
            let txns = self.txns.read().unwrap();
            let txn = txns.get(&txn_id).ok_or(MvccError::NotFound)?;
            (txn.read_ts, txn.write_set.clone(), txn.state)
        };
        match state {
            TransactionState::Committed => return Err(MvccError::AlreadyCommitted),
            TransactionState::Aborted => return Err(MvccError::AlreadyAborted),
            TransactionState::Active | TransactionState::Validation => {}
        }

        // Phase 2: acquire latest_writes write lock and hold it for the
        // entire validate-and-publish critical section. This prevents
        // TOCTOU races where another transaction commits a conflicting
        // write between validation and publication.
        let mut latest_writes = self.latest_writes.write().unwrap();

        // OCC validation: check write-write conflicts.
        for key in &write_set {
            if let Some(&latest) = latest_writes.get(key) {
                if latest > read_ts {
                    // Conflict: abort.
                    drop(latest_writes);
                    let mut txns = self.txns.write().unwrap();
                    if let Some(txn) = txns.get_mut(&txn_id) {
                        txn.state = TransactionState::Aborted;
                    }
                    return Err(MvccError::Conflict);
                }
            }
        }

        // Phase 3: allocate commit_ts and publish.
        let commit_ts = self.oracle.next();

        // Update txn state.
        {
            let mut txns = self.txns.write().unwrap();
            let txn = txns.get_mut(&txn_id).ok_or(MvccError::NotFound)?;
            // Re-check state (could have been aborted by a concurrent commit).
            if txn.state == TransactionState::Aborted {
                drop(latest_writes);
                return Err(MvccError::Aborted);
            }
            txn.commit_ts = Some(commit_ts);
            txn.state = TransactionState::Committed;
        }
        {
            let mut committed_ts = self.committed_ts.write().unwrap();
            committed_ts.insert(commit_ts);
        }
        // Update latest_writes (we still hold the write lock).
        let mut superseded: Vec<(Vec<u8>, u64)> = Vec::new();
        for key in &write_set {
            if let Some(old_ts) = latest_writes.insert(key.clone(), commit_ts) {
                if old_ts < commit_ts {
                    superseded.push((key.clone(), old_ts));
                }
            }
        }
        drop(latest_writes);
        // Notify GC callback for superseded versions (the old version
        // is no longer the latest — the storage layer can reclaim it
        // once it's not visible to any active txn).
        if !superseded.is_empty() {
            let callback = self.gc_callback.read().unwrap();
            if let Some(cb) = callback.as_ref() {
                // Compute min active read_ts directly (not from GC
                // struct, which may be stale).
                let min_active = {
                    let txns = self.txns.read().unwrap();
                    txns.values()
                        .filter(|t| t.state == TransactionState::Active)
                        .map(|t| t.read_ts)
                        .min()
                        .unwrap_or_else(|| self.oracle.current())
                };
                for (key, old_ts) in &superseded {
                    if *old_ts < min_active {
                        cb(key, *old_ts);
                    }
                }
            }
        }

        // Phase 4: periodic GC.
        let count = self.commits_since_gc.fetch_add(1, Ordering::AcqRel) + 1;
        if count >= self.gc_interval {
            self.run_gc();
            self.commits_since_gc.store(0, Ordering::Release);
        }

        Ok(commit_ts)
    }

    /// Abort a transaction.
    pub fn abort(&self, txn_id: u64) -> MvccResult<()> {
        let mut txns = self.txns.write().unwrap();
        let txn = txns.get_mut(&txn_id).ok_or(MvccError::NotFound)?;
        match txn.state {
            TransactionState::Committed => return Err(MvccError::AlreadyCommitted),
            TransactionState::Aborted => return Err(MvccError::AlreadyAborted),
            _ => {}
        }
        txn.state = TransactionState::Aborted;
        Ok(())
    }

    /// Check whether a timestamp corresponds to a committed txn.
    /// Hot read path — takes only a read lock.
    pub fn is_committed(&self, ts: u64) -> bool {
        self.committed_ts.read().unwrap().contains(&ts)
    }

    /// Get the read timestamp for a transaction.
    pub fn read_ts(&self, txn_id: u64) -> Option<u64> {
        self.txns.read().unwrap().get(&txn_id).map(|t| t.read_ts)
    }

    /// Get the commit timestamp (if committed) for a transaction.
    pub fn commit_ts(&self, txn_id: u64) -> Option<u64> {
        self.txns.read().unwrap().get(&txn_id).and_then(|t| t.commit_ts)
    }

    /// Current timestamp (does not advance).
    pub fn current_ts(&self) -> u64 {
        self.oracle.current()
    }

    /// Number of active transactions.
    pub fn active_count(&self) -> usize {
        self.txns.read().unwrap().values()
            .filter(|t| t.state == TransactionState::Active)
            .count()
    }

    /// Run MVCC garbage collection. Removes entries from
    /// `latest_writes` and `committed_ts` that are no longer needed,
    /// and invokes the GC callback for each version that can be
    /// physically removed from storage.
    ///
    /// Safe to call from any thread. Acquires write locks.
    pub fn run_gc(&self) -> usize {
        // Compute min active read_ts.
        let min_active_ts = {
            let txns = self.txns.read().unwrap();
            txns.values()
                .filter(|t| t.state == TransactionState::Active)
                .map(|t| t.read_ts)
                .min()
                .unwrap_or_else(|| self.oracle.current())
        };
        {
            let mut gc = self.gc.write().unwrap();
            gc.set_min_active_ts(min_active_ts);
        }

        let mut collected = 0usize;

        // Clean up latest_writes: remove entries with commit_ts < min_active_ts.
        // These entries are for versions that are no longer the "latest" for
        // their key (they've been superseded) AND the superseding commit is
        // visible to all active txns.
        let keys_to_remove: Vec<Vec<u8>> = {
            let latest_writes = self.latest_writes.read().unwrap();
            latest_writes.iter()
                .filter(|(_, &ts)| ts < min_active_ts)
                .map(|(k, _)| k.clone())
                .collect()
        };
        if !keys_to_remove.is_empty() {
            let mut latest_writes = self.latest_writes.write().unwrap();
            let callback = self.gc_callback.read().unwrap();
            for key in &keys_to_remove {
                if let Some(ts) = latest_writes.remove(key) {
                    collected += 1;
                    if let Some(cb) = callback.as_ref() {
                        cb(key, ts);
                    }
                }
            }
        }

        // Clean up committed_ts: remove timestamps < min_active_ts.
        // These are no longer needed for visibility checks (no active
        // txn has a read_ts old enough to need them).
        {
            let mut committed_ts = self.committed_ts.write().unwrap();
            let old: Vec<u64> = committed_ts.iter()
                .filter(|&&ts| ts < min_active_ts)
                .copied()
                .collect();
            for ts in old {
                committed_ts.remove(&ts);
            }
        }

        // Clean up txn table: remove committed/aborted txns that are
        // older than min_active_ts (their read_ts < min_active_ts means
        // no active txn can reference them).
        {
            let mut txns = self.txns.write().unwrap();
            let to_remove: Vec<u64> = txns.iter()
                .filter(|(_, t)| {
                    t.state != TransactionState::Active && t.read_ts < min_active_ts
                })
                .map(|(id, _)| *id)
                .collect();
            for id in to_remove {
                txns.remove(&id);
            }
        }

        {
            let mut gc = self.gc.write().unwrap();
            for _ in 0..collected {
                gc.record_gc();
            }
        }

        collected
    }

    /// Total versions collected by GC.
    pub fn gc_versions_collected(&self) -> u64 {
        self.gc.read().unwrap().versions_collected()
    }

    /// Current min active timestamp (for visibility checks).
    pub fn min_active_ts(&self) -> u64 {
        self.gc.read().unwrap().min_active_ts()
    }
}

impl Default for ConcurrentTransactionManager {
    fn default() -> Self { Self::new() }
}

// ============================================================================
// Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn concurrent_tm_basic_begin_commit() {
        let tm = ConcurrentTransactionManager::new();
        let txn = tm.begin(IsolationLevel::Snapshot);
        tm.record_write(txn, b"key1").unwrap();
        let commit_ts = tm.commit(txn).unwrap();
        assert!(commit_ts > 0);
        assert!(tm.is_committed(commit_ts));
    }

    #[test]
    fn concurrent_tm_occ_detects_conflict() {
        let tm = ConcurrentTransactionManager::new();
        // T1 writes "x", commits.
        let t1 = tm.begin(IsolationLevel::Snapshot);
        tm.record_write(t1, b"x").unwrap();
        tm.commit(t1).unwrap();
        // T2 and T3 start at same read_ts.
        let t2 = tm.begin(IsolationLevel::Snapshot);
        let t3 = tm.begin(IsolationLevel::Snapshot);
        // Both write "y".
        tm.record_write(t2, b"y").unwrap();
        tm.record_write(t3, b"y").unwrap();
        // T2 commits first (success).
        tm.commit(t2).unwrap();
        // T3 must abort (conflict).
        let result = tm.commit(t3);
        assert!(matches!(result, Err(MvccError::Conflict)));
    }

    #[test]
    fn concurrent_tm_8_threads_no_lost_updates() {
        let tm = Arc::new(ConcurrentTransactionManager::new());
        let mut handles = vec![];
        let success_count = Arc::new(AtomicU64::new(0));
        let abort_count = Arc::new(AtomicU64::new(0));

        for _ in 0..8 {
            let tm = Arc::clone(&tm);
            let sc = Arc::clone(&success_count);
            let ac = Arc::clone(&abort_count);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let txn = tm.begin(IsolationLevel::Snapshot);
                    tm.record_write(txn, b"hot_key").unwrap();
                    match tm.commit(txn) {
                        Ok(_) => { sc.fetch_add(1, Ordering::Relaxed); }
                        Err(MvccError::Conflict) => { ac.fetch_add(1, Ordering::Relaxed); }
                        Err(e) => panic!("unexpected error: {:?}", e),
                    }
                }
            }));
        }
        for h in handles { h.join().unwrap(); }

        let successes = success_count.load(Ordering::Relaxed);
        let aborts = abort_count.load(Ordering::Relaxed);
        // All 800 attempts accounted for.
        assert_eq!(successes + aborts, 800);
        // At least some should abort under contention (8 threads on 1 key).
        assert!(aborts > 0, "expected some aborts under contention, got 0");
    }

    #[test]
    fn concurrent_tm_16_threads_independent_keys() {
        let tm = Arc::new(ConcurrentTransactionManager::new());
        let mut handles = vec![];
        for t in 0..16u64 {
            let tm = Arc::clone(&tm);
            handles.push(thread::spawn(move || {
                for i in 0..100u64 {
                    let txn = tm.begin(IsolationLevel::Snapshot);
                    let key = format!("t{}_k{}", t, i);
                    tm.record_write(txn, key.as_bytes()).unwrap();
                    tm.commit(txn).unwrap(); // no conflict (independent keys)
                }
            }));
        }
        for h in handles { h.join().unwrap(); }
        // 16 threads × 100 txns = 1600 committed.
        assert_eq!(tm.active_count(), 0);
    }

    #[test]
    fn concurrent_tm_gc_runs_on_commit() {
        let tm = ConcurrentTransactionManager::with_gc_interval(10);
        // Commit 20 transactions.
        for i in 0..20u64 {
            let txn = tm.begin(IsolationLevel::Snapshot);
            tm.record_write(txn, format!("k{}", i).as_bytes()).unwrap();
            tm.commit(txn).unwrap();
        }
        // GC should have run at least once (at commit 10).
        assert!(tm.gc_versions_collected() > 0 || tm.min_active_ts() > 0);
    }

    #[test]
    fn concurrent_tm_gc_callback_invoked() {
        let tm = ConcurrentTransactionManager::with_gc_interval(5);
        let collected = Arc::new(AtomicU64::new(0));
        let collected_clone = Arc::clone(&collected);
        tm.set_gc_callback(move |_key, _ts| {
            collected_clone.fetch_add(1, Ordering::Relaxed);
        });
        // Write to the same key 20 times (each commit supersedes the last).
        for _ in 0..20 {
            let txn = tm.begin(IsolationLevel::Snapshot);
            tm.record_write(txn, b"hot_key").unwrap();
            tm.commit(txn).unwrap();
        }
        // Force a GC.
        tm.run_gc();
        // The callback should have been invoked for old versions.
        assert!(collected.load(Ordering::Relaxed) > 0,
            "GC callback should have been invoked");
    }

    #[test]
    fn concurrent_tm_visibility_check() {
        let tm = ConcurrentTransactionManager::new();
        let t1 = tm.begin(IsolationLevel::Snapshot);
        tm.record_write(t1, b"x").unwrap();
        let commit_ts = tm.commit(t1).unwrap();
        // A new txn should see t1's commit.
        let t2 = tm.begin(IsolationLevel::Snapshot);
        assert!(tm.read_ts(t2).unwrap() >= commit_ts);
        assert!(tm.is_committed(commit_ts));
    }

    #[test]
    fn concurrent_tm_abort_then_retry() {
        let tm = ConcurrentTransactionManager::new();
        let t1 = tm.begin(IsolationLevel::Snapshot);
        tm.record_write(t1, b"x").unwrap();
        tm.abort(t1).unwrap();
        assert!(matches!(tm.commit(t1), Err(MvccError::AlreadyAborted)));
        // New txn succeeds.
        let t2 = tm.begin(IsolationLevel::Snapshot);
        tm.record_write(t2, b"y").unwrap();
        tm.commit(t2).unwrap();
    }
}
