//! Per-operation WAL strategy: syncs each root and snapshot record before it
//! returns. Operation records stay in the write buffer until the next
//! `flush_and_sync` (fsync, fsyncdir) or commit, as POSIX allows.

use std::io;
use std::path::Path;
use std::sync::Mutex;

use crate::segment::{SegmentEntry, SegmentWriter};
use super::{WalEntry, WalError, WalStrategy};

/// WAL that calls `sync_all` after each root and snapshot record.
///
/// Operation records are made durable by `flush_and_sync` or a commit, not one
/// by one (see `log_mutation`).
pub struct PerOpWal {
    writer: Mutex<SegmentWriter>,
    /// Path and id of the current segment (for rotation).
    loc: Mutex<(std::path::PathBuf, u64)>,
}

impl PerOpWal {
    /// Create a new `PerOpWal` writing to `path` with the given `segment_id`.
    pub fn new(path: &Path, segment_id: u64) -> io::Result<Self> {
        let writer = SegmentWriter::new(path, segment_id)?;
        Ok(PerOpWal {
            writer: Mutex::new(writer),
            loc: Mutex::new((path.to_path_buf(), segment_id)),
        })
    }
}

impl WalStrategy for PerOpWal {
    fn rotate(&self, seed: &[WalEntry]) -> Result<(), WalError> {
        let mut writer = self.writer.lock().unwrap();
        let mut loc = self.loc.lock().unwrap();
        let (new_writer, new_path, new_id) = super::rotate_segment(&loc.0, loc.1, seed)?;
        *writer = new_writer;
        *loc = (new_path, new_id);
        Ok(())
    }
    fn log_mutation(&self, entry: &WalEntry) -> Result<(), WalError> {
        let seg_entry = wal_entry_to_segment(entry);
        let mut writer = self.writer.lock().unwrap();
        writer.write_entry(&seg_entry)?;
        // Operation records are made durable by fsync or commit, not one by one.
        if !matches!(entry, WalEntry::Op { .. }) {
            writer.sync()?;
        } else if super::spike_mode() == super::SpikeMode::A {
            // SPIKE option A: push the record into the kernel now (no fsync),
            // so a kill -9 of the daemon loses nothing.
            writer.flush_to_kernel()?;
        }
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
        WalEntry::Op { bytes } => SegmentEntry::Op { bytes: bytes.clone() },
        WalEntry::Snapshot { version, root, created_at, name } => SegmentEntry::SnapshotRecord {
            version: *version,
            root: *root,
            created_at: *created_at,
            name: name.clone(),
        },
    }
}
