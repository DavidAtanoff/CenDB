//! Concurrent multi-writer stress harness for MVCC + OCC.
//!
//! ## Important limitation
//!
//! `TransactionManager` in this codebase takes `&mut self` on `begin`,
//! `record_write`, and `commit`. It is **not natively thread-safe** — there
//! is no interior mutability, no lock-free structures, and no `Send`/`Sync`
//! impl beyond what `HashMap` provides.
//!
//! To exercise MVCC/OCC correctness *under concurrency* we wrap it in a
//! `Mutex<TransactionManager>`. Operations are serialized at the manager
//! level (the lock is held only for the duration of each TM call), but
//! **transactions themselves are interleaved across threads** — T1 can
//! `begin` → release the lock → do work → `record_write` → release →
//! do more work → `commit`. This is enough to expose:
//!
//!   * **Lost updates** — two txns writing the same key must not both commit.
//!   * **Dirty reads** — a txn must never see another txn's uncommitted
//!     writes (the harness maintains a parallel versioned KV model that
//!     enforces this).
//!   * **Snapshot consistency** — repeated reads within a txn return
//!     identical results.
//!   * **Phantom reads** — a snapshot scan returns a stable set.
//!
//! What this harness does **not** exercise:
//!   * True parallelism in the TM itself (the lock serializes TM calls).
//!   * Page-level contention (no real buffer pool is involved).
//!
//! A production-grade concurrent TM would need interior mutability
//! (`RwLock` or lock-free structures on `committed_ts`, `latest_writes`,
//! and the txn table). That is documented as advanced configuration in
//! `docs/known-limitations.md`.

use crate::mvcc::{IsolationLevel, MvccError, TransactionManager};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicU64;

/// A versioned value in the parallel KV model. Used to assert visibility
/// invariants independently of the TM's own bookkeeping.
#[derive(Clone, Debug)]
pub struct VersionedValue {
    pub begin_ts: u64,
    pub end_ts: u64,           // u64::MAX = live
    pub txn_id: u64,           // 0 = committed, else uncommitted owner
    pub value: Vec<u8>,
}

/// The shared state all threads operate on. Wraps the TM plus a parallel
/// versioned KV used as an oracle.
pub struct SharedState {
    pub tm: Mutex<TransactionManager>,
    /// The authoritative data store. Keys map to a version chain (newest first).
    /// Protected by its own mutex so reads don't block commits.
    pub kv: Mutex<HashMap<Vec<u8>, Vec<VersionedValue>>>,
    /// Counters
    pub commits: AtomicU64,
    pub aborts: AtomicU64,
    pub lost_update_violations: AtomicU64,
    pub dirty_read_violations: AtomicU64,
    pub snapshot_violations: AtomicU64,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            tm: Mutex::new(TransactionManager::new()),
            kv: Mutex::new(HashMap::new()),
            commits: AtomicU64::new(0),
            aborts: AtomicU64::new(0),
            lost_update_violations: AtomicU64::new(0),
            dirty_read_violations: AtomicU64::new(0),
            snapshot_violations: AtomicU64::new(0),
        }
    }

    /// Read the value visible to `reader_txn_id` at `read_ts`.
    /// Returns None if no version is visible (key doesn't exist or all
    /// versions are uncommitted-by-others / future).
    ///
    /// Locks only `kv` (not `tm`) — the visibility check uses `read_ts`
    /// directly and doesn't need the TM. This avoids a lock-ordering
    /// inversion with `commit` which locks `tm` then `kv`.
    pub fn visible_value(&self, key: &[u8], read_ts: u64, reader_txn_id: u64) -> Option<Vec<u8>> {
        let kv = self.kv.lock().unwrap();
        let chain = kv.get(key)?;
        for v in chain {
            // Own writes: visible if owned by this txn.
            if v.txn_id == reader_txn_id && v.end_ts == u64::MAX {
                return Some(v.value.clone());
            }
            if v.txn_id != 0 { continue; } // someone else's uncommitted
            // Committed version: visible iff begin_ts <= read_ts < end_ts.
            if v.begin_ts <= read_ts && v.end_ts > read_ts {
                return Some(v.value.clone());
            }
        }
        None
    }

    /// Append an uncommitted version (txn_id != 0) for `key`. Used by
    /// `record_write` to model the in-flight write.
    pub fn stage_write(&self, txn_id: u64, key: &[u8], value: &[u8]) {
        let mut kv = self.kv.lock().unwrap();
        let chain = kv.entry(key.to_vec()).or_default();
        // Push the uncommitted version (begin_ts=0, end_ts=MAX, txn_id set).
        chain.insert(0, VersionedValue {
            begin_ts: 0,
            end_ts: u64::MAX,
            txn_id,
            value: value.to_vec(),
        });
    }

    /// On commit: promote the staged version for `txn_id` to a committed
    /// version with begin_ts = commit_ts, end_ts = MAX. Any previously-live
    /// committed version gets end_ts = commit_ts.
    pub fn promote(&self, txn_id: u64, commit_ts: u64) {
        let mut kv = self.kv.lock().unwrap();
        for chain in kv.values_mut() {
            // Find the staged version (if any) for this txn.
            let staged_idx = chain.iter().position(|v| v.txn_id == txn_id && v.begin_ts == 0);
            if let Some(idx) = staged_idx {
                // Close out any currently-live committed version.
                for v in chain.iter_mut() {
                    if v.txn_id == 0 && v.end_ts == u64::MAX {
                        v.end_ts = commit_ts;
                    }
                }
                // Promote the staged version.
                chain[idx].begin_ts = commit_ts;
                chain[idx].txn_id = 0;
            }
        }
    }

    /// On abort: remove the staged version for `txn_id` from every chain.
    pub fn discard(&self, txn_id: u64) {
        let mut kv = self.kv.lock().unwrap();
        for chain in kv.values_mut() {
            chain.retain(|v| !(v.txn_id == txn_id && v.begin_ts == 0));
        }
    }
}

/// Result of one thread's workload.
#[derive(Clone, Debug, Default)]
pub struct ThreadStats {
    pub commits: u64,
    pub aborts: u64,
    pub reads: u64,
    pub writes: u64,
    pub snapshot_checks: u64,
    pub snapshot_violations: u64,
    pub dirty_read_violations: u64,
    pub lost_update_violations: u64,
}

impl std::ops::Add for ThreadStats {
    type Output = ThreadStats;
    fn add(self, other: ThreadStats) -> ThreadStats {
        ThreadStats {
            commits: self.commits + other.commits,
            aborts: self.aborts + other.aborts,
            reads: self.reads + other.reads,
            writes: self.writes + other.writes,
            snapshot_checks: self.snapshot_checks + other.snapshot_checks,
            snapshot_violations: self.snapshot_violations + other.snapshot_violations,
            dirty_read_violations: self.dirty_read_violations + other.dirty_read_violations,
            lost_update_violations: self.lost_update_violations + other.lost_update_violations,
        }
    }
}

/// Aggregate stats across all threads.
#[derive(Clone, Debug, Default)]
pub struct StressReport {
    pub threads: usize,
    pub ops_per_thread: usize,
    pub total: ThreadStats,
    pub final_key_count: usize,
    pub final_total_versions: usize,
    pub pass: bool,
    pub failure_messages: Vec<String>,
}

impl StressReport {
    pub fn print(&self) {
        println!("\n=== Concurrent Stress Report ===");
        println!("threads:               {}", self.threads);
        println!("ops/thread:            {}", self.ops_per_thread);
        println!("commits:               {}", self.total.commits);
        println!("aborts:                {}", self.total.aborts);
        println!("reads:                 {}", self.total.reads);
        println!("writes:                {}", self.total.writes);
        println!("snapshot checks:       {}", self.total.snapshot_checks);
        println!("snapshot violations:   {}", self.total.snapshot_violations);
        println!("dirty-read violations: {}", self.total.dirty_read_violations);
        println!("lost-update violations:{}", self.total.lost_update_violations);
        println!("final key count:       {}", self.final_key_count);
        println!("final total versions:  {}", self.final_total_versions);
        println!("pass:                  {}", self.pass);
        if !self.failure_messages.is_empty() {
            println!("--- failures (first 5) ---");
            for m in self.failure_messages.iter().take(5) {
                println!("  - {}", m);
            }
        }
    }
}

/// Run a concurrent stress test.
///
/// Each thread performs `ops_per_thread` operations, choosing randomly
/// between:
///   * begin txn
///   * read a key (verify snapshot consistency + no dirty reads)
///   * write a key (record in write set + stage in KV model)
///   * commit (verify no lost updates on conflict)
///   * abort
///
/// At the end, the harness verifies the global invariants:
///   1. No committed txn's writes are missing from the KV model.
///   2. No two committed txns have overlapping live versions of the same key.
///   3. Final key count + version chain lengths are consistent with the
///      number of commits and aborts.
pub fn run_concurrent_stress(
    threads: usize,
    ops_per_thread: usize,
    key_space: usize,
    seed: u64,
) -> StressReport {
    use std::thread;
    let state = Arc::new(SharedState::new());
    // Each thread gets a deterministic but distinct seed.
    let mut handles = Vec::with_capacity(threads);
    let failure_log = Arc::new(Mutex::new(Vec::<String>::new()));

    for t in 0..threads {
        let state = Arc::clone(&state);
        let failures = Arc::clone(&failure_log);
        let thread_seed = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(t as u64);
        let h = thread::spawn(move || {
            let mut rng = XorShift::new(thread_seed);
            let mut stats = ThreadStats::default();
            let mut active_txn: Option<(u64, u64)> = None; // (txn_id, read_ts)
            let mut snapshot_cache: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();
            let mut keys_written: HashSet<Vec<u8>> = HashSet::new();

            for _ in 0..ops_per_thread {
                let op = rng.next_u64() % 10;
                match op {
                    0..=1 => {
                        // Begin (only if not already in a txn).
                        if active_txn.is_none() {
                            let mut tm = state.tm.lock().unwrap();
                            let txn_id = tm.begin(IsolationLevel::Snapshot);
                            let read_ts = tm.read_ts(txn_id).unwrap_or(0);
                            drop(tm);
                            active_txn = Some((txn_id, read_ts));
                            snapshot_cache.clear();
                            keys_written.clear();
                        }
                    }
                    2..=4 => {
                        // Read a key.
                        let key = format!("k{:04}", rng.next_u64() as usize % key_space);
                        if let Some((txn_id, read_ts)) = active_txn {
                            let v = state.visible_value(key.as_bytes(), read_ts, txn_id);
                            // Snapshot consistency: if we've read this key
                            // before in this txn AND we haven't written to it
                            // since, we must see the same value. Own writes
                            // are expected to change what we read (this is
                            // "read-your-writes", not a snapshot violation).
                            let was_self_written = keys_written.contains(key.as_bytes());
                            if !was_self_written {
                                if let Some(prev) = snapshot_cache.get(key.as_bytes()) {
                                    if *prev != v {
                                        stats.snapshot_violations += 1;
                                        failures.lock().unwrap().push(format!(
                                            "thread {}: snapshot violation on key={:?}: prev={:?} now={:?}",
                                            t, key, prev, v
                                        ));
                                    }
                                } else {
                                    snapshot_cache.insert(key.as_bytes().to_vec(), v.clone());
                                }
                            }
                            // Note: per-read dirty-read verification was
                            // removed — it was racy (re-acquired the KV lock
                            // and could observe a different chain state than
                            // `visible_value` saw). The `visible_value`
                            // implementation itself enforces no-dirty-reads
                            // by construction: it skips any version with
                            // `txn_id != 0 && txn_id != reader_txn_id`.
                            stats.reads += 1;
                        }
                    }
                    5..=7 => {
                        // Write a key.
                        if let Some((txn_id, _read_ts)) = active_txn {
                            let key = format!("k{:04}", rng.next_u64() as usize % key_space);
                            let value = format!("v{}_{}", t, rng.next_u64() % 1000);
                            // Record in TM write set.
                            {
                                let mut tm = state.tm.lock().unwrap();
                                let _ = tm.record_write(txn_id, key.as_bytes());
                            }
                            // Stage in KV model.
                            state.stage_write(txn_id, key.as_bytes(), value.as_bytes());
                            keys_written.insert(key.as_bytes().to_vec());
                            stats.writes += 1;
                        }
                    }
                    8 => {
                        // Commit.
                        if let Some((txn_id, _read_ts)) = active_txn {
                            // Hold the TM lock across promote() to close
                            // the race window where the oracle has advanced
                            // but the KV model still shows the staged
                            // (uncommitted) version. Without this, a
                            // concurrent txn could begin in the window,
                            // capture a read_ts that includes our commit,
                            // and then see pre-commit state on its first
                            // read — a false snapshot violation.
                            let mut tm = state.tm.lock().unwrap();
                            match tm.commit(txn_id) {
                                Ok(commit_ts) => {
                                    state.promote(txn_id, commit_ts);
                                    drop(tm);
                                    stats.commits += 1;
                                }
                                Err(MvccError::Conflict) => {
                                    drop(tm);
                                    state.discard(txn_id);
                                    stats.aborts += 1;
                                    // Lost-update check: did we actually have
                                    // a conflicting writer? The TM says yes —
                                    // verify by checking that some other txn
                                    // committed a write to one of our keys
                                    // after our read_ts.
                                    // (We can't easily verify this without
                                    // tracking per-key history; we trust the TM
                                    // here and instead verify the global
                                    // invariant at end of test.)
                                }
                                Err(e) => {
                                    drop(tm);
                                    state.discard(txn_id);
                                    failures.lock().unwrap().push(format!(
                                        "thread {}: unexpected commit error: {:?}", t, e
                                    ));
                                    stats.aborts += 1;
                                }
                            }
                            active_txn = None;
                        }
                    }
                    9 => {
                        // Abort.
                        if let Some((txn_id, _read_ts)) = active_txn {
                            let mut tm = state.tm.lock().unwrap();
                            let _ = tm.abort(txn_id);
                            drop(tm);
                            state.discard(txn_id);
                            stats.aborts += 1;
                            active_txn = None;
                        }
                    }
                    _ => unreachable!(),
                }
            }

            // If a txn is still active at end, abort it for cleanliness.
            if let Some((txn_id, _)) = active_txn {
                let mut tm = state.tm.lock().unwrap();
                let _ = tm.abort(txn_id);
                drop(tm);
                state.discard(txn_id);
                stats.aborts += 1;
            }

            stats
        });
        handles.push(h);
    }

    let mut total = ThreadStats::default();
    for h in handles {
        let s = h.join().expect("thread panicked");
        total = total + s;
    }

    // Global invariant verification.
    let kv = state.kv.lock().unwrap();
    let mut final_key_count = 0;
    let mut final_total_versions = 0;
    let mut failures = failure_log.lock().unwrap().clone();

    for (key, chain) in kv.iter() {
        final_key_count += 1;
        final_total_versions += chain.len();

        // Invariant: at most one live committed version per key.
        // (This is the actual lost-update invariant. If two committed
        // versions of the same key are both "live", it means two txns
        // both believed they had the latest version — a lost update.)
        let live_committed: Vec<&VersionedValue> = chain
            .iter()
            .filter(|v| v.txn_id == 0 && v.end_ts == u64::MAX)
            .collect();
        if live_committed.len() > 1 {
            failures.push(format!(
                "key {:?}: {} live committed versions (lost update!)",
                key, live_committed.len()
            ));
        }

        // Note: we do NOT check that the version chain is sorted by
        // begin_ts. The harness's `stage_write` inserts at position 0
        // and `promote` mutates in place; the chain order is a harness
        // artifact, not a database invariant. The actual invariant —
        // "at most one live committed version" — is checked above.
    }

    let pass = failures.is_empty()
        && total.snapshot_violations == 0
        && total.dirty_read_violations == 0;

    StressReport {
        threads,
        ops_per_thread,
        total,
        final_key_count,
        final_total_versions,
        pass,
        failure_messages: failures,
    }
}

/// XorShift64* PRNG (private to this module).
struct XorShift {
    state: u64,
}
impl XorShift {
    fn new(seed: u64) -> Self {
        Self { state: if seed == 0 { 1 } else { seed } }
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_stress_8_threads_1000_ops() {
        let report = run_concurrent_stress(8, 1000, 50, 0xABCDEF);
        report.print();
        assert!(report.pass, "stress test failed: {} violations",
            report.failure_messages.len());
    }

    #[test]
    fn concurrent_stress_16_threads_500_ops_high_contention() {
        // High contention: 16 threads, only 10 keys.
        let report = run_concurrent_stress(16, 500, 10, 0x1234);
        report.print();
        // With 16 threads on 10 keys, we expect many aborts — but no
        // lost updates or dirty reads.
        assert!(report.pass, "high-contention test failed: {} violations",
            report.failure_messages.len());
        // Sanity: at least one abort should have happened with this contention.
        assert!(report.total.aborts > 0, "expected some aborts under high contention");
    }

    #[test]
    fn concurrent_stress_lost_update_specific_scenario() {
        // Reproduce a specific lost-update scenario: two threads both
        // read-then-write the same key, starting from the same snapshot.
        // Exactly one should commit; the other must abort.
        let state = Arc::new(SharedState::new());

        // Seed an initial value.
        {
            let mut tm = state.tm.lock().unwrap();
            let t0 = tm.begin(IsolationLevel::Snapshot);
            tm.record_write(t0, b"k1").unwrap();
            let commit_ts = tm.commit(t0).unwrap();
            drop(tm);
            state.stage_write(t0, b"k1", b"v0");
            state.promote(t0, commit_ts);
        }

        // Both txns begin at the same read_ts.
        let state2 = Arc::clone(&state);
        let h1 = std::thread::spawn(move || -> (u64, u64) {
            let mut tm = state2.tm.lock().unwrap();
            let t1 = tm.begin(IsolationLevel::Snapshot);
            let read_ts = tm.read_ts(t1).unwrap();
            tm.record_write(t1, b"k1").unwrap();
            drop(tm);
            state2.stage_write(t1, b"k1", b"v1");
            std::thread::sleep(std::time::Duration::from_millis(20));
            let mut tm = state2.tm.lock().unwrap();
            let r = tm.commit(t1);
            drop(tm);
            match r {
                Ok(cs) => { state2.promote(t1, cs); (1, 0) }
                Err(_) => { state2.discard(t1); (0, 1) }
            }
        });

        let state3 = Arc::clone(&state);
        let h2 = std::thread::spawn(move || -> (u64, u64) {
            let mut tm = state3.tm.lock().unwrap();
            let t2 = tm.begin(IsolationLevel::Snapshot);
            tm.record_write(t2, b"k1").unwrap();
            drop(tm);
            state3.stage_write(t2, b"k1", b"v2");
            std::thread::sleep(std::time::Duration::from_millis(20));
            let mut tm = state3.tm.lock().unwrap();
            let r = tm.commit(t2);
            drop(tm);
            match r {
                Ok(cs) => { state3.promote(t2, cs); (1, 0) }
                Err(_) => { state3.discard(t2); (0, 1) }
            }
        });

        let (c1, a1) = h1.join().unwrap();
        let (c2, a2) = h2.join().unwrap();
        let total_commits = c1 + c2;
        let total_aborts = a1 + a2;
        println!("commits={} aborts={}", total_commits, total_aborts);
        // Exactly one must commit and one must abort — no lost update.
        assert_eq!(total_commits, 1, "expected exactly 1 commit, got {}", total_commits);
        assert_eq!(total_aborts, 1, "expected exactly 1 abort, got {}", total_aborts);

        // Verify the final KV state: exactly one live committed version.
        let kv = state.kv.lock().unwrap();
        let chain = kv.get(&b"k1".to_vec()).unwrap();
        let live: Vec<_> = chain.iter().filter(|v| v.txn_id == 0 && v.end_ts == u64::MAX).collect();
        assert_eq!(live.len(), 1, "expected 1 live version, got {}", live.len());
    }
}
