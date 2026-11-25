//! Per-operation WAL strategy: syncs each root and snapshot record before it
//! returns. Each operation record is written into the segment file (one
//! `write()` into the kernel, no disk sync) before it returns, so a completed
//! request survives a crash of the daemon. The next `flush_and_sync` (fsync,
//! fsyncdir) or commit syncs it, so that it also survives a power loss, as
//! POSIX allows.

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use chunk_store::lock_stats::Site;
use chunk_store::sync::{Mutex, MutexGuard, Rank};
use crate::segment::{SegmentEntry, SegmentWriter};
use super::{Behind, Lsn, WalCut, WalEntry, WalError, WalStrategy};

/// Records appended and not yet written out, in LSN order.
#[derive(Default)]
struct Queue {
    /// The last LSN given out (the first record gets 1).
    last: u64,
    records: VecDeque<(u64, SegmentEntry)>,
}

fn write_failed() -> WalError {
    WalError::Io(io::Error::other(
        "an earlier write-log write failed; nothing is durable until the next commit",
    ))
}

/// WAL that calls `sync_all` after each root and snapshot record.
///
/// Operation records go into the file at once with no sync; `flush_and_sync`
/// or a commit syncs them (see `log_mutation`).
///
/// Lock order everywhere: `writer`, then `queue`, then `loc`, then
/// `behind`. `append` takes only `queue`, so it never waits for a write.
/// No WAL lock is held during a record write (G2): at most one is in flight
/// per segment, at its end (see `write_upto`).
pub struct PerOpWal {
    writer: Mutex<SegmentWriter>,
    /// Records appended and not yet written out (`append` takes only this).
    queue: Mutex<Queue>,
    /// The highest LSN a write-out took out of the queue (written, or lost
    /// on an error). Changed only under `writer`.
    done: AtomicU64,
    /// A write-out failed in this segment: records may be lost, so every
    /// write-out and fsync fails until a cut or rotate starts a new segment.
    /// Set only under `writer`, before `done`.
    failed: AtomicBool,
    /// Path and id of the current segment (for rotation).
    loc: Mutex<(std::path::PathBuf, u64)>,
    /// Segments before the last cut that are not synced yet (#108).
    behind: Mutex<Behind>,
    #[cfg(test)]
    park: super::Parker,
    /// Test fault hook: the Cut record write of the next cut fails.
    #[cfg(test)]
    fail_cut_record: AtomicBool,
}

impl PerOpWal {
    /// Create a new `PerOpWal` writing to `path` with the given `segment_id`.
    pub fn new(path: &Path, segment_id: u64) -> io::Result<Self> {
        let writer = SegmentWriter::new(path, segment_id)?;
        Ok(PerOpWal {
            writer: Mutex::new(Rank::Store, writer).timed(Site::WalWriter),
            queue: Mutex::new(Rank::Store, Queue::default()).timed(Site::WalQueue),
            done: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            loc: Mutex::new(Rank::Store, (path.to_path_buf(), segment_id)),
            behind: Mutex::new(Rank::Store, Behind::default()),
            #[cfg(test)]
            park: super::Parker::default(),
            #[cfg(test)]
            fail_cut_record: AtomicBool::new(false),
        })
    }

    /// Write the records up to LSN `upto` out, by the one-write rule (G2):
    /// under `writer`, take the queued batch and reserve its sequence
    /// numbers and the end offset (`begin_write`); write it with no WAL lock
    /// held; publish it under `writer` (`end_write`), then move `done`. A
    /// write-out that needs a record of the write in flight waits for that
    /// write with no lock, until `deadline`. A failed segment writes nothing
    /// more: its queued records are taken out as lost.
    fn write_upto(&self, upto: u64, deadline: Instant) -> Result<(), WalError> {
        loop {
            let (slot, batch) = {
                let mut writer = self.writer.lock();
                #[cfg(test)]
                self.park.point("write_out");
                // Under `writer`, `done` is final: a record up to it is in
                // the file (or lost, if `failed`).
                if self.done.load(Ordering::Acquire) >= upto {
                    return if self.failed.load(Ordering::Acquire) { Err(write_failed()) } else { Ok(()) };
                }
                if let Some(wait) = writer.in_flight() {
                    drop(writer);
                    super::wait_for(&wait, deadline)?;
                    continue;
                }
                let batch = std::mem::take(&mut self.queue.lock().records);
                let Some(&(last, _)) = batch.back() else {
                    // Every record up to `upto` left the queue in a write
                    // that ended: nothing is left to wait for.
                    return if self.failed.load(Ordering::Acquire) { Err(write_failed()) } else { Ok(()) };
                };
                if self.failed.load(Ordering::Acquire) {
                    self.done.store(last, Ordering::Release);
                    return Err(write_failed());
                }
                match writer.begin_write(batch.len()) {
                    Ok(slot) => (slot, batch),
                    Err(e) => {
                        self.failed.store(true, Ordering::Release);
                        self.done.store(last, Ordering::Release);
                        return Err(e.into());
                    }
                }
            };
            #[cfg(test)]
            self.park.point("pwrite");
            let last = batch.back().map_or(0, |&(l, _)| l);
            let r = slot.write(batch.iter().map(|(_, e)| e));
            drop(batch);
            let mut writer = self.writer.lock();
            writer.end_write(slot, &r);
            if r.is_err() {
                self.failed.store(true, Ordering::Release);
            }
            self.done.store(last, Ordering::Release);
            drop(writer);
            r?;
        }
    }

    /// For a cut, a rotate or an install: write out every record appended
    /// before this call, then return `writer` locked with no record write in
    /// flight, all within one `WRITE_WAIT`. Records appended meanwhile can
    /// stay queued: their LSNs are above every record written so far, so
    /// they go to whichever segment is current when they are written out.
    /// The flag is true if the segment failed: then its queued records are
    /// taken out as lost (a failed segment never writes them, so the caller
    /// must not wait for them).
    fn settled(&self) -> Result<(MutexGuard<'_, SegmentWriter>, bool), WalError> {
        let deadline = Instant::now() + super::WRITE_WAIT;
        let target = self.queue.lock().last;
        match self.write_upto(target, deadline) {
            Ok(()) => {}
            Err(_) if self.failed.load(Ordering::Acquire) => {}
            Err(e) => return Err(e),
        }
        loop {
            let writer = self.writer.lock();
            if let Some(wait) = writer.in_flight() {
                drop(writer);
                super::wait_for(&wait, deadline)?;
                continue;
            }
            let lost = self.failed.load(Ordering::Acquire);
            if lost {
                let mut q = self.queue.lock();
                if let Some(&(last, _)) = q.records.back() {
                    self.done.store(last, Ordering::Release);
                }
                q.records.clear();
            }
            return Ok((writer, lost));
        }
    }
}

impl WalStrategy for PerOpWal {
    fn append(&self, entry: &WalEntry) -> Result<Lsn, WalError> {
        // The copy of the record is made before the lock.
        let record = wal_entry_to_segment(entry);
        let mut q = self.queue.lock();
        q.last += 1;
        let lsn = q.last;
        q.records.push_back((lsn, record));
        Ok(Lsn(lsn))
    }

    fn write_out(&self, upto: Lsn) -> Result<(), WalError> {
        if self.done.load(Ordering::Acquire) >= upto.0 && !self.failed.load(Ordering::Acquire) {
            return Ok(());
        }
        self.write_upto(upto.0, Instant::now() + super::WRITE_WAIT)
    }

    /// Offline only (`snapshot switch`): holds `writer` across the sync of
    /// the new segment. It waits for no record write in flight first.
    fn rotate(&self, seed: &[WalEntry]) -> Result<(), WalError> {
        // Records appended before the rotate belong to the old segment (the
        // new root holds them); they must not follow the seed.
        let (mut writer, _) = self.settled()?;
        let mut loc = self.loc.lock();
        let (new_writer, new_path, new_id) = super::rotate_segment(&loc.0, loc.1, seed)?;
        *writer = new_writer;
        *loc = (new_path, new_id);
        self.failed.store(false, Ordering::Release);
        self.behind.lock().reset();
        Ok(())
    }
    fn cut(&self) -> Result<Option<WalCut>, WalError> {
        // The new segment file is made before `writer` is taken (G2).
        let at = self.loc.lock().clone();
        #[cfg(test)]
        self.park.point("cut_create");
        #[allow(unused_mut, reason = "only the test fault hook changes next")]
        let mut next = super::open_next_segment(&at)?;
        #[cfg(test)]
        if self.fail_cut_record.swap(false, Ordering::AcqRel) {
            next.0.fail_next_io();
        }
        let new_path = next.1 .0.clone();
        // Every record appended before the cut goes into the old segment.
        // A failure is cut too: the commit is the way back. A failed
        // segment's queued records are dropped as lost, not waited for.
        let (mut writer, lost) = match self.settled() {
            Ok(s) => s,
            Err(e) => {
                drop(next);
                let _ = std::fs::remove_file(&new_path);
                return Err(e);
            }
        };
        let mut loc = self.loc.lock();
        let (mut cut, slot, record) = super::cut_segment(&mut writer, next, &mut loc, &self.behind)?;
        drop(loc);
        cut.old_failed |= lost;
        self.failed.store(false, Ordering::Release);
        drop(writer);
        super::write_cut_record(&self.writer, slot, &record, || self.failed.store(true, Ordering::Release));
        Ok(Some(cut))
    }
    fn install_root(&self, cut: &WalCut, seed: &[WalEntry]) -> Result<(), WalError> {
        let (file, path) = {
            let (mut writer, lost) = self.settled()?;
            if lost {
                return Err(write_failed());
            }
            (writer.file_for_sync()?, self.loc.lock().0.clone())
        };
        // The disk sync runs with no lock held: requests log meanwhile.
        file.sync_all()?;
        super::write_root_segment(&path, cut, seed, &self.behind)
    }
    fn log_mutation(&self, entry: &WalEntry) -> Result<(), WalError> {
        // Op records: into the kernel at once, with no disk sync, so a crash
        // of the daemon does not lose a completed request. fsync, fsyncdir
        // or a commit makes it survive a power loss. Root and snapshot
        // records are synced.
        let lsn = self.append(entry)?;
        self.write_out(lsn)?;
        if !matches!(entry, WalEntry::Op { .. }) {
            super::sync_current(&self.writer, |_| Ok(()), || {})?;
        }
        Ok(())
    }

    fn flush_and_sync(&self) -> Result<(), WalError> {
        // The records after a cut count only on top of the ones before it.
        Behind::settle(&self.behind, || {
            #[cfg(test)]
            self.park.point("settle");
        })?;
        // Write out every record given out so far (no lock across the
        // record write), then take the file under `writer`; the disk sync
        // runs with no lock held, so requests append and write out.
        let target = self.queue.lock().last;
        self.write_upto(target, Instant::now() + super::WRITE_WAIT)?;
        let check = |_: &mut SegmentWriter| {
            if self.failed.load(Ordering::Acquire) { Err(write_failed()) } else { Ok(()) }
        };
        super::sync_current(&self.writer, check, || {
            #[cfg(test)]
            self.park.point("sync");
        })
    }

    fn shutdown(&self) -> Result<(), WalError> {
        self.flush_and_sync()
    }
}

/// Convert a `WalEntry` to the corresponding `SegmentEntry`.
pub(crate) fn wal_entry_to_segment(entry: &WalEntry) -> SegmentEntry {
    match entry {
        WalEntry::RootUpdate { root } => SegmentEntry::RootUpdate { root: *root },
        WalEntry::Op { bytes } => SegmentEntry::Op { bytes: bytes.clone() },
        WalEntry::Snapshot { version, root, created_at, name } => SegmentEntry::SnapshotRecord {
            version: *version,
            root: *root,
            created_at: *created_at,
            name: name.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::SegmentReader;

    /// Option A (#98): an operation record is in the segment file right after
    /// `log_mutation`, with no sync. A second reader of the file sees it while
    /// the WAL is still open, as after a kill -9 of the daemon. The number of
    /// disk syncs does not change.
    #[test]
    fn op_record_reaches_the_file_without_a_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.seg");
        let wal = PerOpWal::new(&path, 1).unwrap();
        let syncs = wal.writer.lock().disk_syncs();
        wal.log_mutation(&WalEntry::Op { bytes: vec![7u8; 40] }).unwrap();
        let on_disk: Vec<SegmentEntry> = SegmentReader::open(&path)
            .map(|r| r.collect())
            .unwrap_or_default();
        assert!(
            matches!(on_disk.as_slice(), [SegmentEntry::Op { bytes }] if bytes == &vec![7u8; 40]),
            "the Op record must be in the file before any sync, got {} records",
            on_disk.len()
        );
        assert_eq!(wal.writer.lock().disk_syncs(), syncs, "no disk sync for an Op record");
    }

    fn op(tag: u8, i: u32) -> WalEntry {
        let mut bytes = vec![tag];
        bytes.extend_from_slice(&i.to_le_bytes());
        WalEntry::Op { bytes }
    }

    fn ops_in(path: &std::path::Path) -> Vec<Vec<u8>> {
        SegmentReader::open(path)
            .map(|r| r.filter_map(|e| match e { SegmentEntry::Op { bytes } => Some(bytes), _ => None }).collect())
            .unwrap_or_default()
    }

    const LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

    /// LOCK-04: an append is a memory copy; it does not wait for a thread
    /// that holds the segment writer in a write-out.
    #[test]
    fn append_returns_while_the_writer_is_held() {
        use std::sync::{mpsc, Arc};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.seg");
        let wal = Arc::new(PerOpWal::new(&path, 1).unwrap());
        let first = wal.append(&op(1, 0)).unwrap();
        let (parked, release) = wal.park.arm("write_out");
        let a = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || wal.write_out(first))
        };
        parked.recv_timeout(LIMIT).expect("thread A must park in write_out with the writer held");
        let (tx, rx) = mpsc::channel();
        let b = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || tx.send(wal.append(&op(2, 0)).unwrap()).unwrap())
        };
        let second = rx.recv_timeout(LIMIT);
        let _ = release.send(());
        let second = second.expect("append must return while another thread holds the writer");
        assert!(second > first, "LSNs grow: {second:?} after {first:?}");
        a.join().unwrap().unwrap();
        b.join().unwrap();
        wal.write_out(second).unwrap();
        assert_eq!(ops_in(&path), vec![vec![1, 0, 0, 0, 0], vec![2, 0, 0, 0, 0]]);
    }

    /// G2: no WAL lock is held during a record write. While thread A is
    /// parked in its record write, thread B takes the writer lock and
    /// appends; thread C's write-out of a later record waits for A's write
    /// (one write in flight), and the segment holds both records in order.
    #[test]
    fn the_writer_lock_is_free_during_a_record_write() {
        use std::sync::{mpsc, Arc};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.seg");
        let wal = Arc::new(PerOpWal::new(&path, 1).unwrap());
        let l1 = wal.append(&op(1, 0)).unwrap();
        let (parked, release) = wal.park.arm("pwrite");
        let a = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || wal.write_out(l1))
        };
        parked.recv_timeout(LIMIT).expect("thread A must park in its record write");
        let (tx, rx) = mpsc::channel();
        let b = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || {
                drop(wal.writer.lock());
                tx.send(wal.append(&op(2, 0)).unwrap()).unwrap();
            })
        };
        let l2 = rx.recv_timeout(LIMIT);
        if l2.is_err() {
            let _ = release.send(());
        }
        let l2 = l2.expect("the writer lock was held during the record write");
        b.join().unwrap();
        let (ctx, crx) = mpsc::channel();
        let c = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || ctx.send(wal.write_out(l2).is_ok()).unwrap())
        };
        let early = crx.recv_timeout(std::time::Duration::from_millis(300));
        let _ = release.send(());
        assert!(early.is_err(), "C's write-out must wait for the write in flight");
        a.join().unwrap().unwrap();
        assert_eq!(crx.recv_timeout(LIMIT), Ok(true), "C's write-out must succeed after A's write");
        c.join().unwrap();
        assert_eq!(ops_in(&path), vec![vec![1, 0, 0, 0, 0], vec![2, 0, 0, 0, 0]]);
    }

    /// An fsync that starts while a record write is parked returns only
    /// after that write ended, and syncs the segment.
    #[test]
    fn flush_and_sync_waits_for_a_parked_record_write() {
        use std::sync::{mpsc, Arc};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.seg");
        let wal = Arc::new(PerOpWal::new(&path, 1).unwrap());
        wal.flush_and_sync().unwrap();
        let syncs = wal.writer.lock().disk_syncs();
        let l1 = wal.append(&op(1, 0)).unwrap();
        let (parked, release) = wal.park.arm("pwrite");
        let a = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || wal.write_out(l1))
        };
        parked.recv_timeout(LIMIT).expect("thread A must park in its record write");
        let (tx, rx) = mpsc::channel();
        let b = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || tx.send(wal.flush_and_sync().is_ok()).unwrap())
        };
        let early = rx.recv_timeout(std::time::Duration::from_millis(300));
        let _ = release.send(());
        assert!(early.is_err(), "fsync must wait for the record write in flight");
        assert_eq!(rx.recv_timeout(LIMIT), Ok(true));
        a.join().unwrap().unwrap();
        b.join().unwrap();
        assert!(wal.writer.lock().disk_syncs() > syncs, "the fsync must sync the segment");
        assert_eq!(ops_in(&path), vec![vec![1, 0, 0, 0, 0]]);
    }

    /// T-19-14: a failed record write makes every later write-out and fsync
    /// fail until a cut; after the cut both work again.
    #[test]
    fn a_failed_record_write_fails_until_a_cut() {
        let dir = tempfile::tempdir().unwrap();
        let wal = PerOpWal::new(&dir.path().join("segment-000001.seg"), 1).unwrap();
        wal.writer.lock().fail_next_io();
        assert!(wal.log_mutation(&op(1, 0)).is_err(), "the injected fault fails the write-out");
        assert!(wal.log_mutation(&op(1, 1)).is_err(), "a later write-out fails too");
        assert!(wal.flush_and_sync().is_err(), "fsync fails after a failed write");
        let cut = wal.cut().unwrap().unwrap();
        assert!(cut.old_failed, "the cut reports the lost records");
        wal.log_mutation(&op(1, 2)).unwrap();
        wal.flush_and_sync().unwrap();
        assert_eq!(ops_in(&dir.path().join("segment-000003.seg")), vec![vec![1, 2, 0, 0, 0]]);
    }

    /// G2: a cut creates its new segment file before it takes the writer:
    /// while the cut is parked there, another thread takes the writer lock.
    /// The new segment starts with the Cut record (seq 0) of the old one.
    #[test]
    fn cut_opens_the_new_segment_before_the_writer() {
        use std::sync::{mpsc, Arc};
        let dir = tempfile::tempdir().unwrap();
        let wal = Arc::new(PerOpWal::new(&dir.path().join("segment-000001.seg"), 1).unwrap());
        wal.log_mutation(&op(1, 0)).unwrap();
        wal.log_mutation(&op(1, 1)).unwrap();
        let (parked, release) = wal.park.arm("cut_create");
        let a = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || wal.cut())
        };
        parked.recv_timeout(LIMIT).expect("thread A must park in the cut");
        let (tx, rx) = mpsc::channel();
        let b = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || {
                drop(wal.writer.lock());
                tx.send(()).unwrap();
            })
        };
        let took = rx.recv_timeout(LIMIT);
        let _ = release.send(());
        assert!(took.is_ok(), "the writer lock was held while the cut created its segment");
        a.join().unwrap().unwrap().unwrap();
        b.join().unwrap();
        wal.log_mutation(&op(1, 2)).unwrap();
        let got: Vec<SegmentEntry> = SegmentReader::open(&dir.path().join("segment-000003.seg")).unwrap().collect();
        assert_eq!(got.first(), Some(&SegmentEntry::Cut { prev_id: 1, records: 2 }), "the Cut record is first");
        assert_eq!(got.len(), 2, "the record after the cut follows it");
    }

    /// T-19-50: a failed Cut record write leaves the cut in place, but every
    /// write-out and fsync fails until the next cut; then both work again.
    #[test]
    fn a_failed_cut_record_write_fails_until_the_next_cut() {
        let dir = tempfile::tempdir().unwrap();
        let wal = PerOpWal::new(&dir.path().join("segment-000001.seg"), 1).unwrap();
        wal.log_mutation(&op(1, 0)).unwrap();
        wal.fail_cut_record.store(true, Ordering::Release);
        let cut = wal.cut().expect("the cut stands").unwrap();
        assert!(!cut.old_failed);
        assert!(wal.log_mutation(&op(1, 1)).is_err(), "a write-out after a failed Cut record fails");
        assert!(wal.flush_and_sync().is_err(), "an fsync after a failed Cut record fails");
        let cut = wal.cut().unwrap().unwrap();
        assert!(cut.old_failed, "the next cut reports the lost records");
        wal.log_mutation(&op(1, 2)).unwrap();
        wal.flush_and_sync().unwrap();
        let got: Vec<SegmentEntry> = SegmentReader::open(&dir.path().join("segment-000005.seg")).unwrap().collect();
        assert_eq!(got.first(), Some(&SegmentEntry::Cut { prev_id: 3, records: 0 }));
        assert_eq!(ops_in(&dir.path().join("segment-000005.seg")), vec![vec![1, 2, 0, 0, 0]]);
    }

    /// Two threads append and write out in parallel: the segment holds every
    /// record once, in LSN order.
    #[test]
    fn records_reach_the_segment_in_lsn_order() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.seg");
        let wal = Arc::new(PerOpWal::new(&path, 1).unwrap());
        let threads: Vec<_> = (0..2u8)
            .map(|t| {
                let wal = Arc::clone(&wal);
                std::thread::spawn(move || {
                    (0..200u32)
                        .map(|i| {
                            let e = op(t, i);
                            let lsn = wal.append(&e).unwrap();
                            wal.write_out(lsn).unwrap();
                            let WalEntry::Op { bytes } = e else { unreachable!() };
                            (lsn, bytes)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut all: Vec<(Lsn, Vec<u8>)> = threads.into_iter().flat_map(|t| t.join().unwrap()).collect();
        all.sort();
        let want: Vec<Vec<u8>> = all.into_iter().map(|(_, b)| b).collect();
        assert_eq!(ops_in(&path), want, "the segment must hold every record once, in LSN order");
    }

    /// write_out is idempotent: a second call, or a call for an LSN that a
    /// later write-out already wrote, writes nothing and returns Ok.
    #[test]
    fn write_out_twice_writes_each_record_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.seg");
        let wal = PerOpWal::new(&path, 1).unwrap();
        let a = wal.append(&op(1, 1)).unwrap();
        let b = wal.append(&op(1, 2)).unwrap();
        wal.write_out(b).unwrap();
        wal.write_out(b).unwrap();
        wal.write_out(a).unwrap();
        assert_eq!(ops_in(&path).len(), 2, "each record once");
    }

    /// No WAL lock is held across the fsync: while thread A is parked
    /// between taking the file and `sync_all`, thread B appends and writes
    /// out. The sync is counted once, and a later fsync with nothing new
    /// does not sync again.
    #[test]
    fn append_and_write_out_pass_a_parked_fsync() {
        use std::sync::{mpsc, Arc};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.seg");
        let wal = Arc::new(PerOpWal::new(&path, 1).unwrap());
        let first = wal.append(&op(1, 0)).unwrap();
        wal.write_out(first).unwrap();
        let syncs = wal.writer.lock().disk_syncs();
        let (parked, release) = wal.park.arm("sync");
        let a = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || wal.flush_and_sync())
        };
        parked.recv_timeout(LIMIT).expect("thread A must park in the fsync");
        let (tx, rx) = mpsc::channel();
        let b = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || {
                let lsn = wal.append(&op(2, 0)).unwrap();
                tx.send(wal.write_out(lsn).is_ok()).unwrap();
            })
        };
        let wrote = rx.recv_timeout(LIMIT);
        let _ = release.send(());
        assert_eq!(wrote, Ok(true), "append and write_out must finish while an fsync is parked");
        a.join().unwrap().unwrap();
        b.join().unwrap();
        assert_eq!(wal.writer.lock().disk_syncs(), syncs + 1, "the parked fsync counts one sync");
        wal.flush_and_sync().unwrap();
        assert_eq!(wal.writer.lock().disk_syncs(), syncs + 2, "B's record came after A took the file");
        wal.flush_and_sync().unwrap();
        assert_eq!(wal.writer.lock().disk_syncs(), syncs + 2, "nothing new: no second sync");
        assert_eq!(ops_in(&path).len(), 2);
    }

    /// `Behind::settle` syncs the segments left by a cut with the `behind`
    /// lock released: a cut (which takes `behind`) finishes meanwhile, and
    /// the segment it leaves behind is synced by the next fsync.
    #[test]
    fn settle_syncs_the_old_segments_with_the_behind_lock_released() {
        use std::sync::{mpsc, Arc};
        let dir = tempfile::tempdir().unwrap();
        let wal = Arc::new(PerOpWal::new(&dir.path().join("segment-000001.seg"), 1).unwrap());
        wal.log_mutation(&op(1, 0)).unwrap();
        wal.cut().unwrap().unwrap();
        let (parked, release) = wal.park.arm("settle");
        let a = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || wal.flush_and_sync())
        };
        parked.recv_timeout(LIMIT).expect("thread A must park in settle");
        let (tx, rx) = mpsc::channel();
        let c = {
            let wal = Arc::clone(&wal);
            std::thread::spawn(move || tx.send(wal.cut().is_ok()).unwrap())
        };
        let cut = rx.recv_timeout(LIMIT);
        let _ = release.send(());
        assert_eq!(cut, Ok(true), "a cut must finish while settle syncs");
        a.join().unwrap().unwrap();
        c.join().unwrap();
        assert_eq!(wal.behind.lock().old.len(), 1, "the segment of the second cut is still to sync");
        wal.flush_and_sync().unwrap();
        assert_eq!(wal.behind.lock().old.len(), 0);
    }

    /// The store logs one record per `log_op`; a failed write-out makes
    /// `flush_wal` (fsync) fail until the next commit.
    #[test]
    fn store_log_op_writes_one_record_and_a_failed_write_out_fails_fsync() {
        use crate::store::DictMetadataStore;
        use crate::store_io::StoreIo;
        use slicefs_traits::metadata::MetadataStore;
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let path = dir.path().join("segments").join("segment-000001.seg");
        let wal = PerOpWal::new(&path, 1).unwrap();
        let mut meta = DictMetadataStore::new(Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap())));
        wal.writer.lock().fail_next_io();
        meta.set_wal(Box::new(wal));
        meta.log_op(b"lost".to_vec());
        assert!(meta.flush_wal().is_err(), "fsync after a failed write-out must fail");
        meta.commit().expect("the commit after a failed write-out");
        assert!(meta.flush_wal().is_ok(), "after the commit, fsync works again");
        for r in [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()] {
            meta.log_op(r);
        }
        meta.flush_wal().unwrap();
        let (_, _, ops) = crate::segment::load_store_with_log(&dir.path().join("segments")).unwrap();
        assert_eq!(ops, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], "one record per log_op");
    }

    /// T-19-50: the cut of a failed log does not wait for its queue. A
    /// record queued after a failed write-out is dropped as lost; the commit
    /// succeeds, fsync works again, and the records after it replay.
    #[test]
    fn a_commit_after_a_failed_write_with_queued_records_succeeds() {
        use crate::store::DictMetadataStore;
        use crate::store_io::StoreIo;
        use slicefs_traits::metadata::MetadataStore;
        use std::sync::{mpsc, Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let path = dir.path().join("segments").join("segment-000001.seg");
        let wal = PerOpWal::new(&path, 1).unwrap();
        let mut meta = DictMetadataStore::new(Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap())));
        wal.writer.lock().fail_next_io();
        meta.set_wal(Box::new(wal));
        meta.log_op(b"lost".to_vec());
        assert!(meta.log_append(b"queued".to_vec()).is_some(), "the record stays queued");
        let meta = Arc::new(meta);
        let (tx, rx) = mpsc::channel();
        let c = {
            let meta = Arc::clone(&meta);
            std::thread::spawn(move || tx.send(meta.commit().is_ok()).unwrap())
        };
        assert_eq!(rx.recv_timeout(LIMIT), Ok(true), "the commit must succeed and not wait for the queue");
        c.join().unwrap();
        assert!(meta.flush_wal().is_ok(), "after the commit, fsync works again");
        for r in [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()] {
            meta.log_op(r);
        }
        meta.flush_wal().unwrap();
        let (_, _, ops) = crate::segment::load_store_with_log(&dir.path().join("segments")).unwrap();
        assert_eq!(ops, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    }
}
