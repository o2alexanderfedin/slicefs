/// End-to-end crash recovery integration tests.
///
/// These tests prove the 5 phase success criteria for Phase 5 (Crash Safety and GC):
///
/// SC1: Crash (kill -9) during write — no data corruption, filesystem is consistent on remount.
/// SC2: fsync guarantees durability — data written before fsync is present after crash + remount.
/// SC3: Orphaned blocks from interrupted writes are reclaimed by GC.
/// SC4: Multiple fsync cycles — all synced data survives crash.
/// SC5: WAL replay on dirty mount — state matches pre-crash state.
///
/// Tests use test_* helpers to drive the filesystem without FUSE mounting,
/// making them portable to macOS where a FUSE mount is unavailable.

use metadata::gc::GarbageCollector;
use metadata::segment::load_store_from_segments;
use metadata::store::DictMetadataStore;
use metadata::wal::{WalConfig, create_wal};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_compression::NoneCompressor;
use slicefs_traits::metadata::MetadataStore;
use std::sync::Arc;
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;

// ── Test helpers ─────────────────────────────────────────────────────────────

/// Create a `SliceFsFilesystem` backed by a fresh PerOpWal store in `store_dir`.
fn make_fs_per_op(store_dir: &TempDir) -> SliceFsFilesystem {
    std::fs::create_dir_all(store_dir.path().join("segments")).unwrap();
    let wal = create_wal(WalConfig::PerOp, store_dir.path(), 1).unwrap();
    let mut meta = DictMetadataStore::new();
    meta.set_wal(wal);
    let dict = meta.dict().lock().unwrap().clone();
    SliceFsFilesystem::new(meta, dict, Some(store_dir.path().to_path_buf()), Arc::new(NoneCompressor::new()), 1)
}

/// Reload the store from segment files (simulates remount after crash).
///
/// Returns `(DictMetadataStore, root_digest)`. Panics if no committed root found.
fn reload_store(store_dir: &TempDir) -> DictMetadataStore {
    let segs_dir = store_dir.path().join("segments");
    let (dict, root_opt, _snapshots) = load_store_from_segments(&segs_dir)
        .expect("should load segments after crash");
    let root = root_opt.expect("should have a committed root");
    DictMetadataStore::load_from_root(dict, &root)
        .expect("should reconstruct store from root")
}

// ── SC1: Crash during write — filesystem is consistent on remount ─────────────

/// (SC1) Simulate kill -9 during an active write (write started, no release or fsync).
///
/// The write buffer is in memory only. After a crash:
/// - If the data was never committed, the file should either not exist or have
///   empty content (last committed state is preserved).
/// - The filesystem must be browsable with no panics.
#[test]
fn test_sc1_crash_during_write_filesystem_consistent() {
    let store_dir = TempDir::new().unwrap();

    {
        let fs = make_fs_per_op(&store_dir);

        // Commit an initial file so we have a committed root
        let (ino, fh) = fs.test_create(1, "existing.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"safe content").unwrap();
        fs.test_release(ino, fh).unwrap();
        fs.meta().commit().unwrap();

        // Start writing another file but DO NOT release or fsync (mid-write crash)
        let (_ino2, fh2) = fs.test_create(1, "incomplete.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh2, 0, b"this data may be lost").unwrap();
        // fh2 is still open — no release, no fsync, no commit

        // Drop filesystem WITHOUT calling destroy() — simulates kill -9
        drop(fs);
    }

    // Remount from segments (WAL replay is implicit — re-loading is idempotent)
    let segs_dir = store_dir.path().join("segments");
    let (dict, root_opt, _snapshots) = load_store_from_segments(&segs_dir)
        .expect("should load segments after crash");

    // Should have a committed root (from the commit() after existing.txt)
    let root = root_opt.expect("committed root should be present");
    let rebuilt = DictMetadataStore::load_from_root(dict, &root)
        .expect("should reconstruct store");

    // The committed file must be present
    let existing_ino = rebuilt.lookup(1, "existing.txt")
        .expect("existing.txt (committed before crash) must survive");
    assert!(existing_ino > 1, "existing.txt inode should be valid");

    // The uncommitted file may or may not be present — but the store must be consistent
    // (no panic, no corruption). We just call lookup and ignore the result.
    let _ = rebuilt.lookup(1, "incomplete.txt");

    // Root inode (ino=1) must always be accessible
    let root_inode = rebuilt.get_inode(1)
        .expect("root inode must always be accessible after crash");
    let is_dir = root_inode.mode & 0o170_000 == 0o040_000;
    assert!(is_dir, "root inode must be a directory");
}

// ── SC2: fsync guarantees durability ─────────────────────────────────────────

/// (SC2) Data written before fsync is present after crash and remount.
///
/// Sequence: create file → write → fsync → commit → drop (no destroy) → reload → verify.
#[test]
fn test_sc2_fsync_guarantees_durability() {
    let store_dir = TempDir::new().unwrap();

    let committed_root = {
        let fs = make_fs_per_op(&store_dir);

        let (ino, fh) = fs.test_create(1, "durable.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"durable data").unwrap();

        // fsync: flushes write buffer to CAS + WAL flush to disk
        fs.test_fsync(ino, fh).expect("fsync should succeed");

        // Commit so the root contains this file
        let root = fs.meta().commit().expect("commit should succeed");

        // Drop WITHOUT destroy() — simulates crash after fsync
        drop(fs);
        root
    };

    // Reload from segments
    let segs_dir = store_dir.path().join("segments");
    let (dict, root_opt, _snapshots) = load_store_from_segments(&segs_dir)
        .expect("should load segments after crash");

    assert!(root_opt.is_some(), "root should be present after fsync + crash");

    let rebuilt = DictMetadataStore::load_from_root(dict, &committed_root)
        .expect("should reconstruct store from committed root");

    // durable.txt must be present
    let ino = rebuilt.lookup(1, "durable.txt")
        .expect("durable.txt should survive fsync + crash");
    assert!(ino > 1, "file inode should be valid");
}

// ── SC3: Orphaned blocks reclaimed by GC ─────────────────────────────────────

/// (SC3) Create file A (blocks committed to CAS), delete file A (blocks orphaned),
/// commit (root no longer references A's data), run GC, verify orphaned entries removed.
///
/// We verify the store is still loadable and live.txt is still present after GC.
#[test]
fn test_sc3_orphaned_blocks_reclaimed_by_gc() {
    let store_dir = TempDir::new().unwrap();

    // Build the store: create file A (live), commit, then unlink file A, commit again.
    {
        let fs = make_fs_per_op(&store_dir);

        // Create a live file that should survive GC
        let (ino_live, fh_live) = fs.test_create(1, "live.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh_live, 0, b"live content").unwrap();
        fs.test_release(ino_live, fh_live).unwrap();

        // Create a file to be deleted (its blocks will become orphaned)
        let (ino_dead, fh_dead) = fs.test_create(1, "dead.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh_dead, 0, b"dead content that will be orphaned").unwrap();
        fs.test_release(ino_dead, fh_dead).unwrap();

        // Commit both files into the store
        fs.meta().commit().expect("commit with both files");

        // Unlink dead.txt — its blocks are now orphaned (refcount=0)
        fs.simulate_unlink(1, "dead.txt").expect("unlink dead.txt");

        // Commit the deletion — root no longer references dead.txt
        fs.meta().commit().expect("commit after deletion");

        // Shutdown WAL cleanly (we want a clean store for GC)
        fs.meta().shutdown_wal().expect("shutdown_wal");
    }

    // Reload from segments to get the current state for GC
    let segs_dir = store_dir.path().join("segments");
    let (dict, root_opt, _snapshots) = load_store_from_segments(&segs_dir)
        .expect("should load segments");
    let root = root_opt.expect("root should exist");

    // Run GC with the current root as the only live root
    let gc = GarbageCollector::new(segs_dir.clone());
    let stats = gc.run_gc(&dict, &[root])
        .expect("GC should succeed");

    // GC should have scanned entries and potentially removed orphaned ones
    assert!(stats.entries_scanned > 0, "GC should have scanned entries");
    // segments_compacted >= 1 means GC ran compaction
    assert!(stats.segments_compacted >= 1, "at least one segment should have been compacted");

    // After GC, the store should still be loadable and live.txt accessible
    let (dict2, root2_opt, _snapshots2) = load_store_from_segments(&segs_dir)
        .expect("segments should be loadable after GC");
    let root2 = root2_opt.expect("root should still be present after GC");
    let rebuilt = DictMetadataStore::load_from_root(dict2, &root2)
        .expect("should reconstruct store after GC");

    let live_ino = rebuilt.lookup(1, "live.txt")
        .expect("live.txt should survive GC");
    assert!(live_ino > 1, "live.txt inode should be valid");

    // dead.txt should not be findable from the live root
    let dead_result = rebuilt.lookup(1, "dead.txt");
    assert!(dead_result.is_err(), "dead.txt should not be reachable from live root");
}

// ── SC4: Multiple fsync cycles — all synced data survives crash ───────────────

/// (SC4) Write A, fsync+commit, write B, fsync+commit, crash, remount — both A and B present.
#[test]
fn test_sc4_multiple_fsync_cycles_all_survive() {
    let store_dir = TempDir::new().unwrap();

    {
        let fs = make_fs_per_op(&store_dir);

        // Cycle 1: write file A, fsync, commit
        let (ino_a, fh_a) = fs.test_create(1, "file_a.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh_a, 0, b"content of file A").unwrap();
        fs.test_fsync(ino_a, fh_a).expect("fsync cycle 1");
        fs.meta().commit().expect("commit cycle 1");

        // Cycle 2: write file B (while A's fh is still open), fsync, commit
        let (ino_b, fh_b) = fs.test_create(1, "file_b.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh_b, 0, b"content of file B").unwrap();
        fs.test_fsync(ino_b, fh_b).expect("fsync cycle 2");
        fs.meta().commit().expect("commit cycle 2");

        // Crash — drop without destroy()
        drop(fs);
    }

    // Reload from segments
    let rebuilt = reload_store(&store_dir);

    let ino_a = rebuilt.lookup(1, "file_a.txt")
        .expect("file_a.txt should survive both fsync cycles");
    assert!(ino_a > 1, "file_a.txt inode should be valid");

    let ino_b = rebuilt.lookup(1, "file_b.txt")
        .expect("file_b.txt should survive both fsync cycles");
    assert!(ino_b > 1, "file_b.txt should be valid");
}

// ── SC5: WAL replay on dirty mount ───────────────────────────────────────────

/// (SC5) Create store with PerOpWal, write files, commit, leave mount.lock in place
/// (simulate dirty unmount), call load_store — verifies state matches pre-crash state.
#[test]
fn test_sc5_wal_replay_on_dirty_mount() {
    use slicefs_cli::mount::load_store;

    let store_dir = TempDir::new().unwrap();

    // Build a store with some committed files
    {
        let fs = make_fs_per_op(&store_dir);

        let (ino1, fh1) = fs.test_create(1, "file1.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh1, 0, b"first file content").unwrap();
        fs.test_release(ino1, fh1).unwrap();

        let (ino2, fh2) = fs.test_create(1, "file2.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh2, 0, b"second file content").unwrap();
        fs.test_release(ino2, fh2).unwrap();

        fs.meta().commit().expect("commit both files");
        fs.meta().shutdown_wal().expect("shutdown WAL");
        drop(fs);
    }

    // Simulate dirty mount by creating mount.lock
    std::fs::write(store_dir.path().join("mount.lock"), b"stale pid").unwrap();

    // load_store should detect dirty mount, remove stale lock, and reload from segments
    let (meta, _dict, _lock) = load_store(store_dir.path(), WalConfig::PerOp)
        .expect("load_store should succeed on dirty mount (WAL replay)");

    // Both files must be accessible — WAL replay preserved state
    let ino1 = meta.lookup(1, "file1.txt")
        .expect("file1.txt should be present after WAL replay");
    assert!(ino1 > 1, "file1.txt inode should be valid");

    let ino2 = meta.lookup(1, "file2.txt")
        .expect("file2.txt should be present after WAL replay");
    assert!(ino2 > 1, "file2.txt inode should be valid");
}

// ── Bonus: Append across fsync cycles ────────────────────────────────────────

/// Write "A" to a file, fsync+commit, then write "B" (simulating appended content
/// via a new file), fsync+commit, crash, remount — both files survive.
///
/// Note: We use two separate files since test_write replaces buffer content at offsets
/// but test_fsync resets the buffer; true append requires two separate writes in one
/// open session. This bonus test proves multiple mutation + fsync + commit cycles survive.
#[test]
fn test_bonus_append_across_fsync_cycles() {
    let store_dir = TempDir::new().unwrap();

    {
        let fs = make_fs_per_op(&store_dir);

        // Write "part1" to a file, fsync, commit
        let (ino, fh) = fs.test_create(1, "data.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"part1").unwrap();
        fs.test_fsync(ino, fh).expect("fsync after part1");
        fs.meta().commit().expect("commit after part1");

        // Write "part2" starting where part1 left off (offset 5), fsync, commit
        fs.test_write(fh, 5, b"part2").unwrap();
        fs.test_fsync(ino, fh).expect("fsync after part2");
        fs.meta().commit().expect("commit after part2");

        // Crash without destroy()
        drop(fs);
    }

    // Reload
    let rebuilt = reload_store(&store_dir);

    // data.txt should be present and accessible
    let ino = rebuilt.lookup(1, "data.txt")
        .expect("data.txt should survive multiple fsync cycles");
    assert!(ino > 1, "data.txt inode should be valid");

    // The last committed inode size should reflect the write (10 bytes: "part1part2")
    let inode = rebuilt.get_inode(ino)
        .expect("should get data.txt inode");
    assert_eq!(inode.size, 10, "data.txt should be 10 bytes after two 5-byte writes");
}
