//! Mount lock file management for crash-safe dirty-mount detection.
//!
//! A `mount.lock` file is created in the store directory when a mount begins.
//! If SliceFS starts and finds an existing `mount.lock`, it knows the previous
//! mount exited uncleanly (crash) and triggers WAL replay.
//!
//! The lock file is removed automatically when `MountLock` is dropped (RAII).

use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors returned by mount lock operations.
#[derive(Debug, Error)]
pub enum MountLockError {
    /// A `mount.lock` file already exists — previous mount exited uncleanly.
    #[error("dirty mount detected: mount.lock already exists")]
    DirtyMount,
    /// I/O error while creating or removing the lock file.
    #[error("mount lock I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// RAII guard for `mount.lock`. Removes the file when dropped.
pub struct MountLock {
    lock_path: PathBuf,
}

impl Drop for MountLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

/// Create a `mount.lock` file in `store_path`.
///
/// Returns `Err(MountLockError::DirtyMount)` if `mount.lock` already exists,
/// indicating a previous unclean shutdown. The caller should perform WAL replay
/// before proceeding with the mount.
///
/// Returns a `MountLock` RAII guard that removes the file on drop.
pub fn acquire_mount_lock(store_path: &Path) -> Result<MountLock, MountLockError> {
    let lock_path = store_path.join("mount.lock");

    if lock_path.exists() {
        return Err(MountLockError::DirtyMount);
    }

    std::fs::write(&lock_path, b"")?;
    Ok(MountLock { lock_path })
}
