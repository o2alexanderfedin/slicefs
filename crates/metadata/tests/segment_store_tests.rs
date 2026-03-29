/// Integration tests for DictMetadataStore segment persistence + WAL integration.
///
/// Covers:
///   - WAL logging on mutations (DictEntry records appear in segment file)
///   - load_store_from_segments reconstructs DictMetadataStore correctly
///   - Legacy dictionary.bin auto-migration to segment format
///   - Lock file lifecycle (acquire creates, drop removes)
///   - Dirty mount detection (lock file present → DirtyMount error)

use tempfile::TempDir;

use metadata::segment::{load_store_from_segments, migrate_legacy_store, SegmentEntry, SegmentReader};
use metadata::store::{serialize_dictionary, DictMetadataStore};
use metadata::wal::{WalConfig, create_wal};
use metadata::mount_lock::{acquire_mount_lock, MountLockError};
use slicefs_traits::metadata::{InodeMeta, MetadataStore};

const S_IFREG: u32 = 0o100_000;

// ── Test 1: WAL is called on mutations ──────────────────────────────────────

/// DictMetadataStore with PerOpWal — create inode, commit; the segment file
/// must contain at least one DictEntry record and one RootUpdate.
#[test]
fn test_wal_logs_mutations_and_root_update() {
    let store_dir = TempDir::new().unwrap();
    std::fs::create_dir_all(store_dir.path().join("segments")).unwrap();

    let wal = create_wal(WalConfig::PerOp, store_dir.path(), 1).unwrap();
    let mut meta = DictMetadataStore::new();
    meta.set_wal(wal);

    // Create an inode — should emit at least one DictEntry WAL entry
    let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let ino = meta.create_inode(&file_meta).unwrap();
    meta.link(1, "test.txt", ino).unwrap();

    // commit() should emit a RootUpdate
    let _root = meta.commit().unwrap();
    meta.shutdown_wal().unwrap();

    // Read back the segment
    let seg_path = store_dir.path().join("segments").join("segment-000001.seg");
    let reader = SegmentReader::open(&seg_path).unwrap();
    let entries: Vec<_> = reader.collect();

    let has_dict_entry = entries.iter().any(|e| matches!(e, SegmentEntry::DictEntry { .. }));
    let has_root_update = entries.iter().any(|e| matches!(e, SegmentEntry::RootUpdate { .. }));

    assert!(has_dict_entry, "segment must contain at least one DictEntry; entries: {}", entries.len());
    assert!(has_root_update, "segment must contain a RootUpdate after commit(); entries: {}", entries.len());
}

// ── Test 2: load_store_from_segments reconstructs state ─────────────────────

/// Write a PerOpWal-backed store, then use load_store_from_segments to rebuild
/// the Dictionary. The rebuilt store must have the same inode we created.
#[test]
fn test_load_store_from_segments_restores_inodes() {
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
    let (dict, loaded_root) = load_store_from_segments(&segs_dir).unwrap();
    assert!(loaded_root.is_some(), "should have a RootUpdate digest");
    assert_eq!(loaded_root.unwrap(), root_digest, "loaded root must match committed root");

    // Reconstruct the metadata store
    let rebuilt = DictMetadataStore::load_from_root(dict, &root_digest).unwrap();
    let found_ino = rebuilt.lookup(1, "hello.txt").unwrap();
    assert!(found_ino > 1, "hello.txt inode should be > 1 after reload");
}

// ── Test 3: legacy dictionary.bin auto-migrated ──────────────────────────────

/// Write a legacy dictionary.bin + root.bin store. Call migrate_legacy_store.
/// Verify segments/ directory is created and dictionary.bin is removed.
#[test]
fn test_legacy_store_auto_migration() {
    let store_dir = TempDir::new().unwrap();

    // Write a legacy store
    let meta = DictMetadataStore::new();
    let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let ino = meta.create_inode(&file_meta).unwrap();
    meta.link(1, "migrated.txt", ino).unwrap();
    let root = meta.commit().unwrap();

    // Write dictionary.bin
    let dict_bytes = {
        let dict = meta.dict().lock().unwrap();
        serialize_dictionary(&*dict)
    };
    std::fs::write(store_dir.path().join("dictionary.bin"), &dict_bytes).unwrap();

    // Write root.bin
    let mut root_bytes = Vec::with_capacity(28);
    for word in &root {
        root_bytes.extend_from_slice(&word.to_le_bytes());
    }
    std::fs::write(store_dir.path().join("root.bin"), &root_bytes).unwrap();

    // Perform migration
    migrate_legacy_store(store_dir.path()).unwrap();

    // dictionary.bin should be gone
    assert!(
        !store_dir.path().join("dictionary.bin").exists(),
        "dictionary.bin should be removed after migration"
    );

    // segments/ directory should exist with at least one segment file
    let segs_dir = store_dir.path().join("segments");
    assert!(segs_dir.is_dir(), "segments/ directory must exist after migration");

    let seg_files: Vec<_> = std::fs::read_dir(&segs_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(!seg_files.is_empty(), "at least one segment file must exist");

    // Load from segments and verify the inode is recoverable
    let (dict, loaded_root) = load_store_from_segments(&segs_dir).unwrap();
    assert!(loaded_root.is_some(), "migrated store must have a RootUpdate");
    let rebuilt = DictMetadataStore::load_from_root(dict, &loaded_root.unwrap()).unwrap();
    let found_ino = rebuilt.lookup(1, "migrated.txt").unwrap();
    assert!(found_ino > 1, "migrated.txt must be findable after migration");
}

// ── Test 4: Lock file lifecycle ───────────────────────────────────────────────

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

// ── Test 5: Dirty mount detection ────────────────────────────────────────────

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
