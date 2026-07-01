# Getting Started

This guide walks you through building CenDB, opening your first database,
and performing basic operations.

## Prerequisites

- Rust 1.70+ (install via [rustup](https://rustup.rs/))
- A C compiler (for the FFI shared library)
- Linux, macOS, or Windows

## Build

```bash
git clone https://example.com/cendb.git
cd cendb

# Build everything (debug):
cargo build --workspace

# Build optimized release artifacts:
cargo build --workspace --release

# Build the C-ABI shared library (for cross-language bindings):
cargo build --release -p cendb-ffi
# → target/release/libcendb_ffi.so (Linux)
# → target/release/libcendb_ffi.dylib (macOS)
# → target/release/cendb_ffi.dll (Windows)
```

## Run the verification suite

The verification suite generates realistic mock data (10K rows, 5K docs,
1K nodes/10K edges, 100K TS readings) and runs functional + performance
tests:

```bash
cargo test --workspace --release -- --nocapture
```

You'll see output like:

```
[gen_relational_10k_rows] inserted 10000 rows in 30.5ms (326846 rows/sec)
[gen_timeseries_100k_readings] inserted 100000 readings in 91.2ms (1095328 reads/sec)
[verify_kv_point_write_and_readback] 1000 KV pairs verified
[verify_timeseries_range_scan_with_zone_map_skipping] range [5000, 5099] touched 1/10 blocks (skipped 9)
[verify_graph_two_hop_traversal] 2-hop from 0: [NodeId(2), NodeId(6)]
[perf_compression_ratio] Time-Series: 0.92x
[perf_point_lookup_vs_scan_latency] point lookup: 109µs/op, full scan: 1.8ms
```

## Your first Rust program

Add CenDB to your `Cargo.toml`:

```toml
[dependencies]
cendb = { path = "/path/to/cendb/crates/cendb" }
```

Then write your first program:

```rust
use cendb::prelude::*;
use cendb_core::{SegmentId, Value, ValueKind};
use cendb_projection::{KvStore, TimeSeriesSchema, TimeSeriesStore};
use cendb_storage::header::ColumnSpec;

fn main() -> CenResult<()> {
    // Key-Value store.
    let mut kv = KvStore::new(SegmentId(1), 64 * 1024);
    kv.put(b"alice", b"password123")?;
    kv.put(b"bob", b"hunter2")?;
    kv.seal()?;

    assert_eq!(kv.get(b"alice")?, Some(b"password123".to_vec()));
    assert_eq!(kv.get(b"charlie")?, None);

    // Time-series store.
    let schema = TimeSeriesSchema {
        ts_col_id: 0,
        series_col_id: 1,
        extra_cols: vec![ColumnSpec::new(2, ValueKind::F64)],
    };
    let mut ts = TimeSeriesStore::new(schema, SegmentId(2), 256 * 1024);
    for ts_val in 0..1000i64 {
        ts.append(ts_val, 1, (ts_val as f64).sin())?;
    }
    ts.flush_pending()?;

    let (touched, results) = ts.range_scan(100, 200)?;
    println!("range [100, 200]: touched {} blocks, {} results",
             touched, results.len());

    Ok(())
}
```

## Cross-language usage

### Python

```python
import sys
sys.path.insert(0, '/path/to/cendb/bindings/python')
import cendb

db = cendb.open()  # in-memory
db.kv_put(b"alice", b"password123")
print(db.kv_get(b"alice"))  # b"password123"
print(cendb.version())      # "0.2.0"
db.close()
```

Set `CENDB_LIB_PATH` to point at `libcendb_ffi.so` if it's not on the
library path.

### Go

```go
package main

import (
    "fmt"
    "log"
    "path/to/cendb/bindings/go/cendb"
)

func main() {
    db, err := cendb.Open("", cendb.DefaultConfig())
    if err != nil {
        log.Fatal(err)
    }
    defer db.Close()

    if err := db.KVPut([]byte("alice"), []byte("password123")); err != nil {
        log.Fatal(err)
    }
    val, err := db.KVGet([]byte("alice"))
    if err != nil {
        log.Fatal(err)
    }
    fmt.Printf("value: %s\n", val)
}
```

Build with:

```bash
CGO_LDFLAGS="-L/path/to/cendb/target/release -lcendb_ffi" go build ./...
```

### C

```c
#include "cendb.h"
#include <stdio.h>

int main(void) {
    CenDb* db = NULL;
    CenConfig cfg = { .page_size = 4096, .block_size = 65536,
                      .pool_frames = 1024, .group_commit_ms = 10, .flags = 0 };
    if (cendb_open(NULL, &cfg, &db) != CEN_OK) {
        fprintf(stderr, "cendb_open: %s\n", cendb_last_error_message());
        return 1;
    }

    cendb_kv_put(db, (const uint8_t*)"alice", 5, (const uint8_t*)"password123", 11);

    CenBytes val = {0};
    if (cendb_kv_get(db, (const uint8_t*)"alice", 5, &val) == CEN_OK) {
        printf("value: %.*s\n", (int)val.len, val.ptr);
        cendb_bytes_free(&val);
    }

    cendb_close(db);
    return 0;
}
```

Compile with:

```bash
gcc -I bindings/c -L target/release -lcendb_ffi my_program.c -o my_program
```

## Next steps

- Read [architecture.md](./architecture.md) to understand the layered
  design.
- Read [cenql.md](./cenql.md) to learn CenQL, the query language.
- Read [storage.md](./storage.md) to understand the PAX page format and
  how to choose encodings.
- Browse the [examples](../bindings/) for working code in each language.
