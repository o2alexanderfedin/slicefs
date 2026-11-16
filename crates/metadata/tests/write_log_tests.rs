//! Write log records (#20): operation records after the last root are
//! returned in order; a commit (RootUpdate) is the cut.

use std::sync::{Arc, Mutex};

use metadata::segment::load_store_with_log;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_traits::metadata::MetadataStore;
use tempfile::TempDir;

fn store(dir: &TempDir, config: WalConfig) -> DictMetadataStore {
    std::fs::create_dir_all(dir.path().join("segments")).unwrap();
    let wal = create_wal(config, dir.path(), 1).unwrap();
    let mut meta = DictMetadataStore::new(Arc::new(Mutex::new(StoreIo::new(dir.path()))));
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
