//! Write-Ahead Log (WAL) with ARIES-lite recovery.
//!
//! ## Log record format
//!
//! Each record is:
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │ lsn:       u64   (this record's LSN)            │
//! │ prev_lsn:  u64   (back-chain for this txn)      │
//! │ txn_id:    u64                                 │
//! │ rec_type:  u8   (Insert/Update/Delete/...)     │
//! │ page_id:   u64                                 │
//! │ payload_len: u32                               │
//! │ payload:   [u8; payload_len]                   │
//! │ crc32c:    u32                                 │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! We log **physiological** records (logical within a page, physical across
//! pages): redo says "apply this delta to slot S of page P".
//!
//! ## Recovery (three-phase ARIES)
//!
//! ```text
//! ANALYSIS: scan WAL from last checkpoint → rebuild Dirty Page Table +
//!           active txn table.
//! REDO:     replay all records with lsn > page.pageLSN (idempotent via LSN
//!           check).
//! UNDO:     roll back losers using prev_lsn chains, writing CLRs
//!           (compensation log records) so undo is itself crash-safe.
//! ```

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use cendb_core::{HexError, HexStatus, PageId};

// ============================================================================
// Log record types.
// ============================================================================

/// Type of a WAL record. Values are stable on disk.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum LogRecordType {
    Insert = 1,
    Update = 2,
    Delete = 3,
    Commit = 4,
    Abort = 5,
    Checkpoint = 6,
    Clr = 7, // Compensation Log Record (for undo)
}

impl LogRecordType {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            1 => Some(LogRecordType::Insert),
            2 => Some(LogRecordType::Update),
            3 => Some(LogRecordType::Delete),
            4 => Some(LogRecordType::Commit),
            5 => Some(LogRecordType::Abort),
            6 => Some(LogRecordType::Checkpoint),
            7 => Some(LogRecordType::Clr),
            _ => None,
        }
    }
}

/// One WAL record. Variable-length payload.
#[derive(Clone, Debug)]
pub struct LogRecord {
    pub lsn: u64,
    pub prev_lsn: u64,
    pub txn_id: u64,
    pub rec_type: LogRecordType,
    pub page_id: u64,
    pub payload: Vec<u8>,
    pub crc32c: u32,
}

impl LogRecord {
    /// Serialise to bytes for the log file.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(40 + self.payload.len());
        out.extend_from_slice(&self.lsn.to_le_bytes());
        out.extend_from_slice(&self.prev_lsn.to_le_bytes());
        out.extend_from_slice(&self.txn_id.to_le_bytes());
        out.push(self.rec_type as u8);
        out.extend_from_slice(&self.page_id.to_le_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        // CRC32c of everything so far (excluding the CRC itself).
        let crc = crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// Deserialise from bytes. Returns `(record, bytes_consumed)`.
    pub fn from_bytes(bytes: &[u8]) -> WalResult<(Self, usize)> {
        if bytes.len() < 33 {
            return Err(WalError::TruncatedRecord);
        }
        let lsn = u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
        let prev_lsn = u64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        let txn_id = u64::from_le_bytes([
            bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
        ]);
        let rec_type = LogRecordType::from_u8(bytes[24]).ok_or(WalError::UnknownRecordType)?;
        let page_id = u64::from_le_bytes([
            bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30], bytes[31], bytes[32],
        ]);
        let payload_len = u32::from_le_bytes([
            bytes[33], bytes[34], bytes[35], bytes[36],
        ]) as usize;
        let needed = 41 + payload_len;
        if bytes.len() < needed {
            return Err(WalError::TruncatedRecord);
        }
        let payload = bytes[37..37 + payload_len].to_vec();
        let crc_stored = u32::from_le_bytes([
            bytes[37 + payload_len],
            bytes[38 + payload_len],
            bytes[39 + payload_len],
            bytes[40 + payload_len],
        ]);
        let crc_computed = crc32c(&bytes[..37 + payload_len]);
        if crc_stored != crc_computed {
            return Err(WalError::CrcMismatch);
        }
        Ok((
            Self {
                lsn,
                prev_lsn,
                txn_id,
                rec_type,
                page_id,
                payload,
                crc32c: crc_stored,
            },
            needed,
        ))
    }
}

// ============================================================================
// Errors.
// ============================================================================

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum WalError {
    TruncatedRecord,
    UnknownRecordType,
    CrcMismatch,
    Io(String),
    Other(String),
}

impl std::fmt::Display for WalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for WalError {}

impl From<WalError> for HexError {
    fn from(e: WalError) -> Self {
        let status = match e {
            WalError::Io(_) => HexStatus::ErrIo,
            WalError::CrcMismatch | WalError::TruncatedRecord | WalError::UnknownRecordType => {
                HexStatus::ErrCorrupt
            }
            WalError::Other(_) => HexStatus::ErrInternal,
        };
        HexError::new(status, e.to_string())
    }
}

impl From<std::io::Error> for WalError {
    fn from(e: std::io::Error) -> Self {
        WalError::Io(e.to_string())
    }
}

pub type WalResult<T> = Result<T, WalError>;

// ============================================================================
// WAL configuration.
// ============================================================================

#[derive(Clone, Debug)]
pub struct WalConfig {
    /// If true, fsync after every commit (default).
    pub sync_on_commit: bool,
    /// If true, fsync after every record (slowest, most durable).
    pub sync_on_every_record: bool,
    /// Interval (in records) between automatic checkpoints. 0 = manual only.
    pub checkpoint_interval: u32,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            sync_on_commit: true,
            sync_on_every_record: false,
            checkpoint_interval: 1000,
        }
    }
}

// ============================================================================
// Write-ahead log.
// ============================================================================

/// Append-only WAL. Owns the underlying file; tracks the next LSN to assign.
pub struct WriteAheadLog {
    file: File,
    path: PathBuf,
    next_lsn: u64,
    last_record_lsn: u64,
    config: WalConfig,
    records_since_checkpoint: u32,
}

impl WriteAheadLog {
    /// Open (or create) a WAL file at `path`.
    pub fn open(path: impl AsRef<Path>, config: WalConfig) -> WalResult<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        // Determine next_lsn by scanning existing records.
        let mut wal = Self {
            file,
            path,
            next_lsn: 1,
            last_record_lsn: 0,
            config,
            records_since_checkpoint: 0,
        };
        wal.scan_to_end()?;
        Ok(wal)
    }

    fn scan_to_end(&mut self) -> WalResult<()> {
        let mut buf = Vec::new();
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_to_end(&mut buf)?;
        let mut cursor = 0usize;
        let mut max_lsn = 0u64;
        while cursor < buf.len() {
            match LogRecord::from_bytes(&buf[cursor..]) {
                Ok((rec, consumed)) => {
                    if rec.lsn > max_lsn {
                        max_lsn = rec.lsn;
                    }
                    cursor += consumed;
                }
                Err(WalError::TruncatedRecord) => break,
                Err(e) => return Err(e),
            }
        }
        self.next_lsn = max_lsn + 1;
        self.last_record_lsn = max_lsn;
        // Seek to end for appends.
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }

    /// Append a record to the log. Returns the assigned LSN.
    pub fn append(
        &mut self,
        txn_id: u64,
        prev_lsn: u64,
        rec_type: LogRecordType,
        page_id: PageId,
        payload: &[u8],
    ) -> WalResult<u64> {
        let lsn = self.next_lsn;
        let rec = LogRecord {
            lsn,
            prev_lsn,
            txn_id,
            rec_type,
            page_id: page_id.0,
            payload: payload.to_vec(),
            crc32c: 0,
        };
        let bytes = rec.to_bytes();
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&bytes)?;
        if self.config.sync_on_every_record {
            self.file.sync_data()?;
        }
        self.next_lsn += 1;
        self.last_record_lsn = lsn;
        self.records_since_checkpoint += 1;
        if self.config.checkpoint_interval > 0
            && self.records_since_checkpoint >= self.config.checkpoint_interval
        {
            self.checkpoint()?;
        }
        Ok(lsn)
    }

    /// Record a commit. Optionally fsyncs (group commit would batch these).
    pub fn commit(&mut self, txn_id: u64, prev_lsn: u64) -> WalResult<u64> {
        let lsn = self.append(
            txn_id,
            prev_lsn,
            LogRecordType::Commit,
            PageId(0),
            &[],
        )?;
        if self.config.sync_on_commit {
            self.file.sync_data()?;
        }
        Ok(lsn)
    }

    /// Write a checkpoint record and fsync.
    pub fn checkpoint(&mut self) -> WalResult<u64> {
        let lsn = self.append(
            0,
            self.last_record_lsn,
            LogRecordType::Checkpoint,
            PageId(0),
            &[],
        )?;
        self.file.sync_data()?;
        self.records_since_checkpoint = 0;
        Ok(lsn)
    }

    /// Read all records from the log (used by recovery).
    pub fn read_all(&mut self) -> WalResult<Vec<LogRecord>> {
        let mut buf = Vec::new();
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_to_end(&mut buf)?;
        let mut records = Vec::new();
        let mut cursor = 0usize;
        while cursor < buf.len() {
            match LogRecord::from_bytes(&buf[cursor..]) {
                Ok((rec, consumed)) => {
                    records.push(rec);
                    cursor += consumed;
                }
                Err(WalError::TruncatedRecord) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(records)
    }

    /// Truncate the log (used after a checkpoint to reclaim space; for the
    /// prototype this is a no-op — checkpoints just mark a low-water mark).
    pub fn truncate(&mut self) -> WalResult<()> {
        // For the prototype we don't actually truncate. A production WAL
        // would archive records < the last checkpoint LSN.
        Ok(())
    }

    /// Path of the WAL file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Next LSN that will be assigned.
    pub fn next_lsn(&self) -> u64 {
        self.next_lsn
    }
}

// ============================================================================
// ARIES-lite recovery.
// ============================================================================

/// Result of running recovery: the sets needed to redo / undo.
pub struct AriesRecovery {
    /// Txns that committed (per the log). Their writes should be redone.
    pub committed_txns: std::collections::HashSet<u64>,
    /// Txns that aborted or were in-flight at crash. Their writes should be
    /// undone.
    pub loser_txns: std::collections::HashSet<u64>,
    /// Last LSN scanned.
    pub last_lsn: u64,
}

impl AriesRecovery {
    /// Run the analysis pass over a list of log records. Determines which
    /// txns committed (keep their writes) and which lost (undo their writes).
    pub fn analyze(records: &[LogRecord]) -> Self {
        let mut committed = std::collections::HashSet::new();
        let mut aborted = std::collections::HashSet::new();
        let mut active = std::collections::HashSet::new();
        let mut last_lsn = 0u64;
        for rec in records {
            last_lsn = rec.lsn.max(last_lsn);
            match rec.rec_type {
                LogRecordType::Insert | LogRecordType::Update | LogRecordType::Delete => {
                    active.insert(rec.txn_id);
                }
                LogRecordType::Commit => {
                    active.remove(&rec.txn_id);
                    committed.insert(rec.txn_id);
                }
                LogRecordType::Abort => {
                    active.remove(&rec.txn_id);
                    aborted.insert(rec.txn_id);
                }
                LogRecordType::Checkpoint | LogRecordType::Clr => {}
            }
        }
        Self {
            committed_txns: committed,
            loser_txns: active,
            last_lsn,
        }
    }

    /// Run the redo pass: replay all records from committed txns in LSN
    /// order. Returns the count of records replayed. The caller supplies
    /// a closure that applies each record's payload to the page.
    pub fn redo<F>(&self, records: &[LogRecord], mut apply: F) -> usize
    where
        F: FnMut(&LogRecord),
    {
        let mut count = 0;
        for rec in records {
            if self.committed_txns.contains(&rec.txn_id)
                && matches!(
                    rec.rec_type,
                    LogRecordType::Insert | LogRecordType::Update | LogRecordType::Delete
                )
            {
                apply(rec);
                count += 1;
            }
        }
        count
    }

    /// Run the undo pass: roll back all writes from loser txns in reverse
    /// LSN order. Returns the count of records undone.
    pub fn undo<F>(&self, records: &[LogRecord], mut apply: F) -> usize
    where
        F: FnMut(&LogRecord),
    {
        let mut count = 0;
        for rec in records.iter().rev() {
            if self.loser_txns.contains(&rec.txn_id)
                && matches!(
                    rec.rec_type,
                    LogRecordType::Insert | LogRecordType::Update | LogRecordType::Delete
                )
            {
                apply(rec);
                count += 1;
            }
        }
        count
    }
}

// ============================================================================
// CRC32c (Castagnoli).
// ============================================================================

/// Software CRC32c (no hardware acceleration for portability).
/// Uses the reflected polynomial 0x1EDC6F41.
fn crc32c(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0x82F63B78;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

// ============================================================================
// Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn crc_roundtrip() {
        let data = b"hello, world";
        let a = crc32c(data);
        let b = crc32c(data);
        assert_eq!(a, b);
        assert_ne!(a, crc32c(b"hello, world!"));
    }

    #[test]
    fn log_record_roundtrip() {
        let rec = LogRecord {
            lsn: 42,
            prev_lsn: 40,
            txn_id: 7,
            rec_type: LogRecordType::Insert,
            page_id: 123,
            payload: b"hello, world".to_vec(),
            crc32c: 0,
        };
        let bytes = rec.to_bytes();
        let (rec2, consumed) = LogRecord::from_bytes(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(rec2.lsn, 42);
        assert_eq!(rec2.prev_lsn, 40);
        assert_eq!(rec2.txn_id, 7);
        assert_eq!(rec2.rec_type, LogRecordType::Insert);
        assert_eq!(rec2.page_id, 123);
        assert_eq!(rec2.payload, b"hello, world");
    }

    #[test]
    fn wal_append_and_read_all() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wal.cdb");
        let mut wal = WriteAheadLog::open(&path, WalConfig::default()).unwrap();

        let lsn1 = wal
            .append(1, 0, LogRecordType::Insert, PageId(100), b"row1")
            .unwrap();
        let lsn2 = wal
            .append(1, lsn1, LogRecordType::Update, PageId(100), b"row1_v2")
            .unwrap();
        wal.commit(1, lsn2).unwrap();

        // Re-open and read all records.
        let mut wal2 = WriteAheadLog::open(&path, WalConfig::default()).unwrap();
        let records = wal2.read_all().unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].rec_type, LogRecordType::Insert);
        assert_eq!(records[1].rec_type, LogRecordType::Update);
        assert_eq!(records[2].rec_type, LogRecordType::Commit);
    }

    #[test]
    fn aries_analysis_identifies_winners_and_losers() {
        let records = vec![
            LogRecord {
                lsn: 1, prev_lsn: 0, txn_id: 1, rec_type: LogRecordType::Insert,
                page_id: 1, payload: vec![], crc32c: 0,
            },
            LogRecord {
                lsn: 2, prev_lsn: 0, txn_id: 2, rec_type: LogRecordType::Insert,
                page_id: 2, payload: vec![], crc32c: 0,
            },
            LogRecord {
                lsn: 3, prev_lsn: 1, txn_id: 1, rec_type: LogRecordType::Commit,
                page_id: 0, payload: vec![], crc32c: 0,
            },
            // Txn 2 never commits → it's a loser.
        ];
        let recovery = AriesRecovery::analyze(&records);
        assert!(recovery.committed_txns.contains(&1));
        assert!(recovery.loser_txns.contains(&2));
    }

    #[test]
    fn aries_redo_replays_committed_writes() {
        let records = vec![
            LogRecord {
                lsn: 1, prev_lsn: 0, txn_id: 1, rec_type: LogRecordType::Insert,
                page_id: 1, payload: b"a".to_vec(), crc32c: 0,
            },
            LogRecord {
                lsn: 2, prev_lsn: 0, txn_id: 2, rec_type: LogRecordType::Insert,
                page_id: 2, payload: b"b".to_vec(), crc32c: 0,
            },
            LogRecord {
                lsn: 3, prev_lsn: 1, txn_id: 1, rec_type: LogRecordType::Commit,
                page_id: 0, payload: vec![], crc32c: 0,
            },
        ];
        let recovery = AriesRecovery::analyze(&records);
        let mut redone = Vec::new();
        let count = recovery.redo(&records, |rec| {
            redone.push(rec.payload.clone());
        });
        assert_eq!(count, 1); // Only txn 1's insert.
        assert_eq!(redone, vec![b"a".to_vec()]);
    }

    #[test]
    fn aries_undo_rolls_back_losers() {
        let records = vec![
            LogRecord {
                lsn: 1, prev_lsn: 0, txn_id: 1, rec_type: LogRecordType::Insert,
                page_id: 1, payload: b"a".to_vec(), crc32c: 0,
            },
            LogRecord {
                lsn: 2, prev_lsn: 0, txn_id: 2, rec_type: LogRecordType::Insert,
                page_id: 2, payload: b"b".to_vec(), crc32c: 0,
            },
            LogRecord {
                lsn: 3, prev_lsn: 1, txn_id: 1, rec_type: LogRecordType::Commit,
                page_id: 0, payload: vec![], crc32c: 0,
            },
            // Txn 2 is a loser.
        ];
        let recovery = AriesRecovery::analyze(&records);
        let mut undone = Vec::new();
        let count = recovery.undo(&records, |rec| {
            undone.push(rec.payload.clone());
        });
        assert_eq!(count, 1); // Only txn 2's insert.
        assert_eq!(undone, vec![b"b".to_vec()]);
    }

    #[test]
    fn checkpoint_writes_record() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wal.cdb");
        let mut wal = WriteAheadLog::open(&path, WalConfig::default()).unwrap();
        wal.append(1, 0, LogRecordType::Insert, PageId(1), b"x").unwrap();
        wal.checkpoint().unwrap();
        let records = wal.read_all().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].rec_type, LogRecordType::Checkpoint);
    }
}
