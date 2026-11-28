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

// ─── Bulk stores (plan 26-11): all five trees at two sizes ───────────────────

use metadata::store_io::{get_value, put_value};
use metadata::testkit::{self, BulkSpec};

/// Offsets of the five tree roots in the root record (pinned by the
/// `root_record_layout` unit test).
const TREE_FIELDS: [usize; 5] = [16, 16 + 33, 16 + 2 * 33, 16 + 3 * 33, 16 + 4 * 33];

/// The spec of a bulk store of `files` files: 100 files per directory, an
/// xattr on one file in ten, one refcount entry per file.
fn spec(files: u64) -> BulkSpec {
    BulkSpec { files, dirs: (files / 100).max(1), xattr_every: 10, refcounts_per_file: 1 }
}

/// A bulk store of `spec` in a new directory: the directory, its node
/// store and the root record key.
fn bulk(spec: &BulkSpec) -> (tempfile::TempDir, Arc<Mutex<StoreIo>>, ObjectKey) {
    let dir = tempfile::TempDir::new().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let started = std::time::Instant::now();
    let root = testkit::bulk_store(&io, spec).unwrap();
    println!("bulk store of {} files: {:.1} s", spec.files, started.elapsed().as_secs_f64());
    (dir, io, root)
}

/// The heights of the five trees of the root record `root`.
fn heights(cs: &chunk_store::ChunkStore, root: &ObjectKey) -> Vec<u8> {
    let rec = get_value(cs, root).unwrap();
    let ctx = prolly::Ctx::uncached(cs);
    TREE_FIELDS
        .iter()
        .map(|&o| prolly::height(&ctx, &chunk_store::ObjectId(rec[o..o + 33].try_into().unwrap())).unwrap())
        .collect()
}

/// A bulk store loads, reads back through the product API, and has the
/// same root record as the same files made with the product calls (the
/// loader writes the store format, T-26-34).
#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn a_bulk_store_is_the_store_of_the_product_calls() {
    let spec = BulkSpec { files: 2_000, dirs: 20, xattr_every: 7, refcounts_per_file: 2 };
    let (_dir, io, root) = bulk(&spec);
    let cs = Arc::clone(io.lock().unwrap().store());
    let store = DictMetadataStore::load_from_root(Arc::clone(&io), &root).unwrap();
    assert_eq!(store.inode_count(), spec.inode_count());
    let tag = put_value(&cs, &testkit::content(0)).unwrap().tag();
    let names: Vec<String> = store.list_directory(1).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names.len() as u64, 2 + spec.dirs);
    let mut seen = 0u64;
    for j in 0..spec.dirs {
        for e in store.list_directory(spec.dir_ino(j)).unwrap() {
            if e.name == "." || e.name == ".." {
                continue;
            }
            let i = e.ino - spec.file_ino(0);
            assert_eq!((e.name.as_str(), spec.parent_of(i)), (testkit::file_name(i).as_str(), spec.dir_ino(j)));
            assert_eq!(store.get_inode(e.ino).unwrap(), spec.inode_meta(e.ino));
            let content = put_value(&cs, &testkit::content(testkit::group_of(i))).unwrap();
            assert_eq!(store.get_manifest(e.ino).unwrap(), [content]);
            match spec.has_xattr(i) {
                true => assert_eq!(store.get_xattr(e.ino, testkit::XATTR_NAME).unwrap(), testkit::xattr_value(i)),
                false => assert!(store.get_xattr(e.ino, testkit::XATTR_NAME).is_err()),
            }
            assert_eq!(store.get_refcount(&testkit::refcount_key(tag, i * 2 + 1)), 1);
            seen += 1;
        }
    }
    assert_eq!(seen, spec.files, "every file is in a directory");

    // The same files through the product calls: the same root record.
    let dir2 = tempfile::TempDir::new().unwrap();
    let io2 = Arc::new(Mutex::new(StoreIo::open(dir2.path()).unwrap()));
    let cs2 = Arc::clone(io2.lock().unwrap().store());
    let api = DictMetadataStore::new(Arc::clone(&io2));
    for j in 0..spec.dirs {
        let ino = api.create_directory(1, &testkit::dir_name(j), &spec.inode_meta(spec.dir_ino(j))).unwrap();
        assert_eq!(ino, spec.dir_ino(j));
    }
    for i in 0..spec.files {
        let ino = api.create_inode(&spec.inode_meta(spec.file_ino(i))).unwrap();
        assert_eq!(ino, spec.file_ino(i));
        api.link(spec.parent_of(i), &testkit::file_name(i), ino).unwrap();
        api.set_manifest(ino, &[put_value(&cs2, &testkit::content(testkit::group_of(i))).unwrap()]).unwrap();
        if spec.has_xattr(i) {
            api.set_xattr(ino, testkit::XATTR_NAME, &testkit::xattr_value(i)).unwrap();
        }
        for r in 0..2 {
            api.increment_refcount(&testkit::refcount_key(tag, i * 2 + r));
        }
    }
    for ino in 1..spec.next_ino() {
        api.update_inode(&spec.inode_meta(ino)).unwrap();
    }
    let api_root = api.commit().unwrap();
    assert_eq!(get_value(&cs2, &api_root).unwrap(), get_value(&cs, &root).unwrap(), "the same root record");
}

/// Bulk stores of `sizes` files: one change in every tree (create, link,
/// set_manifest, set_xattr, increment_refcount), then a commit. Each tree
/// writes at most 2h + 1 and reads at most 3h nodes; at least two trees are
/// higher in the larger store, and the total grows at most with the heights
/// (plus 2 nodes per tree, as in `one_change_manifest_commit_grows_with_height`:
/// the reads of a tree are about 3(h - 1), not a fixed share of h).
fn one_change_in_every_tree(sizes: [u64; 2]) {
    let mut seen = Vec::new();
    for n in sizes {
        let spec = spec(n);
        let (_dir, io, root) = bulk(&spec);
        let cs = Arc::clone(io.lock().unwrap().store());
        let store = DictMetadataStore::load_from_root(Arc::clone(&io), &root).unwrap();
        let ino = store.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
        store.link(spec.dir_ino(spec.dirs / 2), "new file", ino).unwrap();
        let content = put_value(&cs, b"new content").unwrap();
        store.set_manifest(ino, &[content]).unwrap();
        store.set_xattr(ino, "user.new", b"new value").unwrap();
        store.increment_refcount(&content);
        store.commit().unwrap();
        let stats = store.last_commit_stats();
        assert_eq!(stats.trees.len(), 5);
        let mut hs = Vec::new();
        for t in &stats.trees {
            let h = u64::from(t.height);
            let a = &t.apply;
            println!("n={n} {}: height {h}, entries {}, read {}, written {}", t.name, t.entries, a.nodes_read, a.nodes_written);
            assert!(a.nodes_written >= 1, "n={n} {}: the change writes a node", t.name);
            assert!(a.nodes_written <= 2 * h + 1, "n={n} {}: wrote {} nodes, height {h}", t.name, a.nodes_written);
            assert!(a.nodes_read <= 3 * h, "n={n} {}: read {} nodes, height {h}", t.name, a.nodes_read);
            hs.push(h);
        }
        let work: u64 = stats.trees.iter().map(|t| t.apply.nodes_read + t.apply.nodes_written).sum();
        seen.push((hs, work));
    }
    let ((h_small, work_small), (h_large, work_large)) = (&seen[0], &seen[1]);
    let higher = h_small.iter().zip(h_large).filter(|(s, l)| l > s).count();
    assert!(higher >= 2, "the sizes do not test growth: heights {h_small:?} and {h_large:?}");
    let (sum_small, sum_large) = (h_small.iter().sum::<u64>(), h_large.iter().sum::<u64>());
    println!("total work: {work_small} at heights {h_small:?}, {work_large} at heights {h_large:?}");
    assert!(
        *work_large <= work_small * sum_large / sum_small + 2 * 5,
        "work grew faster than the heights: {work_small} at {h_small:?}, {work_large} at {h_large:?}"
    );
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn one_change_in_every_tree_grows_with_height() {
    one_change_in_every_tree([10_000, 300_000]);
}

/// The same check at 1,000,000 files (by hand: `cargo test --release -p
/// metadata --test commit_counter_tests -- --ignored`).
#[test]
#[ignore = "large: run by hand in a release build"]
fn one_change_at_one_million_files() {
    one_change_in_every_tree([10_000, 1_000_000]);
}

/// PMAP-03: a load reads a few nodes, not the maps. On a new node cache, the
/// load of a 300,000-file store misses at most 5 x (h + 1) nodes (h the
/// highest tree). The five root checks of the load read the chunk store
/// directly, not through the cache; they are added to the count.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn loading_a_large_store_reads_a_few_nodes() {
    let (dir, io, root) = bulk(&spec(300_000));
    let cs = Arc::clone(io.lock().unwrap().store());
    let h = u64::from(*heights(&cs, &root).iter().max().unwrap());
    let fresh = Arc::new(Mutex::new(StoreIo::new(dir.path(), Arc::clone(&cs))));
    let store = DictMetadataStore::load_from_root(Arc::clone(&fresh), &root).unwrap();
    let st = fresh.lock().unwrap().node_cache().stats();
    println!("load of 300,000 files: {} cache misses, {} hits, highest tree {h}", st.misses, st.hits);
    assert!(st.misses + 5 <= 5 * (h + 1), "{} misses, height {h}", st.misses);
    assert_eq!(store.inode_count(), spec(300_000).inode_count());
}
