//! Flush-on-fsync WAL strategy — buffers entries; writes on explicit flush.

use std::io;
use std::path::Path;
use std::sync::Mutex;

use crate::segment::SegmentWriter;
use super::{WalEntry, WalError, WalStrategy, per_op::wal_entry_to_segment};

/// WAL that buffers all mutations in memory until `flush_and_sync` is called.
///
/// Suitable when the caller guarantees periodic explicit flushes (e.g., on fsync).
pub struct FlushOnFsyncWal {
    buffer: Mutex<Vec<WalEntry>>,
    writer: Mutex<SegmentWriter>,
}

impl FlushOnFsyncWal {
    /// Create a new `FlushOnFsyncWal` writing to `path` with the given `segment_id`.
    pub fn new(path: &Path, segment_id: u64) -> io::Result<Self> {
        let writer = SegmentWriter::new(path, segment_id)?;
        Ok(FlushOnFsyncWal {
            buffer: Mutex::new(Vec::new()),
            writer: Mutex::new(writer),
        })
    }

    /// Shutdown without flushing buffered entries (for testing: verify entries stay buffered).
    pub fn shutdown_without_flush(self) {
        // Drop without flushing — buffered entries are lost
        drop(self);
    }
}

impl WalStrategy for FlushOnFsyncWal {
    fn log_mutation(&self, entry: &WalEntry) -> Result<(), WalError> {
        self.buffer.lock().unwrap().push(entry.clone());
        Ok(())
    }

    fn flush_and_sync(&self) -> Result<(), WalError> {
        let mut buffer = self.buffer.lock().unwrap();
        let mut writer = self.writer.lock().unwrap();
        for entry in buffer.drain(..) {
            let seg_entry = wal_entry_to_segment(&entry);
            writer.write_entry(&seg_entry)?;
        }
        writer.sync()?;
        Ok(())
    }

    fn shutdown(&self) -> Result<(), WalError> {
        self.flush_and_sync()
    }
}
