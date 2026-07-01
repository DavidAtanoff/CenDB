---
sidebar_position: 1
title: Introduction
---

# CenDB

> Enterprise-grade multi-model embedded database engine built in safe Rust.

CenDB projects **9 data models** — relational, document, graph, key-value, time-series, vector, spatial, search, and RDF — onto a single unified PAX storage substrate. One engine, one backup, one recovery procedure, one security model.

## Why CenDB?

Most databases force you to pick one model: PostgreSQL for relational, MongoDB for documents, Neo4j for graphs, InfluxDB for time-series. Each comes with its own storage engine, backup tooling, security model, and operational burden. CenDB takes a different approach: **one physical substrate, many logical lenses**.

## Highlights

- **9 data models, 1 substrate** — all fully implemented with tests.
- **Corporate-grade security** — XChaCha20-Poly1305 page + field-level encryption, Argon2id key derivation, KMS integration, RBAC, tamper-evident audit logging.
- **Crash-tested** — 500+ on-disk crash iterations, 75K+ fuzz iterations, zero panics. Two critical bugs found and fixed.
- **Concurrent** — thread-safe `ConcurrentTransactionManager` with RwLock-based interior mutability. 8/16/100-thread stress tests pass with no lost updates.
- **Real performance** — KV put p99: 389ns. KV get p99: 1.89µs (was 17µs, fixed). Vectorized scan: 182M rows/sec. Crash recovery: 157K records/sec.
- **p50/p95/p99 latency** — not just mean throughput. Real-world benchmark suite covers disk IO, concurrency scaling, mixed workloads, and durability.
- **C-FFI** — single library with bindings for C, Python, Go, Node.js.

## Quick start

```bash
git clone https://github.com/DavidAtanoff/CenDB.git
cd CenDB/source
cargo build --workspace --release
cargo test --workspace --release
```

## Design philosophy

Three non-negotiable invariants drive every decision:

1. **One physical substrate, many logical lenses.** The "model" is a property of the schema descriptor and the access path planner, not the storage layer.

2. **Pay only for what you touch.** Cold-start, binary size, and memory footprint are first-class. Enforced via Cargo feature gates.

3. **Zero-copy is the default; copying is an explicit cost.** The on-disk representation is the in-memory representation for read paths.

## License

Licensed under the [Atanoff License v1.0](https://github.com/DavidAtanoff/CenDB/blob/main/LICENSE) — MIT-based with attribution required for commercial use and US-territory commercial restriction. Personal use permitted worldwide.
