//! Periodic WAL strategy — buffers entries; flush on background timer (or shutdown).
//!
//! NOTE: The background timer is driven by the GC thread in Plan 04. This implementation
//! is identical to `FlushOnFsyncWal` in structure; the periodic timer calls
//! `flush_and_sync` on a schedule. The struct exists to make the config enum concrete.

use std::io;
use std::path::Path;
use std::sync::Arc;

use super::buffered::BufferedLog;
use super::{WalEntry, WalError, WalStrategy};

/// WAL that buffers mutations; flushes are triggered externally (timer) or at shutdown.
pub struct PeriodicWal {
    log: Arc<BufferedLog>,
}

impl PeriodicWal {
    /// Create a new `PeriodicWal` writing to `path` with the given `segment_id`.
    pub fn new(path: &Path, segment_id: u64) -> io::Result<Self> {
        Ok(PeriodicWal { log: Arc::new(BufferedLog::new(path, segment_id)?) })
    }
}

impl WalStrategy for PeriodicWal {
    fn rotate(&self, seed: &[WalEntry]) -> Result<(), WalError> {
        self.log.rotate(seed)
    }
    fn log_mutation(&self, entry: &WalEntry) -> Result<(), WalError> {
        self.log.log(entry)
    }

    fn flush_and_sync(&self) -> Result<(), WalError> {
        self.log.flush_and_sync()
    }

    fn shutdown(&self) -> Result<(), WalError> {
        self.flush_and_sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_stays_at_most_the_cap_with_no_fsync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.seg");
        let wal = PeriodicWal::new(&path, 1).unwrap();
        super::super::buffered::assert_cap_holds(&wal, &wal.log, &path);
    }
}
