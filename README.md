# CenDB Documentation

Welcome to the CenDB documentation. CenDB (formerly HexaDB) is a
multi-model embedded database engine built in safe Rust. It projects
relational, document, time-series, key-value, and graph data models onto
a single unified PAX storage substrate.

## Table of contents

| Document | Description |
|---|---|
| [getting-started.md](./getting-started.md) | Install, build, and run your first CenDB program. |
| [architecture.md](docs/architecture.md) | The layered architecture: storage → buffer pool → projections → API. |
| [storage.md](docs/storage.md) | The PAX page format, segment/block layout, encodings. |
| [buffer-pool.md](docs/buffer-pool.md) | User-space buffer pool, LRU-K eviction, pinned-page guards, mmap mode. |
| [mvcc.md](docs/mvcc.md) | MVCC, OCC validation, WAL, ARIES-lite recovery. |
| [indexing.md](docs/indexing.md) | Adaptive Radix Tree (ART) primary index. |
| [cenql.md](docs/cenql.md) | CenQL — the pipeline-oriented multi-model query language. |
| [projections.md](docs/projections.md) | The five projections: KV, relational, document, time-series, graph. |
| [ffi.md](docs/ffi.md) | C-ABI for cross-language bindings (Python, Go, Node.js). |
| [benchmarks.md](docs/benchmarks.md) | Performance measurements and tuning guide. |

## Quick links

- **Examples**: see [`bindings/`](bindings/) for working code in C,
  Python, Go, and Node.js.
- **API reference**: each crate has its own `cargo doc` site; run
  `cargo doc --workspace --open` to view them.
- **Source**: all source lives under [`crates/`](source/crates/).

## Design philosophy

Three non-negotiable invariants drive every decision in CenDB:

1. **One physical substrate, six logical lenses.** We don't bolt six
   engines together behind a router. We build a single log-structured,
   slotted, columnar-capable page format and project all six models onto
   it. The "model" is a property of the *schema descriptor* and the
   *access path planner*, not the storage layer.

2. **Pay only for what you touch.** Cold-start, binary size, and memory
   footprint are first-class. A KV-only workload must never link or
   initialize the columnar vectorized executor, the graph BFS engine, or
   the time-series downsampler. This is enforced via Cargo feature gates
   and lazy subsystem initialization.

3. **Zero-copy is the default; copying is an explicit cost.** The on-disk
   representation *is* the in-memory representation for read paths.
   Deserialization that allocates is treated as a defect unless crossing
   the FFI boundary.

## Crate layout

```
cendb-core        — Shared primitives (PageId, errors, config, Value).
cendb-storage     — PAX page format, segment files, encodings.
cendb-buffer      — User-space buffer pool, LRU-K, pinned-page guards.
cendb-projection  — KV, relational, document, time-series, graph models.
cendb-index       — Adaptive Radix Tree (ART) primary index.
cendb-tx          — MVCC + WAL + ARIES-lite recovery.
cendb-cenql       — CenQL lexer + parser + AST.
cendb-ffi         — C-ABI for cross-language bindings.
cendb             — Facade crate with feature gates + prelude.
```

## License

Dual-licensed under MIT OR Apache-2.0.
