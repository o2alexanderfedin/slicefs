//! `StoreIo` — implements `blockset::Io` backed by a directory on disk.
//!
//! The store root directory contains blob files named with their blockset
//! relative paths (e.g., `vt0/<base32>`). `StoreIo` simply joins those
//! relative filenames with the store root to produce absolute paths.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use blockset::Io;

/// Blockset `Io` implementation backed by a filesystem directory.
///
/// Blob files are stored as `<store_root>/<filename>` where `filename` is
/// the relative path produced by blockset (e.g., `vt0/ABCDEF`).
pub struct StoreIo {
    root: PathBuf,
    /// While an online GC runs: the name of each object read or written.
    /// The sweep keeps these, because a writer can depend on an object that
    /// the mark found unreachable (a dedup hit: blockset reads the existing
    /// object and does not write it again).
    touched: Option<HashSet<String>>,
    /// Writes over an existing object file while tracking was on. The online
    /// GC mark reads without the lock, so it must not trust a run in which an
    /// object changed (see `OnlineGc::sweep`).
    overwrites_while_tracking: u64,
    /// Counter for unique temp file names (atomic writes).
    tmp_seq: u64,
    /// Files written since the last `sync_written` (and their directories):
    /// a commit must make them durable before the root that refers to them.
    unsynced: Vec<PathBuf>,
    unsynced_dirs: HashSet<PathBuf>,
    /// Number of `sync_written` calls that had something to sync (for tests).
    syncs: u64,
}

impl StoreIo {
    /// Create a new `StoreIo` rooted at the given directory.
    ///
    /// The directory does not need to exist yet; `write` will create it.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), touched: None, overwrites_while_tracking: 0, tmp_seq: 0, unsynced: Vec::new(), unsynced_dirs: HashSet::new(), syncs: 0 }
    }

    /// The store directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Start to record each object name that is read or written.
    pub fn start_tracking(&mut self) {
        self.touched = Some(HashSet::new());
        self.overwrites_while_tracking = 0;
    }

    /// Files written since the last `sync_written`.
    pub fn unsynced_count(&self) -> usize {
        self.unsynced.len()
    }

    /// Number of syncs that had files to sync.
    pub fn sync_count(&self) -> u64 {
        self.syncs
    }

    /// Make every file written since the last call durable, with the
    /// directory entries that name them. A commit calls this before it writes
    /// the root (a power loss must not leave a root that names objects that
    /// never reached the disk). Linux: one `syncfs`. Other systems: `fsync`
    /// of each file and directory, then one `F_FULLFSYNC` on macOS (a plain
    /// macOS fsync does not flush the drive cache).
    pub fn sync_written(&mut self) -> io::Result<()> {
        if self.unsynced.is_empty() {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let dir = fs::File::open(&self.root)?;
            // SAFETY: syncfs on a valid fd we own.
            if unsafe { libc::syncfs(dir.as_raw_fd()) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            use std::os::fd::AsRawFd;
            for path in self.unsynced.iter().chain(self.unsynced_dirs.iter()) {
                match fs::File::open(path) {
                    // SAFETY: plain fsync on a valid fd we own.
                    Ok(f) => {
                        if unsafe { libc::fsync(f.as_raw_fd()) } != 0 {
                            return Err(io::Error::last_os_error());
                        }
                    }
                    // Deleted since (GC, a newer write): nothing to sync.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            #[cfg(target_os = "macos")]
            {
                let dir = fs::File::open(&self.root)?;
                // SAFETY: fcntl F_FULLFSYNC on a valid fd we own.
                if unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_FULLFSYNC) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        self.unsynced.clear();
        self.unsynced_dirs.clear();
        self.syncs += 1;
        Ok(())
    }

    /// Writes over an existing object file since tracking started.
    pub fn overwrites_while_tracking(&self) -> u64 {
        self.overwrites_while_tracking
    }

    /// Stop recording, and drop the record.
    pub fn stop_tracking(&mut self) {
        self.touched = None;
    }

    /// True if tracking is on and `name` was read or written since it started.
    pub fn is_touched(&self, name: &str) -> bool {
        self.touched.as_ref().is_some_and(|t| t.contains(name))
    }

    /// Full path for a blockset-relative filename.
    fn full_path(&self, filename: &str) -> PathBuf {
        self.root.join(filename)
    }
}

/// Iterator type for `StoreIo::args` — always empty.
pub struct EmptyArgs;

impl Iterator for EmptyArgs {
    type Item = String;
    fn next(&mut self) -> Option<String> {
        None
    }
}

impl Io for StoreIo {
    type Args = EmptyArgs;

    fn read(&mut self, filename: &str) -> io::Result<Vec<u8>> {
        if let Some(t) = &mut self.touched {
            t.insert(filename.to_string());
        }
        fs::read(self.full_path(filename))
    }

    /// Write atomically: a reader sees the old file or the new one, never a
    /// part. (The online GC mark reads objects without the lock.)
    fn write(&mut self, filename: &str, data: &[u8]) -> io::Result<()> {
        let path = self.full_path(filename);
        if let Some(t) = &mut self.touched {
            t.insert(filename.to_string());
            if path.exists() {
                self.overwrites_while_tracking += 1;
            }
        }
        let Some(parent) = path.parent() else {
            return fs::write(path, data);
        };
        self.tmp_seq += 1;
        let tmp = parent.join(format!(".tmp-{}-{}", std::process::id(), self.tmp_seq));
        // Create the directory only when it is missing: a create_dir_all per
        // object costs a stat/mkdir per object write (#4).
        match fs::write(&tmp, data) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(parent)?;
                fs::write(&tmp, data)?;
            }
            r => r?,
        }
        fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        self.unsynced_dirs.insert(parent.to_path_buf());
        self.unsynced.push(path);
        Ok(())
    }

    fn args(&self) -> EmptyArgs {
        EmptyArgs
    }

    fn print(&mut self, _text: &str) {
        // no-op: blockset print output is not used in SliceFS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_store_io_roundtrip() {
        let dir = TempDir::new().unwrap();
        let mut io = StoreIo::new(dir.path());

        let data = b"hello, SliceFS!";
        io.write("vt0/testblob", data).unwrap();

        let read_back = io.read("vt0/testblob").unwrap();
        assert_eq!(read_back, data);
    }

    #[test]
    fn test_store_io_creates_subdirs() {
        let dir = TempDir::new().unwrap();
        let mut io = StoreIo::new(dir.path());

        io.write("a/b/c/blob", b"data").unwrap();
        let read_back = io.read("a/b/c/blob").unwrap();
        assert_eq!(read_back, b"data");
    }

    /// The directory is created again if it was removed after a write (the
    /// write does not depend on a remembered "exists" state).
    #[test]
    fn test_store_io_recreates_a_removed_dir() {
        let dir = TempDir::new().unwrap();
        let mut io = StoreIo::new(dir.path());
        io.write("vt0/a", b"1").unwrap();
        fs::remove_dir_all(dir.path().join("vt0")).unwrap();
        io.write("vt0/b", b"2").unwrap();
        assert_eq!(io.read("vt0/b").unwrap(), b"2");
    }

    #[test]
    fn test_store_io_missing_file() {
        let dir = TempDir::new().unwrap();
        let mut io = StoreIo::new(dir.path());
        let result = io.read("nonexistent");
        assert!(result.is_err());
    }
}
