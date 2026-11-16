//! Online GC on a mounted store (#25): each test acts between the GC steps
//! (start, mark, sweep) and reads the whole file back afterwards. A write
//! that only succeeds proves nothing: blockset treats a missing node as
//! inline data without an error.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use blockset::file_storage_get;
use metadata::segment::load_store_from_segments;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::gc::run_gc;
use slicefs_traits::metadata::MetadataStore;
use tempfile::TempDir;

fn make_fs(store: &Path) -> SliceFsFilesystem {
    std::fs::create_dir_all(store.join("segments")).unwrap();
    let wal = create_wal(WalConfig::PerOp, store, 1).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(store)));
    let mut meta = DictMetadataStore::new(io.clone());
    meta.set_wal(wal);
    SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()))
}

fn data(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

fn write_file(fs: &SliceFsFilesystem, name: &str, bytes: &[u8]) -> u64 {
    let (ino, fh) = fs.test_create(1, name, 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, bytes).unwrap();
    fs.test_release(ino, fh).unwrap();
    ino
}

/// A file that is deleted and committed: its objects are garbage.
fn make_garbage(fs: &SliceFsFilesystem, name: &str, bytes: &[u8]) {
    write_file(fs, name, bytes);
    fs.meta().commit().unwrap();
    fs.simulate_unlink(1, name).unwrap();
    fs.meta().commit().unwrap();
}

/// Unmount, then check from disk: `name` reads back as `want`, and an offline
/// GC finds every object of the store (its mark fails on a missing object).
fn check_after_remount(fs: SliceFsFilesystem, store: &Path, files: &[(&str, &[u8])]) {
    fs.meta().commit().unwrap();
    fs.meta().shutdown_wal().unwrap();
    drop(fs);
    run_gc(store).expect("offline gc: every object must be reachable");
    let (root, _) = load_store_from_segments(&store.join("segments")).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(store)));
    let meta = DictMetadataStore::load_from_root(io, &root.unwrap()).unwrap();
    for (name, want) in files {
        let ino = meta.lookup(1, name).unwrap();
        let manifest = meta.get_manifest(ino).unwrap();
        let got = file_storage_get(&mut StoreIo::new(store), &manifest[0]).unwrap();
        assert!(got == *want, "{name}: {} bytes back, want {}", got.len(), want.len());
    }
}

fn no_stop() -> AtomicBool {
    AtomicBool::new(false)
}

#[test]
fn online_gc_frees_garbage_and_keeps_live_files() {
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    let keep = data(1, 300_000);
    write_file(&fs, "keep.bin", &keep);
    make_garbage(&fs, "del.bin", &data(2, 300_000));

    let stats = fs.gc_online(&no_stop()).unwrap().expect("not stopped");
    assert!(stats.objects_removed > 50, "{stats:?}");
    let ino = fs.test_lookup(1, "keep.bin").unwrap().0;
    assert_eq!(fs.test_read(ino, 0, keep.len() as u32 + 1).unwrap(), keep);
    check_after_remount(fs, dir.path(), &[("keep.bin", &keep)]);
}

#[test]
fn a_write_that_dedups_against_garbage_during_gc_keeps_its_objects() {
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    let same = data(3, 300_000);
    make_garbage(&fs, "old.bin", &same);

    let gc = fs.gc_start().unwrap();
    let live = gc.mark(&no_stop()).unwrap().unwrap();
    // The same bytes again: blockset finds the existing (garbage) objects
    // and does not write them; only the touch protects them.
    write_file(&fs, "new.bin", &same);
    let stats = gc.sweep(&live, &no_stop()).unwrap();
    drop(gc);

    let ino = fs.test_lookup(1, "new.bin").unwrap().0;
    assert_eq!(fs.test_read(ino, 0, same.len() as u32 + 1).unwrap(), same);
    assert!(stats.objects_kept > 0, "{stats:?}");
    check_after_remount(fs, dir.path(), &[("new.bin", &same)]);
}

#[test]
fn an_open_streaming_handle_keeps_its_partial_tree() {
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    make_garbage(&fs, "junk.bin", &data(4, 300_000));
    let content = data(5, 1_000_000);
    let (ino, fh) = fs.test_create(1, "big.bin", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, &content[..300_000]).unwrap();
    fs.test_write(fh, 300_000, &content[300_000..600_000]).unwrap();
    assert!(fs.test_is_streaming(fh));

    let stats = fs.gc_online(&no_stop()).unwrap().unwrap();
    assert!(stats.objects_removed > 50, "garbage must go: {stats:?}");

    fs.test_write(fh, 600_000, &content[600_000..]).unwrap();
    fs.test_release(ino, fh).unwrap();
    assert_eq!(fs.test_read(ino, 0, content.len() as u32 + 1).unwrap(), content);
    check_after_remount(fs, dir.path(), &[("big.bin", &content)]);
}

#[test]
fn uncommitted_files_survive_online_gc() {
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    make_garbage(&fs, "junk.bin", &data(6, 300_000));
    // Released but not committed: only the GC start commit records it.
    let fresh = data(7, 300_000);
    write_file(&fs, "fresh.bin", &fresh);

    fs.gc_online(&no_stop()).unwrap().unwrap();
    check_after_remount(fs, dir.path(), &[("fresh.bin", &fresh)]);
}

#[test]
fn a_stop_request_ends_the_gc_without_more_deletes() {
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    let keep = data(8, 300_000);
    write_file(&fs, "keep.bin", &keep);
    make_garbage(&fs, "junk.bin", &data(9, 300_000));

    let stop = AtomicBool::new(true);
    assert!(fs.gc_online(&stop).unwrap().is_none());
    // Tracking is off again: a later full run works and frees the garbage.
    assert!(fs.gc_online(&no_stop()).unwrap().unwrap().objects_removed > 50);
    check_after_remount(fs, dir.path(), &[("keep.bin", &keep)]);
}

#[test]
fn a_parallel_mark_finds_the_same_live_set_as_one_thread() {
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    for i in 0..12 {
        write_file(&fs, &format!("f{i}"), &data(100 + i, 150_000));
    }
    // Shared content: the same bytes under two names.
    write_file(&fs, "dup", &data(100, 150_000));
    make_garbage(&fs, "junk.bin", &data(99, 300_000));

    let one = fs.gc_start().unwrap().with_threads(1);
    let live_one = one.mark(&no_stop()).unwrap().unwrap();
    drop(one);
    let many = fs.gc_start().unwrap().with_threads(8);
    let live_many = many.mark(&no_stop()).unwrap().unwrap();
    assert_eq!(live_one, live_many);
    let stats = many.sweep(&live_many, &no_stop()).unwrap();
    assert!(stats.objects_removed > 50, "{stats:?}");
    drop(many);
    let files: Vec<(String, Vec<u8>)> = (0..12).map(|i| (format!("f{i}"), data(100 + i, 150_000))).collect();
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (n.as_str(), d.as_slice())).collect();
    check_after_remount(fs, dir.path(), &refs);
}

#[test]
fn a_changed_object_file_during_gc_stops_the_sweep() {
    use blockset::Io;
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    let keep = data(20, 300_000);
    write_file(&fs, "keep.bin", &keep);
    make_garbage(&fs, "junk.bin", &data(21, 300_000));
    let before = std::fs::read_dir(dir.path().join("vt0")).unwrap().count();

    let gc = fs.gc_start().unwrap();
    let live = gc.mark(&no_stop()).unwrap().unwrap();
    // Write an existing object file again (as blockset does when a node of the
    // same name gets other bytes): the lock-free mark can not be trusted now.
    let name = live.iter().next().unwrap().clone();
    let old = std::fs::read(dir.path().join(&name)).unwrap();
    fs.io().lock().unwrap().write(&name, &old).unwrap();
    let err = gc.sweep(&live, &no_stop()).unwrap_err();
    assert!(err.to_string().contains("changed during the GC"), "{err}");
    drop(gc);
    assert_eq!(std::fs::read_dir(dir.path().join("vt0")).unwrap().count(), before, "nothing deleted");
    // The next run works.
    assert!(fs.gc_online(&no_stop()).unwrap().unwrap().objects_removed > 50);
    check_after_remount(fs, dir.path(), &[("keep.bin", &keep)]);
}
