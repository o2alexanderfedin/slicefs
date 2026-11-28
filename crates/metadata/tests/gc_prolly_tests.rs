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
    assert_eq!(listed, nodes_b, "tree_nodes lists the nodes of the tree");
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
