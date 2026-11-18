//! Append-only segment file writer.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{SegmentEntry, SegmentHeader, RecordType};

/// SPIKE (wal-memory): number of write() calls into segment files, process wide.
pub static SEGMENT_WRITE_CALLS: AtomicU64 = AtomicU64::new(0);
/// SPIKE (wal-memory): number of sync_all calls on segment files, process wide.
pub static SEGMENT_SYNC_CALLS: AtomicU64 = AtomicU64::new(0);

/// SPIKE: a File that counts its write() calls.
pub struct CountingFile(pub File);

impl Write for CountingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        SEGMENT_WRITE_CALLS.fetch_add(1, Ordering::Relaxed);
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Writes records to a segment file in append-only fashion.
///
/// Each record is framed as: record_type(u8) + payload_len(u32 LE) +
/// seq(u64 LE) + payload + crc32c(u32 LE) over all the bytes before it.
/// `seq` counts the records of this segment from 0. The file starts with a
/// 16-byte `SegmentHeader`.
pub struct SegmentWriter {
    inner: BufWriter<CountingFile>,
    /// Sequence number of the next record.
    seq: u64,
    /// True when bytes were written since the last successful `sync`.
    unsynced: bool,
    /// Number of `sync_all` calls made (for tests).
    disk_syncs: u64,
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
            inner: BufWriter::new(CountingFile(file)),
            seq: 0,
            unsynced: true,
            disk_syncs: 0,
        };
        let header = SegmentHeader::new(segment_id);
        writer.inner.write_all(&header.to_bytes())?;
        Ok(writer)
    }

    /// Write a single entry (see the frame in the type docs). Returns its
    /// sequence number.
    pub fn write_entry(&mut self, entry: &SegmentEntry) -> io::Result<u64> {
        let payload = entry.payload_bytes();
        let frame = super::frame_record(entry.record_type() as u8, self.seq, &payload);
        self.unsynced = true;
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
        if !self.unsynced {
            return Ok(());
        }
        self.inner.flush()?;
        self.inner.get_mut().0.sync_all()?;
        SEGMENT_SYNC_CALLS.fetch_add(1, Ordering::Relaxed);
        self.disk_syncs += 1;
        self.unsynced = false;
        Ok(())
    }

    /// SPIKE: write the user-space buffer into the kernel (no fsync).
    pub fn flush_to_kernel(&mut self) -> io::Result<()> {
        self.inner.flush()
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
