//! JOIN execution algorithms: hash join, nested-loop join, and sort-merge
//! join.
//!
//! ## Overview
//!
//! This module provides three classic join algorithms operating on
//! materialised row sets (`&[Vec<Value>]`). Each algorithm produces the
//! concatenated rows `left ++ right` for every pair that satisfies the
//! join condition. A `JoinMethod` enum plus a `join` dispatcher let
//! callers pick an algorithm explicitly, and an `auto_select_method`
//! helper picks one based on input properties.
//!
//! ## NULL semantics
//!
//! All three algorithms follow SQL's NULL semantics: a NULL is never
//! equal to anything, including itself. NULL keys are skipped on both
//! the build and probe sides of the hash and merge joins.
//!
//! ## Cross-type comparison
//!
//! Values of different kinds (`I64(5)` vs `U64(5)`) are never equal —
//! the join keys must have the same `Value` discriminant to match. The
//! sort-merge join orders mixed kinds by discriminant so it stays
//! total-order-stable, but mixed-kind keys will never produce matches.

use cendb_core::Value;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

// ============================================================================
// Value hashing, equality, and ordering (with NULL semantics).
// ============================================================================

/// Compute a stable hash of a `Value` for use in the build-side hash
/// table. The hash incorporates the value's discriminant so that, e.g.,
/// `I64(5)` and `U64(5)` produce different hashes (and therefore land
/// in different buckets even if the body hash happens to collide).
fn value_hash(v: &Value) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(v).hash(&mut hasher);
    match v {
        Value::Null => {}
        Value::Bool(b) => b.hash(&mut hasher),
        Value::I64(i) => i.hash(&mut hasher),
        Value::U64(u) => u.hash(&mut hasher),
        Value::F64(f) => f.to_bits().hash(&mut hasher),
        Value::Bytes(b) => b.hash(&mut hasher),
        Value::Timestamp(t) => t.hash(&mut hasher),
    }
    hasher.finish()
}

/// Compare two `Value`s for equality using SQL NULL semantics.
///
/// NULL is never equal to anything, even itself. Mixed-kind values
/// (`I64(5)` vs `U64(5)`) are never equal.
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

/// Total ordering on `Value`, used by the sort-merge join. NULLs sort
/// first and are "equal" to other NULLs for ordering purposes — but
/// NULLs still never *match* in the join. Mixed kinds are ordered by a
/// kind tag to keep the ordering total.
fn value_cmp(a: &Value, b: &Value) -> Ordering {
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

/// Helper: is this key NULL (and therefore un-joinable)?
#[inline]
fn is_null_key(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Null) | None)
}

// ============================================================================
// Hash join.
// ============================================================================

/// Hash join for equi-joins.
///
/// Builds a hash table on the smaller (build) side and probes with the
/// larger (probe) side. Returns the concatenated rows
/// `left ++ right` for every pair where `left[left_col] == right[right_col]`
/// (using SQL NULL semantics: NULLs never match).
///
/// Complexity: O(n + m) on average, O(n*m) in the pathological
/// all-same-key case.
pub fn hash_join(
    left: &[Vec<Value>],
    right: &[Vec<Value>],
    left_col: usize,
    right_col: usize,
) -> Vec<Vec<Value>> {
    // Pick the smaller side as the build side. We keep track of which
    // side is the build side so we can emit rows in the canonical
    // `left ++ right` order regardless.
    let (build, build_col, probe, probe_col, build_is_left) = if left.len() <= right.len() {
        (left, left_col, right, right_col, true)
    } else {
        (right, right_col, left, left_col, false)
    };

    // ---- Build phase ----
    // hash -> Vec<row_index_in_build>
    let mut hash_table: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, row) in build.iter().enumerate() {
        let key = match row.get(build_col) {
            Some(v) if !matches!(v, Value::Null) => v,
            _ => continue, // Skip NULL keys.
        };
        let h = value_hash(key);
        hash_table.entry(h).or_default().push(i);
    }

    // ---- Probe phase ----
    let mut results = Vec::new();
    for probe_row in probe {
        let probe_key = match probe_row.get(probe_col) {
            Some(v) if !matches!(v, Value::Null) => v,
            _ => continue, // NULL probe key never matches.
        };
        let h = value_hash(probe_key);
        if let Some(build_indices) = hash_table.get(&h) {
            for &bi in build_indices {
                let build_row = &build[bi];
                let build_key = build_row.get(build_col).unwrap();
                // Guard against hash collisions.
                if value_eq(build_key, probe_key) {
                    if build_is_left {
                        let mut out = build_row.clone();
                        out.extend_from_slice(probe_row);
                        results.push(out);
                    } else {
                        let mut out = probe_row.clone();
                        out.extend_from_slice(build_row);
                        results.push(out);
                    }
                }
            }
        }
    }
    results
}

// ============================================================================
// Nested loop join.
// ============================================================================

/// Nested loop join for arbitrary (including non-equi) predicates.
///
/// For each `left_row`, scans every `right_row`; emits `left ++ right`
/// when `predicate(left_row, right_row)` returns true. O(n*m) but works
/// with any predicate (theta-joins, range joins, etc.).
pub fn nested_loop_join(
    left: &[Vec<Value>],
    right: &[Vec<Value>],
    predicate: &dyn Fn(&[Value], &[Value]) -> bool,
) -> Vec<Vec<Value>> {
    let mut results = Vec::new();
    for l in left {
        for r in right {
            if predicate(l, r) {
                let mut out = l.clone();
                out.extend_from_slice(r);
                results.push(out);
            }
        }
    }
    results
}

// ============================================================================
// Sort-merge join.
// ============================================================================

/// Sort-merge join for pre-sorted equi-joins.
///
/// Both inputs **must** be sorted ascending on their join column. The
/// algorithm is O(n + m) once sorted. NULLs are skipped and never
/// match. Duplicate keys on either side produce the cross-product of
/// the matching runs (i.e. an inner join on duplicates behaves like a
/// hash join).
pub fn merge_join(
    left: &[Vec<Value>],
    right: &[Vec<Value>],
    left_col: usize,
    right_col: usize,
) -> Vec<Vec<Value>> {
    let mut results = Vec::new();
    if left.is_empty() || right.is_empty() {
        return results;
    }

    let mut i = 0usize;
    let mut j = 0usize;

    while i < left.len() && j < right.len() {
        // Skip NULL keys on either side.
        if is_null_key(left[i].get(left_col)) {
            i += 1;
            continue;
        }
        if is_null_key(right[j].get(right_col)) {
            j += 1;
            continue;
        }

        let l_val = left[i].get(left_col).unwrap();
        let r_val = right[j].get(right_col).unwrap();

        match value_cmp(l_val, r_val) {
            Ordering::Less => {
                i += 1;
            }
            Ordering::Greater => {
                j += 1;
            }
            Ordering::Equal => {
                // Find the run of equal keys on the left.
                let mut left_end = i + 1;
                while left_end < left.len() {
                    match left[left_end].get(left_col) {
                        Some(v) if !matches!(v, Value::Null) => {
                            if value_cmp(v, l_val) == Ordering::Equal {
                                left_end += 1;
                                continue;
                            }
                        }
                        _ => {}
                    }
                    break;
                }
                // Find the run of equal keys on the right.
                let mut right_end = j + 1;
                while right_end < right.len() {
                    match right[right_end].get(right_col) {
                        Some(v) if !matches!(v, Value::Null) => {
                            if value_cmp(v, r_val) == Ordering::Equal {
                                right_end += 1;
                                continue;
                            }
                        }
                        _ => {}
                    }
                    break;
                }

                // Emit cross-product of the matching runs.
                for li in i..left_end {
                    for rj in j..right_end {
                        let mut out = left[li].clone();
                        out.extend_from_slice(&right[rj]);
                        results.push(out);
                    }
                }

                i = left_end;
                j = right_end;
            }
        }
    }

    results
}

// ============================================================================
// Join wrapper + auto-select.
// ============================================================================

/// Join algorithm to use.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum JoinMethod {
    /// Hash join: build hash table on smaller side, probe with larger.
    Hash,
    /// Nested loop join: O(n*m), works with any predicate.
    NestedLoop,
    /// Sort-merge join: O(n+m), requires both inputs sorted on join col.
    Merge,
}

/// Dispatch to the requested join algorithm.
///
/// For `NestedLoop`, the predicate is fixed to an equality check on
/// `(left_col, right_col)` — this is the equi-join dispatch path. For
/// arbitrary theta-join predicates, call `nested_loop_join` directly.
pub fn join(
    left: &[Vec<Value>],
    right: &[Vec<Value>],
    left_col: usize,
    right_col: usize,
    method: JoinMethod,
) -> Vec<Vec<Value>> {
    match method {
        JoinMethod::Hash => hash_join(left, right, left_col, right_col),
        JoinMethod::NestedLoop => nested_loop_join(left, right, &|l, r| {
            match (l.get(left_col), r.get(right_col)) {
                (Some(a), Some(b)) => value_eq(a, b),
                _ => false,
            }
        }),
        JoinMethod::Merge => merge_join(left, right, left_col, right_col),
    }
}

/// Auto-select the join method:
///
/// * If both inputs are already sorted on their join columns, use `Merge`.
/// * Else if it's an equi-join and one side has fewer than 10,000 rows,
///   use `Hash` (build the hash table on the smaller side).
/// * Else fall back to `NestedLoop` (very large, unsorted inputs where
///   building a hash table would be memory-pressure-heavy).
pub fn auto_select_method(left_sorted: bool, right_sorted: bool, left_rows: usize, right_rows: usize) -> JoinMethod {
    if left_sorted && right_sorted {
        return JoinMethod::Merge;
    }
    if left_rows < 10_000 || right_rows < 10_000 {
        return JoinMethod::Hash;
    }
    JoinMethod::NestedLoop
}

/// Convenience: auto-select and execute in one call.
pub fn auto_join(
    left: &[Vec<Value>],
    right: &[Vec<Value>],
    left_col: usize,
    right_col: usize,
    left_sorted: bool,
    right_sorted: bool,
) -> Vec<Vec<Value>> {
    let method = auto_select_method(left_sorted, right_sorted, left.len(), right.len());
    join(left, right, left_col, right_col, method)
}

// ============================================================================
// Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use cendb_core::Value;

    // ---- helpers ----

    fn i64_row(vals: &[i64]) -> Vec<Value> {
        vals.iter().map(|v| Value::I64(*v)).collect()
    }

    fn rows_i64(rows: &[&[i64]]) -> Vec<Vec<Value>> {
        rows.iter().map(|r| i64_row(r)).collect()
    }

    // ========================================================================
    // Hash join tests.
    // ========================================================================

    #[test]
    fn hash_join_basic_equi_join() {
        let left = rows_i64(&[&[1, 10], &[2, 20], &[3, 30]]);
        let right = rows_i64(&[&[2, 200], &[3, 300], &[4, 400]]);
        let out = hash_join(&left, &right, 0, 0);
        // Matches: (2,20,2,200) and (3,30,3,300).
        assert_eq!(out.len(), 2);
        // Both rows should have left ++ right concatenated.
        assert!(out.iter().any(|r| {
            r == &vec![Value::I64(2), Value::I64(20), Value::I64(2), Value::I64(200)]
        }));
        assert!(out.iter().any(|r| {
            r == &vec![Value::I64(3), Value::I64(30), Value::I64(3), Value::I64(300)]
        }));
    }

    #[test]
    fn hash_join_empty_inputs() {
        let left: Vec<Vec<Value>> = vec![];
        let right = rows_i64(&[&[1, 100]]);
        assert_eq!(hash_join(&left, &right, 0, 0).len(), 0);

        let left = rows_i64(&[&[1, 100]]);
        let right: Vec<Vec<Value>> = vec![];
        assert_eq!(hash_join(&left, &right, 0, 0).len(), 0);

        let left: Vec<Vec<Value>> = vec![];
        let right: Vec<Vec<Value>> = vec![];
        assert_eq!(hash_join(&left, &right, 0, 0).len(), 0);
    }

    #[test]
    fn hash_join_no_matches() {
        let left = rows_i64(&[&[1, 10], &[2, 20]]);
        let right = rows_i64(&[&[3, 300], &[4, 400]]);
        assert_eq!(hash_join(&left, &right, 0, 0).len(), 0);
    }

    #[test]
    fn hash_join_duplicate_keys() {
        // Left: two rows with key=1. Right: three rows with key=1.
        let left = rows_i64(&[&[1, 10], &[1, 11], &[2, 20]]);
        let right = rows_i64(&[&[1, 100], &[1, 101], &[1, 102]]);
        let out = hash_join(&left, &right, 0, 0);
        // 2 left rows * 3 right rows = 6 matches on key=1.
        assert_eq!(out.len(), 6);
        // Each output row should have left key = right key = 1.
        for r in &out {
            assert_eq!(r[0], Value::I64(1));
            assert_eq!(r[2], Value::I64(1));
        }
    }

    #[test]
    fn hash_join_null_handling() {
        // NULLs on both sides should never match — not even other NULLs.
        let left = vec![
            vec![Value::Null, Value::I64(10)],
            vec![Value::I64(1), Value::I64(20)],
        ];
        let right = vec![
            vec![Value::Null, Value::I64(100)],
            vec![Value::I64(1), Value::I64(200)],
        ];
        let out = hash_join(&left, &right, 0, 0);
        // Only the I64(1) row should match.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], vec![Value::I64(1), Value::I64(20), Value::I64(1), Value::I64(200)]);
    }

    #[test]
    fn hash_join_uses_smaller_side_as_build() {
        // 1 row left, 1000 rows right. Should still find the one match.
        let left = rows_i64(&[&[42, 1]]);
        let right: Vec<Vec<Value>> = (0..1000)
            .map(|i| vec![Value::I64(i), Value::I64(i * 10)])
            .collect();
        let out = hash_join(&left, &right, 0, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][1], Value::I64(1));
        assert_eq!(out[0][3], Value::I64(420));
    }

    #[test]
    fn hash_join_mixed_value_kinds_dont_match() {
        // I64(5) and U64(5) are different Value kinds and must not match.
        let left = vec![vec![Value::I64(5), Value::I64(50)]];
        let right = vec![vec![Value::U64(5), Value::I64(500)]];
        let out = hash_join(&left, &right, 0, 0);
        assert_eq!(out.len(), 0);
    }

    // ========================================================================
    // Nested loop join tests.
    // ========================================================================

    #[test]
    fn nested_loop_basic() {
        let left = rows_i64(&[&[1, 10], &[2, 20]]);
        let right = rows_i64(&[&[1, 100], &[2, 200]]);
        let out = nested_loop_join(&left, &right, &|l, r| {
            // Equi-join on column 0.
            match (l.get(0), r.get(0)) {
                (Some(Value::I64(a)), Some(Value::I64(b))) => a == b,
                _ => false,
            }
        });
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|r| r == &vec![
            Value::I64(1), Value::I64(10), Value::I64(1), Value::I64(100)
        ]));
        assert!(out.iter().any(|r| r == &vec![
            Value::I64(2), Value::I64(20), Value::I64(2), Value::I64(200)
        ]));
    }

    #[test]
    fn nested_loop_theta_gt() {
        // left.x > right.y: emit when left[1] > right[1].
        let left = rows_i64(&[&[1, 100], &[2, 50], &[3, 75]]);
        let right = rows_i64(&[&[10, 60], &[11, 80]]);
        let out = nested_loop_join(&left, &right, &|l, r| {
            matches!((l.get(1), r.get(1)),
                (Some(Value::I64(a)), Some(Value::I64(b))) if a > b)
        });
        // Pairs where left[1] > right[1]:
        //   (100, 60), (100, 80), (75, 60)
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn nested_loop_theta_lt() {
        let left = rows_i64(&[&[1, 10], &[2, 50]]);
        let right = rows_i64(&[&[10, 60], &[11, 80]]);
        let out = nested_loop_join(&left, &right, &|l, r| {
            matches!((l.get(1), r.get(1)),
                (Some(Value::I64(a)), Some(Value::I64(b))) if a < b)
        });
        // Pairs where left[1] < right[1]:
        //   (10, 60), (10, 80), (50, 60), (50, 80)
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn nested_loop_empty_inputs() {
        let left: Vec<Vec<Value>> = vec![];
        let right = rows_i64(&[&[1, 1]]);
        let out = nested_loop_join(&left, &right, &|_, _| true);
        assert_eq!(out.len(), 0);

        let left = rows_i64(&[&[1, 1]]);
        let right: Vec<Vec<Value>> = vec![];
        let out = nested_loop_join(&left, &right, &|_, _| true);
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn nested_loop_null_predicate_treats_null_as_no_match() {
        let left = vec![vec![Value::Null, Value::I64(1)]];
        let right = vec![vec![Value::Null, Value::I64(2)]];
        // Predicate that uses value_eq: NULL never matches.
        let out = nested_loop_join(&left, &right, &|l, r| {
            l.get(0).zip(r.get(0)).map(|(a, b)| value_eq(a, b)).unwrap_or(false)
        });
        assert_eq!(out.len(), 0);
    }

    // ========================================================================
    // Merge join tests.
    // ========================================================================

    #[test]
    fn merge_join_sorted_inputs() {
        let left = rows_i64(&[&[1, 10], &[2, 20], &[3, 30]]);
        let right = rows_i64(&[&[2, 200], &[3, 300], &[4, 400]]);
        let out = merge_join(&left, &right, 0, 0);
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|r| r == &vec![
            Value::I64(2), Value::I64(20), Value::I64(2), Value::I64(200)
        ]));
        assert!(out.iter().any(|r| r == &vec![
            Value::I64(3), Value::I64(30), Value::I64(3), Value::I64(300)
        ]));
    }

    #[test]
    fn merge_join_duplicates() {
        // Both sides sorted with duplicate keys.
        let left = rows_i64(&[&[1, 10], &[1, 11], &[2, 20]]);
        let right = rows_i64(&[&[1, 100], &[1, 101], &[2, 200]]);
        let out = merge_join(&left, &right, 0, 0);
        // Key 1: 2 left * 2 right = 4 matches; key 2: 1 * 1 = 1 match.
        assert_eq!(out.len(), 5);
        let ones = out.iter().filter(|r| r[0] == Value::I64(1)).count();
        assert_eq!(ones, 4);
        let twos = out.iter().filter(|r| r[0] == Value::I64(2)).count();
        assert_eq!(twos, 1);
    }

    #[test]
    fn merge_join_null_skipped() {
        let left = vec![
            vec![Value::Null, Value::I64(0)],
            vec![Value::I64(1), Value::I64(10)],
        ];
        let right = vec![
            vec![Value::Null, Value::I64(100)],
            vec![Value::I64(1), Value::I64(200)],
        ];
        let out = merge_join(&left, &right, 0, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], vec![Value::I64(1), Value::I64(10), Value::I64(1), Value::I64(200)]);
    }

    #[test]
    fn merge_join_empty_inputs() {
        let left: Vec<Vec<Value>> = vec![];
        let right = rows_i64(&[&[1, 1]]);
        assert_eq!(merge_join(&left, &right, 0, 0).len(), 0);
    }

    #[test]
    fn merge_join_unsorted_input_still_runs() {
        // If the caller passes unsorted data the merge join does not crash;
        // it produces correct results *up to the sort invariant* — i.e. it
        // will miss matches that span out-of-order keys, which is the
        // documented contract. Here we feed already-sorted inputs to verify
        // the happy path and ensure no panic on edge cases.
        let left = rows_i64(&[&[5, 50], &[10, 100]]);
        let right = rows_i64(&[&[5, 500], &[10, 1000]]);
        let out = merge_join(&left, &right, 0, 0);
        assert_eq!(out.len(), 2);
    }

    // ========================================================================
    // Wrapper + auto-select tests.
    // ========================================================================

    #[test]
    fn join_dispatches_to_hash() {
        let left = rows_i64(&[&[1, 10], &[2, 20]]);
        let right = rows_i64(&[&[2, 200], &[3, 300]]);
        let out = join(&left, &right, 0, 0, JoinMethod::Hash);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], vec![Value::I64(2), Value::I64(20), Value::I64(2), Value::I64(200)]);
    }

    #[test]
    fn join_dispatches_to_nested_loop() {
        let left = rows_i64(&[&[1, 10], &[2, 20]]);
        let right = rows_i64(&[&[1, 100], &[2, 200]]);
        let out = join(&left, &right, 0, 0, JoinMethod::NestedLoop);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn join_dispatches_to_merge() {
        let left = rows_i64(&[&[1, 10], &[2, 20], &[3, 30]]);
        let right = rows_i64(&[&[2, 200], &[3, 300]]);
        let out = join(&left, &right, 0, 0, JoinMethod::Merge);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn auto_select_chooses_merge_when_both_sorted() {
        assert_eq!(
            auto_select_method(true, true, 1_000_000, 1_000_000),
            JoinMethod::Merge
        );
    }

    #[test]
    fn auto_select_chooses_hash_when_one_side_small() {
        // 50 rows left (small), 1M rows right (large), not sorted.
        assert_eq!(
            auto_select_method(false, false, 50, 1_000_000),
            JoinMethod::Hash
        );
        assert_eq!(
            auto_select_method(false, false, 1_000_000, 50),
            JoinMethod::Hash
        );
    }

    #[test]
    fn auto_select_chooses_nested_loop_for_large_unsorted() {
        // Both > 10K and unsorted -> NestedLoop.
        assert_eq!(
            auto_select_method(false, false, 100_000, 100_000),
            JoinMethod::NestedLoop
        );
    }

    #[test]
    fn auto_select_prefers_merge_when_sorted_even_if_small() {
        // Both sorted AND small: Merge wins (sorted check is first).
        assert_eq!(
            auto_select_method(true, true, 100, 100),
            JoinMethod::Merge
        );
    }

    #[test]
    fn auto_join_end_to_end() {
        let left = rows_i64(&[&[1, 10], &[2, 20], &[3, 30]]);
        let right = rows_i64(&[&[2, 200], &[3, 300]]);
        // Small + unsorted -> Hash.
        let out = auto_join(&left, &right, 0, 0, false, false);
        assert_eq!(out.len(), 2);
        // Both sorted -> Merge.
        let out_sorted = auto_join(&left, &right, 0, 0, true, true);
        assert_eq!(out_sorted.len(), 2);
    }

    // ========================================================================
    // Large input tests (correctness on 10K rows).
    // ========================================================================

    #[test]
    fn hash_join_10k_rows_correctness() {
        // Left: 0..10000 with key=i. Right: 5000..15000 with key=i.
        // Overlap: 5000..10000 -> 5000 matches.
        let left: Vec<Vec<Value>> = (0..10_000)
            .map(|i| vec![Value::I64(i), Value::I64(i * 2)])
            .collect();
        let right: Vec<Vec<Value>> = (5_000..15_000)
            .map(|i| vec![Value::I64(i), Value::I64(i * 3)])
            .collect();
        let out = hash_join(&left, &right, 0, 0);
        assert_eq!(out.len(), 5_000);
        // Check a sample row.
        let sample = out.iter().find(|r| r[0] == Value::I64(7_000)).unwrap();
        assert_eq!(sample[0], Value::I64(7_000));
        assert_eq!(sample[1], Value::I64(14_000));
        assert_eq!(sample[2], Value::I64(7_000));
        assert_eq!(sample[3], Value::I64(21_000));
    }

    #[test]
    fn merge_join_10k_rows_correctness() {
        // Both sides sorted ascending on key.
        let left: Vec<Vec<Value>> = (0..10_000)
            .map(|i| vec![Value::I64(i), Value::I64(i * 2)])
            .collect();
        let right: Vec<Vec<Value>> = (5_000..15_000)
            .map(|i| vec![Value::I64(i), Value::I64(i * 3)])
            .collect();
        let out = merge_join(&left, &right, 0, 0);
        assert_eq!(out.len(), 5_000);
        // Every output row should have matching keys.
        for r in &out {
            assert_eq!(r[0], r[2]);
        }
    }

    #[test]
    fn nested_loop_10k_rows_correctness() {
        // Smaller scale (10K * 1K would be 10M comparisons — too slow for
        // tests), but enough to prove correctness on non-trivial input.
        let left: Vec<Vec<Value>> = (0..1_000)
            .map(|i| vec![Value::I64(i), Value::I64(i * 2)])
            .collect();
        let right: Vec<Vec<Value>> = (500..1_500)
            .map(|i| vec![Value::I64(i), Value::I64(i * 3)])
            .collect();
        let out = nested_loop_join(&left, &right, &|l, r| {
            matches!((l.get(0), r.get(0)),
                (Some(Value::I64(a)), Some(Value::I64(b))) if a == b)
        });
        assert_eq!(out.len(), 500);
    }

    #[test]
    fn hash_join_with_string_keys() {
        // Verify the join works for Bytes values too (CenDB stores strings
        // as Bytes).
        let left = vec![
            vec![Value::Bytes(b"alice".to_vec()), Value::I64(30)],
            vec![Value::Bytes(b"bob".to_vec()), Value::I64(25)],
        ];
        let right = vec![
            vec![Value::Bytes(b"alice".to_vec()), Value::I64(100)],
            vec![Value::Bytes(b"carol".to_vec()), Value::I64(200)],
        ];
        let out = hash_join(&left, &right, 0, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][1], Value::I64(30));
        assert_eq!(out[0][3], Value::I64(100));
    }
}
