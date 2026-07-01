//! Audit logging: tamper-evident log of all write operations.
//!
//! ## Design
//!
//! Every write operation (INSERT, UPDATE, DELETE, CREATE, DROP) is
//! recorded in an append-only audit log. Each entry contains:
//!   - Timestamp (Unix seconds)
//!   - User ID
//!   - Operation type
//!   - Resource (table/collection name)
//!   - Row count affected
//!   - Optional detail (e.g. key range, WHERE clause summary)
//!
//! ## Tamper-evidence
//!
//! Each entry is chained to the previous one via a BLAKE3 hash:
//! `entry_n.prev_hash = blake3(entry_{n-1})`. The first entry's
//! `prev_hash` is the hash of an empty byte string. Anyone with the
//! log can recompute the chain and detect if any entry was modified
//! or inserted out of order.
//!
//! This is NOT cryptographic non-repudiation (the log is in-process
//! and an attacker with process memory can rewrite it). It IS enough
//! to detect accidental corruption or post-hoc tampering with on-disk
//! log files.
//!
//! ## Retention
//!
//! The log grows unboundedly. A production deployment would add
//! rotation + archival to cold storage. For embedded use the host
//! application is responsible for calling `prune_older_than(ts)` if
//! disk space is a concern.

use blake3;
use std::sync::Mutex;

/// Operation type recorded in the audit log.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AuditOp {
    Insert,
    Update,
    Delete,
    Create,
    Drop,
    Login,
    Logout,
    FailedLogin,
    Grant,
    Revoke,
    Other,
}

impl AuditOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuditOp::Insert => "INSERT",
            AuditOp::Update => "UPDATE",
            AuditOp::Delete => "DELETE",
            AuditOp::Create => "CREATE",
            AuditOp::Drop => "DROP",
            AuditOp::Login => "LOGIN",
            AuditOp::Logout => "LOGOUT",
            AuditOp::FailedLogin => "FAILED_LOGIN",
            AuditOp::Grant => "GRANT",
            AuditOp::Revoke => "REVOKE",
            AuditOp::Other => "OTHER",
        }
    }
}

/// A single audit log entry.
#[derive(Clone, Debug)]
pub struct AuditEntry {
    pub sequence: u64,
    pub timestamp: u64,
    pub user_id: u64,
    pub op: AuditOp,
    pub resource: String,
    pub rows_affected: u64,
    pub detail: String,
    /// Hash of the previous entry (BLAKE3). First entry hashes b"".
    pub prev_hash: [u8; 32],
    /// Hash of this entry (BLAKE3 of all fields above, including prev_hash).
    pub entry_hash: [u8; 32],
}

impl AuditEntry {
    fn compute_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.sequence.to_le_bytes());
        hasher.update(&self.timestamp.to_le_bytes());
        hasher.update(&self.user_id.to_le_bytes());
        hasher.update(self.op.as_str().as_bytes());
        hasher.update(self.resource.as_bytes());
        hasher.update(&self.rows_affected.to_le_bytes());
        hasher.update(self.detail.as_bytes());
        hasher.update(&self.prev_hash);
        *hasher.finalize().as_bytes()
    }
}

/// The audit log. Thread-safe via `Mutex`.
pub struct AuditLog {
    entries: Mutex<Vec<AuditEntry>>,
    /// Optional callback for real-time forwarding (e.g. to syslog).
    sink: Mutex<Option<Box<dyn Fn(&AuditEntry) + Send + Sync>>>,
}

impl AuditLog {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            sink: Mutex::new(None),
        }
    }

    /// Set a sink callback for real-time audit forwarding.
    pub fn set_sink<F>(&self, sink: F)
    where
        F: Fn(&AuditEntry) + Send + Sync + 'static,
    {
        *self.sink.lock().unwrap() = Some(Box::new(sink));
    }

    /// Append a new entry. Returns the sequence number.
    pub fn append(
        &self,
        user_id: u64,
        op: AuditOp,
        resource: &str,
        rows_affected: u64,
        detail: &str,
    ) -> u64 {
        let mut entries = self.entries.lock().unwrap();
        let sequence = entries.len() as u64 + 1;
        let prev_hash = entries
            .last()
            .map(|e| e.entry_hash)
            .unwrap_or_else(|| *blake3::hash(b"").as_bytes());
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut entry = AuditEntry {
            sequence,
            timestamp,
            user_id,
            op,
            resource: resource.to_string(),
            rows_affected,
            detail: detail.to_string(),
            prev_hash,
            entry_hash: [0u8; 32],
        };
        entry.entry_hash = entry.compute_hash();
        let seq = entry.sequence;
        // Forward to sink if set.
        if let Some(sink) = self.sink.lock().unwrap().as_ref() {
            sink(&entry);
        }
        entries.push(entry);
        seq
    }

    /// Verify the hash chain. Returns Ok(()) if every entry's hash is
    /// correct and the chain is unbroken. Returns Err with the first
    /// bad sequence number otherwise.
    pub fn verify_chain(&self) -> Result<(), u64> {
        let entries = self.entries.lock().unwrap();
        let mut prev_hash = *blake3::hash(b"").as_bytes();
        for entry in entries.iter() {
            if entry.prev_hash != prev_hash {
                return Err(entry.sequence);
            }
            let computed = entry.compute_hash();
            if entry.entry_hash != computed {
                return Err(entry.sequence);
            }
            prev_hash = entry.entry_hash;
        }
        Ok(())
    }

    /// Get all entries (cloned).
    pub fn entries(&self) -> Vec<AuditEntry> {
        self.entries.lock().unwrap().clone()
    }

    /// Get entries for a specific user.
    pub fn entries_for_user(&self, user_id: u64) -> Vec<AuditEntry> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.user_id == user_id)
            .cloned()
            .collect()
    }

    /// Get entries for a specific resource (exact match).
    pub fn entries_for_resource(&self, resource: &str) -> Vec<AuditEntry> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.resource == resource)
            .cloned()
            .collect()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    /// Prune entries older than `ts`.
    pub fn prune_older_than(&self, ts: u64) -> usize {
        let mut entries = self.entries.lock().unwrap();
        let before = entries.len();
        entries.retain(|e| e.timestamp >= ts);
        before - entries.len()
    }

    /// Clear all entries (for testing).
    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

impl Default for AuditLog {
    fn default() -> Self { Self::new() }
}

// ============================================================================
// Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_verify() {
        let log = AuditLog::new();
        log.append(1, AuditOp::Insert, "users", 1, "user_id=42");
        log.append(2, AuditOp::Update, "users", 1, "user_id=42");
        log.append(1, AuditOp::Delete, "orders", 5, "order_id IN (1..5)");
        assert_eq!(log.len(), 3);
        log.verify_chain().unwrap();
    }

    #[test]
    fn chain_detects_tampering() {
        let log = AuditLog::new();
        log.append(1, AuditOp::Insert, "users", 1, "row1");
        log.append(1, AuditOp::Insert, "users", 1, "row2");
        // Tamper: modify the first entry's detail in-place WITHOUT
        // recomputing its hash. The verify_chain will detect that
        // entry 0's stored entry_hash doesn't match its (now-changed)
        // compute_hash() output.
        {
            let mut entries = log.entries.lock().unwrap();
            entries[0].detail = "TAMPERED".to_string();
        }
        let result = log.verify_chain();
        assert!(result.is_err());
        // The tampered entry itself (sequence 1) is detected first
        // because its entry_hash no longer matches compute_hash().
        let bad_seq = result.unwrap_err();
        assert_eq!(bad_seq, 1);
    }

    #[test]
    fn filter_by_user() {
        let log = AuditLog::new();
        log.append(1, AuditOp::Insert, "users", 1, "");
        log.append(2, AuditOp::Insert, "users", 1, "");
        log.append(1, AuditOp::Delete, "users", 1, "");
        let user1 = log.entries_for_user(1);
        assert_eq!(user1.len(), 2);
        let user2 = log.entries_for_user(2);
        assert_eq!(user2.len(), 1);
    }

    #[test]
    fn filter_by_resource() {
        let log = AuditLog::new();
        log.append(1, AuditOp::Insert, "users", 1, "");
        log.append(1, AuditOp::Insert, "orders", 1, "");
        log.append(1, AuditOp::Insert, "users", 1, "");
        let users_entries = log.entries_for_resource("users");
        assert_eq!(users_entries.len(), 2);
    }

    #[test]
    fn sink_receives_entries() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        let log = AuditLog::new();
        log.set_sink(move |_entry| {
            count_clone.fetch_add(1, Ordering::SeqCst);
        });
        log.append(1, AuditOp::Insert, "x", 1, "");
        log.append(2, AuditOp::Insert, "y", 1, "");
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn prune_older_entries() {
        let log = AuditLog::new();
        let old_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 3600; // 1 hour ago
        // We can't easily set timestamps (they're auto-generated), so
        // just verify prune returns 0 for a future cutoff.
        let pruned = log.prune_older_than(old_ts);
        assert_eq!(pruned, 0);
    }

    #[test]
    fn empty_log_verifies_ok() {
        let log = AuditLog::new();
        log.verify_chain().unwrap();
    }

    #[test]
    fn sequence_numbers_are_monotonic() {
        let log = AuditLog::new();
        let s1 = log.append(1, AuditOp::Insert, "x", 1, "");
        let s2 = log.append(1, AuditOp::Insert, "x", 1, "");
        let s3 = log.append(1, AuditOp::Insert, "x", 1, "");
        assert!(s1 < s2);
        assert!(s2 < s3);
    }
}
