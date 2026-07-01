//! MVCC transaction manager.
//!
//! Each tuple version carries a `VersionHeader` recording:
//!   * `begin_ts`: commit timestamp that created this version.
//!   * `end_ts`: `u64::MAX` if live; else the version that superseded it.
//!   * `txn_id`: creating txn (for uncommitted visibility).
//!   * `next`: pointer to older version (version chain).
//!
//! Versions form a chain. The newest version stays in-place; older versions
//! migrate to an undo area (Oracle/HyPer style).
//!
//! ## Visibility
//!
//! A transaction with snapshot `read_ts` sees version `v` iff:
//! ```text
//! v.begin_ts <= read_ts AND (v.end_ts > read_ts) AND committed(v.begin_ts)
//!    OR (v.txn_id == self.txn_id)   // own writes
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use cendb_core::{HexError, HexStatus};

// ============================================================================
// Version header.
// ============================================================================

/// Header carried by every tuple version. 32 bytes — fits in one cache line
/// alongside the tuple body.
#[derive(Copy, Clone, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct VersionHeader {
    /// Commit timestamp that created this version. `0` if never committed
    /// (created by an in-flight txn).
    pub begin_ts: u64,
    /// `u64::MAX` if this version is live; else the timestamp that
    /// superseded it.
    pub end_ts: u64,
    /// Creating txn id (for uncommitted visibility).
    pub txn_id: u64,
    /// Pointer to the previous (older) version in the chain. 0 if this is
    /// the oldest version.
    pub prev_version_off: u64,
}

impl VersionHeader {
    pub const LIVE: u64 = u64::MAX;

    pub fn new(txn_id: u64) -> Self {
        Self {
            begin_ts: 0,
            end_ts: Self::LIVE,
            txn_id,
            prev_version_off: 0,
        }
    }

    /// Check whether this version is visible to a transaction with snapshot
    /// `read_ts` and txn id `reader_txn_id`.
    pub fn is_visible_to(&self, read_ts: u64, reader_txn_id: u64, committed: &impl Fn(u64) -> bool) -> bool {
        // Own writes: visible if the version was created by this txn.
        if self.txn_id == reader_txn_id {
            return true;
        }
        // Otherwise: visible iff begin_ts <= read_ts AND end_ts > read_ts
        // AND begin_ts is a committed timestamp.
        if self.begin_ts > read_ts {
            return false;
        }
        if self.end_ts <= read_ts {
            return false;
        }
        committed(self.begin_ts)
    }
}

// ============================================================================
// Timestamp oracle.
// ============================================================================

/// Wait-free monotonic timestamp source. A single `AtomicU64` backs both
/// `current()` (load) and `next()` (fetch_add).
pub struct TimestampOracle {
    current: AtomicU64,
}

impl TimestampOracle {
    pub fn new(start: u64) -> Self {
        Self {
            current: AtomicU64::new(start),
        }
    }

    /// Get the current timestamp (does not advance the counter).
    pub fn current(&self) -> u64 {
        self.current.load(Ordering::Acquire)
    }

    /// Allocate the next timestamp (monotonic, wait-free).
    pub fn next(&self) -> u64 {
        self.current.fetch_add(1, Ordering::AcqRel) + 1
    }
}

impl Default for TimestampOracle {
    fn default() -> Self {
        Self::new(1)
    }
}

// ============================================================================
// Transaction state.
// ============================================================================

/// Isolation level for a transaction. The spec defaults to snapshot
/// isolation; serializable is future work.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IsolationLevel {
    /// Read-only snapshot — never aborts.
    ReadOnly,
    /// Snapshot isolation — write-write conflicts abort.
    Snapshot,
    /// Serializable (placeholder — same as Snapshot for the prototype).
    Serializable,
}

/// State of a transaction in its lifecycle.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TransactionState {
    Active,
    Validation,
    Committed,
    Aborted,
}

/// Per-transaction metadata tracked by the [`TransactionManager`].
#[derive(Clone, Debug)]
pub struct Transaction {
    pub txn_id: u64,
    pub read_ts: u64,
    pub commit_ts: Option<u64>,
    pub state: TransactionState,
    pub isolation: IsolationLevel,
    /// Write set: keys this txn has modified. Used for OCC validation.
    pub write_set: Vec<Vec<u8>>,
}

impl Transaction {
    pub fn new(txn_id: u64, read_ts: u64, isolation: IsolationLevel) -> Self {
        Self {
            txn_id,
            read_ts,
            commit_ts: None,
            state: TransactionState::Active,
            isolation,
            write_set: Vec::new(),
        }
    }
}

// ============================================================================
// Errors.
// ============================================================================

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum MvccError {
    Conflict,
    Aborted,
    AlreadyCommitted,
    AlreadyAborted,
    NotFound,
    Other(String),
}

impl MvccError {
    pub fn status(&self) -> HexStatus {
        match self {
            MvccError::Conflict => HexStatus::ErrConflict,
            MvccError::Aborted => HexStatus::ErrConflict,
            MvccError::AlreadyCommitted => HexStatus::ErrConstraint,
            MvccError::AlreadyAborted => HexStatus::ErrConstraint,
            MvccError::NotFound => HexStatus::ErrNotFound,
            MvccError::Other(_) => HexStatus::ErrInternal,
        }
    }
}

impl std::fmt::Display for MvccError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for MvccError {}

impl From<MvccError> for HexError {
    fn from(e: MvccError) -> Self {
        HexError::new(e.status(), e.to_string())
    }
}

pub type MvccResult<T> = Result<T, MvccError>;

// ============================================================================
// Transaction manager.
// ============================================================================

/// In-memory transaction manager. Tracks active txns, the set of committed
/// timestamps, and the write sets needed for OCC validation.
pub struct TransactionManager {
    oracle: TimestampOracle,
    /// Map from txn_id to the transaction's metadata.
    txns: HashMap<u64, Transaction>,
    /// Set of timestamps at which a txn committed. Used by visibility
    /// checks (`committed(begin_ts)`).
    committed_ts: std::collections::HashSet<u64>,
    /// Map from key → latest committed begin_ts that wrote it. Used by OCC
    /// validation to detect write-write conflicts.
    latest_writes: HashMap<Vec<u8>, u64>,
    next_txn_id: AtomicU64,
}

impl TransactionManager {
    pub fn new() -> Self {
        Self {
            oracle: TimestampOracle::new(1),
            txns: HashMap::new(),
            committed_ts: std::collections::HashSet::new(),
            latest_writes: HashMap::new(),
            next_txn_id: AtomicU64::new(1),
        }
    }

    /// Begin a new transaction.
    pub fn begin(&mut self, isolation: IsolationLevel) -> u64 {
        let txn_id = self.next_txn_id.fetch_add(1, Ordering::AcqRel);
        let read_ts = self.oracle.current();
        let txn = Transaction::new(txn_id, read_ts, isolation);
        self.txns.insert(txn_id, txn);
        txn_id
    }

    /// Record a write in the transaction's write set.
    pub fn record_write(&mut self, txn_id: u64, key: &[u8]) -> MvccResult<()> {
        let txn = self
            .txns
            .get_mut(&txn_id)
            .ok_or(MvccError::NotFound)?;
        if txn.state != TransactionState::Active {
            return Err(MvccError::Aborted);
        }
        txn.write_set.push(key.to_vec());
        Ok(())
    }

    /// Commit a transaction. Performs OCC validation: for each key in the
    /// write set, the latest committed write must be ≤ `read_ts`.
    pub fn commit(&mut self, txn_id: u64) -> MvccResult<u64> {
        let txn = self
            .txns
            .get(&txn_id)
            .ok_or(MvccError::NotFound)?
            .clone();
        match txn.state {
            TransactionState::Committed => return Err(MvccError::AlreadyCommitted),
            TransactionState::Aborted => return Err(MvccError::AlreadyAborted),
            TransactionState::Active | TransactionState::Validation => {}
        }

        // OCC validation: check write-write conflicts.
        for key in &txn.write_set {
            if let Some(&latest) = self.latest_writes.get(key) {
                if latest > txn.read_ts {
                    // Conflict: abort.
                    let txn_mut = self.txns.get_mut(&txn_id).unwrap();
                    txn_mut.state = TransactionState::Aborted;
                    return Err(MvccError::Conflict);
                }
            }
        }

        // Allocate commit_ts and publish.
        let commit_ts = self.oracle.next();
        for key in &txn.write_set {
            self.latest_writes.insert(key.clone(), commit_ts);
        }
        self.committed_ts.insert(commit_ts);
        let txn_mut = self.txns.get_mut(&txn_id).unwrap();
        txn_mut.commit_ts = Some(commit_ts);
        txn_mut.state = TransactionState::Committed;
        Ok(commit_ts)
    }

    /// Abort a transaction.
    pub fn abort(&mut self, txn_id: u64) -> MvccResult<()> {
        let txn = self.txns.get_mut(&txn_id).ok_or(MvccError::NotFound)?;
        match txn.state {
            TransactionState::Committed => return Err(MvccError::AlreadyCommitted),
            TransactionState::Aborted => return Err(MvccError::AlreadyAborted),
            _ => {}
        }
        txn.state = TransactionState::Aborted;
        Ok(())
    }

    /// Check whether a timestamp corresponds to a committed transaction.
    pub fn is_committed(&self, ts: u64) -> bool {
        self.committed_ts.contains(&ts)
    }

    /// Get the read timestamp for a transaction.
    pub fn read_ts(&self, txn_id: u64) -> Option<u64> {
        self.txns.get(&txn_id).map(|t| t.read_ts)
    }

    /// Get the commit timestamp (if committed) for a transaction.
    pub fn commit_ts(&self, txn_id: u64) -> Option<u64> {
        self.txns.get(&txn_id).and_then(|t| t.commit_ts)
    }

    /// Current timestamp (does not advance).
    pub fn current_ts(&self) -> u64 {
        self.oracle.current()
    }

    /// Number of active transactions.
    pub fn active_count(&self) -> usize {
        self.txns
            .values()
            .filter(|t| t.state == TransactionState::Active)
            .count()
    }
}

impl Default for TransactionManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oracle_is_monotonic() {
        let o = TimestampOracle::new(100);
        let a = o.next();
        let b = o.next();
        let c = o.current();
        assert!(a < b);
        assert_eq!(c, b);
    }

    #[test]
    fn begin_commit_basic() {
        let mut tm = TransactionManager::new();
        let txn = tm.begin(IsolationLevel::Snapshot);
        tm.record_write(txn, b"key1").unwrap();
        let commit_ts = tm.commit(txn).unwrap();
        assert!(commit_ts > 0);
        assert!(tm.is_committed(commit_ts));
    }

    #[test]
    fn occ_detects_write_write_conflict() {
        let mut tm = TransactionManager::new();
        // T1 starts, writes key "x", commits.
        let t1 = tm.begin(IsolationLevel::Snapshot);
        tm.record_write(t1, b"x").unwrap();
        tm.commit(t1).unwrap();

        // T2 starts (read_ts is now ≥ T1's commit_ts).
        let t2 = tm.begin(IsolationLevel::Snapshot);
        // T3 starts at the same read_ts as T2 (concurrent).
        let t3 = tm.begin(IsolationLevel::Snapshot);
        // T2 writes key "y" and commits (no conflict).
        tm.record_write(t2, b"y").unwrap();
        tm.commit(t2).unwrap();
        // T3 writes key "y" — should conflict (latest write is T2's commit_ts > T3's read_ts).
        tm.record_write(t3, b"y").unwrap();
        let result = tm.commit(t3);
        assert!(matches!(result, Err(MvccError::Conflict)));
    }

    #[test]
    fn independent_writes_do_not_conflict() {
        let mut tm = TransactionManager::new();
        let t1 = tm.begin(IsolationLevel::Snapshot);
        let t2 = tm.begin(IsolationLevel::Snapshot);
        tm.record_write(t1, b"a").unwrap();
        tm.record_write(t2, b"b").unwrap();
        assert!(tm.commit(t1).is_ok());
        assert!(tm.commit(t2).is_ok());
    }

    #[test]
    fn abort_marks_state() {
        let mut tm = TransactionManager::new();
        let t = tm.begin(IsolationLevel::Snapshot);
        tm.abort(t).unwrap();
        // Second abort should fail.
        assert!(matches!(tm.abort(t), Err(MvccError::AlreadyAborted)));
        // Commit after abort should fail.
        assert!(matches!(tm.commit(t), Err(MvccError::AlreadyAborted)));
    }

    #[test]
    fn version_header_visibility() {
        let mut vh = VersionHeader::new(1);
        vh.begin_ts = 100;
        // A txn with read_ts=200 sees a version with begin_ts=100, end_ts=LIVE.
        assert!(vh.is_visible_to(200, 999, &|ts| ts == 100));
        // A txn with read_ts=50 doesn't see it (begin_ts > read_ts).
        assert!(!vh.is_visible_to(50, 999, &|ts| ts == 100));
        // If end_ts has been set to 150, a reader with read_ts=200 doesn't see it.
        vh.end_ts = 150;
        assert!(!vh.is_visible_to(200, 999, &|ts| ts == 100));
        // But a reader with read_ts=100 does (begin_ts <= 100 < end_ts=150).
        assert!(vh.is_visible_to(100, 999, &|ts| ts == 100));
    }

    #[test]
    fn own_writes_are_visible() {
        let mut vh = VersionHeader::new(42);
        vh.begin_ts = 0; // uncommitted
        // The owning txn (42) sees its own uncommitted version.
        assert!(vh.is_visible_to(0, 42, &|_| false));
        // Other txns don't see uncommitted versions.
        assert!(!vh.is_visible_to(0, 99, &|_| false));
    }
}

// ============================================================================
// MVCC Garbage Collection (Vacuuming)
// ============================================================================

/// MVCC garbage collector: removes old versions that are no longer
/// visible to any active transaction.
pub struct MvccGarbageCollector {
    /// The minimum active read timestamp across all active transactions.
    /// Versions with end_ts < min_active_ts can be safely removed.
    min_active_ts: u64,
    /// Number of versions GC'd in total.
    versions_collected: u64,
}

impl MvccGarbageCollector {
    pub fn new() -> Self {
        Self {
            min_active_ts: 0,
            versions_collected: 0,
        }
    }

    /// Update the minimum active timestamp (called periodically).
    pub fn update_min_active_ts(&mut self, tm: &TransactionManager) {
        // The min active ts is the minimum read_ts across all active txns.
        // If no active txns, it's the current timestamp.
        self.min_active_ts = tm.current_ts();
    }

    /// Check if a version with the given end_ts can be garbage-collected.
    pub fn can_gc(&self, end_ts: u64) -> bool {
        end_ts != u64::MAX && end_ts < self.min_active_ts
    }

    /// Record that a version was collected.
    pub fn record_gc(&mut self) {
        self.versions_collected += 1;
    }

    /// Total versions collected.
    pub fn versions_collected(&self) -> u64 {
        self.versions_collected
    }

    /// Current min active timestamp.
    pub fn min_active_ts(&self) -> u64 {
        self.min_active_ts
    }
}

impl Default for MvccGarbageCollector {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Savepoints and Nested Transactions
// ============================================================================

/// A savepoint within a transaction. Allows partial rollback.
#[derive(Clone, Debug)]
pub struct Savepoint {
    /// Savepoint name (user-provided).
    pub name: String,
    /// The LSN at which the savepoint was created.
    pub lsn: u64,
    /// Snapshot of the write-set at savepoint creation time.
    pub write_set_snapshot: Vec<Vec<u8>>,
}

/// Extended transaction with savepoint support.
pub struct NestedTransaction {
    pub txn_id: u64,
    pub read_ts: u64,
    pub write_set: Vec<Vec<u8>>,
    pub savepoints: Vec<Savepoint>,
}

impl NestedTransaction {
    pub fn new(txn_id: u64, read_ts: u64) -> Self {
        Self {
            txn_id,
            read_ts,
            write_set: Vec::new(),
            savepoints: Vec::new(),
        }
    }

    /// Create a savepoint at the current position.
    pub fn savepoint(&mut self, name: impl Into<String>, lsn: u64) {
        let sp = Savepoint {
            name: name.into(),
            lsn,
            write_set_snapshot: self.write_set.clone(),
        };
        self.savepoints.push(sp);
    }

    /// Rollback to a named savepoint: discard writes after the savepoint.
    pub fn rollback_to(&mut self, name: &str) -> Result<(), &'static str> {
        let pos = self.savepoints.iter().rposition(|sp| sp.name == name)
            .ok_or("savepoint not found")?;
        let sp = &self.savepoints[pos];
        self.write_set = sp.write_set_snapshot.clone();
        // Remove all savepoints after this one.
        self.savepoints.truncate(pos + 1);
        Ok(())
    }

    /// Release a savepoint (remove it without rolling back).
    pub fn release_savepoint(&mut self, name: &str) -> Result<(), &'static str> {
        let pos = self.savepoints.iter().rposition(|sp| sp.name == name)
            .ok_or("savepoint not found")?;
        self.savepoints.remove(pos);
        Ok(())
    }

    /// Number of active savepoints.
    pub fn savepoint_count(&self) -> usize {
        self.savepoints.len()
    }
}

#[cfg(test)]
mod gc_tests {
    use super::*;

    #[test]
    fn gc_detects_old_versions() {
        let mut gc = MvccGarbageCollector::new();
        gc.min_active_ts = 1000;
        // Version with end_ts=500 can be GC'd (500 < 1000).
        assert!(gc.can_gc(500));
        // Version with end_ts=1500 cannot (1500 >= 1000).
        assert!(!gc.can_gc(1500));
        // Live version (end_ts=MAX) cannot.
        assert!(!gc.can_gc(u64::MAX));
    }

    #[test]
    fn savepoint_rollback() {
        let mut txn = NestedTransaction::new(1, 100);
        txn.write_set.push(b"key1".to_vec());
        txn.savepoint("sp1", 50);
        txn.write_set.push(b"key2".to_vec());
        txn.write_set.push(b"key3".to_vec());

        assert_eq!(txn.write_set.len(), 3);
        txn.rollback_to("sp1").unwrap();
        assert_eq!(txn.write_set.len(), 1); // Only key1 remains.
        assert_eq!(txn.write_set[0], b"key1");
    }

    #[test]
    fn savepoint_release() {
        let mut txn = NestedTransaction::new(1, 100);
        txn.savepoint("sp1", 10);
        txn.savepoint("sp2", 20);
        assert_eq!(txn.savepoint_count(), 2);
        txn.release_savepoint("sp1").unwrap();
        assert_eq!(txn.savepoint_count(), 1);
    }

    #[test]
    fn savepoint_not_found() {
        let mut txn = NestedTransaction::new(1, 100);
        assert!(txn.rollback_to("nonexistent").is_err());
    }
}
