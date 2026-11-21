//! Per-operation WAL strategy: syncs each root and snapshot record before it
//! returns. Each operation record is written into the segment file (one
//! `write()` into the kernel, no disk sync) before it returns, so a completed
//! request survives a crash of the daemon. The next `flush_and_sync` (fsync,
//! fsyncdir) or commit syncs it, so that it also survives a power loss, as
//! POSIX allows.

use std::io;
use std::path::Path;
use std::sync::Mutex;

use chunk_store::lock_stats::{self, Site};
use crate::segment::{SegmentEntry, SegmentWriter};
use super::{Behind, WalCut, WalEntry, WalError, WalStrategy};

/// WAL that calls `sync_all` after each root and snapshot record.
///
/// Operation records go into the file at once with no sync; `flush_and_sync`
/// or a commit syncs them (see `log_mutation`).
pub struct PerOpWal {
    writer: Mutex<SegmentWriter>,
    /// Path and id of the current segment (for rotation).
    loc: Mutex<(std::path::PathBuf, u64)>,
    /// Segments before the last cut that are not synced yet (#108).
    behind: Mutex<Behind>,
}

impl PerOpWal {
    /// Create a new `PerOpWal` writing to `path` with the given `segment_id`.
    pub fn new(path: &Path, segment_id: u64) -> io::Result<Self> {
        let writer = SegmentWriter::new(path, segment_id)?;
        Ok(PerOpWal {
            writer: Mutex::new(writer),
            loc: Mutex::new((path.to_path_buf(), segment_id)),
            behind: Mutex::new(Behind::default()),
        })
    }
}

impl WalStrategy for PerOpWal {
    fn rotate(&self, seed: &[WalEntry]) -> Result<(), WalError> {
        let mut writer = lock_stats::lock(&self.writer, Site::WalWriter);
        let mut loc = self.loc.lock().unwrap();
        let (new_writer, new_path, new_id) = super::rotate_segment(&loc.0, loc.1, seed)?;
        *writer = new_writer;
        *loc = (new_path, new_id);
        *self.behind.lock().unwrap() = Behind::default();
        Ok(())
    }
    fn cut(&self) -> Result<Option<WalCut>, WalError> {
        let mut writer = lock_stats::lock(&self.writer, Site::WalWriter);
        let mut loc = self.loc.lock().unwrap();
        super::cut_segment(&mut writer, &mut loc, &self.behind).map(Some)
    }
    fn install_root(&self, cut: &WalCut, seed: &[WalEntry]) -> Result<(), WalError> {
        let (file, path) = {
            let mut writer = lock_stats::lock(&self.writer, Site::WalWriter);
            (writer.file_for_sync()?, self.loc.lock().unwrap().0.clone())
        };
        // The disk sync runs with no lock held: requests log meanwhile.
        file.sync_all()?;
        super::write_root_segment(&path, cut, seed, &self.behind)
    }
    fn log_mutation(&self, entry: &WalEntry) -> Result<(), WalError> {
        let seg_entry = wal_entry_to_segment(entry);
        let mut writer = lock_stats::lock(&self.writer, Site::WalWriter);
        writer.write_entry(&seg_entry)?;
        if matches!(entry, WalEntry::Op { .. }) {
            // Into the kernel now, with no disk sync: a crash of the daemon
            // does not lose a completed request. fsync, fsyncdir or a commit
            // makes it survive a power loss.
            writer.flush()?;
        } else {
            writer.sync()?;
        }
        Ok(())
    }

    fn flush_and_sync(&self) -> Result<(), WalError> {
        // The records after a cut count only on top of the ones before it.
        self.behind.lock().unwrap().settle()?;
        lock_stats::lock(&self.writer, Site::WalWriter).sync()?;
        Ok(())
    }

    fn shutdown(&self) -> Result<(), WalError> {
        // We need to consume the SegmentWriter; extract it from the Mutex.
        // Use a swap with a dummy to take ownership.
        // Since we can't move out of Mutex easily, use close() which consumes self.
        // We'll replace the inner writer with a dummy by using unsafe or restructuring.
        // Simpler: just call sync; we can't call close() without consuming.
        // The close() writes an EOF marker — call flush then sync to simulate.
        let mut writer = lock_stats::lock(&self.writer, Site::WalWriter);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::SegmentReader;

    /// Option A (#98): an operation record is in the segment file right after
    /// `log_mutation`, with no sync. A second reader of the file sees it while
    /// the WAL is still open, as after a kill -9 of the daemon. The number of
    /// disk syncs does not change.
    #[test]
    fn op_record_reaches_the_file_without_a_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.seg");
        let wal = PerOpWal::new(&path, 1).unwrap();
        let syncs = wal.writer.lock().unwrap().disk_syncs();
        wal.log_mutation(&WalEntry::Op { bytes: vec![7u8; 40] }).unwrap();
        let on_disk: Vec<SegmentEntry> = SegmentReader::open(&path)
            .map(|r| r.collect())
            .unwrap_or_default();
        assert!(
            matches!(on_disk.as_slice(), [SegmentEntry::Op { bytes }] if bytes == &vec![7u8; 40]),
            "the Op record must be in the file before any sync, got {} records",
            on_disk.len()
        );
        assert_eq!(wal.writer.lock().unwrap().disk_syncs(), syncs, "no disk sync for an Op record");
    }
}
