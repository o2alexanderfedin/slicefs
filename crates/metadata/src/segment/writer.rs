//! Append-only segment file writer.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use super::{SegmentEntry, SegmentHeader, RecordType};

/// Writes records to a segment file in append-only fashion.
///
/// Each record is framed as: record_type(u8) + payload_len(u32 LE) + payload bytes.
/// The file starts with a 16-byte `SegmentHeader`.
pub struct SegmentWriter {
    inner: BufWriter<File>,
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
            inner: BufWriter::new(file),
            unsynced: true,
            disk_syncs: 0,
        };
        let header = SegmentHeader::new(segment_id);
        writer.inner.write_all(&header.to_bytes())?;
        Ok(writer)
    }

    /// Write a single entry. Frames as type(u8) + payload_len(u32 LE) + payload.
    pub fn write_entry(&mut self, entry: &SegmentEntry) -> io::Result<()> {
        let payload = entry.payload_bytes();
        let record_type = entry.record_type() as u8;
        let payload_len = payload.len() as u32;

        self.inner.write_all(&[record_type])?;
        self.inner.write_all(&payload_len.to_le_bytes())?;
        self.unsynced = true;
        self.inner.write_all(&payload)?;
        Ok(())
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
        self.inner.get_mut().sync_all()?;
        self.disk_syncs += 1;
        self.unsynced = false;
        Ok(())
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
    use slicefs_traits::digest::Digest224;

    #[test]
    fn sync_skips_disk_flush_when_nothing_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = SegmentWriter::new(&dir.path().join("s.seg"), 1).unwrap();
        w.sync().unwrap();
        assert_eq!(w.disk_syncs(), 1, "header must be synced once");
        w.sync().unwrap();
        w.sync().unwrap();
        assert_eq!(w.disk_syncs(), 1, "no new bytes: no disk flush");
        let root: Digest224 = [7; 7];
        w.write_entry(&SegmentEntry::RootUpdate { root }).unwrap();
        w.sync().unwrap();
        assert_eq!(w.disk_syncs(), 2);
    }
}
