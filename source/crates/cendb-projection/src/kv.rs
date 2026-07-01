//! Key-Value projection: a degenerate 2-column table `(key BYTES, value BYTES)`
//! with an in-memory hash index for O(1) point lookups.
//!
//! This is the fast path described in §5/§6 of the spec: KV operations bypass
//! the query planner entirely and go straight from the hash index to the PAX
//! block. Range queries fall back to a linear scan of the block directory's
//! zone map.
//!
//! For the prototype we keep the index in memory; a production version would
//! spill it to a B-link tree on disk (§4.2 of the spec).

use std::collections::HashMap;
use std::path::Path;

use cendb_core::{BlockId, HexResult, SegmentId};
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
    /// Pending writes not yet flushed to a sealed block. We buffer them in
    /// memory and seal a new block when the buffer fills.
    pending: Vec<(Vec<u8>, Vec<u8>)>,
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
            pending: Vec::new(),
            pending_capacity: 1024,
        }
    }

    /// Insert (or overwrite) a key-value pair. The write is buffered in
    /// memory until a block is sealed.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> HexResult<()> {
        // If the key already exists in a sealed block, we record a
        // tombstone (for the prototype: just overwrite the index entry,
        // leaving a stale row in the old block — a production version
        // would mark the slot tombstoned).
        self.pending.push((key.to_vec(), value.to_vec()));
        if self.pending.len() >= self.pending_capacity {
            self.flush_pending()?;
        }
        Ok(())
    }

    /// Look up a key. Returns the value bytes if found.
    pub fn get(&self, key: &[u8]) -> HexResult<Option<Vec<u8>>> {
        // Check pending first (most recent writes).
        for (k, v) in self.pending.iter().rev() {
            if k == key {
                return Ok(Some(v.clone()));
            }
        }
        // Check the in-memory index.
        if let Some(&(block_id, slot)) = self.index.get(key) {
            let block = &self.blocks[block_id.0 as usize];
            // Column 2 is the value (schema: [pk_i64, key_bytes, value_bytes]).
            let value = block.var_value(2, slot as usize)?;
            return Ok(value.map(|b| b.to_vec()));
        }
        Ok(None)
    }

    /// Delete a key (insert a tombstone). Returns `Ok` if the key was
    /// previously present.
    pub fn delete(&mut self, key: &[u8]) -> HexResult<bool> {
        let existed = self.index.remove(key).is_some() || self.pending.iter().any(|(k, _)| k == key);
        if existed {
            // Mark as deleted by inserting an empty value (the canonical
            // tombstone marker for this prototype).
            self.pending.push((key.to_vec(), Vec::new()));
        }
        Ok(existed)
    }

    /// Force-flush the pending buffer into sealed PAX blocks. Multiple
    /// blocks may be created if the pending buffer is too large to fit
    /// in a single block.
    pub fn flush_pending(&mut self) -> HexResult<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let specs = kv_specs();
        // Pre-estimate the per-row byte cost so we can chunk the pending
        // buffer into block-sized batches without trial finalisation.
        // Per row: 8 (pk) + 8 (key slot header) + key.len() + 8 (val slot
        // header) + val.len() bytes of payload, plus alignment padding.
        // Block overhead: 64 (header) + 3*64 (column directory) + bitmap
        // space (~row_count/8 bytes).
        let block_overhead: usize = 64 + 3 * 64 + 256; // header + dir + bitmaps slack
        let usable = (self.block_size as usize).saturating_sub(block_overhead);

        let mut idx = 0usize;
        while idx < self.pending.len() {
            let chunk_start = idx;
            let mut chunk_bytes = 0usize;
            while idx < self.pending.len() {
                let (k, v) = &self.pending[idx];
                let row_bytes = 8 + 8 + k.len() + 8 + v.len() + 16; // +16 alignment slack
                if chunk_bytes + row_bytes > usable && idx > chunk_start {
                    break;
                }
                chunk_bytes += row_bytes;
                idx += 1;
            }
            // Build the block from pending[chunk_start..idx].
            let mut builder = PaxBlockBuilder::new(self.block_size, specs.clone())?;
            let mut new_index_entries: Vec<(Vec<u8>, u32)> = Vec::new();
            for (k, v) in self.pending[chunk_start..idx].iter() {
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
    pub fn seal(&mut self) -> HexResult<()> {
        self.flush_pending()
    }

    /// Persist the KV store to a segment file on disk. Writes all sealed
    /// blocks to the file at `path` and seals the segment.
    pub fn persist_to_segment(&mut self, path: impl AsRef<Path>) -> HexResult<()> {
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
    ) -> HexResult<Self> {
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
        // Pending first.
        if self.pending_idx < self.store.pending.len() {
            let (k, v) = &self.store.pending[self.pending_idx];
            self.pending_idx += 1;
            return Some((k.clone(), v.clone()));
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
    pub fn build_block<'a, I>(block_size: u32, pairs: I) -> HexResult<PaxBlock>
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
        // After delete, the value should be empty (tombstone).
        let v = store.get(b"alice").unwrap();
        assert!(v.is_some() && v.as_ref().unwrap().is_empty());
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
