---
sidebar_position: 2
title: Architecture
---

# Architecture

CenDB is built in layers, each with a single responsibility and a well-defined interface to the layer above and below.

## Layered view

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
│              File Abstraction (pread/pwrite, mmap, io_uring)  │
└──────────────────────────────────────────────────────────────┘
```

## The three invariants

### 1. One physical substrate, many logical lenses

We don't bolt nine engines together behind a router. We build a single log-structured, slotted, columnar-capable page format (PAX) and project all nine models onto it. The "model" is a property of the schema descriptor and the access path planner, not the storage layer.

### 2. Pay only for what you touch

Cold-start, binary size, and memory footprint are first-class. A KV-only workload must never link or initialize the columnar vectorized executor, the graph BFS engine, or the time-series downsampler. This is enforced via Cargo feature gates and lazy subsystem initialization.

### 3. Zero-copy is the default; copying is an explicit cost

The on-disk representation *is* the in-memory representation for read paths. Deserialization that allocates is treated as a defect unless crossing the FFI boundary.

## Crate layout

```
cendb-core        — Shared primitives (PageId, errors, config, Value)
cendb-storage     — PAX page format, segment files, encodings
cendb-buffer      — User-space buffer pool, LRU-K, mmap, pinned-page guards
cendb-projection  — KV, relational, document, time-series, graph models
cendb-index       — Adaptive Radix Tree (ART) primary index
cendb-tx          — MVCC + WAL + ARIES recovery + ConcurrentTransactionManager
cendb-cenql       — CenQL lexer + parser + AST
cendb-security    — TDE, field encryption, KMS, auth, RBAC, audit logging
cendb-replication — WAL shipping + Raft + failover + read router
cendb-rdf         — RDF triple store + SPARQL + N-Triples/Turtle
cendb-search      — Full-text search (BM25, snippets, fuzzy, stemming)
cendb-spatial     — OGC predicates, CRS, GeoJSON/WKT/WKB, R-tree
cendb-vector      — HNSW vector search
cendb-executor    — Vectorized execution, JOIN, subqueries
cendb-optimizer   — Cost-based query optimizer
cendb-ffi         — C-ABI for cross-language bindings
cendb             — Facade crate with feature gates + prelude
```
