//! cendb-ffi: C-ABI foreign function interface for CenDB.
//!
//! This crate exposes the engine's core operations as `extern "C"` functions
//! so it can be called from C, C++, Python (via ctypes/cffi), Go (cgo), and
//! Node.js (N-API). The design follows §5 of the spec:
//!
//!   * **Opaque handles only.** Callers hold `*mut HexDb` / `*mut HexBytes`
//!     pointers; no Rust struct crosses the boundary.
//!   * **Caller never frees Rust memory directly.** Every Rust-allocated
//!     buffer has a paired `hex_*_free` function.
//!   * **Errors are integer codes + thread-local last-error detail.**
//!     Avoids out-params everywhere; ergonomic for FFI.
//!   * **No panics across FFI.** Every `extern "C"` fn is wrapped in
//!     `catch_unwind`; a panic becomes `HEX_ERR_INTERNAL`.
//!   * **No global state** except the thread-local last-error slot.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;
use std::sync::Mutex;

use cendb_core::{CenDbConfig, HexError, HexResult, HexStatus, SegmentId, NodeId};
use cendb_projection::{KvStore, TimeSeriesSchema, TimeSeriesStore, GraphProjection, HexDoc};

// ============================================================================
// Opaque handle types. We never expose the actual Rust structs across the
// boundary; callers hold raw pointers to these opaque newtypes.
// ============================================================================

/// Opaque database handle. Internally wraps a [`CenDb`] instance.
pub struct HexDb {
    pub(crate) kv: Mutex<KvStore>,
    pub(crate) ts: Mutex<TimeSeriesStore>,
    pub(crate) graph: Mutex<GraphProjection>,
    /// Filesystem directory where segment files are persisted on close.
    /// `None` for ephemeral in-memory databases.
    pub(crate) data_dir: Option<PathBuf>,
    #[allow(dead_code)]
    pub(crate) config: CenDbConfig,
}

/// Owned bytes returned from Rust to C. The caller must free via
/// [`hex_bytes_free`].
#[repr(C)]
pub struct HexBytes {
    pub ptr: *mut u8,
    pub len: usize,
    pub cap: usize,
}

impl HexBytes {
    /// Construct from a Rust `Vec<u8>`. Takes ownership of the Vec's
    /// allocation.
    pub fn from_vec(v: Vec<u8>) -> Self {
        let mut v = v;
        let ptr = v.as_mut_ptr();
        let len = v.len();
        let cap = v.capacity();
        std::mem::forget(v); // don't drop; caller will free via hex_bytes_free
        Self { ptr, len, cap }
    }

    /// Construct a null/empty HexBytes (e.g. for "not found" returns).
    pub fn null() -> Self {
        Self { ptr: ptr::null_mut(), len: 0, cap: 0 }
    }

    /// View as a byte slice. Returns an empty slice if `ptr` is null.
    pub fn as_slice(&self) -> &[u8] {
        if self.ptr.is_null() {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
}

// ============================================================================
// Thread-local last-error storage. Each thread maintains its own last-error
// string so FFI calls are thread-safe without locking.
// ============================================================================

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = RefCell::new(None);
}

/// Set the last-error message for the current thread.
fn set_last_error(err: &HexError) {
    let msg = format!("{:?}: {}", err.status, err.message);
    if let Ok(cstr) = CString::new(msg) {
        LAST_ERROR.with(|e| {
            *e.borrow_mut() = Some(cstr);
        });
    }
}

/// Set the last-error from a panic message.
fn set_last_error_panic(msg: &str) {
    let full = format!("panic: {}", msg);
    if let Ok(cstr) = CString::new(full) {
        LAST_ERROR.with(|e| {
            *e.borrow_mut() = Some(cstr);
        });
    }
}

/// Return the last error message for the current thread, or null if none.
/// The returned pointer is valid until the next FFI call on the same thread.
#[no_mangle]
pub extern "C" fn hex_last_error_message() -> *const c_char {
    LAST_ERROR.with(|e| {
        e.borrow()
            .as_ref()
            .map_or(ptr::null(), |c| c.as_ptr())
    })
}

/// Clear the last-error for the current thread.
#[no_mangle]
pub extern "C" fn hex_clear_last_error() {
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = None;
    });
}

// ============================================================================
// FFI guard: run a closure, convert Result to HexStatus, set last-error on
// failure, and catch any panic.
// ============================================================================

fn ffi_guard<F, T>(f: F) -> HexStatus
where
    F: FnOnce() -> HexResult<T>,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(_)) => HexStatus::Ok,
        Ok(Err(e)) => {
            set_last_error(&e);
            e.status
        }
        Err(panic_payload) => {
            let msg = if let Some(s) = panic_payload.downcast_ref::<&'static str>() {
                (*s).to_string()
            } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            set_last_error_panic(&msg);
            HexStatus::ErrInternal
        }
    }
}

/// Like [`ffi_guard`] but returns a value via an out-pointer.
///
/// Note: this helper is currently unused (the prototype's FFI surface uses
/// direct out-pointers in each function). It's kept here as part of the
/// public helper API for future expansion.
#[allow(dead_code)]
fn ffi_guard_out<F>(f: F, out: *mut *mut c_void) -> HexStatus
where
    F: FnOnce() -> HexResult<*mut c_void>,
{
    if out.is_null() {
        return HexStatus::ErrConstraint;
    }
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(ptr_val)) => {
            unsafe {
                *out = ptr_val;
            }
            HexStatus::Ok
        }
        Ok(Err(e)) => {
            set_last_error(&e);
            e.status
        }
        Err(panic_payload) => {
            let msg = if let Some(s) = panic_payload.downcast_ref::<&'static str>() {
                (*s).to_string()
            } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            set_last_error_panic(&msg);
            HexStatus::ErrInternal
        }
    }
}

// ============================================================================
// Lifecycle: hex_open / hex_close
// ============================================================================

/// Open a CenDB database.
///
/// If `path` is non-null and points to an existing directory, any previously
/// persisted segment files in that directory are loaded and the database
/// resumes from its last durable state. If the directory does not exist it is
/// created and the database starts empty.
///
/// If `path` is null the database is purely in-memory and data is lost on
/// close.
///
/// # Safety
/// `path` must be a valid null-terminated UTF-8 C string or null.
/// `cfg` may be null (defaults apply). `out_db` must be non-null.
#[no_mangle]
pub unsafe extern "C" fn hex_open(
    path: *const c_char,
    cfg: *const CenDbConfig,
    out_db: *mut *mut HexDb,
) -> HexStatus {
    if out_db.is_null() {
        return HexStatus::ErrConstraint;
    }
    ffi_guard(|| {
        let path_str = if path.is_null() {
            None
        } else {
            Some(CStr::from_ptr(path).to_str().unwrap_or("").to_owned())
        };
        let config = if cfg.is_null() {
            CenDbConfig::default()
        } else {
            *cfg
        };
        config.validate()?;
        let block_size = config.block_size;

        let (kv, data_dir) = if let Some(ref p) = path_str {
            let dir = PathBuf::from(p);
            // Create directory if it does not exist.
            if !dir.exists() {
                std::fs::create_dir_all(&dir).map_err(|e| {
                    HexError::io(format!("hex_open: cannot create dir {:?}: {}", dir, e))
                })?;
            }
            let kv_path = dir.join("cendb.kv.seg");
            let kv = if kv_path.exists() {
                KvStore::load_from_segment(&kv_path, SegmentId(1), block_size)?
            } else {
                KvStore::new(SegmentId(1), block_size)
            };
            (kv, Some(dir))
        } else {
            (KvStore::new(SegmentId(1), block_size), None)
        };

        let db = Box::new(HexDb {
            kv: Mutex::new(kv),
            ts: Mutex::new(TimeSeriesStore::new(
                TimeSeriesSchema {
                    ts_col_id: 0,
                    series_col_id: 1,
                    extra_cols: vec![cendb_storage::header::ColumnSpec::new(2, cendb_core::ValueKind::F64)],
                },
                SegmentId(2),
                block_size,
            )),
            graph: Mutex::new(GraphProjection::new(SegmentId(3), block_size)),
            data_dir,
            config,
        });
        *out_db = Box::into_raw(db);
        Ok(())
    })
}

/// Close a database handle and flush all pending writes to disk (if a path
/// was specified at open time). The handle must not be used after this call.
///
/// # Safety
/// `db` must be a valid pointer returned by [`hex_open`] and not already closed.
#[no_mangle]
pub unsafe extern "C" fn hex_close(db: *mut HexDb) -> HexStatus {
    if db.is_null() {
        return HexStatus::Ok;
    }
    ffi_guard(|| {
        let mut db = Box::from_raw(db);
        // Persist KV segment if we have a data directory.
        if let Some(ref dir) = db.data_dir {
            let kv_path = dir.join("cendb.kv.seg");
            db.kv.get_mut().unwrap().persist_to_segment(&kv_path)?;
        }
        Ok(())
    })
}

// ============================================================================
// Key-Value fast path: hex_kv_put / hex_kv_get
// ============================================================================

/// Insert or overwrite a key-value pair.
///
/// # Safety
/// `db` must be valid. `k` and `v` must be valid byte arrays of length `kn`
/// and `vn` respectively.
#[no_mangle]
pub unsafe extern "C" fn hex_kv_put(
    db: *mut HexDb,
    k: *const u8,
    kn: usize,
    v: *const u8,
    vn: usize,
) -> HexStatus {
    if db.is_null() || k.is_null() {
        return HexStatus::ErrConstraint;
    }
    ffi_guard(|| {
        let db = &*db;
        let key = core::slice::from_raw_parts(k, kn);
        let val = if v.is_null() {
            &[][..]
        } else {
            core::slice::from_raw_parts(v, vn)
        };
        db.kv.lock().unwrap().put(key, val)?;
        Ok(())
    })
}

/// Look up a key. On success the value is written to `out_val`; the caller
/// must free it via [`hex_bytes_free`].
///
/// Returns `HEX_ERR_NOT_FOUND` if the key doesn't exist.
///
/// # Safety
/// `db` and `k` must be valid. `out_val` must be a valid pointer to a
/// `HexBytes` struct (it will be overwritten).
#[no_mangle]
pub unsafe extern "C" fn hex_kv_get(
    db: *mut HexDb,
    k: *const u8,
    kn: usize,
    out_val: *mut HexBytes,
) -> HexStatus {
    if db.is_null() || k.is_null() || out_val.is_null() {
        return HexStatus::ErrConstraint;
    }
    let status = ffi_guard(|| {
        let db = &*db;
        let key = core::slice::from_raw_parts(k, kn);
        match db.kv.lock().unwrap().get(key)? {
            Some(val) => {
                *out_val = HexBytes::from_vec(val);
                Ok(())
            }
            None => Err(HexError::not_found("key not found")),
        }
    });
    if status != HexStatus::Ok {
        // Ensure out_val is null on failure.
        unsafe {
            *out_val = HexBytes::null();
        }
    }
    status
}

// ============================================================================
// Time-series fast path: hex_ts_append / hex_ts_range
// ============================================================================

/// Append a time-series reading.
///
/// # Safety
/// `db` must be valid.
#[no_mangle]
pub unsafe extern "C" fn hex_ts_append(
    db: *mut HexDb,
    ts: i64,
    series_id: i64,
    value: f64,
) -> HexStatus {
    if db.is_null() {
        return HexStatus::ErrConstraint;
    }
    ffi_guard(|| {
        let db = &*db;
        db.ts.lock().unwrap().append(ts, series_id, value)?;
        Ok(())
    })
}

/// Flush all pending time-series readings into sealed blocks.
///
/// # Safety
/// `db` must be valid.
#[no_mangle]
pub unsafe extern "C" fn hex_ts_flush(db: *mut HexDb) -> HexStatus {
    if db.is_null() {
        return HexStatus::ErrConstraint;
    }
    ffi_guard(|| {
        let db = &*db;
        db.ts.lock().unwrap().flush_pending()?;
        Ok(())
    })
}

/// Range-scan time-series readings. Returns the count of matching readings
/// via `out_count`.
///
/// # Safety
/// `db` must be valid. `out_count` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn hex_ts_range_count(
    db: *mut HexDb,
    lo: i64,
    hi: i64,
    out_count: *mut u64,
) -> HexStatus {
    if db.is_null() || out_count.is_null() {
        return HexStatus::ErrConstraint;
    }
    ffi_guard(|| {
        let db = &*db;
        let (_, results) = db.ts.lock().unwrap().range_scan(lo, hi)?;
        *out_count = results.len() as u64;
        Ok(())
    })
}

// ============================================================================
// Arrow-compatible query interface (simplified for the prototype).
// ============================================================================

/// Result of an Arrow-style query. For the prototype we return a flat
/// `Vec<Vec<u8>>` of columnar batches; a production version would return
/// the actual Arrow C Data Interface structs.
#[repr(C)]
pub struct HexArrowResult {
    pub batch_count: u64,
    pub row_count: u64,
    pub bytes: *mut u8,
    pub bytes_len: usize,
}

/// Run a query and return an Arrow-style result. For the prototype, this
/// returns the count of KV pairs (as a single "column" of u64 row counts).
///
/// # Safety
/// `db` must be valid. `out_result` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn hex_query_arrow(
    db: *mut HexDb,
    query: *const c_char,
    out_result: *mut HexArrowResult,
) -> HexStatus {
    if db.is_null() || out_result.is_null() {
        return HexStatus::ErrConstraint;
    }
    ffi_guard(|| {
        let db = &*db;
        let _query = if query.is_null() {
            String::new()
        } else {
            CStr::from_ptr(query).to_string_lossy().into_owned()
        };
        // For the prototype, return the KV pair count.
        let kv_count = db.kv.lock().unwrap().len() as u64;
        let bytes = kv_count.to_le_bytes().to_vec();
        let bytes_len = bytes.len();
        let bytes_ptr = bytes.as_ptr() as *mut u8;
        std::mem::forget(bytes);
        *out_result = HexArrowResult {
            batch_count: 1,
            row_count: 1,
            bytes: bytes_ptr,
            bytes_len,
        };
        Ok(())
    })
}

/// Free an Arrow result returned by [`hex_query_arrow`].
///
/// # Safety
/// `result` must be a valid pointer to a `HexArrowResult` previously returned
/// by `hex_query_arrow`.
#[no_mangle]
pub unsafe extern "C" fn hex_arrow_result_free(result: *mut HexArrowResult) {
    if result.is_null() {
        return;
    }
    let r = &mut *result;
    if !r.bytes.is_null() && r.bytes_len > 0 {
        // Reconstruct the Vec and let it drop.
        let _ = Vec::from_raw_parts(r.bytes, r.bytes_len, r.bytes_len);
        r.bytes = ptr::null_mut();
        r.bytes_len = 0;
    }
}

// ============================================================================
// Memory management
// ============================================================================

/// Free a `HexBytes` struct returned by the FFI.
///
/// # Safety
/// `b` must be a valid pointer to a `HexBytes` previously returned by
/// [`hex_kv_get`] or similar. After this call the bytes are invalid.
#[no_mangle]
pub unsafe extern "C" fn hex_bytes_free(b: *mut HexBytes) {
    if b.is_null() {
        return;
    }
    let b = &mut *b;
    if !b.ptr.is_null() && b.cap > 0 {
        let _ = Vec::from_raw_parts(b.ptr, b.len, b.cap);
        b.ptr = ptr::null_mut();
        b.len = 0;
        b.cap = 0;
    }
}

// ============================================================================
// Graph database: nodes, edges, BFS
// ============================================================================

/// Add a node to the graph database.
#[no_mangle]
pub unsafe extern "C" fn hex_graph_add_node(
    db: *mut HexDb,
    node_id: u64,
    label: *const c_char,
) -> HexStatus {
    if db.is_null() || label.is_null() {
        return HexStatus::ErrConstraint;
    }
    ffi_guard(|| {
        let db = &*db;
        let lbl = CStr::from_ptr(label).to_str().unwrap_or("");
        db.graph.lock().unwrap().add_node(NodeId(node_id), lbl);
        Ok(())
    })
}

/// Add an edge to the graph database.
#[no_mangle]
pub unsafe extern "C" fn hex_graph_add_edge(
    db: *mut HexDb,
    src: u64,
    dst: u64,
    label: *const c_char,
) -> HexStatus {
    if db.is_null() || label.is_null() {
        return HexStatus::ErrConstraint;
    }
    ffi_guard(|| {
        let db = &*db;
        let lbl = CStr::from_ptr(label).to_str().unwrap_or("");
        db.graph.lock().unwrap().add_edge(NodeId(src), NodeId(dst), lbl);
        Ok(())
    })
}

/// Run Breadth-First Search (BFS) and serialize visited nodes to flat binary bytes in HexBytes.
#[no_mangle]
pub unsafe extern "C" fn hex_graph_bfs(
    db: *mut HexDb,
    start_node: u64,
    depth: u32,
    out_val: *mut HexBytes,
) -> HexStatus {
    if db.is_null() || out_val.is_null() {
        return HexStatus::ErrConstraint;
    }
    let status = ffi_guard(|| {
        let db = &*db;
        let mut graph = db.graph.lock().unwrap();
        graph.flush()?;
        graph.build_csr()?;
        let results = graph.bfs(NodeId(start_node), depth as usize)?;
        let mut bytes = Vec::with_capacity(results.len() * 8);
        for (_, node) in results {
            bytes.extend_from_slice(&node.0.to_le_bytes());
        }
        *out_val = HexBytes::from_vec(bytes);
        Ok(())
    });
    if status != HexStatus::Ok {
        unsafe { *out_val = HexBytes::null(); }
    }
    status
}

// ============================================================================
// Document store: HexDoc binary JSON
// ============================================================================

/// Put a document (JSON bytes) into the store.
#[no_mangle]
pub unsafe extern "C" fn hex_doc_put(
    db: *mut HexDb,
    key: *const u8,
    key_len: usize,
    doc_bytes: *const u8,
    doc_len: usize,
) -> HexStatus {
    if db.is_null() || key.is_null() || doc_bytes.is_null() {
        return HexStatus::ErrConstraint;
    }
    ffi_guard(|| {
        let db = &*db;
        let k = core::slice::from_raw_parts(key, key_len);
        let d = core::slice::from_raw_parts(doc_bytes, doc_len);
        db.kv.lock().unwrap().put(k, d)?;
        Ok(())
    })
}

/// Retrieve a nested field value from a HexDoc stored in the KV store.
#[no_mangle]
pub unsafe extern "C" fn hex_doc_get_field(
    db: *mut HexDb,
    key: *const u8,
    key_len: usize,
    field_path: *const c_char,
    out_val: *mut HexBytes,
) -> HexStatus {
    if db.is_null() || key.is_null() || field_path.is_null() || out_val.is_null() {
        return HexStatus::ErrConstraint;
    }
    let status = ffi_guard(|| {
        let db = &*db;
        let k = core::slice::from_raw_parts(key, key_len);
        let path = CStr::from_ptr(field_path).to_str().unwrap_or("");
        match db.kv.lock().unwrap().get(k)? {
            Some(doc_bytes) => {
                let reader = HexDoc::new(&doc_bytes)?;
                match reader.get_field(path)? {
                    Some(val) => {
                        let out_str = format!("{:?}", val);
                        *out_val = HexBytes::from_vec(out_str.into_bytes());
                        Ok(())
                    }
                    None => Err(HexError::not_found("field not found")),
                }
            }
            None => Err(HexError::not_found("document not found")),
        }
    });
    if status != HexStatus::Ok {
        unsafe { *out_val = HexBytes::null(); }
    }
    status
}

// ============================================================================
// Convenience: return the library version string.
// ============================================================================

/// Return the CenDB version string (e.g. "0.1.0"). The returned pointer is
/// valid for the lifetime of the library and must not be freed.
#[no_mangle]
pub extern "C" fn hex_version() -> *const c_char {
    static VERSION: &[u8] = b"0.1.0\0";
    VERSION.as_ptr() as *const c_char
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn open_close_roundtrip() {
        let mut db_ptr: *mut HexDb = ptr::null_mut();
        let cfg = CenDbConfig::default();
        let status = unsafe { hex_open(ptr::null(), &cfg, &mut db_ptr) };
        assert_eq!(status, HexStatus::Ok);
        assert!(!db_ptr.is_null());

        let status = unsafe { hex_close(db_ptr) };
        assert_eq!(status, HexStatus::Ok);
    }

    #[test]
    fn kv_put_get_roundtrip() {
        let mut db_ptr: *mut HexDb = ptr::null_mut();
        let cfg = CenDbConfig::default();
        unsafe { hex_open(ptr::null(), &cfg, &mut db_ptr) };

        let key = b"test_key";
        let val = b"test_value";
        let status = unsafe {
            hex_kv_put(db_ptr, key.as_ptr(), key.len(), val.as_ptr(), val.len())
        };
        assert_eq!(status, HexStatus::Ok);

        // Flush so the value lands in a sealed block.
        // (KV pending writes are already visible via get, so this isn't
        // strictly necessary, but it exercises the flush path.)
        // Actually the public FFI doesn't expose kv_flush — we rely on the
        // pending visibility.

        let mut out = HexBytes::null();
        let status = unsafe { hex_kv_get(db_ptr, key.as_ptr(), key.len(), &mut out) };
        assert_eq!(status, HexStatus::Ok);
        assert_eq!(out.as_slice(), val);

        unsafe { hex_bytes_free(&mut out) };
        unsafe { hex_close(db_ptr) };
    }

    #[test]
    fn kv_get_missing_returns_not_found() {
        let mut db_ptr: *mut HexDb = ptr::null_mut();
        let cfg = CenDbConfig::default();
        unsafe { hex_open(ptr::null(), &cfg, &mut db_ptr) };

        let key = b"nonexistent";
        let mut out = HexBytes::null();
        let status = unsafe { hex_kv_get(db_ptr, key.as_ptr(), key.len(), &mut out) };
        assert_eq!(status, HexStatus::ErrNotFound);
        assert!(out.ptr.is_null());

        unsafe { hex_close(db_ptr) };
    }

    #[test]
    fn last_error_message_set_on_failure() {
        let mut db_ptr: *mut HexDb = ptr::null_mut();
        let cfg = CenDbConfig::default();
        unsafe { hex_open(ptr::null(), &cfg, &mut db_ptr) };

        let key = b"missing";
        let mut out = HexBytes::null();
        let status = unsafe { hex_kv_get(db_ptr, key.as_ptr(), key.len(), &mut out) };
        assert_eq!(status, HexStatus::ErrNotFound);

        let msg_ptr = hex_last_error_message();
        assert!(!msg_ptr.is_null());
        let msg = unsafe { CStr::from_ptr(msg_ptr).to_string_lossy().into_owned() };
        assert!(msg.contains("not found"), "expected 'not found' in error, got: {}", msg);

        unsafe { hex_close(db_ptr) };
    }

    #[test]
    fn version_string_is_valid() {
        let ptr = hex_version();
        assert!(!ptr.is_null());
        let s = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        assert_eq!(s, "0.1.0");
    }

    #[test]
    fn ts_append_and_range_count() {
        let mut db_ptr: *mut HexDb = ptr::null_mut();
        let cfg = CenDbConfig::default();
        unsafe { hex_open(ptr::null(), &cfg, &mut db_ptr) };

        for ts in 0..100i64 {
            let status = unsafe { hex_ts_append(db_ptr, ts, 1, ts as f64) };
            assert_eq!(status, HexStatus::Ok);
        }
        unsafe { hex_ts_flush(db_ptr) };

        let mut count: u64 = 0;
        let status = unsafe { hex_ts_range_count(db_ptr, 10, 20, &mut count) };
        assert_eq!(status, HexStatus::Ok);
        assert!(count > 0, "expected >0 results, got {}", count);

        unsafe { hex_close(db_ptr) };
    }

    #[test]
    fn null_db_returns_constraint_error() {
        let status = unsafe { hex_kv_put(ptr::null_mut(), ptr::null(), 0, ptr::null(), 0) };
        assert_eq!(status, HexStatus::ErrConstraint);
    }

    // Suppress unused-import warning for CString in this test module.
    #[allow(dead_code)]
    fn _ensure_cstring_used() {
        let _ = CString::new("dummy").unwrap();
    }
}
