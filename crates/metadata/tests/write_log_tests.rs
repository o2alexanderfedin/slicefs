//! Write log records (#20): operation records after the last root are
//! returned in order; a commit (RootUpdate) is the cut.

use std::sync::{Arc, Mutex};

use metadata::segment::{load_store_with_log, scan_store_with_log};
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_traits::metadata::MetadataStore;
use tempfile::TempDir;

fn store(dir: &TempDir, config: WalConfig) -> DictMetadataStore {
    std::fs::create_dir_all(dir.path().join("segments")).unwrap();
    let wal = create_wal(config, dir.path(), 1).unwrap();
    let mut meta = DictMetadataStore::new(Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap())));
    meta.set_wal(wal);
    meta
}

#[test]
fn records_after_the_last_root_come_back_in_order() {
    for config in [WalConfig::PerOp, WalConfig::FlushOnFsync] {
        let dir = TempDir::new().unwrap();
        let meta = store(&dir, config);
        meta.log_op(b"before".to_vec());
        let root = meta.commit().unwrap();
        meta.log_op(b"one".to_vec());
        meta.log_op(b"two".to_vec());
        meta.flush_wal().unwrap();
        // A crash now: the records were synced by flush_wal (fsync).
        let (last, _, ops) = load_store_with_log(&dir.path().join("segments")).unwrap();
        assert_eq!(last, Some(root));
        assert_eq!(ops, vec![b"one".to_vec(), b"two".to_vec()]);
    }
}

#[test]
fn a_commit_cuts_the_log() {
    let dir = TempDir::new().unwrap();
    let meta = store(&dir, WalConfig::PerOp);
    meta.log_op(b"x".to_vec());
    meta.commit().unwrap();
    let (_, _, ops) = load_store_with_log(&dir.path().join("segments")).unwrap();
    assert!(ops.is_empty());
}

fn segment_files(dir: &TempDir) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir.path().join("segments")).unwrap()
        .filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
    v.sort();
    v
}

#[test]
fn each_commit_rotates_the_log_to_one_segment_and_keeps_snapshots() {
    for config in [WalConfig::PerOp, WalConfig::FlushOnFsync] {
        let dir = TempDir::new().unwrap();
        let meta = store(&dir, config);
        meta.log_op(vec![1; 1000]);
        meta.commit().unwrap();
        let snap = meta.create_snapshot(Some("s".into())).unwrap();
        for _ in 0..3 {
            meta.log_op(vec![2; 1000]);
            meta.commit().unwrap();
        }
        let root = meta.commit().unwrap();
        meta.log_op(b"after".to_vec());
        meta.flush_wal().unwrap();
        assert_eq!(segment_files(&dir).len(), 1, "{:?}", segment_files(&dir));
        let (last, snaps, ops) = load_store_with_log(&dir.path().join("segments")).unwrap();
        assert_eq!(last, Some(root));
        assert_eq!(ops, vec![b"after".to_vec()]);
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].root, snap.root);
        assert_eq!(snaps[0].name.as_deref(), Some("s"));
    }
}

#[test]
fn a_commit_makes_its_objects_durable_before_the_root() {
    use slicefs_traits::metadata::InodeMeta;
    let dir = TempDir::new().unwrap();
    let meta = store(&dir, WalConfig::PerOp);
    let io = meta.io().clone();
    meta.commit().unwrap();
    let before = io.lock().unwrap().sync_count();
    let f = InodeMeta::new_file(0, 0, 0, 0o100_644);
    let ino = meta.create_inode(&f).unwrap();
    meta.link(1, "f", ino).unwrap();
    meta.commit().unwrap();
    let io = io.lock().unwrap();
    assert_eq!(io.unsynced_count(), 0, "every object written by the commit is synced");
    assert!(io.sync_count() > before, "the commit synced its objects");
}

// #106: the mount reads the log in two passes. Pass 2 returns exactly the
// records that pass 1 found: not the records added after pass 1, and an
// error (not a shorter log) if a record that pass 1 saw is gone.

fn only_segment(dir: &TempDir) -> std::path::PathBuf {
    let names = segment_files(dir);
    assert_eq!(names.len(), 1, "{names:?}");
    dir.path().join("segments").join(&names[0])
}

fn cut_tail(path: &std::path::Path, n: u64) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_len(f.metadata().unwrap().len() - n).unwrap();
}

#[test]
fn records_added_after_the_scan_are_not_read() {
    let dir = TempDir::new().unwrap();
    let meta = store(&dir, WalConfig::PerOp);
    let root = meta.commit().unwrap();
    meta.log_op(b"one".to_vec());
    meta.log_op(b"two".to_vec());
    meta.flush_wal().unwrap();
    let (last, _, log) = scan_store_with_log(&dir.path().join("segments")).unwrap();
    assert_eq!(last, Some(root));
    assert_eq!(log.len(), 2);
    meta.log_op(b"late".to_vec());
    meta.flush_wal().unwrap();
    let ops: Vec<Vec<u8>> = log.records().collect::<Result<_, _>>().unwrap();
    assert_eq!(ops, vec![b"one".to_vec(), b"two".to_vec()]);
}

#[test]
fn a_record_cut_after_the_scan_is_an_error() {
    let dir = TempDir::new().unwrap();
    let meta = store(&dir, WalConfig::PerOp);
    meta.commit().unwrap();
    meta.log_op(b"one".to_vec());
    meta.log_op(b"two".to_vec());
    meta.flush_wal().unwrap();
    let (_, _, log) = scan_store_with_log(&dir.path().join("segments")).unwrap();
    cut_tail(&only_segment(&dir), 3);
    let got: Vec<_> = log.records().collect();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0].as_ref().unwrap(), b"one");
    assert!(got[1].is_err(), "the cut record must be an error, not the end of the log");
}

#[test]
fn a_torn_record_at_the_scan_ends_both_passes() {
    let dir = TempDir::new().unwrap();
    let meta = store(&dir, WalConfig::PerOp);
    meta.commit().unwrap();
    meta.log_op(b"one".to_vec());
    meta.log_op(b"two".to_vec());
    meta.flush_wal().unwrap();
    cut_tail(&only_segment(&dir), 3);
    let (_, _, log) = scan_store_with_log(&dir.path().join("segments")).unwrap();
    assert_eq!(log.len(), 1);
    let ops: Vec<Vec<u8>> = log.records().collect::<Result<_, _>>().unwrap();
    assert_eq!(ops, vec![b"one".to_vec()]);
}

#[test]
fn the_log_continues_into_later_segments() {
    let dir = TempDir::new().unwrap();
    let meta = store(&dir, WalConfig::PerOp);
    meta.log_op(b"old".to_vec());
    let root = meta.commit().unwrap();
    meta.log_op(b"one".to_vec());
    meta.flush_wal().unwrap();
    // A later daemon logs into a new segment (and crashes before a commit).
    let next = create_wal(WalConfig::PerOp, dir.path(), 1000).unwrap();
    next.log_mutation(&metadata::wal::WalEntry::Op { bytes: b"two".to_vec() }).unwrap();
    next.flush_and_sync().unwrap();
    let (last, _, log) = scan_store_with_log(&dir.path().join("segments")).unwrap();
    assert_eq!(last, Some(root));
    assert_eq!(log.len(), 2);
    let ops: Vec<Vec<u8>> = log.records().collect::<Result<_, _>>().unwrap();
    assert_eq!(ops, vec![b"one".to_vec(), b"two".to_vec()]);
}
