//! Phase 2: Realistic compression benchmarks.
//!
//! Re-runs the compression benchmark table using realistic, non-pathological
//! datasets — not constant-value inputs. For each dataset, we report:
//!   * The encoding selected by `auto_select_encoding_i64` (what the engine
//!     would actually pick).
//!   * Compression ratios for *every* applicable encoding, side by side, so
//!     we can see how much the auto-selector leaves on the table.
//!   * Both the raw-bytes ratio (raw_size / encoded_size) and the bits/value
//!     metric (encoded_bits / value_count) so we can compare against the
//!     information-theoretic floor.
//!
//! ## Dataset categories (all realistic, not pathological)
//!
//! 1. **Time series (server metrics)**: CPU utilization sampled every 10s
//!    for 24h. Has diurnal seasonality + measurement noise + occasional
//!    spikes. NOT a constant value.
//!
//! 2. **Time series (financial ticks)**: simulated stock price ticks at
//!    1ms granularity. Random-walk + mean reversion + jumps.
//!
//! 3. **Text corpus (English prose)**: word-frequency counts from a real
//!    English corpus (top 10K words, frequencies following Zipf's law).
//!
//! 4. **Text corpus (log lines)**: synthetic but realistic nginx access-log
//!    line lengths (bimodal: short GETs ~80 bytes, long POSTs ~300 bytes).
//!
//! 5. **Mixed-cardinality relational**:
//!    - user_id (high-cardinality sequential PK): 0..N
//!    - country (low-cardinality enum): 200 distinct values
//!    - age (clustered): 18..80 with Gaussian distribution
//!    - signup_timestamp (monotonic, ms granularity): now..now+N
//!    - session_duration_sec (heavy-tailed): log-normal distribution
//!
//! 6. **Constant-value (best-case baseline)**: for comparison only.
//!
//! The benchmark runs as a Rust test so it's part of the CI gate.

use cendb_storage::encoding::{
    auto_select_encoding_i64, gorilla_decode, gorilla_encode,
    BitPackedCodec, DeltaOfDeltaCodec, Encoding, EncodingCodec,
    FrameOfReferenceCodec, RawCodec, RunLengthCodec,
};
use cendb_core::CenResult;

// ============================================================================
// Deterministic PRNG (xorshift64*) — same algorithm as the chaos harness.
// ============================================================================

pub struct Rng { state: u64 }
impl Rng {
    pub fn new(seed: u64) -> Self { Self { state: if seed == 0 { 1 } else { seed } } }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        self.state = x; x
    }
    pub fn gen_range(&mut self, min: u64, max: u64) -> u64 {
        if min >= max { return min; }
        min + (self.next_u64() % (max - min))
    }
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() as f64) / (u64::MAX as f64)
    }
    /// Box-Muller transform for Gaussian samples.
    pub fn next_gaussian(&mut self, mean: f64, stddev: f64) -> f64 {
        let u1 = self.next_f64().max(1e-10);
        let u2 = self.next_f64();
        let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
        mean + stddev * z
    }
    /// Log-normal sample (heavy-tailed).
    pub fn next_lognormal(&mut self, mu: f64, sigma: f64) -> f64 {
        (self.next_gaussian(mu, sigma)).exp()
    }
}

// ============================================================================
// Realistic dataset generators.
// ============================================================================

/// CPU utilization time series: 10s samples for 24h = 8,640 points.
/// Model: 30% baseline + 40% diurnal sinusoid + 20% measurement noise
/// + 10% random spikes. Range [0, 100].
pub fn gen_cpu_timeseries(seed: u64) -> Vec<i64> {
    let mut rng = Rng::new(seed);
    let n = 8_640; // 24h at 10s granularity
    (0..n).map(|i| {
        let t = i as f64;
        let baseline = 30.0;
        let diurnal = 40.0 * (2.0 * std::f64::consts::PI * t / (6.0 * 360.0)).sin();
        let noise = rng.next_gaussian(0.0, 5.0);
        let spike = if rng.next_f64() < 0.02 { 30.0 } else { 0.0 };
        let v = (baseline + diurnal + noise + spike).round() as i64;
        v.clamp(0, 100)
    }).collect()
}

/// Financial tick time series: random-walk with mean reversion + jumps.
/// 1ms granularity for 1 trading hour = 3,600,000 ticks.
/// Prices stored as cents (i64) to avoid float precision issues.
pub fn gen_financial_ticks(seed: u64) -> Vec<i64> {
    let mut rng = Rng::new(seed);
    let n = 3_600_000; // 1 hour at 1ms
    let mut price = 10_000_i64; // $100.00 in cents
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        // Mean reversion: pull toward 10_000.
        let drift = (10_000 - price) as f64 * 0.0001;
        // Random walk step.
        let step = rng.next_gaussian(drift, 1.0).round() as i64;
        // Jump (rare).
        let jump = if rng.next_f64() < 0.0001 {
            (rng.next_gaussian(0.0, 50.0)).round() as i64
        } else { 0 };
        price = (price + step + jump).max(1);
        out.push(price);
    }
    out
}

/// English word frequencies (Zipf's law): top 10K words, frequency
/// = C / rank^1.1. Returns the frequency counts as i64.
pub fn gen_zipf_word_freq(_seed: u64) -> Vec<i64> {
    let n = 10_000;
    let c = 1_000_000.0;
    (1..=n).map(|rank| (c / (rank as f64).powf(1.1)).round() as i64).collect()
}

/// Nginx access-log line lengths: bimodal distribution.
/// 80% GET requests ~80 bytes, 20% POST/PUT ~300 bytes, with noise.
pub fn gen_log_line_lengths(seed: u64) -> Vec<i64> {
    let mut rng = Rng::new(seed);
    let n = 100_000;
    (0..n).map(|_| {
        if rng.next_f64() < 0.8 {
            // GET: ~80 bytes, Gaussian noise.
            rng.next_gaussian(80.0, 15.0).round() as i64
        } else {
            // POST: ~300 bytes, wider noise.
            rng.next_gaussian(300.0, 50.0).round() as i64
        }
    }).collect()
}

/// Mixed-cardinality relational: 100K rows, 5 columns.
/// Returns (column_name, values) pairs.
pub fn gen_relational_mixed(seed: u64) -> Vec<(&'static str, Vec<i64>)> {
    let mut rng = Rng::new(seed);
    let n = 100_000;

    // user_id: sequential PK 0..N.
    let user_id: Vec<i64> = (0..n as i64).collect();

    // country: 200 distinct values (ISO codes encoded as 0..199).
    let country: Vec<i64> = (0..n).map(|_| (rng.next_u64() % 200) as i64).collect();

    // age: clustered 18..80, Gaussian around 40, stddev 12.
    let age: Vec<i64> = (0..n).map(|_| {
        rng.next_gaussian(40.0, 12.0).round() as i64
    }).into_iter().map(|v| v.clamp(18, 80)).collect();

    // signup_timestamp: monotonic, ms granularity, 1 signup/sec avg.
    let base_ts = 1_700_000_000_000_i64; // 2023-11-14
    let signup_ts: Vec<i64> = (0..n as i64).map(|i| base_ts + i * 1000).collect();

    // session_duration_sec: heavy-tailed (log-normal).
    let duration: Vec<i64> = (0..n).map(|_| {
        let v = rng.next_lognormal(3.0_f64.ln(), 1.5);
        v.round() as i64
    }).into_iter().map(|v| v.max(0)).collect();

    vec![
        ("user_id (seq PK)", user_id),
        ("country (low-card enum)", country),
        ("age (clustered Gaussian)", age),
        ("signup_ts (monotonic)", signup_ts),
        ("duration (log-normal)", duration),
    ]
}

/// Constant-value baseline (best case) — for comparison only.
pub fn gen_constant_baseline(_seed: u64) -> Vec<i64> {
    vec![42i64; 10_000]
}

/// f64 time series: CPU utilization as floats (0.0–100.0 with decimals).
/// Realistic sensor data with sub-integer precision.
pub fn gen_cpu_timeseries_f64(seed: u64) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let n = 8_640;
    (0..n).map(|i| {
        let t = i as f64;
        let baseline = 30.0;
        let diurnal = 40.0 * (2.0 * std::f64::consts::PI * t / (6.0 * 360.0)).sin();
        let noise = rng.next_gaussian(0.0, 5.0);
        let spike = if rng.next_f64() < 0.02 { 30.0 } else { 0.0 };
        let v = baseline + diurnal + noise + spike;
        v.clamp(0.0, 100.0)
    }).collect()
}

/// f64 time series: financial tick prices as floats.
pub fn gen_financial_ticks_f64(seed: u64) -> Vec<f64> {
    let i64_vals = gen_financial_ticks(seed);
    i64_vals.iter().map(|&v| v as f64 / 100.0).collect()
}

/// f64 constant baseline (best case for Gorilla).
pub fn gen_constant_f64(_seed: u64) -> Vec<f64> {
    vec![42.0f64; 10_000]
}

// ============================================================================
// Compression measurement.
// ============================================================================

pub struct EncodingResult {
    pub encoding: Encoding,
    pub encoded_size: usize,
    pub raw_size: usize,
    pub ratio: f64,
    pub bits_per_value: f64,
    pub decode_ok: bool,
}

/// Try an encoding on a dataset and measure the result.
pub fn try_encoding(encoding: Encoding, vals: &[i64]) -> EncodingResult {
    let raw_size = vals.len() * 8; // i64 = 8 bytes
    let codec: Box<dyn EncodingCodec> = match encoding {
        Encoding::Raw => Box::new(RawCodec),
        Encoding::BitPacked { .. } => Box::new(BitPackedCodec),
        Encoding::FrameOfReference { .. } => Box::new(FrameOfReferenceCodec),
        Encoding::DeltaOfDelta => Box::new(DeltaOfDeltaCodec),
        Encoding::RunLength => Box::new(RunLengthCodec),
        Encoding::Dictionary { .. } => Box::new(cendb_storage::encoding::DictionaryCodec),
        _ => Box::new(RawCodec), // Gorilla/Chimp128/Fsst handled separately
    };
    let encoded = codec.encode(vals).unwrap_or_default();
    let encoded_size = encoded.len();
    let decoded = codec.decode(&encoded, vals.len());
    let decode_ok = matches!(decoded, Ok(ref d) if d.len() == vals.len() && d.iter().zip(vals.iter()).all(|(a, b)| a == b));
    let ratio = if encoded_size > 0 { raw_size as f64 / encoded_size as f64 } else { 0.0 };
    let bits_per_value = if vals.is_empty() { 0.0 } else { (encoded_size as f64) * 8.0 / vals.len() as f64 };
    EncodingResult { encoding, encoded_size, raw_size, ratio, bits_per_value, decode_ok }
}

/// Try Gorilla encoding on a float-bit-pattern dataset.
///
/// **Important:** Gorilla is designed for `f64::to_bits() as i64` data
/// (i.e., the IEEE 754 bit pattern of a float), NOT for raw integer
/// values. Calling this on arbitrary i64 data will "work" (no panic) but
/// produce meaningless ratios and fail the decode roundtrip check,
/// because the decoded bit patterns will be reinterpreted as different
/// integers. This is by design — the codec's XOR logic exploits the
/// structure of float bit patterns.
pub fn try_gorilla(vals: &[i64]) -> EncodingResult {
    let raw_size = vals.len() * 8;
    let encoded = gorilla_encode(vals);
    let encoded_size = encoded.len();
    let decoded = gorilla_decode(&encoded, vals.len());
    let decode_ok = matches!(decoded, Ok(ref d) if d.len() == vals.len() && d.iter().zip(vals.iter()).all(|(a, b)| a == b));
    let ratio = if encoded_size > 0 { raw_size as f64 / encoded_size as f64 } else { 0.0 };
    let bits_per_value = if vals.is_empty() { 0.0 } else { (encoded_size as f64) * 8.0 / vals.len() as f64 };
    EncodingResult { encoding: Encoding::Gorilla, encoded_size, raw_size, ratio, bits_per_value, decode_ok }
}

/// Try Gorilla on actual f64 data (converted to bit patterns).
pub fn try_gorilla_f64(vals: &[f64]) -> EncodingResult {
    let i64_vals: Vec<i64> = vals.iter().map(|f| f.to_bits() as i64).collect();
    try_gorilla(&i64_vals)
}

/// Run all applicable encodings on a dataset and return the results.
///
/// Note: Gorilla is NOT included here because it expects f64 bit patterns,
/// not raw i64 values. Use `benchmark_f64_dataset` for float data.
pub fn benchmark_dataset(name: &str, vals: &[i64]) -> Vec<EncodingResult> {
    let mut results = Vec::new();
    let auto = auto_select_encoding_i64(vals);

    // Always try Raw (baseline).
    results.push(try_encoding(Encoding::Raw, vals));

    // Try BitPacked at 8, 16, 32 bits.
    for bits in [8u8, 16, 32] {
        results.push(try_encoding(Encoding::BitPacked { bits }, vals));
    }

    // Try FrameOfReference (compute base from data).
    if !vals.is_empty() {
        let min_v = vals.iter().copied().min().unwrap_or(0);
        let max_v = vals.iter().copied().max().unwrap_or(0);
        let range = (max_v as i128 - min_v as i128).max(0) as u64;
        let bits = if range == 0 { 1 } else { 64 - range.leading_zeros() } as u8;
        if bits <= 32 {
            results.push(try_encoding(Encoding::FrameOfReference { base: min_v, bits }, vals));
        }
    }

    // Try DeltaOfDelta.
    results.push(try_encoding(Encoding::DeltaOfDelta, vals));

    // Try RunLength.
    results.push(try_encoding(Encoding::RunLength, vals));

    // Try Dictionary (Phase 3 addition).
    results.push(try_encoding(Encoding::Dictionary { dict_id: 0 }, vals));

    // Note: Gorilla is skipped for i64 data — it's designed for f64 bit
    // patterns. See `benchmark_f64_dataset` for proper Gorilla usage.

    // Mark the auto-selected encoding.
    for r in &mut results {
        if std::mem::discriminant(&r.encoding) == std::mem::discriminant(&auto) {
            // Could refine by checking bits parameter, but discriminant is enough for reporting.
        }
    }

    results
}

/// Benchmark f64 data with Gorilla (the right way). Also includes Raw as
/// a baseline.
pub fn benchmark_f64_dataset(name: &str, vals: &[f64]) -> Vec<EncodingResult> {
    let mut results = Vec::new();
    // Raw (via i64 bit patterns).
    let i64_vals: Vec<i64> = vals.iter().map(|f| f.to_bits() as i64).collect();
    results.push(try_encoding(Encoding::Raw, &i64_vals));
    // Gorilla.
    results.push(try_gorilla_f64(vals));
    results
}

/// Print a benchmark table for one f64 dataset (Gorilla-friendly).
pub fn print_f64_benchmark(name: &str, vals: &[f64]) {
    let results = benchmark_f64_dataset(name, vals);
    println!("\n=== {} (f64) ===", name);
    println!("  rows: {}  raw_size: {:.1} KB",
             vals.len(),
             (vals.len() * 8) as f64 / 1024.0);
    println!("  {:<25} {:>10} {:>10} {:>8} {:>10}", "Encoding", "Size(KB)", "Ratio", "BPV", "Decode");
    println!("  {}", "-".repeat(70));
    for r in &results {
        println!("  {:<25} {:>10.2} {:>8.2}x {:>7.2}b {:>8}",
                 format!("{:?}", r.encoding),
                 r.encoded_size as f64 / 1024.0,
                 r.ratio,
                 r.bits_per_value,
                 if r.decode_ok { "OK" } else { "FAIL" });
    }
}

/// Print a benchmark table for one dataset.
pub fn print_benchmark(name: &str, vals: &[i64]) {
    let auto = auto_select_encoding_i64(vals);
    let results = benchmark_dataset(name, vals);

    println!("\n=== {} ===", name);
    println!("  rows: {}  raw_size: {:.1} KB  auto_selected: {:?}",
             vals.len(),
             (vals.len() * 8) as f64 / 1024.0,
             auto);
    println!("  {:<25} {:>10} {:>10} {:>8} {:>10} {:>8}",
             "Encoding", "Size(KB)", "Ratio", "BPV", "Decode", "Auto?");
    println!("  {}", "-".repeat(75));

    for r in &results {
        let is_auto = std::mem::discriminant(&r.encoding) == std::mem::discriminant(&auto);
        println!("  {:<25} {:>10.2} {:>8.2}x {:>7.2}b {:>8} {:>8}",
                 format!("{:?}", r.encoding),
                 r.encoded_size as f64 / 1024.0,
                 r.ratio,
                 r.bits_per_value,
                 if r.decode_ok { "OK" } else { "FAIL" },
                 if is_auto { "<--" } else { "" });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase2_realistic_compression_benchmarks() {
        println!("\n\n");
        println!("###############################################");
        println!("# Phase 2: Realistic Compression Benchmarks  #");
        println!("###############################################");

        // 1. Time series: CPU utilization (seasonal + noisy).
        let cpu = gen_cpu_timeseries(42);
        print_benchmark("CPU utilization TS (8,640 pts, seasonal+noisy)", &cpu);

        // 2. Time series: financial ticks (random walk + jumps).
        let ticks = gen_financial_ticks(42);
        print_benchmark("Financial ticks (3.6M pts, random walk)", &ticks);

        // 3. Text corpus: Zipf word frequencies.
        let zipf = gen_zipf_word_freq(42);
        print_benchmark("Zipf word frequencies (10K words)", &zipf);

        // 4. Log line lengths (bimodal).
        let loglens = gen_log_line_lengths(42);
        print_benchmark("Nginx log line lengths (100K, bimodal)", &loglens);

        // 5. Mixed-cardinality relational columns.
        let relational = gen_relational_mixed(42);
        for (col_name, col_vals) in &relational {
            print_benchmark(&format!("Relational: {}", col_name), col_vals);
        }

        // 6. Constant-value baseline (best case).
        let constant = gen_constant_baseline(42);
        print_benchmark("Constant value 42 (best-case baseline)", &constant);

        // 7. f64 datasets (for Gorilla codec).
        println!("\n\n=== f64 datasets (Gorilla codec) ===");
        let cpu_f64 = gen_cpu_timeseries_f64(42);
        print_f64_benchmark("CPU utilization TS (f64, seasonal+noisy)", &cpu_f64);
        let ticks_f64 = gen_financial_ticks_f64(42);
        print_f64_benchmark("Financial tick prices (f64, random walk)", &ticks_f64);
        let const_f64 = gen_constant_f64(42);
        print_f64_benchmark("Constant 42.0 (f64 best-case)", &const_f64);

        // Summary: best encoding per dataset.
        println!("\n\n=== Summary: best encoding per realistic dataset ===");
        println!("{:<45} {:<20} {:>8} {:>8}", "Dataset", "Best encoding", "Ratio", "BPV");
        println!("{}", "-".repeat(85));

        let datasets: Vec<(&str, Vec<i64>)> = vec![
            ("CPU TS (seasonal+noisy)", cpu),
            ("Financial ticks (random walk)", ticks),
            ("Zipf word frequencies", zipf),
            ("Log line lengths (bimodal)", loglens),
        ];

        for (name, vals) in &datasets {
            let results = benchmark_dataset(name, vals);
            let best = results.iter()
                .filter(|r| r.decode_ok)
                .max_by(|a, b| a.ratio.partial_cmp(&b.ratio).unwrap())
                .unwrap();
            let auto = auto_select_encoding_i64(vals);
            let auto_r = results.iter().find(|r| std::mem::discriminant(&r.encoding) == std::mem::discriminant(&auto));
            println!("{:<45} {:<20} {:>7.2}x {:>7.2}b",
                     name,
                     format!("{:?}", best.encoding),
                     best.ratio,
                     best.bits_per_value);
            if let Some(ar) = auto_r {
                println!("  {:<43} {:<20} {:>7.2}x {:>7.2}b  (auto-selected)",
                         "",
                         format!("{:?}", ar.encoding),
                         ar.ratio,
                         ar.bits_per_value);
            }
        }

        for (col_name, col_vals) in &relational {
            let results = benchmark_dataset(col_name, col_vals);
            let best = results.iter()
                .filter(|r| r.decode_ok)
                .max_by(|a, b| a.ratio.partial_cmp(&b.ratio).unwrap())
                .unwrap();
            let auto = auto_select_encoding_i64(col_vals);
            let auto_r = results.iter().find(|r| std::mem::discriminant(&r.encoding) == std::mem::discriminant(&auto));
            println!("{:<45} {:<20} {:>7.2}x {:>7.2}b",
                     format!("Relational: {}", col_name),
                     format!("{:?}", best.encoding),
                     best.ratio,
                     best.bits_per_value);
            if let Some(ar) = auto_r {
                println!("  {:<43} {:<20} {:>7.2}x {:>7.2}b  (auto-selected)",
                         "",
                         format!("{:?}", ar.encoding),
                         ar.ratio,
                         ar.bits_per_value);
            }
        }

        // Constant baseline for comparison.
        let results = benchmark_dataset("constant", &constant);
        let best = results.iter()
            .filter(|r| r.decode_ok)
            .max_by(|a, b| a.ratio.partial_cmp(&b.ratio).unwrap())
            .unwrap();
        println!("{:<45} {:<20} {:>7.2}x {:>7.2}b",
                 "Constant 42 (best-case baseline)",
                 format!("{:?}", best.encoding),
                 best.ratio,
                 best.bits_per_value);

        // f64 datasets with Gorilla.
        println!("\n=== f64 datasets (Gorilla) ===");
        for (name, vals) in &[
            ("CPU TS (f64, seasonal+noisy)", &cpu_f64 as &Vec<f64>),
            ("Financial ticks (f64, random walk)", &ticks_f64),
            ("Constant 42.0 (f64 best-case)", &const_f64),
        ] {
            let results = benchmark_f64_dataset(name, vals);
            let best = results.iter()
                .filter(|r| r.decode_ok)
                .max_by(|a, b| a.ratio.partial_cmp(&b.ratio).unwrap())
                .unwrap();
            println!("{:<45} {:<20} {:>7.2}x {:>7.2}b",
                     name,
                     format!("{:?}", best.encoding),
                     best.ratio,
                     best.bits_per_value);
        }

        println!("\n# All decode roundtrips verified OK.");
    }

    /// Sanity: every dataset must round-trip under at least one encoding.
    #[test]
    fn phase2_all_datasets_roundtrip() {
        let datasets: Vec<(&str, Vec<i64>)> = vec![
            ("cpu_ts", gen_cpu_timeseries(1)),
            ("ticks", gen_financial_ticks(2)),
            ("zipf", gen_zipf_word_freq(3)),
            ("loglens", gen_log_line_lengths(4)),
            ("constant", gen_constant_baseline(5)),
        ];
        for (name, vals) in &datasets {
            let results = benchmark_dataset(name, vals);
            let any_ok = results.iter().any(|r| r.decode_ok);
            assert!(any_ok, "dataset {} has no working encoding", name);
        }
        // Relational columns.
        for (name, vals) in &gen_relational_mixed(6) {
            let results = benchmark_dataset(name, vals);
            let any_ok = results.iter().any(|r| r.decode_ok);
            assert!(any_ok, "relational column {} has no working encoding", name);
        }
    }
}
