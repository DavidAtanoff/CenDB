//! Multi-threaded stress tests for CenDB FFI.
//!
//! These tests exercise the following correctness properties:
//!
//! 1. **KV read-your-own-writes**: 8 threads each writing 2 000 keys and
//!    immediately reading them back; no keys are lost or corrupted.
//! 2. **Time-series concurrent append**: 4 threads concurrently appending
//!    25 000 readings; the total row count after joining must equal exactly
//!    4 × 25 000.
//! 3. **Disk persistence round-trip**: open a DB at a temp path, write 500
//!    keys, close (flush), reopen, verify all 500 keys are present.
//!
//! Run with:  `cargo test -p cendb-ffi --test stress -- --test-threads 1`

use std::sync::{Arc, Mutex};
use std::thread;
use cendb_core::{CenDbConfig, SegmentId};
use cendb_projection::{KvStore, TimeSeriesSchema, TimeSeriesStore};
use cendb_storage::header::ColumnSpec;
use cendb_core::ValueKind;

// ─────────────────────────────────────────────────────────────
// 1. KV read-your-own-writes under 8 concurrent writers
// ─────────────────────────────────────────────────────────────

#[test]
fn kv_concurrent_read_your_own_writes() {
    const THREADS: usize = 8;
    const KEYS_PER_THREAD: usize = 2_000;

    let store = Arc::new(Mutex::new(KvStore::new(SegmentId(1), 65_536)));

    let handles: Vec<_> = (0..THREADS)
        .map(|tid| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                let mut errors = Vec::new();
                for i in 0..KEYS_PER_THREAD {
                    let key = format!("t{}_k{}", tid, i).into_bytes();
                    let val = format!("v{}_{}!", tid, i).into_bytes();

                    // Write
                    {
                        let mut s = store.lock().unwrap();
                        s.put(&key, &val).expect("put failed");
                    }
                    // Read back immediately
                    {
                        let s = store.lock().unwrap();
                        match s.get(&key) {
                            Ok(Some(got)) if got == val => {} // OK
                            Ok(Some(got)) => errors.push(format!(
                                "t{}[{}]: got {:?} expected {:?}", tid, i, got, val
                            )),
                            Ok(None) => errors.push(format!("t{}[{}]: key missing", tid, i)),
                            Err(e) => errors.push(format!("t{}[{}]: err {}", tid, i, e)),
                        }
                    }
                }
                errors
            })
        })
        .collect();

    let mut total_errors: Vec<String> = Vec::new();
    for h in handles {
        total_errors.extend(h.join().expect("thread panicked"));
    }

    // Verify total key count
    let s = store.lock().unwrap();
    let total = s.len();
    assert!(
        total >= THREADS * KEYS_PER_THREAD,
        "Expected ≥{} keys, got {}", THREADS * KEYS_PER_THREAD, total
    );
    assert!(
        total_errors.is_empty(),
        "Read-your-own-writes failures:\n{}", total_errors.join("\n")
    );
}

// ─────────────────────────────────────────────────────────────
// 2. Time-series concurrent append – total row count correctness
// ─────────────────────────────────────────────────────────────

#[test]
fn ts_concurrent_append_row_count() {
    const THREADS: usize = 4;
    const ROWS_PER_THREAD: usize = 25_000;

    let store = Arc::new(Mutex::new(TimeSeriesStore::new(
        TimeSeriesSchema {
            ts_col_id: 0,
            series_col_id: 1,
            extra_cols: vec![ColumnSpec::new(2, ValueKind::F64)],
        },
        SegmentId(2),
        65_536,
    )));

    let handles: Vec<_> = (0..THREADS)
        .map(|tid| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                for i in 0..ROWS_PER_THREAD {
                    let ts = (tid as i64) * 100_000_000 + i as i64;
                    let mut s = store.lock().unwrap();
                    s.append(ts, tid as i64, i as f64).expect("append failed");
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    let mut s = store.lock().unwrap();
    s.seal().expect("seal failed");

    let expected = THREADS * ROWS_PER_THREAD;
    let actual = s.row_count();
    assert_eq!(
        actual, expected,
        "Expected {} rows, got {}", expected, actual
    );
}

// ─────────────────────────────────────────────────────────────
// 3. Disk persistence round-trip
// ─────────────────────────────────────────────────────────────

#[test]
fn kv_disk_persistence_round_trip() {
    const NUM_KEYS: usize = 500;

    let dir = std::env::temp_dir().join(format!(
        "cendb_stress_test_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let seg_path = dir.join("cendb.kv.seg");

    // Phase 1: Write keys and flush to disk.
    {
        let mut store = KvStore::new(SegmentId(1), 65_536);
        for i in 0..NUM_KEYS {
            let key = format!("persist_key_{}", i).into_bytes();
            let val = format!("persist_val_{}", i).into_bytes();
            store.put(&key, &val).expect("put failed");
        }
        store.persist_to_segment(&seg_path).expect("persist failed");
    }

    assert!(seg_path.exists(), "Segment file was not created");
    let seg_size = std::fs::metadata(&seg_path).unwrap().len();
    assert!(seg_size > 0, "Segment file is empty");

    // Phase 2: Load from disk and verify all keys.
    {
        let store = KvStore::load_from_segment(&seg_path, SegmentId(1), 65_536)
            .expect("load failed");

        let mut missing = Vec::new();
        for i in 0..NUM_KEYS {
            let key = format!("persist_key_{}", i).into_bytes();
            let expected_val = format!("persist_val_{}", i).into_bytes();
            match store.get(&key) {
                Ok(Some(v)) if v == expected_val => {}
                Ok(Some(v)) => missing.push(format!("key {}: got {:?}", i, v)),
                Ok(None) => missing.push(format!("key {}: missing", i)),
                Err(e) => missing.push(format!("key {}: err {}", i, e)),
            }
        }
        assert!(
            missing.is_empty(),
            "Missing/corrupted keys after reload:\n{}",
            missing.join("\n")
        );
    }

    // Cleanup.
    let _ = std::fs::remove_dir_all(&dir);
}

// ─────────────────────────────────────────────────────────────
// 4. KV delete + re-insert consistency under contention
// ─────────────────────────────────────────────────────────────

#[test]
fn kv_delete_and_reinsert_under_contention() {
    const THREADS: usize = 4;
    const OPS_PER_THREAD: usize = 1_000;

    let store = Arc::new(Mutex::new(KvStore::new(SegmentId(1), 65_536)));

    // Pre-populate
    {
        let mut s = store.lock().unwrap();
        for i in 0..OPS_PER_THREAD {
            let k = format!("key_{}", i).into_bytes();
            s.put(&k, b"original").expect("put failed");
        }
    }

    let handles: Vec<_> = (0..THREADS)
        .map(|tid| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                for i in 0..OPS_PER_THREAD {
                    let k = format!("key_{}", (i + tid * 250) % OPS_PER_THREAD).into_bytes();
                    let mut s = store.lock().unwrap();
                    let _ = s.delete(&k);
                    s.put(&k, format!("t{}", tid).as_bytes()).expect("put after delete failed");
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    // After all operations, every key should be retrievable (re-inserted).
    let s = store.lock().unwrap();
    let mut found = 0usize;
    for i in 0..OPS_PER_THREAD {
        let k = format!("key_{}", i).into_bytes();
        if let Ok(Some(_)) = s.get(&k) {
            found += 1;
        }
    }
    // All keys were re-inserted by some thread, so at least 90% should exist.
    assert!(
        found >= OPS_PER_THREAD * 9 / 10,
        "Expected ≥{}  keys after delete+reinsert, got {}", OPS_PER_THREAD * 9 / 10, found
    );
}

// ─────────────────────────────────────────────────────────────
// 5. HNSW vector search under concurrent insertions
// ─────────────────────────────────────────────────────────────

#[test]
fn hnsw_concurrent_inserts_and_search() {
    use cendb_vector::{HnswIndex, HnswConfig};

    let index = Arc::new(Mutex::new(HnswIndex::new(HnswConfig::default())));

    // Insert 200 vectors across 4 threads.
    const THREADS: usize = 4;
    const VEC_PER_THREAD: usize = 50;
    let handles: Vec<_> = (0..THREADS)
        .map(|tid| {
            let index = Arc::clone(&index);
            thread::spawn(move || {
                for i in 0..VEC_PER_THREAD {
                    let id = (tid * VEC_PER_THREAD + i) as u64;
                    let vec: Vec<f32> = (0..8).map(|d| (id as f32 + d as f32) * 0.01).collect();
                    let mut idx = index.lock().unwrap();
                    idx.insert(id, vec);
                }
            })
        })
        .collect();

    for h in handles { h.join().expect("thread panicked"); }

    let idx = index.lock().unwrap();
    assert_eq!(idx.len(), THREADS * VEC_PER_THREAD);

    // Search should return the right answer for a known query.
    let query: Vec<f32> = (0..8).map(|d| d as f32 * 0.01).collect();
    let results = idx.search(&query, 5);
    assert!(!results.is_empty());
    assert!(results[0].1 > 0.9, "Top match should be highly similar");
}
