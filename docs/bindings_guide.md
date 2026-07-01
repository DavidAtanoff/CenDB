# CenDB FFI Bindings Integration Guide

CenDB exposes its core engine capabilities through a stable C-compatible ABI (Application Binary Interface) defined in the FFI crate (`cendb-ffi`). This guide explains how to construct high-performance bindings for other host environments (e.g., Python, Node.js, Go) using the FFI.

## 1. Core Principles

All bindings consuming the FFI must adhere to the following memory and safety invariants:

* **Opaque Handles Only:** The host environment must never allocate or inspect the inner structures of CenDB handles directly. It must only pass and store handles as opaque pointers (e.g., `void*` in C/C++, `ctypes.c_void_p` in Python).
* **Rust-Allocated Memory Ownership:** When Rust allocates memory and returns it to the caller (such as dynamic strings or byte arrays), the caller **must** return the memory back to the engine using the corresponding `hex_*_free` functions (e.g., `hex_bytes_free`, `hex_arrow_result_free`) to prevent memory leaks.
* **Error Handling & Thread Safety:** Every FFI function returns an integer status code (`HexStatus`). If the status is not `0` (OK), the caller can retrieve the detailed error message for the current calling thread by calling `hex_last_error_message()`. This uses thread-local storage under the hood, making error reporting fully thread-safe.
* **Panic Isolation:** The FFI intercepts Rust panics internally and converts them to `HexStatus::ErrInternal` (code `99`), ensuring that crashes do not cause memory corruption across the language boundary.

---

## 2. API Reference

### Database Lifecycle

```c
// Open a database at the given path.
int hex_open(const char* path, const void* config, void** out_db);

// Close the database and release all associated resources.
int hex_close(void* db);
```

### Key-Value Store

```c
// Put a key-value pair.
int hex_kv_put(void* db, const uint8_t* k, size_t kn, const uint8_t* v, size_t vn);

// Retrieve a key-value pair. Out bytes must be freed via hex_bytes_free.
int hex_kv_get(void* db, const uint8_t* k, size_t kn, HexBytes* out_val);
```

### Graph Database (CSR Index)

```c
// Add a node with a label.
int hex_graph_add_node(void* db, uint64_t node_id, const char* label);

// Add an edge between src and dst.
int hex_graph_add_edge(void* db, uint64_t src, uint64_t dst, const char* label);

// Execute a Breadth-First Search. Out bytes must be freed via hex_bytes_free.
int hex_graph_bfs(void* db, uint64_t start_node, uint32_t depth, HexBytes* out_result);
```

### Document Store (HexDoc Binary JSON)

```c
// Insert or update a HexDoc.
int hex_doc_put(void* db, const uint8_t* key, size_t key_len, const uint8_t* doc_bytes, size_t doc_len);

// Query a nested field value from a HexDoc. Out bytes must be freed via hex_bytes_free.
int hex_doc_get_field(void* db, const uint8_t* key, size_t key_len, const char* field_path, HexBytes* out_val);
```
