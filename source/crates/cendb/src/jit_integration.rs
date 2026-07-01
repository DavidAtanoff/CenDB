//! JIT integration: conditionally route hot query paths through the
//! Cranelift-based JIT compiler.
//!
//! Wired into the facade crate to avoid a cyclic dependency between
//! cendb-executor and cendb-jit (cendb-jit depends on cendb-executor
//! for the SelectionVector type).

use cendb_executor::{
    filter_i64_eq, filter_i64_gt, filter_i64_lt, filter_i64_ge, filter_i64_le, filter_i64_ne,
    sum_i64, SelectionVector,
};

/// A filter+aggregate pipeline that can be JIT-compiled.
#[derive(Clone, Debug)]
pub struct FilterAggregatePipeline {
    pub column: Vec<i64>,
    pub op: FilterOp,
    pub value: i64,
    pub aggregate: bool,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FilterOp { Eq, Ne, Gt, Ge, Lt, Le }

impl FilterOp {
    fn to_jit_op(self) -> cendb_jit::JitOp {
        match self {
            FilterOp::Eq => cendb_jit::JitOp::Eq,
            FilterOp::Ne => cendb_jit::JitOp::Ne,
            FilterOp::Gt => cendb_jit::JitOp::Gt,
            FilterOp::Ge => cendb_jit::JitOp::Ge,
            FilterOp::Lt => cendb_jit::JitOp::Lt,
            FilterOp::Le => cendb_jit::JitOp::Le,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PipelineResult {
    pub selection: SelectionVector,
    pub sum: Option<i64>,
    pub used_jit: bool,
    pub jit_reason: String,
}

/// Execute a filter+aggregate pipeline, using JIT if beneficial.
pub fn execute_jit_pipeline(pipeline: &FilterAggregatePipeline) -> PipelineResult {
    let estimated_rows = pipeline.column.len() as u64;
    let decision = cendb_jit::should_jit(estimated_rows, 1, pipeline.aggregate, 0);

    if decision.should_jit {
        let jit_filter = cendb_jit::JitFilter::new(pipeline.op.to_jit_op(), pipeline.value);
        let selection = jit_filter.execute(&pipeline.column);
        let sum = if pipeline.aggregate {
            // Sum the filtered values (those that passed the filter).
            let filtered: Vec<i64> = selection.gather_i64(&pipeline.column);
            Some(sum_i64(&filtered))
        } else { None };
        PipelineResult { selection, sum, used_jit: true, jit_reason: decision.reason }
    } else {
        let selection = match pipeline.op {
            FilterOp::Eq => filter_i64_eq(&pipeline.column, pipeline.value),
            FilterOp::Ne => filter_i64_ne(&pipeline.column, pipeline.value),
            FilterOp::Gt => filter_i64_gt(&pipeline.column, pipeline.value),
            FilterOp::Ge => filter_i64_ge(&pipeline.column, pipeline.value),
            FilterOp::Lt => filter_i64_lt(&pipeline.column, pipeline.value),
            FilterOp::Le => filter_i64_le(&pipeline.column, pipeline.value),
        };
        let sum = if pipeline.aggregate {
            Some(sum_i64(&selection.gather_i64(&pipeline.column)))
        } else { None };
        PipelineResult { selection, sum, used_jit: false, jit_reason: decision.reason }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_pipeline_large_scan_with_aggregation() {
        // JIT triggers when: rows × per_row_speedup > compile_cost
        // 50K rows × (5ns filter + 3.5ns agg) = 425µs savings > 200µs compile → JIT
        let col: Vec<i64> = (0..50_000).collect();
        let pipeline = FilterAggregatePipeline {
            column: col, op: FilterOp::Gt, value: 25_000, aggregate: true,
        };
        let result = execute_jit_pipeline(&pipeline);
        assert!(result.used_jit, "expected JIT: {}", result.jit_reason);
        assert_eq!(result.selection.len(), 24_999);
    }

    #[test]
    fn interpreted_pipeline_small() {
        let col: Vec<i64> = (0..50).collect();
        let pipeline = FilterAggregatePipeline {
            column: col, op: FilterOp::Gt, value: 25, aggregate: false,
        };
        let result = execute_jit_pipeline(&pipeline);
        assert!(!result.used_jit);
        assert_eq!(result.selection.len(), 24);
    }

    #[test]
    fn jit_pipeline_aggregation() {
        // 50K rows with aggregation: savings = 50K × (5+3.5)ns = 425µs > 200µs → JIT
        let col: Vec<i64> = (0..50_000).collect();
        let pipeline = FilterAggregatePipeline {
            column: col, op: FilterOp::Gt, value: 25_000, aggregate: true,
        };
        let result = execute_jit_pipeline(&pipeline);
        assert!(result.used_jit, "expected JIT: {}", result.jit_reason);
        assert!(result.sum.is_some());
        let expected: i64 = (25001..=49999).sum();
        assert_eq!(result.sum.unwrap(), expected);
    }
}
