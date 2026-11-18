//! In-memory [`PackIo`] for tests: no disk, faults on demand, and a crash
//! that keeps only the durable state.
//!
//! Model: each file has its current bytes and its durable bytes. `sync`
//! of a pack makes the bytes written since the last sync durable, and
//! `write_durable` makes the whole file durable. A failed sync acts as on
//! Linux: the unsaved bytes are marked clean, so a later sync does not save
//! them unless they are written again (reads still show them until a crash). A new file is
//! durable in the directory only after `sync_dir` (or `write_durable`).
//! `crash()` drops every non-durable byte and every non-durable new file.
//! Removals are durable at once (this is the order the store needs: it
//! syncs copies before it removes originals).

use crate::pack_io::{Kind, PackFile, PackIo, PackRead, PUNCH_BLOCK};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct MemFile {
    data: Vec<u8>,
    durable: Vec<u8>,
    /// Byte ranges written since the last sync (dirty pages).
    dirty: Vec<(usize, usize)>,
    /// The file name is durable in the directory.
    linked: bool,
}

#[derive(Default)]
struct State {
    dir: bool,
    files: HashMap<(u64, Kind), Arc<Mutex<MemFile>>>,
    /// Bytes that writes may still add (sum of all file growth); `None` = no limit.
    space_left: Option<u64>,
    fail_reads: bool,
    /// After this many more reads, every read fails (`None`: off).
    fail_reads_after: Option<u32>,
    fail_syncs: bool,
    /// The next this many syncs fail (then they work again).
    fail_next_syncs: u32,
    fail_punch: bool,
    no_punch: bool,
}

impl State {
    /// True if this read must fail (counts down `fail_reads_after`).
    fn read_fails(&mut self) -> bool {
        match self.fail_reads_after.as_mut() {
            Some(0) => true,
            Some(n) => {
                *n -= 1;
                self.fail_reads
            }
            None => self.fail_reads,
        }
    }

    /// True if this sync must fail (counts down `fail_next_syncs`).
    fn sync_fails(&mut self) -> bool {
        if self.fail_next_syncs > 0 {
            self.fail_next_syncs -= 1;
            return true;
        }
        self.fail_syncs
    }

    fn grow(&mut self, by: u64) -> io::Result<()> {
        if let Some(left) = self.space_left.as_mut() {
            if by > *left {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            *left -= by;
        }
        Ok(())
    }
}

/// The mock. Clones share the same files.
#[derive(Clone, Default)]
pub struct MemIo {
    s: Arc<Mutex<State>>,
    /// Pack handles that are open now (like file descriptors).
    open: Arc<AtomicUsize>,
}

impl MemIo {
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes may add at most `bytes` more; then they fail with ENOSPC and
    /// write nothing. `None` removes the limit.
    pub fn set_space_left(&self, bytes: Option<u64>) {
        self.s.lock().unwrap().space_left = bytes;
    }

    /// Every read fails with EIO.
    pub fn set_fail_reads(&self, on: bool) {
        self.s.lock().unwrap().fail_reads = on;
    }

    /// The next `n` reads work, then every read fails with EIO until
    /// `clear_fail_reads_after` (a failure in the middle of a long operation).
    pub fn fail_reads_after(&self, n: u32) {
        self.s.lock().unwrap().fail_reads_after = Some(n);
    }

    /// End `fail_reads_after`.
    pub fn clear_fail_reads_after(&self) {
        self.s.lock().unwrap().fail_reads_after = None;
    }

    /// Every sync (pack sync, directory sync, durable write) fails with EIO
    /// and makes nothing durable.
    pub fn set_fail_syncs(&self, on: bool) {
        self.s.lock().unwrap().fail_syncs = on;
    }

    /// The next `n` syncs fail with EIO; later syncs work.
    pub fn fail_next_syncs(&self, n: u32) {
        self.s.lock().unwrap().fail_next_syncs = n;
    }

    /// Every punch fails with EIO (and punches nothing).
    pub fn set_fail_punch(&self, on: bool) {
        self.s.lock().unwrap().fail_punch = on;
    }

    /// Punch is not supported (like a file system without holes).
    pub fn set_no_punch(&self, on: bool) {
        self.s.lock().unwrap().no_punch = on;
    }

    /// A power loss: keep only what is durable.
    pub fn crash(&self) {
        let mut s = self.s.lock().unwrap();
        s.files.retain(|_, f| f.lock().unwrap().linked);
        for f in s.files.values() {
            let mut f = f.lock().unwrap();
            f.data = f.durable.clone();
        }
    }

    /// Pack handles that are open now (the file descriptors a real store
    /// would hold).
    pub fn open_handles(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    /// Current length of a file, if it exists.
    pub fn len(&self, num: u64, kind: Kind) -> Option<usize> {
        self.s.lock().unwrap().files.get(&(num, kind)).map(|f| f.lock().unwrap().data.len())
    }

    /// Change one byte of a file (to test damage detection).
    pub fn flip(&self, num: u64, kind: Kind, offset: usize) {
        let s = self.s.lock().unwrap();
        let mut f = s.files.get(&(num, kind)).expect("no such file").lock().unwrap();
        f.data[offset] ^= 0xff;
        if offset < f.durable.len() {
            f.durable[offset] ^= 0xff;
        }
    }

    /// A handle holds the file itself, so (as with unlink on Unix) it stays
    /// usable after the name is removed.
    fn handle(&self, num: u64, kind: Kind) -> io::Result<MemPack> {
        let f = self.s.lock().unwrap().files.get(&(num, kind)).cloned().ok_or_else(not_found)?;
        self.open.fetch_add(1, Ordering::SeqCst);
        Ok(MemPack { s: Arc::clone(&self.s), f, open: Arc::clone(&self.open) })
    }

    /// A new, empty file that must not exist, and a handle to it. Its name
    /// is durable only after `sync_dir`.
    fn create_new(&self, num: u64, kind: Kind) -> io::Result<MemPack> {
        let mut s = self.s.lock().unwrap();
        if s.files.contains_key(&(num, kind)) {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        s.files.insert((num, kind), Arc::new(Mutex::new(MemFile::default())));
        drop(s);
        self.handle(num, kind)
    }
}

fn not_found() -> io::Error {
    io::Error::from(io::ErrorKind::NotFound)
}

fn eio() -> io::Error {
    io::Error::from_raw_os_error(libc::EIO)
}

struct MemPack {
    s: Arc<Mutex<State>>,
    f: Arc<Mutex<MemFile>>,
    open: Arc<AtomicUsize>,
}

impl Drop for MemPack {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::SeqCst);
    }
}

impl PackRead for MemPack {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let mut s = self.s.lock().unwrap();
        if s.read_fails() {
            return Err(eio());
        }
        let f = self.f.lock().unwrap();
        let start = offset as usize;
        let end = start.checked_add(buf.len()).filter(|&e| e <= f.data.len());
        let end = end.ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        buf.copy_from_slice(&f.data[start..end]);
        Ok(())
    }
}

impl PackFile for MemPack {
    fn write_at(&mut self, data: &[u8], offset: u64) -> io::Result<()> {
        let mut s = self.s.lock().unwrap();
        let mut f = self.f.lock().unwrap();
        let end = offset as usize + data.len();
        s.grow(end.saturating_sub(f.data.len()) as u64)?;
        if f.data.len() < end {
            f.data.resize(end, 0);
        }
        f.data[offset as usize..end].copy_from_slice(data);
        f.dirty.push((offset as usize, end));
        Ok(())
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        let mut s = self.s.lock().unwrap();
        let mut f = self.f.lock().unwrap();
        s.grow((len as usize).saturating_sub(f.data.len()) as u64)?;
        f.data.resize(len as usize, 0);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        let mut s = self.s.lock().unwrap();
        let mut f = self.f.lock().unwrap();
        if s.sync_fails() {
            // As on Linux: the dirty pages are marked clean without a save.
            f.dirty.clear();
            return Err(eio());
        }
        let f = &mut *f;
        f.durable.resize(f.data.len(), 0);
        for (a, b) in f.dirty.drain(..) {
            let b = b.min(f.data.len());
            if a < b {
                f.durable[a..b].copy_from_slice(&f.data[a..b]);
            }
        }
        Ok(())
    }
}

impl PackIo for MemIo {
    fn prepare(&self, read_only: bool) -> io::Result<bool> {
        let mut s = self.s.lock().unwrap();
        if !read_only {
            s.dir = true;
        }
        Ok(s.dir)
    }

    fn list(&self) -> io::Result<Vec<u64>> {
        let s = self.s.lock().unwrap();
        Ok(s.files.keys().filter(|(_, k)| *k == Kind::Pack).map(|(n, _)| *n).collect())
    }

    fn exists(&self, num: u64, kind: Kind) -> bool {
        self.s.lock().unwrap().files.contains_key(&(num, kind))
    }

    fn read(&self, num: u64, kind: Kind) -> io::Result<Vec<u8>> {
        let mut s = self.s.lock().unwrap();
        if s.read_fails() {
            return Err(eio());
        }
        s.files.get(&(num, kind)).map(|f| f.lock().unwrap().data.clone()).ok_or_else(not_found)
    }

    fn write_durable(&self, num: u64, kind: Kind, data: &[u8]) -> io::Result<()> {
        let mut s = self.s.lock().unwrap();
        if s.sync_fails() {
            return Err(eio());
        }
        let cur = s.files.get(&(num, kind)).map_or(0, |f| f.lock().unwrap().data.len());
        s.grow(data.len().saturating_sub(cur) as u64)?;
        // A new file under the name (rename of a temp file): old handles keep the old file.
        let f = MemFile { data: data.to_vec(), durable: data.to_vec(), dirty: Vec::new(), linked: true };
        s.files.insert((num, kind), Arc::new(Mutex::new(f)));
        // A durable write syncs the directory too.
        for f in s.files.values() {
            f.lock().unwrap().linked = true;
        }
        Ok(())
    }

    fn remove(&self, num: u64, kind: Kind) -> io::Result<()> {
        self.s.lock().unwrap().files.remove(&(num, kind)).map(|_| ()).ok_or_else(not_found)
    }

    fn sync_dir(&self) -> io::Result<()> {
        let mut s = self.s.lock().unwrap();
        if s.sync_fails() {
            return Err(eio());
        }
        for f in s.files.values() {
            f.lock().unwrap().linked = true;
        }
        Ok(())
    }

    fn create(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        Ok(Box::new(self.create_new(num, Kind::Pack)?))
    }

    fn open_rw(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        Ok(Box::new(self.handle(num, Kind::Pack)?))
    }

    fn open_read(&self, num: u64) -> io::Result<Arc<dyn PackRead>> {
        Ok(Arc::new(self.handle(num, Kind::Pack)?))
    }

    fn can_punch(&self) -> bool {
        !self.s.lock().unwrap().no_punch
    }

    /// Zeros in the current bytes; like a real punch, it is durable only
    /// after a sync of the pack (a crash before that brings the bytes back).
    fn punch(&self, num: u64, ranges: &[(u64, u64)]) -> io::Result<()> {
        let s = self.s.lock().unwrap();
        if s.no_punch {
            return Err(io::Error::from(io::ErrorKind::Unsupported));
        }
        if s.fail_punch {
            return Err(eio());
        }
        let mut f = s.files.get(&(num, Kind::Pack)).ok_or_else(not_found)?.lock().unwrap();
        for &(offset, len) in ranges {
            if offset % PUNCH_BLOCK != 0 || len % PUNCH_BLOCK != 0 {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            let end = ((offset + len) as usize).min(f.data.len());
            let start = (offset as usize).min(end);
            f.data[start..end].fill(0);
            f.dirty.push((start, end));
        }
        Ok(())
    }

    fn list_runs(&self) -> io::Result<Vec<u64>> {
        let s = self.s.lock().unwrap();
        Ok(s.files.keys().filter(|(_, k)| *k == Kind::Run).map(|(n, _)| *n).collect())
    }

    /// Like `create`: the bytes are durable after `sync`, the name after
    /// `sync_dir`.
    fn create_run(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        Ok(Box::new(self.create_new(num, Kind::Run)?))
    }

    fn open_run(&self, num: u64) -> io::Result<Arc<dyn PackRead>> {
        Ok(Arc::new(self.handle(num, Kind::Run)?))
    }
}
