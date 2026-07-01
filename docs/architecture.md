# Architecture

CenDB is a **multi-model embedded database engine** built in safe Rust.
It implements a single unified storage substrate (PAX pages) onto which
six logical data models are projected: relational, columnar, document,
key-value, time-series, and graph.

## Layered architecture

```
┌──────────────────────────────────────────────────────────────┐
│                     CenQL DSL / C-FFI                        │
├──────────────────────────────────────────────────────────────┤
│   Logical Planner → Optimizer → Physical Planner → Executor  │
│   (model-aware access path selection: row / col / graph / ts) │
├──────────────────────────────────────────────────────────────┤
│   Unified Catalog  │  MVCC Tx Manager  │  Index Layer (ART)   │
├──────────────────────────────────────────────────────────────┤
│        Buffer Pool (user-space, pinning, eviction)           │
├──────────────────────────────────────────────────────────────┤
│   WAL  │  Segment Manager  │  PAX Page Format  │  Compression │
├──────────────────────────────────────────────────────────────┤
│              File Abstraction (pread/pwrite, O_DIRECT opt)    │
└──────────────────────────────────────────────────────────────┘
```

## The three invariants

Every design decision is evaluated against three non-negotiable
invariants:

### 1. One physical substrate, six logical lenses

We don't bolt six engines together behind a router. We build a single
log-structured, slotted, columnar-capable page format (PAX) and project
all six models onto it. The "model" is a property of the *schema
descriptor* and the *access path planner*, not the storage layer.

**Consequence**: KV writes, relational inserts, time-series appends, and
graph edges all land in the same on-disk format. A single backup, a
single compaction strategy, a single recovery procedure.

### 2. Pay only for what you touch

Cold-start, binary size, and memory footprint are first-class. A
KV-only workload must never link or initialize the columnar vectorized
executor, the graph BFS engine, or the time-series downsampler. This is
enforced via Cargo feature gates and lazy subsystem initialization.

**Consequence**: a stripped KV+relational build is < 2 MiB; the full
six-model build is ~6–8 MiB.

### 3. Zero-copy is the default; copying is an explicit cost

The on-disk representation *is* the in-memory representation for read
paths. Deserialization that allocates is treated as a defect unless
crossing the FFI boundary.

**Consequence**: a point lookup touches one page, reinterprets its
minipage as `&[i64]`, and returns. No allocation, no parse, no copy.

## Crate layout

| Crate | Layer | Responsibility |
|---|---|---|
| `cendb-core` | Foundation | Primitives: `PageId`, `BlockId`, `CenError`, `CenDbConfig`, `Value`. |
| `cendb-storage` | Storage | PAX page format, segment files, encodings (Raw, BitPacked, FoR, DoD, Gorilla, RLE). |
| `cendb-buffer` | Memory | User-space buffer pool, LRU-K eviction, `PinnedPage` RAII guard, optional mmap. |
| `cendb-projection` | Models | KV, relational, document (CenDoc), time-series, graph (CSR overlay). |
| `cendb-index` | Indexing | Adaptive Radix Tree (ART) primary index. |
| `cendb-tx` | Transactions | MVCC, OCC validation, WAL with ARIES-lite recovery. |
| `cendb-cenql` | Query | CenQL lexer + recursive-descent parser + AST. |
| `cendb-ffi` | FFI | C-ABI for cross-language bindings. |
| `cendb` | Facade | Re-exports with feature gates + `prelude`. |

## Request lifecycle

A KV point lookup flows through the engine as follows:

```
cendb_kv_put(db, "alice", "password123")
    │
    ▼
ffi_guard { catch_unwind }            ← cendb-ffi
    │
    ▼
KvStore::put("alice", "password123")  ← cendb-projection
    │
    ├─ hash_key("alice") → i64       ← FNV-1a, no allocation
    │
    ▼
buffer into pending: Vec<(Vec<u8>, Vec<u8>)>
    │
    ▼ (on flush_pending)
PaxBlockBuilder::append_row(...)      ← cendb-storage
    │
    ├─ encode I64 column (auto-select: DoD if monotonic, etc.)
    ├─ encode Bytes column (var-heap slots)
    │
    ▼
PaxBlockBuilder::finalize()           ← writes 64B-aligned minipages
    │
    ▼
KvStore::index.insert(key, (block_id, slot))  ← in-memory hash index
```

A read flows the opposite direction:

```
cendb_kv_get(db, "alice")
    │
    ▼
KvStore::get("alice")
    │
    ├─ check pending (most recent writes)
    │
    ▼
index.get("alice") → (block_id, slot)
    │
    ▼
PaxBlock::var_value(2, slot)          ← zero-copy slice into var-heap
    │
    ▼
return Some(bytes)
```

## Concurrency model

- **MVCC** for reader/writer isolation (readers never block writers).
- **Optimistic, multi-writer** transactions validated at commit (good
  under low contention, which dominates embedded use).
- **Segment-partitioned writes** so independent writers touching
  different segments proceed lock-free; only the WAL append and the
  commit-timestamp oracle are shared.

The timestamp oracle is a single `AtomicU64`; `fetch_add` is wait-free.
Validation reads are lock-free via atomic loads on version headers. The
common (uncontended) path is lock-free.

## Crash recovery

We use **WAL, not shadow paging.** The recovery protocol is three-phase
ARIES:

1. **ANALYSIS**: scan WAL from last checkpoint → rebuild Dirty Page
   Table + active txn table.
2. **REDO**: replay all records with `lsn > page.page_lsn` (idempotent
   via LSN check).
3. **UNDO**: roll back losers using `prev_lsn` chains, writing CLRs
   (compensation log records) so undo is itself crash-safe.

## Binary size and cold-start

To honour the resource-efficiency mandate:

- **Cargo feature gates per model**: `relational`, `columnar`,
  `document`, `kv`, `graph`, `timeseries`, plus `jit`, `zstd`, `arrow`,
  `mmap`. A KV-only build (`--no-default-features --features kv`) links
  neither the vectorized executor nor graph code.
- **Lazy subsystem init**: the CSR overlay, JIT compiler, and
  downsampler are constructed on first use, not at `cendb_open`.
- **`panic = "abort"`, LTO, `opt-level = "z"`** profile for the minimal
  artifact; strip symbols.
- **No `serde`/no reflection in the core** — hand-rolled zero-copy
  codecs keep the dependency tree (and binary) lean.
- Target: **< 2 MiB** stripped for the KV+relational core; full
  six-model build with JIT ~ 6–8 MiB.

## Summary of Pareto trade-offs

| Decision | Chosen | Rejected alternative | Justification |
|---|---|---|---|
| Storage layout | PAX | Pure NSM / pure DSM / fractured mirror | One copy serves OLTP + OLAP; no 2× storage |
| Concurrency | MVCC + OCC, segment-partitioned | Single-writer / sharded thread-per-core | Readers never block writers; no distributed-tx tax in embedded |
| Durability | WAL (ARIES-lite), group commit, tunable | Shadow paging | Locality preserved, write amplification minimized |
| Caching | Custom buffer pool | mmap | Predictable latency, error handling, WAL ordering, memory caps |
| Index | ART + LSM delta + B-link + CSR overlay | B-tree only / LSM only | Best point+range+graph profile per model |
| Compression | Two-stage adaptive, type-aware | Single global codec | Max ratio + predicate-on-compressed scans |
| Query lang | CenQL pipeline | Extended SQL | Readable multi-model; composable |
| Execution | Morsel-driven vectorized push + fast path | Volcano pull only | Scan throughput + cheap OLTP point queries |
| FFI | Opaque handles + Arrow C Data Interface | Serialize-everything | Zero-copy bulk transfer; safe ownership |

This architecture pushes the embedded-database Pareto frontier outward:
it delivers SQLite-class footprint and cold-start, RocksDB-class write
throughput, DuckDB-class analytical scans, and Neo4j-class traversal —
from a single unified substrate, in safe Rust.
