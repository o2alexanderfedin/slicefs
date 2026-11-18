//! Flush-on-fsync WAL strategy — buffers entries; writes on explicit flush.

use std::io;
use std::path::Path;
use std::sync::Mutex;

use crate::segment::SegmentWriter;
use super::{WalEntry, WalError, WalStrategy};

/// WAL that buffers all mutations in memory until `flush_and_sync` is called.
///
/// Suitable when the caller guarantees periodic explicit flushes (e.g., on fsync).
pub struct FlushOnFsyncWal {
    buffer: Mutex<(Vec<WalEntry>, usize)>,
    writer: Mutex<SegmentWriter>,
    /// Path and id of the current segment (for rotation).
    loc: Mutex<(std::path::PathBuf, u64)>,
}

impl FlushOnFsyncWal {
    /// Create a new `FlushOnFsyncWal` writing to `path` with the given `segment_id`.
    pub fn new(path: &Path, segment_id: u64) -> io::Result<Self> {
        let writer = SegmentWriter::new(path, segment_id)?;
        Ok(FlushOnFsyncWal {
            buffer: Mutex::new((Vec::new(), 0)),
            writer: Mutex::new(writer),
            loc: Mutex::new((path.to_path_buf(), segment_id)),
        })
    }

    /// Shutdown without flushing buffered entries (for testing: verify entries stay buffered).
    pub fn shutdown_without_flush(self) {
        // Drop without flushing — buffered entries are lost
        drop(self);
    }
}

impl WalStrategy for FlushOnFsyncWal {
    fn rotate(&self, seed: &[WalEntry]) -> Result<(), WalError> {
        // Lock order as in flush_and_sync: buffer, then writer.
        let mut buffer = self.buffer.lock().unwrap();
        let mut writer = self.writer.lock().unwrap();
        let mut loc = self.loc.lock().unwrap();
        // If the new segment cannot be made, keep the buffer: it holds the new
        // root's record, and the next flush writes it to the current segment.
        let (new_writer, new_path, new_id) = super::rotate_segment(&loc.0, loc.1, seed)?;
        *writer = new_writer;
        *loc = (new_path, new_id);
        buffer.0.clear(); // records before the cut are dead
        buffer.1 = 0;
        Ok(())
    }
    fn log_mutation(&self, entry: &WalEntry) -> Result<(), WalError> {
        super::buffered_push(&self.buffer, &self.writer, entry)
    }

    fn flush_and_sync(&self) -> Result<(), WalError> {
        super::buffered_flush(&self.buffer, &self.writer)
    }

    fn shutdown(&self) -> Result<(), WalError> {
        self.flush_and_sync()
    }
}
