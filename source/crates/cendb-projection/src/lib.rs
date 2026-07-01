//! cendb-projection: the multi-model projection layer.
//!
//! This crate maps the six logical data models from the spec onto the unified
//! PAX storage substrate. Each projection is a thin layer that:
//!
//!   * Defines its own column schema (`Vec<ColumnSpec>`).
//!   * Provides typed write/append methods that build PAX blocks.
//!   * Provides typed read/scan methods that consume PAX blocks.
//!
//! The five projections implemented here cover the spec's mandate:
//!
//!   * [`kv`] — Key-Value fast path (point lookup).
//!   * [`relational`] — schema-bound tuples.
//!   * [`document`] — HexDoc binary JSON layout with field offset table.
//!   * [`timeseries`] — time-partitioned blocks with zone-map skipping.
//!   * [`graph`] — CSR (Compressed Sparse Row) overlay for O(1) adjacency.

pub mod document;
pub mod graph;
pub mod kv;
pub mod relational;
pub mod timeseries;

// Re-export the most useful types.
pub use document::{DocValue, HexDoc, HexDocBuilder};
pub use graph::{CsrOverlay, GraphProjection};
pub use kv::{KvProjection, KvStore};
pub use relational::{RelationalProjection, RelationalTable};
pub use timeseries::{TimeSeriesProjection, TimeSeriesSchema, TimeSeriesStore};
