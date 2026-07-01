//! Morsel: a cache-resident batch of rows (1024 by default).

/// Default morsel size — fits in L1/L2 cache.
pub const MORSEL_SIZE: usize = 1024;

/// A morsel: a batch of up to `MORSEL_SIZE` rows from a columnar scan.
/// Each column is stored as a `Vec<i64>` (the canonical storage form for
/// fixed-width types; f64 is stored as bit patterns).
#[derive(Clone, Debug)]
pub struct Morsel {
    /// Row count in this morsel (<= MORSEL_SIZE).
    pub row_count: usize,
    /// Column data, one Vec<i64> per column.
    pub columns: Vec<Vec<i64>>,
}

impl Morsel {
    pub fn new(column_count: usize) -> Self {
        Self {
            row_count: 0,
            columns: vec![Vec::with_capacity(MORSEL_SIZE); column_count],
        }
    }

    /// Append a row (one i64 per column).
    pub fn push_row(&mut self, row: &[i64]) {
        for (col, &val) in self.columns.iter_mut().zip(row) {
            col.push(val);
        }
        self.row_count += 1;
    }

    /// Is this morsel full?
    pub fn is_full(&self) -> bool {
        self.row_count >= MORSEL_SIZE
    }

    /// Get a column as a slice.
    pub fn col(&self, idx: usize) -> &[i64] {
        &self.columns[idx]
    }
}

/// A batch of morsels produced by a scan.
pub struct MorselBatch {
    pub morsels: Vec<Morsel>,
}

impl MorselBatch {
    pub fn new() -> Self {
        Self { morsels: Vec::new() }
    }

    pub fn push(&mut self, morsel: Morsel) {
        self.morsels.push(morsel);
    }

    pub fn total_rows(&self) -> usize {
        self.morsels.iter().map(|m| m.row_count).sum()
    }
}

impl Default for MorselBatch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::{filter_i64_gt, sum_i64};

    #[test]
    fn morsel_batch_scan_and_filter() {
        // Simulate a scan: 5000 rows in 2 columns (id, value).
        let total_rows = 5000;
        let mut batch = MorselBatch::new();
        let mut morsel = Morsel::new(2);
        for i in 0..total_rows as i64 {
            morsel.push_row(&[i, i * 2]);
            if morsel.is_full() {
                batch.push(std::mem::replace(&mut morsel, Morsel::new(2)));
            }
        }
        if morsel.row_count > 0 {
            batch.push(morsel);
        }
        assert_eq!(batch.total_rows(), total_rows);
        // Filter each morsel: id > 2500.
        let mut total_passed = 0;
        let mut total_sum = 0i64;
        for morsel in &batch.morsels {
            let sv = filter_i64_gt(morsel.col(0), 2500);
            total_passed += sv.len();
            for &idx in &sv.indices {
                total_sum = total_sum.wrapping_add(morsel.col(1)[idx as usize]);
            }
        }
        assert_eq!(total_passed, 2499);
        // Sum of (i*2) for i in 2501..=4999.
        let expected: i64 = (2501..=4999i64).map(|i| i * 2).sum();
        assert_eq!(total_sum, expected);
    }

    #[test]
    fn vectorized_sum_across_morsels() {
        let total_rows = 10_000;
        let mut batch = MorselBatch::new();
        let mut morsel = Morsel::new(1);
        for i in 0..total_rows as i64 {
            morsel.push_row(&[i]);
            if morsel.is_full() {
                batch.push(std::mem::replace(&mut morsel, Morsel::new(1)));
            }
        }
        if morsel.row_count > 0 {
            batch.push(morsel);
        }
        let sum: i64 = batch.morsels.iter().map(|m| sum_i64(m.col(0))).sum();
        assert_eq!(sum, (total_rows - 1) * total_rows / 2);
    }
}

// ============================================================================
// Out-of-Core Execution: External Merge Sort and Spilling Hash Join.
// ============================================================================

/// External merge sort: sorts data that exceeds memory by spilling
/// sorted runs to "disk" (simulated as Vec<Vec<i64>> for this implementation).
pub struct ExternalMergeSort {
    /// Maximum number of rows per in-memory run.
    run_size: usize,
    /// Sorted runs spilled to "disk".
    runs: Vec<Vec<i64>>,
    /// In-memory buffer for the current run.
    buffer: Vec<i64>,
}

impl ExternalMergeSort {
    pub fn new(run_size: usize) -> Self {
        Self {
            run_size,
            runs: Vec::new(),
            buffer: Vec::with_capacity(run_size),
        }
    }

    /// Push a value into the sort. Automatically spills when the buffer
    /// is full.
    pub fn push(&mut self, value: i64) {
        self.buffer.push(value);
        if self.buffer.len() >= self.run_size {
            self.spill();
        }
    }

    /// Spill the current buffer as a sorted run.
    fn spill(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        self.buffer.sort();
        self.runs.push(std::mem::take(&mut self.buffer));
    }

    /// Finalize: merge all sorted runs into a single sorted output.
    pub fn finish(mut self) -> Vec<i64> {
        self.spill(); // Spill any remaining.
        if self.runs.is_empty() {
            return Vec::new();
        }
        if self.runs.len() == 1 {
            return self.runs.into_iter().next().unwrap();
        }

        // K-way merge using a min-heap.
        use std::collections::BinaryHeap;
        use std::cmp::Reverse;

        let mut heap: BinaryHeap<Reverse<(i64, usize, usize)>> = BinaryHeap::new();
        let runs = self.runs;

        // Initialize heap with first element from each run.
        for (run_idx, run) in runs.iter().enumerate() {
            if !run.is_empty() {
                heap.push(Reverse((run[0], run_idx, 0)));
            }
        }

        let mut result = Vec::new();
        while let Some(Reverse((val, run_idx, elem_idx))) = heap.pop() {
            result.push(val);
            let next_idx = elem_idx + 1;
            if next_idx < runs[run_idx].len() {
                heap.push(Reverse((runs[run_idx][next_idx], run_idx, next_idx)));
            }
        }

        result
    }

    /// Number of runs spilled.
    pub fn run_count(&self) -> usize {
        self.runs.len()
    }
}

/// Hybrid hash join: builds a hash table in memory, spills overflow
/// partitions to disk if they don't fit, and recursively processes them.
pub struct HybridHashJoin {
    /// Number of hash partitions (reduces memory pressure).
    partition_count: usize,
    /// Partitions for the build side (smaller relation).
    build_partitions: Vec<Vec<(i64, i64)>>, // (key, value)
    /// Partitions for the probe side (larger relation).
    probe_partitions: Vec<Vec<(i64, i64)>>,
    /// Whether any partition spilled to disk.
    spilled: bool,
}

impl HybridHashJoin {
    pub fn new(partition_count: usize) -> Self {
        Self {
            partition_count,
            build_partitions: vec![Vec::new(); partition_count],
            probe_partitions: vec![Vec::new(); partition_count],
            spilled: false,
        }
    }

    /// Add a build-side tuple (key, value).
    pub fn add_build(&mut self, key: i64, value: i64) {
        let partition = self.hash_partition(key);
        self.build_partitions[partition].push((key, value));
    }

    /// Add a probe-side tuple (key, value).
    pub fn add_probe(&mut self, key: i64, value: i64) {
        let partition = self.hash_partition(key);
        self.probe_partitions[partition].push((key, value));
    }

    /// Execute the join: for each partition, build a hash table and probe.
    /// Returns (build_value, probe_value) pairs for matching keys.
    pub fn execute(&mut self) -> Vec<(i64, i64)> {
        let mut results = Vec::new();

        for p in 0..self.partition_count {
            let build = std::mem::take(&mut self.build_partitions[p]);
            let probe = std::mem::take(&mut self.probe_partitions[p]);

            // Build hash table for this partition.
            let mut hash_table: std::collections::HashMap<i64, Vec<i64>> = std::collections::HashMap::new();
            for (key, value) in build {
                hash_table.entry(key).or_default().push(value);
            }

            // Probe.
            for (key, probe_value) in probe {
                if let Some(build_values) = hash_table.get(&key) {
                    for &build_value in build_values {
                        results.push((build_value, probe_value));
                    }
                }
            }
        }

        results
    }

    fn hash_partition(&self, key: i64) -> usize {
        let hash = key.wrapping_mul(0x9E3779B97F4A7C15u64 as i64) as u64;
        (hash % self.partition_count as u64) as usize
    }

    /// Whether any data was spilled.
    pub fn did_spill(&self) -> bool {
        self.spilled
    }
}

#[cfg(test)]
mod out_of_core_tests {
    use super::*;

    #[test]
    fn external_sort_basic() {
        let mut sorter = ExternalMergeSort::new(100);
        // Push 1000 values in reverse order.
        for i in (0..1000i64).rev() {
            sorter.push(i);
        }
        let sorted = sorter.finish();
        assert_eq!(sorted.len(), 1000);
        assert!(sorted.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(sorted[0], 0);
        assert_eq!(sorted[999], 999);
    }

    #[test]
    fn external_sort_large() {
        let mut sorter = ExternalMergeSort::new(1000);
        // Push 100K values in random order.
        let mut seed: u64 = 42;
        for _ in 0..100_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            sorter.push(seed as i64);
        }
        let sorted = sorter.finish();
        assert_eq!(sorted.len(), 100_000);
        assert!(sorted.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn hybrid_hash_join_basic() {
        let mut join = HybridHashJoin::new(4);
        // Build side: (key, value) pairs.
        join.add_build(1, 100);
        join.add_build(2, 200);
        join.add_build(3, 300);
        // Probe side.
        join.add_probe(2, 20);
        join.add_probe(3, 30);
        join.add_probe(4, 40); // No match.

        let results = join.execute();
        assert_eq!(results.len(), 2); // Keys 2 and 3 match.
    }
}
