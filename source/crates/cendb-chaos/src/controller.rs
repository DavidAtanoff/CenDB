//! Chaos controller: configures fault injection points.

use std::collections::HashMap;

/// Type of fault to inject.
#[derive(Clone, Debug)]
pub enum FaultType {
    /// Return an I/O error at the specified operation index.
    IoError,
    /// Write only the first `bytes` bytes of the data, then fail.
    /// Simulates a torn page write.
    TornWrite { bytes: usize },
    /// Pretend fsync succeeded but don't actually persist the data.
    /// The in-memory copy is updated, but a subsequent "crash" (re-open)
    /// will reveal the data was never durable.
    SilentFsyncFail,
    /// Flip `byte_index` bits in the data after writing.
    /// Simulates silent data corruption.
    CorruptData { byte_index: usize },
}

/// Configuration for a single fault injection point.
#[derive(Clone, Debug)]
pub struct FaultConfig {
    /// The operation index at which to trigger the fault (0-based).
    pub at_op: u64,
    /// The type of fault.
    pub fault: FaultType,
    /// Only trigger on this file path (None = any file).
    pub path_filter: Option<String>,
}

impl FaultConfig {
    pub fn io_error(at_op: u64) -> Self {
        Self { at_op, fault: FaultType::IoError, path_filter: None }
    }

    pub fn torn_write(at_op: u64, bytes: usize) -> Self {
        Self { at_op, fault: FaultType::TornWrite { bytes }, path_filter: None }
    }

    pub fn silent_fsync(at_op: u64) -> Self {
        Self { at_op, fault: FaultType::SilentFsyncFail, path_filter: None }
    }

    pub fn corrupt(at_op: u64, byte_index: usize) -> Self {
        Self { at_op, fault: FaultType::CorruptData { byte_index }, path_filter: None }
    }
}

/// The chaos controller. Holds a list of scheduled faults and tracks the
/// current operation counter.
pub struct ChaosController {
    /// Scheduled faults, keyed by operation index.
    faults: HashMap<u64, Vec<FaultConfig>>,
    /// Current operation counter (incremented on every I/O op).
    op_counter: u64,
    /// Whether fault injection is currently enabled.
    enabled: bool,
    /// Operations that have already fired (to prevent double-firing).
    fired: std::collections::HashSet<u64>,
}

impl ChaosController {
    pub fn new() -> Self {
        Self {
            faults: HashMap::new(),
            op_counter: 0,
            enabled: true,
            fired: std::collections::HashSet::new(),
        }
    }

    /// Schedule a fault at a specific operation index.
    pub fn schedule(&mut self, config: FaultConfig) {
        self.faults.entry(config.at_op).or_default().push(config);
    }

    /// Schedule an I/O error at the given operation.
    pub fn fail_at(&mut self, op: u64) {
        self.schedule(FaultConfig::io_error(op));
    }

    /// Schedule a torn write at the given operation.
    pub fn torn_write_at(&mut self, op: u64, bytes: usize) {
        self.schedule(FaultConfig::torn_write(op, bytes));
    }

    /// Schedule a silent fsync failure at the given operation.
    pub fn silent_fsync_at(&mut self, op: u64) {
        self.schedule(FaultConfig::silent_fsync(op));
    }

    /// Schedule data corruption at the given operation.
    pub fn corrupt_at(&mut self, op: u64, byte_index: usize) {
        self.schedule(FaultConfig::corrupt(op, byte_index));
    }

    /// Disable fault injection (all ops pass through).
    pub fn disable(&mut self) {
        self.enabled = false;
    }

    /// Enable fault injection.
    pub fn enable(&mut self) {
        self.enabled = true;
    }

    /// Get the next operation index and increment the counter.
    pub fn next_op(&mut self) -> u64 {
        let op = self.op_counter;
        self.op_counter += 1;
        op
    }

    /// Check if a fault should fire at the current operation index.
    /// Returns the fault configs to apply, if any.
    pub fn check_faults(&mut self, op: u64, path: &str) -> Vec<FaultType> {
        if !self.enabled {
            return Vec::new();
        }
        if self.fired.contains(&op) {
            return Vec::new();
        }
        if let Some(configs) = self.faults.get(&op) {
            self.fired.insert(op);
            configs
                .iter()
                .filter(|c| {
                    c.path_filter.as_ref().map_or(true, |p| path.contains(p))
                })
                .map(|c| c.fault.clone())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Current operation counter.
    pub fn op_count(&self) -> u64 {
        self.op_counter
    }

    /// Reset the controller (clears all scheduled faults and counters).
    pub fn reset(&mut self) {
        self.faults.clear();
        self.op_counter = 0;
        self.fired.clear();
        self.enabled = true;
    }
}

impl Default for ChaosController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_schedules_faults() {
        let mut ctrl = ChaosController::new();
        ctrl.fail_at(5);
        ctrl.torn_write_at(10, 1024);

        let faults = ctrl.check_faults(5, "test");
        assert_eq!(faults.len(), 1);
        assert!(matches!(faults[0], FaultType::IoError));

        // Second check should not fire again.
        let faults = ctrl.check_faults(5, "test");
        assert!(faults.is_empty());

        let faults = ctrl.check_faults(10, "test");
        assert_eq!(faults.len(), 1);
        assert!(matches!(faults[0], FaultType::TornWrite { bytes: 1024 }));
    }

    #[test]
    fn controller_disabled_passes_through() {
        let mut ctrl = ChaosController::new();
        ctrl.fail_at(1);
        ctrl.disable();
        let faults = ctrl.check_faults(1, "test");
        assert!(faults.is_empty());
    }

    #[test]
    fn controller_path_filter() {
        let mut ctrl = ChaosController::new();
        let mut config = FaultConfig::io_error(1);
        config.path_filter = Some("wal".to_string());
        ctrl.schedule(config);

        // Should NOT fire for "segment.cdb" (path doesn't contain "wal").
        let faults = ctrl.check_faults(1, "segment.cdb");
        assert!(faults.is_empty(), "should not fire for non-matching path");

        // Should fire for "wal.cdb" (path contains "wal").
        // But we already fired at op 1 above... need to reset.
        ctrl.reset();
        let mut config2 = FaultConfig::io_error(1);
        config2.path_filter = Some("wal".to_string());
        ctrl.schedule(config2);
        let faults = ctrl.check_faults(1, "wal.cdb");
        assert!(!faults.is_empty(), "should fire for matching path");
    }
}
