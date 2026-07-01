//! cendb-chaos: fault injection, crash recovery simulation, and fuzzing.
//!
//! This crate provides the testing infrastructure to brutally battle-test
//! CenDB's invariants under chaotic conditions:
//!
//!   * **ChaosVfs** — an in-memory virtual file system that intercepts
//!     every I/O operation and can inject failures (EIO, torn writes,
//!     silent fsync failures, data corruption) at precise operation
//!     indices.
//!   * **ChaosController** — configures which faults to inject and when.
//!   * **CrashSimulator** — generates random transaction workloads,
//!     simulates a crash by truncating the WAL at a random point, runs
//!     ARIES recovery, and verifies that committed data survives and
//!     uncommitted data is rolled back.
//!   * **Fuzz helpers** — generate random byte arrays and strings to
//!     fuzz the PAX decoder and CenQL parser.

pub mod controller;
pub mod crash_simulator;
pub mod fuzz;
pub mod vfs;

pub use controller::{ChaosController, FaultConfig, FaultType};
pub use crash_simulator::{CrashPoint, CrashSimulator, RecoveryResult};
pub use fuzz::{
    fuzz_cenql_parser, fuzz_cenql_parser_aggressive, fuzz_encoding_decoders, fuzz_pax_block,
    fuzz_pax_block_aggressive,
};
pub use vfs::{ChaosVfs, VfsError, VfsResult};
