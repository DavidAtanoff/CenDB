//! cendb-export: high-performance data exporters and format integrations.
//!
//!   * **JSON** — export rows as JSON arrays or NDJSON.
//!   * **CSV** — export rows as comma-separated values.
//!   * **Apache Arrow** — full Arrow IPC integration using the official
//!     `arrow` crate. Zero-copy `RecordBatch` conversion.
//!   * **Parquet** — full Parquet read/write using the official `parquet`
//!     crate. Supports Snappy, Zstd, Gzip, LZ4, Brotli compression.

pub mod json;
pub mod csv_export;
pub mod arrow;
pub mod parquet;

pub use json::{export_json, export_ndjson};
pub use csv_export::export_csv;
pub use arrow::{ArrowColumn, ArrowBatch, ArrowTable, ColumnData, export_arrow_ipc};
pub use parquet::{
    write_parquet, read_parquet, write_parquet_file, read_parquet_file,
    parquet_metadata, ParquetWriteOptions, ParquetCompression, ParquetMetadata,
};
