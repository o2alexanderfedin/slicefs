//! ROADMAP criterion 2 on the manifest tree (PMAP-03): a commit of one change
//! reads and writes a number of nodes bounded by the tree height, at two store
//! sizes; a commit with no change keeps the root and writes no tree node.

use std::sync::{Arc, Mutex};

use metadata::store::{CommitStats, DictMetadataStore};
use metadata::store_io::StoreIo;
use slicefs_traits::key::ObjectKey;
use slicefs_traits::metadata::{InodeMeta, MetadataStore};

/// A store with `n` files, each with a manifest, committed. `dir` keeps the
/// store alive; the files' inode numbers are returned.
fn store_with(n: u32) -> (tempfile::TempDir, DictMetadataStore, Vec<u64>) {
    let dir = tempfile::TempDir::new().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let store = DictMetadataStore::new(io);
    let root = store.root_ino();
    let files = (0..n)
        .map(|i| {
            let ino = store.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
            store.link(root, &format!("f{i}"), ino).unwrap();
            store.set_manifest(ino, &[ObjectKey::example(i)]).unwrap();
            ino
        })
        .collect();
    store.commit().unwrap();
    (dir, store, files)
}

/// The stats of the manifest tree in a commit's stats.
fn manifest(s: &CommitStats) -> &metadata::store::TreeStats {
    s.trees.iter().find(|t| t.name == "manifest_data").expect("manifest tree stats")
}

/// Change the manifest of one existing file and commit: the stats of that commit.
fn one_change(store: &DictMetadataStore, files: &[u64]) -> CommitStats {
    store.set_manifest(files[files.len() / 2], &[ObjectKey::example(files.len() as u32 + 7)]).unwrap();
    store.commit().unwrap();
    store.last_commit_stats()
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn one_change_manifest_commit_grows_with_height() {
    let mut seen = Vec::new();
    for n in [2_000u32, 40_000] {
        let (_dir, store, files) = store_with(n);
        let stats = one_change(&store, &files);
        let t = manifest(&stats);
        let h = u64::from(t.height);
        assert_eq!(t.entries, u64::from(n), "entries of the tree");
        assert!(t.apply.nodes_written >= 1, "n={n}: a change writes a node");
        assert!(t.apply.nodes_written <= 2 * h + 1, "n={n}: wrote {} nodes, height {h}", t.apply.nodes_written);
        assert!(t.apply.nodes_read <= 3 * h, "n={n}: read {} nodes, height {h}", t.apply.nodes_read);
        // The commit totals hold the tree work plus the root record.
        assert_eq!(stats.nodes_written, t.apply.nodes_written + 1);
        assert_eq!(stats.nodes_read, t.apply.nodes_read);
        assert!(stats.bytes_written >= 197 + t.apply.bytes_written);
        assert!(stats.presync_ms >= 0.0);
        seen.push((h, t.apply.nodes_read + t.apply.nodes_written));
    }
    let ((h_small, work_small), (h_large, work_large)) = (seen[0], seen[1]);
    assert!(h_small < h_large, "the sizes must give two heights: {h_small} and {h_large}");
    assert!(
        work_large <= work_small * h_large / h_small + 2,
        "work grew faster than the height: {work_small} at height {h_small}, {work_large} at height {h_large}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn a_commit_without_change_keeps_the_root() {
    let (_dir, store, files) = store_with(2_000);
    one_change(&store, &files);
    let second = store.current_root().unwrap();
    let third = store.commit().unwrap();
    assert_eq!(second, third, "a commit with no change keeps the root key");
    let stats = store.last_commit_stats();
    assert_eq!(manifest(&stats).apply.nodes_written, 0, "no tree node is written");
    assert_eq!(manifest(&stats).apply.nodes_read, 0);
    assert!(stats.nodes_written >= 1, "the root record counts");
}
