/// Integration tests for DictMetadataStore segment persistence + WAL integration.
///
/// Covers:
///   - WAL logging on commits (RootUpdate records appear in segment file)
///   - load_store_from_segments reconstructs DictMetadataStore correctly
///   - Lock file lifecycle (acquire creates, drop removes)
///   - Dirty mount detection (lock file present → DirtyMount error)

use tempfile::TempDir;

use metadata::segment::{load_store_from_segments, SegmentEntry, SegmentReader};
use metadata::store::DictMetadataStore;
use metadata::wal::{WalConfig, create_wal};
use metadata::mount_lock::{acquire_mount_lock, MountLockError};
use slicefs_traits::metadata::{InodeMeta, MetadataStore};

const S_IFREG: u32 = 0o100_000;

// ── Test 1: WAL is called on commit ─────────────────────────────────────────

/// DictMetadataStore with PerOpWal — create inode, commit; the segment file
/// must contain a RootUpdate record after commit.
#[test]
fn test_wal_logs_root_update_on_commit() {
    let store_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(store_dir.path().join("segments")).unwrap();

    let wal = create_wal(WalConfig::PerOp, store_dir.path(), 1).unwrap();
    let mut meta = DictMetadataStore::new();
    meta.set_wal(wal);

    // Create an inode
    let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let ino = meta.create_inode(&file_meta).unwrap();
    meta.link(1, "test.txt", ino).unwrap();

    // commit() emits a RootUpdate to the WAL
    let _root = meta.commit().unwrap();
    meta.shutdown_wal().unwrap();

    // Read back the segment
    let seg_path = store_dir.path().join("segments").join("segment-000001.seg");
    let reader = SegmentReader::open(&seg_path).unwrap();
    let entries: Vec<_> = reader.collect();

    let has_root_update = entries.iter().any(|e| matches!(e, SegmentEntry::RootUpdate { .. }));
    assert!(has_root_update, "segment must contain a RootUpdate after commit(); entries: {}", entries.len());
}

// ── Test 2: load_store_from_segments reconstructs state ─────────────────────

/// Write a PerOpWal-backed store, then use load_store_from_segments to get the root.
/// The loaded root must match the committed root.
#[test]
fn test_load_store_from_segments_returns_root() {
    let store_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(store_dir.path().join("segments")).unwrap();

    let root_digest = {
        let wal = create_wal(WalConfig::PerOp, store_dir.path(), 1).unwrap();
        let mut meta = DictMetadataStore::new();
        meta.set_wal(wal);

        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
        let ino = meta.create_inode(&file_meta).unwrap();
        meta.link(1, "hello.txt", ino).unwrap();
        let root = meta.commit().unwrap();
        meta.shutdown_wal().unwrap();
        root
    };

    // Load from segments
    let segs_dir = store_dir.path().join("segments");
    let (loaded_root, _snapshots) = load_store_from_segments(&segs_dir).unwrap();
    assert!(loaded_root.is_some(), "should have a RootUpdate digest");
    assert_eq!(loaded_root.unwrap(), root_digest, "loaded root must match committed root");
}

// ── Test 3: Lock file lifecycle ───────────────────────────────────────────────

/// acquire_mount_lock creates mount.lock; MountLock::drop removes it.
#[test]
fn test_mount_lock_created_and_removed() {
    let store_dir = TempDir::new().unwrap();
    let lock_path = store_dir.path().join("mount.lock");

    assert!(!lock_path.exists(), "no lock file before acquire");

    {
        let _lock = acquire_mount_lock(store_dir.path()).unwrap();
        assert!(lock_path.exists(), "lock file must exist after acquire");
    } // _lock dropped here

    assert!(!lock_path.exists(), "lock file must be removed after MountLock drop");
}

// ── Test 4: Dirty mount detection ────────────────────────────────────────────

/// If mount.lock already exists when acquire_mount_lock is called,
/// it should return a DirtyMount error.
#[test]
fn test_dirty_mount_detected_when_lock_exists() {
    let store_dir = TempDir::new().unwrap();
    let lock_path = store_dir.path().join("mount.lock");

    // Simulate a pre-existing lock file (crash residue)
    std::fs::write(&lock_path, b"").unwrap();

    let result = acquire_mount_lock(store_dir.path());
    assert!(result.is_err(), "should return error on dirty mount");
    match result.err().unwrap() {
        MountLockError::DirtyMount => {} // expected
        e => panic!("expected DirtyMount, got {:?}", e),
    }
}
