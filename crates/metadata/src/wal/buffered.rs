//! The buffered write log shared by `FlushOnFsyncWal` and `PeriodicWal`.
//!
//! Records are held in RAM up to `BUFFER_CAP_BYTES` (#98). `flush_and_sync`
//! writes them to the segment and syncs it. When the held records pass the
//! cap, they are written to the segment with no sync. Thus a crash with no
//! fsync can lose up to the last `BUFFER_CAP_BYTES` of records, and the file
//! of a large write can come back as a correct first part of its data.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chunk_store::lock_stats::{self, Site};
use crate::segment::SegmentWriter;
use super::{BUFFER_CAP_BYTES, Behind, WalCut, WalEntry, WalError, per_op::wal_entry_to_segment};

/// Records held in RAM, and their size in bytes.
#[derive(Default)]
struct Pending {
    entries: Vec<WalEntry>,
    bytes: usize,
    /// A write or sync of the segment failed. The records of that write-out
    /// are lost and the segment can end in a torn record. Until the next
    /// `rotate` (commit), `flush_and_sync` fails and new records are not kept:
    /// nothing after the failure can become durable in this segment.
    failed: bool,
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

/// Write the held records into the segment (no flush, no sync) and empty
/// the RAM. On an error the records are dropped; the caller sets `failed`.
fn write_out(pending: &mut Pending, writer: &mut SegmentWriter) -> io::Result<()> {
    pending.bytes = 0;
    for entry in pending.entries.drain(..) {
        writer.write_entry(&wal_entry_to_segment(&entry))?;
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
            pending: Mutex::new(Pending::default()),
            writer: Mutex::new(SegmentWriter::new(path, segment_id)?),
            loc: Mutex::new((path.to_path_buf(), segment_id)),
            behind: Mutex::new(Behind::default()),
        })
    }

    /// Hold the record in RAM. When the held records pass `BUFFER_CAP_BYTES`,
    /// write them into the segment file (no disk sync) and empty the RAM.
    ///
    /// After a failed write-out, the record is not kept (see `Pending::failed`):
    /// every fsync fails until the next commit puts the state into a root.
    pub(crate) fn log(&self, entry: &WalEntry) -> Result<(), WalError> {
        let mut pending = lock_stats::lock(&self.pending, Site::WalPending);
        if pending.failed {
            return Ok(());
        }
        pending.bytes += record_len(entry);
        pending.entries.push(entry.clone());
        if pending.bytes > BUFFER_CAP_BYTES {
            let mut writer = lock_stats::lock(&self.writer, Site::WalWriter);
            let result = write_out(&mut pending, &mut writer).and_then(|()| writer.flush());
            if let Err(e) = result {
                pending.failed = true;
                return Err(e.into());
            }
        }
        Ok(())
    }

    /// Write the held records to the segment and sync it. Fails after a
    /// failed write or sync, until the next `rotate`.
    pub(crate) fn flush_and_sync(&self) -> Result<(), WalError> {
        // The records after a cut count only on top of the ones before it.
        self.behind.lock().unwrap().settle()?;
        // Lock order everywhere: pending, then writer.
        let mut pending = lock_stats::lock(&self.pending, Site::WalPending);
        if pending.failed {
            return Err(write_failed());
        }
        let mut writer = lock_stats::lock(&self.writer, Site::WalWriter);
        if let Err(e) = write_out(&mut pending, &mut writer).and_then(|()| writer.sync()) {
            pending.failed = true;
            return Err(e.into());
        }
        Ok(())
    }

    pub(crate) fn rotate(&self, seed: &[WalEntry]) -> Result<(), WalError> {
        let mut pending = lock_stats::lock(&self.pending, Site::WalPending);
        let mut writer = lock_stats::lock(&self.writer, Site::WalWriter);
        let mut loc = self.loc.lock().unwrap();
        // If the new segment cannot be made, keep the records: they hold the
        // new root's record, and the next flush writes it to the current segment.
        let (new_writer, new_path, new_id) = super::rotate_segment(&loc.0, loc.1, seed)?;
        *writer = new_writer;
        *loc = (new_path, new_id);
        // Records before the cut are dead; the new segment is whole.
        pending.entries.clear();
        pending.bytes = 0;
        pending.failed = false;
        *self.behind.lock().unwrap() = Behind::default();
        Ok(())
    }

    /// See [`super::WalStrategy::cut`].
    pub(crate) fn cut(&self) -> Result<Option<WalCut>, WalError> {
        let mut pending = lock_stats::lock(&self.pending, Site::WalPending);
        let mut writer = lock_stats::lock(&self.writer, Site::WalWriter);
        let mut loc = self.loc.lock().unwrap();
        // A failed segment is cut too: the root of this commit holds every
        // record before the cut, and the new segment starts whole.
        let failed = pending.failed || write_out(&mut pending, &mut writer).is_err();
        let mut cut = super::cut_segment(&mut writer, &mut loc, &self.behind)?;
        cut.old_failed |= failed;
        pending.entries.clear();
        pending.bytes = 0;
        pending.failed = false;
        Ok(Some(cut))
    }

    /// See [`super::WalStrategy::install_root`].
    pub(crate) fn install_root(&self, cut: &WalCut, seed: &[WalEntry]) -> Result<(), WalError> {
        let (file, path) = {
            let mut pending = lock_stats::lock(&self.pending, Site::WalPending);
            if pending.failed {
                return Err(write_failed());
            }
            let mut writer = lock_stats::lock(&self.writer, Site::WalWriter);
            let r = write_out(&mut pending, &mut writer).and_then(|()| writer.file_for_sync());
            match r {
                Ok(f) => (f, self.loc.lock().unwrap().0.clone()),
                Err(e) => {
                    pending.failed = true;
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
        self.pending.lock().unwrap().bytes
    }

    /// Number of disk syncs of the current segment (for tests).
    #[cfg(test)]
    pub(crate) fn disk_syncs(&self) -> u64 {
        self.writer.lock().unwrap().disk_syncs()
    }

    /// Make the next write or sync of the segment fail (test fault hook).
    #[cfg(test)]
    pub(crate) fn fail_next_io(&self) {
        self.writer.lock().unwrap().fail_next_io();
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
