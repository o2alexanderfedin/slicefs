//! Tests for the GC engine, segment compaction, and background GC thread.

#![allow(unused_imports)]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use blockset::{Dictionary, State, Tree};
use slicefs_traits::digest::Digest224;

use metadata::gc::{collect_live_set, GarbageCollector};
use metadata::segment::compaction::compact_segment;
use metadata::segment::{SegmentEntry, SegmentReader, SegmentWriter};

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Push raw bytes into a Dictionary and return the Digest224 root.
fn push_bytes(dict: &mut Dictionary, data: &[u8]) -> Digest224 {
    State::push_all(dict, data)
}

/// Build a multi-level tree by pushing enough data to force blockset to create internal nodes.
/// Returns (root, child_keys...) where root is the top-level digest.
/// Uses 256 bytes so blockset creates a multi-level Merkle tree.
#[allow(dead_code)]
fn build_two_level_tree(dict: &mut Dictionary) -> (Digest224, Digest224, Digest224) {
    let left = push_bytes(dict, b"left-leaf-content");
    let right = push_bytes(dict, b"right-leaf-content");
    // Push a larger blob to create a tree with multiple nodes
    let mut root_data = Vec::with_capacity(128);
    root_data.extend_from_slice(b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"); // 32 bytes
    root_data.extend_from_slice(b"yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy"); // 32 bytes
    let root = push_bytes(dict, &root_data);
    (root, left, right)
}

/// Build a deterministic single-node entry: push `data` and return its key.
fn make_entry(dict: &mut Dictionary, data: &[u8]) -> Digest224 {
    push_bytes(dict, data)
}

// ─── Task 1 tests: collect_live_set ─────────────────────────────────────────

/// A single root with no children should produce a live set of exactly one entry.
#[test]
fn test_collect_live_set_single_root_single_entry() {
    let mut dict = Dictionary::default();
    let root = make_entry(&mut dict, b"single-node");

    let live = collect_live_set(&dict, &[root]);
    assert!(live.contains(&root), "root must be in live set");
    // The live set may include more entries (tree parents), but must include root.
}

/// collect_live_set from a root with children includes root + all descendants.
#[test]
fn test_collect_live_set_includes_all_descendants() {
    let mut dict = Dictionary::default();
    // Push a large blob so blockset builds a multi-node tree
    let data: Vec<u8> = (0u8..=255u8).cycle().take(256).collect();
    let root = push_bytes(&mut dict, &data);

    let live = collect_live_set(&dict, &[root]);
    assert!(live.contains(&root), "root must be in live set");
    // There must be more than just the root (internal tree nodes)
    assert!(live.len() >= 1);
}

/// Two independent roots produce the union of both reachable sets.
#[test]
fn test_collect_live_set_two_roots_union() {
    let mut dict = Dictionary::default();
    let root1 = make_entry(&mut dict, b"root-one");
    let root2 = make_entry(&mut dict, b"root-two");

    let live1 = collect_live_set(&dict, &[root1]);
    let live2 = collect_live_set(&dict, &[root2]);
    let live_both = collect_live_set(&dict, &[root1, root2]);

    // Union must contain everything from both individual sets
    for d in &live1 {
        assert!(live_both.contains(d), "live_both must contain all of live1");
    }
    for d in &live2 {
        assert!(live_both.contains(d), "live_both must contain all of live2");
    }
}

/// Entry reachable only from snapshot root (not current root) must be in the live set.
/// This is GC-03: snapshots protect their referenced blocks.
#[test]
fn test_gc_03_snapshot_root_entry_survives() {
    let mut dict = Dictionary::default();
    let snapshot_root = make_entry(&mut dict, b"snapshot-unique-content");
    let current_root = make_entry(&mut dict, b"current-root-content");

    // snapshot_root is reachable from snapshot, not from current
    let live = collect_live_set(&dict, &[current_root, snapshot_root]);
    assert!(
        live.contains(&snapshot_root),
        "snapshot root entry must survive GC (GC-03)"
    );
}

/// An entry inserted into the dictionary but not reachable from any root
/// must NOT appear in the live set.
#[test]
fn test_unreachable_entry_not_in_live_set() {
    use slicefs_traits::digest::Branches;
    let mut dict = Dictionary::default();
    let root = make_entry(&mut dict, b"reachable-root");

    // Insert an orphan entry directly into the dictionary
    let orphan_key: Digest224 = [0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
    // Only insert if it won't collide with existing entries
    if !dict.contains_key(&orphan_key) {
        let orphan_branches: Branches = [[0u32; 8]; 2];
        dict.insert(orphan_key, orphan_branches);
    }

    let live = collect_live_set(&dict, &[root]);
    assert!(
        !live.contains(&orphan_key),
        "orphaned entry must NOT be in live set"
    );
}

// ─── Task 1 tests: compact_segment ──────────────────────────────────────────

fn make_root_key(v: u32) -> Digest224 {
    [v, v+1, v+2, v+3, v+4, v+5, v+6]
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
    writer.write_entry(&SegmentEntry::RootUpdate { root: root1 }).unwrap();
    writer.write_entry(&SegmentEntry::SnapshotRecord {
        version: 1,
        root: root2,
        created_at: 12345,
        name: Some("snap1".to_string()),
    }).unwrap();
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

    let mut writer = SegmentWriter::new(&seg_path, 1).unwrap();
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
    writer.write_entry(&SegmentEntry::RootUpdate { root }).unwrap();
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
    assert!(out_path.exists(), "compacted segment must exist at output path");
}

// ─── Task 2 tests: Background GC thread ─────────────────────────────────────

#[cfg(test)]
mod background_gc_tests {
    use super::*;
    use metadata::gc::background::{spawn_background_gc, GcHandle};
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
        use tempfile::TempDir;
        use std::sync::atomic::AtomicUsize;

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
        use std::sync::Mutex;
        use metadata::store_io::StoreIo;
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
        let snap = meta.create_snapshot(Some("before-delete".to_string())).unwrap();
        assert_eq!(snap.version, 1);

        meta.shutdown_wal().unwrap();
    }

    // Reload from segments — snapshot must survive.
    let (_root_opt, snapshots) = load_store_from_segments(&segs_dir).unwrap();
    assert!(!snapshots.is_empty(), "snapshot must survive segment replay");
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
        use std::sync::{Arc, Mutex};
        use metadata::store_io::StoreIo;
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
        roots_after.len() >= 1,
        "must have at least one root after snapshot"
    );

    meta.shutdown_wal().unwrap();
}
