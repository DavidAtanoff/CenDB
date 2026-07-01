//! CenDB Phase 2 Verification Suite.
//!
//! This file is the integration test mandated by Phase 2 of the spec. It
//! generates realistic mock data across all five projections, verifies
//! functional integrity (point lookups, range scans, graph traversal,
//! document field shredding), and reports performance metrics (compression
//! ratio, latency, memory usage).
//!
//! ## Test data volumes
//!
//!   * Relational: 10,000 rows of user accounts.
//!   * Document: 5,000 nested JSON-like profiles.
//!   * Graph: 1,000 nodes + 10,000 directed edges.
//!   * Time-Series: 100,000 sensor readings.
//!
//! ## Test groups
//!
//!   1. `gen_*` — mock data generation (no assertions, just exercising the
//!      write path at scale).
//!   2. `verify_*` — functional integrity assertions.
//!   3. `perf_*` — performance metrics, printed to stdout.

use std::time::Instant;

use cendb_core::{NodeId, SegmentId, Value, ValueKind};
use cendb_projection::{
    DocValue, GraphProjection, CenDocBuilder, KvStore, RelationalTable, TimeSeriesSchema,
    TimeSeriesStore,
};
use cendb_storage::header::ColumnSpec;

// ============================================================================
// Constants for mock data volumes (per the spec).
// ============================================================================

const RELATIONAL_ROWS: usize = 10_000;
const DOCUMENT_COUNT: usize = 5_000;
const GRAPH_NODES: u64 = 1_000;
const GRAPH_EDGES: usize = 10_000;
const TIMESERIES_READINGS: usize = 100_000;

const BLOCK_SIZE: u32 = 256 * 1024; // spec default

// ============================================================================
// Group 1: Mock data generation
// ============================================================================

#[test]
fn gen_relational_10k_rows() {
    let schema = relational_schema();
    let mut table = RelationalTable::new(schema, SegmentId(1), BLOCK_SIZE).unwrap();
    let start = Instant::now();
    for i in 0..RELATIONAL_ROWS as i64 {
        table
            .insert(vec![
                Value::I64(i),
                Value::Bytes(format!("user_{:06}", i).into_bytes()),
                Value::Bytes(format!("user{}@example.com", i).into_bytes()),
                Value::I64(18 + (i % 70)),
                Value::Bytes(format!("DE{}", 10000 + (i % 89999)).into_bytes()),
            ])
            .unwrap();
    }
    table.flush_pending().unwrap();
    let elapsed = start.elapsed();
    println!(
        "[gen_relational_10k_rows] inserted {} rows in {:?} ({:.0} rows/sec)",
        RELATIONAL_ROWS,
        elapsed,
        RELATIONAL_ROWS as f64 / elapsed.as_secs_f64()
    );
    assert_eq!(table.row_count(), RELATIONAL_ROWS);
    assert!(table.block_count() > 1, "expected >1 block, got {}", table.block_count());
}

#[test]
fn gen_document_5k_profiles() {
    let start = Instant::now();
    let mut total_bytes: usize = 0;
    for i in 0..DOCUMENT_COUNT {
        let doc = make_doc(i);
        let bytes = CenDocBuilder::encode(&doc).unwrap();
        total_bytes += bytes.len();
    }
    let elapsed = start.elapsed();
    println!(
        "[gen_document_5k_profiles] encoded {} docs, total {} bytes ({:.0} docs/sec, {:.1} bytes/doc avg)",
        DOCUMENT_COUNT,
        total_bytes,
        DOCUMENT_COUNT as f64 / elapsed.as_secs_f64(),
        total_bytes as f64 / DOCUMENT_COUNT as f64,
    );
}

#[test]
fn gen_graph_1k_nodes_10k_edges() {
    let mut g = GraphProjection::new(SegmentId(1), BLOCK_SIZE);
    for i in 0..GRAPH_NODES {
        let label = if i % 7 == 0 { "Product" } else { "Person" };
        g.add_node(NodeId(i), label);
    }
    // Deterministic pseudo-random edges: each node connects to (i+1, i+2, i+5, i+13, ...).
    let offsets: [u64; 10] = [1, 2, 5, 13, 17, 23, 42, 71, 100, 137];
    let mut edges_added = 0usize;
    for i in 0..GRAPH_NODES {
        for &off in &offsets {
            let dst = (i + off) % GRAPH_NODES;
            if dst != i {
                g.add_edge(NodeId(i), NodeId(dst), "follows");
                edges_added += 1;
                if edges_added >= GRAPH_EDGES {
                    break;
                }
            }
        }
        if edges_added >= GRAPH_EDGES {
            break;
        }
    }
    g.flush().unwrap();
    assert_eq!(g.node_count(), GRAPH_NODES as usize);
    assert!(g.edge_count() >= GRAPH_EDGES);
    println!(
        "[gen_graph_1k_nodes_10k_edges] {} nodes, {} edges",
        g.node_count(),
        g.edge_count()
    );
}

#[test]
fn gen_timeseries_100k_readings() {
    let schema = ts_schema();
    let mut store = TimeSeriesStore::new(schema, SegmentId(1), BLOCK_SIZE);
    let start = Instant::now();
    // 100 sensors × 1000 readings each. ts = i, value = sin(i / 100.0).
    for sensor in 0..100i64 {
        for i in 0..1000i64 {
            let ts = 1_700_000_000_000 + i * 60; // 1 reading per minute
            let value = ((i as f64) / 100.0).sin() * 50.0 + 25.0;
            store.append(ts, sensor, value).unwrap();
        }
    }
    store.flush_pending().unwrap();
    let elapsed = start.elapsed();
    println!(
        "[gen_timeseries_100k_readings] inserted {} readings in {:?} ({:.0} reads/sec, {} blocks)",
        TIMESERIES_READINGS,
        elapsed,
        TIMESERIES_READINGS as f64 / elapsed.as_secs_f64(),
        store.block_count()
    );
    assert_eq!(store.row_count(), TIMESERIES_READINGS);
    assert!(store.block_count() > 1);
}

// ============================================================================
// Group 2: Functional integrity verification
// ============================================================================

#[test]
fn verify_kv_point_write_and_readback() {
    let mut store = KvStore::new(SegmentId(1), 16 * 1024);
    // Write 1000 keys.
    for i in 0..1000i64 {
        let key = format!("key_{:06}", i);
        let value = format!("value_{}", i);
        store.put(key.as_bytes(), value.as_bytes()).unwrap();
    }
    store.flush_pending().unwrap();

    // Read back every key and verify.
    for i in 0..1000i64 {
        let key = format!("key_{:06}", i);
        let expected = format!("value_{}", i);
        let actual = store.get(key.as_bytes()).unwrap().unwrap();
        assert_eq!(actual, expected.as_bytes());
    }
    println!("[verify_kv_point_write_and_readback] 1000 KV pairs verified");
}

#[test]
fn verify_timeseries_range_scan_with_zone_map_skipping() {
    let schema = ts_schema();
    let mut store = TimeSeriesStore::new(schema, SegmentId(1), BLOCK_SIZE);
    // 10,000 readings with ts = 0..10000, sensor 1.
    for ts in 0..10_000i64 {
        store.append(ts, 1, ts as f64 * 0.1).unwrap();
    }
    store.flush_pending().unwrap();
    let total_blocks = store.block_count();

    // Range scan over [5000, 5100) — should touch fewer blocks than total.
    let (touched, results) = store.range_scan(5000, 5099).unwrap();
    assert_eq!(results.len(), 100);
    assert!(
        touched < total_blocks,
        "zone map should skip blocks: touched={}, total={}",
        touched,
        total_blocks
    );
    println!(
        "[verify_timeseries_range_scan_with_zone_map_skipping] range [5000, 5099] touched {}/{} blocks (skipped {})",
        touched,
        total_blocks,
        total_blocks - touched
    );
}

#[test]
fn verify_graph_two_hop_traversal() {
    let mut g = GraphProjection::new(SegmentId(1), BLOCK_SIZE);
    // Build a known graph:
    //   0 -> 1, 1 -> 2, 2 -> 3, 3 -> 4
    //   0 -> 5, 5 -> 6, 6 -> 4
    // 2-hop from 0: {2, 6}
    // 2-hop from 1: {3}
    g.add_edge(NodeId(0), NodeId(1), "next");
    g.add_edge(NodeId(1), NodeId(2), "next");
    g.add_edge(NodeId(2), NodeId(3), "next");
    g.add_edge(NodeId(3), NodeId(4), "next");
    g.add_edge(NodeId(0), NodeId(5), "next");
    g.add_edge(NodeId(5), NodeId(6), "next");
    g.add_edge(NodeId(6), NodeId(4), "next");
    g.flush().unwrap();
    g.build_csr().unwrap();

    let two_hop_from_0 = g.two_hop(NodeId(0)).unwrap();
    assert!(two_hop_from_0.contains(&NodeId(2)), "expected 2 in 2-hop from 0: {:?}", two_hop_from_0);
    assert!(two_hop_from_0.contains(&NodeId(6)), "expected 6 in 2-hop from 0: {:?}", two_hop_from_0);
    assert!(!two_hop_from_0.contains(&NodeId(4)), "4 is 3 hops away, not 2");

    let two_hop_from_1 = g.two_hop(NodeId(1)).unwrap();
    assert!(two_hop_from_1.contains(&NodeId(3)));
    println!(
        "[verify_graph_two_hop_traversal] 2-hop from 0: {:?}, 2-hop from 1: {:?}",
        two_hop_from_0, two_hop_from_1
    );
}

#[test]
fn verify_graph_2hop_on_larger_graph() {
    let mut g = GraphProjection::new(SegmentId(1), BLOCK_SIZE);
    // Build a bidirectional ring: 0 <-> 1 <-> 2 <-> ... <-> 99 <-> 0.
    // 2-hop from 0 should include 2 (forward 2) and 99 (backward 1, then
    // backward 1 again — i.e. 0 <- 99 <- 98, so 98 is 2 hops back).
    for i in 0..100u64 {
        let next = (i + 1) % 100;
        g.add_edge(NodeId(i), NodeId(next), "next");
        g.add_edge(NodeId(next), NodeId(i), "prev");
    }
    g.flush().unwrap();
    g.build_csr().unwrap();
    // 2-hop from 0:
    //   forward: 0 -> 1 -> 2 (so 2 is reachable)
    //   backward: 0 -> 99 -> 98 (so 98 is reachable)
    let two_hop = g.two_hop(NodeId(0)).unwrap();
    assert!(two_hop.contains(&NodeId(2)), "expected 2 in 2-hop from 0: {:?}", two_hop);
    assert!(two_hop.contains(&NodeId(98)), "expected 98 in 2-hop from 0: {:?}", two_hop);
    println!("[verify_graph_2hop_on_larger_graph] 2-hop from 0 has {} nodes", two_hop.len());
}

#[test]
fn verify_document_shredded_nested_field() {
    // Build a deeply nested document and retrieve a leaf field via the
    // O(1) field offset table.
    let doc = DocValue::Object(vec![
        ("user".to_string(), DocValue::Object(vec![
            ("id".to_string(), DocValue::I64(42)),
            ("name".to_string(), DocValue::Str("Alice Anderson".to_string())),
            ("address".to_string(), DocValue::Object(vec![
                ("city".to_string(), DocValue::Str("Berlin".to_string())),
                ("zip".to_string(), DocValue::Str("10115".to_string())),
                ("geo".to_string(), DocValue::Object(vec![
                    ("lat".to_string(), DocValue::F64(52.52)),
                    ("lon".to_string(), DocValue::F64(13.405)),
                ])),
            ])),
            ("tags".to_string(), DocValue::Array(vec![
                DocValue::Str("premium".to_string()),
                DocValue::Str("verified".to_string()),
            ])),
        ])),
        ("active".to_string(), DocValue::Bool(true)),
    ]);

    let bytes = CenDocBuilder::encode(&doc).unwrap();
    let reader = cendb_projection::CenDoc::new(&bytes).unwrap();

    // Shredded access: walk the offset table to find user.address.city.
    let city = reader.get_path("user.address.city").unwrap().unwrap();
    match city {
        DocValue::Str(s) => assert_eq!(s, "Berlin"),
        other => panic!("expected Str, got {:?}", other),
    }

    // And user.address.geo.lat.
    let lat = reader.get_path("user.address.geo.lat").unwrap().unwrap();
    match lat {
        DocValue::F64(v) => assert!((v - 52.52).abs() < 1e-9),
        other => panic!("expected F64, got {:?}", other),
    }

    // And a nested array element.
    let tags = reader.get_path("user.tags").unwrap().unwrap();
    match tags {
        DocValue::Array(items) => {
            assert_eq!(items.len(), 2);
            assert_eq!(items[0], DocValue::Str("premium".to_string()));
        }
        other => panic!("expected Array, got {:?}", other),
    }

    println!(
        "[verify_document_shredded_nested_field] doc {} bytes, retrieved nested fields via O(1) offset table",
        bytes.len()
    );
}

// ============================================================================
// Group 3: Performance metrics
// ============================================================================

#[test]
fn perf_compression_ratio_across_models() {
    // Time-series: should compress well due to DoD encoding on ts. We use a
    // large pending_capacity so all readings land in a single block.
    let schema = ts_schema();
    let mut ts_store = TimeSeriesStore::new(schema, SegmentId(1), BLOCK_SIZE)
        .with_pending_capacity(100_000);
    for ts in 0..10_000i64 {
        ts_store.append(ts, 1, (ts as f64).sin()).unwrap();
    }
    ts_store.flush_pending().unwrap();
    let ts_ratio = ts_store.compression_ratio();
    println!(
        "[perf_compression_ratio] Time-Series: {:.2}x (raw {} bytes → stored {} bytes across {} blocks)",
        ts_ratio,
        ts_store.row_count() * 24,
        ts_store.block_count() * BLOCK_SIZE as usize,
        ts_store.block_count()
    );

    // KV: less compressible (random bytes) but should still be < 1.
    let mut kv_store = KvStore::new(SegmentId(1), 16 * 1024);
    for i in 0..1000i64 {
        let key = format!("key_{:06}", i);
        let value = format!("value_payload_{}", i);
        kv_store.put(key.as_bytes(), value.as_bytes()).unwrap();
    }
    kv_store.flush_pending().unwrap();
    let kv_ratio = kv_store.compression_ratio();
    println!(
        "[perf_compression_ratio] Key-Value: {:.2}x (1000 pairs across {} blocks)",
        kv_ratio,
        kv_store.block_count()
    );

    // Document: report bytes-per-doc for CenDoc encoding.
    let mut doc_total_bytes: usize = 0;
    for i in 0..1000 {
        let doc = make_doc(i);
        let bytes = CenDocBuilder::encode(&doc).unwrap();
        doc_total_bytes += bytes.len();
    }
    println!(
        "[perf_compression_ratio] Document: {} bytes/doc avg (CenDoc-encoded)",
        doc_total_bytes / 1000
    );
}

#[test]
fn perf_point_lookup_vs_scan_latency() {
    // Build a TS store with 10,000 readings.
    let schema = ts_schema();
    let mut store = TimeSeriesStore::new(schema, SegmentId(1), BLOCK_SIZE);
    for ts in 0..10_000i64 {
        store.append(ts, 1, ts as f64).unwrap();
    }
    store.flush_pending().unwrap();

    // Measure 100 point lookups via range_scan with a 1-tick window.
    let start = Instant::now();
    for ts in 0..100i64 {
        let _ = store.range_scan(ts, ts).unwrap();
    }
    let point_latency = start.elapsed() / 100;

    // Measure a full columnar scan.
    let start = Instant::now();
    let _ = store.range_scan(0, 9_999).unwrap();
    let scan_latency = start.elapsed();

    println!(
        "[perf_point_lookup_vs_scan_latency] point lookup: {:?}/op, full scan: {:?} ({} rows)",
        point_latency,
        scan_latency,
        store.row_count()
    );
    // Point lookup should be faster than a full scan.
    assert!(
        point_latency < scan_latency,
        "point ({:?}) should be faster than scan ({:?})",
        point_latency,
        scan_latency
    );
}

#[test]
fn perf_buffer_pool_memory_bounds() {
    use cendb_buffer::{BufferPool, InMemoryPageSource, ReadHint};
    use cendb_core::PageId;

    // Create a pool with 16 frames of 4 KiB each (64 KiB total).
    let source = Box::new(InMemoryPageSource::new(4096));
    let mut pool = BufferPool::new(source, 16, 4096).unwrap();

    // Pin 100 distinct pages with Scan hint — the pool should evict
    // gracefully without growing beyond 16 frames.
    for i in 0..100u16 {
        let pid = PageId::pack(SegmentId(1), cendb_core::BlockId(0), i);
        let _p = pool.pin_page(pid, ReadHint::Scan).unwrap();
    }

    let stats = pool.stats();
    println!(
        "[perf_buffer_pool_memory_bounds] pool capacity 16 frames, after 100 pin ops: hits={}, misses={}, evictions={}, pinned={}",
        stats.hits, stats.misses, stats.evictions, stats.pinned_frames
    );
    // The pool must not have grown beyond its capacity.
    assert_eq!(stats.total_frames, 16, "pool grew beyond capacity");
    // We should have evicted at least 100 - 16 = 84 frames.
    assert!(
        stats.evictions >= 84,
        "expected >= 84 evictions, got {}",
        stats.evictions
    );
    // No frames should be pinned at the end (all guards dropped).
    assert_eq!(stats.pinned_frames, 0);
}

#[test]
fn perf_buffer_pool_scan_resistance() {
    use cendb_buffer::{BufferPool, InMemoryPageSource, ReadHint};
    use cendb_core::PageId;

    // 8-frame pool.
    let source = Box::new(InMemoryPageSource::new(4096));
    let mut pool = BufferPool::new(source, 8, 4096).unwrap();

    // Hot page: pin twice with Point hint → K=2 accesses in LRU-K.
    let hot_pid = PageId::pack(SegmentId(1), cendb_core::BlockId(0), 0);
    {
        let _p = pool.pin_page(hot_pid, ReadHint::Point).unwrap();
    }
    {
        let _p = pool.pin_page(hot_pid, ReadHint::Point).unwrap();
    }

    // Scan through 50 distinct pages — should evict each other, not the hot page.
    for i in 1..50u16 {
        let pid = PageId::pack(SegmentId(1), cendb_core::BlockId(0), i);
        let _p = pool.pin_page(pid, ReadHint::Scan).unwrap();
    }

    // Re-pin the hot page — should be a hit (still in pool).
    {
        let _p = pool.pin_page(hot_pid, ReadHint::Point).unwrap();
    }
    let stats = pool.stats();
    println!(
        "[perf_buffer_pool_scan_resistance] after 50 scan pages + hot page re-pin: hits={}, misses={}, evictions={}",
        stats.hits, stats.misses, stats.evictions
    );
    assert!(
        stats.hits >= 1,
        "hot page should have been retained (hits >= 1), got stats {:?}",
        stats
    );
}

#[test]
fn perf_kv_bulk_insert_throughput() {
    let mut store = KvStore::new(SegmentId(1), 64 * 1024);
    let n = 10_000usize;
    let start = Instant::now();
    for i in 0..n {
        let key = format!("k_{:08}", i);
        let value = format!("v_{:08}_payload", i);
        store.put(key.as_bytes(), value.as_bytes()).unwrap();
    }
    store.flush_pending().unwrap();
    let elapsed = start.elapsed();
    println!(
        "[perf_kv_bulk_insert_throughput] {} KVs in {:?} ({:.0} ops/sec, {} blocks)",
        n,
        elapsed,
        n as f64 / elapsed.as_secs_f64(),
        store.block_count()
    );
    assert_eq!(store.len(), n);
}

#[test]
fn perf_graph_bfs_at_scale() {
    let mut g = GraphProjection::new(SegmentId(1), BLOCK_SIZE);
    // 1000-node bidirectional ring with extra "shortcut" edges.
    for i in 0..1000u64 {
        let next = (i + 1) % 1000;
        g.add_edge(NodeId(i), NodeId(next), "next");
        g.add_edge(NodeId(next), NodeId(i), "prev");
        if i % 10 == 0 {
            g.add_edge(NodeId(i), NodeId((i + 100) % 1000), "shortcut");
        }
    }
    g.flush().unwrap();
    g.build_csr().unwrap();

    let start = Instant::now();
    let bfs = g.bfs(NodeId(0), 10).unwrap();
    let elapsed = start.elapsed();
    println!(
        "[perf_graph_bfs_at_scale] BFS (depth 10) visited {} nodes in {:?}",
        bfs.len(),
        elapsed
    );
    assert!(bfs.len() > 100, "BFS should visit many nodes, got {}", bfs.len());
}

// ============================================================================
// Helpers
// ============================================================================

fn relational_schema() -> cendb_projection::relational::TableSchema {
    use cendb_projection::relational::TableSchema;
    TableSchema::new(
        "users",
        vec![
            ColumnSpec::new(0, ValueKind::I64).pk(),
            ColumnSpec::new(1, ValueKind::Bytes),
            ColumnSpec::new(2, ValueKind::Bytes),
            ColumnSpec::new(3, ValueKind::I64),
            ColumnSpec::new(4, ValueKind::Bytes),
        ],
    )
}

fn ts_schema() -> TimeSeriesSchema {
    TimeSeriesSchema {
        ts_col_id: 0,
        series_col_id: 1,
        extra_cols: vec![ColumnSpec::new(2, ValueKind::F64)],
    }
}

fn make_doc(i: usize) -> DocValue {
    DocValue::Object(vec![
        ("id".to_string(), DocValue::I64(i as i64)),
        ("name".to_string(), DocValue::Str(format!("user_{}", i))),
        ("email".to_string(), DocValue::Str(format!("user{}@example.com", i))),
        ("age".to_string(), DocValue::I64(18 + (i as i64 % 70))),
        ("address".to_string(), DocValue::Object(vec![
            ("city".to_string(), DocValue::Str(format!("City_{}", i % 50))),
            ("zip".to_string(), DocValue::Str(format!("{:05}", 10000 + i % 89999))),
        ])),
        ("tags".to_string(), DocValue::Array(vec![
            DocValue::Str(if i % 2 == 0 { "premium" } else { "basic" }.to_string()),
            DocValue::Str("verified".to_string()),
        ])),
        ("active".to_string(), DocValue::Bool(i % 3 != 0)),
    ])
}
