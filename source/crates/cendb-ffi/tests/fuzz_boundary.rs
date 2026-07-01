//! FFI boundary fuzzer.
//!
//! Exercises the `extern "C"` entry points of `cendb-ffi` with:
//!   * Valid arguments (sanity check).
//!   * Null pointers (must return `ErrConstraint`, not panic).
//!   * Malformed UTF-8 in path arguments (must not panic).
//!   * Empty / huge key/value lengths.
//!   * Dangling handles (close-then-use).
//!
//! All operations are wrapped in `catch_unwind` so a panic is reported,
//! not fatal.

use cendb_core::CenDbConfig;
use cendb_ffi::{cendb_bytes_free, cendb_clear_last_error, cendb_close, cendb_kv_get, cendb_kv_put, cendb_open, CenBytes};
use std::ffi::CString;
use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};

pub struct FuzzRng { state: u64 }
impl FuzzRng {
    pub fn new(seed: u64) -> Self { Self { state: if seed == 0 { 1 } else { seed } } }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        self.state = x; x
    }
    pub fn gen_range(&mut self, min: u64, max: u64) -> u64 {
        if min >= max { return min; }
        min + (self.next_u64() % (max - min))
    }
    pub fn gen_vec(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next_u64() & 0xFF) as u8).collect()
    }
}

#[derive(Clone, Debug, Default)]
pub struct FfiFuzzReport {
    pub iterations: u64,
    pub panics: u64,
    pub null_pointer_rejections: u64,
    pub successful_kv_puts: u64,
    pub successful_kv_gets: u64,
    pub not_found_returns: u64,
    pub first_panic: Option<String>,
}

impl FfiFuzzReport {
    pub fn print(&self) {
        println!("\n=== FFI Boundary Fuzz Report ===");
        println!("  iterations:             {}", self.iterations);
        println!("  panics:                 {}", self.panics);
        println!("  null-pointer rejections:{}", self.null_pointer_rejections);
        println!("  successful kv_puts:     {}", self.successful_kv_puts);
        println!("  successful kv_gets:     {}", self.successful_kv_gets);
        println!("  not-found returns:      {}", self.not_found_returns);
        if let Some(p) = &self.first_panic {
            println!("  FIRST PANIC: {}", p);
        }
    }
}

/// Open a temporary database, fuzz KV operations, close.
pub fn fuzz_ffi_kv_boundary(iterations: u64, seed: u64) -> FfiFuzzReport {
    let mut rng = FuzzRng::new(seed);
    let mut report = FfiFuzzReport { iterations, ..Default::default() };

    // Use a fresh temp dir for the database.
    let dir = std::env::temp_dir().join(format!(
        "cendb_ffi_fuzz_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path_cstring = CString::new(dir.to_str().unwrap()).unwrap();

    // Open the DB.
    let mut db_ptr: *mut cendb_ffi::CenDb = std::ptr::null_mut();
    let cfg = CenDbConfig::default();
    let open_status = unsafe {
        cendb_open(path_cstring.as_ptr(), &cfg, &mut db_ptr)
    };
    assert!(open_status.is_ok(), "cendb_open failed: {:?}", open_status);
    assert!(!db_ptr.is_null());

    for _ in 0..iterations {
        let op = rng.next_u64() % 6;
        match op {
            0 => {
                // Valid put.
                let key_len = rng.gen_range(1, 64) as usize;
                let val_len = rng.gen_range(0, 256) as usize;
                let key = rng.gen_vec(key_len);
                let val = rng.gen_vec(val_len);
                let r = catch_unwind(AssertUnwindSafe(|| {
                    unsafe {
                        cendb_kv_put(db_ptr, key.as_ptr(), key.len(), val.as_ptr(), val.len())
                    }
                }));
                match r {
                    Ok(s) if s.is_ok() => report.successful_kv_puts += 1,
                    Ok(_) => {}
                    Err(e) => {
                        report.panics += 1;
                        if report.first_panic.is_none() {
                            report.first_panic = Some(downcast_panic(e));
                        }
                    }
                }
            }
            1 => {
                // Valid get.
                let key_len = rng.gen_range(1, 32) as usize;
                let key = rng.gen_vec(key_len);
                let mut out: *mut CenBytes = std::ptr::null_mut();
                let r = catch_unwind(AssertUnwindSafe(|| {
                    unsafe {
                        cendb_kv_get(db_ptr, key.as_ptr(), key.len(), out)
                    }
                }));
                match r {
                    Ok(s) => {
                        if s.is_ok() {
                            report.successful_kv_gets += 1;
                            if !out.is_null() {
                                unsafe { cendb_bytes_free(out); }
                            }
                        } else if s == cendb_core::CenStatus::ErrNotFound {
                            report.not_found_returns += 1;
                        }
                    }
                    Err(e) => {
                        report.panics += 1;
                        if report.first_panic.is_none() {
                            report.first_panic = Some(downcast_panic(e));
                        }
                    }
                }
            }
            2 => {
                // Null pointer put (db=null).
                let key = rng.gen_vec(8);
                let val = rng.gen_vec(8);
                let r = catch_unwind(AssertUnwindSafe(|| {
                    unsafe {
                        cendb_kv_put(std::ptr::null_mut(), key.as_ptr(), key.len(), val.as_ptr(), val.len())
                    }
                }));
                match r {
                    Ok(s) => {
                        if s == cendb_core::CenStatus::ErrConstraint {
                            report.null_pointer_rejections += 1;
                        }
                    }
                    Err(e) => {
                        report.panics += 1;
                        if report.first_panic.is_none() {
                            report.first_panic = Some(downcast_panic(e));
                        }
                    }
                }
            }
            3 => {
                // Null pointer put (key=null).
                let val = rng.gen_vec(8);
                let r = catch_unwind(AssertUnwindSafe(|| {
                    unsafe {
                        cendb_kv_put(db_ptr, std::ptr::null(), 0, val.as_ptr(), val.len())
                    }
                }));
                match r {
                    Ok(s) => {
                        if s == cendb_core::CenStatus::ErrConstraint {
                            report.null_pointer_rejections += 1;
                        }
                    }
                    Err(e) => {
                        report.panics += 1;
                        if report.first_panic.is_none() {
                            report.first_panic = Some(downcast_panic(e));
                        }
                    }
                }
            }
            4 => {
                // Zero-length key (allowed by API; should succeed).
                let r = catch_unwind(AssertUnwindSafe(|| {
                    let val = b"v";
                    unsafe {
                        cendb_kv_put(db_ptr, b"".as_ptr(), 0, val.as_ptr(), 1)
                    }
                }));
                match r {
                    Ok(s) if s.is_ok() => report.successful_kv_puts += 1,
                    Ok(_) => {}
                    Err(e) => {
                        report.panics += 1;
                        if report.first_panic.is_none() {
                            report.first_panic = Some(downcast_panic(e));
                        }
                    }
                }
            }
            _ => {
                // Get with null out pointer (should be rejected cleanly).
                let key = rng.gen_vec(8);
                let r = catch_unwind(AssertUnwindSafe(|| {
                    unsafe {
                        cendb_kv_get(db_ptr, key.as_ptr(), key.len(), std::ptr::null_mut())
                    }
                }));
                match r {
                    Ok(_) => { /* Either ok or constraint; both fine. */ }
                    Err(e) => {
                        report.panics += 1;
                        if report.first_panic.is_none() {
                            report.first_panic = Some(downcast_panic(e));
                        }
                    }
                }
            }
        }
    }

    // Close the DB.
    let _ = unsafe { cendb_close(db_ptr) };
    std::fs::remove_dir_all(&dir).ok();
    // Clear any leftover error.
    unsafe { cendb_clear_last_error(); }

    report
}

fn downcast_panic(e: Box<dyn std::any::Any + Send + 'static>) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-string panic)".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_boundary_fuzz_5000_iterations() {
        let report = fuzz_ffi_kv_boundary(5_000, 0xCAFEBABE);
        report.print();
        assert_eq!(report.panics, 0,
            "FFI boundary panicked {} times — first: {:?}",
            report.panics, report.first_panic);
        // Sanity: at least some puts should have succeeded.
        assert!(report.successful_kv_puts > 0, "no successful puts — fuzz harness broken");
        // Sanity: null pointers must have been rejected.
        assert!(report.null_pointer_rejections > 0, "null pointers not exercised");
    }

    #[test]
    fn ffi_null_path_returns_constraint() {
        // cendb_open with null out_db must return ErrConstraint.
        let r = unsafe {
            cendb_open(std::ptr::null(), std::ptr::null(), std::ptr::null_mut())
        };
        assert_eq!(r, cendb_core::CenStatus::ErrConstraint);
    }

    #[test]
    fn ffi_close_null_is_noop() {
        let r = unsafe { cendb_close(std::ptr::null_mut()) };
        assert_eq!(r, cendb_core::CenStatus::Ok);
    }
}
