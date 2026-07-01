//! Cost model for the optimizer.

/// A cost value — higher means more expensive.
pub type Cost = f64;

/// The cost model: maps physical operator properties to estimated costs.
/// These constants are inspired by PostgreSQL's default cost parameters.
pub struct CostModel;

impl CostModel {
    /// Cost of a sequential page fetch (default 1.0).
    pub const SEQ_PAGE_COST: f64 = 1.0;
    /// Cost of a random page fetch (default 4.0).
    pub const RAND_PAGE_COST: f64 = 4.0;
    /// Cost of processing one row by the CPU (default 0.01).
    pub const CPU_TUPLE_COST: f64 = 0.01;
    /// Cost of evaluating an operator on one row (default 0.0025).
    pub const CPU_OPERATOR_COST: f64 = 0.0025;
    /// Cost of processing one row during a hash build (default 0.01).
    pub const CPU_HASH_BUILD_COST: f64 = 0.01;
    /// Cost of probing the hash table for one row (default 0.005).
    pub const CPU_HASH_PROBE_COST: f64 = 0.005;
    /// Cost of sorting one row (default 0.01).
    pub const CPU_SORT_COST: f64 = 0.01;

    /// Estimate the cost of a sequential scan over `rows` rows with the
    /// given average row width.
    pub fn seq_scan(rows: u64, avg_row_width: u32) -> Cost {
        let pages = (rows as f64 * avg_row_width as f64) / 4096.0;
        pages * Self::SEQ_PAGE_COST + rows as f64 * Self::CPU_TUPLE_COST
    }

    /// Estimate the cost of an index lookup returning `rows` rows.
    pub fn index_scan(rows: u64) -> Cost {
        rows as f64 * Self::RAND_PAGE_COST + rows as f64 * Self::CPU_TUPLE_COST
    }

    /// Estimate the cost of a nested loop join.
    pub fn nested_loop_join(
        outer_rows: u64,
        inner_cost_per_row: Cost,
        inner_rows: u64,
    ) -> Cost {
        outer_rows as f64 * inner_cost_per_row
            + outer_rows as f64 * inner_rows as f64 * Self::CPU_OPERATOR_COST
    }

    /// Estimate the cost of a hash join.
    pub fn hash_join(
        build_rows: u64,
        probe_rows: u64,
    ) -> Cost {
        build_rows as f64 * Self::CPU_HASH_BUILD_COST
            + probe_rows as f64 * Self::CPU_HASH_PROBE_COST
            + (build_rows + probe_rows) as f64 * Self::CPU_TUPLE_COST
    }

    /// Estimate the cost of a merge join (assumes sorted inputs).
    pub fn merge_join(
        left_rows: u64,
        right_rows: u64,
    ) -> Cost {
        (left_rows + right_rows) as f64 * Self::CPU_TUPLE_COST
            + (left_rows + right_rows) as f64 * Self::CPU_OPERATOR_COST
    }

    /// Estimate the cost of sorting `rows` rows.
    pub fn sort(rows: u64) -> Cost {
        rows as f64 * Self::CPU_SORT_COST * (rows as f64).log2().max(1.0)
    }
}

// ============================================================================
// Spec-defined standalone cost functions.
//
// These implement the exact cost formulas required by the task spec. They
// are independent of the `CostModel` struct above (which follows the
// PostgreSQL-style cost model and is kept for backward compatibility with
// the existing `Optimizer`).
// ============================================================================

/// Sequential scan cost: `rows * cols * 0.01`.
pub fn seq_scan_cost(rows: usize, cols: usize) -> f64 {
    rows as f64 * cols as f64 * 0.01
}

/// Index lookup cost: constant 1.0.
pub fn index_lookup_cost() -> f64 {
    1.0
}

/// Hash join cost: `left_rows + right_rows`.
pub fn hash_join_cost(left_rows: usize, right_rows: usize) -> f64 {
    left_rows as f64 + right_rows as f64
}

/// Nested loop join cost: `left_rows * right_rows * 0.01`.
pub fn nested_loop_join_cost(left_rows: usize, right_rows: usize) -> f64 {
    left_rows as f64 * right_rows as f64 * 0.01
}

/// Merge join cost (assumes sorted inputs): `left_rows + right_rows`.
pub fn merge_join_cost(left_rows: usize, right_rows: usize) -> f64 {
    left_rows as f64 + right_rows as f64
}

/// Filter cost: `rows * 0.001`.
pub fn filter_cost(rows: usize) -> f64 {
    rows as f64 * 0.001
}

/// Sort cost: `rows * log2(rows) * 0.01`. Returns 0 for 0 rows.
pub fn sort_cost(rows: usize) -> f64 {
    if rows == 0 {
        return 0.0;
    }
    rows as f64 * (rows as f64).log2() * 0.01
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_scan_cost_scales_with_rows() {
        let small = CostModel::seq_scan(100, 64);
        let large = CostModel::seq_scan(100_000, 64);
        assert!(large > small * 100.0);
    }

    #[test]
    fn hash_join_cheaper_than_nested_loop_for_large_inputs() {
        let outer = 100_000u64;
        let inner = 100_000u64;
        let inner_cost_per_row = CostModel::seq_scan(inner, 64) / inner as f64;
        let nl_cost = CostModel::nested_loop_join(outer, inner_cost_per_row, inner);
        let hash_cost = CostModel::hash_join(inner, outer);
        // Hash join should be much cheaper for large equi-joins.
        assert!(
            hash_cost < nl_cost,
            "hash join ({}) should be cheaper than nested loop ({})",
            hash_cost,
            nl_cost
        );
    }
}

#[cfg(test)]
mod spec_cost_tests {
    use super::*;

    #[test]
    fn seq_scan_cost_formula() {
        // rows * cols * 0.01
        assert_eq!(seq_scan_cost(1000, 5), 50.0);
        assert_eq!(seq_scan_cost(0, 5), 0.0);
        assert_eq!(seq_scan_cost(10, 10), 1.0);
    }

    #[test]
    fn index_lookup_cost_is_constant() {
        assert_eq!(index_lookup_cost(), 1.0);
        assert_eq!(index_lookup_cost(), 1.0);
    }

    #[test]
    fn hash_join_cost_formula() {
        // left_rows + right_rows
        assert_eq!(hash_join_cost(100, 200), 300.0);
        assert_eq!(hash_join_cost(0, 0), 0.0);
    }

    #[test]
    fn nested_loop_join_cost_formula() {
        // left_rows * right_rows * 0.01
        assert_eq!(nested_loop_join_cost(100, 200), 200.0);
        assert_eq!(nested_loop_join_cost(0, 100), 0.0);
        assert_eq!(nested_loop_join_cost(1000, 1000), 10_000.0);
    }

    #[test]
    fn merge_join_cost_formula() {
        // left_rows + right_rows
        assert_eq!(merge_join_cost(100, 200), 300.0);
        assert_eq!(merge_join_cost(0, 0), 0.0);
    }

    #[test]
    fn filter_cost_formula() {
        // rows * 0.001
        assert_eq!(filter_cost(1000), 1.0);
        assert_eq!(filter_cost(0), 0.0);
        assert_eq!(filter_cost(5000), 5.0);
    }

    #[test]
    fn sort_cost_formula() {
        // rows * log2(rows) * 0.01
        assert_eq!(sort_cost(0), 0.0);
        // 1024 * log2(1024) * 0.01 = 1024 * 10 * 0.01 = 102.4
        assert!((sort_cost(1024) - 102.4).abs() < 1e-9);
        // 100 * log2(100) * 0.01 ≈ 100 * 6.643856 * 0.01 ≈ 6.643856
        assert!((sort_cost(100) - 100.0f64.log2() * 1.0).abs() < 1e-9);
    }

    #[test]
    fn hash_cheaper_than_nested_loop_for_large() {
        // For large inputs, hash join (n+m) << nested loop (n*m*0.01).
        let n = 10_000;
        let m = 10_000;
        assert!(hash_join_cost(n, m) < nested_loop_join_cost(n, m));
    }

    #[test]
    fn merge_join_same_cost_as_hash() {
        // Merge and hash have the same formula (n+m); the difference is
        // that merge requires sorted inputs.
        assert_eq!(merge_join_cost(1000, 2000), hash_join_cost(1000, 2000));
    }
}
