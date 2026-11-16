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
