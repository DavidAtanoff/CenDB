//! cendb-executor: vectorized execution engine.
//!
//! ## Overview
//!
//! This crate implements a morsel-driven, push-based vectorized execution
//! engine inspired by ClickHouse and DuckDB. Data is processed in
//! cache-resident batches ("morsels") of 1024 rows, enabling SIMD
//! acceleration and amortizing per-tuple overhead.
//!
//! ## Architecture
//!
//! ```text
//! Producer (scan)
//!   reads 1024-row morsels from PAX blocks
//!   pushes them downstream
//!       |
//!       v
//! Filter (vectorized)
//!   evaluates predicate on the whole morsel
//!   using SIMD where possible
//!   produces a selection vector
//!       |
//!       v
//! Project / Aggregate
//!   transforms or aggregates the morsel
//! ```
//!
//! ## Vectorized primitives
//!
//! The executor provides SIMD-accelerated primitives for common operations:
//!
//!   * `filter_i64_eq` — filter a column by equality with a constant.
//!   * `filter_i64_gt` — filter by greater-than.
//!   * `filter_i64_lt` — filter by less-than.
//!   * `sum_i64` — sum a column.
//!   * `sum_f64` — sum a column of floats.
//!
//! These operate on `&[i64]` / `&[f64]` slices and produce selection
//! vectors or scalar aggregates.

pub mod join;
pub mod morsel;
pub mod subquery;
pub mod vector;

pub use join::{auto_join, auto_select_method, hash_join, join, merge_join, nested_loop_join, JoinMethod};
pub use morsel::{Morsel, MorselBatch};
pub use subquery::{execute_pipeline, InMemoryProvider, TableProvider};
pub use vector::{
    filter_f64_gt, filter_f64_lt, filter_i64_eq, filter_i64_ge, filter_i64_gt, filter_i64_le,
    filter_i64_lt, filter_i64_ne, sum_f64, sum_i64, SelectionVector,
};
