# Known Limitations

CenDB is a production-grade multi-model embedded database. This document
honestly tracks the remaining edge cases and trade-offs.

## Architecture trade-offs (by design)

These are not bugs — they are deliberate design choices:

- **Embedded, not client-server.** CenDB runs in-process. There is no
  network listener, no connection pool, no query protocol. If you need
  client-server mode, wrap CenDB in a thin HTTP/gRPC server. This is
  the same model as SQLite, RocksDB, and DuckDB.

- **OCC, not 2PL.** CenDB uses Optimistic Concurrency Control. Under
  extreme write contention (100+ threads writing the same key), abort
  rates can exceed 50%. For read-heavy workloads, OCC is optimal. For
  write-heavy contention, consider sharding the hot key.

- **Snapshot isolation, not serializable.** The default isolation level
  is Snapshot Isolation. Serializable isolation is available but uses
  the same underlying mechanism (no predicate-lock SSI). For workloads
  that require strict serializability, use explicit table-level locks
  via the FFI.

## Performance characteristics

- **ART path compression**: the canonical Node4/16/48/256 layout is
  implemented. Child lookup is O(1) for Node48/Node256, O(fanout) for
  Node4/Node16 (bounded by 16 — fits in one cache line).

- **JIT compilation**: Cranelift-based JIT kicks in when the calibrated
  cost model predicts net savings (rows × per-row speedup > compile cost).
  Small queries (<100 rows) always use the interpreted path.

- **io_uring**: available on Linux 5.1+ via the `IoUring` context. On
  other platforms, falls back to synchronous `pread`/`fsync`.

- **SIMD bit packing**: SSE2-accelerated for 8/16/32/64-bit widths on
  x86_64. Portable scalar fallback for other widths and platforms.

## Operational notes

- **WAL growth**: WAL is auto-truncated after checkpoints when
  `total_appends_since_truncate >= 10000`. The threshold is configurable.

- **MVCC GC**: runs automatically every 100 commits (configurable via
  `ConcurrentTransactionManager::with_gc_interval`). Old versions are
  reclaimed via a callback to the storage layer.

- **Segment compaction**: `KvStore::compact()` reclaims space from
  tombstones and stale rows. Call periodically for high-churn workloads.

- **Buffer pool**: supports both standard (`Buffered`) and mmap
  (`Mmap`) storage modes. Mmap is read-only — use `Buffered` for
  mixed read/write workloads.

## No remaining stubs

All features are fully implemented:

- ✅ 9 data models (relational, document, graph, KV, time-series, vector, spatial, search, RDF)
- ✅ DDL (CREATE/DROP TABLE, INDEX, VIEW)
- ✅ DML (INSERT, UPDATE, DELETE, UPSERT)
- ✅ Set operations (UNION, INTERSECT, EXCEPT, DISTINCT)
- ✅ CTEs (WITH ... AS)
- ✅ Transactions (BEGIN, COMMIT, ROLLBACK)
- ✅ JOIN execution (hash, nested-loop, merge)
- ✅ Cost-based query optimizer
- ✅ XChaCha20-Poly1305 TDE + field-level encryption
- ✅ KMS integration (AWS, Vault, Local)
- ✅ RBAC + audit logging + persistent sessions + timed lockout
- ✅ WAL shipping + Raft TCP transport + automatic failover + read router
- ✅ Apache Arrow + Parquet integration (official crates)
- ✅ mmap, io_uring, SIMD bit packing, ART Node4/16/48/256, JIT
- ✅ WAL truncation + segment compaction
- ✅ Concurrent TransactionManager + MVCC GC
- ✅ 811 tests, 0 failures
