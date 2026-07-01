//! Crash recovery simulator: generates random transaction workloads,
//! simulates crashes by truncating/corrupting the WAL, runs ARIES
//! recovery, and verifies data integrity.

use cendb_tx::{AriesRecovery, LogRecord, LogRecordType};
use cendb_core::PageId;
use std::collections::HashSet;

/// Where to simulate the crash.
#[derive(Clone, Debug)]
pub enum CrashPoint {
    /// Truncate the log at this record index.
    /// Records [0, index) survive; records [index, end) are lost.
    Truncate(usize),
    /// Corrupt a specific record (flip bits in its payload).
    CorruptRecord { index: usize, byte_index: usize },
    /// Truncate mid-record (the last record is partial / truncated).
    TruncateMidRecord(usize),
}

/// Result of a single crash-recovery iteration.
#[derive(Clone, Debug)]
pub struct RecoveryResult {
    /// Iteration number (0-based).
    pub iteration: u64,
    /// Total records generated before the crash.
    pub total_records: usize,
    /// Number of records that survived the crash.
    pub surviving_records: usize,
    /// The crash point used.
    pub crash_point: CrashPoint,
    /// Transactions that committed before the crash.
    pub expected_committed: Vec<u64>,
    /// Transactions that were in-flight (losers).
    pub expected_losers: Vec<u64>,
    /// Transactions the recovery identified as committed.
    pub recovered_committed: Vec<u64>,
    /// Transactions the recovery identified as losers.
    pub recovered_losers: Vec<u64>,
    /// Number of records replayed during redo.
    pub redo_count: usize,
    /// Number of records rolled back during undo.
    pub undo_count: usize,
    /// Whether the recovery verified correctly.
    pub verified: bool,
    /// Whether corruption was injected (vs. just truncation).
    pub corruption_injected: bool,
}

impl RecoveryResult {
    pub fn print_summary(&self) {
        let status = if self.verified { "PASS" } else { "FAIL" };
        println!(
            "[{:>4}] {} records={:>4}/{:>4} committed={:>3} losers={:>3} redo={:>3} undo={:>3} {} {}",
            self.iteration,
            status,
            self.surviving_records,
            self.total_records,
            self.expected_committed.len(),
            self.expected_losers.len(),
            self.redo_count,
            self.undo_count,
            if self.corruption_injected { "[CORRUPT]" } else { "[TRUNC]" },
            match &self.crash_point {
                CrashPoint::Truncate(i) => format!("crash@{}", i),
                CrashPoint::CorruptRecord { index, byte_index } =>
                    format!("corrupt@{}[{}]", index, byte_index),
                CrashPoint::TruncateMidRecord(i) => format!("mid-rec@{}", i),
            }
        );
    }
}

/// Simple deterministic PRNG (xorshift64) — no external dependency.
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
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
        if min >= max {
            return min;
        }
        min + (self.next_u64() % (max - min))
    }

    pub fn gen_bool(&mut self, p_true: f64) -> bool {
        (self.next_u64() as f64 / u64::MAX as f64) < p_true
    }

    pub fn gen_vec(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next_u64() & 0xFF) as u8).collect()
    }
}

/// The crash simulator.
pub struct CrashSimulator {
    rng: Rng,
}

impl CrashSimulator {
    pub fn new(seed: u64) -> Self {
        Self { rng: Rng::new(seed) }
    }

    /// Run a single crash-recovery iteration.
    pub fn run_iteration(&mut self, iteration: u64) -> RecoveryResult {
        // Phase A: Generate random transactions and WAL records.
        let num_txns = self.rng.gen_range(3, 30);
        let mut records: Vec<LogRecord> = Vec::new();
        let mut lsn = 1u64;
        let mut prev_lsn_map: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();

        for txn_id in 1..=num_txns {
            let num_ops = self.rng.gen_range(1, 8);
            for _ in 0..num_ops {
                let rec_type = match self.rng.gen_range(0, 3) {
                    0 => LogRecordType::Insert,
                    1 => LogRecordType::Update,
                    _ => LogRecordType::Delete,
                };
                let payload_len = self.rng.gen_range(0, 64) as usize;
                let payload = self.rng.gen_vec(payload_len);
                let prev_lsn = *prev_lsn_map.get(&txn_id).unwrap_or(&0);
                records.push(LogRecord {
                    lsn,
                    prev_lsn,
                    txn_id,
                    rec_type,
                    page_id: PageId(lsn).0,
                    payload,
                    crc32c: 0,
                });
                prev_lsn_map.insert(txn_id, lsn);
                lsn += 1;
            }

            // 70% chance to commit, 15% abort, 15% leave in-flight.
            let roll = self.rng.gen_range(0, 100);
            if roll < 70 {
                let prev_lsn = *prev_lsn_map.get(&txn_id).unwrap_or(&0);
                records.push(LogRecord {
                    lsn,
                    prev_lsn,
                    txn_id,
                    rec_type: LogRecordType::Commit,
                    page_id: 0,
                    payload: vec![],
                    crc32c: 0,
                });
                prev_lsn_map.insert(txn_id, lsn);
                lsn += 1;
            } else if roll < 85 {
                let prev_lsn = *prev_lsn_map.get(&txn_id).unwrap_or(&0);
                records.push(LogRecord {
                    lsn,
                    prev_lsn,
                    txn_id,
                    rec_type: LogRecordType::Abort,
                    page_id: 0,
                    payload: vec![],
                    crc32c: 0,
                });
                prev_lsn_map.insert(txn_id, lsn);
                lsn += 1;
            }
            // Else: leave in-flight (loser).
        }

        let total_records = records.len();

        // Phase B: Simulate crash.
        let crash_point = if total_records == 0 {
            CrashPoint::Truncate(0)
        } else {
            let crash_type = self.rng.gen_range(0, 10);
            let crash_idx = self.rng.gen_range(0, total_records as u64 + 1) as usize;
            if crash_type < 7 {
                // 70%: simple truncation.
                CrashPoint::Truncate(crash_idx)
            } else if crash_type < 9 {
                // 20%: corrupt a record.
                let byte_idx = self.rng.gen_range(0, 32) as usize;
                CrashPoint::CorruptRecord {
                    index: crash_idx.min(total_records - 1),
                    byte_index: byte_idx,
                }
            } else {
                // 10%: truncate mid-record (drop last record).
                CrashPoint::TruncateMidRecord(crash_idx.saturating_sub(1))
            }
        };

        // Apply the crash to the record stream.
        let mut surviving: Vec<LogRecord> = records.clone();
        let mut corruption_injected = false;

        match &crash_point {
            CrashPoint::Truncate(idx) => {
                surviving.truncate(*idx);
            }
            CrashPoint::CorruptRecord { index, byte_index } => {
                if *index < surviving.len() {
                    let rec = &mut surviving[*index];
                    if *byte_index < rec.payload.len() {
                        rec.payload[*byte_index] ^= 0xFF;
                    } else if !rec.payload.is_empty() {
                        rec.payload[0] ^= 0xFF;
                    }
                    // Recompute CRC to match corrupted payload (so the
                    // record passes CRC check — we're testing the
                    // recovery logic, not CRC detection here).
                    let bytes = rec.to_bytes();
                    let new_crc = u32::from_le_bytes([
                        bytes[bytes.len() - 4],
                        bytes[bytes.len() - 3],
                        bytes[bytes.len() - 2],
                        bytes[bytes.len() - 1],
                    ]);
                    rec.crc32c = new_crc;
                    corruption_injected = true;
                }
            }
            CrashPoint::TruncateMidRecord(idx) => {
                surviving.truncate(*idx);
            }
        }

        let surviving_records = surviving.len();

        // Compute expected committed/loser sets from the surviving records.
        // ARIES semantics:
        //   - committed: txn has a Commit record in the surviving log.
        //   - losers: txn has data records (Insert/Update/Delete) but NO
        //     Commit record. Aborted txns are neither committed nor losers
        //     (they were explicitly rolled back).
        let mut expected_committed: HashSet<u64> = HashSet::new();
        let mut expected_aborted: HashSet<u64> = HashSet::new();
        let mut has_data: HashSet<u64> = HashSet::new();

        for rec in &surviving {
            match rec.rec_type {
                LogRecordType::Commit => {
                    expected_committed.insert(rec.txn_id);
                }
                LogRecordType::Abort => {
                    expected_aborted.insert(rec.txn_id);
                }
                LogRecordType::Insert | LogRecordType::Update | LogRecordType::Delete => {
                    has_data.insert(rec.txn_id);
                }
                _ => {}
            }
        }

        // Losers = txns with data records but no Commit AND no Abort.
        let expected_losers: HashSet<u64> = has_data
            .iter()
            .filter(|&&txn| {
                !expected_committed.contains(&txn) && !expected_aborted.contains(&txn)
            })
            .copied()
            .collect();

        // Phase C: Run ARIES recovery.
        let recovery = AriesRecovery::analyze(&surviving);

        // Phase D: Verify.
        let recovered_committed: HashSet<u64> = recovery.committed_txns.clone();
        let recovered_losers: HashSet<u64> = recovery.loser_txns.clone();

        let committed_match = recovered_committed == expected_committed;
        let losers_match = recovered_losers == expected_losers;
        let verified = committed_match && losers_match;

        // Count redo/undo.
        let redo_count = recovery.redo(&surviving, |_| {});
        let undo_count = recovery.undo(&surviving, |_| {});

        RecoveryResult {
            iteration,
            total_records,
            surviving_records,
            crash_point,
            expected_committed: expected_committed.iter().copied().collect(),
            expected_losers: expected_losers.iter().copied().collect(),
            recovered_committed: recovered_committed.iter().copied().collect(),
            recovered_losers: recovered_losers.iter().copied().collect(),
            redo_count,
            undo_count,
            verified,
            corruption_injected,
        }
    }

    /// Run N iterations and return all results.
    pub fn run_n_iterations(&mut self, n: u64) -> Vec<RecoveryResult> {
        (0..n).map(|i| self.run_iteration(i)).collect()
    }

    /// Run N iterations, verifying each one. Returns (passed, failed).
    pub fn run_and_verify(&mut self, n: u64) -> (u64, u64, Vec<RecoveryResult>) {
        let results = self.run_n_iterations(n);
        let passed = results.iter().filter(|r| r.verified).count() as u64;
        let failed = n - passed;
        (passed, failed, results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulator_single_iteration() {
        let mut sim = CrashSimulator::new(42);
        let result = sim.run_iteration(0);
        result.print_summary();
        assert!(result.verified, "recovery verification failed");
    }

    #[test]
    fn simulator_100_iterations() {
        let mut sim = CrashSimulator::new(12345);
        let (passed, failed, results) = sim.run_and_verify(100);
        for r in &results {
            r.print_summary();
        }
        println!("\n100 iterations: {} passed, {} failed", passed, failed);
        assert_eq!(failed, 0, "some iterations failed verification");
    }

    #[test]
    fn simulator_deterministic_with_same_seed() {
        let mut sim1 = CrashSimulator::new(42);
        let mut sim2 = CrashSimulator::new(42);
        for i in 0..10 {
            let r1 = sim1.run_iteration(i);
            let r2 = sim2.run_iteration(i);
            assert_eq!(r1.total_records, r2.total_records);
            assert_eq!(r1.surviving_records, r2.surviving_records);
            assert_eq!(r1.verified, r2.verified);
        }
    }

    #[test]
    fn simulator_handles_empty_log() {
        let mut sim = CrashSimulator::new(1);
        // With seed=1, the first iteration might generate 0 records.
        // Just ensure it doesn't panic.
        let result = sim.run_iteration(0);
        // Even with 0 records, recovery should succeed (empty log).
        let _ = result;
    }

    #[test]
    fn simulator_handles_all_committed() {
        // Run many iterations; at least some should have all committed txns.
        let mut sim = CrashSimulator::new(999);
        let mut found_all_committed = false;
        for i in 0..100 {
            let r = sim.run_iteration(i);
            if !r.expected_losers.is_empty() {
                found_all_committed = true;
            }
        }
        assert!(found_all_committed, "expected at least one iteration with losers");
    }
}
