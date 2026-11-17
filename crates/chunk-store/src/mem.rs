//! In-memory [`PackIo`] for tests: no disk, faults on demand, and a crash
//! that keeps only the durable state.
//!
//! Model: each file has its current bytes and its durable bytes. `sync`
//! of a pack and `write_durable` make the bytes durable. A new file is
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
    fail_syncs: bool,
    fail_punch: bool,
    no_punch: bool,
}

impl State {
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

    /// Every sync (pack sync, directory sync, durable write) fails with EIO
    /// and makes nothing durable.
    pub fn set_fail_syncs(&self, on: bool) {
        self.s.lock().unwrap().fail_syncs = on;
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
    fn handle(&self, num: u64) -> io::Result<MemPack> {
        let f = self.s.lock().unwrap().files.get(&(num, Kind::Pack)).cloned().ok_or_else(not_found)?;
        self.open.fetch_add(1, Ordering::SeqCst);
        Ok(MemPack { s: Arc::clone(&self.s), f, open: Arc::clone(&self.open) })
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
        let s = self.s.lock().unwrap();
        if s.fail_reads {
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
        let s = self.s.lock().unwrap();
        if s.fail_syncs {
            return Err(eio());
        }
        let mut f = self.f.lock().unwrap();
        f.durable = f.data.clone();
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
        let s = self.s.lock().unwrap();
        if s.fail_reads {
            return Err(eio());
        }
        s.files.get(&(num, kind)).map(|f| f.lock().unwrap().data.clone()).ok_or_else(not_found)
    }

    fn write_durable(&self, num: u64, kind: Kind, data: &[u8]) -> io::Result<()> {
        let mut s = self.s.lock().unwrap();
        if s.fail_syncs {
            return Err(eio());
        }
        let cur = s.files.get(&(num, kind)).map_or(0, |f| f.lock().unwrap().data.len());
        s.grow(data.len().saturating_sub(cur) as u64)?;
        // A new file under the name (rename of a temp file): old handles keep the old file.
        let f = MemFile { data: data.to_vec(), durable: data.to_vec(), linked: true };
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
        let s = self.s.lock().unwrap();
        if s.fail_syncs {
            return Err(eio());
        }
        for f in s.files.values() {
            f.lock().unwrap().linked = true;
        }
        Ok(())
    }

    fn create(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        let mut s = self.s.lock().unwrap();
        if s.files.contains_key(&(num, Kind::Pack)) {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        s.files.insert((num, Kind::Pack), Arc::new(Mutex::new(MemFile::default())));
        drop(s);
        Ok(Box::new(self.handle(num)?))
    }

    fn open_rw(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        Ok(Box::new(self.handle(num)?))
    }

    fn open_read(&self, num: u64) -> io::Result<Arc<dyn PackRead>> {
        Ok(Arc::new(self.handle(num)?))
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
        }
        Ok(())
    }
}
