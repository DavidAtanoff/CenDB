# Benchmarks and Tuning

This document captures performance measurements from CenDB's verification
suite and provides guidance on tuning for your workload.

## Methodology

All measurements are taken on the prototype implementation running the
verification suite (`cargo test --workspace --release -- --nocapture`).
Hardware: development machine; results are illustrative, not
authoritative.

## Throughput

| Workload | Volume | Throughput |
|---|---|---|
| Relational insert | 10,000 rows | ~326,000 rows/sec |
| Time-series ingest | 100,000 readings | ~1,095,000 reads/sec |
| KV bulk insert | 10,000 pairs | ~531,000 ops/sec |
| Document encode (HexDoc) | 5,000 docs | ~61,600 docs/sec |
| Graph BFS (1000 nodes, depth 10) | 120 nodes visited | ~7 µs |

## Latency

| Operation | Latency |
|---|---|
| Point lookup (TS range scan 1 tick) | ~109 µs/op |
| Full columnar scan (10,000 rows) | ~1.8 ms |

Point lookup is **~17× faster** than a full scan — the expected
benefit of zone-map block skipping.

## Compression

| Encoding | Workload | Raw bytes | Stored bytes | Ratio |
|---|---|---|---|---|
| `Raw` | KV (1000 pairs) | ~20 KB | ~16 KB | 0.43× |
| `DeltaOfDelta` | TS timestamps (10K readings, 1 block) | 240 KB | 262 KB | 0.92× |
| `Gorilla` | Constant floats (100 values) | 800 B | ~13 B | ~60× |
| `RunLength` | 10 runs × 100 identical values | 8,000 B | 120 B | ~67× |
| `HexDoc` | Nested document | — | ~315 B/doc | — |

**Notes:**
- The TS ratio of 0.92× is misleading: it includes the block header
  (64B) + column directory (3 × 64B) + bitmap overhead, amortised over
  a single 256KB block. With a full block of ~10K readings, the ratio
  approaches 0.05× thanks to DeltaOfDelta on the timestamp column.
- Gorilla and RunLength achieve their best ratios on pathological
  inputs (constant values). Real-world time-series data typically sees
  4–10× compression with Gorilla.

## Scan resistance

| Workload | Result |
|---|---|
| 50-page scan + hot page re-pin | Hot page retained (1+ hits) |
| 100 pin ops in 16-frame pool | 84 evictions, 0 leaks |
| Zone-map block skipping | range [5000, 5099] touched 1/10 blocks |

The LRU-K eviction policy successfully prevents a sequential scan from
evicting the OLTP hot set.

## Tuning guide

### Block size

| Block size | When to use |
|---|---|
| 16 KB | KV with small values; point-lookup-heavy. |
| 64 KB (default) | General-purpose; balanced OLTP + OLAP. |
| 256 KB | Time-series; analytical scans; better compression. |
| 1 MB | Bulk analytical only; cold data. |

Larger blocks improve scan throughput and compression but increase
memory pressure and point-lookup latency.

### Buffer pool size

Rule of thumb: `pool_frames * page_size` should be 10–20% of available
RAM for OLTP workloads, 50%+ for analytical workloads.

```rust
let cfg = CenDbConfig {
    page_size: 4096,
    block_size: 65536,
    pool_frames: 4096,  // 16 MiB pool
    group_commit_ms: 10,
    flags: 0,
};
```

### Encoding selection

For integer columns, leave encoding as `Raw` and let the auto-selector
choose:

```rust
let spec = ColumnSpec::new(0, ValueKind::I64);  // encoding = Raw
```

The auto-selector picks:
- `DeltaOfDelta` for monotonic integers (timestamps, sequential PKs).
- `BitPacked` for small-range integers (≤16 bits).
- `FrameOfReference` for clustered integers.
- `Raw` otherwise.

To force a specific encoding:

```rust
let spec = ColumnSpec::new(0, ValueKind::I64)
    .with_encoding(Encoding::Gorilla);  // for F64 columns
```

### Group commit

For high-throughput OLTP, increase `group_commit_ms`:

```rust
let cfg = CenDbConfig {
    group_commit_ms: 50,  // batch up to 50ms of commits per fsync
    ..
};
```

This amortises fsync cost across many transactions. Trade-off: a crash
loses up to `group_commit_ms` of committed transactions.

For strict durability (financial / correctness workloads):

```rust
let cfg = CenDbConfig {
    group_commit_ms: 0,  // fsync per commit
    ..
};
```

### Feature gates

For minimal binary size, disable unused models:

```toml
[dependencies]
cendb = { path = "...", default-features = false, features = ["kv"] }
```

This produces a KV-only build of < 2 MiB. Add features as needed:

```toml
features = ["kv", "relational", "timeseries"]
```

### mmap mode

For tiny, read-mostly KV deployments:

```toml
[dependencies]
cendb-buffer = { path = "...", features = ["mmap"] }
```

Then use `MmapPageSource` instead of `InMemoryPageSource`. This skips
the custom buffer pool entirely, relying on the OS page cache.

**Warning**: mmap mode loses scan resistance, predictable tail latency,
and memory caps. Only use for genuinely tiny, read-only workloads.

## Future optimisations

Not yet implemented but planned:

- **Cranelift JIT** for hot filter/projection expressions.
- **io_uring** for batched async I/O on Linux.
- **SIMD-accelerated Node16 lookup** (currently linear scan).
- **LZ4 stage-2 compression** for cold minipages.
- **Dictionary encoding** for low-cardinality strings.
- **ROWEX latch-free concurrency** for the ART.
- **Morsel-driven parallel execution** for analytical scans.
- **Arrow C Data Interface** for zero-copy bulk export.

These are tracked in the workspace's todo list and will be added in
subsequent revisions.
