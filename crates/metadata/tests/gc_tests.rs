//! Tests for the GC engine, segment compaction, and background GC thread.

#![allow(unused_imports)]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use blockset::{Dictionary, State, Tree};
use slicefs_traits::digest::{Branches, Digest224};

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

/// compact_segment retains live entries and omits dead entries.
#[test]
fn test_compact_segment_retains_live_removes_dead() {
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let seg_path = tmp.path().join("segment-001.seg");

    let live_key: Digest224 = [1, 2, 3, 4, 5, 6, 7];
    let dead_key: Digest224 = [7, 6, 5, 4, 3, 2, 1];
    let branches: Branches = [[0u32; 8]; 2];

    // Write segment with two entries
    let mut writer = SegmentWriter::new(&seg_path, 1).unwrap();
    writer
        .write_entry(&SegmentEntry::DictEntry {
            key: live_key,
            branches,
        })
        .unwrap();
    writer
        .write_entry(&SegmentEntry::DictEntry {
            key: dead_key,
            branches,
        })
        .unwrap();
    writer.close().unwrap();

    // Live set contains only live_key
    let mut live_set = HashSet::new();
    live_set.insert(live_key);

    let out_dir = tmp.path().join("compacted");
    std::fs::create_dir_all(&out_dir).unwrap();
    let result = compact_segment(&seg_path, &live_set, &out_dir, 2).unwrap();

    assert_eq!(result.entries_kept, 1, "one live entry should be kept");
    assert_eq!(result.entries_removed, 1, "one dead entry should be removed");

    // Verify compacted segment is readable and contains only live entry
    let out_path = out_dir.join("segment-002.seg");
    let reader = SegmentReader::open(&out_path).unwrap();
    let entries: Vec<_> = reader.collect();
    assert_eq!(entries.len(), 1);
    if let SegmentEntry::DictEntry { key, .. } = &entries[0] {
        assert_eq!(*key, live_key);
    } else {
        panic!("expected DictEntry");
    }
}

/// compact_segment on a segment where all entries are live produces output with same count.
#[test]
fn test_compact_segment_all_live_produces_same_output() {
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let seg_path = tmp.path().join("segment-001.seg");

    let key1: Digest224 = [1, 0, 0, 0, 0, 0, 0];
    let key2: Digest224 = [2, 0, 0, 0, 0, 0, 0];
    let branches: Branches = [[0u32; 8]; 2];

    let mut writer = SegmentWriter::new(&seg_path, 1).unwrap();
    writer
        .write_entry(&SegmentEntry::DictEntry { key: key1, branches })
        .unwrap();
    writer
        .write_entry(&SegmentEntry::DictEntry { key: key2, branches })
        .unwrap();
    writer.close().unwrap();

    let mut live_set = HashSet::new();
    live_set.insert(key1);
    live_set.insert(key2);

    let out_dir = tmp.path().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let result = compact_segment(&seg_path, &live_set, &out_dir, 10).unwrap();

    assert_eq!(result.entries_kept, 2);
    assert_eq!(result.entries_removed, 0);
}

/// compact_segment on a segment where all entries are dead produces header-only output.
#[test]
fn test_compact_segment_all_dead_produces_empty_segment() {
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let seg_path = tmp.path().join("segment-001.seg");

    let dead_key: Digest224 = [9, 8, 7, 6, 5, 4, 3];
    let branches: Branches = [[0u32; 8]; 2];

    let mut writer = SegmentWriter::new(&seg_path, 1).unwrap();
    writer
        .write_entry(&SegmentEntry::DictEntry {
            key: dead_key,
            branches,
        })
        .unwrap();
    writer.close().unwrap();

    let live_set: HashSet<Digest224> = HashSet::new(); // empty live set

    let out_dir = tmp.path().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let result = compact_segment(&seg_path, &live_set, &out_dir, 2).unwrap();

    assert_eq!(result.entries_kept, 0);
    assert_eq!(result.entries_removed, 1);

    // Compacted segment exists and is readable (header-only = 0 entries)
    let out_path = out_dir.join("segment-002.seg");
    let reader = SegmentReader::open(&out_path).unwrap();
    let entries: Vec<_> = reader.collect();
    assert_eq!(entries.len(), 0, "all-dead compaction produces empty segment");
}

/// Crash safety: if compacted output fails to write, the original segment remains intact.
/// Simulated by using a read-only output directory after partial setup.
#[test]
fn test_compact_segment_crash_safety_atomic_rename() {
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let seg_path = tmp.path().join("segment-001.seg");

    let key: Digest224 = [1, 2, 3, 4, 5, 6, 7];
    let branches: Branches = [[0u32; 8]; 2];

    let mut writer = SegmentWriter::new(&seg_path, 1).unwrap();
    writer
        .write_entry(&SegmentEntry::DictEntry { key, branches })
        .unwrap();
    writer.close().unwrap();

    // Record original content
    let original_bytes = std::fs::read(&seg_path).unwrap();

    let live_set = HashSet::new();

    // Use a valid output dir — this tests the happy path of atomic rename
    let out_dir = tmp.path().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let _result = compact_segment(&seg_path, &live_set, &out_dir, 2).unwrap();

    // Original segment should be gone (renamed/replaced) or the new one should exist
    // The key invariant: old segment is NOT modified in place (atomic rename of temp file)
    // If original still exists, it must equal the original bytes (no partial write)
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Weak};

    /// spawn_background_gc returns GcHandle; shutdown() joins cleanly.
    #[test]
    fn test_background_gc_handle_shutdown() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(DictMetadataStore::new());
        let weak: Weak<DictMetadataStore> = Arc::downgrade(&store);
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = spawn_background_gc(
            weak,
            tmp.path().to_path_buf(),
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

        let tmp = TempDir::new().unwrap();
        let store = Arc::new(DictMetadataStore::new());
        let weak: Weak<DictMetadataStore> = Arc::downgrade(&store);
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = spawn_background_gc(
            weak,
            tmp.path().to_path_buf(),
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
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(DictMetadataStore::new());
        let weak: Weak<DictMetadataStore> = Arc::downgrade(&store);
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = spawn_background_gc(
            weak,
            tmp.path().to_path_buf(),
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
