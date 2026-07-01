import os
import sys
import time
import shutil
import tempfile
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor

# Setup lib path
here = Path(__file__).resolve().parent
candidates = [
    here.parent.parent / "target" / "release" / "cendb_ffi.dll",
    here.parent.parent / "target" / "debug" / "deps" / "cendb_ffi.dll",
    here.parent.parent / "target" / "release" / "deps" / "cendb_ffi.dll",
]

lib_path = None
for c in candidates:
    if c.exists():
        lib_path = c
        break

if not lib_path:
    # try looking with standard platform suffixes
    for path_dir in [here.parent.parent / "target" / "release", here.parent.parent / "target" / "debug" / "deps"]:
        for ext in [".dll", ".so", ".dylib"]:
            for name in ["cendb_ffi", "libcendb_ffi"]:
                candidate = path_dir / f"{name}{ext}"
                if candidate.exists():
                    lib_path = candidate
                    break
            if lib_path:
                break
        if lib_path:
            break

if not lib_path:
    raise RuntimeError("Could not find cendb_ffi shared library!")

os.environ["CENDB_LIB_PATH"] = str(lib_path)
sys.path.append(str(here))

import cendb

def run_kv_stress():
    print("--- Starting CenDB Python KV Stress Benchmark ---")
    print(f"Using shared library at: {lib_path}")
    
    # Create temp directory for persistence testing
    db_dir = Path(tempfile.mkdtemp(prefix="cendb_py_stress_"))
    db_path = str(db_dir)
    print(f"Database directory: {db_path}")

    # Open database
    db = cendb.open(db_path)

    num_threads = 8
    ops_per_thread = 5000
    total_ops = num_threads * ops_per_thread

    def worker(thread_idx):
        latencies = []
        errors = 0
        for i in range(ops_per_thread):
            key = f"thread_{thread_idx}_key_{i}".encode()
            val = f"val_{thread_idx}_{i}".encode()
            
            start = time.perf_counter()
            try:
                db.kv_put(key, val)
                got = db.kv_get(key)
                if got != val:
                    errors += 1
            except Exception as e:
                errors += 1
            latencies.append(time.perf_counter() - start)
        return latencies, errors

    start_time = time.perf_counter()
    with ThreadPoolExecutor(max_workers=num_threads) as executor:
        results = list(executor.map(worker, range(num_threads)))
    end_time = time.perf_counter()

    db.close()

    total_time = end_time - start_time
    throughput = total_ops / total_time

    all_latencies = []
    total_errors = 0
    for lats, errs in results:
        all_latencies.extend(lats)
        total_errors += errs

    all_latencies.sort()
    p50 = all_latencies[int(len(all_latencies) * 0.50)] * 1000
    p95 = all_latencies[int(len(all_latencies) * 0.95)] * 1000
    p99 = all_latencies[int(len(all_latencies) * 0.99)] * 1000

    print(f"\nResults:")
    print(f"  Total Operations: {total_ops}")
    print(f"  Elapsed Time:     {total_time:.2f} seconds")
    print(f"  Throughput:       {throughput:.2f} ops/sec")
    print(f"  P50 Latency:      {p50:.3f} ms")
    print(f"  P95 Latency:      {p95:.3f} ms")
    print(f"  P99 Latency:      {p99:.3f} ms")
    print(f"  Error count:      {total_errors}")

    # Reopen to verify disk persistence
    print("\nVerifying disk persistence...")
    db_reopened = cendb.open(db_path)
    persistence_errors = 0
    for tid in range(num_threads):
        # Sample keys
        for i in [0, ops_per_thread // 2, ops_per_thread - 1]:
            key = f"thread_{tid}_key_{i}".encode()
            expected = f"val_{tid}_{i}".encode()
            got = db_reopened.kv_get(key)
            if got != expected:
                persistence_errors += 1
    db_reopened.close()

    if persistence_errors == 0:
        print("  Durable readback verification: PASSED")
    else:
        print(f"  Durable readback verification: FAILED ({persistence_errors} errors)")

    # Clean up
    shutil.rmtree(db_dir)
    print("--- Benchmark finished ---")
    
    assert total_errors == 0, f"Expected 0 worker errors, got {total_errors}"
    assert persistence_errors == 0, f"Expected 0 persistence verification errors, got {persistence_errors}"

if __name__ == "__main__":
    run_kv_stress()
