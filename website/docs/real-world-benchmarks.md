---
sidebar_position: 14
title: Real-World Benchmarks
---

# Real-World Benchmark Suite

The benchmark suite at `crates/cendb/tests/real_world_bench.rs` addresses 5 concerns that are often missing from database benchmarks.

## Concern 1: Disk IO, WAL fsync, compaction

### WAL fsync cost

| Config | p50 | p95 | p99 |
|---|---|---|---|
| sync_on_commit=true | 8.49µs | 18.46µs | 27.16µs |
| sync_on_commit=false | 8.10µs | 11.27µs | 21.49µs |

fsync overhead: ~1.1× slower with sync_on_commit=true.

### Disk IO

| Operation | Time | Throughput |
|---|---|---|
| 10K puts (in-memory) | 36ms | — |
| persist_to_segment (disk write) | 1.62ms | 6.2M pairs/sec |
| load_from_segment (disk read) | 12.93ms | 774K pairs/sec |
| Segment file size | 1.2 MB | — |

### Compaction

| Metric | Value |
|---|---|
| Blocks before/after | 9 / 4 |
| Rows before/after | 9000 / 4000 |
| Bytes reclaimed | 327,680 |
| Compaction time | 12.52ms |

## Concern 2: Thread scaling and contention

### 1 vs 16 thread scaling

| Threads | Throughput | Committed | Aborted | Abort % |
|---|---|---|---|---|
| 1 | 302,602 ops/sec | 5000 | 0 | 0.0% |
| 2 | 228,160 ops/sec | 9959 | 41 | 0.4% |
| 4 | 177,717 ops/sec | 19763 | 237 | 1.2% |
| 8 | 154,231 ops/sec | 38504 | 1496 | 3.7% |
| 16 | 150,618 ops/sec | 74116 | 5884 | 7.4% |

### Extreme contention (100 threads, 1 hot key)

| Metric | Value |
|---|---|
| Total ops | 20,000 |
| Throughput | 135,526 ops/sec |
| Committed | 8,464 (42.3%) |
| Aborted | 11,536 (57.7%) |

Under extreme contention, OCC correctly aborts conflicting transactions — no lost updates, but high abort rate is expected.

## Concern 3: Mixed workload, deletes, tombstones

### Mixed read/write (8 threads, 80% reads / 20% writes)

| Operation | p50 | p95 | p99 |
|---|---|---|---|
| Read (mixed) | 1.50µs | 59.50µs | 96.76µs |
| Write (mixed) | 2.42µs | 61.30µs | 97.49µs |

Total: 80,000 ops in 220ms = 364,152 ops/sec.

### Deletes and tombstones

| Operation | p50 | p95 | p99 |
|---|---|---|---|
| Get (deleted key, tombstone) | 599ns | 650ns | 675ns |
| Get (alive key) | 634ns | 798ns | 1.00µs |
| Get (after compaction) | 434ns | 523ns | 620ns |

Tombstone lookups are O(1) — no linear scan. Compaction improves get latency by ~30%.

## Concern 4: KV get under load (HashMap fix)

| Implementation | p99 |
|---|---|
| Old (Vec linear scan) | 17.1µs |
| New (HashMap) | 1.89µs |

**9× improvement.** The HashMap fix eliminated the structural weakness where pending writes were scanned linearly.

## Concern 5: Durability story

### Crash recovery

| Phase | Result |
|---|---|
| Phase 1: wrote 5000 committed txns | WAL = 450KB |
| Phase 2: simulated crash (truncate at 75%) | WAL = 337KB |
| Phase 3: ARIES recovery | 47.8ms |
| Surviving records | 7,500 |
| Committed txns recovered | 3,750 |
| Loser txns (rolled back) | 0 |
| Replay throughput | 157,000 records/sec |

### fsync cost

| Operation | p50 | p99 |
|---|---|---|
| Raw fsync (1 byte) | 352ns | 571ns |
| WAL commit (sync=true) | 4.21µs | 13.50µs |
| WAL commit (sync=false) | 3.87µs | 5.50µs |

sync_on_commit=true adds ~584ns per commit — the fsync cost. For durability, use sync_on_commit=true. For throughput, use sync_on_commit=false with periodic checkpoints.
