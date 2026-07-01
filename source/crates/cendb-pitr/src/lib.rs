//! cendb-pitr: Point-in-Time Recovery and time-travel queries.
//!
//! Maintains an MVCC version history that allows querying the state of
//! the database at any past timestamp. Built on top of cendb-tx's
//! VersionHeader and commit-timestamp tracking.

use cendb_tx::{TransactionManager, IsolationLevel};
use std::collections::BTreeMap;

/// A versioned value: the value of a key at a specific commit timestamp.
#[derive(Clone, Debug)]
pub struct VersionedValue<V: Clone> {
    pub begin_ts: u64,
    pub end_ts: u64,
    pub value: V,
}

/// A version chain for a single key: all committed versions, ordered by
/// `begin_ts`. Uses a `BTreeMap` for efficient range queries by timestamp.
pub struct VersionChain<V: Clone> {
    versions: BTreeMap<u64, VersionedValue<V>>,
}

impl<V: Clone> VersionChain<V> {
    pub fn new() -> Self {
        Self {
            versions: BTreeMap::new(),
        }
    }

    /// Record a new version of the value at `commit_ts`.
    pub fn commit(&mut self, commit_ts: u64, value: V) {
        // Mark the previous version as superseded.
        if let Some((&prev_ts, _)) = self.versions.range(..commit_ts).next_back() {
            if let Some(prev) = self.versions.get_mut(&prev_ts) {
                prev.end_ts = commit_ts;
            }
        }
        self.versions.insert(
            commit_ts,
            VersionedValue {
                begin_ts: commit_ts,
                end_ts: u64::MAX,
                value,
            },
        );
    }

    /// Query the value as of `read_ts` (time-travel query).
    pub fn read_at(&self, read_ts: u64) -> Option<&V> {
        // Find the version whose begin_ts <= read_ts AND end_ts > read_ts.
        for (_, version) in self.versions.range(..=read_ts).rev() {
            if version.begin_ts <= read_ts && version.end_ts > read_ts {
                return Some(&version.value);
            }
        }
        None
    }

    /// Get all versions (for debugging / history inspection).
    pub fn history(&self) -> Vec<&VersionedValue<V>> {
        self.versions.values().collect()
    }

    /// Garbage-collect versions older than `min_ts` that have been
    /// superseded. Returns the number of versions removed.
    pub fn gc(&mut self, min_ts: u64) -> usize {
        let to_remove: Vec<u64> = self
            .versions
            .iter()
            .filter(|(_, v)| v.end_ts <= min_ts)
            .map(|(ts, _)| *ts)
            .collect();
        let count = to_remove.len();
        for ts in to_remove {
            self.versions.remove(&ts);
        }
        count
    }

    /// Number of versions in the chain.
    pub fn version_count(&self) -> usize {
        self.versions.len()
    }
}

impl<V: Clone> Default for VersionChain<V> {
    fn default() -> Self {
        Self::new()
    }
}

/// The PITR manager: maintains version chains for all keys.
pub struct PitrManager<V: Clone> {
    chains: std::collections::HashMap<Vec<u8>, VersionChain<V>>,
    tx_manager: TransactionManager,
}

impl<V: Clone> PitrManager<V> {
    pub fn new() -> Self {
        Self {
            chains: std::collections::HashMap::new(),
            tx_manager: TransactionManager::new(),
        }
    }

    /// Write a value: begin a transaction, record the write, commit.
    pub fn write(&mut self, key: &[u8], value: V) -> u64 {
        let txn = self.tx_manager.begin(IsolationLevel::Snapshot);
        self.tx_manager.record_write(txn, key).unwrap();
        let commit_ts = self.tx_manager.commit(txn).unwrap();
        self.chains
            .entry(key.to_vec())
            .or_insert_with(VersionChain::new)
            .commit(commit_ts, value);
        commit_ts
    }

    /// Time-travel read: get the value of `key` as of `read_ts`.
    pub fn read_at(&self, key: &[u8], read_ts: u64) -> Option<&V> {
        self.chains.get(key)?.read_at(read_ts)
    }

    /// Read the current (latest) value.
    pub fn read_current(&self, key: &[u8]) -> Option<&V> {
        let current_ts = self.tx_manager.current_ts();
        self.read_at(key, current_ts)
    }

    /// Current timestamp.
    pub fn current_ts(&self) -> u64 {
        self.tx_manager.current_ts()
    }

    /// Get the version history for a key.
    pub fn history(&self, key: &[u8]) -> Vec<&VersionedValue<V>> {
        self.chains.get(key).map(|c| c.history()).unwrap_or_default()
    }

    /// Garbage-collect old versions.
    pub fn gc(&mut self, min_ts: u64) -> usize {
        let mut total = 0;
        for chain in self.chains.values_mut() {
            total += chain.gc(min_ts);
        }
        total
    }
}

impl<V: Clone> Default for PitrManager<V> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_travel_read() {
        let mut mgr = PitrManager::<String>::new();

        let ts1 = mgr.write(b"key", "value_v1".to_string());
        let ts2 = mgr.write(b"key", "value_v2".to_string());
        let ts3 = mgr.write(b"key", "value_v3".to_string());

        // Read at each timestamp.
        assert_eq!(mgr.read_at(b"key", ts1), Some(&"value_v1".to_string()));
        assert_eq!(mgr.read_at(b"key", ts2), Some(&"value_v2".to_string()));
        assert_eq!(mgr.read_at(b"key", ts3), Some(&"value_v3".to_string()));

        // Current read.
        assert_eq!(mgr.read_current(b"key"), Some(&"value_v3".to_string()));

        // Read before any write.
        assert_eq!(mgr.read_at(b"key", 0), None);
    }

    #[test]
    fn version_history() {
        let mut mgr = PitrManager::<i64>::new();
        for i in 0..10 {
            mgr.write(b"counter", i);
        }
        let history = mgr.history(b"counter");
        assert_eq!(history.len(), 10);
        // Latest version should have end_ts = MAX.
        assert_eq!(history.last().unwrap().end_ts, u64::MAX);
    }

    #[test]
    fn gc_removes_old_versions() {
        let mut mgr = PitrManager::<String>::new();
        let ts1 = mgr.write(b"k", "v1".to_string());
        let ts2 = mgr.write(b"k", "v2".to_string());
        let ts3 = mgr.write(b"k", "v3".to_string());

        // GC versions older than ts3 (should remove v1 and v2 since
        // their end_ts <= ts3).
        let removed = mgr.gc(ts3);
        assert_eq!(removed, 2, "expected 2 old versions removed, got {}", removed);

        // Can still read the current version.
        assert_eq!(mgr.read_current(b"k"), Some(&"v3".to_string()));

        // Can't read old versions anymore.
        assert_eq!(mgr.read_at(b"k", ts1), None);
    }

    #[test]
    fn multiple_keys_independent() {
        let mut mgr = PitrManager::<i64>::new();
        mgr.write(b"a", 1);
        mgr.write(b"b", 100);
        mgr.write(b"a", 2);

        assert_eq!(mgr.read_current(b"a"), Some(&2));
        assert_eq!(mgr.read_current(b"b"), Some(&100));
    }
}
