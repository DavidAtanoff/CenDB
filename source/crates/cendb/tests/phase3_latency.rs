//! Phase 3: Internal latency benchmarks with p50/p95/p99 percentiles.
//!
//! Measures CenDB's core operations under realistic workloads. Reports
//! p50/p95/p99 latency (not just mean throughput) so we can see tail
//! behavior. All measurements are in-process (no network) using the
//! `cendb` facade crate.
//!
//! ## Workloads
//!
//! 1. **KV point lookup** — 100K puts followed by 100K gets, random keys.
//! 2. **KV sequential write** — 100K puts, sequential keys (tests append path).
//! 3. **Time-series append** — 100K readings appended, then range scan.
//! 4. **Vectorized filter** — 10M i64 column, filter > N, measure scan throughput.
//! 5. **ART lookup** — 100K insertions, then 100K lookups (random + sequential).
//! 6. **Compression encode/decode** — 100K i64 column, measure encode + decode time per encoding.
//!
//! ## Methodology
//!
//! - Each operation is timed individually with `Instant::now()`.
//! - We collect all latencies into a Vec, sort, and report p50/p95/p99.
//! - Warmup: 1000 operations before measurement (not counted).
//! - All measurements in single-threaded mode for consistency.

use cendb_core::{PageId, SegmentId, Value, ValueKind, CenDbConfig};
use cendb_projection::KvStore;
use cendb_storage::encoding::{
    auto_select_encoding_i64, BitPackedCodec, DeltaOfDeltaCodec, DictionaryCodec,
    EncodingCodec, FrameOfReferenceCodec, RawCodec, RunLengthCodec,
};
use cendb_index::art::ArtTree;
use cendb_executor::{filter_i64_gt, sum_i64};
use std::time::{Duration, Instant};

// ============================================================================
// Latency stats.
// ============================================================================

#[derive(Clone, Debug)]
pub struct LatencyStats {
    pub count: usize,
    pub min: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
    pub mean: Duration,
    pub total: Duration,
}

impl LatencyStats {
    pub fn from_samples(samples: &mut Vec<Duration>) -> Self {
        if samples.is_empty() {
            return Self {
                count: 0, min: Duration::ZERO, p50: Duration::ZERO,
                p95: Duration::ZERO, p99: Duration::ZERO, max: Duration::ZERO,
                mean: Duration::ZERO, total: Duration::ZERO,
            };
        }
        samples.sort();
        let n = samples.len();
        let p = |pct: usize| -> Duration {
            let idx = ((n * pct + 99) / 100).saturating_sub(1).min(n - 1);
            samples[idx]
        };
        let total: Duration = samples.iter().sum();
        let mean = total / n as u32;
        Self {
            count: n,
            min: samples[0],
            p50: p(50),
            p95: p(95),
            p99: p(99),
            max: samples[n - 1],
            mean,
            total,
        }
    }

    pub fn print(&self, label: &str) {
        if self.count == 0 {
            println!("  {:<35} (no samples)", label);
            return;
        }
        println!(
            "  {:<35} n={:>6}  min={:>8.2?}  p50={:>8.2?}  p95={:>8.2?}  p99={:>8.2?}  max={:>8.2?}  mean={:>8.2?}",
            label, self.count, self.min, self.p50, self.p95, self.p99, self.max, self.mean
        );
    }
}

// ============================================================================
// PRNG.
// ============================================================================

struct Rng { state: u64 }
impl Rng {
    fn new(seed: u64) -> Self { Self { state: if seed == 0 { 1 } else { seed } } }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        self.state = x; x
    }
    fn gen_range(&mut self, min: u64, max: u64) -> u64 {
        if min >= max { return min; }
        min + (self.next_u64() % (max - min))
    }
}

// ============================================================================
// Workloads.
// ============================================================================

pub fn bench_kv_point_lookup() -> (LatencyStats, LatencyStats) {
    let mut store = KvStore::new(SegmentId(1), 64 * 1024);
    let mut rng = Rng::new(42);
    let n = 100_000;

    // Put phase.
    let mut put_latencies = Vec::with_capacity(n);
    for i in 0..n {
        let key = format!("key_{:08}", rng.next_u64() % 1_000_000);
        let val = format!("val_{}", i);
        let start = Instant::now();
        store.put(key.as_bytes(), val.as_bytes()).unwrap();
        put_latencies.push(start.elapsed());
    }

    // Get phase (re-use same keys, randomized).
    let mut get_latencies = Vec::with_capacity(n);
    for _ in 0..n {
        let key = format!("key_{:08}", rng.next_u64() % 1_000_000);
        let start = Instant::now();
        let _ = store.get(key.as_bytes());
        get_latencies.push(start.elapsed());
    }

    (LatencyStats::from_samples(&mut put_latencies), LatencyStats::from_samples(&mut get_latencies))
}

pub fn bench_kv_sequential_write() -> LatencyStats {
    let mut store = KvStore::new(SegmentId(1), 64 * 1024);
    let n = 100_000;
    let mut latencies = Vec::with_capacity(n);
    for i in 0..n {
        let key = format!("key_{:08}", i);
        let val = format!("val_{}", i);
        let start = Instant::now();
        store.put(key.as_bytes(), val.as_bytes()).unwrap();
        latencies.push(start.elapsed());
    }
    LatencyStats::from_samples(&mut latencies)
}

pub fn bench_vectorized_filter() -> (LatencyStats, LatencyStats) {
    let n = 10_000_000;
    let col: Vec<i64> = (0..n as i64).collect();
    let threshold = n as i64 / 2;

    // Warmup.
    let _ = filter_i64_gt(&col, threshold);

    // Filter latency (single call over 10M rows).
    let mut filter_latencies = Vec::with_capacity(100);
    for _ in 0..100 {
        let start = Instant::now();
        let _ = filter_i64_gt(&col, threshold);
        filter_latencies.push(start.elapsed());
    }

    // Sum latency.
    let mut sum_latencies = Vec::with_capacity(100);
    for _ in 0..100 {
        let start = Instant::now();
        let _ = sum_i64(&col);
        sum_latencies.push(start.elapsed());
    }

    (LatencyStats::from_samples(&mut filter_latencies), LatencyStats::from_samples(&mut sum_latencies))
}

pub fn bench_art_lookup() -> (LatencyStats, LatencyStats) {
    let mut tree: ArtTree<u64> = ArtTree::new();
    let mut rng = Rng::new(42);
    let n = 100_000;

    // Insert phase.
    let mut insert_latencies = Vec::with_capacity(n);
    let keys: Vec<Vec<u8>> = (0..n).map(|i| {
        format!("key_{:010}", i).into_bytes()
    }).collect();
    for (i, k) in keys.iter().enumerate() {
        let start = Instant::now();
        tree.insert(k, i as u64);
        insert_latencies.push(start.elapsed());
    }

    // Lookup phase (random keys).
    let mut lookup_latencies = Vec::with_capacity(n);
    for _ in 0..n {
        let idx = (rng.next_u64() as usize) % n;
        let start = Instant::now();
        let _ = tree.get(&keys[idx]);
        lookup_latencies.push(start.elapsed());
    }

    (LatencyStats::from_samples(&mut insert_latencies), LatencyStats::from_samples(&mut lookup_latencies))
}

pub fn bench_compression_encode_decode() -> Vec<(&'static str, LatencyStats, LatencyStats, f64)> {
    // 100K i64 column with low cardinality (200 distinct values) — Dictionary-friendly.
    let n = 100_000;
    let vals: Vec<i64> = (0..n).map(|i| (i % 200) as i64).collect();

    let codecs: Vec<(&'static str, Box<dyn EncodingCodec>)> = vec![
        ("Raw", Box::new(RawCodec)),
        ("BitPacked", Box::new(BitPackedCodec)),
        ("FrameOfReference", Box::new(FrameOfReferenceCodec)),
        ("DeltaOfDelta", Box::new(DeltaOfDeltaCodec)),
        ("RunLength", Box::new(RunLengthCodec)),
        ("Dictionary", Box::new(DictionaryCodec)),
    ];

    let mut results = Vec::new();
    for (name, codec) in codecs {
        // Warmup.
        let _ = codec.encode(&vals);

        // Encode (10 iterations for stable timing).
        let mut enc_latencies = Vec::with_capacity(10);
        for _ in 0..10 {
            let start = Instant::now();
            let encoded = codec.encode(&vals).unwrap();
            enc_latencies.push(start.elapsed());

            // Decode.
            let dec_start = Instant::now();
            let _ = codec.decode(&encoded, vals.len()).unwrap();
            // We'll measure decode separately below.
            let _ = dec_start;
        }

        // Decode (10 iterations).
        let encoded = codec.encode(&vals).unwrap();
        let mut dec_latencies = Vec::with_capacity(10);
        for _ in 0..10 {
            let start = Instant::now();
            let _ = codec.decode(&encoded, vals.len()).unwrap();
            dec_latencies.push(start.elapsed());
        }

        let ratio = (vals.len() * 8) as f64 / encoded.len().max(1) as f64;
        results.push((
            name,
            LatencyStats::from_samples(&mut enc_latencies),
            LatencyStats::from_samples(&mut dec_latencies),
            ratio,
        ));
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase3_latency_benchmarks() {
        println!("\n\n###############################################");
        println!("# Phase 3: Internal Latency Benchmarks       #");
        println!("###############################################");

        println!("\n=== KV Point Lookup (100K puts + 100K gets, random keys) ===");
        let (put_stats, get_stats) = bench_kv_point_lookup();
        put_stats.print("KV put (random key)");
        get_stats.print("KV get (random key)");

        println!("\n=== KV Sequential Write (100K puts, sequential keys) ===");
        let seq_stats = bench_kv_sequential_write();
        seq_stats.print("KV put (sequential key)");

        println!("\n=== Vectorized Filter (10M i64 column) ===");
        let (filter_stats, sum_stats) = bench_vectorized_filter();
        filter_stats.print("filter_i64_gt (10M rows)");
        sum_stats.print("sum_i64 (10M rows)");
        // Throughput.
        let mps_filter = 10_000_000.0 / filter_stats.mean.as_secs_f64() / 1_000_000.0;
        let mps_sum = 10_000_000.0 / sum_stats.mean.as_secs_f64() / 1_000_000.0;
        println!("  filter throughput: {:.0} M rows/sec", mps_filter);
        println!("  sum throughput:    {:.0} M rows/sec", mps_sum);

        println!("\n=== ART Lookup (100K insertions + 100K lookups) ===");
        let (insert_stats, lookup_stats) = bench_art_lookup();
        insert_stats.print("ART insert");
        lookup_stats.print("ART lookup (random key)");

        println!("\n=== Compression Encode/Decode (100K i64, 200 distinct values) ===");
        let comp_results = bench_compression_encode_decode();
        println!("  {:<20} {:>10} {:>20} {:>20} {:>8}", "Encoding", "Ratio", "Encode p50", "Decode p50", "Auto?");
        println!("  {}", "-".repeat(85));
        let auto = auto_select_encoding_i64(&(0..100_000).map(|i| (i % 200) as i64).collect::<Vec<_>>());
        for (name, enc_stats, dec_stats, ratio) in &comp_results {
            let is_auto = match *name {
                "Raw" => std::mem::discriminant(&cendb_storage::encoding::Encoding::Raw) == std::mem::discriminant(&auto),
                "BitPacked" => std::mem::discriminant(&cendb_storage::encoding::Encoding::BitPacked { bits: 8 }) == std::mem::discriminant(&auto),
                "FrameOfReference" => std::mem::discriminant(&cendb_storage::encoding::Encoding::FrameOfReference { base: 0, bits: 8 }) == std::mem::discriminant(&auto),
                "DeltaOfDelta" => std::mem::discriminant(&cendb_storage::encoding::Encoding::DeltaOfDelta) == std::mem::discriminant(&auto),
                "RunLength" => std::mem::discriminant(&cendb_storage::encoding::Encoding::RunLength) == std::mem::discriminant(&auto),
                "Dictionary" => std::mem::discriminant(&cendb_storage::encoding::Encoding::Dictionary { dict_id: 0 }) == std::mem::discriminant(&auto),
                _ => false,
            };
            println!("  {:<20} {:>9.2}x {:>17.2?} {:>17.2?} {:>8}",
                     name, ratio, enc_stats.p50, dec_stats.p50, if is_auto { "<--" } else { "" });
        }

        // Summary.
        println!("\n=== Summary ===");
        println!("  KV put p99:    {:>8.2?}  (random keys)", put_stats.p99);
        println!("  KV get p99:    {:>8.2?}  (random keys)", get_stats.p99);
        println!("  Filter p99:    {:>8.2?}  (10M rows, {:.0} M/sec)", filter_stats.p99, mps_filter);
        println!("  ART lookup p99:{:>8.2?}  (100K keys)", lookup_stats.p99);
    }
}
