# CenDB Production Readiness Checklist & Missing Features Roadmap

To transition CenDB from a high-performance prototype to a mature, enterprise-grade, battle-tested database engine, the following components and validation layers should be developed.

---

## 1. Storage & Durability Architecture Gaps

- **Write-Ahead Logging (WAL)**
  - *Current Status*: Writes are buffered in memory and persisted directly to sealed PAX block files. If the process crashes mid-block write or before segment seal, data is lost or corrupted.
  - *Missing*: A sequential append-only WAL. Transactions must write to the WAL and `fsync` before reporting success. On startup, the engine must replay the WAL to reconstruct unsealed memory states.
- **Buffer Pool Manager & Disk Spilling**
  - *Current Status*: PAX blocks are largely held in-memory or read on-demand without a robust replacement policy.
  - *Missing*: An LRU-K or 2Q buffer pool manager that keeps hot pages/blocks in memory and transparently spills cold data to disk, managing memory usage within a strict user-configured boundary.
- **B-Link Tree (or LSM-Tree) Index Disk Persistence**
  - *Current Status*: Key-Value points use an in-memory Hash Index (`HashMap`).
  - *Missing*: A disk-backed B-link Tree or Partitioned LSM-tree index structure to prevent OOM errors when database keys exceed memory capacity.

---

## 2. Advanced Algorithmic Maturity

- **Vector Store (HNSW) Page Serialization**
  - *Current Status*: The HNSW index lives purely in-memory.
  - *Missing*: Serialization of the HNSW multi-layer graph pages to disk segments so that vector searches survive database restarts.
- **Consensus & Real Networked Replication**
  - *Current Status*: Replication is simulated or single-process.
  - *Missing*: Real network sockets (TCP/gRPC) implementing Raft or Paxos consensus across independent physical nodes to support high-availability (HA), partition tolerance, and split-brain recovery.
- **True Multi-Version Concurrency Control (MVCC)**
  - *Current Status*: Thread-safety is achieved by locking global projection Mutexes. This serializes all writes.
  - *Missing*: Row/cell versioning inside PAX blocks with transaction LSNs (Log Sequence Numbers) to allow lock-free concurrent readers and writers (Snapshot Isolation).

---

## 3. Battle Testing & Resiliency Pipelines

- **Jepsen Testing**
  - Integrate a Jepsen test suite to inject network partitions, packet loss, node crashes, and clock drifts while concurrently running KV, TS, and Graph queries to verify linearizability and consensus durability.
- **Fuzzing & Memory Safety Checkers**
  - Run continuous Rust cargo-fuzz (libFuzzer) against FFI borders, PAX block deserializers, and CenQL parsers to guard against memory corruption, buffer overflows, and panic-inducing input vectors.
- **AddressSanitizer (ASan) & LeakSanitizer (LSan)**
  - Incorporate ASan and LSan checks into the CI pipeline (especially when compiling FFI staticlibs/cdylibs) to detect memory leaks, double frees, or use-after-free bugs across Python/Rust boundaries.
- **Chaos Ingestion Injectors**
  - Run high-concurrency benchmarks while simulating disk-full events, OS SIGKILL signals, and corrupted segment headers to ensure the recovery manager detects and isolates corrupted blocks gracefully.

---

## 4. API & Query Capability Improvements

- **Full Apache Arrow/Parquet Direct Integration**
  - Build zero-copy column exporters that dump PAX blocks directly into Arrow RecordBatches, allowing direct vectorization inside analytical engines like DuckDB, Polars, or Pandas.
- **CenQL Parser & Planner Optimizations**
  - Complete the SQL parser for CenQL to support nested subqueries, joins, aggregation pushdowns, and execution plan caching.
