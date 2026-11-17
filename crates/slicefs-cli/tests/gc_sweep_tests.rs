//! Offline GC frees the stored objects of deleted files (#25), and keeps the
//! objects of live files and of snapshots.

use std::path::Path;
use std::sync::{Arc, Mutex};

use metadata::segment::load_store_from_segments;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::gc::run_gc;
use slicefs_traits::key::ObjectKey;
use slicefs_traits::metadata::MetadataStore;
use tempfile::TempDir;

fn make_fs(store: &Path) -> SliceFsFilesystem {
    std::fs::create_dir_all(store.join("segments")).unwrap();
    let wal = create_wal(WalConfig::PerOp, store, 1).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let mut meta = DictMetadataStore::new(io.clone());
    meta.set_wal(wal);
    SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()))
}

/// Stored objects: metadata objects in vt0/ and file content in the chunk store.
fn objects(store: &Path) -> usize {
    let vt0 = std::fs::read_dir(store.join("vt0")).map(|d| d.count()).unwrap_or(0);
    vt0 + slicefs_cli::content::open_read_only(store).unwrap().stats().objects as usize
}

fn data(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

fn write_file(fs: &SliceFsFilesystem, name: &str, bytes: &[u8]) {
    let (ino, fh) = fs.test_create(1, name, 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, bytes).unwrap();
    fs.test_release(ino, fh).unwrap();
}

/// Read `name` from the metadata at `root`, straight from the store objects.
fn read_at_root(store: &Path, root: &ObjectKey, name: &str) -> Vec<u8> {
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let meta = DictMetadataStore::load_from_root(io, root).unwrap();
    let ino = meta.lookup(1, name).unwrap();
    let manifest = meta.get_manifest(ino).unwrap();
    let cs = slicefs_cli::content::open_read_only(store).unwrap();
    slicefs_cli::content::get_all(&cs, &manifest[0]).unwrap()
}

#[test]
fn gc_frees_deleted_files_and_keeps_live_and_snapshot_data() {
    let dir = TempDir::new().unwrap();
    let store = dir.path();
    let (keep, snap, del) = (data(1, 300_000), data(2, 300_000), data(3, 300_000));
    let snap_root = {
        let fs = make_fs(store);
        write_file(&fs, "keep.bin", &keep);
        write_file(&fs, "snap.bin", &snap);
        fs.meta().commit().unwrap();
        let s = fs.meta().create_snapshot(Some("s1".into())).unwrap();
        // del.bin is committed after the snapshot, then deleted: no kept root has it.
        write_file(&fs, "del.bin", &del);
        fs.meta().commit().unwrap();
        fs.simulate_unlink(1, "snap.bin").unwrap();
        fs.simulate_unlink(1, "del.bin").unwrap();
        fs.meta().commit().unwrap();
        fs.meta().shutdown_wal().unwrap();
        s.root
    };
    let before = objects(store);

    run_gc(store).unwrap();

    let after = objects(store);
    // del.bin is in no root: its ~40 objects go. snap.bin is in the snapshot.
    assert!(after + 30 < before, "gc must free del.bin objects: before {before}, after {after}");
    let (root, _) = load_store_from_segments(&store.join("segments")).unwrap();
    assert_eq!(read_at_root(store, &root.unwrap(), "keep.bin"), keep);
    assert_eq!(read_at_root(store, &snap_root, "snap.bin"), snap);
    assert_eq!(read_at_root(store, &snap_root, "keep.bin"), keep);

    // A second run finds nothing more to free.
    run_gc(store).unwrap();
    assert_eq!(objects(store), after);
}

#[test]
fn gc_after_deleting_every_file_leaves_an_almost_empty_store() {
    let empty = TempDir::new().unwrap();
    {
        let fs = make_fs(empty.path());
        fs.meta().commit().unwrap();
        fs.meta().shutdown_wal().unwrap();
    }
    let empty_objects = objects(empty.path());

    let dir = TempDir::new().unwrap();
    let store = dir.path();
    {
        let fs = make_fs(store);
        for i in 0..5 {
            write_file(&fs, &format!("f{i}"), &data(10 + i, 200_000));
        }
        fs.meta().commit().unwrap();
        for i in 0..5 {
            fs.simulate_unlink(1, &format!("f{i}")).unwrap();
        }
        fs.meta().commit().unwrap();
        fs.meta().shutdown_wal().unwrap();
    }
    assert!(objects(store) > empty_objects + 100);

    run_gc(store).unwrap();

    assert!(
        objects(store) <= empty_objects + 2,
        "after gc: {} objects, empty store: {empty_objects}",
        objects(store)
    );
    let (root, _) = load_store_from_segments(&store.join("segments")).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let meta = DictMetadataStore::load_from_root(io, &root.unwrap()).unwrap();
    assert!(meta.lookup(1, "f0").is_err());
}
