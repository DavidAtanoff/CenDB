//! cendb-system: system tables, information schema, and hot backup.
//!
//! ## System Tables
//!
//! Exposes internal engine state as queryable virtual tables:
//!   * `__tables` — schema catalog (table name, columns, model).
//!   * `__transactions` — active transactions (txn_id, read_ts, state).
//!   * `__buffer_pool` — frame allocation, dirty pages, hit/miss stats.
//!   * `__indexes` — registered indexes (name, type, column, entries).
//!
//! ## Hot Backups
//!
//! Provides a consistent snapshot API that can be taken while the database
//! is active. Uses the WAL's checkpoint mechanism to ensure consistency:
//!   1. Flush all dirty pages (checkpoint).
//!   2. Copy segment files to the backup destination.
//!   3. Copy the WAL up to the checkpoint LSN.
//! The backup is consistent as of the checkpoint timestamp.

pub mod catalog;
pub mod backup;

pub use catalog::{SystemCatalog, TableInfo, ColumnInfo, IndexInfo};
pub use backup::{HotBackup, BackupResult};
