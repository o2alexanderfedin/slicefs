//! A decorator over any [`PackIo`] that recovers from a short sync error.
//!
//! A plain retry of a failed sync is not safe: on Linux the kernel can mark
//! the unsaved pages clean after the error, so the retry succeeds without
//! the data. This decorator keeps a copy of every write to a pack since its
//! last good sync. When a sync fails, it writes the copy again (the pages
//! are dirty again) and then syncs again. A success after that is real.
//!
//! The copy is the data of one commit, at most one pack. After the last
//! attempt the error goes to the store, which then stops all writes.
//!
//! `write_durable` is retried as a whole (it writes the full file again).
//! Directory syncs and punches are not retried.

use crate::pack_io::{Kind, PackFile, PackIo, PackRead};
use std::io;
use std::sync::Arc;
use std::time::Duration;

/// Waits before each new attempt; the number of entries is the number of
/// retries.
const BACKOFF: [Duration; 3] = [Duration::from_millis(10), Duration::from_millis(100), Duration::from_millis(500)];

pub struct RetryingIo {
    inner: Arc<dyn PackIo>,
    backoff: Vec<Duration>,
}

impl RetryingIo {
    pub fn new(inner: Arc<dyn PackIo>) -> Self {
        RetryingIo { inner, backoff: BACKOFF.to_vec() }
    }

    /// With other waits (tests use zero waits).
    pub fn with_backoff(inner: Arc<dyn PackIo>, backoff: Vec<Duration>) -> Self {
        RetryingIo { inner, backoff }
    }

    fn wrap(&self, file: Box<dyn PackFile>) -> Box<dyn PackFile> {
        Box::new(RetryingPack { file, unsynced: Vec::new(), len: None, backoff: self.backoff.clone() })
    }
}

struct RetryingPack {
    file: Box<dyn PackFile>,
    /// Writes since the last good sync, in order.
    unsynced: Vec<(u64, Vec<u8>)>,
    /// The last `set_len` since the last good sync.
    len: Option<u64>,
    backoff: Vec<Duration>,
}

impl RetryingPack {
    fn write_again(&mut self) -> io::Result<()> {
        if let Some(len) = self.len {
            self.file.set_len(len)?;
        }
        for (offset, data) in &self.unsynced {
            self.file.write_at(data, *offset)?;
        }
        Ok(())
    }
}

impl PackRead for RetryingPack {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.file.read_at(buf, offset)
    }
}

impl PackFile for RetryingPack {
    fn write_at(&mut self, data: &[u8], offset: u64) -> io::Result<()> {
        self.file.write_at(data, offset)?;
        self.unsynced.push((offset, data.to_vec()));
        Ok(())
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)?;
        // Writes before a cut that they do not reach again are cut too.
        self.unsynced.retain(|(o, _)| *o < len);
        for (o, d) in &mut self.unsynced {
            d.truncate((len - *o) as usize);
        }
        self.len = Some(len);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        let mut err = match self.file.sync() {
            Ok(()) => {
                self.unsynced.clear();
                self.len = None;
                return Ok(());
            }
            Err(e) => e,
        };
        for wait in self.backoff.clone() {
            std::thread::sleep(wait);
            let r = self.write_again().and_then(|()| self.file.sync());
            match r {
                Ok(()) => {
                    tracing::warn!("chunk store: a sync failed ({err}); the data was written again and synced");
                    self.unsynced.clear();
                    self.len = None;
                    return Ok(());
                }
                Err(e) => err = e,
            }
        }
        Err(err)
    }
}

impl PackIo for RetryingIo {
    fn prepare(&self, read_only: bool) -> io::Result<bool> {
        self.inner.prepare(read_only)
    }
    fn list(&self) -> io::Result<Vec<u64>> {
        self.inner.list()
    }
    fn exists(&self, num: u64, kind: Kind) -> bool {
        self.inner.exists(num, kind)
    }
    fn read(&self, num: u64, kind: Kind) -> io::Result<Vec<u8>> {
        self.inner.read(num, kind)
    }
    fn write_durable(&self, num: u64, kind: Kind, data: &[u8]) -> io::Result<()> {
        let mut r = self.inner.write_durable(num, kind, data);
        for wait in &self.backoff {
            if r.is_ok() {
                break;
            }
            std::thread::sleep(*wait);
            r = self.inner.write_durable(num, kind, data);
        }
        r
    }
    fn remove(&self, num: u64, kind: Kind) -> io::Result<()> {
        self.inner.remove(num, kind)
    }
    fn sync_dir(&self) -> io::Result<()> {
        self.inner.sync_dir()
    }
    fn create(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        Ok(self.wrap(self.inner.create(num)?))
    }
    fn open_rw(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        Ok(self.wrap(self.inner.open_rw(num)?))
    }
    fn open_read(&self, num: u64) -> io::Result<Arc<dyn PackRead>> {
        self.inner.open_read(num)
    }
    fn can_punch(&self) -> bool {
        self.inner.can_punch()
    }
    /// Not retried: the index is already durable without the punched
    /// records, so a failed punch only leaves their bytes on disk; GC
    /// reports the error, and a later GC compacts or deletes the pack.
    fn punch(&self, num: u64, ranges: &[(u64, u64)]) -> io::Result<()> {
        self.inner.punch(num, ranges)
    }
}
