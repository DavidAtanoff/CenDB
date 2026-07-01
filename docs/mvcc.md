# MVCC + WAL + ARIES-lite Recovery

The transaction layer (`cendb-tx`) implements the concurrency, durability,
and crash-recovery protocols described in §2 of the spec.

## Design

- **MVCC** for reader/writer isolation (readers never block writers).
- **Optimistic, multi-writer** transactions validated at commit (good
  under low contention, which dominates embedded use).
- **Segment-partitioned writes** so independent writers touching
  different segments proceed lock-free; only the WAL append and the
  commit-timestamp oracle are shared.
- **WAL, not shadow paging.** Recovery is three-phase ARIES.

## MVCC version model

Each tuple version carries a `VersionHeader`:

```rust
#[repr(C)]
pub struct VersionHeader {           // 32 bytes
    pub begin_ts: u64,               // commit ts that created this version
    pub end_ts:   u64,               // u64::MAX if live; else ts that superseded
    pub txn_id:   u64,               // creating txn (for uncommitted visibility)
    pub prev_version_off: u64,       // pointer to older version (chain)
}
```

Versions form a chain. We use **newest-to-oldest in-place for the latest
version, older versions appended to an undo segment** (Oracle/HyPer
style) rather than RocksDB-style LSM stacking, because:
- Latest version stays hot in the PAX block (point lookups don't walk a
  chain).
- Old versions migrate to a separate undo area, keeping main blocks
  dense → better scan & compression.

## Visibility

A transaction with snapshot `read_ts` sees version `v` iff:

```
v.begin_ts <= read_ts
AND (v.end_ts > read_ts)
AND committed(v.begin_ts)
OR (v.txn_id == self.txn_id)   // own writes
```

Implemented in `VersionHeader::is_visible_to`:

```rust
pub fn is_visible_to(
    &self,
    read_ts: u64,
    reader_txn_id: u64,
    committed: &impl Fn(u64) -> bool,
) -> bool {
    if self.txn_id == reader_txn_id {
        return true;  // own writes
    }
    if self.begin_ts > read_ts { return false; }
    if self.end_ts <= read_ts { return false; }
    committed(self.begin_ts)
}
```

## Timestamp oracle

A single `AtomicU64` backs the timestamp oracle. `fetch_add` is
wait-free; `current()` is a load.

```rust
pub struct TimestampOracle {
    current: AtomicU64,
}

impl TimestampOracle {
    pub fn current(&self) -> u64 { self.current.load(Acquire) }
    pub fn next(&self) -> u64 { self.current.fetch_add(1, AcqRel) + 1 }
}
```

Validation reads are lock-free via atomic loads on version headers. The
common (uncontended) path is lock-free.

## Commit protocol (optimistic OCC)

```
1. READ phase:  acquire read_ts = oracle.current()
                buffer writes in private write-set (no locks)
2. VALIDATE:    acquire commit_ts = oracle.next()   // monotonic, CAS on atomic u64
                for each key in write-set:
                    if latest_committed_version(key).begin_ts > read_ts:
                        ABORT  // write-write conflict
3. WRITE phase: append redo records to WAL (group-committed)
                install versions, set end_ts on superseded versions
                publish commit_ts (make versions visible)
```

Implementation: `TransactionManager::commit` performs the validation
loop and either aborts (returning `MvccError::Conflict`) or publishes
the commit timestamp.

## Adaptive durability

Per-transaction durability levels (planned, currently always Strict):

| Level | Behaviour | Use case |
|---|---|---|
| `Strict` | fsync before ack | Financial / correctness |
| `Group` | ack after group fsync (default; bounded latency window) | General OLTP |
| `Async` | ack on WAL buffer write, fsync deferred | Time-series ingest |

This lets the time-series ingest lens hit millions of writes/sec while
relational transactions stay durable — same engine, per-tx policy.

## WAL record format

Each record is variable-length:

```
┌─────────────────────────────────────────────────┐
│ lsn:       u64   (this record's LSN)            │
│ prev_lsn:  u64   (back-chain for this txn)      │
│ txn_id:    u64                                 │
│ rec_type:  u8   (Insert/Update/Delete/...)     │
│ page_id:   u64                                 │
│ payload_len: u32                               │
│ payload:   [u8; payload_len]                   │
│ crc32c:    u32                                 │
└─────────────────────────────────────────────────┘
```

We log **physiological** records (logical within a page, physical across
pages): redo says "apply this delta to slot S of page P". This is
compact (no full-page logging except on first dirty after checkpoint,
à la PostgreSQL `full_page_writes`).

## CRC32c

Every WAL record carries a CRC32c (Castagnoli) checksum of its contents
(excluding the CRC itself). On read, the CRC is recomputed and compared;
mismatch → `WalError::CrcMismatch`.

The software CRC32c uses the reflected polynomial `0x1EDC6F41`. A
production build would use `crc32c` hardware instructions (SSE 4.2 on
x86, ARMv8 crypto extensions on aarch64).

## Recovery (three-phase ARIES)

```
ANALYSIS: scan WAL from last checkpoint → rebuild Dirty Page Table +
          active txn table.
REDO:     replay all records with lsn > page.pageLSN (idempotent via LSN
          check).
UNDO:     roll back losers using prev_lsn chains, writing CLRs
          (compensation log records) so undo is itself crash-safe.
```

Implementation: `AriesRecovery::analyze` takes a `&[LogRecord]` and
partitions them into committed (winners) and in-flight (losers). `redo`
and `undo` apply the records via caller-supplied closures.

## Group commit

Commits accumulate in a ring buffer; one `fsync` flushes many txns
(amortized durability cost). For the prototype, `WalConfig` exposes
two sync knobs:

- `sync_on_commit` (default true): fsync after every commit record.
- `sync_on_every_record` (default false): fsync after every record
  (slowest, most durable).

A production version would add a background group-commit thread that
batches fsyncs on a configurable latency window (default 10ms).

## API

```rust
use cendb_tx::{TransactionManager, IsolationLevel};

let mut tm = TransactionManager::new();
let txn = tm.begin(IsolationLevel::Snapshot);
tm.record_write(txn, b"key1").unwrap();
let commit_ts = tm.commit(txn).unwrap();
```

For WAL:

```rust
use cendb_tx::{WriteAheadLog, WalConfig, LogRecordType};
use cendb_core::PageId;

let mut wal = WriteAheadLog::open("wal.cdb", WalConfig::default())?;
let lsn1 = wal.append(1, 0, LogRecordType::Insert, PageId(100), b"row1")?;
wal.commit(1, lsn1)?;

// Recovery:
let records = wal.read_all()?;
let recovery = cendb_tx::AriesRecovery::analyze(&records);
println!("committed txns: {:?}", recovery.committed_txns);
println!("loser txns: {:?}", recovery.loser_txns);
```
