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
        meta.set_manifest(ino, &[slicefs_traits::key::ObjectKey::example(i)]).unwrap();
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
        assert_eq!(meta.get_manifest(*ino).unwrap(), vec![slicefs_traits::key::ObjectKey::example(i as u32)]);
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
        meta.set_manifest(ino, &[slicefs_traits::key::ObjectKey::example(i)]).unwrap();
        last = ino;
    }
    meta.commit().unwrap();
    let full = cs.stats().stored_bytes;
    meta.set_manifest(last, &[slicefs_traits::key::ObjectKey::example(9)]).unwrap();
    meta.commit().unwrap();
    let grew = cs.stats().stored_bytes - full;
    eprintln!("first commit {full} bytes, next commit +{grew} bytes");
    assert!(grew * 10 < full, "first commit {full} bytes, next commit +{grew} bytes");
}

/// #65: a node that cannot be read says why. An I/O error stays an I/O
/// error (MetaError::Io); a node that is not in the store is corrupt
/// metadata, and the message carries the chunk-store error.
#[test]
fn a_node_read_error_keeps_its_kind() {
    use slicefs_traits::metadata::MetaError;
    let mem = MemIo::new();
    let meta = DictMetadataStore::new(open(&mem));
    let ino = meta.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
    meta.link(1, "f", ino).unwrap();
    let root = meta.commit().unwrap();
    drop(meta);

    mem.set_fail_reads(true);
    let e = DictMetadataStore::load_from_root(open_after_reads(&mem), &root).err().expect("reads fail");
    assert!(matches!(e, MetaError::Io(_)), "an I/O error, got {e:?}");
    mem.set_fail_reads(false);

    use chunk_store::ObjectHasher;
    let absent = metadata::store_io::to_key(chunk_store::Blake3_256.id(b"no node has these bytes"));
    let e = DictMetadataStore::load_from_root(open(&mem), &absent).err().expect("no such root");
    match e {
        MetaError::Corrupted(m) => assert!(m.contains("root node") && m.contains("not in the store"), "{m}"),
        e => panic!("corrupt metadata expected, got {e:?}"),
    }
}

/// Open the store first, then let reads fail (the open itself reads).
fn open_after_reads(mem: &MemIo) -> Arc<Mutex<StoreIo>> {
    mem.set_fail_reads(false);
    let io = open(mem);
    mem.set_fail_reads(true);
    io
}
