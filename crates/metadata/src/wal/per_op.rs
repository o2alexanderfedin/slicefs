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

use chunk_store::lock_stats::Site;
use chunk_store::sync::{Mutex, Rank};
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
        })
    }

    /// Write every queued record into `writer`, in LSN order, and flush it
    /// to the kernel. The caller holds `writer`; the queue lock is held only
    /// to take the records out. Fails after a failed write-out in this
    /// segment (see `failed`).
    fn drain(&self, writer: &mut SegmentWriter) -> Result<(), WalError> {
        let batch = std::mem::take(&mut self.queue.lock().records);
        if let Some(&(last, _)) = batch.back() {
            let r = if self.failed.load(Ordering::Acquire) {
                Err(write_failed())
            } else {
                batch
                    .iter()
                    .try_for_each(|(_, e)| writer.write_entry(e).map(drop))
                    .and_then(|()| writer.flush())
                    .map_err(WalError::from)
            };
            if r.is_err() {
                self.failed.store(true, Ordering::Release);
            }
            self.done.store(last, Ordering::Release);
            return r;
        }
        if self.failed.load(Ordering::Acquire) {
            return Err(write_failed());
        }
        Ok(())
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
        let mut writer = self.writer.lock();
        #[cfg(test)]
        self.park.point("write_out");
        // Under `writer`, `done` is final: a record up to it is in the file.
        self.drain(&mut writer)
    }

    fn rotate(&self, seed: &[WalEntry]) -> Result<(), WalError> {
        let mut writer = self.writer.lock();
        // Records appended before the rotate belong to the old segment (the
        // new root holds them); they must not follow the seed.
        let _ = self.drain(&mut writer);
        let mut loc = self.loc.lock();
        let (new_writer, new_path, new_id) = super::rotate_segment(&loc.0, loc.1, seed)?;
        *writer = new_writer;
        *loc = (new_path, new_id);
        self.failed.store(false, Ordering::Release);
        self.behind.lock().reset();
        Ok(())
    }
    fn cut(&self) -> Result<Option<WalCut>, WalError> {
        let mut writer = self.writer.lock();
        // Every record appended before the cut goes into the old segment.
        // A failure is cut too: the commit is the way back.
        let lost = self.drain(&mut writer).is_err();
        let mut loc = self.loc.lock();
        let mut cut = super::cut_segment(&mut writer, &mut loc, &self.behind)?;
        cut.old_failed |= lost;
        self.failed.store(false, Ordering::Release);
        Ok(Some(cut))
    }
    fn install_root(&self, cut: &WalCut, seed: &[WalEntry]) -> Result<(), WalError> {
        let (file, path) = {
            let mut writer = self.writer.lock();
            self.drain(&mut writer)?;
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
        // Under `writer`: write the queue out and take the file; the disk
        // sync runs with no lock held, so requests append and write out.
        super::sync_current(&self.writer, |w| self.drain(w), || {
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
}
