# CenDB

> Enterprise-grade multi-model embedded database engine built in safe Rust.

CenDB projects **9 data models** — relational, document, graph, key-value,
time-series, vector, spatial, search, and (planned) RDF — onto a single
unified PAX storage substrate. One engine, one backup, one recovery
procedure, one security model.

[![tests](https://img.shields.io/badge/tests-421%20passing-brightgreen)]()
[![license](https://img.shields.io/badge/license-Atanoff%20v1.0-blue)]()
[![rust](https://img.shields.io/badge/rust-1.96%2B-orange)]()

## Why CenDB?

Most databases force you to pick one model: PostgreSQL for relational,
MongoDB for documents, Neo4j for graphs, InfluxDB for time-series. Each
comes with its own storage engine, backup tooling, security model, and
operational burden. CenDB takes a different approach: **one physical
substrate, many logical lenses**.

## Highlights

- **9 data models, 1 substrate** — 7 fully supported, 2 partially, 1 gap
  documented.
- **Corporate-grade security** — XChaCha20-Poly1305 page encryption,
  Argon2id key derivation, RBAC, tamper-evident audit logging.
- **Crash-tested** — 500+ on-disk crash iterations, 50K+ fuzz iterations,
  zero panics. Two critical bugs found and fixed in Phase 1.
- **Real performance** — KV put p99: 389ns. Vectorized scan: 182M
  rows/sec. Realistic compression: 4-11×.
- **p50/p95/p99 latency** — not just mean throughput.
- **C-FFI** — single library with bindings for C, Python, Go, Node.js.

## Quick start

```bash
git clone https://github.com/DavidAtanoff/CenDB.git
cd CenDB/source
cargo build --workspace --release
cargo test --workspace --release
```

## Documentation

| Resource | Description |
|---|---|
| [docs-site](docs-site/) | Modern technical-minimalism docs website (GitHub/Cloudflare Pages ready) |
| [docs/architecture.md](docs/architecture.md) | Layered architecture and design invariants |
| [docs/benchmarks.md](docs/benchmarks.md) | Real p50/p95/p99 latency + realistic compression ratios |
| [docs/security.md](docs/security.md) | TDE, auth, RBAC, audit logging, threat model |
| [docs/known-limitations.md](docs/known-limitations.md) | Honest accounting of what's not (yet) done |
| [docs/phase-1-report.md](docs/phase-1-report.md) | Durability proof: crash harness, fuzzer, bug fixes |
| [docs/phase-2-report.md](docs/phase-2-report.md) | Realistic compression benchmarks |
| [docs/phase-6-data-model-audit.md](docs/phase-6-data-model-audit.md) | 9-model coverage audit |
| [docs/getting-started.md](docs/getting-started.md) | Build, install, first program |
| [docs/storage.md](docs/storage.md) | PAX page format, segment layout, encodings |
| [docs/mvcc.md](docs/mvcc.md) | MVCC, OCC, WAL, ARIES recovery |
| [docs/cenql.md](docs/cenql.md) | Pipeline-oriented query language |
| [docs/ffi.md](docs/ffi.md) | C-ABI for cross-language bindings |

## Crate layout

```
cendb-core        — Shared primitives (PageId, errors, config, Value)
cendb-storage     — PAX page format, segment files, encodings
cendb-buffer      — User-space buffer pool, LRU-K, pinned-page guards
cendb-projection  — KV, relational, document, time-series, graph models
cendb-index       — Adaptive Radix Tree (ART) primary index
cendb-tx          — MVCC + WAL + ARIES-lite recovery + concurrent stress
cendb-cenql       — CenQL lexer + parser + AST
cendb-security    — TDE (XChaCha20-Poly1305), auth, RBAC, audit logging
cendb-replication — WAL shipping + Raft simulation
cendb-ffi         — C-ABI for cross-language bindings
cendb             — Facade crate with feature gates + prelude
```

## Design philosophy

Three non-negotiable invariants drive every decision:

1. **One physical substrate, many logical lenses.** The "model" is a
   property of the schema descriptor and the access path planner, not
   the storage layer.

2. **Pay only for what you touch.** Cold-start, binary size, and memory
   footprint are first-class. Enforced via Cargo feature gates.

3. **Zero-copy is the default; copying is an explicit cost.** The
   on-disk representation is the in-memory representation for read
   paths.

## License

Licensed under the [Atanoff License v1.0](LICENSE) — a permissive
license based on MIT with two additions:

- **Attribution required for commercial use.** Commercial users must
  credit "David Atanoff" in their application and documentation.
- **Commercial use restricted to US territory.** Personal use is
  permitted worldwide.

See [LICENSE](LICENSE) for the full text.

## Author

**David Atanoff** — [github.com/DavidAtanoff](https://github.com/DavidAtanoff)
