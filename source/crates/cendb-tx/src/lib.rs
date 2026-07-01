//! cendb-tx: MVCC transactions + WAL + ARIES-lite recovery.
//!
//! ## Design
//!
//! The transaction layer implements §2 of the spec:
//!
//!   * **MVCC** for reader/writer isolation (readers never block writers).
//!   * **Optimistic multi-writer** transactions validated at commit.
//!   * **Segment-partitioned writes** so independent writers touching
//!     different segments proceed lock-free; only the WAL append and the
//!     commit-timestamp oracle are shared.
//!
//! ## Concurrency
//!
//! The timestamp oracle is a single `AtomicU64`; `fetch_add` is wait-free.
//! Validation reads are lock-free via atomic loads on version headers.
//! The common (uncontended) path is lock-free.
//!
//! ## Crash recovery
//!
//! We use **WAL, not shadow paging.** The recovery protocol is three-phase
//! ARIES:
//!
//!   1. **ANALYSIS**: scan WAL from last checkpoint → rebuild Dirty Page
//!      Table + active txn table.
//!   2. **REDO**: replay all records with `lsn > page.page_lsn` (idempotent
//!      via LSN check).
//!   3. **UNDO**: roll back losers using `prev_lsn` chains, writing CLRs
//!      (compensation log records) so undo is itself crash-safe.
//!
//! For the prototype we implement a single-threaded, in-memory WAL with
//! synchronous fsync-on-commit; production would add group commit and
//! `io_uring` batching.

pub mod mvcc;
pub mod wal;

pub use mvcc::{
    IsolationLevel, MvccError, MvccResult, TimestampOracle, Transaction, TransactionManager,
    TransactionState, VersionHeader,
};
pub use wal::{
    AriesRecovery, LogRecord, LogRecordType, WalConfig, WalError, WalResult, WriteAheadLog,
};
