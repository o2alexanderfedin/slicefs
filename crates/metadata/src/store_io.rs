//! `StoreIo` — implements `blockset::Io` backed by a directory on disk.
//!
//! The store root directory contains blob files named with their blockset
//! relative paths (e.g., `vt0/<base32>`). `StoreIo` simply joins those
//! relative filenames with the store root to produce absolute paths.

use std::fs;
use std::io;
use std::path::PathBuf;

use blockset::Io;

/// Blockset `Io` implementation backed by a filesystem directory.
///
/// Blob files are stored as `<store_root>/<filename>` where `filename` is
/// the relative path produced by blockset (e.g., `vt0/ABCDEF`).
pub struct StoreIo {
    root: PathBuf,
}

impl StoreIo {
    /// Create a new `StoreIo` rooted at the given directory.
    ///
    /// The directory does not need to exist yet; `write` will create it.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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
        fs::read(self.full_path(filename))
    }

    fn write(&mut self, filename: &str, data: &[u8]) -> io::Result<()> {
        let path = self.full_path(filename);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, data)
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
