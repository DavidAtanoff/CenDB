//! Secondary indexes: map non-PK column values to RowLocators.
//!
//! ## MVCC Alignment
//!
//! Secondary indexes point to **physical RowLocators** (segment, block,
//! slot). When a transaction updates a row:
//!
//!   1. The new version is written to a new slot.
//!   2. The secondary index entry for the old value is marked as
//!      superseded (end_ts set).
//!   3. A new secondary index entry for the new value is added.
//!   4. On commit, the changes are published atomically.
//!
//! This requires index updates during MVCC commit, which is handled by
//! piggybacking secondary index entries onto the transaction's write-set.
//!
//! ## Block Compaction
//!
//! When a block is compacted (rows rewritten to a new block), all
//! secondary index entries pointing to the old block must be updated.
//! This is done via a compaction callback that rewrites RowLocators.

use std::collections::HashMap;
use cendb_core::{BlockId, RowLocator, SegmentId, SlotId};

/// A secondary index entry: maps a value to a row locator, with MVCC
/// version metadata.
#[derive(Clone, Debug)]
pub struct SecondaryIndexEntry {
    /// The indexed value (as i64 for fixed-width columns).
    pub value: i64,
    /// The physical location of the row.
    pub row_locator: RowLocator,
    /// Begin timestamp (when this index entry was created).
    pub begin_ts: u64,
    /// End timestamp (u64::MAX if live; else superseded).
    pub end_ts: u64,
    /// Creating transaction ID (for uncommitted visibility).
    pub txn_id: u64,
}

/// A secondary index on a single column. Maps values to row locators
/// with MVCC versioning.
pub struct SecondaryIndex {
    /// Column name being indexed.
    pub column_name: String,
    /// The index: value → list of versioned entries.
    /// Multiple entries per value form a version chain.
    entries: HashMap<i64, Vec<SecondaryIndexEntry>>,
}

impl SecondaryIndex {
    pub fn new(column_name: impl Into<String>) -> Self {
        Self {
            column_name: column_name.into(),
            entries: HashMap::new(),
        }
    }

    /// Insert an index entry. Called during MVCC commit.
    pub fn insert(
        &mut self,
        value: i64,
        locator: RowLocator,
        begin_ts: u64,
        txn_id: u64,
    ) {
        let entry = SecondaryIndexEntry {
            value,
            row_locator: locator,
            begin_ts,
            end_ts: u64::MAX,
            txn_id,
        };
        self.entries.entry(value).or_default().push(entry);
    }

    /// Look up rows by exact value. Returns all visible row locators
    /// (filtered by MVCC visibility).
    pub fn lookup(&self, value: i64, read_ts: u64) -> Vec<RowLocator> {
        match self.entries.get(&value) {
            None => Vec::new(),
            Some(chain) => chain
                .iter()
                .filter(|e| {
                    // MVCC visibility: begin_ts <= read_ts AND end_ts > read_ts.
                    e.begin_ts <= read_ts && e.end_ts > read_ts
                })
                .map(|e| e.row_locator)
                .collect(),
        }
    }

    /// Look up rows by range: `lo <= value <= hi`.
    pub fn lookup_range(&self, lo: i64, hi: i64, read_ts: u64) -> Vec<RowLocator> {
        let mut results = Vec::new();
        for (&value, chain) in &self.entries {
            if value < lo || value > hi {
                continue;
            }
            for entry in chain {
                if entry.begin_ts <= read_ts && entry.end_ts > read_ts {
                    results.push(entry.row_locator);
                }
            }
        }
        results
    }

    /// Mark an entry as superseded (end_ts set). Called when a new version
    /// replaces an old one.
    pub fn supersede(&mut self, value: i64, old_begin_ts: u64, new_end_ts: u64) {
        if let Some(chain) = self.entries.get_mut(&value) {
            for entry in chain.iter_mut() {
                if entry.begin_ts == old_begin_ts {
                    entry.end_ts = new_end_ts;
                }
            }
        }
    }

    /// Garbage-collect entries with end_ts < min_ts. Called during vacuum.
    pub fn gc(&mut self, min_ts: u64) -> usize {
        let mut removed = 0;
        for chain in self.entries.values_mut() {
            let before = chain.len();
            chain.retain(|e| e.end_ts >= min_ts || e.end_ts == u64::MAX);
            removed += before - chain.len();
        }
        // Remove empty value entries.
        self.entries.retain(|_, chain| !chain.is_empty());
        removed
    }

    /// Update row locators when a block is compacted (rows move to new
    /// block/slot). This is the compaction callback.
    pub fn relocate(
        &mut self,
        old_block: BlockId,
        old_slot: SlotId,
        new_segment: SegmentId,
        new_block: BlockId,
        new_slot: SlotId,
    ) {
        for chain in self.entries.values_mut() {
            for entry in chain.iter_mut() {
                if entry.row_locator.block == old_block && entry.row_locator.slot == old_slot {
                    entry.row_locator = RowLocator::new(new_segment, new_block, new_slot);
                }
            }
        }
    }

    /// Number of unique values indexed.
    pub fn value_count(&self) -> usize {
        self.entries.len()
    }

    /// Total number of index entries (including old versions).
    pub fn entry_count(&self) -> usize {
        self.entries.values().map(|c| c.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_lookup() {
        let mut idx = SecondaryIndex::new("age");
        idx.insert(30, RowLocator::new(SegmentId(1), BlockId(0), SlotId(0)), 100, 1);
        idx.insert(30, RowLocator::new(SegmentId(1), BlockId(0), SlotId(1)), 100, 1);
        idx.insert(25, RowLocator::new(SegmentId(1), BlockId(0), SlotId(2)), 100, 1);

        let results = idx.lookup(30, 200);
        assert_eq!(results.len(), 2);

        let results = idx.lookup(25, 200);
        assert_eq!(results.len(), 1);

        let results = idx.lookup(99, 200);
        assert!(results.is_empty());
    }

    #[test]
    fn range_lookup() {
        let mut idx = SecondaryIndex::new("score");
        for i in 0..100i64 {
            idx.insert(i, RowLocator::new(SegmentId(1), BlockId(0), SlotId(i as u32)), 100, 1);
        }
        let results = idx.lookup_range(10, 20, 200);
        assert_eq!(results.len(), 11); // 10..=20 inclusive
    }

    #[test]
    fn mvcc_visibility() {
        let mut idx = SecondaryIndex::new("val");
        // v1 created at ts=100.
        idx.insert(42, RowLocator::new(SegmentId(1), BlockId(0), SlotId(0)), 100, 1);
        // v1 superseded at ts=200 by a new version.
        idx.supersede(42, 100, 200);
        // v2 created at ts=200.
        idx.insert(42, RowLocator::new(SegmentId(1), BlockId(0), SlotId(1)), 200, 2);

        // Read at ts=150: should see v1.
        let r150 = idx.lookup(42, 150);
        assert_eq!(r150.len(), 1);
        assert_eq!(r150[0].slot.0, 0);

        // Read at ts=250: should see v2.
        let r250 = idx.lookup(42, 250);
        assert_eq!(r250.len(), 1);
        assert_eq!(r250[0].slot.0, 1);
    }

    #[test]
    fn gc_removes_old_versions() {
        let mut idx = SecondaryIndex::new("val");
        idx.insert(42, RowLocator::new(SegmentId(1), BlockId(0), SlotId(0)), 100, 1);
        idx.supersede(42, 100, 200);
        idx.insert(42, RowLocator::new(SegmentId(1), BlockId(0), SlotId(1)), 200, 2);

        let removed = idx.gc(250);
        assert_eq!(removed, 1); // Old version removed.
        assert_eq!(idx.entry_count(), 1); // Only v2 remains.
    }

    #[test]
    fn relocate_on_compaction() {
        let mut idx = SecondaryIndex::new("val");
        idx.insert(42, RowLocator::new(SegmentId(1), BlockId(5), SlotId(3)), 100, 1);

        // Simulate compaction: row moves from block 5 slot 3 to block 10 slot 0.
        idx.relocate(BlockId(5), SlotId(3), SegmentId(1), BlockId(10), SlotId(0));

        let results = idx.lookup(42, 200);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].block.0, 10);
        assert_eq!(results[0].slot.0, 0);
    }
}
