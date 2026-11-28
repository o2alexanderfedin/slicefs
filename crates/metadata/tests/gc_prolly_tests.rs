//! The manifest map as a prolly tree, end to end: set, commit, reload,
//! change and delete, commit, full GC (frees the nodes no root reaches,
//! keeps the others), reload (#215, plan 26-04).

use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use metadata::store::{tree_nodes, DictMetadataStore};
use metadata::store_io::{get_value, put_value, to_id, NodeReader, StoreIo};
use slicefs_traits::key::{ObjectKey, KEY_BYTES};
use slicefs_traits::metadata::{InodeMeta, MetadataStore};

/// Offset of the manifest tree root in the root record (pinned by the
/// `root_record_layout` unit test): root dir ino, next ino, inode and dir maps.
const MANIFEST_DATA: usize = 8 + 8 + 2 * KEY_BYTES;
/// Offset of the xattr tree root: after the manifest field.
const XATTR_DATA: usize = MANIFEST_DATA + KEY_BYTES;
/// Offset of the refcount tree root: after the manifest and xattr fields.
const REFCOUNTS: usize = MANIFEST_DATA + 2 * KEY_BYTES;
/// Offset of the inode tree root: after the root dir ino and next ino.
const INODE_DATA: usize = 8 + 8;
/// The offset of the `DIR_DATA` field of a root record.
const DIR_DATA: usize = INODE_DATA + KEY_BYTES;

fn manifest_tree(cs: &chunk_store::ChunkStore, root: &ObjectKey) -> chunk_store::ObjectId {
    let rec = get_value(cs, root).unwrap();
    chunk_store::ObjectId(rec[MANIFEST_DATA..MANIFEST_DATA + KEY_BYTES].try_into().unwrap())
}

/// The node IDs of a tree, with `prolly::visit`.
fn nodes_of(cs: &chunk_store::ChunkStore, tree: &chunk_store::ObjectId) -> BTreeSet<chunk_store::ObjectId> {
    let mut out = BTreeSet::new();
    assert!(prolly::visit(&prolly::Ctx::uncached(cs), tree, &mut |v| out.insert(v.id) || true).unwrap());
    out
}

fn full_gc(cs: &Arc<chunk_store::ChunkStore>, roots: &[ObjectKey]) {
    let boundary = cs.gc_begin_full().unwrap();
    let stop = AtomicBool::new(false);
    let live = metadata::gc::live_objects_exact(cs, roots, &[], &stop).unwrap().unwrap();
    cs.gc_finish_full(live, boundary).unwrap();
}

/// The manifest of file `i` at `version`: one real content object (the
/// GC mark reads every content key).
fn manifest(cs: &chunk_store::ChunkStore, i: u32, version: u32) -> Vec<ObjectKey> {
    vec![put_value(cs, format!("file {i} version {version}").as_bytes()).unwrap()]
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn manifest_tree_end_to_end() {
    let dir = tempfile::TempDir::new().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let cs = Arc::clone(io.lock().unwrap().store());
    let store = DictMetadataStore::new(Arc::clone(&io));
    let r = store.root_ino();
    let files: Vec<u64> = (0..3_000u32)
        .map(|i| {
            let ino = store.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
            store.link(r, &format!("f{i}"), ino).unwrap();
            store.set_manifest(ino, &manifest(&cs, i, 0)).unwrap();
            ino
        })
        .collect();
    let a = store.commit().unwrap();
    let tree_a = manifest_tree(&cs, &a);
    assert!(prolly::height(&prolly::Ctx::uncached(&cs), &tree_a).unwrap() >= 2, "a tree of more than one level");
    assert_eq!(prolly::count(&prolly::Ctx::uncached(&cs), &tree_a).unwrap(), files.len() as u64);

    let store = DictMetadataStore::load_from_root(Arc::clone(&io), &a).unwrap();
    for (i, f) in files.iter().enumerate() {
        assert_eq!(store.get_manifest(*f).unwrap(), manifest(&cs, i as u32, 0));
    }

    // Change 100 manifests, delete 50 files: root B.
    for (i, f) in files.iter().enumerate().take(100) {
        store.set_manifest(*f, &manifest(&cs, i as u32, 1)).unwrap();
    }
    for (i, f) in files.iter().enumerate().skip(1_000).take(50) {
        store.unlink(r, &format!("f{i}")).unwrap();
        store.delete_inode(*f).unwrap();
    }
    let b = store.commit().unwrap();
    let tree_b = manifest_tree(&cs, &b);
    let (nodes_a, nodes_b) = (nodes_of(&cs, &tree_a), nodes_of(&cs, &tree_b));
    let mut io_r = NodeReader(Arc::clone(&cs));
    let listed: BTreeSet<_> = tree_nodes(&mut io_r, &b).unwrap().iter().map(to_id).collect();
    // The refcount tree (plan 26-07), the xattr tree (26-15), the inode
    // tree (26-08) and the directory tree (26-09) are in the list too.
    let rec = get_value(&cs, &b).unwrap();
    let field = |o: usize| chunk_store::ObjectId(rec[o..o + KEY_BYTES].try_into().unwrap());
    let mut want: BTreeSet<_> = nodes_b.union(&nodes_of(&cs, &field(REFCOUNTS))).copied().collect();
    want.extend(nodes_of(&cs, &field(XATTR_DATA)));
    want.extend(nodes_of(&cs, &field(INODE_DATA)));
    want.extend(nodes_of(&cs, &field(DIR_DATA)));
    assert_eq!(listed, want, "tree_nodes lists the nodes of the trees");
    let old: Vec<_> = nodes_a.difference(&nodes_b).copied().collect();
    assert!(!old.is_empty(), "B rewrote nodes of A");

    full_gc(&cs, &[b]);
    assert!(
        old.iter().any(|id| matches!(cs.read(id), Err(chunk_store::Error::Missing(_)))),
        "a node only A reached is freed"
    );
    for id in &nodes_b {
        cs.read(id).unwrap_or_else(|e| panic!("node {id:?} of B: {e}"));
    }

    let store = DictMetadataStore::load_from_root(Arc::clone(&io), &b).unwrap();
    for (i, f) in files.iter().enumerate() {
        let got = store.get_manifest(*f);
        match i {
            0..100 => assert_eq!(got.unwrap(), manifest(&cs, i as u32, 1)),
            1_000..1_050 => assert!(got.is_err(), "file {i} is deleted"),
            _ => assert_eq!(got.unwrap(), manifest(&cs, i as u32, 0)),
        }
    }
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn an_empty_store_keeps_its_trees() {
    let dir = tempfile::TempDir::new().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let cs = Arc::clone(io.lock().unwrap().store());
    let store = DictMetadataStore::new(Arc::clone(&io));
    let root = store.commit().unwrap();
    let tree = manifest_tree(&cs, &root);
    assert_eq!(prolly::count(&prolly::Ctx::uncached(&cs), &tree).unwrap(), 0);
    full_gc(&cs, &[root]);
    cs.read(&tree).expect("the empty manifest tree is kept");
    cs.read(&to_id(&root)).expect("the root record is kept");
    let store = DictMetadataStore::load_from_root(Arc::clone(&io), &root).unwrap();
    assert!(store.get_manifest(1).is_err());
    store.commit().unwrap();
}

/// PMAP-06 ordering: with the online GC order (`gc_begin`, then a commit,
/// then the mark from the new root), nodes that a hot-path flush wrote after
/// `gc_begin` survive the sweep. (Integration tests have a flush limit of
/// 16,384 dirty entries, so the writes go past it.)
#[test]
#[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
fn nodes_of_a_flush_after_gc_begin_survive_the_sweep() {
    const FILES: u64 = 20_000;
    let dir = tempfile::TempDir::new().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let cs = Arc::clone(io.lock().unwrap().store());
    let store = DictMetadataStore::new(Arc::clone(&io));
    let inos: Vec<u64> = (0..FILES).map(|_| store.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap()).collect();
    // One real manifest (the mark reads its content key), shared by all files.
    let digest = store.prepare_manifest(&manifest(&cs, 0, 0)).unwrap();
    store.commit().unwrap();

    let boundary = cs.gc_begin_full().unwrap();
    let before = cs.stats().objects;
    for ino in &inos {
        store.set_manifest_digest(*ino, digest).unwrap();
    }
    assert!(cs.stats().objects > before, "a flush wrote tree nodes after gc_begin");
    let b = store.commit().unwrap();

    let stop = AtomicBool::new(false);
    let live = metadata::gc::live_objects_exact(&cs, &[b], &[], &stop).unwrap().unwrap();
    cs.gc_finish_full(live, boundary).unwrap();

    let tree_b = manifest_tree(&cs, &b);
    let nodes_b = nodes_of(&cs, &tree_b);
    assert!(nodes_b.len() > 100, "a tree of many nodes");
    for id in &nodes_b {
        cs.read(id).unwrap_or_else(|e| panic!("node {id:?} of the new root: {e}"));
    }
    let store = DictMetadataStore::load_from_root(Arc::clone(&io), &b).unwrap();
    for ino in &inos {
        assert_eq!(store.manifest_digest(*ino), Some(digest), "inode {ino}");
    }
}

/// A spilled xattr value is a metadata value of the xattr tree: a full GC
/// keeps it while an entry refers to it, and frees it after the entry is
/// replaced, or after its inode is deleted (T-26-20).
#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn a_spilled_xattr_value_lives_while_referenced() {
    let dir = tempfile::TempDir::new().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let cs = Arc::clone(io.lock().unwrap().store());
    let store = DictMetadataStore::new(Arc::clone(&io));
    let r = store.root_ino();
    let file = |name: &str| {
        let ino = store.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
        store.link(r, name, ino).unwrap();
        ino
    };
    let (a, b, c) = (file("a"), file("b"), file("c"));
    let big = |seed: u32| -> Vec<u8> { (0..200u32 << 10).map(|i| (i.wrapping_mul(seed).wrapping_add(i >> 7) >> 3) as u8).collect() };
    let (big_a, big_c) = (big(2_654_435_761), big(40_503));
    store.set_xattr(a, "user.big", &big_a).unwrap();
    store.set_xattr(b, "user.small", &[5u8; 50]).unwrap();
    store.set_xattr(c, "user.big", &big_c).unwrap();
    // The root object of a spilled value: put_value of the same bytes
    // (a spilled value is stored like file content) gives the same key.
    let key_a = put_value(&cs, &big_a).unwrap();
    let key_c = put_value(&cs, &big_c).unwrap();
    let root = store.commit().unwrap();
    full_gc(&cs, &[root]);
    assert_eq!(store.get_xattr(a, "user.big").unwrap(), big_a, "kept while referenced");
    assert_eq!(store.get_xattr(c, "user.big").unwrap(), big_c);
    assert_eq!(store.get_xattr(b, "user.small").unwrap(), [5u8; 50]);

    // Replaced by a short value: the old value is freed.
    store.set_xattr(a, "user.big", &[1u8; 50]).unwrap();
    // The inode of c is deleted: its value is freed.
    store.unlink(r, "c").unwrap();
    store.delete_inode(c).unwrap();
    let root = store.commit().unwrap();
    full_gc(&cs, &[root]);
    assert!(matches!(cs.read(&to_id(&key_a)), Err(chunk_store::Error::Missing(_))), "the replaced value is freed");
    assert!(matches!(cs.read(&to_id(&key_c)), Err(chunk_store::Error::Missing(_))), "the value of a deleted inode is freed");
    assert_eq!(store.get_xattr(a, "user.big").unwrap(), [1u8; 50]);
    assert_eq!(store.get_xattr(b, "user.small").unwrap(), [5u8; 50]);
    let store = DictMetadataStore::load_from_root(Arc::clone(&io), &root).unwrap();
    assert_eq!(store.get_xattr(a, "user.big").unwrap(), [1u8; 50]);
    assert!(store.list_xattrs(c).unwrap().is_empty());
}

// ─── Snapshots and the online GC on bulk stores (plan 26-11) ─────────────────

use metadata::testkit::{self, BulkSpec};

/// A bulk store of `files` files in 30 directories, loaded: the directory,
/// the chunk store, the spec and the store.
fn bulk_loaded(files: u64) -> (tempfile::TempDir, Arc<chunk_store::ChunkStore>, BulkSpec, DictMetadataStore) {
    let dir = tempfile::TempDir::new().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let spec = BulkSpec { files, dirs: 30, xattr_every: 10, refcounts_per_file: 1 };
    let root = testkit::bulk_store(&io, &spec).unwrap();
    let cs = Arc::clone(io.lock().unwrap().store());
    let store = DictMetadataStore::load_from_root(io, &root).unwrap();
    (dir, cs, spec, store)
}

/// The tree nodes of the root record `root`, each once (the walk of the mark).
fn node_set(cs: &Arc<chunk_store::ChunkStore>, root: &ObjectKey) -> BTreeSet<ObjectKey> {
    tree_nodes(&mut NodeReader(Arc::clone(cs)), root).unwrap().into_iter().collect()
}

fn is_missing(cs: &chunk_store::ChunkStore, k: &ObjectKey) -> bool {
    matches!(cs.read(&to_id(k)), Err(chunk_store::Error::Missing(_)))
}

/// Change the manifests of files `0..100` (version `v`) and delete the 50
/// files from 1,000 of the bulk store.
fn change_and_delete(cs: &chunk_store::ChunkStore, spec: &BulkSpec, store: &DictMetadataStore, v: u32) {
    for i in 0..100 {
        store.set_manifest(spec.file_ino(i), &manifest(cs, i as u32, v)).unwrap();
    }
    for i in 1_000..1_050 {
        store.unlink(spec.parent_of(i), &testkit::file_name(i)).unwrap();
        store.delete_inode(spec.file_ino(i)).unwrap();
    }
}

/// PMAP-06, T-26-32: a GC with the live root and a snapshot root keeps the
/// nodes of both (also the nodes they share, which the live root alone
/// still reaches, and the nodes only the snapshot reaches); a GC with the
/// live root only frees the snapshot-only nodes and keeps every live one.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn a_node_shared_with_a_snapshot_survives_gc() {
    let (dir, cs, spec, store) = bulk_loaded(3_000);
    let snap = store.create_snapshot(Some("before".into())).unwrap();
    change_and_delete(&cs, &spec, &store, 1);
    let live = store.commit().unwrap();
    let (live_nodes, snap_nodes) = (node_set(&cs, &live), node_set(&cs, &snap.root));
    let shared: Vec<_> = live_nodes.intersection(&snap_nodes).copied().collect();
    let snap_only: Vec<_> = snap_nodes.difference(&live_nodes).copied().collect();
    assert!(!shared.is_empty() && !snap_only.is_empty(), "{} shared, {} snapshot-only", shared.len(), snap_only.len());

    full_gc(&cs, &[live, snap.root]);
    for k in live_nodes.iter().chain(&snap_nodes) {
        cs.read(&to_id(k)).unwrap_or_else(|e| panic!("node {k:?}: {e}"));
    }
    let old = DictMetadataStore::load_from_root(Arc::new(Mutex::new(StoreIo::new(dir.path(), Arc::clone(&cs)))), &snap.root).unwrap();
    let content = put_value(&cs, &testkit::content(1)).unwrap();
    assert_eq!(old.get_manifest(spec.file_ino(1_010)).unwrap(), [content], "a file deleted after the snapshot");

    full_gc(&cs, &[live]);
    assert!(snap_only.iter().any(|k| is_missing(&cs, k)), "a snapshot-only node is freed");
    for k in &live_nodes {
        cs.read(&to_id(k)).unwrap_or_else(|e| panic!("live node {k:?}: {e}"));
    }
    for i in 0..100 {
        assert_eq!(store.get_manifest(spec.file_ino(i)).unwrap(), manifest(&cs, i as u32, 1));
    }
}

/// The online GC order on the trees (`gc_begin_full`, a commit, the mark
/// of `OnlineGc` from its root, `gc_finish_full`): the nodes of an older
/// root that no root reaches are freed; the live tree reads.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn online_gc_frees_unreached_tree_nodes() {
    let (_dir, cs, spec, store) = bulk_loaded(3_000);
    let first = store.current_root().unwrap();
    change_and_delete(&cs, &spec, &store, 1);
    let second = store.commit().unwrap();
    let old: BTreeSet<_> = node_set(&cs, &first).union(&node_set(&cs, &second)).copied().collect();

    let boundary = cs.gc_begin_full().unwrap();
    store.set_manifest(spec.file_ino(2_000), &manifest(&cs, 2_000, 2)).unwrap();
    let live = store.commit().unwrap();
    let gc = metadata::gc::OnlineGc::new(Arc::clone(&cs), vec![live], vec![]);
    let marked = gc.live_exact(&AtomicBool::new(false)).unwrap().unwrap();
    let st = cs.gc_finish_full(marked, boundary).unwrap();
    assert!(st.removed_objects > 0, "{st:?}");

    let live_nodes = node_set(&cs, &live);
    let unreached: Vec<_> = old.difference(&live_nodes).collect();
    assert!(!unreached.is_empty());
    assert!(unreached.iter().any(|k| is_missing(&cs, k)), "a node of an older root is freed");
    for k in &live_nodes {
        cs.read(&to_id(k)).unwrap_or_else(|e| panic!("live node {k:?}: {e}"));
    }
    assert_eq!(store.get_manifest(spec.file_ino(2_000)).unwrap(), manifest(&cs, 2_000, 2));
    assert_eq!(store.get_manifest(spec.file_ino(5)).unwrap(), manifest(&cs, 5, 1));
}

/// The accepted cost of snapshots (26-CONTEXT.md, Scope): the mark walks
/// every root in full, so 1 root and 3 snapshots that share almost all
/// nodes cost about 4 walks, not one walk and the few changed nodes.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn snapshot_roots_cost_one_walk_each() {
    let (_dir, cs, spec, store) = bulk_loaded(3_000);
    let mut roots = Vec::new();
    for s in 0..3u32 {
        roots.push(store.create_snapshot(Some(format!("s{s}"))).unwrap().root);
        store.set_manifest(spec.file_ino(u64::from(s) * 1_000), &manifest(&cs, s, 3)).unwrap();
    }
    let live = store.commit().unwrap();
    roots.insert(0, live);
    let walk = |r: &ObjectKey| tree_nodes(&mut NodeReader(Arc::clone(&cs)), r).unwrap().len();
    let one = walk(&live);
    let four: usize = roots.iter().map(walk).sum();
    let distinct = roots.iter().fold(BTreeSet::new(), |mut a, r| {
        a.extend(node_set(&cs, r));
        a
    });
    println!("GC mark walk of 3,000 files: 1 root {one} nodes; 1 root + 3 snapshots {four} nodes ({} distinct)", distinct.len());
    assert!(four <= 4 * one + 5, "{four} nodes for 4 roots, {one} for 1");
    assert!(four >= 4 * one - 5, "each root is walked in full: {four} nodes for 4 roots, {one} for 1");
    assert!(distinct.len() < 2 * one, "the roots share most nodes");
}
