//! cendb-export: high-performance data exporters.
//!
//!   * **JSON** — export rows as JSON arrays or NDJSON (newline-delimited).
//!   * **CSV** — export rows as comma-separated values.
//!   * **Arrow-compatible** — export columnar data in Arrow IPC format
//!     (simplified; produces Arrow-compatible flat arrays without the
//!     full Arrow Rust SDK dependency).

pub mod json;
pub mod csv_export;
pub mod arrow;

pub use json::{export_json, export_ndjson};
pub use csv_export::export_csv;
pub use arrow::{ArrowColumn, ArrowBatch, export_arrow_ipc};
