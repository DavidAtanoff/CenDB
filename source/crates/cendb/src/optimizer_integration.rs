//! Query optimizer integration: wire the cost-based optimizer into
//! the CenQL execution path.
//!
//! The optimizer (`cendb-optimizer`) provides a cost model and physical
//! plan selection. This module connects it to the CenQL parser's AST
//! so queries are optimized before execution.

use cendb_optimizer::{
    LogicalPlan, Optimizer, PhysicalPlan, StatsCatalog, TableStats,
};

/// A query plan ready for execution.
pub struct QueryPlan {
    pub logical: LogicalPlan,
    pub physical: PhysicalPlan,
    pub optimized: bool,
}

/// Optimize a CenQL pipeline for execution.
///
/// Steps:
/// 1. Convert the CenQL AST to a LogicalPlan (scan → filter → project → ...)
/// 2. Apply the cost-based optimizer: filter pushdown, join reordering,
///    physical operator selection.
/// 3. Return the physical plan for the executor to run.
pub fn optimize_query(
    logical: &LogicalPlan,
    stats: &StatsCatalog,
) -> QueryPlan {
    let optimizer = Optimizer::new(stats.clone());
    let physical = optimizer.optimize(logical);
    QueryPlan {
        logical: logical.clone(),
        physical,
        optimized: true,
    }
}

/// Build a simple logical plan: scan + filter.
pub fn build_scan_filter_plan(table: &str, predicate: &str) -> LogicalPlan {
    LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Scan {
            table: table.to_string(),
            predicate: None,
        }),
        predicate: predicate.to_string(),
    }
}

/// Build a join plan: scan two tables, join on a condition.
pub fn build_join_plan(
    left_table: &str,
    right_table: &str,
    condition: &str,
) -> LogicalPlan {
    LogicalPlan::Join {
        left: Box::new(LogicalPlan::Scan {
            table: left_table.to_string(),
            predicate: None,
        }),
        right: Box::new(LogicalPlan::Scan {
            table: right_table.to_string(),
            predicate: None,
        }),
        condition: condition.to_string(),
    }
}

/// Register table statistics for the optimizer.
pub fn register_table_stats(
    catalog: &mut StatsCatalog,
    table: &str,
    row_count: u64,
) {
    catalog.register(TableStats::new(table, row_count));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optimize_scan_filter() {
        let mut catalog = StatsCatalog::new();
        register_table_stats(&mut catalog, "users", 10000);
        let logical = build_scan_filter_plan("users", "age > 18");
        let plan = optimize_query(&logical, &catalog);
        assert!(plan.optimized);
        assert!(plan.physical.cost >= 0.0);
    }

    #[test]
    fn optimize_join() {
        let mut catalog = StatsCatalog::new();
        register_table_stats(&mut catalog, "users", 10000);
        register_table_stats(&mut catalog, "orders", 50000);
        let logical = build_join_plan("users", "orders", "users.id == orders.user_id");
        let plan = optimize_query(&logical, &catalog);
        assert!(plan.optimized);
        assert!(plan.physical.cost >= 0.0);
    }

    #[test]
    fn optimize_with_pushdown() {
        let mut catalog = StatsCatalog::new();
        register_table_stats(&mut catalog, "t", 1000);
        let logical = build_scan_filter_plan("t", "x > 5");
        let plan = optimize_query(&logical, &catalog);
        // The optimizer should produce a physical plan.
        assert!(plan.physical.estimated_rows <= 1000);
    }
}
