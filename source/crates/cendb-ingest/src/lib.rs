//! cendb-ingest: high-performance data ingestion.
//!
//! Features:
//!   * Zero-allocation CSV parser with automatic type inference.
//!   * Streaming ingestion into CenDB relational tables.
//!   * Virtual table support with promotion to materialized columns.

pub mod csv;
pub mod type_inference;
pub mod virtual_table;

pub use csv::{CsvParser, CsvRecord};
pub use type_inference::{infer_column_types, InferredType};
pub use virtual_table::{VirtualTable, VirtualColumn};
