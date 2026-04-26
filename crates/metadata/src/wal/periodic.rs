//! Periodic WAL strategy — buffers entries; flush on background timer (or shutdown).
//!
//! NOTE: The background timer is driven by the GC thread in Plan 04. This implementation
//! is identical to `FlushOnFsyncWal` in structure; the periodic timer calls
//! `flush_and_sync` on a schedule. The struct exists to make the config enum concrete.

use std::io;
use std::path::Path;
use std::sync::Mutex;

use super::{WalEntry, WalError, WalStrategy, per_op::wal_entry_to_segment};
use crate::segment::SegmentWriter;

/// WAL that buffers mutations; flushes are triggered externally (timer) or at shutdown.
pub struct PeriodicWal {
    buffer: Mutex<Vec<WalEntry>>,
    writer: Mutex<SegmentWriter>,
}

impl PeriodicWal {
    /// Create a new `PeriodicWal` writing to `path` with the given `segment_id`.
    pub fn new(path: &Path, segment_id: u64) -> io::Result<Self> {
        let writer = SegmentWriter::new(path, segment_id)?;
        Ok(PeriodicWal {
            buffer: Mutex::new(Vec::new()),
            writer: Mutex::new(writer),
        })
    }
}

impl WalStrategy for PeriodicWal {
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
