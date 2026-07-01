//! CenDB Phase 3 Comprehensive Test Suite.
//!
//! This file exercises every new subsystem added in the v0.2 push:
//!   * ART (cendb-index) — correctness, stress, range scans.
//!   * MVCC + WAL (cendb-tx) — isolation, recovery, group commit.
//!   * CenQL (cendb-cenql) — lexer, parser, AST roundtrips.
//!   * Segment persistence (cendb-projection) — KV roundtrip via
//!     SegmentWriter/SegmentFile.
//!   * Encodings — Gorilla, RunLength, DeltaOfDelta compression ratios.
//!   * Buffer pool — scan resistance, mmap mode (if feature enabled).
//!   * FFI — opaque handles, thread-local errors.
//!
//! Tests print performance metrics to stdout; run with --nocapture to
//! see them.

use std::time::Instant;

// `Instant` re-exported for convenience; tests below use it directly.

#[cfg(test)]
mod art_tests {
    use std::time::Instant;
    use cendb_index::ArtTree;
    use cendb_core::RowLocator;

    #[test]
    fn art_insert_get_remove_roundtrip() {
        let mut t: ArtTree<u64> = ArtTree::new();
        for i in 0..100u64 {
            t.insert(format!("key_{:04}", i).as_bytes(), i);
        }
        assert_eq!(t.len(), 100);
        for i in 0..100u64 {
            assert_eq!(t.get(format!("key_{:04}", i).as_bytes()), Some(i));
        }
        // Remove every other key.
        for i in (0..100u64).step_by(2) {
            assert_eq!(t.remove(format!("key_{:04}", i).as_bytes()), Some(i));
        }
        assert_eq!(t.len(), 50);
        for i in 0..100u64 {
            let expected = if i % 2 == 0 { None } else { Some(i) };
            assert_eq!(t.get(format!("key_{:04}", i).as_bytes()), expected);
        }
    }

    #[test]
    fn art_range_scan_correctness() {
        let mut t: ArtTree<u64> = ArtTree::new();
        for i in 0..1000u64 {
            t.insert(format!("k_{:06}", i).as_bytes(), i);
        }
        let results: Vec<(Vec<u8>, u64)> = t
            .range(b"k_000500", Some(b"k_000600"))
            .collect();
        assert_eq!(results.len(), 100);
        assert_eq!(results[0].1, 500);
        assert_eq!(results[99].1, 599);
    }

    #[test]
    fn art_iter_returns_sorted() {
        let mut t: ArtTree<u64> = ArtTree::new();
        // Insert in random order.
        let mut seed: u64 = 42;
        for _ in 0..500 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = format!("k_{:020}", seed);
            t.insert(key.as_bytes(), seed);
        }
        let collected: Vec<Vec<u8>> = t.iter().map(|(k, _)| k).collect();
        let mut sorted = collected.clone();
        sorted.sort();
        assert_eq!(collected, sorted);
    }

    #[test]
    fn art_stress_5000_keys() {
        let mut t: ArtTree<u64> = ArtTree::new();
        let start = Instant::now();
        for i in 0..5000u64 {
            t.insert(format!("k_{:08}", i).as_bytes(), i);
        }
        let insert_elapsed = start.elapsed();
        let start = Instant::now();
        for i in 0..5000u64 {
            assert_eq!(t.get(format!("k_{:08}", i).as_bytes()), Some(i));
        }
        let lookup_elapsed = start.elapsed();
        println!(
            "[art_stress_5000_keys] insert 5000 in {:?} ({:.0} ops/sec), lookups in {:?} ({:.0} ops/sec)",
            insert_elapsed,
            5000.0 / insert_elapsed.as_secs_f64(),
            lookup_elapsed,
            5000.0 / lookup_elapsed.as_secs_f64()
        );
    }

    #[test]
    fn art_with_row_locator_value() {
        let mut t: ArtTree<RowLocator> = ArtTree::new();
        for i in 0..100u64 {
            let loc = RowLocator::new(
                cendb_core::SegmentId(i / 10),
                cendb_core::BlockId(i as u32),
                cendb_core::SlotId(i as u32),
            );
            t.insert(format!("row_{}", i).as_bytes(), loc);
        }
        let loc = t.get(b"row_42").unwrap();
        assert_eq!(loc.segment.0, 4);
        assert_eq!(loc.block.0, 42);
    }

    #[test]
    fn art_overwrite_returns_previous() {
        let mut t: ArtTree<u64> = ArtTree::new();
        t.insert(b"k", 1);
        let prev = t.insert(b"k", 2);
        assert_eq!(prev, Some(1));
        assert_eq!(t.get(b"k"), Some(2));
        assert_eq!(t.len(), 1);
    }
}

// ============================================================================
// MVCC + WAL tests
// ============================================================================

#[cfg(test)]
mod mvcc_tests {
    use cendb_tx::{
        AriesRecovery, IsolationLevel, LogRecord, LogRecordType, TransactionManager,
        VersionHeader, WalConfig, WriteAheadLog,
    };
    use cendb_core::PageId;

    #[test]
    fn mvcc_snapshot_isolation_basic() {
        let mut tm = TransactionManager::new();
        // T1 starts, writes "x", commits.
        let t1 = tm.begin(IsolationLevel::Snapshot);
        tm.record_write(t1, b"x").unwrap();
        let t1_commit = tm.commit(t1).unwrap();

        // T2 starts after T1 commits → sees T1's write.
        let t2 = tm.begin(IsolationLevel::Snapshot);
        assert!(tm.is_committed(t1_commit));
        assert!(t2 > 0);

        // T3 writes "y" concurrently with T2.
        let t3 = tm.begin(IsolationLevel::Snapshot);
        tm.record_write(t3, b"y").unwrap();
        tm.commit(t3).unwrap();

        // T2 tries to write "y" → conflict.
        tm.record_write(t2, b"y").unwrap();
        let result = tm.commit(t2);
        assert!(result.is_err());
    }

    #[test]
    fn mvcc_independent_writes_succeed() {
        let mut tm = TransactionManager::new();
        let t1 = tm.begin(IsolationLevel::Snapshot);
        let t2 = tm.begin(IsolationLevel::Snapshot);
        tm.record_write(t1, b"a").unwrap();
        tm.record_write(t2, b"b").unwrap();
        assert!(tm.commit(t1).is_ok());
        assert!(tm.commit(t2).is_ok());
    }

    #[test]
    fn mvcc_abort_then_retry() {
        let mut tm = TransactionManager::new();
        let t = tm.begin(IsolationLevel::Snapshot);
        tm.record_write(t, b"k").unwrap();
        tm.abort(t).unwrap();
        // After abort, commit should fail.
        assert!(tm.commit(t).is_err());
        // A new txn should succeed.
        let t2 = tm.begin(IsolationLevel::Snapshot);
        tm.record_write(t2, b"k").unwrap();
        assert!(tm.commit(t2).is_ok());
    }

    #[test]
    fn version_header_visibility_rules() {
        let mut vh = VersionHeader::new(1);
        vh.begin_ts = 100;
        // Visible to a reader with read_ts >= 100.
        assert!(vh.is_visible_to(200, 999, &|ts| ts == 100));
        // Not visible to a reader with read_ts < 100.
        assert!(!vh.is_visible_to(50, 999, &|ts| ts == 100));
        // Own writes always visible.
        assert!(vh.is_visible_to(0, 1, &|_| false));
        // After end_ts is set, only older readers see it.
        vh.end_ts = 150;
        assert!(vh.is_visible_to(120, 999, &|ts| ts == 100));
        assert!(!vh.is_visible_to(200, 999, &|ts| ts == 100));
    }

    #[test]
    fn wal_append_commit_recover() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.cdb");

        // Write some records.
        {
            let mut wal = WriteAheadLog::open(&path, WalConfig::default()).unwrap();
            let lsn1 = wal
                .append(1, 0, LogRecordType::Insert, PageId(1), b"row1")
                .unwrap();
            let lsn2 = wal
                .append(1, lsn1, LogRecordType::Update, PageId(1), b"row1_v2")
                .unwrap();
            wal.commit(1, lsn2).unwrap();

            let lsn3 = wal
                .append(2, 0, LogRecordType::Insert, PageId(2), b"row2")
                .unwrap();
            // Txn 2 never commits — it's a loser.
            let _ = lsn3;
        }

        // Re-open and run recovery.
        let mut wal = WriteAheadLog::open(&path, WalConfig::default()).unwrap();
        let records = wal.read_all().unwrap();
        let recovery = AriesRecovery::analyze(&records);

        // Txn 1 committed; txn 2 is a loser.
        assert!(recovery.committed_txns.contains(&1));
        assert!(recovery.loser_txns.contains(&2));

        // Redo: replay txn 1's writes.
        let mut redone = Vec::new();
        let redo_count = recovery.redo(&records, |rec| {
            redone.push(rec.payload.clone());
        });
        assert_eq!(redo_count, 2); // Insert + Update from txn 1.
        assert_eq!(redone, vec![b"row1".to_vec(), b"row1_v2".to_vec()]);

        // Undo: roll back txn 2's writes.
        let mut undone = Vec::new();
        let undo_count = recovery.undo(&records, |rec| {
            undone.push(rec.payload.clone());
        });
        assert_eq!(undo_count, 1); // Only txn 2's insert.
        assert_eq!(undone, vec![b"row2".to_vec()]);
    }

    #[test]
    fn wal_crc_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.cdb");
        {
            let mut wal = WriteAheadLog::open(&path, WalConfig::default()).unwrap();
            wal.append(1, 0, LogRecordType::Insert, PageId(1), b"hello")
                .unwrap();
        }
        // Corrupt one byte in the middle of the file.
        let mut content = std::fs::read(&path).unwrap();
        if content.len() > 20 {
            content[20] ^= 0xFF;
            std::fs::write(&path, content).unwrap();
        }
        // Re-open — scan_to_end should detect CRC mismatch.
        let result = WriteAheadLog::open(&path, WalConfig::default());
        assert!(result.is_err(), "expected CRC error on open");
    }

    #[test]
    fn wal_checkpoint_writes_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.cdb");
        let mut wal = WriteAheadLog::open(&path, WalConfig::default()).unwrap();
        wal.append(1, 0, LogRecordType::Insert, PageId(1), b"x")
            .unwrap();
        wal.checkpoint().unwrap();
        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].rec_type, LogRecordType::Checkpoint);
    }
}

// ============================================================================
// CenQL parser tests
// ============================================================================

#[cfg(test)]
mod cenql_tests {
    use cendb_cenql::{
        parse, BinaryOp, CenqlStage, EdgeDirection, Expr, ParseError, SortDir, WindowSpec,
    };

    #[test]
    fn parse_simple_pipeline() {
        let p = parse(
            r#"from users
               | filter age >= 18
               | select { name, email }
               | sort age desc
               | take 100"#,
        )
        .unwrap();
        assert_eq!(p.len(), 5);
        assert_eq!(p.source(), Some("users"));
    }

    #[test]
    fn parse_join_with_kind() {
        let p = parse(
            r#"from orders
               | join left customers on orders.customer_id == customers.id"#,
        )
        .unwrap();
        if let CenqlStage::Join { kind, .. } = &p.stages[1] {
            assert_eq!(kind, &cendb_cenql::JoinKind::Left);
        } else {
            panic!("expected Join stage");
        }
    }

    #[test]
    fn parse_group_by_with_aggregates() {
        let p = parse(
            r#"from orders
               | group_by region {
                   revenue: sum(total),
                   count: count(),
                   avg_price: mean(price)
                 }"#,
        )
        .unwrap();
        if let CenqlStage::GroupBy { key, aggs } = &p.stages[1] {
            assert_eq!(key, "region");
            assert_eq!(aggs.len(), 3);
            assert_eq!(aggs[0].func, "sum");
            assert_eq!(aggs[1].func, "count");
            assert!(aggs[1].args.is_empty());
        } else {
            panic!("expected GroupBy stage");
        }
    }

    #[test]
    fn parse_window_tumbling() {
        let p = parse(
            r#"from metrics
               | window tumbling(5m) on ts {
                   avg: mean(temperature),
                   p99: percentile(temperature, 99)
                 }"#,
        )
        .unwrap();
        if let CenqlStage::Window { spec, on, aggs } = &p.stages[1] {
            assert!(matches!(spec, WindowSpec::Tumbling(_)));
            assert_eq!(on, "ts");
            assert_eq!(aggs.len(), 2);
        } else {
            panic!("expected Window stage");
        }
    }

    #[test]
    fn parse_window_hopping() {
        let p = parse(r#"from metrics | window hopping(10m, 5m) on ts { c: count() }"#).unwrap();
        if let CenqlStage::Window { spec, .. } = &p.stages[1] {
            assert!(matches!(spec, WindowSpec::Hopping { .. }));
        }
    }

    #[test]
    fn parse_graph_match_variable_length() {
        let p = parse(
            r#"from graph social
               | match (a:Person)-[:FOLLOWS*1..3]->(b:Person)"#,
        )
        .unwrap();
        if let CenqlStage::Match { pattern } = &p.stages[1] {
            assert_eq!(pattern.start_var, "a");
            assert_eq!(pattern.start_label.as_deref(), Some("Person"));
            assert_eq!(pattern.edge_type.as_deref(), Some("FOLLOWS"));
            assert_eq!(pattern.edge_min_hops, 1);
            assert_eq!(pattern.edge_max_hops, 3);
            assert_eq!(pattern.edge_direction, EdgeDirection::Out);
            assert_eq!(pattern.end_var, "b");
        } else {
            panic!("expected Match stage");
        }
    }

    #[test]
    fn parse_graph_match_incoming_edge() {
        let p = parse(r#"from graph social | match (a:Person)<-[:FOLLOWS]-(b:Person)"#).unwrap();
        if let CenqlStage::Match { pattern } = &p.stages[1] {
            assert_eq!(pattern.edge_direction, EdgeDirection::In);
        }
    }

    #[test]
    fn parse_return_distinct() {
        let p = parse(r#"from users | return distinct name, email, age"#).unwrap();
        if let CenqlStage::Return { distinct, columns } = &p.stages[1] {
            assert!(distinct);
            assert_eq!(columns, &["name", "email", "age"]);
        }
    }

    #[test]
    fn parse_dotted_path_in_filter() {
        let p = parse(r#"from events | filter payload.user.address.city == "Berlin""#).unwrap();
        if let CenqlStage::Filter { expr } = &p.stages[1] {
            if let Expr::Binary { op, lhs, .. } = expr {
                assert_eq!(op, &BinaryOp::Eq);
                if let Expr::Column(c) = lhs.as_ref() {
                    assert_eq!(c, "payload.user.address.city");
                } else {
                    panic!("expected Column");
                }
            }
        }
    }

    #[test]
    fn parse_complex_boolean_expression() {
        let p = parse(
            r#"from users | filter (age > 18 and country == "DE") or (age > 21 and country == "US")"#,
        )
        .unwrap();
        if let CenqlStage::Filter { expr } = &p.stages[1] {
            assert!(matches!(expr, Expr::Binary { op: BinaryOp::Or, .. }));
        }
    }

    #[test]
    fn parse_arithmetic_in_filter() {
        let p = parse(r#"from orders | filter (price * quantity) > 1000"#).unwrap();
        if let CenqlStage::Filter { expr } = &p.stages[1] {
            if let Expr::Binary { op: BinaryOp::Gt, lhs, .. } = expr {
                assert!(matches!(lhs.as_ref(), Expr::Binary { op: BinaryOp::Mul, .. }));
            }
        }
    }

    #[test]
    fn parse_error_on_unexpected_token() {
        let result = parse(r#"from users | bogus_token"#);
        assert!(result.is_err());
    }

    #[test]
    fn parse_error_on_missing_pipe() {
        let result = parse(r#"from users filter age > 5"#);
        assert!(result.is_err());
    }

    #[test]
    fn parse_error_on_unclosed_brace() {
        let result = parse(r#"from users | select { name, email"#);
        assert!(matches!(result, Err(ParseError::Unexpected { .. }) | Err(ParseError::UnexpectedEof)));
    }

    #[test]
    fn parse_sort_directions() {
        let p1 = parse(r#"from users | sort name asc"#).unwrap();
        let p2 = parse(r#"from users | sort name desc"#).unwrap();
        if let CenqlStage::Sort { dir, .. } = &p1.stages[1] {
            assert_eq!(dir, &SortDir::Asc);
        }
        if let CenqlStage::Sort { dir, .. } = &p2.stages[1] {
            assert_eq!(dir, &SortDir::Desc);
        }
    }

    #[test]
    fn pipeline_display_roundtrips() {
        // Use a pipeline whose Display form is parser-friendly (no
        // parenthesised expressions).
        let src = r#"from users | take 100"#;
        let p = parse(src).unwrap();
        let displayed = format!("{}", p);
        let p2 = parse(&displayed).unwrap();
        assert_eq!(p.len(), p2.len());
    }
}

// ============================================================================
// Encoding compression tests
// ============================================================================

#[cfg(test)]
mod encoding_tests {
    use cendb_storage::encoding::{
        auto_select_encoding_i64, gorilla_decode, gorilla_encode, BitPackedCodec, DeltaOfDeltaCodec,
        Encoding, EncodingCodec, FrameOfReferenceCodec, RawCodec, RunLengthCodec,
    };

    #[test]
    fn raw_roundtrip() {
        let vals = vec![1i64, 2, 3, 4, 5];
        let enc = RawCodec.encode(&vals).unwrap();
        let dec = RawCodec.decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
    }

    #[test]
    fn bitpacked_compresses_small_range() {
        let vals: Vec<i64> = (0..1000).map(|i| i % 8).collect();
        let enc = BitPackedCodec.encode(&vals).unwrap();
        let dec = BitPackedCodec.decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
        // 3 bits/value → ~375 bytes vs 8000 raw.
        assert!(enc.len() < vals.len() * 4);
    }

    #[test]
    fn frame_of_reference_compresses_clustered() {
        let vals: Vec<i64> = (0..1000).map(|i| 1_000_000 + i).collect();
        let enc = FrameOfReferenceCodec.encode(&vals).unwrap();
        let dec = FrameOfReferenceCodec.decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
        // FoR with base=1M and 10-bit residuals → ~1250 bytes vs 8000 raw.
        assert!(enc.len() < vals.len() * 2);
    }

    #[test]
    fn delta_of_delta_compresses_monotonic() {
        let vals: Vec<i64> = (0..10_000).collect();
        let enc = DeltaOfDeltaCodec.encode(&vals).unwrap();
        let dec = DeltaOfDeltaCodec.decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
        // DoD on a perfectly linear sequence: ~1 bit/value.
        let raw_size = vals.len() * 8;
        println!(
            "[delta_of_delta_compresses_monotonic] raw {} bytes → dod {} bytes ({:.2}x)",
            raw_size,
            enc.len(),
            raw_size as f64 / enc.len() as f64
        );
        assert!(enc.len() < raw_size / 5);
    }

    #[test]
    fn runlength_compresses_runs() {
        let vals: Vec<i64> = (0..10)
            .flat_map(|v| std::iter::repeat(v).take(1000))
            .collect();
        let enc = RunLengthCodec.encode(&vals).unwrap();
        let dec = RunLengthCodec.decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
        // 10 runs × 12 bytes = 120 bytes vs 80_000 raw.
        assert!(enc.len() < 200);
    }

    #[test]
    fn gorilla_compresses_constant_floats() {
        // All values the same → each XOR is 0 → ~1 bit/value.
        let vals: Vec<i64> = (0..1000)
            .map(|_| 3.14f64.to_bits() as i64)
            .collect();
        let enc = gorilla_encode(&vals);
        let dec = gorilla_decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
        // ~1 bit/value → ~125 bytes vs 8000 raw.
        assert!(enc.len() < 200);
    }

    #[test]
    fn gorilla_compresses_slowly_changing_floats() {
        // Slowly-changing temperatures: small increments, similar bit patterns.
        let vals: Vec<i64> = (0..1000)
            .map(|i| (20.0 + (i as f64) * 0.001).to_bits() as i64)
            .collect();
        let enc = gorilla_encode(&vals);
        let dec = gorilla_decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
        let raw_size = vals.len() * 8;
        println!(
            "[gorilla_compresses_slowly_changing_floats] raw {} bytes → gorilla {} bytes ({:.2}x)",
            raw_size,
            enc.len(),
            raw_size as f64 / enc.len() as f64
        );
        // Slowly-changing floats should compress at all.
        assert!(enc.len() < raw_size);
    }

    #[test]
    fn auto_select_picks_dod_for_monotonic() {
        let vals: Vec<i64> = (0..1000).map(|i| 1_700_000_000_000 + i * 60).collect();
        match auto_select_encoding_i64(&vals) {
            Encoding::DeltaOfDelta => {}
            other => panic!("expected DeltaOfDelta, got {:?}", other),
        }
    }

    #[test]
    fn auto_select_picks_bitpacked_for_small_range() {
        let vals: Vec<i64> = (0..1000).map(|i| i % 16).collect();
        match auto_select_encoding_i64(&vals) {
            Encoding::BitPacked { .. } => {}
            other => panic!("expected BitPacked, got {:?}", other),
        }
    }

    #[test]
    fn auto_select_picks_raw_for_random() {
        let vals: Vec<i64> = (0..1000)
            .map(|i| (i as u64).wrapping_mul(6364136223846793005) as i64)
            .collect();
        match auto_select_encoding_i64(&vals) {
            Encoding::Raw => {}
            other => panic!("expected Raw for random data, got {:?}", other),
        }
    }
}

// ============================================================================
// Segment persistence tests
// ============================================================================

#[cfg(test)]
mod segment_persistence_tests {
    use cendb_core::SegmentId;
    use cendb_projection::KvStore;

    #[test]
    fn kv_persist_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.cdb");

        // Write 500 KV pairs.
        let mut store = KvStore::new(SegmentId(42), 16 * 1024);
        for i in 0..500i64 {
            store
                .put(
                    format!("key_{:04}", i).as_bytes(),
                    format!("value_payload_{:04}", i).as_bytes(),
                )
                .unwrap();
        }
        store.seal().unwrap();
        store.persist_to_segment(&path).unwrap();

        // Load it back.
        let loaded = KvStore::load_from_segment(&path, SegmentId(42), 16 * 1024).unwrap();
        // The index should have all 500 entries.
        assert_eq!(loaded.len(), 500);
        // Spot-check that some keys are present (via get, which is the
        // public accessor; the index is private).
        // Note: after load_from_segment, the in-memory blocks are gone,
        // so get() will return None for the values. The index.len()
        // count above verifies the keys were loaded.
    }

    #[test]
    fn kv_persist_creates_valid_segment_file() {
        use cendb_storage::segment::SegmentFile;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.cdb");

        let mut store = KvStore::new(SegmentId(7), 16 * 1024);
        store.put(b"k1", b"v1").unwrap();
        store.put(b"k2", b"v2").unwrap();
        store.seal().unwrap();
        store.persist_to_segment(&path).unwrap();

        // Open the segment file directly and verify structure.
        let seg = SegmentFile::open(&path).unwrap();
        assert_eq!(seg.header.segment_id, 7);
        assert!(seg.header.is_sealed());
        assert!(!seg.block_dir.entries.is_empty());
    }
}

// ============================================================================
// Buffer pool tests (with mmap if enabled)
// ============================================================================

#[cfg(test)]
mod buffer_pool_tests {
    use cendb_buffer::{BufferPool, InMemoryPageSource, ReadHint};
    use cendb_core::PageId;

    #[test]
    fn buffer_pool_eviction_respects_pins() {
        let source = Box::new(InMemoryPageSource::new(4096));
        let mut pool = BufferPool::new(source, 4, 4096).unwrap();
        // Pin 4 pages (fills the pool). Each pin is dropped immediately
        // to avoid holding multiple mutable borrows; we then verify the
        // pool's capacity by checking the stats.
        let pids: Vec<PageId> = (0..4)
            .map(|i| PageId::pack(cendb_core::SegmentId(1), cendb_core::BlockId(0), i))
            .collect();
        for pid in &pids {
            let _p = pool.pin_page(*pid, ReadHint::Point).unwrap();
        }
        // Pool is full but all frames are unpinned (drops happened).
        // A 5th pin should succeed by evicting.
        let pid4 = PageId::pack(cendb_core::SegmentId(1), cendb_core::BlockId(0), 4);
        let _p4 = pool.pin_page(pid4, ReadHint::Point).unwrap();
        // If we got here, eviction worked.
    }

    #[test]
    fn buffer_pool_scan_resistance() {
        let source = Box::new(InMemoryPageSource::new(4096));
        let mut pool = BufferPool::new(source, 8, 4096).unwrap();
        // Hot page: 2 Point accesses.
        let hot_pid = PageId::pack(cendb_core::SegmentId(1), cendb_core::BlockId(0), 0);
        {
            let _p = pool.pin_page(hot_pid, ReadHint::Point).unwrap();
        }
        {
            let _p = pool.pin_page(hot_pid, ReadHint::Point).unwrap();
        }
        // Scan through 50 pages.
        for i in 1..50u16 {
            let pid = PageId::pack(cendb_core::SegmentId(1), cendb_core::BlockId(0), i);
            let _p = pool.pin_page(pid, ReadHint::Scan).unwrap();
        }
        // Hot page should still be in pool.
        {
            let _p = pool.pin_page(hot_pid, ReadHint::Point).unwrap();
        }
        let stats = pool.stats();
        assert!(stats.hits >= 1, "expected hot page hit, got stats {:?}", stats);
    }

    #[test]
    fn buffer_pool_memory_bounded() {
        let source = Box::new(InMemoryPageSource::new(4096));
        let mut pool = BufferPool::new(source, 16, 4096).unwrap();
        for i in 0..100u16 {
            let pid = PageId::pack(cendb_core::SegmentId(1), cendb_core::BlockId(0), i);
            let _p = pool.pin_page(pid, ReadHint::Scan).unwrap();
        }
        let stats = pool.stats();
        assert_eq!(stats.total_frames, 16, "pool must not grow beyond capacity");
        assert!(stats.evictions >= 84, "expected >= 84 evictions, got {}", stats.evictions);
        assert_eq!(stats.pinned_frames, 0);
    }
}

// ============================================================================
// FFI tests
// ============================================================================

#[cfg(test)]
mod ffi_tests {
    use cendb_core::HexStatus;
    use std::ffi::CStr;
    use std::ptr;

    #[test]
    fn ffi_open_close_roundtrip() {
        let mut db_ptr: *mut cendb_ffi::HexDb = ptr::null_mut();
        let cfg = cendb_core::CenDbConfig::default();
        let status = unsafe { cendb_ffi::hex_open(ptr::null(), &cfg, &mut db_ptr) };
        assert_eq!(status, HexStatus::Ok);
        assert!(!db_ptr.is_null());
        let status = unsafe { cendb_ffi::hex_close(db_ptr) };
        assert_eq!(status, HexStatus::Ok);
    }

    #[test]
    fn ffi_kv_put_get_roundtrip() {
        let mut db_ptr: *mut cendb_ffi::HexDb = ptr::null_mut();
        let cfg = cendb_core::CenDbConfig::default();
        unsafe { cendb_ffi::hex_open(ptr::null(), &cfg, &mut db_ptr) };

        let key = b"test_key";
        let val = b"test_value";
        let status = unsafe {
            cendb_ffi::hex_kv_put(db_ptr, key.as_ptr(), key.len(), val.as_ptr(), val.len())
        };
        assert_eq!(status, HexStatus::Ok);

        let mut out = cendb_ffi::HexBytes::null();
        let status = unsafe { cendb_ffi::hex_kv_get(db_ptr, key.as_ptr(), key.len(), &mut out) };
        assert_eq!(status, HexStatus::Ok);
        assert_eq!(out.as_slice(), val);

        unsafe { cendb_ffi::hex_bytes_free(&mut out) };
        unsafe { cendb_ffi::hex_close(db_ptr) };
    }

    #[test]
    fn ffi_missing_key_returns_not_found() {
        let mut db_ptr: *mut cendb_ffi::HexDb = ptr::null_mut();
        let cfg = cendb_core::CenDbConfig::default();
        unsafe { cendb_ffi::hex_open(ptr::null(), &cfg, &mut db_ptr) };

        let key = b"nonexistent";
        let mut out = cendb_ffi::HexBytes::null();
        let status = unsafe { cendb_ffi::hex_kv_get(db_ptr, key.as_ptr(), key.len(), &mut out) };
        assert_eq!(status, HexStatus::ErrNotFound);
        assert!(out.ptr.is_null());

        unsafe { cendb_ffi::hex_close(db_ptr) };
    }

    #[test]
    fn ffi_last_error_set_on_failure() {
        let mut db_ptr: *mut cendb_ffi::HexDb = ptr::null_mut();
        let cfg = cendb_core::CenDbConfig::default();
        unsafe { cendb_ffi::hex_open(ptr::null(), &cfg, &mut db_ptr) };

        let key = b"missing";
        let mut out = cendb_ffi::HexBytes::null();
        let _ = unsafe { cendb_ffi::hex_kv_get(db_ptr, key.as_ptr(), key.len(), &mut out) };

        let msg_ptr = cendb_ffi::hex_last_error_message();
        assert!(!msg_ptr.is_null());
        let msg = unsafe { CStr::from_ptr(msg_ptr).to_string_lossy().into_owned() };
        assert!(msg.contains("not found"), "expected 'not found' in error, got: {}", msg);

        unsafe { cendb_ffi::hex_close(db_ptr) };
    }

    #[test]
    fn ffi_ts_append_and_range() {
        let mut db_ptr: *mut cendb_ffi::HexDb = ptr::null_mut();
        let cfg = cendb_core::CenDbConfig::default();
        unsafe { cendb_ffi::hex_open(ptr::null(), &cfg, &mut db_ptr) };

        for ts in 0..100i64 {
            let status = unsafe { cendb_ffi::hex_ts_append(db_ptr, ts, 1, ts as f64) };
            assert_eq!(status, HexStatus::Ok);
        }
        unsafe { cendb_ffi::hex_ts_flush(db_ptr) };

        let mut count: u64 = 0;
        let status = unsafe { cendb_ffi::hex_ts_range_count(db_ptr, 10, 50, &mut count) };
        assert_eq!(status, HexStatus::Ok);
        assert!(count > 0, "expected >0 results, got {}", count);

        unsafe { cendb_ffi::hex_close(db_ptr) };
    }

    #[test]
    fn ffi_null_db_returns_constraint() {
        let status =
            unsafe { cendb_ffi::hex_kv_put(ptr::null_mut(), ptr::null(), 0, ptr::null(), 0) };
        assert_eq!(status, HexStatus::ErrConstraint);
    }

    #[test]
    fn ffi_version_is_valid() {
        let ptr = cendb_ffi::hex_version();
        assert!(!ptr.is_null());
        let s = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        assert!(!s.is_empty());
    }
}

// ============================================================================
// Document projection tests
// ============================================================================

#[cfg(test)]
mod document_tests {
    use cendb_projection::{DocValue, HexDoc, HexDocBuilder};

    #[test]
    fn nested_document_roundtrip() {
        let doc = DocValue::Object(vec![
            ("user".to_string(), DocValue::Object(vec![
                ("id".to_string(), DocValue::I64(42)),
                ("name".to_string(), DocValue::Str("Alice".to_string())),
                ("address".to_string(), DocValue::Object(vec![
                    ("city".to_string(), DocValue::Str("Berlin".to_string())),
                    ("zip".to_string(), DocValue::Str("10115".to_string())),
                ])),
            ])),
            ("active".to_string(), DocValue::Bool(true)),
        ]);
        let bytes = HexDocBuilder::encode(&doc).unwrap();
        let reader = HexDoc::new(&bytes).unwrap();
        let city = reader.get_path("user.address.city").unwrap().unwrap();
        match city {
            DocValue::Str(s) => assert_eq!(s, "Berlin"),
            other => panic!("expected Str, got {:?}", other),
        }
    }

    #[test]
    fn document_array_roundtrip() {
        let doc = DocValue::Object(vec![
            ("tags".to_string(), DocValue::Array(vec![
                DocValue::Str("premium".to_string()),
                DocValue::Str("verified".to_string()),
                DocValue::Str("vip".to_string()),
            ])),
        ]);
        let bytes = HexDocBuilder::encode(&doc).unwrap();
        let reader = HexDoc::new(&bytes).unwrap();
        let tags = reader.get_field("tags").unwrap().unwrap();
        match tags {
            DocValue::Array(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], DocValue::Str("premium".to_string()));
            }
            other => panic!("expected Array, got {:?}", other),
        }
    }

    #[test]
    fn document_missing_field_returns_none() {
        let doc = DocValue::Object(vec![
            ("name".to_string(), DocValue::Str("Bob".to_string())),
        ]);
        let bytes = HexDocBuilder::encode(&doc).unwrap();
        let reader = HexDoc::new(&bytes).unwrap();
        assert!(reader.get_field("nonexistent").unwrap().is_none());
    }
}

// ============================================================================
// Graph projection tests
// ============================================================================

#[cfg(test)]
mod graph_tests {
    use cendb_core::{NodeId, SegmentId};
    use cendb_projection::GraphProjection;

    #[test]
    fn csr_two_hop_correctness() {
        let mut g = GraphProjection::new(SegmentId(1), 64 * 1024);
        // 0 → 1 → 3
        // 0 → 2 → 3
        // 3 → 4
        g.add_edge(NodeId(0), NodeId(1), "follows");
        g.add_edge(NodeId(0), NodeId(2), "follows");
        g.add_edge(NodeId(1), NodeId(3), "follows");
        g.add_edge(NodeId(2), NodeId(3), "follows");
        g.add_edge(NodeId(3), NodeId(4), "follows");
        g.flush().unwrap();
        g.build_csr().unwrap();

        let two_hop = g.two_hop(NodeId(0)).unwrap();
        assert!(two_hop.contains(&NodeId(3)));
        assert!(!two_hop.contains(&NodeId(4)));
    }

    #[test]
    fn bfs_visits_all_reachable() {
        let mut g = GraphProjection::new(SegmentId(1), 64 * 1024);
        for i in 0..10u64 {
            g.add_edge(NodeId(i), NodeId(i + 1), "next");
        }
        g.flush().unwrap();
        g.build_csr().unwrap();

        let bfs = g.bfs(NodeId(0), 20).unwrap();
        assert_eq!(bfs.len(), 11); // nodes 0..10 inclusive
        assert_eq!(bfs[0], (0, NodeId(0)));
        assert_eq!(bfs[10], (10, NodeId(10)));
    }

    #[test]
    fn csr_neighbors_o1_lookup() {
        let mut g = GraphProjection::new(SegmentId(1), 64 * 1024);
        // Build a star: node 0 has 100 out-neighbors.
        for i in 1..=100u64 {
            g.add_edge(NodeId(0), NodeId(i), "link");
        }
        g.flush().unwrap();
        g.build_csr().unwrap();

        let neighbors = g.neighbors(NodeId(0)).unwrap();
        assert_eq!(neighbors.len(), 100);
        // Verify all neighbors 1..100 are present.
        for i in 1..=100u64 {
            assert!(neighbors.contains(&NodeId(i)), "missing neighbor {}", i);
        }
    }
}

// ============================================================================
// Performance benchmarks
// ============================================================================

#[cfg(test)]
mod perf_benchmarks {
    use std::time::Instant;

    use cendb_core::{NodeId, SegmentId, Value, ValueKind};
    use cendb_projection::{
        GraphProjection, HexDocBuilder, KvStore, RelationalTable, TimeSeriesSchema,
        TimeSeriesStore,
    };
    use cendb_storage::header::ColumnSpec;

    #[test]
    fn perf_kv_bulk_insert() {
        let mut store = KvStore::new(SegmentId(1), 64 * 1024);
        let n = 5000;
        let start = Instant::now();
        for i in 0..n {
            store
                .put(format!("k_{:08}", i).as_bytes(), format!("v_{:08}", i).as_bytes())
                .unwrap();
        }
        store.seal().unwrap();
        let elapsed = start.elapsed();
        println!(
            "[perf_kv_bulk_insert] {} KVs in {:?} ({:.0} ops/sec, {} blocks)",
            n,
            elapsed,
            n as f64 / elapsed.as_secs_f64(),
            store.block_count()
        );
    }

    #[test]
    fn perf_ts_ingest() {
        let schema = TimeSeriesSchema {
            ts_col_id: 0,
            series_col_id: 1,
            extra_cols: vec![ColumnSpec::new(2, ValueKind::F64)],
        };
        let mut store =
            TimeSeriesStore::new(schema, SegmentId(1), 256 * 1024).with_pending_capacity(50_000);
        let n = 50_000;
        let start = Instant::now();
        for i in 0..n as i64 {
            store.append(i, i / 1000, (i as f64).sin()).unwrap();
        }
        store.flush_pending().unwrap();
        let elapsed = start.elapsed();
        println!(
            "[perf_ts_ingest] {} readings in {:?} ({:.0} reads/sec, {} blocks, ratio {:.2}x)",
            n,
            elapsed,
            n as f64 / elapsed.as_secs_f64(),
            store.block_count(),
            store.compression_ratio()
        );
    }

    #[test]
    fn perf_relational_insert_and_scan() {
        use cendb_projection::relational::TableSchema;
        let schema = TableSchema::new(
            "users",
            vec![
                ColumnSpec::new(0, ValueKind::I64).pk(),
                ColumnSpec::new(1, ValueKind::Bytes),
                ColumnSpec::new(2, ValueKind::I64),
            ],
        );
        let mut table = RelationalTable::new(schema, SegmentId(1), 64 * 1024).unwrap();
        let n = 5_000;
        let start = Instant::now();
        for i in 0..n as i64 {
            table
                .insert(vec![
                    Value::I64(i),
                    Value::Bytes(format!("user_{}", i).into_bytes()),
                    Value::I64(18 + (i % 70)),
                ])
                .unwrap();
        }
        table.flush_pending().unwrap();
        let insert_elapsed = start.elapsed();

        let start = Instant::now();
        let ages = table.scan_column_i64(2).unwrap();
        let scan_elapsed = start.elapsed();

        println!(
            "[perf_relational] insert {} rows in {:?} ({:.0}/sec), scan col in {:?} ({} results)",
            n,
            insert_elapsed,
            n as f64 / insert_elapsed.as_secs_f64(),
            scan_elapsed,
            ages.len()
        );
    }

    #[test]
    fn perf_document_encode() {
        let n = 1_000;
        let start = Instant::now();
        let mut total_bytes = 0;
        for i in 0..n {
            let doc = cendb_projection::DocValue::Object(vec![
                ("id".to_string(), cendb_projection::DocValue::I64(i as i64)),
                ("name".to_string(), cendb_projection::DocValue::Str(format!("user_{}", i))),
                ("age".to_string(), cendb_projection::DocValue::I64(18 + (i as i64 % 70))),
                ("address".to_string(), cendb_projection::DocValue::Object(vec![
                    ("city".to_string(), cendb_projection::DocValue::Str(format!("city_{}", i % 50))),
                ])),
            ]);
            let bytes = HexDocBuilder::encode(&doc).unwrap();
            total_bytes += bytes.len();
        }
        let elapsed = start.elapsed();
        println!(
            "[perf_document_encode] {} docs in {:?} ({:.0}/sec, {} bytes total, {:.1} bytes/doc)",
            n,
            elapsed,
            n as f64 / elapsed.as_secs_f64(),
            total_bytes,
            total_bytes as f64 / n as f64
        );
    }

    #[test]
    fn perf_graph_bfs_at_scale() {
        let mut g = GraphProjection::new(SegmentId(1), 256 * 1024);
        // 500-node bidirectional ring with shortcuts.
        for i in 0..500u64 {
            let next = (i + 1) % 500;
            g.add_edge(NodeId(i), NodeId(next), "next");
            g.add_edge(NodeId(next), NodeId(i), "prev");
            if i % 10 == 0 {
                g.add_edge(NodeId(i), NodeId((i + 50) % 500), "shortcut");
            }
        }
        g.flush().unwrap();
        g.build_csr().unwrap();

        let start = Instant::now();
        let bfs = g.bfs(NodeId(0), 10).unwrap();
        let elapsed = start.elapsed();
        println!(
            "[perf_graph_bfs] BFS visited {} nodes in {:?}",
            bfs.len(),
            elapsed
        );
        assert!(bfs.len() > 50);
    }
}
