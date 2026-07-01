# C-FFI and Cross-Language Bindings

CenDB's `cendb-ffi` crate exposes a C-ABI so it can be called from C,
C++, Python (ctypes/cffi), Go (cgo), and Node.js (N-API/ffi-napi).

## FFI principles

1. **Opaque handles only.** No Rust structs cross the boundary; callers
   hold `*mut CenDb`.
2. **Caller never frees Rust memory directly.** Every Rust-allocated
   buffer has a paired `hex_*_free` function.
3. **Errors are integer codes + thread-local last-error detail.**
   Avoids out-params everywhere.
4. **No panics across FFI.** Every `extern "C"` fn is wrapped in
   `catch_unwind`; a panic becomes `CEN_ERR_INTERNAL`.
5. **No global state** except the thread-local last-error slot.

## Building the shared library

```bash
cargo build --release -p cendb-ffi
```

Produces:
- Linux: `target/release/libcendb_ffi.so`
- macOS: `target/release/libcendb_ffi.dylib`
- Windows: `target/release/cendb_ffi.dll`

## C header

The canonical C header is at
[`bindings/c/cendb.h`](../bindings/c/cendb.h). It declares every
function, struct, and status code exposed by the FFI.

```c
#include "cendb.h"

CenDb* db = NULL;
CenConfig cfg = { .page_size = 4096, .block_size = 65536,
                  .pool_frames = 1024, .group_commit_ms = 10, .flags = 0 };
CenStatus st = cendb_open(NULL, &cfg, &db);
if (st != CEN_OK) {
    fprintf(stderr, "cendb_open failed: %s\n", cendb_last_error_message());
    return 1;
}

cendb_kv_put(db, (const uint8_t*)"alice", 5,
           (const uint8_t*)"password123", 11);

CenBytes val = {0};
if (cendb_kv_get(db, (const uint8_t*)"alice", 5, &val) == CEN_OK) {
    printf("value: %.*s\n", (int)val.len, val.ptr);
    cendb_bytes_free(&val);
}

cendb_close(db);
```

Compile with:

```bash
gcc -I bindings/c -L target/release -lcendb_ffi my_program.c -o my_program
```

## Function reference

### Lifecycle

| Function | Description |
|---|---|
| `cendb_open(path, cfg, out_db)` | Open a database. `path` may be NULL for in-memory. |
| `cendb_close(db)` | Close a database handle. |

### Key-Value fast path

| Function | Description |
|---|---|
| `cendb_kv_put(db, k, kn, v, vn)` | Insert or overwrite a key. |
| `cendb_kv_get(db, k, kn, out_val)` | Look up a key. Returns `CEN_ERR_NOT_FOUND` if missing. |

### Time-Series fast path

| Function | Description |
|---|---|
| `cendb_ts_append(db, ts, series_id, value)` | Append a reading. |
| `cendb_ts_flush(db)` | Flush pending readings to sealed blocks. |
| `cendb_ts_range_count(db, lo, hi, out_count)` | Count readings in `[lo, hi]`. |

### Bulk query

| Function | Description |
|---|---|
| `cendb_query_arrow(db, query, out_result)` | Run a CenQL query; return Arrow-style result. |
| `cendb_arrow_result_free(result)` | Free an Arrow result. |

### Memory management

| Function | Description |
|---|---|
| `cendb_bytes_free(b)` | Free a `CenBytes` returned by `cendb_kv_get`. |

### Errors

| Function | Description |
|---|---|
| `cendb_last_error_message()` | Thread-local error string (valid until next FFI call). |
| `cendb_clear_last_error()` | Clear the thread-local error. |
| `cendb_version()` | Library version string (statically allocated; do not free). |

## Status codes

| Code | Meaning | Retryable? |
|---|---|---|
| `CEN_OK` (0) | Success | — |
| `CEN_ERR_NOT_FOUND` (1) | Key not found | No |
| `CEN_ERR_CONSTRAINT` (2) | Schema / constraint violation | No |
| `CEN_ERR_CONFLICT` (3) | MVCC abort | **Yes** (caller may retry) |
| `CEN_ERR_IO` (4) | I/O error | Depends |
| `CEN_ERR_CORRUPT` (5) | On-disk data corrupt | No |
| `CEN_ERR_SYNTAX` (6) | CenQL syntax error | No |
| `CEN_ERR_INTERNAL` (99) | Internal error / panic | No |

## Python bindings

```python
import sys
sys.path.insert(0, '/path/to/cendb/bindings/python')
import cendb

db = cendb.open()  # in-memory
db.kv_put(b"alice", b"password123")
print(db.kv_get(b"alice"))  # b"password123"
print(cendb.version())
db.close()
```

Set `CENDB_LIB_PATH` to point at `libcendb_ffi.so` if it's not on the
library path. See [`bindings/python/cendb.py`](../bindings/python/cendb.py).

## Go bindings

```go
package main

import (
    "fmt"
    "log"
    "path/to/cendb/bindings/go/cendb"
)

func main() {
    db, err := cendb.Open("", cendb.DefaultConfig())
    if err != nil { log.Fatal(err) }
    defer db.Close()

    db.KVPut([]byte("alice"), []byte("password123"))
    val, _ := db.KVGet([]byte("alice"))
    fmt.Printf("value: %s\n", val)
}
```

Build with:

```bash
CGO_LDFLAGS="-L/path/to/cendb/target/release -lcendb_ffi" go build ./...
```

See [`bindings/go/cendb/cendb.go`](../bindings/go/cendb/cendb.go).

## Node.js bindings

```javascript
const cendb = require('./cendb.js');
const db = cendb.open();
db.kvPut(Buffer.from('alice'), Buffer.from('password123'));
console.log(db.kvGet(Buffer.from('alice')));
db.close();
```

Install dependencies with `npm install ffi-napi ref-napi`. Set
`LD_LIBRARY_PATH` so Node.js can find `libcendb_ffi`. See
[`bindings/nodejs/cendb.js`](../bindings/nodejs/cendb.js).

## Thread safety

Each `CenDb*` handle is **not** thread-safe. Use one handle per thread,
or wrap calls in a mutex. The thread-local last-error slot is per-thread
and does not require synchronisation.

## Memory ownership

| Returned by | Owner | Free with |
|---|---|---|
| `CenDb*` from `cendb_open` | Rust | `cendb_close` |
| `CenBytes` from `cendb_kv_get` | Rust | `cendb_bytes_free` |
| `CenArrowResult` from `cendb_query_arrow` | Rust | `cendb_arrow_result_free` |
| `const char*` from `cendb_last_error_message` | Rust (thread-local) | (auto; valid until next call) |
| `const char*` from `cendb_version` | Rust (static) | (never free) |

## Arrow C Data Interface (planned)

For zero-copy bulk transfer to pandas/Polars/Arrow, the spec calls for
exposing the Arrow C Data Interface (`ArrowArray`/`ArrowSchema`). The
current `cendb_query_arrow` returns a simplified result; a production
version would return proper Arrow batches.
