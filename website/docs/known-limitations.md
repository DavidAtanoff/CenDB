---
sidebar_position: 15
title: Known Limitations
---

# Known Limitations

An honest accounting of what CenDB does not (yet) do.

## Addressed (previously listed as limitations)

- ✅ **Concurrent TransactionManager** — `ConcurrentTransactionManager` with RwLock-based interior mutability.
- ✅ **MVCC garbage collection** — wired into commit path, runs periodically.
- ✅ **KV get linear scan** — fixed with HashMap (p99: 17µs → 1.89µs).
- ✅ **WAL truncation** — `truncate_to_checkpoint()` reclaims space after checkpoints.
- ✅ **Segment compaction** — `compact()` reclaims space from stale rows and tombstones.
- ✅ **JOIN execution** — hash join, nested loop join, merge join implemented.
- ✅ **Subqueries** — FROM subqueries and IN subqueries supported.
- ✅ **Query optimizer** — cost-based optimizer with join selection and filter pushdown.
- ✅ **Field-level encryption** — per-column keys via `FieldEncryptor`.
- ✅ **KMS integration** — `KmsProvider` trait with AWS/GCP/Vault configs.
- ✅ **Persistent sessions** — `FileSessionStore` survives restart.
- ✅ **Timed lockout** — 15-minute default, configurable.
- ✅ **RDF store** — triple store + SPARQL + N-Triples/Turtle.
- ✅ **FTS ranking** — BM25 + snippets + fuzzy matching + Porter stemming.
- ✅ **Spatial OGC** — all DE-9IM predicates + CRS + GeoJSON/WKT/WKB.

## Remaining limitations

### Performance
- **Dictionary encode is O(n log n)** — dominant cost for very large columns. A production version would use external sort.
- **BitWriter/BitReader are bit-at-a-time** — no SIMD-accelerated bit packing (planned).
- **ART uses simple Vec layout** — not canonical Node4/16/48/256. Child lookup is O(fanout) (planned).
- **No JIT** — Cranelift JIT stub exists but is not wired into the executor (planned).
- **No io_uring** — all I/O is synchronous (planned for Linux).

### Storage
- **mmap mode exists but is not wired** into the main read path — segments larger than RAM require manual integration.

### Query
- **Optimizer is not wired into CenQL execution** — cost model and physical plan selection exist but the CenQL executor does not call them yet.

### Replication
- **Raft TCP transport** — the transport layer exists but is not battle-tested in production.
- **Read router** — basic round-robin; no health-aware routing or connection pooling.
