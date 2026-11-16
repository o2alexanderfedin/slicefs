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
}

impl StoreIo {
    /// Create a new `StoreIo` rooted at the given directory.
    ///
    /// The directory does not need to exist yet; `write` will create it.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), touched: None, overwrites_while_tracking: 0, tmp_seq: 0 }
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
        fs::create_dir_all(parent)?;
        self.tmp_seq += 1;
        let tmp = parent.join(format!(".tmp-{}-{}", std::process::id(), self.tmp_seq));
        fs::write(&tmp, data)?;
        fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
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

    #[test]
    fn test_store_io_missing_file() {
        let dir = TempDir::new().unwrap();
        let mut io = StoreIo::new(dir.path());
        let result = io.read("nonexistent");
        assert!(result.is_err());
    }
}
