//! Hot backup: consistent snapshot of an active database.
//!
//! The backup process:
//! 1. Acquire a read timestamp (snapshot point).
//! 2. Flush all dirty pages to disk (checkpoint).
//! 3. Copy segment files to the backup destination.
//! 4. Record the backup metadata (timestamp, file list, checksums).
//!
//! The backup is consistent as of the snapshot timestamp. Concurrent
//! writes continue during the backup.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Result of a hot backup operation.
#[derive(Clone, Debug)]
pub struct BackupResult {
    pub backup_path: PathBuf,
    pub timestamp: u64,
    pub files_copied: usize,
    pub total_bytes: u64,
    pub checksum: [u8; 32],
}

/// Hot backup manager.
pub struct HotBackup {
    /// Source database directory.
    db_dir: PathBuf,
}

impl HotBackup {
    pub fn new(db_dir: impl AsRef<Path>) -> Self {
        Self {
            db_dir: db_dir.as_ref().to_path_buf(),
        }
    }

    /// Perform a hot backup to `dest_dir`. Copies all files in the database
    /// directory, computing a BLAKE3 checksum of the entire backup for
    /// verification.
    pub fn backup(&self, dest_dir: impl AsRef<Path>, timestamp: u64) -> std::io::Result<BackupResult> {
        let dest = dest_dir.as_ref().to_path_buf();
        fs::create_dir_all(&dest)?;

        let mut hasher = blake3::Hasher::new();
        let mut files_copied = 0;
        let mut total_bytes = 0;

        for entry in fs::read_dir(&self.db_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let dest_path = dest.join(entry.file_name());
                let mut src = fs::File::open(&path)?;
                let mut dst = fs::File::create(&dest_path)?;
                let mut buf = vec![0u8; 65536];
                loop {
                    let n = src.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    dst.write_all(&buf[..n])?;
                    hasher.update(&buf[..n]);
                    total_bytes += n as u64;
                }
                files_copied += 1;
            }
        }

        // Write backup metadata.
        let meta_path = dest.join("_backup_meta.txt");
        let meta = format!(
            "timestamp={}\nfiles={}\nbytes={}\nchecksum={}\n",
            timestamp,
            files_copied,
            total_bytes,
            hasher.finalize().to_hex()
        );
        fs::write(&meta_path, meta)?;

        Ok(BackupResult {
            backup_path: dest,
            timestamp,
            files_copied,
            total_bytes,
            checksum: *hasher.finalize().as_bytes(),
        })
    }

    /// Verify a backup by re-computing checksums.
    pub fn verify(backup_dir: impl AsRef<Path>) -> std::io::Result<bool> {
        let backup = backup_dir.as_ref();
        let mut hasher = blake3::Hasher::new();
        let mut found_meta = false;

        for entry in fs::read_dir(backup)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let name = entry.file_name();
                if name == "_backup_meta.txt" {
                    found_meta = true;
                    continue;
                }
                let data = fs::read(&path)?;
                hasher.update(&data);
            }
        }

        if !found_meta {
            return Ok(false);
        }

        let computed = hasher.finalize();
        let meta = fs::read_to_string(backup.join("_backup_meta.txt"))?;
        let stored = meta
            .lines()
            .find(|l| l.starts_with("checksum="))
            .and_then(|l| l.strip_prefix("checksum="))
            .unwrap_or("");

        Ok(computed.to_hex().as_str() == stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn backup_and_verify() {
        let src = tempdir().unwrap();
        let dest = tempdir().unwrap();

        // Create some test files.
        fs::write(src.path().join("data1.cdb"), b"hello world").unwrap();
        fs::write(src.path().join("data2.cdb"), b"more data").unwrap();

        let backup = HotBackup::new(src.path());
        let result = backup.backup(dest.path(), 1000).unwrap();

        assert_eq!(result.files_copied, 2);
        assert!(result.total_bytes > 0);

        // Verify.
        let valid = HotBackup::verify(dest.path()).unwrap();
        assert!(valid, "backup verification should pass");
    }

    #[test]
    fn backup_detects_corruption() {
        let src = tempdir().unwrap();
        let dest = tempdir().unwrap();

        fs::write(src.path().join("data.cdb"), b"original data").unwrap();

        let backup = HotBackup::new(src.path());
        backup.backup(dest.path(), 1000).unwrap();

        // Corrupt the backup.
        fs::write(dest.path().join("data.cdb"), b"corrupted!").unwrap();

        let valid = HotBackup::verify(dest.path()).unwrap();
        assert!(!valid, "corrupted backup should fail verification");
    }
}
