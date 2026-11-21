//! Lock timing report check (SPIKE-02): a small workload through the request
//! handlers must record hold and wait times for every lock label. Only counts
//! are asserted; times depend on the machine.
//!
//! This is a separate test binary on purpose: the recorder is process-global,
//! so a separate process keeps the harness tests from adding to these counts.
#![cfg(feature = "lock-stats")]

use std::path::Path;
use std::sync::{Arc, Mutex};

use chunk_store::lock_stats;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::handlers::{self as h, CreateResult, DataResult, EmptyResult, OpenResult, ReaddirResult};
use slicefs_traits::metadata::MetadataStore;
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;
const ROOT: u64 = 1;

fn new_fs(store: &Path, config: WalConfig) -> SliceFsFilesystem {
    std::fs::create_dir_all(store.join("segments")).unwrap();
    let wal = create_wal(config, store, 1).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let mut meta = DictMetadataStore::new(io.clone());
    meta.set_wal(wal);
    let fs = SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()));
    fs.meta().commit().unwrap();
    fs
}

fn create(fs: &SliceFsFilesystem, name: &str) -> (u64, u64) {
    match h::handle_create(fs, ROOT, name, S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR) {
        CreateResult::Ok(ino, fh, _) => (ino, fh),
        CreateResult::Error(e) => panic!("create errno {e}"),
    }
}

fn release(fs: &SliceFsFilesystem, ino: u64, fh: u64) {
    assert!(matches!(h::handle_release(fs, ino, fh), EmptyResult::Ok), "release");
}

#[test]
fn lock_stats_report() {
    let data = vec![0x5au8; 64 * 1024];

    // Per-op log: every request is written and synced by the log writer.
    let dir = TempDir::new().unwrap();
    let fs = new_fs(dir.path(), WalConfig::PerOp);
    lock_stats::reset();
    let (ino, fh) = create(&fs, "a");
    assert!(matches!(h::handle_write(&fs, fh, 0, &data), h::WriteResult::Ok(_)), "write");
    assert!(matches!(h::handle_fsync(&fs, ino, fh), EmptyResult::Ok), "fsync");
    release(&fs, ino, fh);
    fs.meta().commit().unwrap();
    // After the release and the commit the read goes through `read_root`.
    let fh = match h::handle_open(&fs, ino, libc::O_RDONLY) {
        OpenResult::Ok(fh, _) => fh,
        OpenResult::Error(e) => panic!("open errno {e}"),
    };
    match h::handle_read(&fs, ino, 0, data.len() as u32) {
        DataResult::Ok(got) => assert_eq!(got, data, "read-back"),
        DataResult::Error(e) => panic!("read errno {e}"),
    }
    release(&fs, ino, fh);
    assert!(matches!(h::handle_readdir(&fs, ROOT, 0), ReaddirResult::Ok(_)), "readdir");

    // Buffered log: requests wait in the `pending` buffer until the fsync.
    let dir2 = TempDir::new().unwrap();
    let fs2 = new_fs(dir2.path(), WalConfig::FlushOnFsync);
    let (ino2, fh2) = create(&fs2, "b");
    assert!(matches!(h::handle_write(&fs2, fh2, 0, &data[..4096]), h::WriteResult::Ok(_)), "write");
    assert!(matches!(h::handle_fsync(&fs2, ino2, fh2), EmptyResult::Ok), "fsync");
    release(&fs2, ino2, fh2);

    let snap = lock_stats::snapshot();
    for s in &snap {
        println!(
            "lock site={} n={} wait_max_us={:.1} hold_max_us={:.1}",
            s.label,
            s.count,
            s.wait_max_ns as f64 / 1000.0,
            s.hold_max_ns as f64 / 1000.0
        );
    }
    let n = |label: &str| snap.iter().find(|s| s.label == label).map_or(0, |s| s.count);
    for label in ["store", "io", "wal", "wal_writer", "wal_pending", "open_files", "readers"] {
        assert!(n(label) > 0, "no samples for lock site {label}");
    }
}
