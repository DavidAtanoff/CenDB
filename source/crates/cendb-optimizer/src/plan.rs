//! Logical and physical plan types for the optimizer.

use crate::stats::StatsCatalog;

// ============================================================================
// Logical plan (relational algebra).
// ============================================================================

/// A logical plan node — describes *what* to compute, not *how*.
#[derive(Clone, Debug)]
pub enum LogicalPlan {
    /// Scan a table.
    Scan {
        table: String,
        /// Optional predicate pushed down from a parent Filter.
        predicate: Option<String>,
    },
    /// Filter rows by a predicate.
    Filter {
        input: Box<LogicalPlan>,
        predicate: String,
    },
    /// Project to a subset of columns.
    Project {
        input: Box<LogicalPlan>,
        columns: Vec<String>,
    },
    /// Join two inputs.
    Join {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        /// Join condition (e.g. "left.id == right.id").
        condition: String,
    },
    /// Aggregate by a key.
    Aggregate {
        input: Box<LogicalPlan>,
        group_key: String,
        aggs: Vec<String>,
    },
    /// Limit to N rows.
    Limit {
        input: Box<LogicalPlan>,
        count: u64,
    },
}

impl LogicalPlan {
    /// Estimate the output cardinality of this plan using the given
    /// statistics catalog.
    pub fn estimate_rows(&self, catalog: &StatsCatalog) -> u64 {
        match self {
            LogicalPlan::Scan { table, predicate } => {
                let base = catalog.get(table).map(|s| s.row_count).unwrap_or(1000);
                if predicate.is_some() {
                    // Default selectivity for an unknown predicate: 0.1.
                    (base as f64 * 0.1) as u64
                } else {
                    base
                }
            }
            LogicalPlan::Filter { input, .. } => {
                let base = input.estimate_rows(catalog);
                (base as f64 * 0.1) as u64
            }
            LogicalPlan::Project { input, .. } => input.estimate_rows(catalog),
            LogicalPlan::Join { left, right, .. } => {
                let left_rows = left.estimate_rows(catalog);
                let right_rows = right.estimate_rows(catalog);
                // Without column stats, default to a cross-product with
                // a 0.01 selectivity factor.
                ((left_rows * right_rows) as f64 * 0.01) as u64
            }
            LogicalPlan::Aggregate { input, group_key, .. } => {
                let base = input.estimate_rows(catalog);
                // Estimate: number of groups ≈ sqrt(rows).
                let _ = group_key;
                (base as f64).sqrt() as u64
            }
            LogicalPlan::Limit { input, count } => {
                input.estimate_rows(catalog).min(*count)
            }
        }
    }
}

// ============================================================================
// Physical plan.
// ============================================================================

/// Method for executing a join.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum JoinMethod {
    /// Nested loop: for each row in the outer, scan the inner.
    /// Best for small outer or when inner has an index.
    NestedLoop,
    /// Hash join: build a hash table on the smaller side, probe with the
    /// larger. Best for equi-joins on large inputs.
    Hash,
    /// Merge join: both inputs sorted on the join key, merge.
    /// Best when both inputs are already sorted.
    Merge,
}

impl JoinMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            JoinMethod::NestedLoop => "NestedLoop",
            JoinMethod::Hash => "HashJoin",
            JoinMethod::Merge => "MergeJoin",
        }
    }
}

/// A physical operator — describes *how* to execute a step.
#[derive(Clone, Debug)]
pub enum PhysicalOperator {
    SeqScan { table: String, predicate: Option<String> },
    IndexScan { table: String, index: String, key: String },
    Filter { input: Box<PhysicalPlan>, predicate: String },
    Project { input: Box<PhysicalPlan>, columns: Vec<String> },
    Join {
        method: JoinMethod,
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        condition: String,
    },
    Aggregate { input: Box<PhysicalPlan>, group_key: String, aggs: Vec<String> },
    Limit { input: Box<PhysicalPlan>, count: u64 },
}

/// A physical plan node with an estimated cost.
#[derive(Clone, Debug)]
pub struct PhysicalPlan {
    pub operator: PhysicalOperator,
    pub cost: f64,
    pub estimated_rows: u64,
}

impl PhysicalPlan {
    pub fn new(operator: PhysicalOperator, cost: f64, estimated_rows: u64) -> Self {
        Self {
            operator,
            cost,
            estimated_rows,
        }
    }
}

/// A complete physical plan tree.
pub type PlanNode = PhysicalPlan;

// ============================================================================
// Plan builder / optimizer.
// ============================================================================

/// The optimizer: converts a logical plan into a physical plan using
/// cost-based decisions.
pub struct Optimizer {
    catalog: StatsCatalog,
}

impl Optimizer {
    pub fn new(catalog: StatsCatalog) -> Self {
        Self { catalog }
    }

    /// Optimize a logical plan into a physical plan.
    pub fn optimize(&self, logical: &LogicalPlan) -> PhysicalPlan {
        self.optimize_node(logical)
    }

    fn optimize_node(&self, logical: &LogicalPlan) -> PhysicalPlan {
        match logical {
            LogicalPlan::Scan { table, predicate } => {
                let rows = logical.estimate_rows(&self.catalog);
                let cost = self.scan_cost(table, rows);
                PhysicalPlan::new(
                    PhysicalOperator::SeqScan {
                        table: table.clone(),
                        predicate: predicate.clone(),
                    },
                    cost,
                    rows,
                )
            }
            LogicalPlan::Filter { input, predicate } => {
                let child = self.optimize_node(input);
                let rows = (child.estimated_rows as f64 * 0.1) as u64;
                let cost = child.cost + rows as f64 * 0.01;
                PhysicalPlan::new(
                    PhysicalOperator::Filter {
                        input: Box::new(child),
                        predicate: predicate.clone(),
                    },
                    cost,
                    rows,
                )
            }
            LogicalPlan::Project { input, columns } => {
                let child = self.optimize_node(input);
                let rows = child.estimated_rows;
                let cost = child.cost + rows as f64 * 0.005;
                PhysicalPlan::new(
                    PhysicalOperator::Project {
                        input: Box::new(child),
                        columns: columns.clone(),
                    },
                    cost,
                    rows,
                )
            }
            LogicalPlan::Join { left, right, condition } => {
                let left_plan = self.optimize_node(left);
                let right_plan = self.optimize_node(right);
                let method = self.choose_join_method(&left_plan, &right_plan);
                let rows = self.estimate_join_rows(&left_plan, &right_plan);
                let cost = self.join_cost(method, &left_plan, &right_plan, rows);
                PhysicalPlan::new(
                    PhysicalOperator::Join {
                        method,
                        left: Box::new(left_plan),
                        right: Box::new(right_plan),
                        condition: condition.clone(),
                    },
                    cost,
                    rows,
                )
            }
            LogicalPlan::Aggregate { input, group_key, aggs } => {
                let child = self.optimize_node(input);
                let rows = (child.estimated_rows as f64).sqrt() as u64;
                let cost = child.cost + child.estimated_rows as f64 * 0.02;
                PhysicalPlan::new(
                    PhysicalOperator::Aggregate {
                        input: Box::new(child),
                        group_key: group_key.clone(),
                        aggs: aggs.clone(),
                    },
                    cost,
                    rows,
                )
            }
            LogicalPlan::Limit { input, count } => {
                let child = self.optimize_node(input);
                let rows = child.estimated_rows.min(*count);
                let cost = child.cost;
                PhysicalPlan::new(
                    PhysicalOperator::Limit {
                        input: Box::new(child),
                        count: *count,
                    },
                    cost,
                    rows,
                )
            }
        }
    }

    /// Choose the best join method based on the estimated cardinalities.
    fn choose_join_method(&self, left: &PhysicalPlan, right: &PhysicalPlan) -> JoinMethod {
        let left_rows = left.estimated_rows;
        let right_rows = right.estimated_rows;

        // Rule 1: If one side is very small (< 100 rows), use nested loop.
        if left_rows < 100 || right_rows < 100 {
            return JoinMethod::NestedLoop;
        }

        // Rule 2: If both sides are large, use hash join.
        if left_rows > 10_000 && right_rows > 10_000 {
            return JoinMethod::Hash;
        }

        // Rule 3: For medium-sized inputs, use hash join (it's generally
        // the best default for equi-joins).
        JoinMethod::Hash
    }

    /// Estimate the output cardinality of a join.
    fn estimate_join_rows(&self, left: &PhysicalPlan, right: &PhysicalPlan) -> u64 {
        // Without detailed column stats, use a default selectivity of 0.01.
        ((left.estimated_rows * right.estimated_rows) as f64 * 0.01) as u64
    }

    // ========================================================================
    // Cost functions.
    // ========================================================================

    fn scan_cost(&self, table: &str, rows: u64) -> f64 {
        let stats = self.catalog.get(table);
        let row_width = stats.map(|s| s.avg_row_width).unwrap_or(64) as f64;
        let page_size = 4096.0;
        // Cost = number of pages * page_fetch_cost.
        let pages = (rows as f64 * row_width) / page_size;
        pages * 1.0 // seq_page_cost = 1.0
    }

    fn join_cost(
        &self,
        method: JoinMethod,
        left: &PhysicalPlan,
        right: &PhysicalPlan,
        output_rows: u64,
    ) -> f64 {
        let left_cost = left.cost;
        let right_cost = right.cost;
        match method {
            JoinMethod::NestedLoop => {
                // Cost = left_cost + left_rows * right_cost_per_row.
                left_cost + (left.estimated_rows as f64) * (right_cost / right.estimated_rows.max(1) as f64)
            }
            JoinMethod::Hash => {
                // Cost = left_cost + right_cost + build_cost + probe_cost.
                let build_cost = (left.estimated_rows.min(right.estimated_rows)) as f64 * 0.01;
                let probe_cost = (left.estimated_rows.max(right.estimated_rows)) as f64 * 0.005;
                left_cost + right_cost + build_cost + probe_cost
            }
            JoinMethod::Merge => {
                // Cost = left_cost + right_cost + sort_cost + merge_cost.
                let sort_cost = (left.estimated_rows + right.estimated_rows) as f64 * 0.01;
                let merge_cost = output_rows as f64 * 0.005;
                left_cost + right_cost + sort_cost + merge_cost
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::{ColumnStats, TableStats};

    fn make_catalog() -> StatsCatalog {
        let mut catalog = StatsCatalog::new();
        catalog.register(
            TableStats::new("users", 10_000)
                .with_column("id", ColumnStats {
                    distinct_count: 10_000,
                    null_count: 0,
                    min_i64: Some(1),
                    max_i64: Some(10_000),
                    most_common: vec![],
                }),
        );
        catalog.register(
            TableStats::new("orders", 100_000)
                .with_column("user_id", ColumnStats {
                    distinct_count: 10_000,
                    null_count: 0,
                    min_i64: Some(1),
                    max_i64: Some(10_000),
                    most_common: vec![],
                }),
        );
        catalog
    }

    #[test]
    fn optimizer_chooses_hash_join_for_large_inputs() {
        let catalog = make_catalog();
        let opt = Optimizer::new(catalog);
        let logical = LogicalPlan::Join {
            left: Box::new(LogicalPlan::Scan {
                table: "users".to_string(),
                predicate: None,
            }),
            right: Box::new(LogicalPlan::Scan {
                table: "orders".to_string(),
                predicate: None,
            }),
            condition: "users.id == orders.user_id".to_string(),
        };
        let physical = opt.optimize(&logical);
        if let PhysicalOperator::Join { method, .. } = &physical.operator {
            // Both inputs are large (> 10K), so hash join should be chosen.
            assert_eq!(*method, JoinMethod::Hash);
        } else {
            panic!("expected Join operator");
        }
    }

    #[test]
    fn optimizer_chooses_nested_loop_for_small_input() {
        let mut catalog = StatsCatalog::new();
        catalog.register(TableStats::new("small", 50));
        catalog.register(TableStats::new("large", 1_000_000));
        let opt = Optimizer::new(catalog);
        let logical = LogicalPlan::Join {
            left: Box::new(LogicalPlan::Scan {
                table: "small".to_string(),
                predicate: None,
            }),
            right: Box::new(LogicalPlan::Scan {
                table: "large".to_string(),
                predicate: None,
            }),
            condition: "small.id == large.small_id".to_string(),
        };
        let physical = opt.optimize(&logical);
        if let PhysicalOperator::Join { method, .. } = &physical.operator {
            assert_eq!(*method, JoinMethod::NestedLoop);
        } else {
            panic!("expected Join operator");
        }
    }

    #[test]
    fn optimizer_estimates_costs() {
        let catalog = make_catalog();
        let opt = Optimizer::new(catalog);
        let logical = LogicalPlan::Scan {
            table: "users".to_string(),
            predicate: None,
        };
        let physical = opt.optimize(&logical);
        assert!(physical.cost > 0.0);
        assert_eq!(physical.estimated_rows, 10_000);
    }

    #[test]
    fn limit_reduces_estimated_rows() {
        let catalog = make_catalog();
        let opt = Optimizer::new(catalog);
        let logical = LogicalPlan::Limit {
            input: Box::new(LogicalPlan::Scan {
                table: "users".to_string(),
                predicate: None,
            }),
            count: 100,
        };
        let physical = opt.optimize(&logical);
        assert_eq!(physical.estimated_rows, 100);
    }
}

// ============================================================================
// EXPLAIN and EXPLAIN ANALYZE
// ============================================================================

/// EXPLAIN output: the physical plan with cost estimates.
#[derive(Clone, Debug)]
pub struct ExplainOutput {
    pub plan_text: String,
    pub estimated_cost: f64,
    pub estimated_rows: u64,
}

/// EXPLAIN ANALYZE output: actual execution statistics.
#[derive(Clone, Debug)]
pub struct ExplainAnalyzeOutput {
    pub plan_text: String,
    pub estimated_cost: f64,
    pub estimated_rows: u64,
    pub actual_rows: u64,
    pub actual_time_us: u64,
    pub loops: u64,
}

/// Generate EXPLAIN output for a physical plan.
pub fn explain(plan: &PhysicalPlan) -> ExplainOutput {
    let mut text = String::new();
    explain_node(plan, 0, &mut text);
    ExplainOutput {
        plan_text: text,
        estimated_cost: plan.cost,
        estimated_rows: plan.estimated_rows,
    }
}

/// Generate EXPLAIN ANALYZE output (with actual execution stats).
pub fn explain_analyze(plan: &PhysicalPlan, actual_rows: u64, actual_time_us: u64) -> ExplainAnalyzeOutput {
    let mut text = String::new();
    explain_node(plan, 0, &mut text);
    ExplainAnalyzeOutput {
        plan_text: text,
        estimated_cost: plan.cost,
        estimated_rows: plan.estimated_rows,
        actual_rows,
        actual_time_us,
        loops: 1,
    }
}

fn explain_node(plan: &PhysicalPlan, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    match &plan.operator {
        PhysicalOperator::SeqScan { table, predicate } => {
            out.push_str(&format!("{}SeqScan on {}", indent, table));
            if let Some(p) = predicate {
                out.push_str(&format!(" (filter: {})", p));
            }
            out.push_str(&format!("  (cost={:.2} rows={})\n", plan.cost, plan.estimated_rows));
        }
        PhysicalOperator::IndexScan { table, index, .. } => {
            out.push_str(&format!("{}IndexScan on {} using {}  (cost={:.2} rows={})\n",
                indent, table, index, plan.cost, plan.estimated_rows));
        }
        PhysicalOperator::Filter { input, predicate } => {
            out.push_str(&format!("{}Filter: {}  (cost={:.2} rows={})\n",
                indent, predicate, plan.cost, plan.estimated_rows));
            explain_node(input, depth + 1, out);
        }
        PhysicalOperator::Project { input, columns } => {
            out.push_str(&format!("{}Project: [{}]\n", indent, columns.join(", ")));
            explain_node(input, depth + 1, out);
        }
        PhysicalOperator::Join { method, left, right, condition } => {
            out.push_str(&format!("{}{} on {}  (cost={:.2} rows={})\n",
                indent, method.as_str(), condition, plan.cost, plan.estimated_rows));
            explain_node(left, depth + 1, out);
            explain_node(right, depth + 1, out);
        }
        PhysicalOperator::Aggregate { input, group_key, aggs } => {
            out.push_str(&format!("{}HashAggregate: group_by={} aggs=[{}]\n",
                indent, group_key, aggs.join(", ")));
            explain_node(input, depth + 1, out);
        }
        PhysicalOperator::Limit { input, count } => {
            out.push_str(&format!("{}Limit: {}\n", indent, count));
            explain_node(input, depth + 1, out);
        }
    }
}

#[cfg(test)]
mod explain_tests {
    use super::*;
    use crate::stats::{ColumnStats, TableStats, StatsCatalog};

    #[test]
    fn explain_shows_plan() {
        let mut catalog = StatsCatalog::new();
        catalog.register(TableStats::new("users", 10_000));
        let opt = Optimizer::new(catalog);
        let logical = LogicalPlan::Scan { table: "users".into(), predicate: None };
        let physical = opt.optimize(&logical);
        let output = explain(&physical);
        assert!(output.plan_text.contains("SeqScan"));
        assert!(output.plan_text.contains("users"));
    }

    #[test]
    fn explain_analyze_shows_actuals() {
        let mut catalog = StatsCatalog::new();
        catalog.register(TableStats::new("orders", 100_000));
        let opt = Optimizer::new(catalog);
        let logical = LogicalPlan::Limit {
            input: Box::new(LogicalPlan::Scan { table: "orders".into(), predicate: None }),
            count: 100,
        };
        let physical = opt.optimize(&logical);
        let output = explain_analyze(&physical, 100, 500);
        assert!(output.actual_rows == 100);
        assert!(output.actual_time_us == 500);
    }
}
