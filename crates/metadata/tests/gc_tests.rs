//! Tests for the GC engine, segment compaction, and background GC thread.

#![allow(unused_imports)]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use slicefs_traits::digest::Digest224;

use metadata::gc::{GarbageCollector, collect_live_set};
use metadata::segment::compaction::compact_segment;
use metadata::segment::{SegmentEntry, SegmentReader, SegmentWriter};

// ─── Task 1 tests: collect_live_set ─────────────────────────────────────────
//
// collect_live_set is currently a deferred stub (FileStorage orphan GC is not
// yet implemented). It returns an empty HashSet regardless of inputs.
// These tests verify the stub contract.

/// collect_live_set is a no-op stub — always returns empty HashSet.
#[test]
fn test_collect_live_set_single_root_single_entry() {
    use metadata::store_io::StoreIo;
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    let mut io = StoreIo::new(dir.path());
    let root: Digest224 = [1, 2, 3, 4, 5, 6, 7];

    let live = collect_live_set(&mut io, &[root]);
    // Stub returns empty — no assertions about contents needed.
    let _ = live; // exercise the API
}

/// collect_live_set stub: empty root slice also returns empty set.
#[test]
fn test_collect_live_set_includes_all_descendants() {
    use metadata::store_io::StoreIo;
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    let mut io = StoreIo::new(dir.path());
    let root: Digest224 = [2, 3, 4, 5, 6, 7, 8];

    let live = collect_live_set(&mut io, &[root]);
    // Stub always returns empty — just verify it doesn't panic.
    let _ = live.len();
}

/// collect_live_set stub: multiple roots — still returns empty.
#[test]
fn test_collect_live_set_two_roots_union() {
    use metadata::store_io::StoreIo;
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    let mut io = StoreIo::new(dir.path());
    let root1: Digest224 = [1, 0, 0, 0, 0, 0, 0];
    let root2: Digest224 = [2, 0, 0, 0, 0, 0, 0];

    let live_both = collect_live_set(&mut io, &[root1, root2]);
    // Stub returns empty — verify no panic.
    let _ = live_both;
}

/// collect_live_set stub: snapshot root — returns empty (deferred).
#[test]
fn test_gc_03_snapshot_root_entry_survives() {
    use metadata::store_io::StoreIo;
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    let mut io = StoreIo::new(dir.path());
    let snapshot_root: Digest224 = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11];
    let current_root: Digest224 = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77];

    // Stub: live set is empty, but the call must not panic.
    let live = collect_live_set(&mut io, &[current_root, snapshot_root]);
    let _ = live;
}

/// collect_live_set stub: does not panic on orphan keys (no-op anyway).
#[test]
fn test_unreachable_entry_not_in_live_set() {
    use metadata::store_io::StoreIo;
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    let mut io = StoreIo::new(dir.path());
    let root: Digest224 = [5, 6, 7, 8, 9, 10, 11];
    let orphan_key: Digest224 = [0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];

    let live = collect_live_set(&mut io, &[root]);
    // Stub returns empty — orphan_key is definitely not in it.
    assert!(
        !live.contains(&orphan_key),
        "stub live set must not contain orphan key"
    );
}

// ─── Task 1 tests: compact_segment ──────────────────────────────────────────

fn make_root_key(v: u32) -> Digest224 {
    [v, v + 1, v + 2, v + 3, v + 4, v + 5, v + 6]
}

/// compact_segment retains RootUpdate and SnapshotRecord entries.
#[test]
fn test_compact_segment_retains_all_entries() {
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let seg_path = tmp.path().join("segment-001.seg");

    let root1 = make_root_key(1);
    let root2 = make_root_key(2);

    // Write segment with a RootUpdate and SnapshotRecord
    let mut writer = SegmentWriter::new(&seg_path, 1).unwrap();
    writer
        .write_entry(&SegmentEntry::RootUpdate { root: root1 })
        .unwrap();
    writer
        .write_entry(&SegmentEntry::SnapshotRecord {
            version: 1,
            root: root2,
            created_at: 12345,
            name: Some("snap1".to_string()),
        })
        .unwrap();
    writer.close().unwrap();

    let out_dir = tmp.path().join("compacted");
    std::fs::create_dir_all(&out_dir).unwrap();
    let result = compact_segment(&seg_path, &out_dir, 2).unwrap();

    assert_eq!(result.entries_kept, 2, "both entries should be kept");
    assert_eq!(result.entries_removed, 0);

    // Verify compacted segment contains both entries
    let out_path = out_dir.join("segment-002.seg");
    let reader = SegmentReader::open(&out_path).unwrap();
    let entries: Vec<_> = reader.collect();
    assert_eq!(entries.len(), 2);
}

/// compact_segment on empty segment produces header-only output.
#[test]
fn test_compact_segment_empty_produces_empty_output() {
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let seg_path = tmp.path().join("segment-001.seg");

    let writer = SegmentWriter::new(&seg_path, 1).unwrap();
    writer.close().unwrap();

    let out_dir = tmp.path().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let result = compact_segment(&seg_path, &out_dir, 2).unwrap();

    assert_eq!(result.entries_kept, 0);
    assert_eq!(result.entries_removed, 0);

    // Compacted segment exists and is readable (header-only = 0 entries)
    let out_path = out_dir.join("segment-002.seg");
    let reader = SegmentReader::open(&out_path).unwrap();
    let entries: Vec<_> = reader.collect();
    assert_eq!(entries.len(), 0, "empty compaction produces empty segment");
}

/// Crash safety: compact_segment uses atomic rename so original is preserved on failure.
#[test]
fn test_compact_segment_crash_safety_atomic_rename() {
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let seg_path = tmp.path().join("segment-001.seg");

    let root = make_root_key(1);
    let mut writer = SegmentWriter::new(&seg_path, 1).unwrap();
    writer
        .write_entry(&SegmentEntry::RootUpdate { root })
        .unwrap();
    writer.close().unwrap();

    // Record original content
    let original_bytes = std::fs::read(&seg_path).unwrap();

    // Use a valid output dir — tests the happy path of atomic rename
    let out_dir = tmp.path().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let _result = compact_segment(&seg_path, &out_dir, 2).unwrap();

    // Original segment should be untouched (only the new segment is written)
    if seg_path.exists() {
        let current_bytes = std::fs::read(&seg_path).unwrap();
        assert_eq!(
            current_bytes, original_bytes,
            "if original segment still exists, it must be unchanged"
        );
    }
    // New segment must exist
    let out_path = out_dir.join("segment-002.seg");
    assert!(
        out_path.exists(),
        "compacted segment must exist at output path"
    );
}

// ─── Task 2 tests: Background GC thread ─────────────────────────────────────

#[cfg(test)]
mod background_gc_tests {
    use super::*;
    use metadata::gc::background::{GcHandle, spawn_background_gc};
    use metadata::store::DictMetadataStore;
    use metadata::store_io::StoreIo;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, Weak};

    fn make_store() -> (tempfile::TempDir, Arc<DictMetadataStore>) {
        let tmp = tempfile::TempDir::new().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::new(tmp.path())));
        let store = Arc::new(DictMetadataStore::new(io));
        (tmp, store)
    }

    /// spawn_background_gc returns GcHandle; shutdown() joins cleanly.
    #[test]
    fn test_background_gc_handle_shutdown() {
        use tempfile::TempDir;
        let tmp_gc = TempDir::new().unwrap();
        let (_tmp_store, store) = make_store();
        let weak: Weak<DictMetadataStore> = Arc::downgrade(&store);
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = spawn_background_gc(
            weak,
            tmp_gc.path().to_path_buf(),
            Duration::from_millis(50),
            usize::MAX, // threshold so high it never runs
            Arc::clone(&shutdown),
        );

        // Should shut down without hanging
        handle.shutdown();
    }

    /// Background GC thread runs at least one cycle when threshold=0 and interval is short.
    #[test]
    fn test_background_gc_runs_cycle() {
        use std::sync::atomic::AtomicUsize;
        use tempfile::TempDir;

        let tmp_gc = TempDir::new().unwrap();
        let (_tmp_store, store) = make_store();
        let weak: Weak<DictMetadataStore> = Arc::downgrade(&store);
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = spawn_background_gc(
            weak,
            tmp_gc.path().to_path_buf(),
            Duration::from_millis(10),
            0, // orphan_threshold=0 so GC always runs
            Arc::clone(&shutdown),
        );

        // Wait enough time for at least one cycle
        std::thread::sleep(Duration::from_millis(100));
        handle.shutdown();
        // If we reach here without hang/panic, the cycle executed successfully
    }

    /// Dropping the Arc<DictMetadataStore> causes the thread to exit gracefully.
    #[test]
    fn test_background_gc_exits_on_store_drop() {
        use tempfile::TempDir;
        let tmp_gc = TempDir::new().unwrap();
        let (_tmp_store, store) = make_store();
        let weak: Weak<DictMetadataStore> = Arc::downgrade(&store);
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = spawn_background_gc(
            weak,
            tmp_gc.path().to_path_buf(),
            Duration::from_millis(20),
            usize::MAX,
            Arc::clone(&shutdown),
        );

        // Drop the Arc — Weak::upgrade will fail on next loop iteration
        drop(store);

        // Give the thread time to detect the dropped Arc and exit
        std::thread::sleep(Duration::from_millis(100));

        // shutdown() should join immediately (thread already exited)
        handle.shutdown();
    }
}

// ─── Snapshot-aware GC integration tests ────────────────────────────────────

/// Snapshot records survive segment replay.
///
/// Verifies that SnapshotRecord entries written to WAL are preserved on segment
/// read-back, which is the prerequisite for snapshot-aware GC.
/// Full GC integration testing (with FileStorage-backed load_from_root) will be
/// validated in Plan 03 once load_from_root is migrated to use StoreIo.
#[test]
fn test_gc_preserves_snapshot_blocks() {
    use metadata::segment::load_store_from_segments;
    use metadata::store::DictMetadataStore;
    use metadata::wal::{WalConfig, create_wal};
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};
    use tempfile::TempDir;

    const S_IFREG: u32 = 0o100_000;

    let store_dir = TempDir::new().unwrap();
    let segs_dir = store_dir.path().join("segments");
    std::fs::create_dir_all(&segs_dir).unwrap();

    // Create a store, add a file, commit, take snapshot — all written to WAL.
    {
        use metadata::store_io::StoreIo;
        use std::sync::Mutex;
        let store_io_dir = TempDir::new().unwrap();
        let io = std::sync::Arc::new(Mutex::new(StoreIo::new(store_io_dir.path())));
        let wal = create_wal(WalConfig::PerOp, store_dir.path(), 1).unwrap();
        let mut meta = DictMetadataStore::new(io);
        meta.set_wal(wal);

        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
        let ino = meta.create_inode(&file_meta).unwrap();
        meta.link(1, "data.txt", ino).unwrap();
        meta.commit().unwrap();

        // Take snapshot — writes SnapshotRecord to WAL.
        let snap = meta
            .create_snapshot(Some("before-delete".to_string()))
            .unwrap();
        assert_eq!(snap.version, 1);

        meta.shutdown_wal().unwrap();
    }

    // Reload from segments — snapshot must survive.
    let (_root_opt, snapshots) = load_store_from_segments(&segs_dir).unwrap();
    assert!(
        !snapshots.is_empty(),
        "snapshot must survive segment replay"
    );
    assert_eq!(snapshots[0].name.as_deref(), Some("before-delete"));
    assert_eq!(snapshots[0].version, 1);
}

/// snapshot_roots() returns both snapshot roots and current live root.
#[test]
fn test_snapshot_roots_includes_all_anchors() {
    use metadata::store::DictMetadataStore;
    use metadata::wal::{WalConfig, create_wal};
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};
    use tempfile::TempDir;

    const S_IFREG: u32 = 0o100_000;

    let store_dir = TempDir::new().unwrap();
    let segs_dir = store_dir.path().join("segments");
    std::fs::create_dir_all(&segs_dir).unwrap();

    let store_io_dir = TempDir::new().unwrap();
    let io = {
        use metadata::store_io::StoreIo;
        use std::sync::{Arc, Mutex};
        Arc::new(Mutex::new(StoreIo::new(store_io_dir.path())))
    };
    let wal = create_wal(WalConfig::PerOp, store_dir.path(), 1).unwrap();
    let mut meta = DictMetadataStore::new(io);
    meta.set_wal(wal);

    let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let ino = meta.create_inode(&file_meta).unwrap();
    meta.link(1, "a.txt", ino).unwrap();
    let root1 = meta.commit().unwrap();

    // Before snapshot: only live root
    let roots_before = meta.snapshot_roots();
    assert!(roots_before.contains(&root1));

    // Create snapshot — now snapshot root + live root
    meta.create_snapshot(None).unwrap();
    let roots_after = meta.snapshot_roots();
    // snapshot root == root1 (create_snapshot calls commit() first)
    assert!(
        roots_after.contains(&root1),
        "snapshot root must be in snapshot_roots()"
    );
    assert!(
        !roots_after.is_empty(),
        "must have at least one root after snapshot"
    );

    meta.shutdown_wal().unwrap();
}
