//! Subquery execution: FROM-subquery and IN-subquery support.
//!
//! ## Overview
//!
//! CenQL allows two kinds of subqueries:
//!
//!   * **FROM subquery**: `from (subquery) | ...` — the subquery is
//!     materialised first, then its rows feed the rest of the outer
//!     pipeline as if they came from a named table.
//!
//!   * **IN subquery**: `filter x in (subquery)` — the subquery is
//!     materialised into a set of values (taken from its first column),
//!     and the outer filter keeps rows where `x` is in that set.
//!
//! Both kinds are executed **eagerly**: the subquery runs to completion,
//! its result is materialised into `Vec<Vec<Value>>`, then the outer
//! query consumes that materialised result. This is the simplest
//! execution strategy and matches what the spec asks for.
//!
//! ## Table provider
//!
//! The executor is parameterised by a `TableProvider` trait so that
//! tests can plug in an in-memory provider. A real engine would plug
//! in a provider that reads from PAX blocks / the buffer pool.

use cendb_cenql::ast::{BinaryOp, CenqlPipeline, CenqlStage, Expr, SortDir};
use cendb_core::{CenError, CenResult, CenStatus, Value};
use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

// ============================================================================
// Table provider.
// ============================================================================

/// A table provider: maps table names to row sets. Each row is a
/// `Vec<Value>` (one entry per column).
pub trait TableProvider {
    /// Look up a table by name. Returns `Ok(rows)` if found,
    /// `Err(ErrNotFound)` otherwise. The returned slice is borrowed for
    /// the lifetime of the provider — implementations may cache.
    fn scan(&self, table: &str) -> CenResult<&[Vec<Value>]>;

    /// Look up the column names of a table, in order. Returns `None` if
    /// column metadata is unavailable for `table` (the executor falls
    /// back to positional indexing in that case).
    fn columns(&self, table: &str) -> Option<Vec<String>>;
}

/// In-memory table provider: a `HashMap<String, Vec<Vec<Value>>>`.
/// Useful for tests and as a materialised-subquery holder.
#[derive(Default, Clone)]
pub struct InMemoryProvider {
    tables: HashMap<String, Vec<Vec<Value>>>,
    columns: HashMap<String, Vec<String>>,
}

impl InMemoryProvider {
    pub fn new() -> Self {
        Self {
            tables: HashMap::new(),
            columns: HashMap::new(),
        }
    }

    /// Register a table by name with the given rows and column names.
    pub fn register_with_columns(
        &mut self,
        name: impl Into<String>,
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    ) {
        let name = name.into();
        self.columns.insert(name.clone(), columns);
        self.tables.insert(name, rows);
    }

    /// Register a table by name with the given rows (no column metadata).
    pub fn register(&mut self, name: impl Into<String>, rows: Vec<Vec<Value>>) {
        self.tables.insert(name.into(), rows);
    }

    /// Add a single named table built from i64 rows (helper for tests).
    pub fn register_i64(&mut self, name: &str, columns: &[&str], rows: &[&[i64]]) {
        let rows: Vec<Vec<Value>> = rows
            .iter()
            .map(|r| r.iter().map(|v| Value::I64(*v)).collect())
            .collect();
        let cols: Vec<String> = columns.iter().map(|s| s.to_string()).collect();
        self.register_with_columns(name, cols, rows);
    }
}

impl TableProvider for InMemoryProvider {
    fn scan(&self, table: &str) -> CenResult<&[Vec<Value>]> {
        self.tables
            .get(table)
            .map(|v| v.as_slice())
            .ok_or_else(|| CenError::not_found(format!("table not found: {}", table)))
    }

    fn columns(&self, table: &str) -> Option<Vec<String>> {
        self.columns.get(table).cloned()
    }
}

// ============================================================================
// Value helpers (with SQL NULL semantics).
// ============================================================================

/// Hashable wrapper for `Value` used by IN-subquery membership sets.
/// NULLs are *excluded* from the set (they never match an IN predicate).
#[derive(Clone, Debug)]
struct HashableValue<'a>(&'a Value);

impl<'a> Hash for HashableValue<'a> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self.0 {
            Value::Null => 0u8.hash(state),
            Value::Bool(b) => {
                1u8.hash(state);
                b.hash(state)
            }
            Value::I64(i) => {
                2u8.hash(state);
                i.hash(state)
            }
            Value::U64(u) => {
                3u8.hash(state);
                u.hash(state)
            }
            Value::F64(f) => {
                4u8.hash(state);
                f.to_bits().hash(state)
            }
            Value::Bytes(b) => {
                5u8.hash(state);
                b.hash(state)
            }
            Value::Timestamp(t) => {
                6u8.hash(state);
                t.hash(state)
            }
        }
    }
}

impl<'a> PartialEq for HashableValue<'a> {
    fn eq(&self, other: &Self) -> bool {
        // SQL NULL semantics: NULL != NULL.
        match (self.0, other.0) {
            (Value::Null, _) | (_, Value::Null) => false,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::I64(a), Value::I64(b)) => a == b,
            (Value::U64(a), Value::U64(b)) => a == b,
            (Value::F64(a), Value::F64(b)) => a.to_bits() == b.to_bits(),
            (Value::Bytes(a), Value::Bytes(b)) => a == b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a == b,
            _ => false,
        }
    }
}

impl<'a> Eq for HashableValue<'a> {}

// ============================================================================
// Pipeline executor.
// ============================================================================

/// Execute a CenQL pipeline against a `TableProvider`. Returns the
/// materialised result rows.
///
/// Supports: `from <table>`, `from (subquery)`, `filter`, `select`,
/// `sort`, `take`. Other stage kinds (group_by, window, match, return,
/// join) are not implemented and return an `ErrInternal`.
pub fn execute_pipeline<P: TableProvider>(
    pipeline: &CenqlPipeline,
    provider: &P,
) -> CenResult<Vec<Vec<Value>>> {
    let (rows, _cols) = execute_pipeline_with_cols(pipeline, provider)?;
    Ok(rows)
}

/// Execute a single stage, given the upstream rows and (optional)
/// column names. Returns the new rows + new column names.
fn execute_stage<P: TableProvider>(
    stage: &CenqlStage,
    input_rows: Vec<Vec<Value>>,
    input_cols: Option<Vec<String>>,
    provider: &P,
) -> CenResult<(Vec<Vec<Value>>, Option<Vec<String>>)> {
    match stage {
        CenqlStage::From { name } => {
            let rows = provider.scan(name)?;
            // Clone to detach from the provider's borrow.
            let cols = provider.columns(name);
            Ok((rows.to_vec(), cols))
        }
        CenqlStage::FromSubquery { pipeline } => {
            // Materialise the subquery first, then use its rows as the
            // outer pipeline's source. The subquery's last Select stage
            // (if any) determines the column names; otherwise we propagate
            // whatever column metadata the subquery produced.
            let (sub_rows, sub_cols) = execute_pipeline_with_cols(pipeline, provider)?;
            Ok((sub_rows, sub_cols))
        }
        CenqlStage::Filter { expr } => {
            let mut out = Vec::with_capacity(input_rows.len());
            for row in &input_rows {
                if eval_predicate(expr, row, &input_cols, provider)? {
                    out.push(row.clone());
                }
            }
            Ok((out, input_cols))
        }
        CenqlStage::Select { columns } => {
            // Project: keep only the named columns. Look up each column's
            // index in `input_cols`. If we don't have column metadata,
            // fall back to positional indexing (the first N columns).
            let indices: Vec<usize> = match &input_cols {
                Some(cols) => columns
                    .iter()
                    .map(|c| {
                        let bare = c.rsplit('.').next().unwrap_or(c);
                        cols.iter()
                            .position(|x| x == c || x.rsplit('.').next().unwrap_or(x) == bare)
                            .unwrap_or(0)
                    })
                    .collect(),
                None => (0..columns.len()).collect(),
            };
            let out: Vec<Vec<Value>> = input_rows
                .iter()
                .map(|row| {
                    indices
                        .iter()
                        .map(|&i| row.get(i).cloned().unwrap_or(Value::Null))
                        .collect()
                })
                .collect();
            Ok((out, Some(columns.clone())))
        }
        CenqlStage::Sort { column, dir } => {
            let idx = match &input_cols {
                Some(cols) => {
                    let bare = column.rsplit('.').next().unwrap_or(column);
                    cols.iter()
                        .position(|c| c == column || c.rsplit('.').next().unwrap_or(c) == bare)
                        .unwrap_or(0)
                }
                None => 0,
            };
            let mut sorted = input_rows;
            sorted.sort_by(|a, b| {
                let av = a.get(idx).unwrap_or(&Value::Null);
                let bv = b.get(idx).unwrap_or(&Value::Null);
                let ord = value_cmp(av, bv);
                match dir {
                    SortDir::Asc => ord,
                    SortDir::Desc => ord.reverse(),
                }
            });
            Ok((sorted, input_cols))
        }
        CenqlStage::Take { n } => {
            let n = (*n as usize).min(input_rows.len());
            let out = input_rows.into_iter().take(n).collect();
            Ok((out, input_cols))
        }
        CenqlStage::Join { .. }
        | CenqlStage::GroupBy { .. }
        | CenqlStage::Window { .. }
        | CenqlStage::Match { .. }
        | CenqlStage::Return { .. } => Err(CenError::internal(format!(
            "stage not supported by subquery executor: {:?}",
            stage
        ))),
    }
}

/// Like `execute_pipeline` but also returns the final column metadata.
fn execute_pipeline_with_cols<P: TableProvider>(
    pipeline: &CenqlPipeline,
    provider: &P,
) -> CenResult<(Vec<Vec<Value>>, Option<Vec<String>>)> {
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut columns: Option<Vec<String>> = None;
    for stage in &pipeline.stages {
        let (new_rows, new_cols) = execute_stage(stage, rows, columns, provider)?;
        rows = new_rows;
        columns = new_cols;
    }
    Ok((rows, columns))
}

// ============================================================================
// Predicate evaluation.
// ============================================================================

/// Evaluate a boolean expression against a row.
fn eval_predicate<P: TableProvider>(
    expr: &Expr,
    row: &[Value],
    columns: &Option<Vec<String>>,
    provider: &P,
) -> CenResult<bool> {
    let v = eval_expr(expr, row, columns, provider)?;
    Ok(matches!(v, Value::Bool(true)))
}

/// Evaluate an `Expr` to a `Value`.
fn eval_expr<P: TableProvider>(
    expr: &Expr,
    row: &[Value],
    columns: &Option<Vec<String>>,
    provider: &P,
) -> CenResult<Value> {
    match expr {
        Expr::Column(name) => {
            // Look up the column by name in `columns`; fall back to index 0
            // if we don't have metadata. Strip dotted prefixes
            // (`users.age` -> `age`) for matching.
            let bare = name.rsplit('.').next().unwrap_or(name);
            let idx = match columns {
                Some(cols) => cols
                    .iter()
                    .position(|c| c == name || c.rsplit('.').next().unwrap_or(c) == bare)
                    .unwrap_or(0),
                None => 0,
            };
            Ok(row.get(idx).cloned().unwrap_or(Value::Null))
        }
        Expr::I64(v) => Ok(Value::I64(*v)),
        Expr::F64(v) => Ok(Value::F64(*v)),
        Expr::Str(s) => Ok(Value::Bytes(s.as_bytes().to_vec())),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Binary { op, lhs, rhs } => {
            let l = eval_expr(lhs, row, columns, provider)?;
            let r = eval_expr(rhs, row, columns, provider)?;
            Ok(eval_binary(*op, &l, &r))
        }
        Expr::Call { .. } => Err(CenError::internal(
            "function calls not supported by subquery executor",
        )),
        Expr::In { value, subquery } => {
            // Materialise the subquery, collect its first-column values
            // into a set, then check membership.
            let sub_rows = execute_pipeline(subquery, provider)?;
            let mut set: HashSet<HashableValueOwned> = HashSet::new();
            for r in &sub_rows {
                if let Some(v) = r.first() {
                    if !matches!(v, Value::Null) {
                        set.insert(HashableValueOwned(v.clone()));
                    }
                }
            }
            let probe = eval_expr(value, row, columns, provider)?;
            if matches!(probe, Value::Null) {
                return Ok(Value::Bool(false)); // NULL IN (...) is false.
            }
            let contains = set.contains(&HashableValueOwned(probe));
            Ok(Value::Bool(contains))
        }
    }
}

/// Owned hashable value (for the IN-subquery membership set).
#[derive(Clone, Debug)]
struct HashableValueOwned(Value);

impl Hash for HashableValueOwned {
    fn hash<H: Hasher>(&self, state: &mut H) {
        HashableValue(&self.0).hash(state);
    }
}

impl PartialEq for HashableValueOwned {
    fn eq(&self, other: &Self) -> bool {
        HashableValue(&self.0).eq(&HashableValue(&other.0))
    }
}

impl Eq for HashableValueOwned {}

/// Evaluate a binary operator on two values. Returns `Value::Bool(...)`
/// for comparison/logical ops, or the arithmetic result for arithmetic
/// ops. NULL propagates: any comparison involving NULL yields
/// `Value::Bool(false)` (SQL three-valued logic collapsed to false).
fn eval_binary(op: BinaryOp, l: &Value, r: &Value) -> Value {
    // NULL propagation: any op with a NULL operand yields Bool(false) for
    // comparisons and Null for arithmetic.
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return match op {
            BinaryOp::And | BinaryOp::Or => Value::Bool(false),
            BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt
            | BinaryOp::Ge => Value::Bool(false),
            _ => Value::Null,
        };
    }
    match op {
        BinaryOp::Eq => Value::Bool(value_eq(l, r)),
        BinaryOp::Ne => Value::Bool(!value_eq(l, r)),
        BinaryOp::Lt => Value::Bool(value_cmp(l, r) == std::cmp::Ordering::Less),
        BinaryOp::Le => Value::Bool(matches!(value_cmp(l, r), std::cmp::Ordering::Less | std::cmp::Ordering::Equal)),
        BinaryOp::Gt => Value::Bool(value_cmp(l, r) == std::cmp::Ordering::Greater),
        BinaryOp::Ge => Value::Bool(matches!(value_cmp(l, r), std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)),
        BinaryOp::And => Value::Bool(matches!(l, Value::Bool(true)) && matches!(r, Value::Bool(true))),
        BinaryOp::Or => Value::Bool(matches!(l, Value::Bool(true)) || matches!(r, Value::Bool(true))),
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div => {
            eval_arith(op, l, r)
        }
    }
}

/// Evaluate arithmetic on numeric values. Returns `Value::Null` if
/// types are incompatible.
fn eval_arith(op: BinaryOp, l: &Value, r: &Value) -> Value {
    use BinaryOp::*;
    match (l, r) {
        (Value::I64(a), Value::I64(b)) => match op {
            Add => Value::I64(a.wrapping_add(*b)),
            Sub => Value::I64(a.wrapping_sub(*b)),
            Mul => Value::I64(a.wrapping_mul(*b)),
            Div => {
                if *b == 0 {
                    Value::Null
                } else {
                    Value::I64(a / b)
                }
            }
            _ => Value::Null,
        },
        (Value::F64(a), Value::F64(b)) => match op {
            Add => Value::F64(a + b),
            Sub => Value::F64(a - b),
            Mul => Value::F64(a * b),
            Div => Value::F64(a / b),
            _ => Value::Null,
        },
        _ => Value::Null,
    }
}

// ============================================================================
// Value comparison helpers (mirrors the ones in `join.rs` but kept here
// to avoid a cross-module dependency).
// ============================================================================

fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => false,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::I64(x), Value::I64(y)) => x == y,
        (Value::U64(x), Value::U64(y)) => x == y,
        (Value::F64(x), Value::F64(y)) => x.to_bits() == y.to_bits(),
        (Value::Bytes(x), Value::Bytes(y)) => x == y,
        (Value::Timestamp(x), Value::Timestamp(y)) => x == y,
        _ => false,
    }
}

fn value_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn kind_tag(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::I64(_) => 2,
            Value::U64(_) => 3,
            Value::F64(_) => 4,
            Value::Bytes(_) => 5,
            Value::Timestamp(_) => 6,
        }
    }
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::I64(x), Value::I64(y)) => x.cmp(y),
        (Value::U64(x), Value::U64(y)) => x.cmp(y),
        (Value::F64(x), Value::F64(y)) => x.total_cmp(y),
        (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
        (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
        _ => kind_tag(a).cmp(&kind_tag(b)),
    }
}

// Re-export CenStatus for the test attribute path.
#[allow(unused_imports)]
use CenStatus as _CenStatus;

// ============================================================================
// Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use cendb_cenql::ast::{CenqlPipeline, CenqlStage, Expr, BinaryOp};

    // ---- helpers ----

    fn col(name: &str) -> Expr {
        Expr::Column(name.to_string())
    }
    fn i64_lit(v: i64) -> Expr {
        Expr::I64(v)
    }
    fn bin(op: BinaryOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary { op, lhs: Box::new(l), rhs: Box::new(r) }
    }

    fn make_provider() -> InMemoryProvider {
        let mut p = InMemoryProvider::new();
        p.register_i64("users", &["id", "age", "country"], &[
            &[1, 30, 0],   // country code 0 = US
            &[2, 25, 1],   // 1 = DE
            &[3, 40, 0],
            &[4, 17, 1],
            &[5, 65, 0],
        ]);
        p.register_i64("orders", &["order_id", "user_id", "total"], &[
            &[100, 1, 50],
            &[101, 2, 75],
            &[102, 1, 200],
            &[103, 3, 30],
            &[104, 5, 500],
        ]);
        p
    }

    // ---- FROM subquery tests ----

    #[test]
    fn from_subquery_basic() {
        // from (from users | filter users.age > 18)
        let inner = CenqlPipeline::new(vec![
            CenqlStage::From { name: "users".to_string() },
            CenqlStage::Filter {
                expr: bin(BinaryOp::Gt, col("age"), i64_lit(18)),
            },
        ]);
        let outer = CenqlPipeline::new(vec![
            CenqlStage::FromSubquery { pipeline: Box::new(inner) },
        ]);
        let provider = make_provider();
        let rows = execute_pipeline(&outer, &provider).unwrap();
        // users with age > 18: ids 1, 2, 3, 5 (4 rows; id=4 has age=17).
        assert_eq!(rows.len(), 4);
        let ids: Vec<i64> = rows.iter().filter_map(|r| match r.first() {
            Some(Value::I64(v)) => Some(*v),
            _ => None,
        }).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(ids.contains(&3));
        assert!(ids.contains(&5));
        assert!(!ids.contains(&4));
    }

    #[test]
    fn from_subquery_with_filter_on_outer() {
        // from (from users | filter users.age > 18) | filter users.age < 60
        //   -> age in (19, 60), i.e. ids 1, 2, 3.
        let inner = CenqlPipeline::new(vec![
            CenqlStage::From { name: "users".to_string() },
            CenqlStage::Filter {
                expr: bin(BinaryOp::Gt, col("age"), i64_lit(18)),
            },
        ]);
        let outer = CenqlPipeline::new(vec![
            CenqlStage::FromSubquery { pipeline: Box::new(inner) },
            CenqlStage::Filter {
                expr: bin(BinaryOp::Lt, col("age"), i64_lit(60)),
            },
        ]);
        let provider = make_provider();
        let rows = execute_pipeline(&outer, &provider).unwrap();
        // ids 1, 2, 3 (id=5 has age=65, filtered out by outer).
        assert_eq!(rows.len(), 3);
        let ids: Vec<i64> = rows.iter().filter_map(|r| match r.first() {
            Some(Value::I64(v)) => Some(*v),
            _ => None,
        }).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(ids.contains(&3));
        assert!(!ids.contains(&5));
    }

    #[test]
    fn from_subquery_with_take_on_outer() {
        // from (from users) | take 2
        let inner = CenqlPipeline::new(vec![
            CenqlStage::From { name: "users".to_string() },
        ]);
        let outer = CenqlPipeline::new(vec![
            CenqlStage::FromSubquery { pipeline: Box::new(inner) },
            CenqlStage::Take { n: 2 },
        ]);
        let provider = make_provider();
        let rows = execute_pipeline(&outer, &provider).unwrap();
        assert_eq!(rows.len(), 2);
    }

    // ---- IN subquery tests ----

    #[test]
    fn in_subquery_basic() {
        // from orders | filter user_id in (from users | filter users.age > 30 | select { id })
        //   -> users with age > 30 are ids 3, 5.
        //   -> orders with user_id in {3, 5} are order 103 (user 3) and 104 (user 5).
        let sub = CenqlPipeline::new(vec![
            CenqlStage::From { name: "users".to_string() },
            CenqlStage::Filter {
                expr: bin(BinaryOp::Gt, col("age"), i64_lit(30)),
            },
        ]);
        let outer = CenqlPipeline::new(vec![
            CenqlStage::From { name: "orders".to_string() },
            CenqlStage::Filter {
                expr: Expr::In {
                    value: Box::new(col("user_id")),
                    subquery: Box::new(sub),
                },
            },
        ]);
        let provider = make_provider();
        let rows = execute_pipeline(&outer, &provider).unwrap();
        // Order_ids: 103 (user 3), 104 (user 5).
        assert_eq!(rows.len(), 2);
        let order_ids: Vec<i64> = rows.iter().filter_map(|r| match r.first() {
            Some(Value::I64(v)) => Some(*v),
            _ => None,
        }).collect();
        assert!(order_ids.contains(&103));
        assert!(order_ids.contains(&104));
    }

    #[test]
    fn in_subquery_empty_result() {
        // Subquery returns no rows -> outer filter eliminates all rows.
        let sub = CenqlPipeline::new(vec![
            CenqlStage::From { name: "users".to_string() },
            CenqlStage::Filter {
                expr: bin(BinaryOp::Gt, col("age"), i64_lit(1000)),
            },
        ]);
        let outer = CenqlPipeline::new(vec![
            CenqlStage::From { name: "orders".to_string() },
            CenqlStage::Filter {
                expr: Expr::In {
                    value: Box::new(col("user_id")),
                    subquery: Box::new(sub),
                },
            },
        ]);
        let provider = make_provider();
        let rows = execute_pipeline(&outer, &provider).unwrap();
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn in_subquery_no_match() {
        // Subquery returns rows whose values don't match any outer row.
        // users.age > 60 -> id=5 only. orders.user_id == 5 -> order 104.
        // But we filter on user_id IN (SELECT id FROM users WHERE age > 65)
        //   -> empty (no users have age > 65 except id=5 with age=65, which is not > 65).
        // Actually wait — id=5 has age=65, so >65 is empty.
        let sub = CenqlPipeline::new(vec![
            CenqlStage::From { name: "users".to_string() },
            CenqlStage::Filter {
                expr: bin(BinaryOp::Gt, col("age"), i64_lit(65)),
            },
        ]);
        let outer = CenqlPipeline::new(vec![
            CenqlStage::From { name: "orders".to_string() },
            CenqlStage::Filter {
                expr: Expr::In {
                    value: Box::new(col("user_id")),
                    subquery: Box::new(sub),
                },
            },
        ]);
        let provider = make_provider();
        let rows = execute_pipeline(&outer, &provider).unwrap();
        assert_eq!(rows.len(), 0);
    }

    // ---- Nested subquery tests ----

    #[test]
    fn nested_subquery_subquery_within_subquery() {
        // Outer: from ( from users | filter id in ( from orders | filter total > 100 | select { user_id } ) )
        //   -> orders with total > 100: order 102 (user 1), 104 (user 5).
        //   -> select { user_id } produces [{1}, {5}].
        //   -> users with id in {1, 5}: ids 1, 5.
        let inner_sub = CenqlPipeline::new(vec![
            CenqlStage::From { name: "orders".to_string() },
            CenqlStage::Filter {
                expr: bin(BinaryOp::Gt, col("total"), i64_lit(100)),
            },
            CenqlStage::Select { columns: vec!["user_id".to_string()] },
        ]);
        let middle = CenqlPipeline::new(vec![
            CenqlStage::From { name: "users".to_string() },
            CenqlStage::Filter {
                expr: Expr::In {
                    value: Box::new(col("id")),
                    subquery: Box::new(inner_sub),
                },
            },
        ]);
        let outer = CenqlPipeline::new(vec![
            CenqlStage::FromSubquery { pipeline: Box::new(middle) },
        ]);
        let provider = make_provider();
        let rows = execute_pipeline(&outer, &provider).unwrap();
        // Should be users 1 and 5.
        assert_eq!(rows.len(), 2);
        let ids: Vec<i64> = rows.iter().filter_map(|r| match r.first() {
            Some(Value::I64(v)) => Some(*v),
            _ => None,
        }).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&5));
    }

    // ---- Provider error tests ----

    #[test]
    fn from_unknown_table_returns_error() {
        let pipeline = CenqlPipeline::new(vec![
            CenqlStage::From { name: "nonexistent".to_string() },
        ]);
        let provider = make_provider();
        let result = execute_pipeline(&pipeline, &provider);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.status, CenStatus::ErrNotFound);
    }

    // ---- Sort + take on subquery result ----

    #[test]
    fn from_subquery_with_sort_and_take() {
        // from (from users) | sort id desc | take 2
        //   -> users sorted desc by id: 5, 4, 3, 2, 1
        //   -> take 2: ids 5, 4
        let inner = CenqlPipeline::new(vec![
            CenqlStage::From { name: "users".to_string() },
        ]);
        let outer = CenqlPipeline::new(vec![
            CenqlStage::FromSubquery { pipeline: Box::new(inner) },
            CenqlStage::Sort { column: "id".to_string(), dir: SortDir::Desc },
            CenqlStage::Take { n: 2 },
        ]);
        let provider = make_provider();
        let rows = execute_pipeline(&outer, &provider).unwrap();
        assert_eq!(rows.len(), 2);
        // First row should have id=5, second should have id=4.
        assert_eq!(rows[0][0], Value::I64(5));
        assert_eq!(rows[1][0], Value::I64(4));
    }
}
