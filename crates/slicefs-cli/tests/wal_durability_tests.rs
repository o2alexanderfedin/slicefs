//! Durability promises of the write log (#20), with a write log that counts
//! its syncs and can fail on demand.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{Lsn, WalEntry, WalError, WalStrategy};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::handlers::{self as h, EmptyResult};
use slicefs_traits::metadata::MetadataStore;
use tempfile::TempDir;

#[derive(Clone, Default)]
struct Probe {
    syncs: Arc<AtomicUsize>,
    fail_appends: Arc<AtomicBool>,
}

struct ProbeWal(Probe);

impl WalStrategy for ProbeWal {
    fn append(&self, _e: &WalEntry) -> Result<Lsn, WalError> {
        if self.0.fail_appends.load(Ordering::SeqCst) {
            return Err(WalError::Io(std::io::Error::from_raw_os_error(libc::ENOSPC)));
        }
        Ok(Lsn(0))
    }
    fn write_out(&self, _upto: Lsn) -> Result<(), WalError> {
        Ok(())
    }
    fn flush_and_sync(&self) -> Result<(), WalError> {
        self.0.syncs.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn shutdown(&self) -> Result<(), WalError> {
        Ok(())
    }
}

fn fs_with(store: &Path, probe: &Probe) -> SliceFsFilesystem {
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let mut meta = DictMetadataStore::new(io.clone());
    meta.set_wal(Box::new(ProbeWal(probe.clone())));
    SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()))
}

// fsync of a directory makes the log durable (before: ENOSYS, which the
// kernel takes as success, and nothing was synced).
#[test]
fn fsync_of_a_directory_syncs_the_write_log() {
    let dir = TempDir::new().unwrap();
    let probe = Probe::default();
    let fs = fs_with(dir.path(), &probe);
    let before = probe.syncs.load(Ordering::SeqCst);
    assert!(matches!(h::handle_fsyncdir(&fs), EmptyResult::Ok));
    assert_eq!(probe.syncs.load(Ordering::SeqCst), before + 1, "fsyncdir must sync the write log");
}

// A failed append to the log makes every later fsync fail until a commit,
// so a record that is not in the log is never reported durable.
#[test]
fn a_failed_log_append_fails_fsync_until_the_next_commit() {
    let dir = TempDir::new().unwrap();
    let probe = Probe::default();
    let fs = fs_with(dir.path(), &probe);
    fs.meta().commit().unwrap();
    probe.fail_appends.store(true, Ordering::SeqCst);
    // A logged request (the handlers log; test_* methods do not).
    assert!(matches!(h::handle_mkdir(&fs, 1, "d", 0o755, 0, 0, 0), h::EntryResult::Ok(..)));
    probe.fail_appends.store(false, Ordering::SeqCst);
    assert!(matches!(h::handle_fsyncdir(&fs), EmptyResult::Error(libc::EIO)), "the log lost a record");
    assert!(fs.meta().flush_wal().is_err());
    // A commit puts everything in a root and starts the log again.
    fs.meta().commit().unwrap();
    assert!(fs.meta().flush_wal().is_ok(), "after a commit the log is whole");
}
