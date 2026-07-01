//! Focused benchmarks for the new v0.3 subsystems: CAS, optimizer, executor.

use std::time::Instant;

// ============================================================================
// CAS benchmarks
// ============================================================================

#[test]
fn bench_cas_blake3_throughput() {
    let sizes: &[(usize, &str)] = &[
        (1024, "1 KB"),
        (64 * 1024, "64 KB"),
        (1024 * 1024, "1 MB"),
        (8 * 1024 * 1024, "8 MB"),
    ];
    println!("\n=== BLAKE3 Hashing Throughput ===");
    println!("{:<12} {:>12} {:>12}", "Size", "Time", "Throughput");
    for &(size, label) in sizes {
        let data = vec![0xABu8; size];
        let start = Instant::now();
        let hash = cendb_cas::Hash::of(&data);
        let elapsed = start.elapsed();
        let mbps = (size as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();
        println!("{:<12} {:>12.3?} {:>8.0} MB/s", label, elapsed, mbps);
        let _ = hash;
    }
}

#[test]
fn bench_cas_put_get_latency() {
    use cendb_cas::BlobStore;
    let dir = tempfile::tempdir().unwrap();
    let mut store = BlobStore::open(dir.path()).unwrap();

    println!("\n=== CAS Put/Get Latency ===");
    println!("{:<12} {:>12} {:>12} {:>12}", "Blob Size", "Put", "Get", "Dedup Hit");

    let sizes: &[(usize, &str)] = &[
        (256, "256 B"),
        (4096, "4 KB"),
        (65536, "64 KB"),
        (1_048_576, "1 MB"),
    ];

    for &(size, label) in sizes {
        let data = vec![0x42u8; size];
        // First put (new blob).
        let start = Instant::now();
        let (hash, _) = store.put(&data).unwrap();
        let put_time = start.elapsed();

        // Get.
        let start = Instant::now();
        let _retrieved = store.get(&hash).unwrap();
        let get_time = start.elapsed();

        // Dedup put (same data).
        let start = Instant::now();
        let (_, is_new) = store.put(&data).unwrap();
        let dedup_time = start.elapsed();
        assert!(!is_new);

        println!(
            "{:<12} {:>12.3?} {:>12.3?} {:>12.3?}",
            label, put_time, get_time, dedup_time
        );
    }
}

#[test]
fn bench_cas_dedup_at_scale() {
    use cendb_cas::BlobStore;
    let dir = tempfile::tempdir().unwrap();
    let mut store = BlobStore::open(dir.path()).unwrap();

    // Simulate 10,000 uploads where 90% are duplicates.
    let unique_blobs = 1000;
    let total_uploads = 10_000;
    let blob_size = 64 * 1024; // 64 KB each

    let mut blobs: Vec<Vec<u8>> = Vec::new();
    for i in 0..unique_blobs {
        let mut data = vec![0u8; blob_size];
        data[0] = (i & 0xFF) as u8;
        data[1] = ((i >> 8) & 0xFF) as u8;
        blobs.push(data);
    }

    let start = Instant::now();
    let mut new_count = 0;
    let mut dedup_count = 0;
    for i in 0..total_uploads {
        let blob = &blobs[i % unique_blobs];
        let (_, is_new) = store.put(blob).unwrap();
        if is_new {
            new_count += 1;
        } else {
            dedup_count += 1;
        }
    }
    let elapsed = start.elapsed();

    let stats = store.stats();
    println!("\n=== CAS Deduplication at Scale ===");
    println!("  Total uploads:     {}", total_uploads);
    println!("  Unique blobs:      {}", unique_blobs);
    println!("  New writes:        {} ({:.1}%)", new_count, new_count as f64 / total_uploads as f64 * 100.0);
    println!("  Deduplicated:      {} ({:.1}%)", dedup_count, dedup_count as f64 / total_uploads as f64 * 100.0);
    println!("  Blobs on disk:     {}", stats.blob_count);
    println!("  Total data:        {:.1} MB", stats.total_size as f64 / 1024.0 / 1024.0);
    println!("  Stored on disk:    {:.1} MB", stats.total_stored_size as f64 / 1024.0 / 1024.0);
    println!("  Dedup savings:     {:.1} MB", stats.dedup_savings as f64 / 1024.0 / 1024.0);
    println!("  Total time:        {:?}", elapsed);
    println!("  Throughput:        {:.0} uploads/sec", total_uploads as f64 / elapsed.as_secs_f64());
    assert_eq!(stats.blob_count, unique_blobs as u64);
}

#[test]
fn bench_cas_compression_ratios() {
    use cendb_cas::{BlobStore, CompressionKind};
    let dir = tempfile::tempdir().unwrap();

    println!("\n=== CAS Compression Ratios ===");
    println!("{:<20} {:>12} {:>12} {:>12}", "Data Type", "Original", "Stored", "Ratio");

    let test_cases: Vec<(&str, Vec<u8>, CompressionKind)> = vec![
        ("All zeros (1 MB)", vec![0u8; 1_048_576], CompressionKind::Zstd),
        ("Repetitive text", b"hello world ".repeat(87000), CompressionKind::Zstd),
        ("Random data", {
            let mut d = Vec::with_capacity(65536);
            let mut s: u64 = 42;
            for _ in 0..8192 {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                d.extend_from_slice(&s.to_le_bytes());
            }
            d
        }, CompressionKind::None),
        ("Sparse (mostly 0)", {
            let mut d = vec![0u8; 1_048_576];
            for i in (0..d.len()).step_by(4096) {
                d[i] = 0xFF;
            }
            d
        }, CompressionKind::Zstd),
    ];

    for (label, data, expected) in test_cases {
        let mut store = BlobStore::open(dir.path().join(format!("test_{}", label.len()))).unwrap();
        let (hash, _) = store.put(&data).unwrap();
        let meta = store.meta(&hash).unwrap();
        let ratio = meta.size as f64 / meta.stored_size as f64;
        println!(
            "{:<20} {:>10.1} KB {:>10.1} KB {:>8.1}x ({:?})",
            label,
            meta.size as f64 / 1024.0,
            meta.stored_size as f64 / 1024.0,
            ratio,
            meta.compression
        );
        assert_eq!(meta.compression, expected);
    }
}

// ============================================================================
// Executor benchmarks
// ============================================================================

#[test]
fn bench_vectorized_filter_vs_naive() {
    use cendb_executor::{filter_i64_gt, sum_i64};

    let n = 10_000_000;
    let col: Vec<i64> = (0..n as i64).collect();

    println!("\n=== Vectorized Filter Performance ({} rows) ===", n);

    // Vectorized filter.
    let start = Instant::now();
    let sv = filter_i64_gt(&col, n as i64 / 2);
    let filter_time = start.elapsed();
    let filter_mps = n as f64 / filter_time.as_secs_f64() / 1_000_000.0;

    // Vectorized sum (all rows).
    let start = Instant::now();
    let total = sum_i64(&col);
    let sum_time = start.elapsed();
    let sum_mps = n as f64 / sum_time.as_secs_f64() / 1_000_000.0;

    // Gather + sum (filtered rows only).
    let start = Instant::now();
    let filtered_sum: i64 = sv.gather_i64(&col).iter().sum();
    let gather_time = start.elapsed();
    let gather_mps = n as f64 / gather_time.as_secs_f64() / 1_000_000.0;

    println!("  filter_i64_gt:  {:>10.3?}  ({:>6.0} M rows/sec, {} passed)", filter_time, filter_mps, sv.len());
    println!("  sum_i64:        {:>10.3?}  ({:>6.0} M rows/sec, sum={})", sum_time, sum_mps, total);
    println!("  gather+sum:     {:>10.3?}  ({:>6.0} M rows/sec, filtered_sum={})", gather_time, gather_mps, filtered_sum);

    assert_eq!(sv.len(), n / 2 - 1);
    assert_eq!(total, (n as i64 - 1) * n as i64 / 2);
}

#[test]
fn bench_vectorized_f64_filter() {
    use cendb_executor::{filter_f64_lt, sum_f64};

    let n = 1_000_000;
    let col: Vec<i64> = (0..n)
        .map(|i| ((i as f64) * 0.001).sin().to_bits() as i64)
        .collect();

    println!("\n=== Vectorized F64 Filter ({} rows) ===", n);

    let start = Instant::now();
    let sv = filter_f64_lt(&col, 0.5);
    let filter_time = start.elapsed();

    let start = Instant::now();
    let sum = sum_f64(&col);
    let sum_time = start.elapsed();

    println!(
        "  filter_f64_lt:  {:>10.3?}  ({:>6.0} M rows/sec, {} passed)",
        filter_time,
        n as f64 / filter_time.as_secs_f64() / 1_000_000.0,
        sv.len()
    );
    println!(
        "  sum_f64:        {:>10.3?}  ({:>6.0} M rows/sec, sum={:.4})",
        sum_time,
        n as f64 / sum_time.as_secs_f64() / 1_000_000.0,
        sum
    );
}

#[test]
fn bench_morsel_pipeline() {
    use cendb_executor::{Morsel, MorselBatch, filter_i64_gt, sum_i64};

    let total_rows = 5_000_000;
    println!("\n=== Morsel Pipeline ({} rows, 1024-row morsels) ===", total_rows);

    let start = Instant::now();
    let mut batch = MorselBatch::new();
    let mut morsel = Morsel::new(2); // (id, value)
    for i in 0..total_rows as i64 {
        morsel.push_row(&[i, i * 2]);
        if morsel.is_full() {
            batch.push(std::mem::replace(&mut morsel, Morsel::new(2)));
        }
    }
    if morsel.row_count > 0 {
        batch.push(morsel);
    }
    let build_time = start.elapsed();

    // Pipeline: filter (id > 50% of total) → sum values.
    let start = Instant::now();
    let mut total_passed = 0usize;
    let mut total_sum = 0i64;
    for m in &batch.morsels {
        let sv = filter_i64_gt(m.col(0), total_rows as i64 / 2);
        total_passed += sv.len();
        for &idx in &sv.indices {
            total_sum = total_sum.wrapping_add(m.col(1)[idx as usize]);
        }
    }
    let pipeline_time = start.elapsed();

    // Alternative: vectorized sum across morsels (no filter).
    let start = Instant::now();
    let full_sum: i64 = batch.morsels.iter().map(|m| sum_i64(m.col(1))).sum();
    let vec_sum_time = start.elapsed();

    println!("  Build morsels:       {:>10.3?}  ({} morsels)", build_time, batch.morsels.len());
    println!(
        "  Filter + sum:        {:>10.3?}  ({} passed, sum={})",
        pipeline_time, total_passed, total_sum
    );
    println!(
        "  Vectorized sum only: {:>10.3?}  (sum={})",
        vec_sum_time, full_sum
    );
    println!(
        "  Pipeline throughput: {:>6.0} M rows/sec",
        total_rows as f64 / pipeline_time.as_secs_f64() / 1_000_000.0
    );
}

// ============================================================================
// Optimizer benchmarks
// ============================================================================

#[test]
fn bench_optimizer_join_selection() {
    use cendb_optimizer::{
        ColumnStats, JoinMethod, LogicalPlan, Optimizer, PhysicalOperator, StatsCatalog,
        TableStats,
    };

    println!("\n=== Optimizer Join Method Selection ===");

    let scenarios: Vec<(&str, u64, u64, JoinMethod)> = vec![
        ("small x small (50 x 80)", 50, 80, JoinMethod::NestedLoop),
        ("small x large (50 x 1M)", 50, 1_000_000, JoinMethod::NestedLoop),
        ("medium x medium (5K x 8K)", 5_000, 8_000, JoinMethod::Hash),
        ("large x large (100K x 500K)", 100_000, 500_000, JoinMethod::Hash),
    ];

    println!("{:<35} {:>15} {:>15} {:>12}", "Scenario", "Est. Cost", "Est. Rows", "Method");
    for (label, left_rows, right_rows, expected) in scenarios {
        let mut catalog = StatsCatalog::new();
        catalog.register(TableStats::new("left", left_rows));
        catalog.register(TableStats::new("right", right_rows));
        let opt = Optimizer::new(catalog);
        let logical = LogicalPlan::Join {
            left: Box::new(LogicalPlan::Scan {
                table: "left".to_string(),
                predicate: None,
            }),
            right: Box::new(LogicalPlan::Scan {
                table: "right".to_string(),
                predicate: None,
            }),
            condition: "left.id == right.id".to_string(),
        };
        let physical = opt.optimize(&logical);
        if let PhysicalOperator::Join { method, .. } = &physical.operator {
            println!(
                "{:<35} {:>15.2} {:>15} {:>12}",
                label, physical.cost, physical.estimated_rows, method.as_str()
            );
            assert_eq!(*method, expected);
        }
    }
}

#[test]
fn bench_optimizer_cost_model_comparison() {
    use cendb_optimizer::CostModel;

    println!("\n=== Join Cost Model Comparison (equi-join) ===");

    let scenarios: Vec<(&str, u64, u64)> = vec![
        ("50 × 1M", 50, 1_000_000),
        ("1K × 100K", 1_000, 100_000),
        ("10K × 10K", 10_000, 10_000),
        ("100K × 500K", 100_000, 500_000),
        ("1M × 1M", 1_000_000, 1_000_000),
    ];

    println!(
        "{:<15} {:>15} {:>15} {:>15} {:>10}",
        "Join Sizes", "NestedLoop", "HashJoin", "MergeJoin", "Winner"
    );
    for (label, left, right) in scenarios {
        let inner_cost = CostModel::seq_scan(right, 64) / right as f64;
        let nl = CostModel::nested_loop_join(left, inner_cost, right);
        let hash = CostModel::hash_join(left.min(right), left.max(right));
        let merge = CostModel::merge_join(left, right);
        let winner = if nl <= hash && nl <= merge {
            "NL"
        } else if hash <= merge {
            "Hash"
        } else {
            "Merge"
        };
        println!(
            "{:<15} {:>15.2} {:>15.2} {:>15.2} {:>10}",
            label, nl, hash, merge, winner
        );
    }
}

// ============================================================================
// End-to-end throughput comparison
// ============================================================================

#[test]
fn bench_end_to_end_summary() {
    use cendb_core::{SegmentId, Value, ValueKind};
    use cendb_projection::{KvStore, TimeSeriesSchema, TimeSeriesStore};
    use cendb_storage::header::ColumnSpec;

    println!("\n=== End-to-End Throughput Summary ===");
    println!("{:<25} {:>15} {:>15}", "Operation", "Throughput", "Latency");

    // KV put.
    let mut kv = KvStore::new(SegmentId(1), 64 * 1024);
    let n = 10_000;
    let start = Instant::now();
    for i in 0..n {
        kv.put(format!("k_{:08}", i).as_bytes(), format!("v_{:08}", i).as_bytes())
            .unwrap();
    }
    let elapsed = start.elapsed();
    println!(
        "{:<25} {:>12.0}/s {:>12.1}µs",
        "KV put (batched)",
        n as f64 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1_000_000.0 / n as f64
    );

    // KV get (point lookup).
    let start = Instant::now();
    for i in 0..n {
        let _ = kv.get(format!("k_{:08}", i).as_bytes());
    }
    let elapsed = start.elapsed();
    println!(
        "{:<25} {:>12.0}/s {:>12.1}µs",
        "KV get (point lookup)",
        n as f64 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1_000_000.0 / n as f64
    );

    // TS ingest.
    let schema = TimeSeriesSchema {
        ts_col_id: 0,
        series_col_id: 1,
        extra_cols: vec![ColumnSpec::new(2, ValueKind::F64)],
    };
    let mut ts = TimeSeriesStore::new(schema, SegmentId(2), 256 * 1024)
        .with_pending_capacity(100_000);
    let n = 100_000;
    let start = Instant::now();
    for i in 0..n as i64 {
        ts.append(i, i / 1000, (i as f64).sin()).unwrap();
    }
    ts.flush_pending().unwrap();
    let elapsed = start.elapsed();
    println!(
        "{:<25} {:>12.0}/s {:>12.1}µs",
        "TS ingest (100K readings)",
        n as f64 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1_000_000.0 / n as f64
    );

    // TS range scan.
    let start = Instant::now();
    let (_, results) = ts.range_scan(0, 99_999).unwrap();
    let elapsed = start.elapsed();
    println!(
        "{:<25} {:>12.0}/s {:>12.1}µs",
        "TS range scan (100K rows)",
        n as f64 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1_000_000.0
    );
    assert_eq!(results.len(), n);

    // Graph BFS.
    use cendb_core::NodeId;
    use cendb_projection::GraphProjection;
    let mut g = GraphProjection::new(SegmentId(3), 256 * 1024);
    for i in 0..1000u64 {
        let next = (i + 1) % 1000;
        g.add_edge(NodeId(i), NodeId(next), "next");
    }
    g.flush().unwrap();
    g.build_csr().unwrap();
    let start = Instant::now();
    let bfs = g.bfs(NodeId(0), 999).unwrap();
    let elapsed = start.elapsed();
    println!(
        "{:<25} {:>12.0}/s {:>12.1}µs",
        "Graph BFS (1000 nodes)",
        bfs.len() as f64 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1_000_000.0
    );

    // Vectorized filter (1M rows).
    use cendb_executor::{filter_i64_gt, sum_i64};
    let col: Vec<i64> = (0..1_000_000).collect();
    let start = Instant::now();
    let sv = filter_i64_gt(&col, 500_000);
    let elapsed = start.elapsed();
    println!(
        "{:<25} {:>10.0} M/s {:>12.1}µs",
        "Vec filter (1M rows)",
        1_000_000.0 / elapsed.as_secs_f64() / 1_000_000.0,
        elapsed.as_secs_f64() * 1_000_000.0
    );

    // Vectorized sum (1M rows).
    let start = Instant::now();
    let sum = sum_i64(&col);
    let elapsed = start.elapsed();
    println!(
        "{:<25} {:>10.0} M/s {:>12.1}µs",
        "Vec sum (1M rows)",
        1_000_000.0 / elapsed.as_secs_f64() / 1_000_000.0,
        elapsed.as_secs_f64() * 1_000_000.0
    );
    let _ = sum;

    // CAS dedup (1000 × 64KB).
    use cendb_cas::BlobStore;
    let dir = tempfile::tempdir().unwrap();
    let mut store = BlobStore::open(dir.path()).unwrap();
    let blob = vec![0x42u8; 64 * 1024];
    let start = Instant::now();
    for _ in 0..1000 {
        store.put(&blob).unwrap();
    }
    let elapsed = start.elapsed();
    println!(
        "{:<25} {:>12.0}/s {:>12.1}µs",
        "CAS dedup (1K × 64KB)",
        1000.0 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1_000_000.0 / 1000.0
    );
}
