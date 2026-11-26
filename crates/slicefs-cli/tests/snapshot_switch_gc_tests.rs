//! A switch segment supersedes snapshot roots (the switch of `slicefs
//! rehash`), and a GC on the open store frees what only the old roots used.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use metadata::segment::{load_store_from_segments, scan_store_with_log, write_switch_segment};
use metadata::snapshot::SnapshotEntry;
use metadata::store::{content_keys, DictMetadataStore};
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::content;
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_traits::metadata::MetadataStore;
use tempfile::TempDir;

fn next_segment(store: &Path) -> u64 {
    std::fs::read_dir(store.join("segments"))
        .unwrap()
        .filter_map(|e| {
            let n = e.ok()?.file_name().to_string_lossy().to_string();
            n.strip_prefix("segment-")?.strip_suffix(".seg")?.parse::<u64>().ok()
        })
        .max()
        .unwrap_or(0)
        + 1
}

/// Open the store as a mount does: the metadata from the last root, with
/// the snapshots.
fn open_fs(store: &Path) -> SliceFsFilesystem {
    let segs = store.join("segments");
    let io = Arc::new(Mutex::new(content::store_io(store).unwrap()));
    let (root, snapshots) = load_store_from_segments(&segs).unwrap();
    let mut meta = DictMetadataStore::load_from_root(io.clone(), &root.unwrap()).unwrap();
    meta.set_snapshots(snapshots);
    meta.set_wal(create_wal(WalConfig::PerOp, store, next_segment(store)).unwrap());
    SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()))
}

fn close_fs(fs: SliceFsFilesystem) {
    fs.meta().commit().unwrap();
    fs.meta().shutdown_wal().unwrap();
    fs.content().close().unwrap();
}

fn data(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

fn read_file(fs: &SliceFsFilesystem, parent: u64, name: &str) -> Vec<u8> {
    let (ino, attr) = fs.test_lookup(parent, name).unwrap();
    fs.test_read(ino, 0, attr.size as u32).unwrap()
}

#[test]
fn a_superseded_snapshot_root_is_no_longer_kept_by_gc() {
    let src = TempDir::new().unwrap();
    std::fs::write(src.path().join("a"), data(1, 200_000)).unwrap();
    let dir = TempDir::new().unwrap();
    let store = dir.path();
    let segs = store.join("segments");
    slicefs_cli::seed::run_seed(store, src.path(), None).unwrap();
    slicefs_cli::snapshot::run_snapshot_create(store, Some("s1".into())).unwrap();
    let (r1, snaps) = load_store_from_segments(&segs).unwrap();
    let r1 = r1.unwrap();
    assert_eq!(snaps.len(), 1);
    let v1 = snaps[0].clone();
    assert_eq!((v1.version, v1.root), (1, r1));

    // A new content of `a`: a new root, the snapshot keeps the old one.
    let fs = open_fs(store);
    let (ino, _) = fs.test_lookup(1, "a").unwrap();
    fs.test_setattr_size(ino, None, 0).unwrap();
    let (fh, _) = fs.test_open(ino, libc::O_WRONLY).unwrap();
    fs.test_write(fh, 0, &data(2, 200_000)).unwrap();
    fs.test_release(ino, fh).unwrap();
    close_fs(fs);
    let (r2, snaps) = load_store_from_segments(&segs).unwrap();
    let r2 = r2.unwrap();
    assert_ne!(r1, r2);
    assert_eq!(snaps, vec![v1.clone()], "before the switch the snapshot keeps the old root");

    // The objects that only the old root uses.
    let mut io = content::store_io(store).unwrap();
    let new_keys: HashSet<_> = content_keys(&mut io, &r2).unwrap().into_iter().collect();
    let mut old_ids = Vec::new();
    for k in content_keys(&mut io, &r1).unwrap() {
        if !new_keys.contains(&k) {
            old_ids.extend(chunk_store::file::file_objects(io.store(), content::to_id(&k)).unwrap());
        }
    }
    assert!(!old_ids.is_empty());
    io.store().close().unwrap();
    drop(io);

    // The switch: the snapshot now points at the new root.
    let new_v1 = SnapshotEntry { root: r2, ..v1.clone() };
    write_switch_segment(&segs, r2, std::slice::from_ref(&new_v1)).unwrap();
    let (root, snaps) = load_store_from_segments(&segs).unwrap();
    assert_eq!((root, snaps), (Some(r2), vec![new_v1.clone()]));
    let (root, snaps, log) = scan_store_with_log(&segs).unwrap();
    assert_eq!((root, snaps), (Some(r2), vec![new_v1.clone()]));
    assert!(log.is_empty());
    assert_eq!(new_v1.name.as_deref(), Some("s1"));

    // A GC on the open store with the loaded roots frees the old objects.
    let (root, snaps) = load_store_from_segments(&segs).unwrap();
    let roots: Vec<_> = root.into_iter().chain(snaps.iter().map(|s| s.root)).collect();
    assert_eq!(roots, [r2, r2]);
    let cs = content::open(store).unwrap();
    assert!(old_ids.iter().all(|id| cs.contains(id)), "the old objects are there before the GC");
    let stats = slicefs_cli::gc::gc_on_open_store(&cs, &roots, true).unwrap();
    assert!(stats.removed_objects as usize >= old_ids.len(), "{stats:?}");
    assert_eq!(stats.dead_left, Some(0), "{stats:?}");
    for id in &old_ids {
        assert!(!cs.contains(id), "old object {id:?} is freed");
    }
    cs.close().unwrap();
    drop(cs);

    let fs = open_fs(store);
    assert_eq!(read_file(&fs, 1, "a"), data(2, 200_000));
    close_fs(fs);
    slicefs_cli::scrub::run_scrub(store, false).unwrap();
}
