//! A decorator over any [`PackIo`] that recovers from a short sync error.
//!
//! A plain retry of a failed sync is not safe: on Linux the kernel can mark
//! the unsaved pages clean after the error, so the retry succeeds without
//! the data. This decorator keeps a copy of every write to a pack since its
//! last good sync. When a sync fails, it writes the copy again (the pages
//! are dirty again) and then syncs again. A success after that is real.
//!
//! The copy is bounded: when it passes `KEEP_LIMIT` bytes, a sync of the
//! pack starts in a background thread, and the writes go on (#108: this
//! sync took 0.35 to 1.7 s on a slow disk, inside a FUSE write). A copied
//! write is dropped only after a sync that started after it succeeded. If
//! the copy reaches `KEEP_CAP` while that sync runs, the next write waits
//! for it. So the RAM this layer uses does not depend on the size of a file
//! or a commit (#98). A failed background sync is repaired as any other
//! failed sync, at the next write or sync. `sync` waits for a background
//! sync and then syncs the rest, so its promise does not change. After the
//! last attempt the error goes to the store, which then stops all writes.
//!
//! `write_durable` is retried as a whole (it writes the full file again).
//! Directory syncs and punches are not retried.

use crate::pack_io::{Kind, PackFile, PackIo, PackRead, RunMap, SyncWait};
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Unsynced bytes of one pack at which a background sync starts (an early
/// sync is safe: objects that are durable before their root are only
/// garbage until the next GC).
pub const KEEP_LIMIT: usize = 8 << 20;

/// Most bytes of unsynced writes one pack keeps in RAM while a background
/// sync runs; a write that passes it waits for that sync.
pub const KEEP_CAP: usize = 3 * KEEP_LIMIT;

/// When true, each pack sync prints one line to stderr: the wall time (epoch
/// seconds), the unsynced bytes it made durable and its duration, and each
/// wait of a write for a background sync (#108: match slow writes to their
/// cause). The mount turns it on.
pub static LOG_SYNCS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const BACKOFF: [Duration; 3] = [Duration::from_millis(10), Duration::from_millis(100), Duration::from_millis(500)];

pub(crate) fn log(what: &str) {
    if LOG_SYNCS.load(std::sync::atomic::Ordering::Relaxed) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        eprintln!("[t={}.{:03}][pack] {what}", now.as_secs(), now.subsec_millis());
    }
}

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
        Box::new(RetryingPack {
            file,
            unsynced: VecDeque::new(),
            kept: 0,
            len: None,
            backoff: self.backoff.clone(),
            flight: None,
            broken: false,
        })
    }
}

/// A background sync that runs now.
struct Flight {
    /// It covers this many first entries of `unsynced`...
    covers: usize,
    /// ...with this many bytes.
    bytes: usize,
    thread: JoinHandle<io::Result<()>>,
    done: SyncWait,
}

struct RetryingPack {
    file: Box<dyn PackFile>,
    /// Writes since the last good sync, in order.
    unsynced: VecDeque<(u64, Vec<u8>)>,
    /// Sum of the bytes in `unsynced`.
    kept: usize,
    /// The last `set_len` since the last good sync.
    len: Option<u64>,
    backoff: Vec<Duration>,
    flight: Option<Flight>,
    /// A sync failed after all retries. The pages may be clean without the
    /// data, so a later sync could succeed falsely: every later write and
    /// sync fails.
    broken: bool,
}

fn broken() -> io::Error {
    io::Error::other("an earlier sync of this pack failed after retries")
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

    /// Every copied write is durable now.
    fn all_synced(&mut self) {
        self.unsynced.clear();
        self.kept = 0;
        self.len = None;
    }

    /// After the failed sync `err`: write the copy again and sync, with
    /// the waits of `backoff`.
    fn repair(&mut self, mut err: io::Error) -> io::Result<()> {
        for wait in self.backoff.clone() {
            std::thread::sleep(wait);
            let r = self.write_again().and_then(|()| self.file.sync());
            match r {
                Ok(()) => {
                    tracing::warn!("chunk store: a sync failed ({err}); the data was written again and synced");
                    self.all_synced();
                    return Ok(());
                }
                Err(e) => err = e,
            }
        }
        self.broken = true;
        Err(err)
    }

    /// Sync now, in this thread (no background sync runs).
    fn sync_here(&mut self) -> io::Result<()> {
        let t = Instant::now();
        let first = self.file.sync();
        log(&format!("sync bytes={} ms={:.1}", self.kept, t.elapsed().as_secs_f64() * 1000.0));
        match first {
            Ok(()) => {
                self.all_synced();
                Ok(())
            }
            Err(e) => self.repair(e),
        }
    }

    /// Start a background sync of every write so far (or sync here if the
    /// file has no sync handle or no thread can start).
    fn start(&mut self) -> io::Result<()> {
        let Some(h) = self.file.sync_handle() else { return self.sync_here() };
        let bytes = self.kept;
        let done = SyncWait::default();
        let end = done.clone();
        let spawned = std::thread::Builder::new().name("pack-sync".into()).spawn(move || {
            let t = Instant::now();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.sync()))
                .unwrap_or_else(|_| Err(io::Error::other("the pack sync panicked")));
            log(&format!("sync bytes={bytes} ms={:.1} bg=1", t.elapsed().as_secs_f64() * 1000.0));
            end.done();
            r
        });
        match spawned {
            Ok(thread) => {
                self.flight = Some(Flight { covers: self.unsynced.len(), bytes, thread, done });
                Ok(())
            }
            Err(_) => self.sync_here(),
        }
    }

    /// Settle the background sync if it ended, or (with `wait`) after it
    /// ends: drop the writes it made durable, or repair a failure.
    fn settle(&mut self, wait: bool) -> io::Result<()> {
        if !self.flight.as_ref().is_some_and(|f| wait || f.thread.is_finished()) {
            return Ok(());
        }
        let f = self.flight.take().unwrap();
        let t = Instant::now();
        let r = f.thread.join().unwrap_or_else(|_| Err(io::Error::other("the pack sync thread panicked")));
        if wait {
            log(&format!("wait ms={:.1}", t.elapsed().as_secs_f64() * 1000.0));
        }
        match r {
            Ok(()) => {
                self.unsynced.drain(..f.covers);
                self.kept -= f.bytes;
                // `set_len` settles first, so a cut is older than the sync.
                self.len = None;
                Ok(())
            }
            Err(e) => self.repair(e),
        }
    }
}

impl Drop for RetryingPack {
    fn drop(&mut self) {
        // Do not leave a sync running on a file that is gone for the store.
        if let Some(f) = self.flight.take() {
            let _ = f.thread.join();
        }
    }
}

impl PackRead for RetryingPack {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.file.read_at(buf, offset)
    }
}

impl PackFile for RetryingPack {
    fn write_at(&mut self, data: &[u8], offset: u64) -> io::Result<()> {
        if self.broken {
            return Err(broken());
        }
        self.file.write_at(data, offset)?;
        self.unsynced.push_back((offset, data.to_vec()));
        self.kept += data.len();
        self.settle(false)?;
        if self.flight.is_some() && self.kept > KEEP_CAP {
            self.settle(true)?;
        }
        if self.flight.is_none() && self.kept > KEEP_LIMIT {
            self.start()?;
        }
        Ok(())
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        // The entries must not change under a background sync.
        self.settle(true)?;
        self.file.set_len(len)?;
        // Writes before a cut that they do not reach again are cut too.
        self.unsynced.retain(|(o, _)| *o < len);
        for (o, d) in &mut self.unsynced {
            d.truncate((len - *o) as usize);
        }
        self.kept = self.unsynced.iter().map(|(_, d)| d.len()).sum();
        self.len = Some(len);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        // A background sync covers the older writes; one more sync covers
        // the rest (it flushes only the pages dirtied since).
        if self.broken {
            return Err(broken());
        }
        self.settle(true)?;
        self.sync_here()
    }

    fn sync_soon(&mut self) -> Option<SyncWait> {
        if self.broken || self.settle(false).is_err() {
            return None;
        }
        if self.flight.is_none() && self.kept > 0 {
            // An error marks the pack broken; `sync` reports it.
            let _ = self.start();
        }
        self.flight.as_ref().map(|f| f.done.clone())
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
    fn file_len(&self, num: u64, kind: Kind) -> io::Result<u64> {
        self.inner.file_len(num, kind)
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
    fn list_runs(&self) -> io::Result<Vec<u64>> {
        self.inner.list_runs()
    }
    /// Wrapped like a pack: a failed sync of a run is retried with the copy
    /// of its unsynced bytes (at most `KEEP_LIMIT` of them).
    fn create_run(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        Ok(self.wrap(self.inner.create_run(num)?))
    }
    fn open_run(&self, num: u64) -> io::Result<Arc<dyn PackRead>> {
        self.inner.open_run(num)
    }
    fn map_run(&self, num: u64) -> io::Result<Option<RunMap>> {
        self.inner.map_run(num)
    }
}
