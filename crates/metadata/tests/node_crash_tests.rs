//! The metadata nodes are in the chunk store: a commit makes them durable
//! before it returns the root (and before the root is logged).

use std::sync::{Arc, Mutex};

use chunk_store::mem::MemIo;
use chunk_store::{ChunkStore, Options};
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_traits::metadata::{InodeMeta, MetadataStore};

fn open(mem: &MemIo) -> Arc<Mutex<StoreIo>> {
    let cs = ChunkStore::open_with(Arc::new(mem.clone()), Options::default()).unwrap();
    Arc::new(Mutex::new(StoreIo::new("/unused", Arc::new(cs))))
}

#[test]
fn a_committed_root_loads_after_a_crash() {
    let mem = MemIo::new();
    let meta = DictMetadataStore::new(open(&mem));
    let mut inos = Vec::new();
    for i in 0..20 {
        let ino = meta.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
        meta.link(1, &format!("f{i}"), ino).unwrap();
        meta.set_manifest(ino, &[[i, 2, 3, 4, 5, 6, 7, 8]]).unwrap();
        inos.push(ino);
    }
    let root = meta.commit().unwrap();
    // More changes, not committed: their nodes may be lost.
    let late = meta.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
    meta.link(1, "late", late).unwrap();
    drop(meta);

    mem.crash();
    let meta = DictMetadataStore::load_from_root(open(&mem), &root).unwrap();
    for (i, ino) in inos.iter().enumerate() {
        assert_eq!(meta.lookup(1, &format!("f{i}")).unwrap(), *ino);
        assert_eq!(meta.get_manifest(*ino).unwrap(), vec![[i as u32, 2, 3, 4, 5, 6, 7, 8]]);
    }
    assert!(meta.lookup(1, "late").is_err(), "not in the committed root");
}

#[test]
fn a_failed_node_sync_fails_the_commit() {
    let mem = MemIo::new();
    let meta = DictMetadataStore::new(open(&mem));
    let ino = meta.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
    meta.link(1, "f", ino).unwrap();
    mem.set_fail_syncs(true);
    assert!(meta.commit().is_err(), "a root over nodes that are not durable must not be returned");
}

/// The maps are stored whole at each commit, but in FastCDC chunks: a
/// commit after a small change stores only a few chunks again, not the
/// whole maps.
#[test]
fn a_small_change_stores_little_at_the_next_commit() {
    let mem = MemIo::new();
    let io = open(&mem);
    let cs = Arc::clone(io.lock().unwrap().store());
    let meta = DictMetadataStore::new(io);
    let mut last = 0;
    for i in 0..5000u32 {
        let ino = meta.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
        meta.link(1, &format!("f{i}"), ino).unwrap();
        meta.set_manifest(ino, &[[i, 2, 3, 4, 5, 6, 7, 8]]).unwrap();
        last = ino;
    }
    meta.commit().unwrap();
    let full = cs.stats().stored_bytes;
    meta.set_manifest(last, &[[9, 9, 9, 9, 9, 9, 9, 9]]).unwrap();
    meta.commit().unwrap();
    let grew = cs.stats().stored_bytes - full;
    eprintln!("first commit {full} bytes, next commit +{grew} bytes");
    assert!(grew * 10 < full, "first commit {full} bytes, next commit +{grew} bytes");
}
