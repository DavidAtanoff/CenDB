//! ChaosVfs: in-memory virtual file system with fault injection.
//!
//! Wraps all file I/O in a mock that counts operations and can inject
//! failures at precise points. The VFS maintains an in-memory file system
//! (`HashMap<String, Vec<u8>>`) and optionally a "durable" copy that
//! represents what would survive a crash (updated only on `fsync`).

use std::collections::HashMap;

use crate::controller::{ChaosController, FaultType};

/// VFS error type.
#[derive(Debug, Clone)]
pub enum VfsError {
    /// Injected I/O error.
    InjectedIo,
    /// File not found.
    NotFound,
    /// Operation would go out of bounds.
    OutOfBounds,
}

impl std::fmt::Display for VfsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VfsError::InjectedIo => write!(f, "injected I/O error"),
            VfsError::NotFound => write!(f, "file not found"),
            VfsError::OutOfBounds => write!(f, "offset out of bounds"),
        }
    }
}

impl std::error::Error for VfsError {}

pub type VfsResult<T> = Result<T, VfsError>;

/// In-memory virtual file system with fault injection.
///
/// Files are stored as `Vec<u8>`. The VFS maintains two copies:
///   * `volatile`: the in-memory working copy (what the process sees).
///   * `durable`: what would survive a crash (updated only on `fsync`).
///
/// On "crash" (simulated by dropping the VFS and re-opening), the durable
/// copy is loaded. If `fsync` was never called (or silently failed), the
/// durable copy is stale.
pub struct ChaosVfs {
    /// Volatile (in-memory) file storage.
    volatile: HashMap<String, Vec<u8>>,
    /// Durable (post-fsync) file storage. Updated only on successful fsync.
    durable: HashMap<String, Vec<u8>>,
    /// The chaos controller.
    controller: ChaosController,
}

impl ChaosVfs {
    pub fn new(controller: ChaosController) -> Self {
        Self {
            volatile: HashMap::new(),
            durable: HashMap::new(),
            controller,
        }
    }

    /// Create a new file.
    pub fn create(&mut self, path: &str) -> VfsResult<()> {
        let op = self.controller.next_op();
        let faults = self.controller.check_faults(op, path);
        for fault in &faults {
            match fault {
                FaultType::IoError => return Err(VfsError::InjectedIo),
                _ => {}
            }
        }
        self.volatile.insert(path.to_string(), Vec::new());
        Ok(())
    }

    /// Check if a file exists (in volatile storage).
    pub fn exists(&self, path: &str) -> bool {
        self.volatile.contains_key(path)
    }

    /// Write data at an offset. Applies fault injection.
    pub fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> VfsResult<()> {
        let op = self.controller.next_op();
        let faults = self.controller.check_faults(op, path);

        let file = self
            .volatile
            .get_mut(path)
            .ok_or(VfsError::NotFound)?;

        for fault in &faults {
            match fault {
                FaultType::IoError => return Err(VfsError::InjectedIo),
                FaultType::TornWrite { bytes } => {
                    // Write only the first `bytes` bytes, then fail.
                    let torn = data.iter().take(*bytes).copied().collect::<Vec<_>>();
                    let off = offset as usize;
                    while file.len() < off + torn.len() {
                        file.resize(off + torn.len(), 0);
                    }
                    file[off..off + torn.len()].copy_from_slice(&torn);
                    return Err(VfsError::InjectedIo);
                }
                FaultType::CorruptData { byte_index } => {
                    // Write data, then flip a byte.
                    let off = offset as usize;
                    while file.len() < off + data.len() {
                        file.resize(off + data.len(), 0);
                    }
                    file[off..off + data.len()].copy_from_slice(data);
                    if *byte_index < data.len() {
                        file[off + *byte_index] ^= 0xFF;
                    }
                    return Ok(());
                }
                FaultType::SilentFsyncFail => {
                    // This fault only applies to fsync, not write.
                }
            }
        }

        // Normal write.
        let off = offset as usize;
        while file.len() < off + data.len() {
            file.resize(off + data.len(), 0);
        }
        file[off..off + data.len()].copy_from_slice(data);
        Ok(())
    }

    /// Append data to the end of a file.
    pub fn append(&mut self, path: &str, data: &[u8]) -> VfsResult<()> {
        let len = self.file_len(path)?;
        self.write(path, len, data)
    }

    /// Read data from a file.
    pub fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> VfsResult<()> {
        let file = self
            .volatile
            .get(path)
            .ok_or(VfsError::NotFound)?;
        let off = offset as usize;
        if off + buf.len() > file.len() {
            return Err(VfsError::OutOfBounds);
        }
        buf.copy_from_slice(&file[off..off + buf.len()]);
        Ok(())
    }

    /// Read the entire file.
    pub fn read_all(&self, path: &str) -> VfsResult<Vec<u8>> {
        let file = self
            .volatile
            .get(path)
            .ok_or(VfsError::NotFound)?;
        Ok(file.clone())
    }

    /// Fsync a file. Copies volatile → durable unless a fault is injected.
    pub fn fsync(&mut self, path: &str) -> VfsResult<()> {
        let op = self.controller.next_op();
        let faults = self.controller.check_faults(op, path);

        for fault in &faults {
            match fault {
                FaultType::IoError => return Err(VfsError::InjectedIo),
                FaultType::SilentFsyncFail => {
                    // Pretend success but don't copy to durable.
                    return Ok(());
                }
                _ => {}
            }
        }

        // Normal fsync: copy volatile → durable.
        if let Some(data) = self.volatile.get(path) {
            self.durable.insert(path.to_string(), data.clone());
        }
        Ok(())
    }

    /// Get the length of a file.
    pub fn file_len(&self, path: &str) -> VfsResult<u64> {
        self.volatile
            .get(path)
            .map(|f| f.len() as u64)
            .ok_or(VfsError::NotFound)
    }

    /// Simulate a crash: replace volatile with the durable copy.
    /// Any writes since the last successful fsync are lost.
    pub fn crash_and_reboot(&mut self) {
        self.volatile = self.durable.clone();
        // Reset the controller for the recovery phase.
        self.controller.reset();
    }

    /// Get a reference to the controller.
    pub fn controller(&self) -> &ChaosController {
        &self.controller
    }

    /// Get a mutable reference to the controller.
    pub fn controller_mut(&mut self) -> &mut ChaosController {
        &mut self.controller
    }

    /// Snapshot of the durable state (for verification).
    pub fn durable_snapshot(&self) -> &HashMap<String, Vec<u8>> {
        &self.durable
    }

    /// Snapshot of the volatile state.
    pub fn volatile_snapshot(&self) -> &HashMap<String, Vec<u8>> {
        &self.volatile
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vfs_basic_write_read() {
        let ctrl = ChaosController::new();
        let mut vfs = ChaosVfs::new(ctrl);
        vfs.create("test.dat").unwrap();
        vfs.write("test.dat", 0, b"hello").unwrap();
        let mut buf = [0u8; 5];
        vfs.read("test.dat", 0, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn vfs_fsync_makes_data_durable() {
        let mut ctrl = ChaosController::new();
        let mut vfs = ChaosVfs::new(ctrl);
        vfs.create("test.dat").unwrap();
        vfs.write("test.dat", 0, b"persistent").unwrap();
        vfs.fsync("test.dat").unwrap();

        // Crash — data should survive.
        vfs.crash_and_reboot();
        assert!(vfs.exists("test.dat"));
        let data = vfs.read_all("test.dat").unwrap();
        assert_eq!(data, b"persistent");
    }

    #[test]
    fn vfs_data_without_fsync_is_lost_on_crash() {
        let ctrl = ChaosController::new();
        let mut vfs = ChaosVfs::new(ctrl);
        vfs.create("test.dat").unwrap();
        vfs.write("test.dat", 0, b"volatile").unwrap();
        // No fsync!

        vfs.crash_and_reboot();
        // File should not exist in durable storage.
        assert!(!vfs.exists("test.dat"));
    }

    #[test]
    fn vfs_injected_io_error() {
        let mut ctrl = ChaosController::new();
        ctrl.fail_at(1); // Fail on the second operation (write).
        let mut vfs = ChaosVfs::new(ctrl);
        vfs.create("test.dat").unwrap(); // op 0
        let result = vfs.write("test.dat", 0, b"fail me"); // op 1
        assert!(matches!(result, Err(VfsError::InjectedIo)));
    }

    #[test]
    fn vfs_torn_write() {
        let mut ctrl = ChaosController::new();
        ctrl.torn_write_at(1, 4); // Write only 4 bytes of 8.
        let mut vfs = ChaosVfs::new(ctrl);
        vfs.create("test.dat").unwrap(); // op 0
        let result = vfs.write("test.dat", 0, b"01234567"); // op 1
        assert!(matches!(result, Err(VfsError::InjectedIo)));
        // Verify partial write.
        let data = vfs.read_all("test.dat").unwrap();
        assert_eq!(&data, b"0123");
    }

    #[test]
    fn vfs_silent_fsync_failure() {
        let mut ctrl = ChaosController::new();
        ctrl.silent_fsync_at(2); // fsync is op 2 (create=0, write=1, fsync=2)
        let mut vfs = ChaosVfs::new(ctrl);
        vfs.create("test.dat").unwrap(); // op 0
        vfs.write("test.dat", 0, b"should persist").unwrap(); // op 1
        vfs.fsync("test.dat").unwrap(); // op 2 — silently fails

        // Crash — data should be lost because fsync didn't actually persist.
        vfs.crash_and_reboot();
        assert!(!vfs.exists("test.dat"));
    }

    #[test]
    fn vfs_data_corruption() {
        let mut ctrl = ChaosController::new();
        ctrl.corrupt_at(1, 0); // Corrupt byte 0 of the write data.
        let mut vfs = ChaosVfs::new(ctrl);
        vfs.create("test.dat").unwrap(); // op 0
        vfs.write("test.dat", 0, b"ABCD").unwrap(); // op 1 — corrupted
        let data = vfs.read_all("test.dat").unwrap();
        // Byte 0 should be flipped.
        assert_ne!(&data, b"ABCD");
        assert_eq!(data[1], b'B'); // Other bytes intact.
    }
}
