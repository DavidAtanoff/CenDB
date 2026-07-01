# Buffer Pool

The buffer pool (`cendb-buffer`) is the in-memory cache layer between
segment files on disk and the PAX block readers. It deliberately avoids
`mmap` for the reasons set out in §3.1 of the spec.

## Why not mmap?

| Concern | `mmap` | Custom buffer pool |
|---|---|---|
| **I/O stalls** | Page fault → *invisible* blocking I/O | Explicit I/O; we control prefetch, async |
| **Eviction control** | Kernel decides; no scan-resistance | LRU-K with scan resistance |
| **Error handling** | `SIGBUS`, near-impossible to handle | `Result<>` on every read |
| **Transactional safety** | `msync` + WAL ordering is fragile | We control flush ordering |
| **Memory accounting** | Page cache invisible to process | Hard, configurable memory budget |
| **Predictability** | Tail latency spikes from faults | Bounded, measurable |

## Frame layout

Each frame is a fixed `page_size` byte buffer, 64-byte aligned:

```rust
pub struct Frame {
    data:      AlignedBlock,         // 64-byte aligned
    page_id:   AtomicU64,            // INVALID_PAGE_ID if empty
    pin_count: AtomicU32,            // >0 ⇒ cannot evict
    dirty:     AtomicBool,           // modified since load
    page_lsn:  AtomicU64,            // WAL invariant: flush WAL ≥ page_lsn first
    frame_id:  FrameId,              // cached for reverse lookup
}
```

All mutable fields are atomic; pin/unpin uses `fetch_add`/`fetch_sub`
without locks. The eviction path uses `compare_exchange` to skip pinned
frames.

## LRU-K eviction (scan-resistant)

Plain LRU is vulnerable to sequential scans: a long scan pollutes the
cache with one-shot pages, evicting the OLTP hot set. LRU-K fixes this
by keeping, for each frame, the timestamps of its last K accesses; a
frame becomes evictable only after K accesses.

### Algorithm

1. For each frame, track the timestamps of its last K=2 accesses.
2. **Eviction preference**: scan-resistant — prefer to evict frames
   with **fewer than K accesses** (one-shot scan pages) over frames
   with K accesses (hot pages).
3. Among evictable frames with K accesses, pick the one whose K-th-
   most-recent access is the oldest (classic LRU-K rule).

### Read hints

Callers pass a `ReadHint` to `pin_page`:

- `Point` — point lookup or OLTP access; counts toward LRU-K history.
- `Scan` — sequential scan; the page is marked evictable immediately
  after the pin is released.

This makes a 1000-page analytical scan unable to evict a hot OLTP
working set.

## PinnedPage guard

`PinnedPage<'pool>` is an RAII guard:

```rust
pub struct PinnedPage<'pool> {
    frame_id: FrameId,
    pool: &'pool mut BufferPool,
}

impl Drop for PinnedPage<'_> {
    fn drop(&mut self) {
        self.pool.release_pin(self.frame_id);
    }
}
```

The borrow checker guarantees that any `ColumnView` derived from a
pinned page cannot outlive the pin — **compile-time prevention of
use-after-evict**.

## BufferPool API

```rust
let source = Box::new(InMemoryPageSource::new(4096));
let mut pool = BufferPool::new(source, 16, 4096)?;

// Pin a page (read from disk on miss).
let pinned = pool.pin_page(page_id, ReadHint::Point)?;
let bytes: &[u8] = pinned.as_bytes();
// ... use bytes ...
// pinned drops here → pin_count decremented, frame becomes evictable.

// Allocate a new page.
let pinned = pool.new_page(new_page_id)?;
pinned.mark_dirty(42);  // page_lsn = 42

// Flush dirty pages.
pool.flush_all()?;
```

## PageSource trait

Backends implement `PageSource`:

```rust
pub trait PageSource {
    fn read_page(&mut self, page_id: PageId, buf: &mut [u8]) -> HexResult<()>;
    fn write_page(&mut self, page_id: PageId, buf: &[u8]) -> HexResult<()>;
    fn page_size(&self) -> usize;
}
```

Implementations:
- `InMemoryPageSource` — `HashMap<PageId, Vec<u8>>`; used by tests.
- `MmapPageSource` (feature-gated) — read-only mmap; for tiny, read-
  mostly deployments where the OS page cache is sufficient.

## Optional mmap mode

For tiny, read-mostly KV deployments where binary minimalism trumps
control, enable the `mmap` cargo feature:

```toml
[dependencies]
cendb-buffer = { path = "...", features = ["mmap"] }
```

Then use `MmapPageSource` instead of `InMemoryPageSource`:

```rust
let mut src = MmapPageSource::open("data.cdb", 4096)?;
// read-only; write_page() returns Err.
```

**When to use mmap mode:**
- The dataset fits comfortably in RAM.
- The workload is dominated by point lookups (no scans).
- Cold-start latency matters more than eviction control.

**When NOT to use mmap mode:**
- Mixed OLTP/OLAP workloads (scan resistance requires the custom pool).
- Large datasets (mmap page faults cause unpredictable tail latency).
- Embedded deployments with hard memory caps.

## Statistics

`pool.stats()` returns a `PoolStats` snapshot:

```rust
pub struct PoolStats {
    pub hits: u64,           // page was already in pool
    pub misses: u64,         // page had to be loaded
    pub evictions: u64,      // frames reclaimed
    pub flushes: u64,        // dirty pages written back
    pub pinned_frames: u32,  // currently pinned
    pub total_frames: u32,   // pool capacity
}
```

Use these to tune `pool_frames` and identify scan-pollution issues.

## WAL invariant

A frame is only written to disk after the WAL has durably persisted up to
`frame.page_lsn`. This prevents a torn page from exposing uncommitted
data:

```
1. Txn modifies page P, records WAL entry with lsn=42.
2. Frame for P is marked dirty with page_lsn=42.
3. Eviction wants to write P to disk.
4. Eviction waits for WAL to have persisted up to lsn=42.
5. Eviction writes P to disk, clears dirty flag.
```

Without this invariant, a crash between steps 4 and 5 could leave the
page on disk with modifications whose WAL records were never persisted —
violating durability.

## Concurrency (future work)

The current pool is `!Sync` (single-threaded). A production version
would:
- Wrap the page table in a `DashMap` for concurrent lookups.
- Use per-frame latches (not in `Frame` directly) for writers.
- Implement `io_uring` for batched async I/O on Linux.

The `Frame` struct's atomic fields are already atomic so the concurrency
upgrade is mostly mechanical.
