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
        self.inner.write_all(&payload)?;
        Ok(())
    }

    /// Flush buffered data and call `sync_all` on the underlying file.
    pub fn sync(&mut self) -> io::Result<()> {
        self.inner.flush()?;
        self.inner.get_mut().sync_all()
    }

    /// Write EOF marker, sync, and close the file.
    pub fn close(mut self) -> io::Result<()> {
        // EofMarker: type=0xFF, payload_len=0
        self.inner.write_all(&[RecordType::EofMarker as u8])?;
        self.inner.write_all(&0u32.to_le_bytes())?;
        self.sync()
    }
}
