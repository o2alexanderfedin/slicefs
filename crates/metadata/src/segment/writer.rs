//! Append-only segment file writer.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use super::{SegmentEntry, SegmentHeader, RecordType};

/// What a [`SegmentWriter::begin_sync`] covered: the segment and the
/// number of records written then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncMark {
    id: u64,
    writes: u64,
}

/// Writes records to a segment file in append-only fashion.
///
/// Each record is framed as: record_type(u8) + payload_len(u32 LE) +
/// seq(u64 LE) + payload + crc32c(u32 LE) over all the bytes before it.
/// `seq` counts the records of this segment from 0. The file starts with a
/// 16-byte `SegmentHeader`.
pub struct SegmentWriter {
    inner: BufWriter<File>,
    /// Sequence number of the next record.
    seq: u64,
    /// True when bytes were written since the last successful `sync`.
    unsynced: bool,
    /// Number of `sync_all` calls made (for tests).
    disk_syncs: u64,
    /// Id of this segment.
    id: u64,
    /// Number of records written (for `end_sync`).
    writes: u64,
    /// Test fault hook: the next `write_entry`, `flush` or `sync` fails.
    #[cfg(test)]
    fail_next: bool,
}

impl SegmentWriter {
    /// Create a new segment file at `path` with the given `segment_id`.
    /// Writes the 16-byte header immediately.
    pub fn new(path: &Path, segment_id: u64) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
        let mut writer = SegmentWriter {
            inner: BufWriter::new(file),
            seq: 0,
            unsynced: true,
            disk_syncs: 0,
            id: segment_id,
            writes: 0,
            #[cfg(test)]
            fail_next: false,
        };
        let header = SegmentHeader::new(segment_id);
        writer.inner.write_all(&header.to_bytes())?;
        Ok(writer)
    }

    /// Write a single entry (see the frame in the type docs). Returns its
    /// sequence number.
    pub fn write_entry(&mut self, entry: &SegmentEntry) -> io::Result<u64> {
        self.injected_fault()?;
        let payload = entry.payload_bytes();
        let frame = super::frame_record(entry.record_type() as u8, self.seq, &payload);
        self.unsynced = true;
        self.writes += 1;
        self.inner.write_all(&frame)?;
        self.seq += 1;
        Ok(self.seq - 1)
    }

    /// Flush buffered data and call `sync_all` on the underlying file.
    ///
    /// Does nothing when no bytes were written since the last successful sync.
    /// `sync_all` is a full disk flush (F_FULLFSYNC on macOS, ~10-20 ms), and
    /// FUSE fsync calls this on every file close over NFS.
    pub fn sync(&mut self) -> io::Result<()> {
        self.injected_fault()?;
        if !self.unsynced {
            return Ok(());
        }
        self.inner.flush()?;
        self.inner.get_mut().sync_all()?;
        self.disk_syncs += 1;
        self.unsynced = false;
        Ok(())
    }

    /// Write the records in the user-space buffer into the file (one
    /// `write()` into the kernel), with no disk sync. After this, a crash of
    /// the process does not lose them; a power loss can. A later `sync` still
    /// syncs them.
    pub fn flush(&mut self) -> io::Result<()> {
        self.injected_fault()?;
        self.inner.flush()
    }

    /// Make the next `write_entry`, `flush` or `sync` fail (test fault hook).
    #[cfg(test)]
    pub(crate) fn fail_next_io(&mut self) {
        self.fail_next = true;
    }

    #[cfg(test)]
    fn injected_fault(&mut self) -> io::Result<()> {
        if std::mem::take(&mut self.fail_next) {
            return Err(io::Error::other("injected write-log fault"));
        }
        Ok(())
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn injected_fault(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Number of records written so far (the sequence number of the next).
    pub fn records(&self) -> u64 {
        self.seq
    }

    /// Write the user-space buffer into the file and return a second handle
    /// of the file, so a caller can sync it without holding this writer
    /// (`sync_all` on the clone syncs the same file).
    pub fn file_for_sync(&mut self) -> io::Result<File> {
        self.injected_fault()?;
        self.inner.flush()?;
        self.inner.get_ref().try_clone()
    }

    /// First half of a sync that runs with no lock held: write the
    /// user-space buffer into the file and return a second handle of it for
    /// `sync_all`, and a mark for [`Self::end_sync`]. `None`: nothing was
    /// written since the last sync.
    pub fn begin_sync(&mut self) -> io::Result<Option<(File, SyncMark)>> {
        self.injected_fault()?;
        if !self.unsynced {
            return Ok(None);
        }
        self.inner.flush()?;
        let file = self.inner.get_ref().try_clone()?;
        Ok(Some((file, SyncMark { id: self.id, writes: self.writes })))
    }

    /// Second half of a sync, after `sync_all` on the handle of `mark`'s
    /// `begin_sync` succeeded: count the disk sync, and mark the segment
    /// synced if nothing was written since. Does nothing if this writer is
    /// another segment (a cut or rotate replaced it meanwhile).
    pub fn end_sync(&mut self, mark: SyncMark) {
        if mark.id != self.id {
            return;
        }
        self.disk_syncs += 1;
        if mark.writes == self.writes {
            self.unsynced = false;
        }
    }

    /// Id of this segment.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Number of real disk syncs so far.
    pub fn disk_syncs(&self) -> u64 {
        self.disk_syncs
    }

    /// Write EOF marker, sync, and close the file.
    pub fn close(mut self) -> io::Result<()> {
        // EofMarker: type=0xFF, payload_len=0
        self.unsynced = true;
        self.inner.write_all(&[RecordType::EofMarker as u8])?;
        self.inner.write_all(&0u32.to_le_bytes())?;
        self.sync()
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;
    use slicefs_traits::key::ObjectKey;

    #[test]
    fn sync_skips_disk_flush_when_nothing_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = SegmentWriter::new(&dir.path().join("s.seg"), 1).unwrap();
        w.sync().unwrap();
        assert_eq!(w.disk_syncs(), 1, "header must be synced once");
        w.sync().unwrap();
        w.sync().unwrap();
        assert_eq!(w.disk_syncs(), 1, "no new bytes: no disk flush");
        let root: ObjectKey = ObjectKey::example(7);
        w.write_entry(&SegmentEntry::RootUpdate { root }).unwrap();
        w.sync().unwrap();
        assert_eq!(w.disk_syncs(), 2);
    }
}
