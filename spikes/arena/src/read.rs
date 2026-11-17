//! Read paths: how `get` and compaction reach the bytes of a record.
//!
//! The trait is object-safe so the store holds a `Box<dyn ReadPath>` and a
//! new read path (spike B: `map.rs`) needs only a new arm in `make`. The
//! record bytes are valid only inside the callback, so no borrowed slice
//! leaves the store (spike C's `reuse_gate` can cover the whole copy-out).

use crate::arena::arena_path;
use crate::codec::rec_len;
use crate::{Loc, Options, ReadMode};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub trait ReadPath: Send + Sync {
    /// Call `f` with the unpadded record bytes at `loc` (marker .. CRC).
    /// Returns false (and does not call `f`) if they cannot be read.
    fn read_record(&self, loc: &Loc, f: &mut dyn FnMut(&[u8])) -> bool;

    /// The store made, resized or resumed an arena of `len` bytes (logical
    /// file length). `fixed`: the length does not change while it is used.
    fn arena_ready(&self, _arena: u64, _len: u64, _fixed: bool) {}

    /// Drop every handle on `arena` (before GC unlinks it).
    fn forget(&self, arena: u64);

    fn name(&self) -> &'static str;
}

/// `with_record` of the plan: the bytes are valid only inside `f`.
pub fn with_record<R>(p: &dyn ReadPath, loc: &Loc, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
    let mut f = Some(f);
    let mut out = None;
    p.read_record(loc, &mut |b| out = Some((f.take().unwrap())(b)));
    out
}

/// The read path for `opts`.
pub fn make(dir: &Path, opts: &Options) -> io::Result<Box<dyn ReadPath>> {
    match (opts.read_only, opts.read) {
        (true, _) | (_, ReadMode::Pread) => Ok(Box::new(PreadPath::new(dir))),
        (false, ReadMode::Mmap { .. }) => Err(io::Error::other("mmap read path is spike B (map.rs); not built in spike A")),
    }
}

/// `pread` into a new buffer, one shared `File` per arena (as `chunk-store`).
pub struct PreadPath {
    dir: PathBuf,
    files: Mutex<HashMap<u64, Arc<File>>>,
}

impl PreadPath {
    pub fn new(dir: &Path) -> Self {
        PreadPath { dir: dir.to_owned(), files: Mutex::new(HashMap::new()) }
    }

    pub fn file(&self, arena: u64) -> io::Result<Arc<File>> {
        let mut files = self.files.lock().unwrap();
        if let Some(f) = files.get(&arena) {
            return Ok(Arc::clone(f));
        }
        let f = Arc::new(File::open(arena_path(&self.dir, arena))?);
        files.insert(arena, Arc::clone(&f));
        Ok(f)
    }
}

impl ReadPath for PreadPath {
    fn read_record(&self, loc: &Loc, f: &mut dyn FnMut(&[u8])) -> bool {
        let Ok(file) = self.file(loc.arena) else { return false };
        let mut buf = vec![0u8; rec_len(loc.stored)];
        if file.read_exact_at(&mut buf, loc.off).is_err() {
            return false;
        }
        f(&buf);
        true
    }

    fn forget(&self, arena: u64) {
        self.files.lock().unwrap().remove(&arena);
    }

    fn name(&self) -> &'static str {
        "pread"
    }
}
