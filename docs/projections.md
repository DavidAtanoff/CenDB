# Multi-Model Projections

CenDB's projection layer (`cendb-projection`) maps the six logical data
models from the spec onto the unified PAX storage substrate. Each
projection is a thin layer that:

1. Defines its own column schema (`Vec<ColumnSpec>`).
2. Provides typed write/append methods that build PAX blocks.
3. Provides typed read/scan methods that consume PAX blocks.

## The five projections

| Projection | Schema | Index | Special feature |
|---|---|---|---|
| **KV** | `(pk_hash i64, key bytes, value bytes)` | In-memory hash | Point-lookup fast path; bypasses planner |
| **Relational** | User-defined | In-memory PK hash | Columnar scan via `scan_column_i64` |
| **Document** | Single `bytes` column holding HexDoc | — | O(1) field offset table |
| **Time-Series** | `(ts, series_id, value)` | Per-block zone map | Block-skipping range scans |
| **Graph** | Nodes block + Edges block | CSR overlay | O(1) neighbor enumeration |

## Key-Value projection

A degenerate 2-column table `(key BYTES, value BYTES)` with an in-memory
hash index for O(1) point lookups.

```rust
use cendb_projection::KvStore;
use cendb_core::SegmentId;

let mut store = KvStore::new(SegmentId(1), 64 * 1024);
store.put(b"alice", b"password123")?;
store.put(b"bob", b"hunter2")?;
store.seal()?;

assert_eq!(store.get(b"alice")?, Some(b"password123".to_vec()));
```

The PK column stores `hash_key(key)` (FNV-1a) so the block has a numeric
zone map for range pruning.

### Persistence

```rust
store.persist_to_segment("kv.cdb")?;

let loaded = KvStore::load_from_segment("kv.cdb", SegmentId(1), 64 * 1024)?;
```

`persist_to_segment` writes all sealed blocks via `SegmentWriter` and
seals the segment. `load_from_segment` reads the segment back and
rebuilds the in-memory key index.

## Relational projection

A schema-bound table: a sequence of PAX blocks sharing the same column
schema. Point lookups use the in-memory PK index; range scans stream
over blocks and decode columns lazily.

```rust
use cendb_projection::{RelationalTable, relational::TableSchema};
use cendb_storage::header::ColumnSpec;
use cendb_core::{SegmentId, Value, ValueKind};

let schema = TableSchema::new("users", vec![
    ColumnSpec::new(0, ValueKind::I64).pk(),
    ColumnSpec::new(1, ValueKind::Bytes),
    ColumnSpec::new(2, ValueKind::I64),
]);

let mut table = RelationalTable::new(schema, SegmentId(1), 64 * 1024)?;
table.insert(vec![Value::I64(42), Value::Bytes(b"alice".to_vec()), Value::I64(30)])?;
table.flush_pending()?;

let row = table.find_by_pk(42)?.unwrap();
let ages = table.scan_column_i64(2)?;  // columnar scan, returns Vec<i64>
```

The "columnar projection" benefit of PAX: `scan_column_i64(col_idx)`
decodes only that column's minipage, skipping the others. A scan over a
3-column table that only touches one column reads ~1/3 the bytes.

## Document projection (HexDoc)

HexDoc is CenDB's binary JSON format with an O(1) field offset table.
The layout:

```
┌──────────────────────────────────────────────────────┐
│ Header (20 bytes)                                    │
│  magic, root_kind, _pad, field_count, root_off,      │
│  string_pool_off                                     │
├──────────────────────────────────────────────────────┤
│ FieldOffsetTable (field_count × 8 bytes)             │
│  each entry: (name_id: u32, value_off: u32)          │
├──────────────────────────────────────────────────────┤
│ String pool (length-prefixed UTF-8 strings)          │
├──────────────────────────────────────────────────────┤
│ Values region (variable-width, tagged)               │
└──────────────────────────────────────────────────────┘
```

To read `doc["user"]["address"]["city"]` we walk the offset table in
O(1) per level — no full parse of the document.

```rust
use cendb_projection::{DocValue, HexDocBuilder, HexDoc};

let doc = DocValue::Object(vec![
    ("user".to_string(), DocValue::Object(vec![
        ("id".to_string(), DocValue::I64(42)),
        ("address".to_string(), DocValue::Object(vec![
            ("city".to_string(), DocValue::Str("Berlin".to_string())),
        ])),
    ])),
]);

let bytes = HexDocBuilder::encode(&doc)?;
let reader = HexDoc::new(&bytes)?;
let city = reader.get_path("user.address.city")?.unwrap();
// → DocValue::Str("Berlin")
```

### Two storage strategies (planned)

The spec calls for adaptive per-collection storage:
- **Shredded** (default for stable schemas): nested JSON is path-
  shredded into virtual columns using Dremel-style repetition/definition
  levels.
- **Blob** (for wild schemas): the document is stored as a compressed
  HexDoc blob with O(1) field access.

The current implementation provides the Blob strategy; Shredded is
future work.

## Time-Series projection

A relational table with a mandatory timestamp partitioning key. The
novelty is in the *reader*: range scans consult the in-memory block
directory's zone map and skip entire blocks whose zone map doesn't
overlap the query range.

```rust
use cendb_projection::{TimeSeriesSchema, TimeSeriesStore};
use cendb_storage::header::ColumnSpec;
use cendb_core::{SegmentId, ValueKind};

let schema = TimeSeriesSchema {
    ts_col_id: 0,
    series_col_id: 1,
    extra_cols: vec![ColumnSpec::new(2, ValueKind::F64)],
};

let mut store = TimeSeriesStore::new(schema, SegmentId(1), 256 * 1024);
for ts in 0..10_000i64 {
    store.append(ts, 1, (ts as f64).sin())?;
}
store.flush_pending()?;

let (touched, results) = store.range_scan(5000, 5099)?;
// touched < total_blocks — zone map skipping saved I/O.
```

For a query `WHERE ts BETWEEN X AND Y` this often reduces I/O from
O(N blocks) to O(blocks overlapping [X, Y]).

Per-series filtering uses both zone maps (ts + series_id):

```rust
let (touched, results) = store.range_scan_for_series(42, 1000, 2000)?;
```

## Graph projection (CSR overlay)

Nodes + edges stored as PAX blocks, plus an in-memory Compressed
Sparse Row (CSR) overlay for O(1) neighbor enumeration.

```
┌─────────────────────────────────────────────────┐
│ Nodes PAX block(s): (node_id, label, props)     │
├─────────────────────────────────────────────────┤
│ Edges PAX block(s): (src, dst, type, props)     │
│  - sorted by (src, type)                        │
├─────────────────────────────────────────────────┤
│ CSR overlay (in memory):                        │
│   offsets:   Vec<u64>     len = N+1             │
│   adjacency: Vec<NodeId>  len = E               │
│   edge_refs: Vec<EdgeRef> len = E (parallel)    │
└─────────────────────────────────────────────────┘
```

The CSR overlay is the spec's "index-free adjacency" mechanism: given
node `u`, `adjacency[offsets[u]..offsets[u+1]]` is the list of `u`'s
out-neighbors — a contiguous slice, no index lookup needed.

```rust
use cendb_projection::GraphProjection;
use cendb_core::{NodeId, SegmentId};

let mut g = GraphProjection::new(SegmentId(1), 256 * 1024);
g.add_edge(NodeId(0), NodeId(1), "follows");
g.add_edge(NodeId(0), NodeId(2), "follows");
g.add_edge(NodeId(1), NodeId(3), "follows");
g.add_edge(NodeId(2), NodeId(3), "follows");
g.flush()?;
g.build_csr()?;

let neighbors = g.neighbors(NodeId(0))?;
// → [NodeId(1), NodeId(2)]

let two_hop = g.two_hop(NodeId(0))?;
// → [NodeId(3)]  (0 → 1 → 3 and 0 → 2 → 3)

let bfs = g.bfs(NodeId(0), 10)?;
// → [(0, NodeId(0)), (1, NodeId(1)), (1, NodeId(2)), (2, NodeId(3))]
```

This gives BFS/DFS/shortest-path the pointer-chasing locality of a
native graph DB while reusing the columnar substrate for edge/node
*properties* (so `WHERE edge.weight > 5` is a columnar predicate).

## Choosing a projection

| If your workload... | Use... |
|---|---|
| is pure key → value lookups | KV |
| has a fixed schema and mixed point/range queries | Relational |
| has variable/nested JSON | Document |
| is timestamped and append-mostly | Time-Series |
| involves traversing relationships | Graph |

Multiple projections can coexist in the same database — they share the
buffer pool, segment manager, and WAL.
