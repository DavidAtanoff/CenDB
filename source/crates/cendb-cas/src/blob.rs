//! Blob store: content-addressable storage for large binary files.
//!
//! Blobs are stored in a dedicated directory (one file per blob, named by
//! hex hash). An in-memory ART index maps `Hash → BlobMeta` for O(k) point
//! lookups. Reference counting enables garbage collection of unreferenced
//! blobs.
//!
//! ## Compression
//!
//! Blobs can be stored:
//!   * `None` — raw bytes (already-compressed formats like JPEG/PNG/WebP).
//!   * `Zstd` — zstd-compressed (uncompressed data like raw bitmaps, CSV).
//!
//! The compression kind is recorded in the blob's metadata file and applied
//! transparently on read.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use cendb_core::{HexError, HexResult};
use cendb_index::ArtTree;

use crate::hash::Hash;

// ============================================================================
// Types.
// ============================================================================

/// Identifies a blob within the store. Internally this is the BLAKE3 hash
/// of the blob's content, but we expose it as a separate type for API
/// clarity.
pub type BlobId = Hash;

/// How the blob is stored on disk.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CompressionKind {
    /// Raw bytes — no compression (best for already-compressed formats).
    None = 0,
    /// Zstd compression (best for uncompressed data).
    Zstd = 1,
}

impl CompressionKind {
    pub fn from_u8(b: u8) -> Self {
        match b {
            1 => CompressionKind::Zstd,
            _ => CompressionKind::None,
        }
    }
}

/// Metadata for a stored blob.
#[derive(Clone, Debug)]
pub struct BlobMeta {
    /// The content hash (also the blob's ID).
    pub hash: Hash,
    /// Original (uncompressed) size in bytes.
    pub size: u64,
    /// On-disk (possibly compressed) size in bytes.
    pub stored_size: u64,
    /// Compression kind.
    pub compression: CompressionKind,
    /// Reference count — how many tables/columns reference this blob.
    pub refcount: u64,
}

/// Statistics snapshot for the blob store.
#[derive(Copy, Clone, Debug, Default)]
pub struct BlobStoreStats {
    pub blob_count: u64,
    pub total_size: u64,
    pub total_stored_size: u64,
    pub dedup_savings: u64,
    pub compression_savings: u64,
}

// ============================================================================
// Blob store.
// ============================================================================

/// Content-addressable blob store. Owns a directory on disk and an
/// in-memory ART index mapping `Hash → BlobMeta`.
pub struct BlobStore {
    /// Directory where blob files live.
    dir: PathBuf,
    /// In-memory index: hash bytes → blob metadata.
    index: ArtTree<BlobMeta>,
    /// Whether to compress new blobs by default.
    default_compression: CompressionKind,
}

impl BlobStore {
    /// Open (or create) a blob store at `dir`. The directory is created if
    /// it doesn't exist. Existing blobs are indexed on open.
    pub fn open(dir: impl AsRef<Path>) -> HexResult<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let mut store = Self {
            dir,
            index: ArtTree::new(),
            default_compression: CompressionKind::Zstd,
        };
        store.scan_existing()?;
        Ok(store)
    }

    /// Set the default compression for new blobs.
    pub fn with_default_compression(mut self, c: CompressionKind) -> Self {
        self.default_compression = c;
        self
    }

    fn scan_existing(&mut self) -> HexResult<()> {
        // Load the index from the index file if it exists.
        let index_path = self.dir.join("_index.cdb");
        if index_path.exists() {
            let bytes = fs::read(&index_path)?;
            self.load_index(&bytes)?;
        } else {
            // Scan the directory for .blob files and rebuild the index.
            for entry in fs::read_dir(&self.dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().map(|e| e == "blob").unwrap_or(false) {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        if let Ok(hash) = Hash::from_hex(stem) {
                            let meta_path = path.with_extension("meta");
                            if meta_path.exists() {
                                let meta_bytes = fs::read(&meta_path)?;
                                if let Ok(meta) = Self::deserialize_meta(&meta_bytes) {
                                    self.index.insert(hash.as_slice(), meta);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Store a blob. If the hash already exists, increments the refcount
    /// and returns the existing `BlobId` without re-writing the data
    /// (**deduplication**). Otherwise, compresses (if enabled), writes to
    /// disk, and adds to the index.
    ///
    /// Returns the `BlobId` (which is the content hash) and whether the
    /// blob was newly created (`true`) or deduplicated (`false`).
    pub fn put(&mut self, data: &[u8]) -> HexResult<(BlobId, bool)> {
        let hash = Hash::of(data);

        // Check for existing (deduplication fast path).
        if let Some(existing) = self.index.get(hash.as_slice()) {
            // Already exists — increment refcount.
            let mut meta = existing;
            meta.refcount += 1;
            self.index.insert(hash.as_slice(), meta.clone());
            self.persist_index()?;
            return Ok((hash, false));
        }

        // New blob — compress if enabled.
        let (stored_bytes, compression) = match self.default_compression {
            CompressionKind::None => (data.to_vec(), CompressionKind::None),
            CompressionKind::Zstd => {
                let compressed = zstd::encode_all(data, 3)
                    .map_err(|e| HexError::io(format!("zstd compress: {}", e)))?;
                // Only use compressed version if it's actually smaller.
                if compressed.len() < data.len() {
                    (compressed, CompressionKind::Zstd)
                } else {
                    (data.to_vec(), CompressionKind::None)
                }
            }
        };

        let blob_path = self.blob_path(&hash);
        let meta_path = self.meta_path(&hash);

        // Write the blob data.
        let mut file = fs::File::create(&blob_path)?;
        file.write_all(&stored_bytes)?;
        file.sync_all()?;

        // Write the metadata.
        let meta = BlobMeta {
            hash,
            size: data.len() as u64,
            stored_size: stored_bytes.len() as u64,
            compression,
            refcount: 1,
        };
        let meta_bytes = Self::serialize_meta(&meta);
        fs::write(&meta_path, meta_bytes)?;

        // Add to index.
        self.index.insert(hash.as_slice(), meta);
        self.persist_index()?;

        Ok((hash, true))
    }

    /// Retrieve a blob's content by hash. Decompresses if needed.
    pub fn get(&self, hash: &Hash) -> HexResult<Vec<u8>> {
        let meta = self
            .index
            .get(hash.as_slice())
            .ok_or_else(|| HexError::not_found(format!("blob {} not found", hash)))?;
        let blob_path = self.blob_path(hash);
        let stored = fs::read(&blob_path)?;
        let data = match meta.compression {
            CompressionKind::None => stored,
            CompressionKind::Zstd => {
                zstd::decode_all(&stored[..])
                    .map_err(|e| HexError::io(format!("zstd decompress: {}", e)))?
            }
        };
        Ok(data)
    }

    /// Get metadata for a blob without reading its content.
    pub fn meta(&self, hash: &Hash) -> Option<BlobMeta> {
        self.index.get(hash.as_slice())
    }

    /// Check whether a blob exists (without reading it).
    pub fn contains(&self, hash: &Hash) -> bool {
        self.index.get(hash.as_slice()).is_some()
    }

    /// Decrement the reference count. If it hits zero, the blob is deleted
    /// from disk (garbage collection).
    pub fn release(&mut self, hash: &Hash) -> HexResult<bool> {
        let meta = self
            .index
            .get(hash.as_slice())
            .ok_or_else(|| HexError::not_found(format!("blob {} not found", hash)))?;
        if meta.refcount <= 1 {
            // Delete the blob.
            let blob_path = self.blob_path(hash);
            let meta_path = self.meta_path(hash);
            if blob_path.exists() {
                fs::remove_file(&blob_path)?;
            }
            if meta_path.exists() {
                fs::remove_file(&meta_path)?;
            }
            self.index.remove(hash.as_slice());
            self.persist_index()?;
            Ok(true) // deleted
        } else {
            let mut updated = meta;
            updated.refcount -= 1;
            self.index.insert(hash.as_slice(), updated);
            self.persist_index()?;
            Ok(false) // refcount decremented
        }
    }

    /// Increment the reference count (e.g., when another table references
    /// an existing blob).
    pub fn retain(&mut self, hash: &Hash) -> HexResult<()> {
        let meta = self
            .index
            .get(hash.as_slice())
            .ok_or_else(|| HexError::not_found(format!("blob {} not found", hash)))?;
        let mut updated = meta;
        updated.refcount += 1;
        self.index.insert(hash.as_slice(), updated);
        self.persist_index()
    }

    /// Number of blobs in the store.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Compute statistics.
    pub fn stats(&self) -> BlobStoreStats {
        let mut s = BlobStoreStats::default();
        for (_, meta) in self.index.iter() {
            s.blob_count += 1;
            s.total_size += meta.size;
            s.total_stored_size += meta.stored_size;
            if meta.size > meta.stored_size {
                s.compression_savings += meta.size - meta.stored_size;
            }
            // Dedup savings: (refcount - 1) * size for each blob.
            if meta.refcount > 1 {
                s.dedup_savings += (meta.refcount - 1) * meta.size;
            }
        }
        s
    }

    /// Collect all (hash, meta) pairs into a Vec.
    pub fn list(&self) -> Vec<(Hash, BlobMeta)> {
        self.index
            .iter()
            .map(|(k, v)| {
                let mut hash = [0u8; 32];
                hash.copy_from_slice(&k);
                (Hash(hash), v)
            })
            .collect()
    }

    // ========================================================================
    // Internal helpers.
    // ========================================================================

    fn blob_path(&self, hash: &Hash) -> PathBuf {
        self.dir.join(format!("{}.blob", hash.to_hex()))
    }

    fn meta_path(&self, hash: &Hash) -> PathBuf {
        self.dir.join(format!("{}.meta", hash.to_hex()))
    }

    fn serialize_meta(meta: &BlobMeta) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&meta.hash.0);
        out.extend_from_slice(&meta.size.to_le_bytes());
        out.extend_from_slice(&meta.stored_size.to_le_bytes());
        out.push(meta.compression as u8);
        out.extend_from_slice(&meta.refcount.to_le_bytes());
        out
    }

    fn deserialize_meta(bytes: &[u8]) -> HexResult<BlobMeta> {
        if bytes.len() < 32 + 8 + 8 + 1 + 8 {
            return Err(HexError::corrupt("blob meta too short"));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[..32]);
        let size = u64::from_le_bytes([
            bytes[32], bytes[33], bytes[34], bytes[35], bytes[36], bytes[37], bytes[38], bytes[39],
        ]);
        let stored_size = u64::from_le_bytes([
            bytes[40], bytes[41], bytes[42], bytes[43], bytes[44], bytes[45], bytes[46], bytes[47],
        ]);
        let compression = CompressionKind::from_u8(bytes[48]);
        let refcount = u64::from_le_bytes([
            bytes[49], bytes[50], bytes[51], bytes[52], bytes[53], bytes[54], bytes[55], bytes[56],
        ]);
        Ok(BlobMeta {
            hash: Hash(hash),
            size,
            stored_size,
            compression,
            refcount,
        })
    }

    fn persist_index(&self) -> HexResult<()> {
        let index_path = self.dir.join("_index.cdb");
        let mut out = Vec::new();
        for (key, meta) in self.index.iter() {
            out.extend_from_slice(&key);
            out.extend_from_slice(&Self::serialize_meta(&meta));
        }
        fs::write(&index_path, out)?;
        Ok(())
    }

    fn load_index(&mut self, bytes: &[u8]) -> HexResult<()> {
        let record_size = 32 + 32 + 8 + 8 + 1 + 8; // key + meta
        let mut cursor = 0;
        while cursor + record_size <= bytes.len() {
            let key = &bytes[cursor..cursor + 32];
            let meta = Self::deserialize_meta(&bytes[cursor + 32..cursor + record_size])?;
            self.index.insert(key, meta);
            cursor += record_size;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn put_get_roundtrip() {
        let dir = tempdir().unwrap();
        let mut store = BlobStore::open(dir.path()).unwrap();
        let data = b"hello, world! This is a test blob.";
        let (hash, is_new) = store.put(data).unwrap();
        assert!(is_new);
        let retrieved = store.get(&hash).unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn deduplication_on_same_content() {
        let dir = tempdir().unwrap();
        let mut store = BlobStore::open(dir.path()).unwrap();
        let data = b"identical content";
        let (hash1, is_new1) = store.put(data).unwrap();
        let (hash2, is_new2) = store.put(data).unwrap();
        assert_eq!(hash1, hash2);
        assert!(is_new1);
        assert!(!is_new2); // deduplicated
        assert_eq!(store.len(), 1); // only one blob
        let meta = store.meta(&hash1).unwrap();
        assert_eq!(meta.refcount, 2);
    }

    #[test]
    fn different_content_gets_different_hash() {
        let dir = tempdir().unwrap();
        let mut store = BlobStore::open(dir.path()).unwrap();
        let (h1, _) = store.put(b"data one").unwrap();
        let (h2, _) = store.put(b"data two").unwrap();
        assert_ne!(h1, h2);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn release_decrements_refcount() {
        let dir = tempdir().unwrap();
        let mut store = BlobStore::open(dir.path()).unwrap();
        let (hash, _) = store.put(b"shared blob").unwrap();
        store.retain(&hash).unwrap(); // refcount = 2
        let deleted = store.release(&hash).unwrap();
        assert!(!deleted); // refcount went to 1, not deleted
        let deleted = store.release(&hash).unwrap();
        assert!(deleted); // refcount hit 0, deleted
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn large_blob_4k_image_simulated() {
        let dir = tempdir().unwrap();
        let mut store = BlobStore::open(dir.path()).unwrap();
        // Simulate a 4K image: 8 MB of pseudo-random data.
        let mut data = Vec::with_capacity(8 * 1024 * 1024);
        let mut seed: u64 = 42;
        for _ in 0..(8 * 1024 * 1024 / 8) {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            data.extend_from_slice(&seed.to_le_bytes());
        }
        let start = std::time::Instant::now();
        let (hash, is_new) = store.put(&data).unwrap();
        let put_elapsed = start.elapsed();
        let start = std::time::Instant::now();
        let retrieved = store.get(&hash).unwrap();
        let get_elapsed = start.elapsed();
        println!(
            "[large_blob] put 8MB in {:?} ({:.0} MB/s), get in {:?} ({:.0} MB/s)",
            put_elapsed,
            8.0 / put_elapsed.as_secs_f64(),
            get_elapsed,
            8.0 / get_elapsed.as_secs_f64()
        );
        assert!(is_new);
        assert_eq!(retrieved, data);
        let meta = store.meta(&hash).unwrap();
        println!(
            "[large_blob] size {} → stored {} (compression: {:.1}x)",
            meta.size,
            meta.stored_size,
            meta.size as f64 / meta.stored_size as f64
        );
    }

    #[test]
    fn compression_saves_space_for_redundant_data() {
        let dir = tempdir().unwrap();
        let mut store = BlobStore::open(dir.path()).unwrap();
        // Highly compressible data (all zeros).
        let data = vec![0u8; 1024 * 1024]; // 1 MB of zeros
        let (hash, _) = store.put(&data).unwrap();
        let meta = store.meta(&hash).unwrap();
        assert_eq!(meta.size, 1024 * 1024);
        assert!(meta.stored_size < 1024); // zstd should compress to < 1KB
        assert_eq!(meta.compression, CompressionKind::Zstd);
    }

    #[test]
    fn no_compression_for_already_compressed_data() {
        let dir = tempdir().unwrap();
        let mut store = BlobStore::open(dir.path()).unwrap();
        // Random data — incompressible.
        let mut data = Vec::with_capacity(65536);
        let mut seed: u64 = 42;
        for _ in 0..8192 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            data.extend_from_slice(&seed.to_le_bytes());
        }
        let (hash, _) = store.put(&data).unwrap();
        let meta = store.meta(&hash).unwrap();
        // Should fall back to None compression (stored >= original).
        assert_eq!(meta.compression, CompressionKind::None);
    }

    #[test]
    fn persistence_across_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let data = b"persistent blob";
        let hash = {
            let mut store = BlobStore::open(&path).unwrap();
            let (h, _) = store.put(data).unwrap();
            h
        };
        // Re-open and verify the blob is still there.
        let store = BlobStore::open(&path).unwrap();
        assert_eq!(store.len(), 1);
        let retrieved = store.get(&hash).unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn security_camera_deduplication_scenario() {
        // Simulate 1000 security cameras capturing the same background image.
        let dir = tempdir().unwrap();
        let mut store = BlobStore::open(dir.path()).unwrap();
        let background_image = vec![0xAAu8; 1024 * 768]; // 768KB "image"
        let mut hashes = Vec::new();
        for _ in 0..1000 {
            let (hash, _) = store.put(&background_image).unwrap();
            hashes.push(hash);
        }
        // All 1000 puts should produce the same hash.
        assert!(hashes.iter().all(|h| *h == hashes[0]));
        // The blob should be stored exactly once.
        assert_eq!(store.len(), 1);
        let stats = store.stats();
        assert_eq!(stats.blob_count, 1);
        // Dedup savings = 999 * 768KB.
        assert!(stats.dedup_savings > 0);
        println!(
            "[security_camera_dedup] 1000 identical 768KB images → 1 blob on disk (dedup savings: {:.1} MB)",
            stats.dedup_savings as f64 / (1024.0 * 1024.0)
        );
    }
}
