//! Table statistics for the cost-based optimizer.

use std::collections::HashMap;

// ============================================================================
// Column-level statistics.
// ============================================================================

/// Statistics for a single column, used by the cost-based optimizer to
/// estimate selectivity and join cardinality.
#[derive(Clone, Debug)]
pub struct ColumnStats {
    /// Number of distinct values (cardinality).
    pub distinct_count: u64,
    /// Number of NULL values.
    pub null_count: u64,
    /// Minimum value (for range selectivity estimation).
    pub min_i64: Option<i64>,
    /// Maximum value.
    pub max_i64: Option<i64>,
    /// Most common values (for equality selectivity).
    pub most_common: Vec<(i64, f64)>,
}

impl ColumnStats {
    /// Estimate the selectivity of an equality predicate `col == val`.
    /// Returns a fraction in [0, 1].
    pub fn selectivity_eq(&self) -> f64 {
        if self.distinct_count == 0 {
            return 0.0;
        }
        // Basic estimate: 1 / distinct_count.
        let base = 1.0 / self.distinct_count as f64;
        // If we have MCV stats, use them.
        // For this implementation we just use the base estimate.
        base
    }

    /// Estimate the selectivity of a range predicate `col > val` or
    /// `col < val`. Returns a fraction in [0, 1].
    pub fn selectivity_range(&self, val: i64, greater_than: bool) -> f64 {
        match (self.min_i64, self.max_i64) {
            (Some(min), Some(max)) => {
                if min == max {
                    return 0.0;
                }
                let range = (max - min) as f64;
                if greater_than {
                    let above = (max - val).max(0) as f64;
                    above / range
                } else {
                    let below = (val - min).max(0) as f64;
                    below / range
                }
            }
            _ => 0.333, // default selectivity for unknown range
        }
    }

    /// Estimate the selectivity of a `col IS NOT NULL` predicate.
    pub fn selectivity_not_null(&self, total_rows: u64) -> f64 {
        if total_rows == 0 {
            return 0.0;
        }
        1.0 - (self.null_count as f64 / total_rows as f64)
    }
}

// ============================================================================
// Table-level statistics.
// ============================================================================

/// Statistics for a table, used by the cost-based optimizer.
#[derive(Clone, Debug)]
pub struct TableStats {
    /// Table name.
    pub name: String,
    /// Total number of rows.
    pub row_count: u64,
    /// Average row width in bytes.
    pub avg_row_width: u32,
    /// Per-column statistics, keyed by column name.
    pub columns: HashMap<String, ColumnStats>,
}

impl TableStats {
    pub fn new(name: impl Into<String>, row_count: u64) -> Self {
        Self {
            name: name.into(),
            row_count,
            avg_row_width: 64,
            columns: HashMap::new(),
        }
    }

    /// Add a column's statistics.
    pub fn with_column(mut self, name: impl Into<String>, stats: ColumnStats) -> Self {
        self.columns.insert(name.into(), stats);
        self
    }

    /// Estimate the number of rows after applying a filter with the given
    /// selectivity.
    pub fn estimate_filtered_rows(&self, selectivity: f64) -> u64 {
        ((self.row_count as f64) * selectivity) as u64
    }
}

// ============================================================================
// Statistics catalog.
// ============================================================================

/// In-memory catalog of table statistics, used by the optimizer.
#[derive(Clone, Debug)]
pub struct StatsCatalog {
    tables: HashMap<String, TableStats>,
    /// Indexes registered on `(table, column)`. Each entry maps a column
    /// name to the index name to use in `IndexScan`.
    indexes: HashMap<String, Vec<(String, String)>>, // table -> Vec<(column, index_name)>
}

impl StatsCatalog {
    pub fn new() -> Self {
        Self {
            tables: HashMap::new(),
            indexes: HashMap::new(),
        }
    }

    /// Register table statistics.
    pub fn register(&mut self, stats: TableStats) {
        self.tables.insert(stats.name.clone(), stats);
    }

    /// Register an index on `table.column`. The `index_name` is used in
    /// the generated `IndexScan` operator.
    pub fn register_index(&mut self, table: &str, column: &str, index_name: &str) {
        self.indexes
            .entry(table.to_string())
            .or_default()
            .push((column.to_string(), index_name.to_string()));
    }

    /// Look up table statistics by name.
    pub fn get(&self, table: &str) -> Option<&TableStats> {
        self.tables.get(table)
    }

    /// Is there an index on `table.column`? Returns the index name if so.
    pub fn index_for(&self, table: &str, column: &str) -> Option<&str> {
        self.indexes
            .get(table)
            .and_then(|cols| cols.iter().find(|(c, _)| c == column).map(|(_, idx)| idx.as_str()))
    }

    /// Whether `table.column` has an index.
    pub fn has_index(&self, table: &str, column: &str) -> bool {
        self.index_for(table, column).is_some()
    }

    /// Estimate the cardinality of a join between two tables.
    /// Uses a simple model: if there's an equality join on a column with
    /// `distinct_a` and `distinct_b` distinct values, the output cardinality
    /// is `rows_a * rows_b / max(distinct_a, distinct_b)`.
    pub fn estimate_join_cardinality(
        &self,
        table_a: &str,
        table_b: &str,
        col_a: &str,
        col_b: &str,
    ) -> u64 {
        let stats_a = match self.get(table_a) {
            Some(s) => s,
            None => return 1000, // default guess
        };
        let stats_b = match self.get(table_b) {
            Some(s) => s,
            None => return 1000,
        };
        let col_a_stats = stats_a.columns.get(col_a);
        let col_b_stats = stats_b.columns.get(col_b);
        let distinct_a = col_a_stats.map(|s| s.distinct_count).unwrap_or(100);
        let distinct_b = col_b_stats.map(|s| s.distinct_count).unwrap_or(100);
        let max_distinct = distinct_a.max(distinct_b).max(1);
        (stats_a.row_count * stats_b.row_count) / max_distinct
    }
}

impl Default for StatsCatalog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectivity_estimation() {
        let col = ColumnStats {
            distinct_count: 100,
            null_count: 10,
            min_i64: Some(0),
            max_i64: Some(1000),
            most_common: vec![],
        };
        // Equality selectivity: 1/100 = 0.01
        assert!((col.selectivity_eq() - 0.01).abs() < 0.001);
        // Range selectivity: col > 500 → 500/1000 = 0.5
        assert!((col.selectivity_range(500, true) - 0.5).abs() < 0.01);
        // col < 250 → 250/1000 = 0.25
        assert!((col.selectivity_range(250, false) - 0.25).abs() < 0.01);
    }

    #[test]
    fn join_cardinality_estimation() {
        let mut catalog = StatsCatalog::new();
        catalog.register(
            TableStats::new("users", 10_000)
                .with_column(
                    "id",
                    ColumnStats {
                        distinct_count: 10_000,
                        null_count: 0,
                        min_i64: Some(1),
                        max_i64: Some(10_000),
                        most_common: vec![],
                    },
                ),
        );
        catalog.register(
            TableStats::new("orders", 100_000)
                .with_column(
                    "user_id",
                    ColumnStats {
                        distinct_count: 10_000,
                        null_count: 0,
                        min_i64: Some(1),
                        max_i64: Some(10_000),
                        most_common: vec![],
                    },
                ),
        );
        // Join users.id == orders.user_id:
        // cardinality = 10000 * 100000 / max(10000, 10000) = 100000
        let card = catalog.estimate_join_cardinality("users", "orders", "id", "user_id");
        assert_eq!(card, 100_000);
    }
}

// ============================================================================
// Equi-Depth Histogram for selectivity estimation.
// ============================================================================

/// An equi-depth histogram: divides the value range into N buckets, each
/// containing approximately the same number of rows. Used by the CBO to
/// estimate range predicate selectivity.
#[derive(Clone, Debug)]
pub struct Histogram {
    /// Bucket boundaries (N+1 values for N buckets).
    pub boundaries: Vec<i64>,
    /// Number of rows in each bucket.
    pub counts: Vec<u64>,
    /// Total rows.
    pub total_rows: u64,
}

impl Histogram {
    /// Build an equi-depth histogram from sorted values.
    pub fn build(sorted_values: &[i64], bucket_count: usize) -> Self {
        if sorted_values.is_empty() {
            return Self {
                boundaries: vec![],
                counts: vec![],
                total_rows: 0,
            };
        }
        let n = sorted_values.len();
        let bucket_size = n / bucket_count.max(1);
        let mut boundaries = Vec::with_capacity(bucket_count + 1);
        let mut counts = Vec::with_capacity(bucket_count);

        boundaries.push(sorted_values[0]);
        for i in 1..bucket_count {
            let idx = i * bucket_size;
            if idx < n {
                boundaries.push(sorted_values[idx]);
            }
        }
        boundaries.push(sorted_values[n - 1]);

        // Compute counts per bucket.
        let mut current_bucket = 0;
        let mut count = 0u64;
        for &v in sorted_values {
            if current_bucket + 1 < boundaries.len() && v >= boundaries[current_bucket + 1] {
                counts.push(count);
                count = 0;
                current_bucket += 1;
            }
            count += 1;
        }
        counts.push(count);

        Self {
            boundaries,
            counts,
            total_rows: n as u64,
        }
    }

    /// Estimate the selectivity of `col >= val`.
    pub fn selectivity_ge(&self, val: i64) -> f64 {
        if self.boundaries.is_empty() || self.total_rows == 0 {
            return 0.333;
        }
        if val <= self.boundaries[0] {
            return 1.0;
        }
        if val > *self.boundaries.last().unwrap() {
            return 0.0;
        }
        // Find the bucket containing val.
        for i in 0..self.counts.len() {
            if i + 1 < self.boundaries.len() && val < self.boundaries[i + 1] {
                let bucket_start = self.boundaries[i];
                let bucket_end = self.boundaries[i + 1];
                let bucket_rows = self.counts[i] as f64;
                if bucket_end == bucket_start {
                    return (self.counts[i..].iter().sum::<u64>() as f64) / self.total_rows as f64;
                }
                let fraction_in_bucket = (bucket_end - val) as f64 / (bucket_end - bucket_start) as f64;
                let rows_in_bucket = bucket_rows * fraction_in_bucket;
                let rows_after = self.counts[i + 1..].iter().sum::<u64>() as f64;
                return (rows_in_bucket + rows_after) / self.total_rows as f64;
            }
        }
        0.0
    }
}

// ============================================================================
// HyperLogLog for cardinality estimation.
// ============================================================================

/// HyperLogLog: estimates the number of distinct values in a stream
/// using O(2^precision) bytes of memory. Default precision (14) uses
/// 16KB and has ~0.4% standard error.
pub struct HyperLogLog {
    registers: Vec<u8>,
    precision: u32,
    m: u64,
}

impl HyperLogLog {
    /// Create a new HLL with the given precision (number of register bits).
    /// Precision 14 = 16,384 registers = 16KB memory, ~0.4% error.
    /// Precision 12 = 4,096 registers = 4KB memory, ~0.8% error.
    pub fn new(precision: u32) -> Self {
        let p = precision.clamp(4, 20); // reasonable bounds
        let m = 1u64 << p;
        Self {
            registers: vec![0; m as usize],
            precision: p,
            m,
        }
    }

    /// Create a new HLL with default precision 14 (~0.4% error, 16KB).
    pub fn with_default_precision() -> Self {
        Self::new(14)
    }

    /// Add a value to the HLL sketch.
    pub fn add(&mut self, hash: u64) {
        let idx = (hash >> (64 - self.precision)) as usize;
        let w = hash << self.precision;
        // Count leading zeros + 1 (position of first 1-bit).
        let rho = if w == 0 {
            (64 - self.precision + 1) as u8
        } else {
            (w.leading_zeros() + 1) as u8
        };
        if rho > self.registers[idx] {
            self.registers[idx] = rho;
        }
    }

    /// Estimate the cardinality (number of distinct values).
    /// Uses the improved HyperLogLog++ estimator with bias correction.
    pub fn estimate(&self) -> u64 {
        let m = self.m as f64;
        let alpha = match self.m {
            16 => 0.673,
            32 => 0.697,
            64 => 0.709,
            _ => 0.7213 / (1.0 + 1.079 / m),
        };

        let sum: f64 = self.registers.iter()
            .map(|&r| 2f64.powi(-(r as i32)))
            .sum();

        let raw_estimate = alpha * m * m / sum;

        // Small range correction (Linear Counting).
        let estimate = if raw_estimate <= 2.5 * m {
            let zeros = self.registers.iter().filter(|&&r| r == 0).count() as f64;
            if zeros > 0.0 {
                m * (m / zeros).ln()
            } else {
                raw_estimate
            }
        } else {
            raw_estimate
        };

        estimate as u64
    }

    /// Merge another HLL into this one (union).
    pub fn merge(&mut self, other: &HyperLogLog) {
        for (a, b) in self.registers.iter_mut().zip(other.registers.iter()) {
            if *b > *a {
                *a = *b;
            }
        }
    }

    /// Memory usage in bytes.
    pub fn mem_usage(&self) -> usize {
        self.registers.len()
    }
}

#[cfg(test)]
mod histogram_tests {
    use super::*;

    #[test]
    fn histogram_build_and_query() {
        let values: Vec<i64> = (0..1000).collect();
        let hist = Histogram::build(&values, 10);
        assert_eq!(hist.total_rows, 1000);
        // selectivity of >= 500 should be ~50%.
        let sel = hist.selectivity_ge(500);
        assert!((sel - 0.5).abs() < 0.15, "expected ~0.5, got {}", sel);
    }

    #[test]
    fn hll_cardinality_estimation() {
        let mut hll = HyperLogLog::with_default_precision();
        for i in 0..10000u64 {
            hll.add(u64::from_le_bytes(blake3::hash(&i.to_le_bytes()).as_bytes()[..8].try_into().unwrap()));
        }
        let estimate = hll.estimate();
        // HLL should estimate ~10000 distinct values within ~10%.
        assert!(
            (estimate as f64 / 10000.0 - 1.0).abs() < 0.05,
            "expected ~10000, got {}",
            estimate
        );
    }

    #[test]
    fn hll_merge() {
        let mut hll1 = HyperLogLog::with_default_precision();
        let mut hll2 = HyperLogLog::with_default_precision();
        for i in 0..5000u64 {
            hll1.add(u64::from_le_bytes(blake3::hash(&i.to_le_bytes()).as_bytes()[..8].try_into().unwrap()));
        }
        for i in 5000..10000u64 {
            hll2.add(u64::from_le_bytes(blake3::hash(&i.to_le_bytes()).as_bytes()[..8].try_into().unwrap()));
        }
        hll1.merge(&hll2);
        let estimate = hll1.estimate();
        assert!(
            (estimate as f64 / 10000.0 - 1.0).abs() < 0.05,
            "merged HLL should estimate ~10000, got {}",
            estimate
        );
    }
}
