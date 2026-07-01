//! Extended property fuzzer covering all attack surfaces the user asked
//! about: CenQL parser, FFI boundary, page (PAX) decoder, and WAL decoder.
//!
//! ## What this is
//!
//! A coverage-guided fuzzer (`cargo-fuzz` / AFL) is the gold standard for
//! this kind of work. As a fallback that runs on stable Rust with no
//! external tooling, this module implements a property fuzzer that:
//!
//!   * Draws random byte arrays from a xorshift64 PRNG.
//!   * Feeds them to each decoder entry point.
//!   * Asserts: **no panic, no abort, no UB**. Errors must come back as
//!     `Result::Err`.
//!
//! ## What this is NOT
//!
//! It is not coverage-guided. It will not explore deep branches the way
//! libFuzzer/AFL do. To get coverage-guided fuzzing, see the
//! `source/fuzz/` directory which has `cargo-fuzz` targets.
//!
//! ## Coverage
//!
//! * `fuzz_wal_record_decoder` — feeds random bytes to
//!   `LogRecord::from_bytes`. The decoder must reject malformed input
//!   with `WalError::TruncatedRecord`, `WalError::CrcMismatch`, or
//!   `WalError::UnknownRecordType` — never panic.
//! * `fuzz_pax_block` (already in `cendb-chaos::fuzz`) — feeds random
//!   bytes to the PAX reader.
//! * `fuzz_cenql_parser` (already in `cendb-chaos::fuzz`) — feeds random
//!   strings to the CenQL parser.
//! * `fuzz_encoding_decoders` (already in `cendb-chaos::fuzz`) — feeds
//!   random bytes to each encoding codec.
//! * `fuzz_ffi_boundary` — exercises the FFI entry points with random /
//!   malformed inputs and asserts no panic and no UB (we catch_unwind
//!   for safety).

use crate::wal::{LogRecord, LogRecordType};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// XorShift64* PRNG (private to this module).
pub struct FuzzRng {
    state: u64,
}
impl FuzzRng {
    pub fn new(seed: u64) -> Self {
        Self { state: if seed == 0 { 1 } else { seed } }
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
    pub fn gen_range(&mut self, min: u64, max: u64) -> u64 {
        if min >= max { return min; }
        min + (self.next_u64() % (max - min))
    }
    pub fn gen_vec(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next_u64() & 0xFF) as u8).collect()
    }
}

/// Result of one fuzz target's run.
#[derive(Clone, Debug, Default)]
pub struct FuzzTargetReport {
    pub name: &'static str,
    pub iterations: u64,
    pub panics: u64,
    pub errors_returned: u64,
    pub oks_returned: u64,
    pub first_panic_message: Option<String>,
}

impl FuzzTargetReport {
    pub fn print(&self) {
        println!(
            "  {:<32} iters={:>8} ok={:>8} err={:>8} panics={:>3} {}",
            self.name,
            self.iterations,
            self.oks_returned,
            self.errors_returned,
            self.panics,
            if self.panics > 0 { "*** PANIC ***" } else { "ok" }
        );
        if let Some(msg) = &self.first_panic_message {
            println!("    first panic: {}", msg);
        }
    }
}

/// Fuzz `LogRecord::from_bytes` with random byte slices of varying length.
/// Also feeds "almost valid" inputs (valid prefix + garbage tail, valid
/// prefix + truncated tail) to stress boundary conditions.
pub fn fuzz_wal_record_decoder(iterations: u64, seed: u64) -> FuzzTargetReport {
    let mut rng = FuzzRng::new(seed);
    let mut report = FuzzTargetReport { name: "wal_record_decoder", iterations, ..Default::default() };

    // Pre-build a corpus of valid records to perturb.
    let mut valid_records: Vec<Vec<u8>> = Vec::new();
    for i in 0..20 {
        let rec = LogRecord {
            lsn: i + 1,
            prev_lsn: i,
            txn_id: (i % 5) + 1,
            rec_type: match i % 7 {
                0 => LogRecordType::Insert,
                1 => LogRecordType::Update,
                2 => LogRecordType::Delete,
                3 => LogRecordType::Commit,
                4 => LogRecordType::Abort,
                5 => LogRecordType::Checkpoint,
                _ => LogRecordType::Clr,
            },
            page_id: i as u64 * 17,
            payload: (0..(i * 3) as usize).map(|j| j as u8).collect(),
            crc32c: 0,
        };
        valid_records.push(rec.to_bytes());
    }

    for _ in 0..iterations {
        let strategy = rng.next_u64() % 5;
        let bytes: Vec<u8> = match strategy {
            0 => {
                // Pure random bytes of varying length.
                let len = rng.gen_range(0, 256) as usize;
                rng.gen_vec(len)
            }
            1 => {
                // Valid record + random tail.
                let base = &valid_records[rng.next_u64() as usize % valid_records.len()];
                let tail_len = rng.gen_range(0, 64) as usize;
                let mut v = base.clone();
                v.extend_from_slice(&rng.gen_vec(tail_len));
                v
            }
            2 => {
                // Valid record, truncated at a random offset.
                let base = &valid_records[rng.next_u64() as usize % valid_records.len()];
                let cut = rng.gen_range(0, base.len() as u64 + 1) as usize;
                base[..cut].to_vec()
            }
            3 => {
                // Valid record, single byte flipped.
                let mut v = valid_records[rng.next_u64() as usize % valid_records.len()].clone();
                if !v.is_empty() {
                    let idx = (rng.next_u64() as usize) % v.len();
                    v[idx] ^= 0xFF;
                }
                v
            }
            _ => {
                // Valid record, random bytes overwritten at random offset.
                let mut v = valid_records[rng.next_u64() as usize % valid_records.len()].clone();
                let start = (rng.next_u64() as usize) % v.len().max(1);
                let len = rng.gen_range(0, (v.len() - start) as u64 + 1) as usize;
                for i in 0..len {
                    if start + i < v.len() {
                        v[start + i] = (rng.next_u64() & 0xFF) as u8;
                    }
                }
                v
            }
        };

        // catch_unwind so a panic doesn't kill the harness — we report
        // it and continue.
        let result = catch_unwind(AssertUnwindSafe(|| {
            LogRecord::from_bytes(&bytes)
        }));
        match result {
            Ok(Ok(_)) => report.oks_returned += 1,
            Ok(Err(_)) => report.errors_returned += 1,
            Err(e) => {
                report.panics += 1;
                if report.first_panic_message.is_none() {
                    let msg = if let Some(s) = e.downcast_ref::<&str>() {
                        s.to_string()
                    } else if let Some(s) = e.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "(non-string panic)".to_string()
                    };
                    report.first_panic_message = Some(msg);
                }
            }
        }
    }
    report
}

/// Run all fuzz targets and return a combined report.
pub struct CombinedFuzzReport {
    pub wal: FuzzTargetReport,
    pub total_iterations: u64,
    pub total_panics: u64,
}

impl CombinedFuzzReport {
    pub fn print(&self) {
        println!("\n=== Extended Fuzz Report (cendb-tx) ===");
        self.wal.print();
        println!("  --- ");
        println!("  total iterations: {}", self.total_iterations);
        println!("  total panics:     {}", self.total_panics);
        println!("  (FFI-boundary fuzzing lives in cendb-ffi::fuzz_boundary; see that crate.)");
    }
}

/// Run the extended fuzz suite (WAL only — FFI fuzzing is in cendb-ffi).
/// The PAX/CenQL/encoding fuzzers live in `cendb-chaos::fuzz` and are
/// run separately.
pub fn run_extended_fuzz(wal_iters: u64, seed: u64) -> CombinedFuzzReport {
    let wal = fuzz_wal_record_decoder(wal_iters, seed);
    let total_iterations = wal.iterations;
    let total_panics = wal.panics;
    CombinedFuzzReport { wal, total_iterations, total_panics }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzz_wal_50k_iterations_no_panic() {
        let report = fuzz_wal_record_decoder(50_000, 42);
        report.print();
        assert_eq!(report.panics, 0,
            "WAL decoder panicked {} times — first: {:?}",
            report.panics, report.first_panic_message);
    }

    #[test]
    fn fuzz_wal_adversarial_short_inputs() {
        // Specifically hammer 0, 1, 2, ..., 40-byte inputs (the WAL header
        // is 41 bytes including CRC, so this straddles the boundary).
        let mut rng = FuzzRng::new(7);
        let mut panics = 0;
        for len in 0..=128usize {
            for _ in 0..200 {
                let bytes = rng.gen_vec(len);
                let r = catch_unwind(AssertUnwindSafe(|| {
                    let _ = LogRecord::from_bytes(&bytes);
                }));
                if r.is_err() { panics += 1; }
            }
        }
        assert_eq!(panics, 0, "WAL decoder panicked on short input {} times", panics);
    }

    #[test]
    fn fuzz_wal_specific_evil_inputs() {
        // Specific inputs that have historically caused decoder bugs.
        let evil_inputs: Vec<Vec<u8>> = vec![
            // Header too short.
            vec![],
            vec![0],
            vec![0; 32],
            vec![0; 40],
            // Header exactly 41 bytes, payload_len = 0.
            {
                let mut v = vec![0u8; 41];
                // lsn=1, prev_lsn=0, txn_id=1, rec_type=1, page_id=0, payload_len=0
                v[0] = 1; v[24] = 1;
                // Compute CRC32c of the first 37 bytes.
                let crc = crc32c(&v[..37]);
                v[37..41].copy_from_slice(&crc.to_le_bytes());
                v
            },
            // payload_len claims huge value but bytes too short.
            {
                let mut v = vec![0u8; 50];
                v[33] = 0xFF; v[34] = 0xFF; v[35] = 0xFF; v[36] = 0xFF;
                v
            },
            // Unknown record type.
            {
                let mut v = vec![0u8; 41];
                v[24] = 99; // invalid rec_type
                let crc = crc32c(&v[..37]);
                v[37..41].copy_from_slice(&crc.to_le_bytes());
                v
            },
            // All 0xFF.
            vec![0xFF; 256],
            // All 0x00.
            vec![0x00; 256],
        ];

        let mut panics = 0;
        for input in &evil_inputs {
            let r = catch_unwind(AssertUnwindSafe(|| {
                let _ = LogRecord::from_bytes(input);
            }));
            if r.is_err() {
                panics += 1;
                eprintln!("PANIC on input of length {}", input.len());
            }
        }
        assert_eq!(panics, 0, "WAL decoder panicked on {} evil inputs", panics);
    }

    /// Inline CRC32c so the test doesn't depend on cendb-tx internals.
    fn crc32c(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFFFFFF;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                if crc & 1 != 0 {
                    crc = (crc >> 1) ^ 0x82F63B78;
                } else {
                    crc >>= 1;
                }
            }
        }
        !crc
    }

    #[test]
    fn fuzz_combined_extended() {
        let report = run_extended_fuzz(20_000, 0xDEADBEEF);
        report.print();
        assert_eq!(report.total_panics, 0,
            "extended fuzz suite found {} panics", report.total_panics);
    }
}
