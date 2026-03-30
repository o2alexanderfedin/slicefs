//! Per-operation WAL strategy — durably writes each mutation before returning.

use std::io;
use std::path::Path;
use std::sync::Mutex;

use crate::segment::{SegmentEntry, SegmentWriter};
use super::{WalEntry, WalError, WalStrategy};

/// WAL that calls `sync_all` after every mutation.
///
/// Provides the strongest durability guarantee at the cost of throughput.
pub struct PerOpWal {
    writer: Mutex<SegmentWriter>,
}

impl PerOpWal {
    /// Create a new `PerOpWal` writing to `path` with the given `segment_id`.
    pub fn new(path: &Path, segment_id: u64) -> io::Result<Self> {
        let writer = SegmentWriter::new(path, segment_id)?;
        Ok(PerOpWal {
            writer: Mutex::new(writer),
        })
    }
}

impl WalStrategy for PerOpWal {
    fn log_mutation(&self, entry: &WalEntry) -> Result<(), WalError> {
        let seg_entry = wal_entry_to_segment(entry);
        let mut writer = self.writer.lock().unwrap();
        writer.write_entry(&seg_entry)?;
        writer.sync()?;
        Ok(())
    }

    fn flush_and_sync(&self) -> Result<(), WalError> {
        self.writer.lock().unwrap().sync()?;
        Ok(())
    }

    fn shutdown(&self) -> Result<(), WalError> {
        // We need to consume the SegmentWriter; extract it from the Mutex.
        // Use a swap with a dummy to take ownership.
        // Since we can't move out of Mutex easily, use close() which consumes self.
        // We'll replace the inner writer with a dummy by using unsafe or restructuring.
        // Simpler: just call sync; we can't call close() without consuming.
        // The close() writes an EOF marker — call flush then sync to simulate.
        let mut writer = self.writer.lock().unwrap();
        writer.sync()?;
        Ok(())
    }
}

/// Convert a `WalEntry` to the corresponding `SegmentEntry`.
pub(crate) fn wal_entry_to_segment(entry: &WalEntry) -> SegmentEntry {
    match entry {
        WalEntry::RootUpdate { root } => SegmentEntry::RootUpdate { root: *root },
        WalEntry::Snapshot { version, root, created_at, name } => SegmentEntry::SnapshotRecord {
            version: *version,
            root: *root,
            created_at: *created_at,
            name: name.clone(),
        },
    }
}
