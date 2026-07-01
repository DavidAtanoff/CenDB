//! Key-Value projection: a degenerate 2-column table `(key BYTES, value BYTES)`
//! with an in-memory hash index for O(1) point lookups.
//!
//! This is the fast path described in §5/§6 of the spec: KV operations bypass
//! the query planner entirely and go straight from the hash index to the PAX
//! block. Range queries fall back to a linear scan of the block directory's
//! zone map.
//!
//! For this implementation we keep the index in memory; a production version would
//! spill it to a B-link tree on disk (§4.2 of the spec).

use std::collections::HashMap;
use std::path::Path;

use cendb_core::{BlockId, CenResult, SegmentId};
use cendb_storage::header::ColumnSpec;
use cendb_storage::pax::{PaxBlock, PaxBlockBuilder, PaxBlockReader};
use cendb_storage::segment::{SegmentFile, SegmentWriter};
use cendb_core::{Value, ValueKind};

/// In-memory index: key bytes → (block_id, slot_id).
type KeyIndex = HashMap<Vec<u8>, (BlockId, u32)>;

/// Key-Value projection. Owns a list of sealed PAX blocks plus an in-memory
/// hash index for point lookups.
pub struct KvStore {
    segment_id: SegmentId,
    block_size: u32,
    blocks: Vec<PaxBlock>,
    index: KeyIndex,
    /// Pending writes not yet flushed to a sealed block. We use a
    /// HashMap for O(1) point lookup (the old Vec-based scan was O(n)
    /// and dominated get latency at 17µs p99 — see Phase 3 benchmarks).
    /// The Vec is kept only for ordered iteration during flush.
    pending: HashMap<Vec<u8>, Vec<u8>>,
    /// Insertion order for pending (for deterministic flush). We keep
    /// this in sync with `pending` — keys are appended on put, removed
    /// on flush.
    pending_order: Vec<Vec<u8>>,
    pending_capacity: usize,
}

impl KvStore {
    /// Construct a new KV store with the given block size.
    pub fn new(segment_id: SegmentId, block_size: u32) -> Self {
        Self {
            segment_id,
            block_size,
            blocks: Vec::new(),
            index: HashMap::new(),
            pending: HashMap::new(),
            pending_order: Vec::new(),
            pending_capacity: 1024,
        }
    }

    /// Insert (or overwrite) a key-value pair. The write is buffered in
    /// memory until a block is sealed.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> CenResult<()> {
        // If the key already exists in a sealed block, we record a
        // tombstone (for this implementation: just overwrite the index entry,
        // leaving a stale row in the old block — a production version
        // would mark the slot tombstoned).
        let key_vec = key.to_vec();
        if !self.pending.contains_key(&key_vec) {
            self.pending_order.push(key_vec.clone());
        }
        self.pending.insert(key_vec, value.to_vec());
        if self.pending.len() >= self.pending_capacity {
            self.flush_pending()?;
        }
        Ok(())
    }

    /// Look up a key. Returns the value bytes if found.
    ///
    /// O(1) in both the pending buffer (HashMap) and the sealed
    /// index (HashMap). No linear scan.
    ///
    /// Returns `None` for deleted keys (tombstones are empty values;
    /// this method treats them as deleted and returns `None`).
    pub fn get(&self, key: &[u8]) -> CenResult<Option<Vec<u8>>> {
        // Check pending first (most recent writes) — O(1) HashMap lookup.
        if let Some(v) = self.pending.get(key) {
            // Empty value = tombstone (deleted key). Return None.
            if v.is_empty() {
                return Ok(None);
            }
            return Ok(Some(v.clone()));
        }
        // Check the in-memory index — O(1) HashMap lookup.
        if let Some(&(block_id, slot)) = self.index.get(key) {
            let block = &self.blocks[block_id.0 as usize];
            // Column 2 is the value (schema: [pk_i64, key_bytes, value_bytes]).
            let value = block.var_value(2, slot as usize)?;
            // Empty value = tombstone. Return None.
            if let Some(ref v) = value {
                if v.is_empty() {
                    return Ok(None);
                }
            }
            return Ok(value.map(|b| b.to_vec()));
        }
        Ok(None)
    }

    /// Delete a key (insert a tombstone). Returns `Ok` if the key was
    /// previously present.
    pub fn delete(&mut self, key: &[u8]) -> CenResult<bool> {
        let in_index = self.index.remove(key).is_some();
        let in_pending = self.pending.remove(key).is_some();
        if in_pending {
            self.pending_order.retain(|k| k != key);
        }
        let existed = in_index || in_pending;
        if existed {
            // Mark as deleted by inserting an empty value (the canonical
            // tombstone marker for this implementation).
            self.put(key, &[])?;
        }
        Ok(existed)
    }

    /// Force-flush the pending buffer into sealed PAX blocks. Multiple
    /// blocks may be created if the pending buffer is too large to fit
    /// in a single block.
    pub fn flush_pending(&mut self) -> CenResult<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let specs = kv_specs();
        let block_overhead: usize = 64 + 3 * 64 + 256;
        let usable = (self.block_size as usize).saturating_sub(block_overhead);

        // Collect pending entries in insertion order (for deterministic flush).
        let ordered: Vec<(Vec<u8>, Vec<u8>)> = self.pending_order.iter()
            .filter_map(|k| self.pending.get(k).map(|v| (k.clone(), v.clone())))
            .collect();

        let mut idx = 0usize;
        while idx < ordered.len() {
            let chunk_start = idx;
            let mut chunk_bytes = 0usize;
            while idx < ordered.len() {
                let (k, v) = &ordered[idx];
                let row_bytes = 8 + 8 + k.len() + 8 + v.len() + 16;
                if chunk_bytes + row_bytes > usable && idx > chunk_start {
                    break;
                }
                chunk_bytes += row_bytes;
                idx += 1;
            }
            let mut builder = PaxBlockBuilder::new(self.block_size, specs.clone())?;
            let mut new_index_entries: Vec<(Vec<u8>, u32)> = Vec::new();
            for (k, v) in ordered[chunk_start..idx].iter() {
                let pk = hash_key(k);
                let row_id = builder.append_row(&[
                    Value::I64(pk),
                    Value::Bytes(k.clone()),
                    Value::Bytes(v.clone()),
                ])?;
                new_index_entries.push((k.clone(), row_id.0));
            }
            let block = builder.finalize()?;
            let block_id = BlockId(self.blocks.len() as u32);
            for (k, slot) in new_index_entries.drain(..) {
                self.index.insert(k, (block_id, slot));
            }
            self.blocks.push(block);
        }
        self.pending.clear();
        self.pending_order.clear();
        Ok(())
    }

    /// Iterate over all key-value pairs (used by range scans and exports).
    pub fn iter(&self) -> KvIter<'_> {
        KvIter {
            store: self,
            pending_idx: 0,
            block_idx: 0,
            slot_idx: 0,
        }
    }

    /// Number of keys currently stored (pending + sealed).
    pub fn len(&self) -> usize {
        self.pending.len() + self.index.len()
    }

    /// Seal the store (flush all pending writes).
    pub fn seal(&mut self) -> CenResult<()> {
        self.flush_pending()
    }

    /// Persist the KV store to a segment file on disk. Writes all sealed
    /// blocks to the file at `path` and seals the segment.
    pub fn persist_to_segment(&mut self, path: impl AsRef<Path>) -> CenResult<()> {
        self.flush_pending()?;
        let block_size = self.block_size;
        let page_size = 4096u32;
        let mut writer = SegmentWriter::create(
            path,
            self.segment_id,
            page_size,
            block_size,
            0, // created_lsn (no WAL wired up here)
        )?;
        for block in &self.blocks {
            writer.append_block(block)?;
        }
        writer.seal(0)?;
        Ok(())
    }

    /// Load a KV store from a previously-persisted segment file. Reads all
    /// blocks back into memory and rebuilds the in-memory key index.
    pub fn load_from_segment(
        path: impl AsRef<Path>,
        segment_id: SegmentId,
        block_size: u32,
    ) -> CenResult<Self> {
        let mut seg = SegmentFile::open(path)?;
        let mut store = Self::new(segment_id, block_size);
        // Snapshot the entries to avoid holding an immutable borrow of seg
        // while we mutably borrow it for read_block.
        let entries: Vec<(u32, u32)> = seg
            .block_dir
            .entries
            .iter()
            .map(|e| (e.block_id, e.row_count))
            .collect();
        for (block_id, row_count) in entries {
            let mut buf = vec![0u8; block_size as usize];
            seg.read_block(BlockId(block_id), &mut buf)?;
            let reader = PaxBlockReader::new(&buf, block_size);
            for slot in 0..row_count {
                let key = reader.var_value(1, slot as usize)?;
                if let Some(k) = key {
                    store.index.insert(k.to_vec(), (BlockId(block_id), slot));
                }
            }
            // Parse PaxBlock from the read bytes and push to store.blocks
            let mut aligned = cendb_storage::pax::AlignedBlock::zeroed(block_size as usize)?;
            aligned.as_mut_slice().copy_from_slice(&buf);
            let block = PaxBlock::from_owned(aligned, block_size)?;
            store.blocks.push(block);
        }
        Ok(store)
    }

    /// Compression ratio across all sealed blocks: ratio of "raw bytes if
    /// stored as plain `(key_len + val_len)`" to actual block bytes used.
    pub fn compression_ratio(&self) -> f64 {
        let mut raw_bytes: usize = 0;
        let mut block_bytes: usize = 0;
        for b in &self.blocks {
            block_bytes += b.as_bytes().len();
            let hdr = b.header();
            for slot in 0..hdr.row_count {
                // Column 1 is key, column 2 is value.
                if let (Ok(Some(k)), Ok(Some(v))) = (b.var_value(1, slot as usize), b.var_value(2, slot as usize)) {
                    raw_bytes += k.len() + v.len() + 8; // +8 for key hash
                }
            }
        }
        if block_bytes == 0 {
            return 1.0;
        }
        raw_bytes as f64 / block_bytes as f64
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Total physical bytes used by all sealed blocks. Each PAX block is
    /// allocated at exactly `self.block_size` bytes, so this is
    /// `block_count() * block_size`.
    pub fn sealed_bytes(&self) -> u64 {
        self.blocks
            .iter()
            .map(|b| b.as_bytes().len() as u64)
            .sum()
    }

    /// Compact the store: rewrite surviving rows into densely-packed
    /// blocks, discarding stale rows and tombstones.
    ///
    /// # What gets discarded
    ///
    /// * **Stale rows** — a row is stale if a later block contains a row
    ///   with the same key. The later block's entry supersedes the older
    ///   one; only the latest version of each key is kept.
    /// * **Tombstones** — a row is a tombstone if its value column is
    ///   empty (0 bytes). Tombstones are written by [`Self::delete`] and
    ///   represent a key that no longer exists. Compaction removes all
    ///   tombstones, so reads of those keys will return `None` after
    ///   compaction (rather than `Some(empty)`).
    ///
    /// # Algorithm
    ///
    /// 1. Flush any pending writes so compaction sees a complete view.
    /// 2. Iterate the in-memory index (which already points at the
    ///    latest version of each key) and collect `(key, value)` pairs
    ///    whose value is non-empty.
    /// 3. Drop every sealed block and clear the index.
    /// 4. Re-insert the survivors in key-sorted order (deterministic
    ///    layout, better locality for range scans) and flush them into
    ///    new, densely-packed PAX blocks.
    ///
    /// The in-memory index is rebuilt automatically during step 4.
    ///
    /// # Returns
    ///
    /// A [`CompactionStats`] record with block/row counts before and
    /// after, and the number of physical bytes reclaimed.
    pub fn compact(&mut self) -> CenResult<CompactionStats> {
        // Flush any pending writes so we compact a complete view of
        // the data. (Pending writes would otherwise be invisible to
        // the sealed-block scan below.)
        self.flush_pending()?;

        let blocks_before = self.blocks.len();
        let rows_before: u64 = self
            .blocks
            .iter()
            .map(|b| b.header().row_count as u64)
            .sum();
        let bytes_before: u64 = self.sealed_bytes();

        // Collect the latest non-tombstone value for each key. The
        // in-memory index already points at the latest version of each
        // key (the put/flush code overwrites index entries on each
        // flush), so we can iterate it directly rather than scanning
        // every row of every block.
        let mut survivors: Vec<(Vec<u8>, Vec<u8>)> =
            Vec::with_capacity(self.index.len());
        for (key, &(block_id, slot)) in &self.index {
            // Defensive: skip index entries that point outside the
            // current block list. This should never happen if the
            // index invariants are maintained, but a corrupt entry
            // shouldn't crash compaction.
            let Some(block) = self.blocks.get(block_id.0 as usize) else {
                continue;
            };
            match block.var_value(2, slot as usize)? {
                Some(v) if !v.is_empty() => {
                    survivors.push((key.clone(), v.to_vec()));
                }
                _ => {
                    // Tombstone (empty value) or null — discard.
                }
            }
        }

        // Reset the store's sealed-block state. Pending is empty
        // (we just flushed), so we only need to clear the blocks
        // and the index.
        self.blocks.clear();
        self.index.clear();

        // Re-insert survivors in a deterministic order (sorted by
        // key) so the resulting block layout is reproducible across
        // runs. This also gives better locality for range scans.
        survivors.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, v) in &survivors {
            self.put(k, v)?;
        }
        // Flush the re-inserted survivors into new densely-packed
        // blocks. This rebuilds the in-memory index automatically.
        self.flush_pending()?;

        let blocks_after = self.blocks.len();
        let rows_after: u64 = self
            .blocks
            .iter()
            .map(|b| b.header().row_count as u64)
            .sum();
        let bytes_after: u64 = self.sealed_bytes();

        Ok(CompactionStats {
            blocks_before,
            blocks_after,
            rows_before,
            rows_after,
            bytes_reclaimed: bytes_before.saturating_sub(bytes_after),
        })
    }
}

/// Statistics returned by [`KvStore::compact`].
///
/// All fields are non-negative; `bytes_reclaimed` is `bytes_before -
/// bytes_after` (saturating at 0). After a no-op compaction (no stale
/// rows or tombstones, and the existing blocks were already densely
/// packed), every field pair is equal and `bytes_reclaimed == 0`.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct CompactionStats {
    /// Number of sealed blocks before compaction.
    pub blocks_before: usize,
    /// Number of sealed blocks after compaction.
    pub blocks_after: usize,
    /// Total rows across all sealed blocks before compaction (includes
    /// stale rows and tombstones).
    pub rows_before: u64,
    /// Total rows across all sealed blocks after compaction (only
    /// surviving latest-version rows).
    pub rows_after: u64,
    /// Physical bytes reclaimed: `bytes_before - bytes_after`, where
    /// bytes are summed across all sealed blocks at their full
    /// `block_size` (each PAX block is allocated at exactly
    /// `block_size` bytes).
    pub bytes_reclaimed: u64,
}

impl CompactionStats {
    /// `true` if compaction reclaimed any space (blocks or bytes).
    pub fn reclaimed_anything(&self) -> bool {
        self.blocks_after < self.blocks_before || self.bytes_reclaimed > 0
    }
}

/// Iterator over all KV pairs in the store.
pub struct KvIter<'a> {
    store: &'a KvStore,
    pending_idx: usize,
    block_idx: usize,
    slot_idx: u32,
}

impl<'a> Iterator for KvIter<'a> {
    type Item = (Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        // Pending first (in insertion order via pending_order).
        if self.pending_idx < self.store.pending_order.len() {
            let k = &self.store.pending_order[self.pending_idx];
            self.pending_idx += 1;
            if let Some(v) = self.store.pending.get(k) {
                return Some((k.clone(), v.clone()));
            }
            // Key was removed from pending but still in order — skip.
            return self.next();
        }
        // Then sealed blocks.
        while self.block_idx < self.store.blocks.len() {
            let block = &self.store.blocks[self.block_idx];
            let hdr = block.header();
            if self.slot_idx >= hdr.row_count {
                self.block_idx += 1;
                self.slot_idx = 0;
                continue;
            }
            let slot = self.slot_idx as usize;
            self.slot_idx += 1;
            // Column 1 is key, column 2 is value.
            let key = block.var_value(1, slot).ok().flatten();
            let val = block.var_value(2, slot).ok().flatten();
            if let (Some(k), Some(v)) = (key, val) {
                // Skip tombstones (empty value).
                if v.is_empty() {
                    continue;
                }
                return Some((k.to_vec(), v.to_vec()));
            }
        }
        None
    }
}

/// Schema for the KV projection: (key_hash i64 pk, key bytes, value bytes).
fn kv_specs() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new(0, ValueKind::I64).pk(),
        ColumnSpec::new(1, ValueKind::Bytes),
        ColumnSpec::new(2, ValueKind::Bytes),
    ]
}

/// Hash a byte key into an i64. We use FNV-1a (simple, fast, no deps).
pub fn hash_key(key: &[u8]) -> i64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in key {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash as i64
}

/// Stateless projection helpers — useful for the FFI layer.
pub struct KvProjection;

impl KvProjection {
    /// Build a single PAX block from an iterator of (key, value) pairs.
    pub fn build_block<'a, I>(block_size: u32, pairs: I) -> CenResult<PaxBlock>
    where
        I: IntoIterator<Item = (&'a [u8], &'a [u8])>,
    {
        let specs = kv_specs();
        let mut builder = PaxBlockBuilder::new(block_size, specs)?;
        for (k, v) in pairs {
            let pk = hash_key(k);
            builder.append_row(&[Value::I64(pk), Value::Bytes(k.to_vec()), Value::Bytes(v.to_vec())])?;
        }
        builder.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_roundtrip() {
        let mut store = KvStore::new(SegmentId(1), 16 * 1024);
        store.put(b"alice", b"password123").unwrap();
        store.put(b"bob", b"hunter2").unwrap();
        store.flush_pending().unwrap();

        assert_eq!(store.get(b"alice").unwrap(), Some(b"password123".to_vec()));
        assert_eq!(store.get(b"bob").unwrap(), Some(b"hunter2".to_vec()));
        assert_eq!(store.get(b"charlie").unwrap(), None);
    }

    #[test]
    fn pending_writes_visible_before_flush() {
        let mut store = KvStore::new(SegmentId(1), 16 * 1024);
        store.put(b"key1", b"value1").unwrap();
        // No flush — get should still find the pending write.
        assert_eq!(store.get(b"key1").unwrap(), Some(b"value1".to_vec()));
    }

    #[test]
    fn delete_inserts_tombstone() {
        let mut store = KvStore::new(SegmentId(1), 16 * 1024);
        store.put(b"alice", b"pwd").unwrap();
        store.flush_pending().unwrap();
        assert!(store.delete(b"alice").unwrap());
        store.flush_pending().unwrap();
        // After delete, get() returns None (tombstone treated as deleted).
        let v = store.get(b"alice").unwrap();
        assert!(v.is_none(), "deleted key should return None, got {:?}", v);
    }

    #[test]
    fn iter_visits_all_pairs() {
        let mut store = KvStore::new(SegmentId(1), 16 * 1024);
        for i in 0..50 {
            store.put(format!("k{}", i).as_bytes(), format!("v{}", i).as_bytes()).unwrap();
        }
        store.flush_pending().unwrap();
        let collected: Vec<_> = store.iter().collect();
        assert_eq!(collected.len(), 50);
    }

    #[test]
    fn many_keys_span_multiple_blocks() {
        let mut store = KvStore::new(SegmentId(1), 16 * 1024);
        // Each value ~200 bytes; ~50 fit per 16KB block.
        let big_value = vec![b'x'; 200];
        for i in 0..200 {
            store.put(format!("key-{:04}", i).as_bytes(), &big_value).unwrap();
        }
        store.flush_pending().unwrap();
        assert!(store.block_count() > 1, "expected >1 block, got {}", store.block_count());
        // Spot check.
        assert_eq!(store.get(b"key-0100").unwrap(), Some(big_value.clone()));
    }

    // ========================================================================
    // Compaction tests (Fix 2: reclaim space from stale/deleted rows).
    // ========================================================================
    //
    // The headline behaviour is in `compact_reclaims_space_and_preserves_keys`
    // — insert 1000 keys, overwrite 500, delete 200, compact, and verify
    // (a) all surviving keys are still readable with their latest values,
    // (b) the block count decreased, (c) the total bytes decreased.

    /// Helper: build a key like `b"key-000123"`.
    fn key(i: u32) -> Vec<u8> {
        format!("key-{:06}", i).into_bytes()
    }

    /// The headline compaction test from the task description.
    ///
    /// * Insert 1000 keys with value `v1`.
    /// * Overwrite keys 0..500 with value `v2` (creates 500 stale rows).
    /// * Delete keys 500..700 (creates 200 tombstones).
    /// * Compact.
    ///
    /// After compaction:
    /// * Keys 0..500 read back as `v2` (the latest version).
    /// * Keys 700..1000 read back as `v1` (untouched).
    /// * Keys 500..700 read back as `None` (tombstones removed).
    /// * `blocks_after < blocks_before`.
    /// * `bytes_reclaimed > 0`.
    /// * `rows_after < rows_before`.
    #[test]
    fn compact_reclaims_space_and_preserves_keys() {
        let mut store = KvStore::new(SegmentId(1), 16 * 1024);
        let v1 = b"value-version-1".to_vec();
        let v2 = b"value-version-2-longer".to_vec();

        // Insert 1000 keys with v1.
        for i in 0..1000 {
            store.put(&key(i), &v1).unwrap();
        }
        store.flush_pending().unwrap();
        let blocks_after_insert = store.block_count();
        assert!(blocks_after_insert > 1, "1000 keys should span >1 block");

        // Overwrite keys 0..500 with v2.
        for i in 0..500 {
            store.put(&key(i), &v2).unwrap();
        }
        store.flush_pending().unwrap();

        // Delete keys 500..700.
        for i in 500..700 {
            assert!(store.delete(&key(i)).unwrap(), "delete key {} should succeed", i);
        }
        store.flush_pending().unwrap();

        let stats = store.compact().unwrap();

        // (a) All surviving keys are still readable with their latest values.
        for i in 0..500 {
            assert_eq!(
                store.get(&key(i)).unwrap(),
                Some(v2.clone()),
                "key {} should have v2 after compaction",
                i
            );
        }
        for i in 700..1000 {
            assert_eq!(
                store.get(&key(i)).unwrap(),
                Some(v1.clone()),
                "key {} should have v1 after compaction",
                i
            );
        }
        // (b) Deleted keys are gone (tombstones removed).
        for i in 500..700 {
            assert_eq!(
                store.get(&key(i)).unwrap(),
                None,
                "deleted key {} should be None after compaction",
                i
            );
        }

        // (c) Block count decreased.
        assert!(
            stats.blocks_after < stats.blocks_before,
            "blocks_after ({}) should be < blocks_before ({})",
            stats.blocks_after,
            stats.blocks_before
        );
        // (d) Total bytes decreased.
        assert!(
            stats.bytes_reclaimed > 0,
            "bytes_reclaimed should be > 0, got {}",
            stats.bytes_reclaimed
        );
        // (e) Row count decreased.
        assert!(
            stats.rows_after < stats.rows_before,
            "rows_after ({}) should be < rows_before ({})",
            stats.rows_after,
            stats.rows_before
        );
        // rows_after should equal the survivor count: 500 (v2) + 300 (v1) = 800.
        assert_eq!(stats.rows_after, 800, "expected 800 surviving rows");
    }

    /// Compaction must preserve the *latest* value for each key, not the
    /// first. This guards against a bug where compaction would pick the
    /// oldest version instead of the newest.
    #[test]
    fn compact_keeps_latest_value_not_oldest() {
        let mut store = KvStore::new(SegmentId(1), 16 * 1024);
        // Three versions of the same key, each in its own flush (so they
        // land in different blocks).
        store.put(b"k", b"v1").unwrap();
        store.flush_pending().unwrap();
        store.put(b"k", b"v2").unwrap();
        store.flush_pending().unwrap();
        store.put(b"k", b"v3").unwrap();
        store.flush_pending().unwrap();

        assert_eq!(store.get(b"k").unwrap(), Some(b"v3".to_vec()));
        assert_eq!(store.block_count(), 3, "expected 3 blocks (one per version)");

        let stats = store.compact().unwrap();
        assert_eq!(stats.blocks_before, 3);
        assert_eq!(stats.blocks_after, 1);
        assert_eq!(stats.rows_before, 3);
        assert_eq!(stats.rows_after, 1);
        // Latest value (v3) must survive.
        assert_eq!(store.get(b"k").unwrap(), Some(b"v3".to_vec()));
    }

    /// Compaction removes tombstones: after deleting every key and
    /// compacting, the store should be empty (no blocks, no index entries).
    #[test]
    fn compact_removes_all_tombstones_when_store_is_empty() {
        let mut store = KvStore::new(SegmentId(1), 16 * 1024);
        for i in 0..100u32 {
            store.put(&key(i), b"v").unwrap();
        }
        store.flush_pending().unwrap();
        for i in 0..100u32 {
            assert!(store.delete(&key(i)).unwrap());
        }
        store.flush_pending().unwrap();
        assert!(store.block_count() > 0, "tombstones should occupy blocks");

        let stats = store.compact().unwrap();
        assert_eq!(stats.blocks_after, 0, "no survivors → no blocks");
        assert_eq!(stats.rows_after, 0);
        // Since blocks_after == 0, every byte from blocks_before must
        // have been reclaimed.
        assert!(stats.bytes_reclaimed > 0);
        // Every key must read back as None.
        for i in 0..100u32 {
            assert_eq!(store.get(&key(i)).unwrap(), None);
        }
        // iter() should yield nothing.
        let collected: Vec<_> = store.iter().collect();
        assert!(collected.is_empty());
    }

    /// Compaction is a no-op when there are no stale rows or tombstones.
    /// The block/row counts should be unchanged (or smaller only if the
    /// original blocks were under-full — but with a single dense flush,
    /// they should be equal).
    #[test]
    fn compact_is_noop_without_stale_rows() {
        let mut store = KvStore::new(SegmentId(1), 16 * 1024);
        for i in 0..50u32 {
            store.put(&key(i), b"value").unwrap();
        }
        store.flush_pending().unwrap();
        let blocks_before = store.block_count();
        let bytes_before = store.sealed_bytes();

        let stats = store.compact().unwrap();

        assert_eq!(stats.blocks_before, blocks_before);
        assert_eq!(stats.blocks_after, blocks_before);
        assert_eq!(stats.rows_before, 50);
        assert_eq!(stats.rows_after, 50);
        // bytes_reclaimed may be 0 (already dense) or small (re-pack
        // saved a few bytes of overhead). It must NOT be negative.
        assert!(
            stats.bytes_reclaimed <= bytes_before,
            "reclaimed {} exceeds total bytes {}",
            stats.bytes_reclaimed,
            bytes_before
        );
        // All keys still readable.
        for i in 0..50u32 {
            assert_eq!(store.get(&key(i)).unwrap(), Some(b"value".to_vec()));
        }
    }

    /// Compaction must work when there are no sealed blocks at all (the
    /// store is empty). It should return a no-op stats record.
    #[test]
    fn compact_on_empty_store_is_noop() {
        let mut store = KvStore::new(SegmentId(1), 16 * 1024);
        let stats = store.compact().unwrap();
        assert_eq!(stats.blocks_before, 0);
        assert_eq!(stats.blocks_after, 0);
        assert_eq!(stats.rows_before, 0);
        assert_eq!(stats.rows_after, 0);
        assert_eq!(stats.bytes_reclaimed, 0);
        assert!(!stats.reclaimed_anything());
    }

    /// Compaction must flush pending writes first, so any unflushed puts
    /// are included in the compacted output. (Otherwise pending writes
    /// would be lost when the index is cleared.)
    #[test]
    fn compact_flushes_pending_first() {
        let mut store = KvStore::new(SegmentId(1), 16 * 1024);
        // Sealed writes.
        for i in 0..10u32 {
            store.put(&key(i), b"sealed").unwrap();
        }
        store.flush_pending().unwrap();
        // Pending writes (not flushed).
        for i in 10..20u32 {
            store.put(&key(i), b"pending").unwrap();
        }
        assert_eq!(store.block_count(), 1, "10 small keys fit in one block");

        let stats = store.compact().unwrap();
        // All 20 keys must survive.
        for i in 0..10u32 {
            assert_eq!(store.get(&key(i)).unwrap(), Some(b"sealed".to_vec()));
        }
        for i in 10..20u32 {
            assert_eq!(store.get(&key(i)).unwrap(), Some(b"pending".to_vec()));
        }
        assert_eq!(stats.rows_after, 20);
    }

    /// Compaction must respect overwrites that happen in pending (not
    /// yet flushed). If a key was sealed with v1 and then overwritten
    /// with v2 in pending, compaction must keep v2 — not v1.
    #[test]
    fn compact_respects_pending_overwrites() {
        let mut store = KvStore::new(SegmentId(1), 16 * 1024);
        store.put(b"k", b"sealed-v1").unwrap();
        store.flush_pending().unwrap();
        // Overwrite in pending (not flushed).
        store.put(b"k", b"pending-v2").unwrap();
        // Before compaction, get returns the pending value.
        assert_eq!(store.get(b"k").unwrap(), Some(b"pending-v2".to_vec()));

        store.compact().unwrap();
        // After compaction, the pending value must win.
        assert_eq!(store.get(b"k").unwrap(), Some(b"pending-v2".to_vec()));
    }

    /// Compaction must be idempotent: calling it twice in a row should
    /// reclaim nothing on the second call.
    #[test]
    fn compact_is_idempotent() {
        let mut store = KvStore::new(SegmentId(1), 16 * 1024);
        // Use larger values so the data spans multiple blocks —
        // otherwise the first compaction may not reclaim any bytes
        // (everything fits in one block already).
        let v1 = vec![b'a'; 200];
        let v2 = vec![b'b'; 200];
        for i in 0..200u32 {
            store.put(&key(i), &v1).unwrap();
        }
        store.flush_pending().unwrap();
        // Create some stale rows. IMPORTANT: flush between phases so the
        // overwrites/deletes land in *new* blocks (otherwise they update
        // the pending HashMap in place and never create stale sealed rows).
        for i in 0..100u32 {
            store.put(&key(i), &v2).unwrap();
        }
        store.flush_pending().unwrap();
        for i in 100..150u32 {
            assert!(store.delete(&key(i)).unwrap());
        }
        store.flush_pending().unwrap();
        assert!(
            store.block_count() > 1,
            "expected >1 block before compaction, got {}",
            store.block_count()
        );

        let first = store.compact().unwrap();
        assert!(
            first.reclaimed_anything(),
            "first compaction should reclaim something: {:?}",
            first
        );

        let second = store.compact().unwrap();
        assert_eq!(second.blocks_after, second.blocks_before);
        assert_eq!(second.rows_after, second.rows_before);
        assert_eq!(second.bytes_reclaimed, 0);
        assert!(!second.reclaimed_anything());
    }

    /// Compaction must produce a store that can still be persisted to a
    /// segment file and loaded back. This guards against the compaction
    /// leaving the in-memory state inconsistent with the block layout.
    #[test]
    fn compact_then_persist_then_load() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let path = dir.path().join("kv-compacted.cdb");

        let mut store = KvStore::new(SegmentId(7), 16 * 1024);
        for i in 0..200u32 {
            store.put(&key(i), b"v1").unwrap();
        }
        store.flush_pending().unwrap();
        // Overwrite half to create stale rows.
        for i in 0..100u32 {
            store.put(&key(i), b"v2").unwrap();
        }
        store.flush_pending().unwrap();
        // Delete a quarter.
        for i in 100..150u32 {
            assert!(store.delete(&key(i)).unwrap());
        }
        store.flush_pending().unwrap();

        store.compact().unwrap();
        store.persist_to_segment(&path).unwrap();

        let loaded = KvStore::load_from_segment(&path, SegmentId(7), 16 * 1024).unwrap();
        // 100 v2 + 50 v1 = 150 surviving keys (the 50 deleted are gone).
        for i in 0..100u32 {
            assert_eq!(loaded.get(&key(i)).unwrap(), Some(b"v2".to_vec()));
        }
        for i in 150..200u32 {
            assert_eq!(loaded.get(&key(i)).unwrap(), Some(b"v1".to_vec()));
        }
        for i in 100..150u32 {
            assert_eq!(loaded.get(&key(i)).unwrap(), None);
        }
    }

    /// `iter()` after compaction must yield exactly the surviving keys
    /// (no tombstones, no stale versions).
    #[test]
    fn compact_then_iter_yields_survivors_only() {
        let mut store = KvStore::new(SegmentId(1), 16 * 1024);
        for i in 0..50u32 {
            store.put(&key(i), b"v1").unwrap();
        }
        store.flush_pending().unwrap();
        // Overwrite 20 keys (creates 20 stale rows).
        for i in 0..20u32 {
            store.put(&key(i), b"v2").unwrap();
        }
        store.flush_pending().unwrap();
        // Delete 10 keys (creates 10 tombstones).
        for i in 20..30u32 {
            assert!(store.delete(&key(i)).unwrap());
        }
        store.flush_pending().unwrap();

        store.compact().unwrap();

        let collected: std::collections::HashMap<Vec<u8>, Vec<u8>> =
            store.iter().collect();
        // 20 v2 (overwritten) + 20 v1 (untouched, keys 30..50) = 40 survivors.
        assert_eq!(collected.len(), 40, "iter should yield 40 survivors");
        for i in 0..20u32 {
            assert_eq!(collected.get(&key(i)), Some(&b"v2".to_vec()));
        }
        for i in 30..50u32 {
            assert_eq!(collected.get(&key(i)), Some(&b"v1".to_vec()));
        }
        // Deleted keys (20..30) must not appear.
        for i in 20..30u32 {
            assert!(!collected.contains_key(&key(i)));
        }
    }
}

#[cfg(test)]
mod segment_persistence_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn persist_and_load_segment() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("kv.cdb");

        // Write 100 KV pairs.
        let mut store = KvStore::new(SegmentId(42), 16 * 1024);
        for i in 0..100i64 {
            store
                .put(format!("key_{:04}", i).as_bytes(), format!("value_{}", i).as_bytes())
                .unwrap();
        }
        store.seal().unwrap();
        store.persist_to_segment(&path).unwrap();

        // Load it back.
        let loaded = KvStore::load_from_segment(&path, SegmentId(42), 16 * 1024).unwrap();
        // The index should have 100 entries.
        assert_eq!(loaded.index.len(), 100);
    }
}
