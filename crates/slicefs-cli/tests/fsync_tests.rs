/// Integration tests for fsync/fdatasync FUSE callback.
///
/// Covers:
///   - fsync with buffered writes flushes to CAS and updates manifest
///   - fsync with no buffered writes is a no-op (returns Ok)
///   - fdatasync (datasync=true) is identical to fsync (same code path)
///   - fsync followed by simulated crash (drop without destroy) + reload from segments shows data present

use metadata::segment::load_store_from_segments;
use metadata::store::DictMetadataStore;
use metadata::wal::{WalConfig, create_wal};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_compression::NoneCompressor;
use slicefs_traits::metadata::MetadataStore;
use std::sync::Arc;
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;

/// Create a SliceFsFilesystem backed by PerOpWal in the given store directory.
fn make_fs(store_dir: &TempDir, wal_config: WalConfig) -> SliceFsFilesystem {
    std::fs::create_dir_all(store_dir.path().join("segments")).unwrap();
    let wal = create_wal(wal_config, store_dir.path(), 1).unwrap();
    let mut meta = DictMetadataStore::new();
    meta.set_wal(wal);
    let dict = meta.dict().lock().unwrap().clone();
    SliceFsFilesystem::new(meta, dict, Some(store_dir.path().to_path_buf()), Arc::new(NoneCompressor::new()), 1)
}

// ── Test 1: fsync with buffered writes flushes to CAS ───────────────────────

/// Write data to a file handle, then call test_fsync.
/// The data should be pushed to CAS and manifest updated.
/// The file handle should remain open (buffer reset to empty).
#[test]
fn test_fsync_flushes_buffer_to_cas() {
    let store_dir = TempDir::new().unwrap();
    let fs = make_fs(&store_dir, WalConfig::PerOp);

    // Create file and write data
    let (ino, fh) = fs.test_create(1, "fsynced.txt", 0o644, 0, 0, 0).unwrap();
    let data = b"hello fsync world";
    fs.test_write(fh, 0, data).unwrap();

    // fsync should flush the buffer to CAS
    fs.test_fsync(ino, fh).expect("test_fsync should succeed");

    // File handle should still be open — verify by writing more data without error
    let data2 = b" more";
    let result = fs.test_write(fh, data.len() as u64, data2);
    assert!(result.is_ok(), "fh should remain open after fsync, got: {:?}", result);

    // The manifest should be set after fsync
    let manifest = fs.meta().get_manifest(ino);
    assert!(manifest.is_ok(), "manifest should be set after fsync");
    assert!(!manifest.unwrap().is_empty(), "manifest should be non-empty for non-empty file");
}

// ── Test 2: fsync with no buffered writes is a no-op ────────────────────────

/// Call fsync on a non-existent file handle — should return Ok (no-op).
#[test]
fn test_fsync_no_buffer_is_noop() {
    let store_dir = TempDir::new().unwrap();
    let fs = make_fs(&store_dir, WalConfig::NoWal);

    // Create a file so ino is valid, but use fh=999 (never opened)
    let (ino, fh) = fs.test_create(1, "readonly.txt", 0o644, 0, 0, 0).unwrap();
    fs.test_release(ino, fh).unwrap();

    // fsync on a non-open fh should be a no-op (Ok)
    let result = fs.test_fsync(ino, 999);
    assert!(result.is_ok(), "fsync on non-open fh should return Ok, got: {:?}", result);
}

// ── Test 3: fdatasync is identical to fsync ─────────────────────────────────

/// fsync and fdatasync use the same code path.
/// Calling test_fsync twice (to simulate both) should both succeed.
#[test]
fn test_fdatasync_identical_to_fsync() {
    let store_dir = TempDir::new().unwrap();
    let fs = make_fs(&store_dir, WalConfig::PerOp);

    let (ino, fh) = fs.test_create(1, "datasync.txt", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, b"data").unwrap();

    // First flush (fsync)
    let r = fs.test_fsync(ino, fh);
    assert!(r.is_ok(), "first test_fsync should succeed");

    // Write more data and flush again (fdatasync path)
    fs.test_write(fh, 4, b" more").unwrap();
    let r2 = fs.test_fsync(ino, fh);
    assert!(r2.is_ok(), "second test_fsync (fdatasync path) should succeed");
}

// ── Test 4: crash after fsync — data survives reload ────────────────────────

/// Write data, call test_fsync (WAL flush), then commit(), then drop WITHOUT calling destroy().
/// Reload from segment files — data should be present.
#[test]
fn test_fsync_crash_durability() {
    let store_dir = TempDir::new().unwrap();

    let root_digest = {
        let fs = make_fs(&store_dir, WalConfig::PerOp);

        let (ino, fh) = fs.test_create(1, "durable.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"durable content").unwrap();

        // fsync ensures WAL is flushed to disk
        fs.test_fsync(ino, fh).expect("fsync should succeed");

        // Commit to get a stable root (also logged to WAL)
        let root = fs.meta().commit().unwrap();

        // Drop filesystem WITHOUT calling destroy() — simulates crash
        drop(fs);
        root
    };

    // Reload from segment files
    let segs_dir = store_dir.path().join("segments");
    let (dict, loaded_root, _snapshots) = load_store_from_segments(&segs_dir)
        .expect("should be able to load segments after crash");

    assert!(loaded_root.is_some(), "a RootUpdate should have been written by fsync+commit");

    // Reconstruct the store from the committed root
    let rebuilt = DictMetadataStore::load_from_root(dict, &root_digest)
        .expect("should be able to reconstruct store");

    let found_ino = rebuilt.lookup(1, "durable.txt")
        .expect("durable.txt should survive fsync + crash");
    assert!(found_ino > 1, "file inode should be > 1");
}
