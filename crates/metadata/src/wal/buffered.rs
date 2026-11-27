//! The buffered write log shared by `FlushOnFsyncWal` and `PeriodicWal`.
//!
//! Records are held in RAM up to `BUFFER_CAP_BYTES` (#98). `flush_and_sync`
//! writes them to the segment and syncs it. When the held records pass the
//! cap, `write_out` writes them to the segment with no sync. Thus a crash
//! with no fsync can lose up to the last `BUFFER_CAP_BYTES` of records, and
//! the file of a large write can come back as a correct first part of its
//! data.
//!
//! Lock order everywhere (requests, fsync, the periodic timer, rotate, cut,
//! install_root, shutdown): `writer`, then `pending`, then `loc`, then
//! `behind`. `append` takes only `pending`; the records are taken out of
//! `pending` before they are written, so an append never waits for a write.
//! No WAL lock is held during a record write (G2): the batch is taken and
//! its place reserved under `writer` and `pending`, written with no lock,
//! and published under `writer`, with at most one write in flight.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chunk_store::lock_stats::Site;
use chunk_store::sync::{Mutex, Rank};
use chunk_store::sync::MutexGuard;
use crate::segment::SegmentWriter;
use super::{BUFFER_CAP_BYTES, Behind, Lsn, WalCut, WalEntry, WalError, per_op::wal_entry_to_segment};

/// Records held in RAM, and their size in bytes.
#[derive(Default)]
struct Pending {
    entries: Vec<WalEntry>,
    bytes: usize,
    /// The last LSN given out.
    last: u64,
    /// A write or sync of the segment failed. The records of that write-out
    /// are lost and the segment can end in a torn record. Until the next
    /// `rotate` or `cut` (commit), `flush_and_sync` fails and new records are
    /// not kept: nothing after the failure can become durable in this segment.
    failed: bool,
}

impl Pending {
    /// Take every held record out (the caller holds `writer`).
    fn take(&mut self) -> Vec<WalEntry> {
        self.bytes = 0;
        std::mem::take(&mut self.entries)
    }
}

fn write_failed() -> WalError {
    WalError::Io(io::Error::other(
        "an earlier write-log write or sync failed; nothing is durable until the next commit",
    ))
}

/// Size of a record frame on disk: type, length, sequence, payload, CRC.
fn record_len(entry: &WalEntry) -> usize {
    let payload = match entry {
        WalEntry::Op { bytes } => bytes.len(),
        other => wal_entry_to_segment(other).payload_bytes().len(),
    };
    1 + 4 + 8 + payload + 4
}

/// Records are held in RAM and written to the segment by `flush_and_sync`.
pub(crate) struct BufferedLog {
    pending: Mutex<Pending>,
    writer: Mutex<SegmentWriter>,
    /// Path and id of the current segment (for rotation).
    loc: Mutex<(PathBuf, u64)>,
    /// Segments before the last cut that are not synced yet (#108).
    behind: Mutex<Behind>,
    #[cfg(test)]
    pub(crate) park: super::Parker,
}

impl BufferedLog {
    pub(crate) fn new(path: &Path, segment_id: u64) -> io::Result<Self> {
        Ok(BufferedLog {
            pending: Mutex::new(Rank::Store, Pending::default()).timed(Site::WalPending),
            writer: Mutex::new(Rank::Store, SegmentWriter::new(path, segment_id)?).timed(Site::WalWriter),
            loc: Mutex::new(Rank::Store, (path.to_path_buf(), segment_id)),
            behind: Mutex::new(Rank::Store, Behind::default()),
            #[cfg(test)]
            park: super::Parker::default(),
        })
    }

    /// Hold the record in RAM (memory only). After a failed write-out, the
    /// record is not kept (see `Pending::failed`): every fsync fails until
    /// the next commit puts the state into a root.
    pub(crate) fn append(&self, entry: &WalEntry) -> Result<Lsn, WalError> {
        let (entry, len) = (entry.clone(), record_len(entry));
        let mut pending = self.pending.lock();
        pending.last += 1;
        if !pending.failed {
            pending.bytes += len;
            pending.entries.push(entry);
        }
        Ok(Lsn(pending.last))
    }

    /// `writer`, locked with no record write in flight: a write in flight
    /// is waited for with no lock, until `deadline`.
    fn idle(&self, deadline: Instant) -> Result<MutexGuard<'_, SegmentWriter>, WalError> {
        loop {
            let writer = self.writer.lock();
            match writer.in_flight() {
                None => return Ok(writer),
                Some(wait) => {
                    drop(writer);
                    super::wait_for(&wait, deadline)?;
                }
            }
        }
    }

    /// Write the held records into the segment (no sync) by the one-write
    /// rule: after the write in flight (if any) ended, take the records and
    /// reserve their place under `writer`, write them with no WAL lock, and
    /// publish them under `writer`. With `force` false, only if they pass
    /// `BUFFER_CAP_BYTES`. Fails after a failed write (with `force`), until
    /// the next `rotate` or `cut`; a failed write sets `Pending::failed`.
    fn write_held(&self, force: bool, deadline: Instant) -> Result<(), WalError> {
        let (slot, batch) = {
            let mut writer = self.idle(deadline)?;
            let batch = {
                let mut pending = self.pending.lock();
                if pending.failed {
                    return if force { Err(write_failed()) } else { Ok(()) };
                }
                if !force && pending.bytes <= BUFFER_CAP_BYTES {
                    return Ok(());
                }
                pending.take()
            };
            if batch.is_empty() {
                return Ok(());
            }
            match writer.begin_write(batch.len()) {
                Ok(slot) => (slot, batch),
                Err(e) => {
                    self.pending.lock().failed = true;
                    return Err(e.into());
                }
            }
        };
        #[cfg(test)]
        self.park.point("pwrite");
        let r = slot.write(batch.iter().map(wal_entry_to_segment));
        drop(batch);
        let mut writer = self.writer.lock();
        writer.end_write(slot, &r);
        if r.is_err() {
            self.pending.lock().failed = true;
        }
        drop(writer);
        r.map(drop).map_err(WalError::from)
    }

    /// When the held records pass `BUFFER_CAP_BYTES`, write them into the
    /// segment file (no disk sync) and empty the RAM; else nothing. A write
    /// in flight is waited for first, then the cap is checked again.
    pub(crate) fn write_out(&self, _upto: Lsn) -> Result<(), WalError> {
        if self.pending.lock().bytes <= BUFFER_CAP_BYTES {
            return Ok(());
        }
        self.write_held(false, Instant::now() + super::WRITE_WAIT)
    }

    /// Write the held records to the segment and sync it. Fails after a
    /// failed write or sync, until the next `rotate` or `cut`.
    pub(crate) fn flush_and_sync(&self) -> Result<(), WalError> {
        // The records after a cut count only on top of the ones before it.
        Behind::settle(&self.behind, || {})?;
        // The held records go out first, with no lock across the write;
        // then the file is taken under `writer` and synced with no lock.
        self.write_held(true, Instant::now() + super::WRITE_WAIT)?;
        let check = |_: &mut SegmentWriter| {
            if self.pending.lock().failed { Err(write_failed()) } else { Ok(()) }
        };
        if let Err(e) = super::sync_current(&self.writer, check, || {}) {
            self.pending.lock().failed = true;
            return Err(e);
        }
        Ok(())
    }

    /// Offline only (`snapshot switch`): holds `writer` across the sync of
    /// the new segment. It waits for no record write in flight first.
    pub(crate) fn rotate(&self, seed: &[WalEntry]) -> Result<(), WalError> {
        let mut writer = self.idle(Instant::now() + super::WRITE_WAIT)?;
        // `loc` changes only under `writer`, so this copy stays current.
        let (path, id) = self.loc.lock().clone();
        // If the new segment cannot be made, keep the records: they hold the
        // new root's record, and the next flush writes it to the current segment.
        let (new_writer, new_path, new_id) = super::rotate_segment(&path, id, seed)?;
        *writer = new_writer;
        // Records before the rotate are dead (the new root holds them); the
        // new segment is whole.
        let mut pending = self.pending.lock();
        pending.take();
        pending.failed = false;
        *self.loc.lock() = (new_path, new_id);
        self.behind.lock().reset();
        Ok(())
    }

    /// See [`super::WalStrategy::cut`]. The new segment file is made before
    /// `writer` is taken; under `writer` the cut only swaps the writers and
    /// reserves the Cut record, which is written with no lock (G2).
    pub(crate) fn cut(&self) -> Result<Option<WalCut>, WalError> {
        let at = self.loc.lock().clone();
        #[cfg(test)]
        self.park.point("cut_create");
        let next = super::open_next_segment(&at)?;
        let new_path = next.1 .0.clone();
        let deadline = Instant::now() + super::WRITE_WAIT;
        // Every record held before the cut goes into the old segment. A
        // failed segment never writes them: they are dropped as lost, not
        // waited for (the root of this commit holds their requests).
        let lost = match self.write_held(true, deadline) {
            Ok(()) => false,
            Err(_) if self.pending.lock().failed => true,
            Err(e) => {
                drop(next);
                let _ = std::fs::remove_file(&new_path);
                return Err(e);
            }
        };
        let mut writer = match self.idle(deadline) {
            Ok(w) => w,
            Err(e) => {
                drop(next);
                let _ = std::fs::remove_file(&new_path);
                return Err(e);
            }
        };
        let mut pending = self.pending.lock();
        let failed = lost || pending.failed;
        if failed {
            pending.take();
        }
        let mut loc = self.loc.lock();
        let (mut cut, slot, record) = match super::cut_segment(&mut writer, next, &mut loc, &self.behind) {
            Ok(c) => c,
            Err(e) => {
                pending.failed |= failed;
                return Err(e);
            }
        };
        drop(loc);
        cut.old_failed |= failed;
        pending.failed = false;
        drop(pending);
        drop(writer);
        super::write_cut_record(&self.writer, slot, &record, || self.pending.lock().failed = true);
        Ok(Some(cut))
    }

    /// See [`super::WalStrategy::install_root`].
    pub(crate) fn install_root(&self, cut: &WalCut, seed: &[WalEntry]) -> Result<(), WalError> {
        // The held records go out first (no lock across the write); then
        // the second handle of the file is taken under `writer`.
        self.write_held(true, Instant::now() + super::WRITE_WAIT)?;
        let (file, path) = {
            let mut writer = self.writer.lock();
            if self.pending.lock().failed {
                return Err(write_failed());
            }
            match writer.file_for_sync() {
                Ok(f) => (f, self.loc.lock().0.clone()),
                Err(e) => {
                    self.pending.lock().failed = true;
                    return Err(e.into());
                }
            }
        };
        // The disk sync runs with no lock held: requests log meanwhile.
        file.sync_all()?;
        super::write_root_segment(&path, cut, seed, &self.behind)
    }
    /// Bytes of the records held in RAM (for tests).
    #[cfg(test)]
    pub(crate) fn buffered_bytes(&self) -> usize {
        self.pending.lock().bytes
    }

    /// Number of disk syncs of the current segment (for tests).
    #[cfg(test)]
    pub(crate) fn disk_syncs(&self) -> u64 {
        self.writer.lock().disk_syncs()
    }

    /// Make the next write or sync of the segment fail (test fault hook).
    #[cfg(test)]
    pub(crate) fn fail_next_io(&self) {
        self.writer.lock().fail_next_io();
    }
}

/// Test helper: log more than `BUFFER_CAP_BYTES` of operation records with
/// no fsync. The RAM held stays at most the cap, no disk sync is made, and
/// the records that left RAM are in the segment file, in order (a second
/// reader sees them while the WAL is open, as after a kill -9).
#[cfg(test)]
pub(crate) fn assert_cap_holds(wal: &dyn super::WalStrategy, log: &BufferedLog, path: &Path) {
    use crate::segment::{SegmentEntry, SegmentReader};
    let syncs = log.disk_syncs();
    let record = 64 * 1024;
    let count = (super::BUFFER_CAP_BYTES / record) * 5 / 4; // about 5 MiB
    for i in 0..count {
        wal.log_mutation(&WalEntry::Op { bytes: vec![i as u8; record] }).unwrap();
        assert!(
            log.buffered_bytes() <= super::BUFFER_CAP_BYTES,
            "after record {i}: {} bytes held in RAM, cap {}",
            log.buffered_bytes(),
            super::BUFFER_CAP_BYTES
        );
    }
    assert_eq!(log.disk_syncs(), syncs, "the cap must not sync");
    let on_disk: Vec<SegmentEntry> =
        SegmentReader::open(path).map(|r| r.collect()).unwrap_or_default();
    assert!(!on_disk.is_empty(), "the records over the cap must be in the file");
    for (i, e) in on_disk.iter().enumerate() {
        assert!(
            matches!(e, SegmentEntry::Op { bytes } if bytes.len() == record && bytes[0] == i as u8),
            "record {i} in the file is not the record logged in place {i}"
        );
    }
    let held = log.buffered_bytes();
    assert_eq!(held, (count - on_disk.len()) * record_len(&WalEntry::Op { bytes: vec![0; record] }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::{SegmentEntry, SegmentReader};
    use std::sync::{mpsc, Arc};

    const LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

    fn ops_in(path: &Path) -> Vec<Vec<u8>> {
        SegmentReader::open(path)
            .map(|r| r.filter_map(|e| match e { SegmentEntry::Op { bytes } => Some(bytes), _ => None }).collect())
            .unwrap_or_default()
    }

    /// Take the writer lock and append on another thread; true if both
    /// finished within `LIMIT`.
    fn writer_is_free(log: &Arc<BufferedLog>) -> bool {
        let (tx, rx) = mpsc::channel();
        let t = {
            let log = Arc::clone(log);
            std::thread::spawn(move || {
                drop(log.writer.lock());
                tx.send(log.append(&WalEntry::Op { bytes: vec![0xEE; 8] }).unwrap()).unwrap();
            })
        };
        let ok = rx.recv_timeout(LIMIT).is_ok();
        if ok {
            t.join().unwrap();
        }
        ok
    }

    /// G2: the write of the held records past the cap runs with no WAL lock:
    /// while it is parked, another thread takes the writer lock and appends.
    /// The segment holds every record in LSN order.
    #[test]
    #[cfg_attr(miri, ignore = "too slow under Miri (over 300 s in the probe)")]
    fn buffered_write_out_passes_a_parked_record_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("segment-000001.seg");
        let log = Arc::new(BufferedLog::new(&path, 1).unwrap());
        let record = 64 * 1024;
        let n = BUFFER_CAP_BYTES / record + 1;
        let mut last = Lsn(0);
        for i in 0..n {
            last = log.append(&WalEntry::Op { bytes: vec![i as u8; record] }).unwrap();
        }
        let (parked, release) = log.park.arm("pwrite");
        let a = {
            let log = Arc::clone(&log);
            std::thread::spawn(move || log.write_out(last))
        };
        parked.recv_timeout(LIMIT).expect("thread A must park in its record write");
        let free = writer_is_free(&log);
        let _ = release.send(());
        assert!(free, "the writer lock was held during the record write");
        a.join().unwrap().unwrap();
        log.flush_and_sync().unwrap();
        let mut want: Vec<Vec<u8>> = (0..n).map(|i| vec![i as u8; record]).collect();
        want.push(vec![0xEE; 8]);
        assert_eq!(ops_in(&path), want, "every record, in LSN order");
    }

    /// G2: a cut creates its new segment file before it takes the writer.
    /// The new segment starts with the Cut record of the old one.
    #[test]
    fn buffered_cut_opens_the_new_segment_before_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(BufferedLog::new(&dir.path().join("segment-000001.seg"), 1).unwrap());
        for i in 0..3u8 {
            log.append(&WalEntry::Op { bytes: vec![i; 8] }).unwrap();
        }
        let (parked, release) = log.park.arm("cut_create");
        let a = {
            let log = Arc::clone(&log);
            std::thread::spawn(move || log.cut())
        };
        parked.recv_timeout(LIMIT).expect("thread A must park in the cut");
        let free = writer_is_free(&log);
        let _ = release.send(());
        assert!(free, "the writer lock was held while the cut created its segment");
        a.join().unwrap().unwrap().unwrap();
        log.flush_and_sync().unwrap();
        let got: Vec<SegmentEntry> = SegmentReader::open(&dir.path().join("segment-000003.seg")).unwrap().collect();
        // B's append came before the cut's write-out: the old segment has 4.
        let n_old = ops_in(&dir.path().join("segment-000001.seg")).len() as u64;
        assert_eq!(got.first(), Some(&SegmentEntry::Cut { prev_id: 1, records: n_old }), "the Cut record is first");
        assert_eq!(n_old + got.len() as u64 - 1, 4, "every record is in one of the two segments");
    }
}
