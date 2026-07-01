---
sidebar_position: 13
title: Benchmarks
---

# Benchmarks

## Methodology

All measurements use p50/p95/p99 percentiles (not just mean). The real-world benchmark suite at `crates/cendb/tests/real_world_bench.rs` covers disk IO, concurrency scaling, mixed workloads, and durability.

## Throughput

| Workload | Volume | Throughput |
|---|---|---|
| Relational insert | 10,000 rows | ~326,000 rows/sec |
| Time-series ingest | 100,000 readings | ~1,095,000 reads/sec |
| KV bulk insert | 10,000 pairs | ~531,000 ops/sec |
| Vectorized filter | 10M i64 rows | 96 M rows/sec |
| Vectorized sum | 10M i64 rows | 182 M rows/sec |
| Disk persist | 10K KV pairs | 6.2M pairs/sec |
| Disk load | 10K KV pairs | 729K pairs/sec |

## Latency (p50/p95/p99)

| Operation | p50 | p95 | p99 |
|---|---|---|---|
| KV put (random key) | 135ns | 279ns | 389ns |
| KV get (random key) | 972ns | 1.29µs | 1.89µs |
| KV get (deleted/tombstone) | 599ns | 650ns | 675ns |
| ART insert | 603ns | 809ns | 4.6µs |
| ART lookup | 735ns | 1.2µs | 1.7µs |
| WAL commit (sync=true) | 4.21µs | 4.69µs | 13.5µs |
| WAL commit (sync=false) | 3.87µs | 4.14µs | 5.50µs |
| Filter (10M rows) | 104ms | 106ms | 107ms |
| Sum (10M rows) | 55ms | 56ms | 57ms |

### KV get fix

The KV get p99 was **17.1µs** (linear scan of pending buffer) and is now **1.89µs** (HashMap lookup) — a **9× improvement**. The old implementation scanned a Vec linearly; the new one uses a HashMap for O(1) lookup on both pending and sealed data.

## Thread scaling

| Threads | Throughput | Abort rate |
|---|---|---|
| 1 | 302,602 ops/sec | 0.0% |
| 2 | 228,160 ops/sec | 0.4% |
| 4 | 177,717 ops/sec | 1.2% |
| 8 | 154,231 ops/sec | 3.7% |
| 16 | 150,618 ops/sec | 7.4% |

Under extreme contention (100 threads on 1 key): 42.3% commit rate, 57.7% abort rate — OCC correctly prevents lost updates.

## Durability

| Metric | Value |
|---|---|
| Crash recovery (5000 txns) | 48ms |
| Replay throughput | 157,000 records/sec |
| fsync overhead per commit | 584ns |
| Compaction (5000→4000 rows) | 13ms, 328KB reclaimed |

## Realistic compression ratios

| Dataset | Best encoding | Ratio | Auto-selected? |
|---|---|---|---|
| CPU TS (seasonal+noisy) | BitPacked | 9.14× | ✓ |
| Financial ticks (random walk) | DeltaOfDelta | 8.00× | ✗ |
| Zipf word frequencies | DeltaOfDelta | 7.96× | ✓ |
| Log line lengths (bimodal) | Dictionary | 6.89× | ✗ |
| Relational: age (clustered Gaussian) | FoR | 10.67× | ✗ |
| Relational: signup_ts (monotonic) | DeltaOfDelta | 8.00× | ✓ |

Real-world compression is 4-11×, not the 60-6,150× best-case numbers.
