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
//! or a commit (#98). `sync` waits for a background sync and then syncs the
//! rest, so its promise does not change. After the last attempt the error
//! goes to the store, which then stops all writes.
//!
//! A pack also has a sync handle (`sync_handle`), which the store calls with
//! no store lock (LOCK-01), and which the background sync uses. The copies
//! are in a state that the pack and its handles share; each copy has a
//! sequence number. A handle sync takes a mark (the newest copy) when it
//! starts, and on success drops the copies up to the mark. On an error it
//! repairs in its own thread (it writes every copy again through the
//! file's writer, then syncs, with the waits of `backoff`), with no lock
//! held across the I/O. A failed sync can mark clean the pages of writes
//! that other threads made meanwhile, so a sync that saw a failure (or an
//! unrepaired one) proves nothing, and a write that saw one writes again.
//!
//! `write_durable` is retried as a whole (it writes the full file again).
//! Directory syncs and punches are not retried.

use crate::pack_io::{Kind, PackFile, PackIo, PackRead, PackSync, PackWrite, RunMap, SyncWait};
use crate::sync::{Mutex, Rank};
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
        Box::new(RetryingPack::new(file, self.backoff.clone()))
    }
}

/// A background sync that runs now (through a sync handle, which repairs
/// a failure in its own thread).
struct Flight {
    thread: JoinHandle<io::Result<()>>,
    done: SyncWait,
}

/// One copied write.
struct Copy {
    /// Its place in the order of copies and cuts (from 1).
    seq: u64,
    offset: u64,
    data: Arc<[u8]>,
}

/// The copy state of one pack. The pack and its sync handles share it
/// (a small leaf lock, never held across I/O).
#[derive(Default)]
struct Copies {
    /// Writes since the last good sync, in order.
    list: VecDeque<Copy>,
    /// Sum of the bytes in `list`.
    kept: usize,
    /// The sequence of the newest copy or cut (0: none yet).
    last: u64,
    /// Every copy and cut up to this sequence is durable.
    synced: u64,
    /// The last `set_len` since the last good sync, and its sequence.
    len: Option<(u64, u64)>,
    /// Failed syncs of this file so far, on any path. A failed sync can
    /// mark every dirty page clean, also of writes that other threads made
    /// meanwhile: a write that saw this change while it ran writes again,
    /// and a sync that saw it change proves nothing.
    fails: u64,
    /// The value of `fails` that the last good repair started at: every
    /// copy up to then was written again and synced. A sync that starts
    /// while `repaired < fails` proves nothing either (the pages of a
    /// failure may still be clean).
    repaired: u64,
    /// A sync failed after all retries. The pages may be clean without the
    /// data, so a later sync could succeed falsely: every later write and
    /// sync fails.
    broken: bool,
}

/// The state at the start of a sync: its mark (the newest copy then), the
/// failures seen, and whether every failure before it was repaired.
#[derive(Clone, Copy)]
struct Start {
    mark: u64,
    fails: u64,
    clean: bool,
}

impl Copies {
    fn push(&mut self, offset: u64, data: &[u8]) {
        self.last += 1;
        self.list.push_back(Copy { seq: self.last, offset, data: data.into() });
        self.kept += data.len();
    }

    fn start(&self) -> io::Result<Start> {
        if self.broken {
            return Err(broken());
        }
        Ok(Start { mark: self.last, fails: self.fails, clean: self.repaired == self.fails })
    }

    /// A sync that began at `st` succeeded: drop the copies it covers, if
    /// it proves anything (see `fails`). False if it does not.
    fn ended_well(&mut self, st: Start) -> bool {
        if !st.clean || self.fails != st.fails {
            return false;
        }
        self.synced_to(st.mark);
        true
    }

    fn synced_to(&mut self, mark: u64) {
        self.synced = self.synced.max(mark);
        while self.list.front().is_some_and(|c| c.seq <= self.synced) {
            let c = self.list.pop_front().unwrap();
            self.kept -= c.data.len();
        }
        if self.len.is_some_and(|(_, q)| q <= self.synced) {
            self.len = None;
        }
    }
}

fn broken() -> io::Error {
    io::Error::other("an earlier sync of this pack failed after retries")
}

/// One step of a repair, done by the caller's file or handle.
enum Redo<'a> {
    Cut(u64),
    Write(&'a [u8], u64),
    Sync,
}

/// After the failed sync `err` (already counted in `fails`, or a sync that
/// proved nothing): write every copy again and sync, with the waits of
/// `backoff`. No lock is held during the writes and syncs: the copies are
/// cloned (`Arc`s) first. After the last attempt the pack is broken.
fn repair(st: &Mutex<Copies>, backoff: &[Duration], mut err: io::Error, step: &mut dyn FnMut(Redo<'_>) -> io::Result<()>) -> io::Result<()> {
    for wait in backoff {
        std::thread::sleep(*wait);
        let (mark, fails, len, copies) = {
            let s = st.lock();
            if s.broken {
                return Err(broken());
            }
            let copies: Vec<(u64, Arc<[u8]>)> = s.list.iter().map(|c| (c.offset, Arc::clone(&c.data))).collect();
            (s.last, s.fails, s.len.map(|l| l.0), copies)
        };
        let r = (|| {
            if let Some(len) = len {
                step(Redo::Cut(len))?;
            }
            for (offset, data) in &copies {
                step(Redo::Write(data, *offset))?;
            }
            step(Redo::Sync)
        })();
        let mut s = st.lock();
        match r {
            Ok(()) if s.fails == fails => {
                s.repaired = s.repaired.max(fails);
                s.synced_to(mark);
                drop(s);
                tracing::warn!("chunk store: a sync failed ({err}); the data was written again and synced");
                return Ok(());
            }
            // Another sync failed meanwhile: write again.
            Ok(()) => {}
            Err(e) => {
                s.fails += 1;
                err = e;
            }
        }
    }
    st.lock().broken = true;
    Err(err)
}

/// Write with `write`, then keep a copy. A sync that failed while the
/// write ran may have marked its pages clean: then write again.
fn write_kept(st: &Mutex<Copies>, data: &[u8], offset: u64, mut write: impl FnMut(&[u8], u64) -> io::Result<()>) -> io::Result<()> {
    loop {
        let fails = {
            let s = st.lock();
            if s.broken {
                return Err(broken());
            }
            s.fails
        };
        write(data, offset)?;
        let mut s = st.lock();
        if s.fails == fails {
            s.push(offset, data);
            return Ok(());
        }
    }
}

/// The sync handle of a [`RetryingPack`]: it syncs from any thread with no
/// lock of the pack held, drops the copies of the writes before its start
/// on success, and repairs a failure itself (LOCK-01: the store calls it
/// with no store lock).
struct RetrySync {
    sync: Arc<dyn PackSync>,
    write: Arc<dyn PackWrite>,
    st: Arc<Mutex<Copies>>,
    backoff: Vec<Duration>,
}

impl RetrySync {
    fn repair(&self, err: io::Error) -> io::Result<()> {
        // A handle cannot cut: the store cuts a pack only at open, and
        // syncs it there at once with the pack's own `sync`, which cuts
        // again in its repair.
        repair(&self.st, &self.backoff, err, &mut |r| match r {
            Redo::Cut(_) => Ok(()),
            Redo::Write(d, o) => self.write.write_at(d, o),
            Redo::Sync => self.sync.sync(),
        })
    }
}

impl PackSync for RetrySync {
    fn sync(&self) -> io::Result<()> {
        let (st, bytes) = {
            let s = self.st.lock();
            (s.start()?, s.kept)
        };
        let t = Instant::now();
        let first = self.sync.sync();
        log(&format!("sync bytes={bytes} ms={:.1} bg=1", t.elapsed().as_secs_f64() * 1000.0));
        let mut s = self.st.lock();
        match first {
            Ok(()) if s.ended_well(st) => Ok(()),
            Ok(()) => {
                drop(s);
                self.repair(io::Error::other("a sync of this pack failed while another one ran"))
            }
            Err(e) => {
                s.fails += 1;
                drop(s);
                self.repair(e)
            }
        }
    }
}

struct RetryingPack {
    file: Box<dyn PackFile>,
    st: Arc<Mutex<Copies>>,
    backoff: Vec<Duration>,
    flight: Option<Flight>,
}

impl RetryingPack {
    fn new(file: Box<dyn PackFile>, backoff: Vec<Duration>) -> Self {
        RetryingPack { file, st: Arc::new(Mutex::new(Rank::Store, Copies::default())), backoff, flight: None }
    }

    fn handle(&self) -> Option<RetrySync> {
        Some(RetrySync {
            sync: self.file.sync_handle()?,
            write: self.file.writer()?,
            st: Arc::clone(&self.st),
            backoff: self.backoff.clone(),
        })
    }

    fn repair_here(&mut self, err: io::Error) -> io::Result<()> {
        let file = &mut self.file;
        repair(&self.st, &self.backoff, err, &mut |r| match r {
            Redo::Cut(len) => file.set_len(len),
            Redo::Write(d, o) => file.write_at(d, o),
            Redo::Sync => file.sync(),
        })
    }

    /// Sync now, in this thread (no background sync runs).
    fn sync_here(&mut self) -> io::Result<()> {
        let (st, bytes) = {
            let s = self.st.lock();
            (s.start()?, s.kept)
        };
        let t = Instant::now();
        let first = self.file.sync();
        log(&format!("sync bytes={bytes} ms={:.1}", t.elapsed().as_secs_f64() * 1000.0));
        let mut s = self.st.lock();
        match first {
            Ok(()) if s.ended_well(st) => Ok(()),
            Ok(()) => {
                drop(s);
                self.repair_here(io::Error::other("a sync of this pack failed while another one ran"))
            }
            Err(e) => {
                s.fails += 1;
                drop(s);
                self.repair_here(e)
            }
        }
    }

    /// Start a background sync of every write so far (or sync here if the
    /// file has no sync handle or no thread can start).
    fn start(&mut self) -> io::Result<()> {
        let Some(h) = self.handle() else { return self.sync_here() };
        let done = SyncWait::default();
        let end = done.clone();
        let spawned = std::thread::Builder::new().name("pack-sync".into()).spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.sync()))
                .unwrap_or_else(|_| Err(io::Error::other("the pack sync panicked")));
            end.done();
            r
        });
        match spawned {
            Ok(thread) => {
                self.flight = Some(Flight { thread, done });
                Ok(())
            }
            Err(_) => self.sync_here(),
        }
    }

    /// Settle the background sync if it ended, or (with `wait`) after it
    /// ends. It dropped the copies it made durable, or repaired a failure,
    /// in its own thread; an error here means the pack is broken.
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
        if r.is_err() {
            self.st.lock().broken = true;
        }
        r
    }

    fn kept(&self) -> usize {
        self.st.lock().kept
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
        let file = &mut self.file;
        write_kept(&self.st, data, offset, |d, o| file.write_at(d, o))?;
        self.settle(false)?;
        if self.flight.is_some() && self.kept() > KEEP_CAP {
            self.settle(true)?;
        }
        if self.flight.is_none() && self.kept() > KEEP_LIMIT {
            self.start()?;
        }
        Ok(())
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        // The copies must not change under a background sync.
        self.settle(true)?;
        self.file.set_len(len)?;
        // Writes before a cut that they do not reach again are cut too.
        let mut s = self.st.lock();
        s.list.retain(|c| c.offset < len);
        for c in &mut s.list {
            let keep = (len - c.offset) as usize;
            if keep < c.data.len() {
                c.data = c.data[..keep].into();
            }
        }
        s.kept = s.list.iter().map(|c| c.data.len()).sum();
        s.last += 1;
        s.len = Some((len, s.last));
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        // A background sync covers the older writes; one more sync covers
        // the rest (it flushes only the pages dirtied since).
        self.st.lock().start()?;
        self.settle(true)?;
        self.sync_here()
    }

    /// A handle that syncs from another thread with no lock of this pack:
    /// it drops the copies of the writes made before its sync starts, and
    /// repairs a failure itself (see `RetrySync`). `None` if the inner file
    /// has no sync handle or no writer (the caller then calls `sync`).
    fn sync_handle(&self) -> Option<Arc<dyn PackSync>> {
        Some(Arc::new(self.handle()?))
    }

    /// The background sync that `write_at` would wait for at `KEEP_CAP`.
    /// The store waits for it without its lock and then writes in one hold
    /// with this check, so the cap holds; the wait in `write_at` stays for
    /// the callers that do not ask (run files, GC copies).
    fn write_wait(&self, len: usize) -> Option<SyncWait> {
        let f = self.flight.as_ref()?;
        (self.kept() + len > KEEP_CAP && !f.done.is_done()).then(|| f.done.clone())
    }

    fn sync_soon(&mut self) -> Option<SyncWait> {
        if self.st.lock().broken || self.settle(false).is_err() {
            return None;
        }
        if self.flight.is_none() && self.kept() > 0 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::{Hold, MemIo};

    const LIMIT: Duration = Duration::from_secs(5);

    fn pack(mem: &MemIo) -> RetryingPack {
        RetryingPack::new(mem.create(1).unwrap(), Vec::new())
    }

    fn piece(i: u8) -> Vec<u8> {
        vec![i; 1000]
    }

    // A write made while the handle sync runs (after its start) keeps its
    // copy; the copies of the writes before it are dropped.
    #[test]
    fn a_handle_sync_drops_the_copies_made_before_it() {
        let mem = MemIo::new();
        let mut p = pack(&mem);
        for i in 0..3u8 {
            p.write_at(&piece(i + 1), i as u64 * 1000).unwrap();
        }
        let h = p.sync_handle().expect("a handle");
        mem.hold_syncs(true);
        let t = std::thread::spawn(move || h.sync());
        assert!(mem.wait_parked(Hold::Syncs, 1, LIMIT), "the handle sync is held");
        p.write_at(&piece(4), 3000).unwrap();
        mem.hold_syncs(false);
        t.join().unwrap().unwrap();
        p.settle(false).unwrap();
        assert_eq!(p.kept(), 1000, "only the copy of piece 4 is kept");
        p.sync().unwrap();
        assert_eq!(p.kept(), 0);
    }

    // A short error of a handle sync is repaired in the handle: the copies
    // are written again and synced, and the data is durable.
    #[test]
    fn a_handle_sync_repairs_a_short_error() {
        let mem = MemIo::new();
        // One retry, with no wait.
        let mut p = RetryingPack::new(mem.create(1).unwrap(), vec![Duration::ZERO]);
        for i in 0..3u8 {
            p.write_at(&piece(i + 1), i as u64 * 1000).unwrap();
        }
        mem.sync_dir().unwrap();
        let h = p.sync_handle().expect("a handle");
        mem.fail_next_syncs(1);
        h.sync().unwrap();
        assert_eq!(p.kept(), 0);
        drop(p);
        mem.crash();
        let saved = mem.read(1, Kind::Pack).unwrap();
        for i in 0..3u8 {
            assert_eq!(&saved[i as usize * 1000..(i as usize + 1) * 1000], &piece(i + 1)[..], "piece {i}");
        }
    }
}
