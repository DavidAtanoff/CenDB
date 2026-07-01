//! cendb-optimizer: cost-based query optimizer.
//!
//! ## Overview
//!
//! This crate implements a cost-based optimizer (CBO) inspired by
//! PostgreSQL's approach. It uses table statistics to choose between
//! different join methods and orderings, and applies rule-based
//! transformations (predicate pushdown, projection pushdown).
//!
//! ## Architecture
//!
//! ```text
//! CenQL AST
//!    │
//!    ▼
//! Logical Plan (relational algebra)
//!    │
//!    ▼
//! Rule-based rewrites (predicate pushdown, projection pushdown)
//!    │
//!    ▼
//! Cost-based optimization (join reordering, join method selection)
//!    │
//!    ▼
//! Physical Plan (with concrete operators)
//! ```
//!
//! ## Cost model
//!
//! The cost model estimates the I/O and CPU cost of each operator:
//!
//!   * **Sequential scan**: `cost = rows * row_width / page_size`
//!   * **Index lookup**: `cost = log(rows) * page_fetch_cost`
//!   * **Nested loop join**: `cost = outer_rows * inner_lookup_cost`
//!   * **Hash join**: `cost = outer_rows + inner_rows + hash_build_cost`
//!   * **Merge join**: `cost = outer_rows + inner_rows + sort_cost`
//!
//! The optimizer picks the plan with the lowest estimated cost.

pub mod cost;
pub mod plan;
pub mod stats;

pub use cost::{
    filter_cost, hash_join_cost, index_lookup_cost, merge_join_cost, nested_loop_join_cost,
    seq_scan_cost, sort_cost, Cost, CostModel,
};
pub use plan::{
    cenql_to_logical, choose_join_method_spec, choose_scan_op, explain, explain_analyze,
    is_equi_join, physical_plan, push_down_filters, JoinMethod, LogicalPlan, Optimizer,
    PhysicalOperator, PhysicalPlan, PlanNode,
};
pub use stats::{ColumnStats, Histogram, HyperLogLog, StatsCatalog, TableStats};
