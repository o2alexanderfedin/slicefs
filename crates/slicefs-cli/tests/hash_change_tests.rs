//! A change of the write hash (`--hash`): the store records it, new objects
//! carry the new tag, old files stay readable, directory lookups still work
//! (name keys are always BLAKE3-256), and online GC handles both tags.
//! The second hash is `test-keyed-blake3` (the `test-hasher` feature of
//! `chunk-store`, on in the dev-dependencies).

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use metadata::segment::load_store_from_segments;
use metadata::store::DictMetadataStore;
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::content::{self, FORMAT, FORMAT_FILE};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::seed::run_seed;
use slicefs_traits::metadata::MetadataStore;
use tempfile::TempDir;

const TEST_HASH: &str = "test-keyed-blake3";

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

/// Open the store as a mount does: the chunk store with the recorded write
/// hash, the metadata from the last root.
fn open_fs(store: &Path) -> SliceFsFilesystem {
    let segs = store.join("segments");
    std::fs::create_dir_all(&segs).unwrap();
    let io = Arc::new(Mutex::new(content::store_io(store).unwrap()));
    let (root, _) = load_store_from_segments(&segs).unwrap();
    let mut meta = match root {
        Some(r) => DictMetadataStore::load_from_root(io.clone(), &r).unwrap(),
        None => DictMetadataStore::new(io.clone()),
    };
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

fn write_file(fs: &SliceFsFilesystem, parent: u64, name: &str, bytes: &[u8]) -> u64 {
    let (ino, fh) = fs.test_create(parent, name, 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, bytes).unwrap();
    fs.test_release(ino, fh).unwrap();
    ino
}

fn read_file(fs: &SliceFsFilesystem, parent: u64, name: &str) -> Vec<u8> {
    let (ino, attr) = fs.test_lookup(parent, name).unwrap();
    fs.test_read(ino, 0, attr.size as u32).unwrap()
}

/// The tag of the content key of `ino`.
fn tag(fs: &SliceFsFilesystem, ino: u64) -> u8 {
    fs.meta().get_manifest(ino).unwrap()[0].tag()
}

fn marker(store: &Path) -> String {
    std::fs::read_to_string(store.join(FORMAT_FILE)).unwrap()
}

#[test]
fn a_new_write_hash_keeps_old_files_and_lookups() {
    let dir = TempDir::new().unwrap();
    let store = dir.path();

    // A store written with the default hash.
    let fs = open_fs(store);
    let (sub, _) = fs.test_mkdir_full(1, "old_dir", 0o755, 0o022, 0, 0).unwrap();
    let old_root = write_file(&fs, 1, "old.bin", &data(1, 200_000));
    let old_sub = write_file(&fs, sub, "in_dir.bin", &data(2, 50_000));
    let old_garbage = write_file(&fs, 1, "old_garbage.bin", &data(3, 200_000));
    assert_eq!((tag(&fs, old_root), tag(&fs, old_sub)), (0x1e, 0x1e));
    let old_garbage_key = fs.meta().get_manifest(old_garbage).unwrap()[0];
    close_fs(fs);
    assert_eq!(marker(store), format!("{FORMAT}hash blake3-256\n"));

    // Mount with another hash: recorded, new objects carry its tag.
    content::select_hash(store, Some(TEST_HASH)).unwrap();
    assert_eq!(marker(store), format!("{FORMAT}hash {TEST_HASH}\n"));
    let fs = open_fs(store);
    assert_eq!(fs.content().hasher().name(), TEST_HASH);

    // Old files read back; lookups in old directories still work.
    assert_eq!(read_file(&fs, 1, "old.bin"), data(1, 200_000));
    let (sub2, _) = fs.test_lookup(1, "old_dir").unwrap();
    assert_eq!(sub2, sub);
    assert_eq!(read_file(&fs, sub, "in_dir.bin"), data(2, 50_000));

    // New files (also in an old directory) and a new directory.
    let new_sub = write_file(&fs, sub, "new_in_old_dir.bin", &data(4, 50_000));
    let (new_dir, _) = fs.test_mkdir_full(1, "new_dir", 0o755, 0o022, 0, 0).unwrap();
    let new_file = write_file(&fs, new_dir, "new.bin", &data(5, 200_000));
    assert_eq!((tag(&fs, new_sub), tag(&fs, new_file)), (0xf0, 0xf0));
    assert_eq!(fs.test_lookup(sub, "in_dir.bin").unwrap().0, old_sub, "old name, old dir");
    assert_eq!(fs.test_lookup(sub, "new_in_old_dir.bin").unwrap().0, new_sub);

    // Garbage of both tags; online GC frees it and keeps the live files.
    let new_garbage = write_file(&fs, 1, "new_garbage.bin", &data(6, 200_000));
    let new_garbage_key = fs.meta().get_manifest(new_garbage).unwrap()[0];
    assert_eq!((old_garbage_key.tag(), new_garbage_key.tag()), (0x1e, 0xf0));
    fs.meta().commit().unwrap();
    fs.simulate_unlink(1, "old_garbage.bin").unwrap();
    fs.simulate_unlink(1, "new_garbage.bin").unwrap();
    fs.meta().commit().unwrap();
    let stats = fs.gc_online(&AtomicBool::new(false)).unwrap().expect("not stopped");
    assert!(stats.removed_objects > 40, "{stats:?}");
    for k in [old_garbage_key, new_garbage_key] {
        assert!(!fs.content().contains(&content::to_id(&k)), "dead root of tag {:#04x} is freed", k.tag());
    }
    assert_eq!(read_file(&fs, 1, "old.bin"), data(1, 200_000));
    assert_eq!(read_file(&fs, sub, "in_dir.bin"), data(2, 50_000));
    assert_eq!(read_file(&fs, sub, "new_in_old_dir.bin"), data(4, 50_000));
    assert_eq!(read_file(&fs, new_dir, "new.bin"), data(5, 200_000));
    close_fs(fs);

    // Back to blake3-256: both kinds of objects stay readable.
    content::select_hash(store, Some("blake3-256")).unwrap();
    let fs = open_fs(store);
    assert_eq!(read_file(&fs, 1, "old.bin"), data(1, 200_000));
    assert_eq!(read_file(&fs, new_dir, "new.bin"), data(5, 200_000));
    close_fs(fs);
}

/// `seed --hash`: a known name is recorded and used; an unknown one is
/// refused with the known names; no hash line means blake3-256.
#[test]
fn seed_hash_is_recorded_and_unknown_names_are_refused() {
    let src = TempDir::new().unwrap();
    std::fs::write(src.path().join("a.txt"), b"hello").unwrap();

    let dir = TempDir::new().unwrap();
    run_seed(dir.path(), src.path(), Some(TEST_HASH)).unwrap();
    assert_eq!(marker(dir.path()), format!("{FORMAT}hash {TEST_HASH}\n"));
    let fs = open_fs(dir.path());
    let (ino, _) = fs.test_lookup(1, "a.txt").unwrap();
    assert_eq!(tag(&fs, ino), 0xf0);
    assert_eq!(read_file(&fs, 1, "a.txt"), b"hello");
    drop(fs);

    let bad = TempDir::new().unwrap();
    let err = run_seed(bad.path(), src.path(), Some("md5")).unwrap_err().to_string();
    assert!(err.contains("unknown hash \"md5\"") && err.contains("blake3-256"), "{err}");
    let err = content::select_hash(dir.path(), Some("md5")).unwrap_err().to_string();
    assert!(err.contains("blake3-256"), "{err}");
    assert_eq!(marker(dir.path()), format!("{FORMAT}hash {TEST_HASH}\n"), "a refused name changes nothing");

    // A marker without a hash line: blake3-256.
    let plain = TempDir::new().unwrap();
    std::fs::write(plain.path().join(FORMAT_FILE), FORMAT).unwrap();
    assert_eq!(content::write_hasher(plain.path()).unwrap().name(), "blake3-256");
    content::check_format(plain.path()).unwrap();

    // `--hash` on a store of an older format: refused, the marker stays.
    let old = TempDir::new().unwrap();
    let five = "slicefs-store 5 (metadata and file content in packs)\n";
    std::fs::write(old.path().join(FORMAT_FILE), five).unwrap();
    let err = content::select_hash(old.path(), Some(TEST_HASH)).unwrap_err().to_string();
    assert!(err.contains("older SliceFS"), "{err}");
    assert_eq!(marker(old.path()), five);

    // A recorded hash this build does not know: refused at open.
    std::fs::write(plain.path().join(FORMAT_FILE), format!("{FORMAT}hash sha3-1024\n")).unwrap();
    let err = content::open(plain.path()).err().expect("unknown recorded hash").to_string();
    assert!(err.contains("sha3-1024") && err.contains("does not know"), "{err}");
}
