# Benchmarks and Tuning

This document captures performance measurements from CenDB's
verification suite and provides guidance on tuning for your workload.

## Methodology

All measurements are taken on the implementation running the
verification suite (`cargo test --workspace --release -- --nocapture`).
Phase 3 latency numbers come from `crates/cendb/tests/phase3_latency.rs`
which reports p50/p95/p99 percentiles (not just mean).

Hardware: development environment; results are representative of
what you should expect on commodity server hardware (x86-64, SSD).
Absolute numbers will vary with hardware; relative comparisons
(between encodings, between operations) are stable.

## Throughput

| Workload | Volume | Throughput |
|---|---|---|
| Relational insert | 10,000 rows | ~326,000 rows/sec |
| Time-series ingest | 100,000 readings | ~1,095,000 reads/sec |
| KV bulk insert | 10,000 pairs | ~531,000 ops/sec |
| Document encode (CenDoc) | 5,000 docs | ~61,600 docs/sec |
| Graph BFS (1000 nodes, depth 10) | 120 nodes visited | ~7 µs |
| Vectorized filter (10M i64) | 10,000,000 rows | ~96 M rows/sec |
| Vectorized sum (10M i64) | 10,000,000 rows | ~182 M rows/sec |

## Latency (Phase 3 — p50/p95/p99)

These numbers come from `phase3_latency.rs` and represent real
percentile measurements, not just means. The distinction matters:
p99 latency is what users actually feel, and it can be 10-100×
the mean if the system has outliers.

| Operation | p50 | p95 | p99 | Notes |
|---|---|---|---|---|
| KV put (random key) | 135ns | 279ns | 389ns | In-memory hash index; sub-µs |
| KV put (sequential key) | 107ns | 221ns | 245ns | Sequential → better cache behavior |
| KV get (random key) | 7.4µs | 8.5µs | 17.1µs | Linear scan of pending buffer; see Known Limitations |
| ART insert | 603ns | 809ns | 4.6µs | p99 spike from tree rebalancing |
| ART lookup (random key) | 735ns | 1.2µs | 1.7µs | 100K keys in tree |
| Vectorized filter (10M rows) | 104ms | 106ms | 107ms | Tight distribution; SIMD-friendly |
| Vectorized sum (10M rows) | 55ms | 56ms | 57ms | Tighter than filter |

### Key observations

1. **KV put is sub-µs at p99.** The hash index + PAX block append is
   very fast. The sequential-key case is even faster due to better
   cache locality.

2. **KV get is 50× slower than put at p99.** This is a known
   limitation: `KvStore::get` linearly scans the pending write buffer
   before checking the index. For workloads with many unflushed
   writes, this dominates. A B-tree or sorted index on pending
   would fix this.

3. **ART p99 is 6× the p50.** Tree rebalancing (path compression
   adjustments) causes occasional spikes. The canonical Node4/16/48/
   256 layout would reduce this.

4. **Vectorized filter/sum have tight distributions.** p99 is within
   3% of p50. This is the benefit of vectorized execution: no
   per-row branching, no cache misses, predictable throughput.

### Throughput from latency

- KV put: 100K ops at 2.4µs mean = ~417K ops/sec
- KV get: 100K ops at 7.8µs mean = ~128K ops/sec
- ART insert: 100K ops at 729ns mean = ~1.37M ops/sec
- ART lookup: 100K ops at 810ns mean = ~1.23M ops/sec
- Filter: 96 M rows/sec (10M rows in ~104ms)
- Sum: 182 M rows/sec (10M rows in ~55ms)

## Compression

### Best-case (pathological) ratios — for reference only

These numbers are from synthetic inputs designed to make each encoding
look its best. **Do not use them to estimate real-world compression.**
They are included only as an upper bound.

| Encoding | Workload | Raw bytes | Stored bytes | Ratio |
|---|---|---|---|---|
| `Raw` | KV (1000 pairs) | ~20 KB | ~16 KB | 0.43× |
| `DeltaOfDelta` | TS timestamps (10K readings, 1 block) | 240 KB | 262 KB | 0.92× |
| `Gorilla` | Constant floats (100 values) | 800 B | ~13 B | ~63× |
| `RunLength` | 10 runs × 100 identical values | 8,000 B | 120 B | ~6,150× |
| `CenDoc` | Nested document | — | ~315 B/doc | — |

### Realistic ratios (Phase 2 benchmarks)

Re-run with realistic, non-pathological datasets. **These are the
numbers you should expect in production.** See
`crates/cendb/tests/phase2_compression.rs` for the generators.

| Dataset | Best encoding | Ratio | Bits/value | Auto-selected? |
|---|---|---|---|---|
| CPU utilization TS (8,640 pts, seasonal+noisy) | BitPacked | 9.14× | 7.0b | ✓ |
| Financial ticks (3.6M pts, random walk) | DeltaOfDelta | 8.00× | 8.0b | ✗ (auto: BitPacked @ 4.57×) |
| Zipf word frequencies (10K words) | DeltaOfDelta | 7.96× | 8.0b | ✓ (Phase 3 improved) |
| Nginx log line lengths (100K, bimodal) | Dictionary | 6.89× | 9.3b | ✗ (auto: BitPacked @ 6.40×) |
| Relational: user_id (seq PK) | DeltaOfDelta | 8.00× | 8.0b | ✓ |
| Relational: country (low-card enum) | BitPacked | 8.00× | 8.0b | ✓ |
| Relational: age (clustered Gaussian) | FoR | 10.67× | 6.0b | ✗ (auto: Dictionary @ 10.59×) |
| Relational: signup_ts (monotonic) | DeltaOfDelta | 8.00× | 8.0b | ✓ |
| Relational: duration (log-normal) | DeltaOfDelta | 7.39× | 8.7b | ✗ (auto: BitPacked @ 5.82×) |
| Constant 42 (best-case baseline) | RunLength | 6,153× | 0.0b | n/a |

**f64 datasets (Gorilla codec):**

| Dataset | Ratio | Bits/value |
|---|---|---|
| CPU TS (f64, seasonal+noisy) | 1.22× | 52.3b |
| Financial tick prices (f64, random walk) | 2.01× | 31.9b |
| Constant 42.0 (f64 best-case) | 63.3× | 1.0b |

### Phase 3 compression improvements

Phase 3 added **Dictionary encoding** as a real codec (was a stub)
and improved the auto-selector:

1. **Dictionary encoding** — builds a sorted dictionary of distinct
   values, then bit-packs per-row code indices. Wins on
   low-cardinality columns (≤1% cardinality, ≤4096 distinct values).
   Phase 3 result: 10.59× on the "age" column vs 9.14× for BitPacked.

2. **Relaxed DeltaOfDelta selection** — now accepts "mostly
   monotonic" data (≥80% non-decreasing pairs + net positive trend +
   range > 16 bits), not just strictly monotonic. Phase 3 result:
   DeltaOfDelta is now auto-selected for Zipf word frequencies
   (7.96× vs the old 3.20× with FoR — a **2.5× improvement**).

3. **Dictionary decode fast path** — byte-aligned codes (8/16/32/64
   bits) skip the bit-by-bit BitReader and use direct byte copies.
   Decode is 8.4× faster (1.29ms vs 10.83ms for 100K values).

### Compression codec encode/decode latency

Measured on 100K i64 values with 200 distinct values (Dictionary-
friendly). See `phase3_latency.rs`.

| Encoding | Ratio | Encode p50 | Decode p50 | Auto? |
|---|---|---|---|---|
| Raw | 1.00× | 2.97ms | 1.87ms | |
| BitPacked | 8.00× | 1.45ms | 1.18ms | |
| FrameOfReference | 8.00× | 1.51ms | 1.19ms | |
| DeltaOfDelta | 7.92× | 1.21ms | 3.71ms | |
| RunLength | 0.67× | 5.65ms | 3.23ms | |
| Dictionary | 7.87× | 40.4ms | 1.29ms | ✓ |

Dictionary encode is slow (40ms) because of the O(n log n) sort to
build the dictionary. Decode is fast (1.29ms) thanks to the byte-
aligned fast path. For read-heavy workloads this is the right
tradeoff.

### Key findings

1. **Real-world compression is 4–11×, not 60–6000×.** The best-case
   numbers are driven by pathological inputs (constant values,
   perfect runs). Real data has noise, seasonality, and heavy-tailed
   distributions that prevent encodings from reaching their
   theoretical floor.

2. **Gorilla underperforms on realistic f64 data.** 1.22× on noisy
   CPU time series, 2.01× on random-walk prices. The 63× ratio only
   appears on constant floats. This matches published Gorilla results
   (Facebook's original paper reports ~1.4× on real monitoring data,
   not the 60× best-case).

3. **RunLength is pathological on non-constant data** — it expands
   the data (0.67× ratio means 50% larger than raw). The auto-
   selector correctly avoids it; manual users should too.

4. **FrameOfReference wins on clustered integers** — 10.67× on ages
   (range 18–80, Gaussian around 40) vs. 9.14× for BitPacked.

### Notes

- The TS ratio of 0.92× in the best-case table is misleading: it
  includes the block header (64B) + column directory (3 × 64B) + bitmap
  overhead, amortised over a single 256KB block. With a full block of
  ~10K readings, the ratio approaches 0.05× thanks to DeltaOfDelta on
  the timestamp column.
- All realistic ratios verified by decode roundtrip (encoded then
  decoded, asserted equal to input).

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
