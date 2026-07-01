import os
import sys
import time
import statistics
from pathlib import Path

# Setup the library path environment variable for CenDB
here = Path(__file__).resolve().parent
lib_path = here.parent.parent / "target" / "release" / "cendb_ffi.dll"
if not lib_path.exists():
    # Try dynamic extensions / lib names
    for ext in [".dll", ".so", ".dylib"]:
        candidate = here.parent.parent / "target" / "release" / f"cendb_ffi{ext}"
        if candidate.exists():
            lib_path = candidate
            break
        candidate = here.parent.parent / "target" / "release" / f"libcendb_ffi{ext}"
        if candidate.exists():
            lib_path = candidate
            break

os.environ["CENDB_LIB_PATH"] = str(lib_path)
sys.path.append(str(here))

import cendb

def benchmark_kv(db, num_ops=50000):
    print(f"\n--- Benchmarking Key-Value Store ({num_ops:,} operations) ---")
    
    # 1. KV Write (Put)
    keys = [f"key_{i}".encode() for i in range(num_ops)]
    values = [f"val_{i}".encode() for i in range(num_ops)]
    
    start_time = time.perf_counter()
    latencies = []
    
    for i in range(num_ops):
        op_start = time.perf_counter()
        db.kv_put(keys[i], values[i])
        latencies.append(time.perf_counter() - op_start)
        
    end_time = time.perf_counter()
    duration = end_time - start_time
    write_tps = num_ops / duration
    
    # Latency stats in ms
    latencies_ms = [l * 1000 for l in latencies]
    avg_lat = statistics.mean(latencies_ms)
    p95_lat = statistics.quantiles(latencies_ms, n=20)[18]  # 95th percentile
    p99_lat = statistics.quantiles(latencies_ms, n=100)[98] # 99th percentile
    
    print(f"Write Throughput: {write_tps:.2f} ops/sec")
    print(f"Write Latency: Avg={avg_lat:.4f}ms, P95={p95_lat:.4f}ms, P99={p99_lat:.4f}ms")
    
    # 2. KV Read (Get)
    start_time = time.perf_counter()
    latencies = []
    
    for i in range(num_ops):
        op_start = time.perf_counter()
        val = db.kv_get(keys[i])
        latencies.append(time.perf_counter() - op_start)
        assert val == values[i], f"Expected {values[i]}, got {val}"
        
    end_time = time.perf_counter()
    duration = end_time - start_time
    read_tps = num_ops / duration
    
    latencies_ms = [l * 1000 for l in latencies]
    avg_lat = statistics.mean(latencies_ms)
    p95_lat = statistics.quantiles(latencies_ms, n=20)[18]
    p99_lat = statistics.quantiles(latencies_ms, n=100)[98]
    
    print(f"Read Throughput:  {read_tps:.2f} ops/sec")
    print(f"Read Latency:  Avg={avg_lat:.4f}ms, P95={p95_lat:.4f}ms, P99={p99_lat:.4f}ms")

def benchmark_ts(db, num_ops=100000, num_queries=1000):
    print(f"\n--- Benchmarking Time-Series Store ({num_ops:,} appends, {num_queries:,} queries) ---")
    
    # 1. TS Append
    start_time = time.perf_counter()
    latencies = []
    
    for i in range(num_ops):
        op_start = time.perf_counter()
        db.ts_append(ts=i, series_id=1, value=float(i * 1.5))
        latencies.append(time.perf_counter() - op_start)
        
    # Flush pending
    flush_start = time.perf_counter()
    db.ts_flush()
    flush_duration = time.perf_counter() - flush_start
    
    end_time = time.perf_counter()
    duration = end_time - start_time
    append_tps = num_ops / duration
    
    latencies_ms = [l * 1000 for l in latencies]
    avg_lat = statistics.mean(latencies_ms)
    p95_lat = statistics.quantiles(latencies_ms, n=20)[18]
    p99_lat = statistics.quantiles(latencies_ms, n=100)[98]
    
    print(f"Append Throughput: {append_tps:.2f} ops/sec (including flush: {flush_duration*1000:.2f}ms)")
    print(f"Append Latency: Avg={avg_lat:.4f}ms, P95={p95_lat:.4f}ms, P99={p99_lat:.4f}ms")
    
    # 2. TS Range Queries
    # We query ranges of size 1000 points
    query_ranges = [(i, i + 1000) for i in range(0, num_ops - 1000, (num_ops - 1000) // num_queries)]
    query_ranges = query_ranges[:num_queries]
    
    start_time = time.perf_counter()
    latencies = []
    
    for lo, hi in query_ranges:
        op_start = time.perf_counter()
        count = db.ts_range_count(lo, hi)
        latencies.append(time.perf_counter() - op_start)
        # range count logic is inclusive/exclusive depending on Rust side
        assert count > 0, "Expected non-zero range count"
        
    end_time = time.perf_counter()
    duration = end_time - start_time
    query_qps = len(query_ranges) / duration
    
    latencies_ms = [l * 1000 for l in latencies]
    avg_lat = statistics.mean(latencies_ms)
    p95_lat = statistics.quantiles(latencies_ms, n=20)[18]
    p99_lat = statistics.quantiles(latencies_ms, n=100)[98]
    
    print(f"Range Query QPS: {query_qps:.2f} queries/sec")
    print(f"Query Latency:  Avg={avg_lat:.4f}ms, P95={p95_lat:.4f}ms, P99={p99_lat:.4f}ms")

if __name__ == "__main__":
    print(f"CenDB Version: {cendb.version()}")
    print("Opening database...")
    db = cendb.open()
    try:
        benchmark_kv(db)
        benchmark_ts(db)
    finally:
        db.close()
        print("Database closed.")
