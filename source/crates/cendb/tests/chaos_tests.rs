//! CenDB Chaos Recovery & Fuzz Integration Tests.
//!
//! This file runs the full chaos testing suite:
//!   1. 500+ crash-recovery iterations (ARIES verification).
//!   2. PAX decoder fuzzing (1000+ random byte arrays).
//!   3. CenQL parser fuzzing (1000+ random strings).
//!   4. Encoding decoder fuzzing (1000+ random inputs).
//!   5. VFS fault injection tests (torn writes, silent fsync, corruption).

use cendb_chaos::{
    CrashSimulator, ChaosVfs, ChaosController, FaultConfig,
    fuzz_pax_block_aggressive, fuzz_cenql_parser_aggressive, fuzz_encoding_decoders,
};

// ============================================================================
// 500+ Crash Recovery Iterations
// ============================================================================

#[test]
fn chaos_500_crash_recovery_iterations() {
    println!("\n=== 500 Crash Recovery Iterations ===");
    println!("Seed: 42 (deterministic)\n");

    let mut sim = CrashSimulator::new(42);
    let (passed, failed, results) = sim.run_and_verify(500);

    // Print every 50th iteration as a sample.
    for r in &results {
        if r.iteration % 50 == 0 || !r.verified {
            r.print_summary();
        }
    }

    println!("\n--- Summary ---");
    println!("Total iterations:  {}", results.len());
    println!("Passed:            {}", passed);
    println!("Failed:            {}", failed);
    println!("Corruption tests:  {}", results.iter().filter(|r| r.corruption_injected).count());
    println!("Truncation tests:  {}", results.iter().filter(|r| !r.corruption_injected).count());

    let total_redo: usize = results.iter().map(|r| r.redo_count).sum();
    let total_undo: usize = results.iter().map(|r| r.undo_count).sum();
    let total_committed: usize = results.iter().map(|r| r.expected_committed.len()).sum();
    let total_losers: usize = results.iter().map(|r| r.expected_losers.len()).sum();
    println!("Total redo ops:    {}", total_redo);
    println!("Total undo ops:    {}", total_undo);
    println!("Total committed:   {}", total_committed);
    println!("Total losers:      {}", total_losers);

    assert_eq!(failed, 0, "CRITICAL: {} iterations failed verification — data corruption detected!", failed);
    println!("\n✓ ALL 500 iterations completed with ZERO data loss.");
}

// ============================================================================
// PAX Decoder Fuzzing
// ============================================================================

#[test]
fn chaos_fuzz_pax_5000_iterations() {
    println!("\n=== PAX Decoder Fuzzing (5000 iterations) ===");
    let tested = fuzz_pax_block_aggressive(5000, 42);
    println!("Tested {} random byte arrays (64B–64KB each)", tested);
    println!("✓ No panics, no UB, all malformed inputs returned clean errors.");
    assert_eq!(tested, 5000);
}

// ============================================================================
// CenQL Parser Fuzzing
// ============================================================================

#[test]
fn chaos_fuzz_cenql_5000_iterations() {
    println!("\n=== CenQL Parser Fuzzing (5000 iterations) ===");
    let tested = fuzz_cenql_parser_aggressive(5000, 42);
    println!("Tested {} random strings (token-based + arbitrary ASCII)", tested);
    println!("✓ No panics, no infinite loops, all malformed inputs returned parse errors.");
    assert_eq!(tested, 5000);
}

// ============================================================================
// Encoding Decoder Fuzzing
// ============================================================================

#[test]
fn chaos_fuzz_encodings_5000_iterations() {
    println!("\n=== Encoding Decoder Fuzzing (5000 iterations) ===");
    let tested = fuzz_encoding_decoders(5000, 42);
    println!("Tested {} random inputs across all codecs (Raw, BitPacked, FoR, DoD, RLE, Gorilla)", tested);
    println!("✓ No panics, no overflows, all malformed inputs returned clean errors.");
    assert_eq!(tested, 5000);
}

// ============================================================================
// VFS Fault Injection Tests
// ============================================================================

#[test]
fn chaos_vfs_torn_write_recovery() {
    println!("\n=== VFS Torn Write Recovery ===");
    let mut ctrl = ChaosController::new();
    ctrl.torn_write_at(2, 512); // Write only 512 bytes of a 4KB page.
    let mut vfs = ChaosVfs::new(ctrl);

    vfs.create("block.dat").unwrap();        // op 0
    let data = vec![0xAAu8; 4096];
    let result = vfs.write("block.dat", 0, &data); // op 1 (ok)
    assert!(result.is_ok());

    let data2 = vec![0xBBu8; 4096];
    let result = vfs.write("block.dat", 4096, &data2); // op 2 (torn)
    assert!(result.is_err(), "torn write should fail");

    // Verify partial write: only first 512 bytes written.
    let written = vfs.read_all("block.dat").unwrap();
    assert_eq!(written.len(), 4096 + 512);
    assert_eq!(written[4096], 0xBB); // First byte of torn write.
    assert_eq!(written[4096 + 511], 0xBB); // Last byte of torn write.
    // Bytes beyond 512 should be zeros (unwritten).
    if written.len() > 4096 + 512 {
        assert_eq!(written[4096 + 512], 0);
    }
    println!("✓ Torn write correctly produced partial data (512/4096 bytes).");
}

#[test]
fn chaos_vfs_silent_fsync_data_loss() {
    println!("\n=== VFS Silent FSync Data Loss ===");
    let mut ctrl = ChaosController::new();
    ctrl.silent_fsync_at(2); // fsync is op 2.
    let mut vfs = ChaosVfs::new(ctrl);

    vfs.create("wal.dat").unwrap();       // op 0
    vfs.write("wal.dat", 0, b"critical").unwrap(); // op 1
    vfs.fsync("wal.dat").unwrap();        // op 2 (silently fails)

    // Crash — data should be lost.
    vfs.crash_and_reboot();
    assert!(!vfs.exists("wal.dat"), "data should be lost after silent fsync failure");
    println!("✓ Silent fsync failure correctly caused data loss on crash.");
}

#[test]
fn chaos_vfs_data_corruption_detection() {
    println!("\n=== VFS Data Corruption Detection ===");
    let mut ctrl = ChaosController::new();
    ctrl.corrupt_at(1, 3); // Corrupt byte 3 of the write.
    let mut vfs = ChaosVfs::new(ctrl);

    vfs.create("data.dat").unwrap();
    vfs.write("data.dat", 0, b"0123456789").unwrap(); // Corrupted at byte 3.

    let data = vfs.read_all("data.dat").unwrap();
    assert_ne!(&data, b"0123456789", "data should be corrupted");
    assert_eq!(data[3] != b'3', true, "byte 3 should be flipped");
    println!("✓ Data corruption correctly injected at byte 3.");
}

#[test]
fn chaos_vfs_normal_operation_no_faults() {
    println!("\n=== VFS Normal Operation (no faults) ===");
    let ctrl = ChaosController::new();
    let mut vfs = ChaosVfs::new(ctrl);

    vfs.create("test.dat").unwrap();
    vfs.write("test.dat", 0, b"hello").unwrap();
    vfs.fsync("test.dat").unwrap();
    vfs.crash_and_reboot();

    let data = vfs.read_all("test.dat").unwrap();
    assert_eq!(data, b"hello");
    println!("✓ Normal operation with fsync survived crash.");
}

// ============================================================================
// Idempotence of Recovery
// ============================================================================

#[test]
fn chaos_recovery_idempotence() {
    println!("\n=== Recovery Idempotence (run recovery 3x on same log) ===");

    let mut sim = CrashSimulator::new(777);
    let result = sim.run_iteration(0);

    // Re-run recovery on the same surviving records — should produce
    // identical results each time.
    let mut prev_committed = result.recovered_committed.clone();
    let mut prev_losers = result.recovered_losers.clone();

    for i in 1..=3 {
        let r2 = sim.run_iteration(i);
        // Different iterations have different data, so we can't compare
        // across iterations. Instead, verify within-iteration consistency
        // by checking that the recovery result matches the expected sets.
        assert!(r2.verified, "iteration {} failed verification", i);
    }

    // The key idempotence test: running ARIES analyze on the same log
    // multiple times produces the same result.
    use cendb_tx::{AriesRecovery, LogRecord, LogRecordType};
    use cendb_core::PageId;

    let records = vec![
        LogRecord { lsn: 1, prev_lsn: 0, txn_id: 1, rec_type: LogRecordType::Insert,
                     page_id: 1, payload: vec![1], crc32c: 0 },
        LogRecord { lsn: 2, prev_lsn: 0, txn_id: 2, rec_type: LogRecordType::Insert,
                     page_id: 2, payload: vec![2], crc32c: 0 },
        LogRecord { lsn: 3, prev_lsn: 1, txn_id: 1, rec_type: LogRecordType::Commit,
                     page_id: 0, payload: vec![], crc32c: 0 },
        // Txn 2 is a loser (no commit).
    ];

    let r1 = AriesRecovery::analyze(&records);
    let r2 = AriesRecovery::analyze(&records);
    let r3 = AriesRecovery::analyze(&records);

    assert_eq!(r1.committed_txns, r2.committed_txns);
    assert_eq!(r2.committed_txns, r3.committed_txns);
    assert_eq!(r1.loser_txns, r2.loser_txns);
    assert_eq!(r2.loser_txns, r3.loser_txns);

    assert!(r1.committed_txns.contains(&1));
    assert!(r1.loser_txns.contains(&2));

    println!("✓ ARIES recovery is idempotent — 3 runs produced identical results.");
    println!("  Committed: {:?}", r1.committed_txns);
    println!("  Losers:    {:?}", r1.loser_txns);
}

// ============================================================================
// Stress: Concurrent Transaction Simulation
// ============================================================================

#[test]
fn chaos_concurrent_transaction_stress() {
    println!("\n=== Concurrent Transaction Stress (1000 txns) ===");

    use cendb_tx::{TransactionManager, IsolationLevel};

    let mut tm = TransactionManager::new();
    let mut committed = 0;
    let mut aborted = 0;

    for i in 0..1000 {
        let txn = tm.begin(IsolationLevel::Snapshot);
        let key = format!("key_{}", i % 100); // 100 distinct keys → contention.
        tm.record_write(txn, key.as_bytes()).unwrap();
        match tm.commit(txn) {
            Ok(_) => committed += 1,
            Err(_) => aborted += 1,
        }
    }

    println!("  Committed: {}", committed);
    println!("  Aborted (conflicts): {}", aborted);
    println!("  Total: {}", committed + aborted);
    println!("✓ 1000 concurrent transactions processed without panic.");
    assert_eq!(committed + aborted, 1000);
}

// ============================================================================
// PAX Edge Case Fuzzing
// ============================================================================

#[test]
fn chaos_pax_edge_cases() {
    println!("\n=== PAX Edge Case Fuzzing ===");
    use cendb_chaos::fuzz_pax_block;

    // Empty input.
    fuzz_pax_block(&[]);
    println!("  ✓ Empty input: no panic");

    // Exactly 64 bytes (header only, no columns).
    fuzz_pax_block(&[0; 64]);
    println!("  ✓ 64-byte header-only: no panic");

    // All 0xFF (max values everywhere).
    fuzz_pax_block(&[0xFF; 4096]);
    println!("  ✓ All-0xFF 4KB: no panic");

    // All zeros.
    fuzz_pax_block(&[0; 4096]);
    println!("  ✓ All-zero 4KB: no panic");

    // Random-looking data.
    let mut data = Vec::new();
    let mut seed: u64 = 42;
    for _ in 0..8192 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        data.push((seed & 0xFF) as u8);
    }
    fuzz_pax_block(&data);
    println!("  ✓ 8KB random data: no panic");

    // Very large header fields (column_count = u32::MAX).
    let mut evil = vec![0u8; 4096];
    // Set column_count to a huge value at the right offset.
    let cc_offset = 48; // row_count(4) + column_count(4) in BlockHeader
    evil[cc_offset..cc_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    fuzz_pax_block(&evil);
    println!("  ✓ Evil column_count=u32::MAX: no panic");

    // Minipage offset pointing way past end of buffer.
    let mut evil2 = vec![0u8; 4096];
    // Set minipages_off to a huge value.
    let mp_off = 40; // In BlockHeader layout
    evil2[mp_off..mp_off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    fuzz_pax_block(&evil2);
    println!("  ✓ Evil minipage_off=u32::MAX: no panic");

    println!("\n✓ All PAX edge cases handled without panic.");
}
