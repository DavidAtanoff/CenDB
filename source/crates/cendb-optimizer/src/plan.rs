//! Logical and physical plan types for the optimizer.

use crate::cost as spec_cost;
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
    /// Sort the input by `column` ascending. Also acts as a marker that
    /// the data is sorted (used by the optimizer to choose merge join).
    Sort {
        input: Box<LogicalPlan>,
        column: String,
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
            LogicalPlan::Sort { input, .. } => input.estimate_rows(catalog),
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

    /// Collect the set of table names referenced by this plan (depth-first,
    /// left-to-right). Used by filter pushdown to know which tables a
    /// predicate conjunct references.
    pub fn table_names(&self) -> Vec<String> {
        fn go(plan: &LogicalPlan, out: &mut Vec<String>) {
            match plan {
                LogicalPlan::Scan { table, .. } => {
                    if !out.contains(table) {
                        out.push(table.clone());
                    }
                }
                LogicalPlan::Filter { input, .. }
                | LogicalPlan::Project { input, .. }
                | LogicalPlan::Sort { input, .. }
                | LogicalPlan::Limit { input, .. } => go(input, out),
                LogicalPlan::Join { left, right, .. } => {
                    go(left, out);
                    go(right, out);
                }
                LogicalPlan::Aggregate { input, .. } => go(input, out),
            }
        }
        let mut out = Vec::new();
        go(self, &mut out);
        out
    }

    /// Is this plan node a `Sort` (i.e. produces sorted output)?
    pub fn is_sorted(&self) -> bool {
        matches!(self, LogicalPlan::Sort { .. })
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
    Sort { input: Box<PhysicalPlan>, column: String },
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
            LogicalPlan::Sort { input, column } => {
                let child = self.optimize_node(input);
                let rows = child.estimated_rows;
                let cost = child.cost + Self::sort_cost_helper(rows);
                PhysicalPlan::new(
                    PhysicalOperator::Sort {
                        input: Box::new(child),
                        column: column.clone(),
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

    /// Sort cost helper used by `optimize_node` for the `Sort` logical node.
    /// Matches the spec cost model: `rows * log2(rows) * 0.01`.
    fn sort_cost_helper(rows: u64) -> f64 {
        if rows == 0 {
            0.0
        } else {
            rows as f64 * (rows as f64).log2() * 0.01
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
// Spec-defined physical plan selection (Part 2 of the task).
//
// `physical_plan(logical, stats)` walks the logical plan, applies rule-based
// filter pushdown, and selects the cheapest physical operator for each node
// using the spec cost model (`cost::seq_scan_cost`, etc.).
// ============================================================================

/// Whether a join condition is an equi-join. We treat any condition that
/// contains `==` (without `!=`) as an equi-join. Other operators (`<`, `>`,
/// `<=`, `>=`, `!=`) mark it as a theta-join, for which only nested loop
/// works.
pub fn is_equi_join(condition: &str) -> bool {
    // Strip negated equality first so that `!=` doesn't count as `==`.
    let without_neq = condition.replace("!=", " ");
    without_neq.contains("==")
}

/// Determine which side of a join a conjunct refers to. Returns
/// `Some(true)` for left, `Some(false)` for right, `None` if it references
/// both (or neither) — in which case the conjunct stays at the join level.
fn conjunct_side(conjunct: &str, left_tables: &[String], right_tables: &[String]) -> Option<bool> {
    // Look for table-qualified column references like `users.age` or
    // bare table names.
    let mut refs_left = false;
    let mut refs_right = false;
    for token in conjunct.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '.') {
        if token.is_empty() {
            continue;
        }
        // token could be "users" or "users.age" — check both halves.
        let prefix = token.split('.').next().unwrap_or(token);
        if left_tables.iter().any(|t| t == prefix) {
            refs_left = true;
        }
        if right_tables.iter().any(|t| t == prefix) {
            refs_right = true;
        }
    }
    match (refs_left, refs_right) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

/// Recursively push Filter nodes down through the plan tree. Currently
/// implemented rules:
///   * `Filter { input: Join, predicate }` -> `Join { Filter { left, predicate_left }, Filter { right, predicate_right }, ... }`
///     where each conjunct is pushed to whichever side its column refs belong.
///     Conjuncts that reference both sides (or neither) stay at the join
///     level.
///   * `Filter { input: Filter, predicate }` -> merged.
///   * `Filter { input: Project, predicate }` -> pushed through (best effort).
pub fn push_down_filters(plan: &LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Filter { input, predicate } => {
            let pushed_input = push_down_filters(input);
            push_filter_into(pushed_input, predicate.clone())
        }
        LogicalPlan::Join { left, right, condition } => LogicalPlan::Join {
            left: Box::new(push_down_filters(left)),
            right: Box::new(push_down_filters(right)),
            condition: condition.clone(),
        },
        LogicalPlan::Project { input, columns } => LogicalPlan::Project {
            input: Box::new(push_down_filters(input)),
            columns: columns.clone(),
        },
        LogicalPlan::Sort { input, column } => LogicalPlan::Sort {
            input: Box::new(push_down_filters(input)),
            column: column.clone(),
        },
        LogicalPlan::Aggregate { input, group_key, aggs } => LogicalPlan::Aggregate {
            input: Box::new(push_down_filters(input)),
            group_key: group_key.clone(),
            aggs: aggs.clone(),
        },
        LogicalPlan::Limit { input, count } => LogicalPlan::Limit {
            input: Box::new(push_down_filters(input)),
            count: *count,
        },
        LogicalPlan::Scan { .. } => plan.clone(),
    }
}

/// Push a single filter predicate into `input`, recursing into joins etc.
fn push_filter_into(input: LogicalPlan, predicate: String) -> LogicalPlan {
    match input {
        LogicalPlan::Join { left, right, condition } => {
            // Split the predicate by ` and ` (case-insensitive), classify each
            // conjunct by which side it references.
            let conjuncts: Vec<String> = split_and(&predicate);
            let left_tables = left.table_names();
            let right_tables = right.table_names();

            let mut left_conjuncts: Vec<String> = Vec::new();
            let mut right_conjuncts: Vec<String> = Vec::new();
            let mut top_conjuncts: Vec<String> = Vec::new();

            for c in conjuncts {
                match conjunct_side(&c, &left_tables, &right_tables) {
                    Some(true) => left_conjuncts.push(c),
                    Some(false) => right_conjuncts.push(c),
                    None => top_conjuncts.push(c),
                }
            }

            let new_left = if left_conjuncts.is_empty() {
                push_down_filters(&left)
            } else {
                push_filter_into(push_down_filters(&left), join_and(&left_conjuncts))
            };
            let new_right = if right_conjuncts.is_empty() {
                push_down_filters(&right)
            } else {
                push_filter_into(push_down_filters(&right), join_and(&right_conjuncts))
            };

            let new_join = LogicalPlan::Join {
                left: Box::new(new_left),
                right: Box::new(new_right),
                condition,
            };
            if top_conjuncts.is_empty() {
                new_join
            } else {
                LogicalPlan::Filter {
                    input: Box::new(new_join),
                    predicate: join_and(&top_conjuncts),
                }
            }
        }
        LogicalPlan::Filter { input: inner, predicate: inner_pred } => {
            // Merge two filters: combined predicate is `inner_pred AND predicate`.
            let merged = format!("{} and {}", inner_pred, predicate);
            push_filter_into(*inner, merged)
        }
        other => LogicalPlan::Filter {
            input: Box::new(other),
            predicate,
        },
    }
}

/// Split a predicate string on ` and ` (case-insensitive). Whitespace
/// around each conjunct is trimmed.
fn split_and(predicate: &str) -> Vec<String> {
    predicate
        .split_whitespace()
        .fold(Vec::<String>::new(), |mut acc, w| {
            if w.eq_ignore_ascii_case("and") {
                acc.push(String::new());
            } else if let Some(last) = acc.last_mut() {
                if !last.is_empty() {
                    last.push(' ');
                }
                last.push_str(w);
            } else {
                acc.push(w.to_string());
            }
            acc
        })
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect()
}

/// Join a list of conjuncts with ` and `.
fn join_and(conjuncts: &[String]) -> String {
    conjuncts.join(" and ")
}

/// Choose the physical join method using the spec rules:
///   * If both inputs are sorted on their join column, use `Merge`.
///   * Else if equi-join and at least one side has < 10,000 rows, use `Hash`.
///   * Else if equi-join (but both sides large), use `Hash` (still cheaper
///     than nested loop).
///   * Else (theta-join), use `NestedLoop`.
pub fn choose_join_method_spec(
    left: &LogicalPlan,
    right: &LogicalPlan,
    condition: &str,
    stats: &StatsCatalog,
) -> JoinMethod {
    let equi = is_equi_join(condition);
    if !equi {
        return JoinMethod::NestedLoop;
    }
    if left.is_sorted() && right.is_sorted() {
        return JoinMethod::Merge;
    }
    let left_rows = left.estimate_rows(stats);
    let right_rows = right.estimate_rows(stats);
    if left_rows < 10_000 || right_rows < 10_000 {
        return JoinMethod::Hash;
    }
    // Both large and equi-join: hash is still cheaper than nested loop for
    // large n. (n+m) << (n*m*0.01) when n,m >= 10K.
    JoinMethod::Hash
}

/// Estimate the column count for a scan, used by `seq_scan_cost`. Falls
/// back to the registered `avg_row_width / 8` (assuming 8-byte columns)
/// or 4 if stats are unknown.
fn estimate_column_count(stats: &StatsCatalog, table: &str) -> usize {
    stats
        .get(table)
        .map(|s| s.columns.len().max(1))
        .unwrap_or(4)
}

/// Choose between SeqScan and IndexScan for a logical Scan node. Uses
/// index existence and a simple selectivity heuristic.
pub fn choose_scan_op(
    table: &str,
    predicate: &Option<String>,
    stats: &StatsCatalog,
) -> PhysicalOperator {
    // If there's no predicate, always seq scan.
    let pred = match predicate {
        Some(p) if !p.is_empty() => p,
        _ => {
            let rows = stats.get(table).map(|s| s.row_count).unwrap_or(1000) as usize;
            let cols = estimate_column_count(stats, table);
            let _ = rows;
            let _ = cols;
            return PhysicalOperator::SeqScan {
                table: table.to_string(),
                predicate: predicate.clone(),
            };
        }
    };

    // Try to find an indexed column referenced in the predicate.
    // We scan the predicate for any `table.col` or `col` token whose column
    // has an index.
    if let Some(col) = first_indexed_column_in_predicate(table, pred, stats) {
        let index_name = stats
            .index_for(table, &col)
            .unwrap_or("default_index")
            .to_string();
        return PhysicalOperator::IndexScan {
            table: table.to_string(),
            index: index_name,
            key: col,
        };
    }
    PhysicalOperator::SeqScan {
        table: table.to_string(),
        predicate: predicate.clone(),
    }
}

/// Find the first indexed column of `table` that appears in `predicate`.
fn first_indexed_column_in_predicate(
    table: &str,
    predicate: &str,
    stats: &StatsCatalog,
) -> Option<String> {
    // Walk tokens; for each table-qualified column `table.col`, check
    // whether `col` is indexed.
    for token in predicate.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '.') {
        if token.is_empty() {
            continue;
        }
        if let Some((t, c)) = token.split_once('.') {
            if t == table && stats.has_index(table, c) {
                return Some(c.to_string());
            }
        } else if stats.has_index(table, token) {
            return Some(token.to_string());
        }
    }
    None
}

/// Build a physical plan from a logical plan using the spec cost model.
///
/// Steps:
///   1. Apply `push_down_filters` (rule-based rewrite).
///   2. Walk the (rewritten) plan, choosing the cheapest physical operator
///      for each node using the spec cost functions
///      (`cost::seq_scan_cost`, `cost::hash_join_cost`, etc.).
pub fn physical_plan(logical: &LogicalPlan, stats: &StatsCatalog) -> PhysicalPlan {
    let pushed = push_down_filters(logical);
    build_physical(&pushed, stats)
}

/// Recursive worker for `physical_plan`.
fn build_physical(plan: &LogicalPlan, stats: &StatsCatalog) -> PhysicalPlan {
    match plan {
        LogicalPlan::Scan { table, predicate } => {
            let op = choose_scan_op(table, predicate, stats);
            let rows = stats.get(table).map(|s| s.row_count).unwrap_or(1000) as usize;
            let cols = estimate_column_count(stats, table);
            let cost = match &op {
                PhysicalOperator::IndexScan { .. } => spec_cost::index_lookup_cost(),
                _ => spec_cost::seq_scan_cost(rows, cols),
            };
            // Index lookups return ~1 row; seq scans return all rows.
            let est_rows = match &op {
                PhysicalOperator::IndexScan { .. } => 1u64,
                _ => rows as u64,
            };
            PhysicalPlan::new(op, cost, est_rows)
        }
        LogicalPlan::Filter { input, predicate } => {
            let child = build_physical(input, stats);
            let rows_in = child.estimated_rows as usize;
            // Default selectivity 0.1 -> output rows = rows_in / 10.
            let rows_out = (rows_in as f64 * 0.1) as u64;
            let cost = child.cost + spec_cost::filter_cost(rows_in);
            PhysicalPlan::new(
                PhysicalOperator::Filter {
                    input: Box::new(child),
                    predicate: predicate.clone(),
                },
                cost,
                rows_out,
            )
        }
        LogicalPlan::Project { input, columns } => {
            let child = build_physical(input, stats);
            let rows = child.estimated_rows;
            // Projection is essentially free relative to scan/join; we add
            // a small per-row cost of rows * cols * 0.001.
            let cost = child.cost + rows as f64 * columns.len() as f64 * 0.001;
            PhysicalPlan::new(
                PhysicalOperator::Project {
                    input: Box::new(child),
                    columns: columns.clone(),
                },
                cost,
                rows,
            )
        }
        LogicalPlan::Sort { input, column } => {
            let child = build_physical(input, stats);
            let rows = child.estimated_rows as usize;
            let cost = child.cost + spec_cost::sort_cost(rows);
            PhysicalPlan::new(
                PhysicalOperator::Sort {
                    input: Box::new(child),
                    column: column.clone(),
                },
                cost,
                rows as u64,
            )
        }
        LogicalPlan::Join { left, right, condition } => {
            let left_plan = build_physical(left, stats);
            let right_plan = build_physical(right, stats);
            let method = choose_join_method_spec(left, right, condition, stats);
            let left_rows = left_plan.estimated_rows as usize;
            let right_rows = right_plan.estimated_rows as usize;
            let join_cost = match method {
                JoinMethod::Hash => spec_cost::hash_join_cost(left_rows, right_rows),
                JoinMethod::NestedLoop => spec_cost::nested_loop_join_cost(left_rows, right_rows),
                JoinMethod::Merge => spec_cost::merge_join_cost(left_rows, right_rows),
            };
            // Output cardinality: cross-product * 0.01 default selectivity.
            let est_rows = ((left_plan.estimated_rows * right_plan.estimated_rows) as f64 * 0.01) as u64;
            let cost = left_plan.cost + right_plan.cost + join_cost;
            PhysicalPlan::new(
                PhysicalOperator::Join {
                    method,
                    left: Box::new(left_plan),
                    right: Box::new(right_plan),
                    condition: condition.clone(),
                },
                cost,
                est_rows,
            )
        }
        LogicalPlan::Aggregate { input, group_key, aggs } => {
            let child = build_physical(input, stats);
            let rows_in = child.estimated_rows as usize;
            let rows_out = (rows_in as f64).sqrt() as u64;
            let cost = child.cost + rows_in as f64 * 0.02;
            PhysicalPlan::new(
                PhysicalOperator::Aggregate {
                    input: Box::new(child),
                    group_key: group_key.clone(),
                    aggs: aggs.clone(),
                },
                cost,
                rows_out,
            )
        }
        LogicalPlan::Limit { input, count } => {
            let child = build_physical(input, stats);
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

#[cfg(test)]
mod spec_plan_tests {
    use super::*;
    use crate::stats::{ColumnStats, StatsCatalog, TableStats};

    fn make_catalog() -> StatsCatalog {
        let mut catalog = StatsCatalog::new();
        catalog.register(
            TableStats::new("users", 1_000)
                .with_column("id", ColumnStats {
                    distinct_count: 1_000,
                    null_count: 0,
                    min_i64: Some(1),
                    max_i64: Some(1_000),
                    most_common: vec![],
                })
                .with_column("age", ColumnStats {
                    distinct_count: 80,
                    null_count: 0,
                    min_i64: Some(0),
                    max_i64: Some(120),
                    most_common: vec![],
                }),
        );
        catalog.register(
            TableStats::new("orders", 100_000)
                .with_column("user_id", ColumnStats {
                    distinct_count: 1_000,
                    null_count: 0,
                    min_i64: Some(1),
                    max_i64: Some(1_000),
                    most_common: vec![],
                })
                .with_column("total", ColumnStats {
                    distinct_count: 50_000,
                    null_count: 0,
                    min_i64: Some(1),
                    max_i64: Some(1_000_000),
                    most_common: vec![],
                }),
        );
        // Index on users.id (point lookups).
        catalog.register_index("users", "id", "users_id_idx");
        catalog
    }

    // ---- Cost-model tests ----

    #[test]
    fn cost_functions_match_spec_formulas() {
        // Verify each spec cost function returns the expected value.
        assert_eq!(spec_cost::seq_scan_cost(1000, 5), 50.0);
        assert_eq!(spec_cost::index_lookup_cost(), 1.0);
        assert_eq!(spec_cost::hash_join_cost(100, 200), 300.0);
        assert_eq!(spec_cost::nested_loop_join_cost(100, 200), 200.0);
        assert_eq!(spec_cost::merge_join_cost(100, 200), 300.0);
        assert_eq!(spec_cost::filter_cost(1000), 1.0);
        assert!((spec_cost::sort_cost(1024) - 102.4).abs() < 1e-9);
    }

    // ---- Join selection tests ----

    #[test]
    fn join_selection_small_small_uses_hash() {
        let stats = make_catalog();
        let left = LogicalPlan::Scan { table: "users".to_string(), predicate: None };
        let right = LogicalPlan::Scan { table: "users".to_string(), predicate: None };
        let m = choose_join_method_spec(&left, &right, "users.id == users.id", &stats);
        assert_eq!(m, JoinMethod::Hash);
    }

    #[test]
    fn join_selection_small_large_uses_hash() {
        let stats = make_catalog();
        let left = LogicalPlan::Scan { table: "users".to_string(), predicate: None };
        let right = LogicalPlan::Scan { table: "orders".to_string(), predicate: None };
        let m = choose_join_method_spec(&left, &right, "users.id == orders.user_id", &stats);
        // users (1K) < 10K -> Hash.
        assert_eq!(m, JoinMethod::Hash);
    }

    #[test]
    fn join_selection_unsorted_unsorted_equi_uses_hash() {
        let stats = make_catalog();
        // Both unsorted (Scan), equi-join -> Hash (one side is small).
        let left = LogicalPlan::Scan { table: "users".to_string(), predicate: None };
        let right = LogicalPlan::Scan { table: "orders".to_string(), predicate: None };
        let m = choose_join_method_spec(&left, &right, "users.id == orders.user_id", &stats);
        assert_eq!(m, JoinMethod::Hash);
    }

    #[test]
    fn join_selection_sorted_sorted_uses_merge() {
        let stats = make_catalog();
        let left = LogicalPlan::Sort {
            input: Box::new(LogicalPlan::Scan { table: "users".to_string(), predicate: None }),
            column: "id".to_string(),
        };
        let right = LogicalPlan::Sort {
            input: Box::new(LogicalPlan::Scan { table: "orders".to_string(), predicate: None }),
            column: "user_id".to_string(),
        };
        let m = choose_join_method_spec(&left, &right, "users.id == orders.user_id", &stats);
        assert_eq!(m, JoinMethod::Merge);
    }

    #[test]
    fn join_selection_theta_uses_nested_loop() {
        let stats = make_catalog();
        let left = LogicalPlan::Scan { table: "users".to_string(), predicate: None };
        let right = LogicalPlan::Scan { table: "orders".to_string(), predicate: None };
        let m = choose_join_method_spec(&left, &right, "users.id > orders.user_id", &stats);
        assert_eq!(m, JoinMethod::NestedLoop);
    }

    #[test]
    fn join_selection_not_equal_is_theta() {
        let stats = make_catalog();
        let left = LogicalPlan::Scan { table: "users".to_string(), predicate: None };
        let right = LogicalPlan::Scan { table: "orders".to_string(), predicate: None };
        // `!=` is not an equi-join.
        let m = choose_join_method_spec(&left, &right, "users.id != orders.user_id", &stats);
        assert_eq!(m, JoinMethod::NestedLoop);
    }

    // ---- Filter pushdown tests ----

    #[test]
    fn filter_pushdown_through_join() {
        // Filter { Join { Scan users, Scan orders, condition }, "users.age > 18 and orders.total > 100" }
        //   -> Join { Filter { Scan users, "users.age > 18" }, Filter { Scan orders, "orders.total > 100" }, condition }
        let logical = LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Join {
                left: Box::new(LogicalPlan::Scan { table: "users".to_string(), predicate: None }),
                right: Box::new(LogicalPlan::Scan { table: "orders".to_string(), predicate: None }),
                condition: "users.id == orders.user_id".to_string(),
            }),
            predicate: "users.age > 18 and orders.total > 100".to_string(),
        };
        let pushed = push_down_filters(&logical);
        // Top level should now be the Join (no remaining top-level filter).
        match pushed {
            LogicalPlan::Join { left, right, condition } => {
                assert_eq!(condition, "users.id == orders.user_id");
                // Left should be a Filter on users.age.
                match left.as_ref() {
                    LogicalPlan::Filter { input, predicate } => {
                        assert_eq!(predicate, "users.age > 18");
                        assert!(matches!(input.as_ref(), LogicalPlan::Scan { table, .. } if table == "users"));
                    }
                    other => panic!("expected Filter on left, got {:?}", other),
                }
                // Right should be a Filter on orders.total.
                match right.as_ref() {
                    LogicalPlan::Filter { input, predicate } => {
                        assert_eq!(predicate, "orders.total > 100");
                        assert!(matches!(input.as_ref(), LogicalPlan::Scan { table, .. } if table == "orders"));
                    }
                    other => panic!("expected Filter on right, got {:?}", other),
                }
            }
            other => panic!("expected Join at top level, got {:?}", other),
        }
    }

    #[test]
    fn filter_pushdown_keeps_cross_side_conjuncts_at_top() {
        // A conjunct that references both sides stays at the join level.
        let logical = LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Join {
                left: Box::new(LogicalPlan::Scan { table: "users".to_string(), predicate: None }),
                right: Box::new(LogicalPlan::Scan { table: "orders".to_string(), predicate: None }),
                condition: "users.id == orders.user_id".to_string(),
            }),
            predicate: "users.age > 18 and orders.total > users.age".to_string(),
        };
        let pushed = push_down_filters(&logical);
        // Top should be Filter with the cross-side conjunct.
        match pushed {
            LogicalPlan::Filter { input, predicate } => {
                assert!(predicate.contains("orders.total > users.age"));
                assert!(matches!(input.as_ref(), LogicalPlan::Join { .. }));
            }
            other => panic!("expected Filter at top, got {:?}", other),
        }
    }

    #[test]
    fn filter_pushdown_merges_adjacent_filters() {
        // Filter { Filter { Scan, p1 }, p2 } -> Filter { Scan, "p1 and p2" }
        let logical = LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Filter {
                input: Box::new(LogicalPlan::Scan { table: "users".to_string(), predicate: None }),
                predicate: "users.age > 18".to_string(),
            }),
            predicate: "users.age < 65".to_string(),
        };
        let pushed = push_down_filters(&logical);
        match pushed {
            LogicalPlan::Filter { input, predicate } => {
                assert!(predicate.contains("users.age > 18"));
                assert!(predicate.contains("users.age < 65"));
                assert!(matches!(input.as_ref(), LogicalPlan::Scan { table, .. } if table == "users"));
            }
            other => panic!("expected merged Filter, got {:?}", other),
        }
    }

    // ---- Index selection tests ----

    #[test]
    fn index_selection_point_lookup_uses_index_scan() {
        let stats = make_catalog();
        // Predicate on indexed column users.id.
        let op = choose_scan_op("users", &Some("users.id == 42".to_string()), &stats);
        match op {
            PhysicalOperator::IndexScan { table, index, key } => {
                assert_eq!(table, "users");
                assert_eq!(index, "users_id_idx");
                assert_eq!(key, "id");
            }
            other => panic!("expected IndexScan, got {:?}", other),
        }
    }

    #[test]
    fn index_selection_full_scan_uses_seq_scan() {
        let stats = make_catalog();
        // No predicate -> SeqScan.
        let op = choose_scan_op("users", &None, &stats);
        assert!(matches!(op, PhysicalOperator::SeqScan { .. }));

        // Predicate on non-indexed column -> SeqScan.
        let op = choose_scan_op("users", &Some("users.age > 18".to_string()), &stats);
        assert!(matches!(op, PhysicalOperator::SeqScan { .. }));
    }

    // ---- End-to-end tests ----

    #[test]
    fn end_to_end_simple_scan_uses_seq_scan() {
        let stats = make_catalog();
        let logical = LogicalPlan::Scan { table: "users".to_string(), predicate: None };
        let physical = physical_plan(&logical, &stats);
        assert!(matches!(physical.operator, PhysicalOperator::SeqScan { .. }));
        // cost = 1000 rows * 2 cols * 0.01 = 20.
        assert!((physical.cost - 20.0).abs() < 1e-9, "cost was {}", physical.cost);
    }

    #[test]
    fn end_to_end_index_lookup_uses_index_scan() {
        let stats = make_catalog();
        let logical = LogicalPlan::Scan {
            table: "users".to_string(),
            predicate: Some("users.id == 42".to_string()),
        };
        let physical = physical_plan(&logical, &stats);
        assert!(matches!(physical.operator, PhysicalOperator::IndexScan { .. }));
        assert!((physical.cost - 1.0).abs() < 1e-9);
    }

    #[test]
    fn end_to_end_join_uses_hash_for_small_large() {
        let stats = make_catalog();
        let logical = LogicalPlan::Join {
            left: Box::new(LogicalPlan::Scan { table: "users".to_string(), predicate: None }),
            right: Box::new(LogicalPlan::Scan { table: "orders".to_string(), predicate: None }),
            condition: "users.id == orders.user_id".to_string(),
        };
        let physical = physical_plan(&logical, &stats);
        match physical.operator {
            PhysicalOperator::Join { method, .. } => {
                assert_eq!(method, JoinMethod::Hash);
            }
            other => panic!("expected Join, got {:?}", other),
        }
    }

    #[test]
    fn end_to_end_theta_join_uses_nested_loop() {
        let stats = make_catalog();
        let logical = LogicalPlan::Join {
            left: Box::new(LogicalPlan::Scan { table: "users".to_string(), predicate: None }),
            right: Box::new(LogicalPlan::Scan { table: "orders".to_string(), predicate: None }),
            condition: "users.id > orders.user_id".to_string(),
        };
        let physical = physical_plan(&logical, &stats);
        match physical.operator {
            PhysicalOperator::Join { method, .. } => {
                assert_eq!(method, JoinMethod::NestedLoop);
            }
            other => panic!("expected Join, got {:?}", other),
        }
    }

    #[test]
    fn end_to_end_sorted_join_uses_merge() {
        let stats = make_catalog();
        let logical = LogicalPlan::Join {
            left: Box::new(LogicalPlan::Sort {
                input: Box::new(LogicalPlan::Scan { table: "users".to_string(), predicate: None }),
                column: "id".to_string(),
            }),
            right: Box::new(LogicalPlan::Sort {
                input: Box::new(LogicalPlan::Scan { table: "orders".to_string(), predicate: None }),
                column: "user_id".to_string(),
            }),
            condition: "users.id == orders.user_id".to_string(),
        };
        let physical = physical_plan(&logical, &stats);
        match physical.operator {
            PhysicalOperator::Join { method, .. } => {
                assert_eq!(method, JoinMethod::Merge);
            }
            other => panic!("expected Join, got {:?}", other),
        }
    }

    #[test]
    fn end_to_end_filter_pushdown_into_join() {
        let stats = make_catalog();
        let logical = LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Join {
                left: Box::new(LogicalPlan::Scan { table: "users".to_string(), predicate: None }),
                right: Box::new(LogicalPlan::Scan { table: "orders".to_string(), predicate: None }),
                condition: "users.id == orders.user_id".to_string(),
            }),
            predicate: "users.age > 18".to_string(),
        };
        let physical = physical_plan(&logical, &stats);
        // After pushdown: Join with Filter{Scan users, "users.age > 18"} on left.
        match physical.operator {
            PhysicalOperator::Join { left, .. } => {
                assert!(matches!(left.operator, PhysicalOperator::Filter { .. }));
            }
            other => panic!("expected Join at top level after pushdown, got {:?}", other),
        }
    }

    #[test]
    fn end_to_end_cenql_query_with_join() {
        // Parse a CenQL query with a join, convert to a logical plan,
        // optimize it, and verify the physical plan uses the right operators.
        use cendb_cenql::parser::parse;
        let src = r#"from users
                    | join orders on users.id == orders.user_id
                    | take 10"#;
        let pipeline = parse(src).expect("parse");
        let logical = cenql_to_logical(&pipeline);
        let stats = make_catalog();
        let physical = physical_plan(&logical, &stats);
        // Top level should be Limit (the `take 10`).
        match physical.operator {
            PhysicalOperator::Limit { ref input, count } => {
                assert_eq!(count, 10);
                // Input should be a Join (Hash, because users is small).
                match input.operator {
                    PhysicalOperator::Join { method, .. } => {
                        assert_eq!(method, JoinMethod::Hash);
                    }
                    ref other => panic!("expected Join under Limit, got {:?}", other),
                }
            }
            ref other => panic!("expected Limit at top level, got {:?}", other),
        }
    }

    // ---- Helper tests ----

    #[test]
    fn split_and_handles_case() {
        let parts = split_and("users.age > 18 and orders.total > 100");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "users.age > 18");
        assert_eq!(parts[1], "orders.total > 100");
    }

    #[test]
    fn split_and_single_conjunct() {
        let parts = split_and("users.age > 18");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], "users.age > 18");
    }

    #[test]
    fn is_equi_join_detects_equality() {
        assert!(is_equi_join("users.id == orders.user_id"));
        assert!(is_equi_join("a.x == b.y and a.z > 5"));
        assert!(!is_equi_join("users.id != orders.user_id"));
        assert!(!is_equi_join("users.age > 18"));
    }
}

// ============================================================================
// CenQL AST -> LogicalPlan converter. Used by the end-to-end optimizer test
// to turn a parsed CenQL pipeline into a logical plan that the optimizer
// can work with.
// ============================================================================

use cendb_cenql::ast::{CenqlPipeline, CenqlStage, Expr, BinaryOp};

/// Format a CenQL expression as a SQL-style predicate string suitable for
/// the optimizer's `condition` / `predicate` fields (e.g.
/// `users.id == orders.user_id`). This is more useful than `Expr::Display`,
/// which formats binary operators with their Debug repr (`Eq`).
fn expr_to_predicate(expr: &Expr) -> String {
    match expr {
        Expr::Column(c) => c.clone(),
        Expr::I64(v) => v.to_string(),
        Expr::F64(v) => v.to_string(),
        Expr::Str(s) => format!("\"{}\"", s),
        Expr::Bool(b) => b.to_string(),
        Expr::Binary { op, lhs, rhs } => {
            let op_str = match op {
                BinaryOp::Eq => "==",
                BinaryOp::Ne => "!=",
                BinaryOp::Lt => "<",
                BinaryOp::Le => "<=",
                BinaryOp::Gt => ">",
                BinaryOp::Ge => ">=",
                BinaryOp::And => "and",
                BinaryOp::Or => "or",
                BinaryOp::Add => "+",
                BinaryOp::Sub => "-",
                BinaryOp::Mul => "*",
                BinaryOp::Div => "/",
            };
            let lhs_s = expr_to_predicate(lhs);
            let rhs_s = expr_to_predicate(rhs);
            if matches!(op, BinaryOp::And | BinaryOp::Or) {
                format!("{} {} {}", lhs_s, op_str, rhs_s)
            } else {
                format!("{} {} {}", lhs_s, op_str, rhs_s)
            }
        }
        Expr::Call { name, args } => {
            let args_s: Vec<String> = args.iter().map(expr_to_predicate).collect();
            format!("{}({})", name, args_s.join(", "))
        }
        Expr::In { value, subquery } => {
            // The optimizer's predicate string is opaque; we just record
            // that this is an IN-subquery test. The subquery text is
            // included for debug visibility.
            format!("{} in ({})", expr_to_predicate(value), subquery)
        }
    }
}

/// Convert a parsed CenQL pipeline into a logical plan.
///
/// This is a best-effort converter for the operators relevant to the
/// optimizer implementation: `from`, `filter`, `select`, `sort`, `take`,
/// and `join`. Other stage kinds (group_by, window, match, return)
/// fall back to passing the existing plan through unchanged.
pub fn cenql_to_logical(pipeline: &CenqlPipeline) -> LogicalPlan {
    let mut plan: Option<LogicalPlan> = None;
    for stage in &pipeline.stages {
        plan = Some(match (stage, plan.take()) {
            (CenqlStage::From { name }, _) => LogicalPlan::Scan {
                table: name.clone(),
                predicate: None,
            },
            (CenqlStage::Filter { expr }, Some(input)) => LogicalPlan::Filter {
                input: Box::new(input),
                predicate: expr_to_predicate(expr),
            },
            (CenqlStage::Select { columns }, Some(input)) => LogicalPlan::Project {
                input: Box::new(input),
                columns: columns.clone(),
            },
            (CenqlStage::Sort { column, dir: _ }, Some(input)) => LogicalPlan::Sort {
                input: Box::new(input),
                column: column.clone(),
            },
            (CenqlStage::Take { n }, Some(input)) => LogicalPlan::Limit {
                input: Box::new(input),
                count: *n,
            },
            (CenqlStage::Join { source, kind: _, on }, Some(input)) => LogicalPlan::Join {
                left: Box::new(input),
                right: Box::new(LogicalPlan::Scan {
                    table: source.clone(),
                    predicate: None,
                }),
                condition: expr_to_predicate(on),
            },
            (_, None) => {
                // No prior plan (shouldn't happen for valid pipelines starting
                // with `from`); fall back to a placeholder scan.
                LogicalPlan::Scan {
                    table: "_unknown".to_string(),
                    predicate: None,
                }
            }
            (_, Some(input)) => {
                // Unsupported stage; pass through the existing plan.
                input
            }
        });
    }
    plan.expect("pipeline must have at least one stage")
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
        PhysicalOperator::Sort { input, column } => {
            out.push_str(&format!("{}Sort: {}  (cost={:.2} rows={})\n",
                indent, column, plan.cost, plan.estimated_rows));
            explain_node(input, depth + 1, out);
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
