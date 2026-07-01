"""ctypes bindings for CenDB.

Usage:

    import cendb
    db = cendb.open("/path/to/db.cdb")
    db.kv_put(b"alice", b"password123")
    print(db.kv_get(b"alice"))  # b"password123"
    print(db.ts_range_count(0, 1000))
    db.close()

This module is a thin wrapper over the C-ABI exposed by `cendb-ffi`. It
requires `libcendb_ffi.so` (Linux), `libcendb_ffi.dylib` (macOS), or
`cendb_ffi.dll` (Windows) to be on the dynamic linker path. Build it with:

    cargo build --release -p cendb-ffi

The resulting shared library is at
`target/release/libcendb_ffi.so` (or platform equivalent).
"""

from __future__ import annotations

import ctypes
import os
import sys
from ctypes import (
    Structure,
    POINTER,
    c_char,
    c_char_p,
    c_int,
    c_uint8,
    c_uint16,
    c_uint32,
    c_uint64,
    c_int64,
    c_double,
    c_size_t,
    byref,
)
from pathlib import Path


# ============================================================================
# ctypes struct definitions (mirror cendb.h).
# ============================================================================


class CenStatus:
    OK = 0
    ERR_NOT_FOUND = 1
    ERR_CONSTRAINT = 2
    ERR_CONFLICT = 3
    ERR_IO = 4
    ERR_CORRUPT = 5
    ERR_SYNTAX = 6
    ERR_INTERNAL = 99


class CenConfig(Structure):
    _fields_ = [
        ("page_size", c_uint32),
        ("block_size", c_uint32),
        ("pool_frames", c_uint32),
        ("group_commit_ms", c_uint32),
        ("flags", c_uint64),
    ]

    @classmethod
    def default(cls) -> "CenConfig":
        return cls(
            page_size=4096,
            block_size=65536,
            pool_frames=1024,
            group_commit_ms=10,
            flags=0,
        )


class CenBytes(Structure):
    _fields_ = [
        ("ptr", POINTER(c_uint8)),
        ("len", c_size_t),
        ("cap", c_size_t),
    ]

    def to_bytes(self) -> bytes:
        if not self.ptr:
            return b""
        return ctypes.string_at(self.ptr, self.len)


class CenArrowResult(Structure):
    _fields_ = [
        ("batch_count", c_uint64),
        ("row_count", c_uint64),
        ("bytes", POINTER(c_uint8)),
        ("bytes_len", c_size_t),
    ]


# ============================================================================
# Library loading.
# ============================================================================


def _load_library() -> ctypes.CDLL:
    """Locate and load the cendb-ffi shared library."""
    # 1. Check CENDB_LIB_PATH env var.
    env_path = os.environ.get("CENDB_LIB_PATH")
    if env_path and os.path.exists(env_path):
        return ctypes.CDLL(env_path)
    # 2. Look in the workspace target directory (dev mode).
    here = Path(__file__).resolve().parent
    for candidate in [
        here.parent.parent / "target" / "release",
        here.parent.parent / "target" / "debug",
        here / "lib",
    ]:
        if sys.platform.startswith("linux"):
            lib = candidate / "libcendb_ffi.so"
        elif sys.platform == "darwin":
            lib = candidate / "libcendb_ffi.dylib"
        elif sys.platform == "win32":
            lib = candidate / "cendb_ffi.dll"
        else:
            continue
        if lib.exists():
            return ctypes.CDLL(str(lib))
    # 3. Fall back to system loader.
    try:
        return ctypes.CDLL("cendb_ffi")
    except OSError as e:
        raise RuntimeError(
            "Could not find libcendb_ffi. Set CENDB_LIB_PATH or build with "
            "`cargo build --release -p cendb-ffi`."
        ) from e


_LIB = _load_library()


# ============================================================================
# Function signatures.
# ============================================================================


_LIB.cendb_open.argtypes = [c_char_p, POINTER(CenConfig), POINTER(ctypes.c_void_p)]
_LIB.cendb_open.restype = c_int

_LIB.cendb_close.argtypes = [ctypes.c_void_p]
_LIB.cendb_close.restype = c_int

_LIB.cendb_kv_put.argtypes = [
    ctypes.c_void_p,
    POINTER(c_uint8),
    c_size_t,
    POINTER(c_uint8),
    c_size_t,
]
_LIB.cendb_kv_put.restype = c_int

_LIB.cendb_kv_get.argtypes = [
    ctypes.c_void_p,
    POINTER(c_uint8),
    c_size_t,
    POINTER(CenBytes),
]
_LIB.cendb_kv_get.restype = c_int

_LIB.cendb_ts_append.argtypes = [ctypes.c_void_p, c_int64, c_int64, c_double]
_LIB.cendb_ts_append.restype = c_int

_LIB.cendb_ts_flush.argtypes = [ctypes.c_void_p]
_LIB.cendb_ts_flush.restype = c_int

_LIB.cendb_ts_range_count.argtypes = [
    ctypes.c_void_p,
    c_int64,
    c_int64,
    POINTER(c_uint64),
]
_LIB.cendb_ts_range_count.restype = c_int

_LIB.cendb_query_arrow.argtypes = [
    ctypes.c_void_p,
    c_char_p,
    POINTER(CenArrowResult),
]
_LIB.cendb_query_arrow.restype = c_int

_LIB.cendb_arrow_result_free.argtypes = [POINTER(CenArrowResult)]
_LIB.cendb_arrow_result_free.restype = None

_LIB.cendb_bytes_free.argtypes = [POINTER(CenBytes)]
_LIB.cendb_bytes_free.restype = None

_LIB.cendb_graph_add_node.argtypes = [ctypes.c_void_p, c_uint64, c_char_p]
_LIB.cendb_graph_add_node.restype = c_int

_LIB.cendb_graph_add_edge.argtypes = [ctypes.c_void_p, c_uint64, c_uint64, c_char_p]
_LIB.cendb_graph_add_edge.restype = c_int

_LIB.cendb_graph_bfs.argtypes = [ctypes.c_void_p, c_uint64, c_uint32, POINTER(CenBytes)]
_LIB.cendb_graph_bfs.restype = c_int

_LIB.cendb_doc_put.argtypes = [ctypes.c_void_p, POINTER(c_uint8), c_size_t, POINTER(c_uint8), c_size_t]
_LIB.cendb_doc_put.restype = c_int

_LIB.cendb_doc_get_field.argtypes = [ctypes.c_void_p, POINTER(c_uint8), c_size_t, c_char_p, POINTER(CenBytes)]
_LIB.cendb_doc_get_field.restype = c_int

_LIB.cendb_last_error_message.argtypes = []
_LIB.cendb_last_error_message.restype = c_char_p

_LIB.cendb_clear_last_error.argtypes = []
_LIB.cendb_clear_last_error.restype = None

_LIB.cendb_version.argtypes = []
_LIB.cendb_version.restype = c_char_p


# ============================================================================
# Errors.
# ============================================================================


class CenDBError(Exception):
    """Raised when an FFI call returns a non-OK status."""

    def __init__(self, status: int, message: str):
        super().__init__(f"CenStatus={status}: {message}")
        self.status = status
        self.message = message


def _check(status: int) -> None:
    if status != CenStatus.OK:
        msg_ptr = _LIB.cendb_last_error_message()
        msg = msg_ptr.decode("utf-8") if msg_ptr else "(no message)"
        raise CenDBError(status, msg)


# ============================================================================
# High-level database handle.
# ============================================================================


class Database:
    """A CenDB database handle."""

    def __init__(self, _handle: int):
        self._handle = _handle

    @classmethod
    def open(cls, path: str | None = None, config: CenConfig | None = None) -> "Database":
        cfg = config or CenConfig.default()
        path_bytes = path.encode("utf-8") if path else None
        handle = ctypes.c_void_p()
        _check(_LIB.cendb_open(path_bytes, byref(cfg), byref(handle)))
        return cls(handle.value)

    def kv_put(self, key: bytes, value: bytes) -> None:
        key_buf = (c_uint8 * len(key))(*key) if key else None
        val_buf = (c_uint8 * len(value))(*value) if value else None
        _check(
            _LIB.cendb_kv_put(
                self._handle,
                key_buf,
                len(key),
                val_buf,
                len(value),
            )
        )

    def kv_get(self, key: bytes) -> bytes | None:
        key_buf = (c_uint8 * len(key))(*key) if key else None
        out = CenBytes()
        status = _LIB.cendb_kv_get(self._handle, key_buf, len(key), byref(out))
        if status == CenStatus.ERR_NOT_FOUND:
            return None
        _check(status)
        try:
            return out.to_bytes()
        finally:
            _LIB.cendb_bytes_free(byref(out))

    def ts_append(self, ts: int, series_id: int, value: float) -> None:
        _check(_LIB.cendb_ts_append(self._handle, ts, series_id, value))

    def ts_flush(self) -> None:
        _check(_LIB.cendb_ts_flush(self._handle))

    def ts_range_count(self, lo: int, hi: int) -> int:
        out = c_uint64(0)
        _check(_LIB.cendb_ts_range_count(self._handle, lo, hi, byref(out)))
        return out.value

    def query_arrow(self, query: str) -> int:
        out = CenArrowResult()
        _check(_LIB.cendb_query_arrow(self._handle, query.encode("utf-8"), byref(out)))
        try:
            return out.row_count
        finally:
            _LIB.cendb_arrow_result_free(byref(out))

    def graph_add_node(self, node_id: int, label: str) -> None:
        _check(_LIB.cendb_graph_add_node(self._handle, node_id, label.encode("utf-8")))

    def graph_add_edge(self, src: int, dst: int, label: str) -> None:
        _check(_LIB.cendb_graph_add_edge(self._handle, src, dst, label.encode("utf-8")))

    def graph_bfs(self, start_node: int, depth: int) -> list[int]:
        out = CenBytes()
        _check(_LIB.cendb_graph_bfs(self._handle, start_node, depth, byref(out)))
        try:
            b = out.to_bytes()
            import struct
            count = len(b) // 8
            return list(struct.unpack(f"<{count}Q", b))
        finally:
            _LIB.cendb_bytes_free(byref(out))

    def doc_put(self, key: bytes, doc_bytes: bytes) -> None:
        key_buf = (c_uint8 * len(key))(*key) if key else None
        doc_buf = (c_uint8 * len(doc_bytes))(*doc_bytes) if doc_bytes else None
        _check(_LIB.cendb_doc_put(self._handle, key_buf, len(key), doc_buf, len(doc_bytes)))

    def doc_get_field(self, key: bytes, field_path: str) -> str | None:
        key_buf = (c_uint8 * len(key))(*key) if key else None
        out = CenBytes()
        status = _LIB.cendb_doc_get_field(self._handle, key_buf, len(key), field_path.encode("utf-8"), byref(out))
        if status == CenStatus.ERR_NOT_FOUND:
            return None
        _check(status)
        try:
            return out.to_bytes().decode("utf-8")
        finally:
            _LIB.cendb_bytes_free(byref(out))

    def close(self) -> None:
        if self._handle:
            _check(_LIB.cendb_close(self._handle))
            self._handle = None

    def __enter__(self) -> "Database":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()


# ============================================================================
# Module-level convenience.
# ============================================================================


def open(path: str | None = None, config: CenConfig | None = None) -> Database:
    """Open a CenDB database. Shorthand for `Database.open`."""
    return Database.open(path, config)


def version() -> str:
    """Return the CenDB library version string."""
    return _LIB.cendb_version().decode("utf-8")


__all__ = [
    "Database",
    "CenConfig",
    "CenStatus",
    "CenDBError",
    "open",
    "version",
]
