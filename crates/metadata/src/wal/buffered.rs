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

use std::io;
use std::path::{Path, PathBuf};

use chunk_store::lock_stats::Site;
use chunk_store::sync::{Mutex, Rank};
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

/// Write records into the segment (no flush, no sync).
fn write_all(entries: &[WalEntry], writer: &mut SegmentWriter) -> io::Result<()> {
    for entry in entries {
        writer.write_entry(&wal_entry_to_segment(entry))?;
    }
    Ok(())
}

/// Records are held in RAM and written to the segment by `flush_and_sync`.
pub(crate) struct BufferedLog {
    pending: Mutex<Pending>,
    writer: Mutex<SegmentWriter>,
    /// Path and id of the current segment (for rotation).
    loc: Mutex<(PathBuf, u64)>,
    /// Segments before the last cut that are not synced yet (#108).
    behind: Mutex<Behind>,
}

impl BufferedLog {
    pub(crate) fn new(path: &Path, segment_id: u64) -> io::Result<Self> {
        Ok(BufferedLog {
            pending: Mutex::new(Rank::Store, Pending::default()).timed(Site::WalPending),
            writer: Mutex::new(Rank::Store, SegmentWriter::new(path, segment_id)?).timed(Site::WalWriter),
            loc: Mutex::new(Rank::Store, (path.to_path_buf(), segment_id)),
            behind: Mutex::new(Rank::Store, Behind::default()),
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

    /// When the held records pass `BUFFER_CAP_BYTES`, write them into the
    /// segment file (no disk sync) and empty the RAM; else nothing.
    pub(crate) fn write_out(&self, _upto: Lsn) -> Result<(), WalError> {
        if self.pending.lock().bytes <= BUFFER_CAP_BYTES {
            return Ok(());
        }
        let mut writer = self.writer.lock();
        let batch = {
            let mut pending = self.pending.lock();
            if pending.failed || pending.bytes <= BUFFER_CAP_BYTES {
                return Ok(());
            }
            pending.take()
        };
        if let Err(e) = write_all(&batch, &mut writer).and_then(|()| writer.flush()) {
            self.pending.lock().failed = true;
            return Err(e.into());
        }
        Ok(())
    }

    /// Write the held records to the segment and sync it. Fails after a
    /// failed write or sync, until the next `rotate` or `cut`.
    pub(crate) fn flush_and_sync(&self) -> Result<(), WalError> {
        // The records after a cut count only on top of the ones before it.
        Behind::settle(&self.behind, || {})?;
        // Under `writer`: write the held records out and take the file; the
        // disk sync runs with no lock held.
        let prepare = |writer: &mut SegmentWriter| {
            let batch = {
                let mut pending = self.pending.lock();
                if pending.failed {
                    return Err(write_failed());
                }
                pending.take()
            };
            write_all(&batch, writer).map_err(WalError::from)
        };
        if let Err(e) = super::sync_current(&self.writer, prepare, || {}) {
            self.pending.lock().failed = true;
            return Err(e);
        }
        Ok(())
    }

    pub(crate) fn rotate(&self, seed: &[WalEntry]) -> Result<(), WalError> {
        let mut writer = self.writer.lock();
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

    /// See [`super::WalStrategy::cut`].
    pub(crate) fn cut(&self) -> Result<Option<WalCut>, WalError> {
        let mut writer = self.writer.lock();
        let mut pending = self.pending.lock();
        // A failed segment is cut too: the root of this commit holds every
        // record before the cut, and the new segment starts whole.
        let batch = pending.take();
        let failed = pending.failed || write_all(&batch, &mut writer).is_err();
        let mut loc = self.loc.lock();
        let mut cut = match super::cut_segment(&mut writer, &mut loc, &self.behind) {
            Ok(cut) => cut,
            Err(e) => {
                pending.failed |= failed;
                return Err(e);
            }
        };
        cut.old_failed |= failed;
        pending.failed = false;
        Ok(Some(cut))
    }

    /// See [`super::WalStrategy::install_root`].
    pub(crate) fn install_root(&self, cut: &WalCut, seed: &[WalEntry]) -> Result<(), WalError> {
        let (file, path) = {
            let mut writer = self.writer.lock();
            let batch = {
                let mut pending = self.pending.lock();
                if pending.failed {
                    return Err(write_failed());
                }
                pending.take()
            };
            match write_all(&batch, &mut writer).and_then(|()| writer.file_for_sync()) {
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
