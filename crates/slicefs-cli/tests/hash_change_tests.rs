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
use slicefs_cli::header::{HASH_SHA256_FLAG, HASH_SHA256_RELEASE};
use slicefs_cli::seed::run_seed;
use slicefs_traits::key::ObjectKey;
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
    // The live mark is a salted Bloom filter (#113): a false positive can keep
    // a dead object for one GC. A later GC has a new salt and frees it.
    for _ in 0..3 {
        if [old_garbage_key, new_garbage_key].iter().all(|k| !fs.content().contains(&content::to_id(k))) {
            break;
        }
        fs.gc_online(&AtomicBool::new(false)).unwrap().expect("not stopped");
    }
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

/// WR-03: the header step of `seed --hash` (`check_write`) on a new store
/// gives no hash-change line and writes the chosen hash in the first
/// header; an unknown name makes no header. On an existing store a change
/// gives the line.
#[test]
fn seed_hash_on_a_new_store_prints_no_change_line() {
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("new");
    assert_eq!(content::check_write(&store, Some(TEST_HASH)).unwrap(), None, "no line for a new store");
    assert_eq!(marker(&store), format!("{FORMAT}hash {TEST_HASH}\n"));
    let line = content::check_write(&store, Some("blake3-256")).unwrap();
    assert_eq!(
        line.as_deref(),
        Some("New objects use hash blake3-256 (was test-keyed-blake3); stored objects stay readable.")
    );
    let bad = dir.path().join("bad");
    assert!(content::check_write(&bad, Some("md5")).is_err());
    assert!(!bad.exists(), "an unknown hash name makes no store");
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
    slicefs_cli::header::check(plain.path(), slicefs_cli::header::Want::Read).unwrap();

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

/// The `hash-sha256` flag line of this build.
fn sha256_flag() -> String {
    format!("incompat {HASH_SHA256_FLAG} {HASH_SHA256_RELEASE}\n")
}

/// Every object of the last root (metadata nodes and file content), with
/// the "no object" key left out.
fn all_objects(store: &Path) -> Vec<chunk_store::ObjectId> {
    let (root, _) = load_store_from_segments(&store.join("segments")).unwrap();
    let root = root.expect("a root");
    let mut io = content::store_io(store).unwrap();
    let mut keys = metadata::store::referenced_roots(&mut io, &root).unwrap();
    keys.extend(metadata::store::content_keys(&mut io, &root).unwrap());
    let cs = Arc::clone(io.store());
    let ids = keys
        .iter()
        .filter(|k| **k != ObjectKey::ZERO)
        .flat_map(|k| chunk_store::file::file_objects(&cs, content::to_id(k)).unwrap())
        .collect();
    cs.close().unwrap();
    ids
}

/// HASH-01 tracer: `seed --hash sha256` makes a store whose first header
/// has the hash line and the incompat flag, whose objects all have tag
/// 0x12, whose files read back, and which scrub accepts.
#[test]
fn sha256_seed_records_the_flag_and_tags_every_object() {
    let src = TempDir::new().unwrap();
    let files: [(&str, Vec<u8>); 3] = [("big.bin", data(1, 300_000)), ("small.txt", b"ten bytes!".to_vec()), ("empty", Vec::new())];
    for (name, bytes) in &files {
        std::fs::write(src.path().join(name), bytes).unwrap();
    }
    let dir = TempDir::new().unwrap();
    let store = dir.path();
    run_seed(store, src.path(), Some("sha256")).unwrap();
    assert_eq!(marker(store), format!("{FORMAT}hash sha256\n{}", sha256_flag()));

    let ids = all_objects(store);
    assert!(ids.len() > 10, "{}", ids.len());
    assert!(ids.iter().all(|id| id.tag() == 0x12), "every object has tag 0x12");

    let fs = open_fs(store);
    for (name, bytes) in &files {
        assert_eq!(&read_file(&fs, 1, name), bytes, "{name}");
    }
    close_fs(fs);
    slicefs_cli::scrub::run_scrub(store, false).unwrap();
    let st = slicefs_cli::stats::store_stats(store).unwrap();
    let names: Vec<_> = st.flags.incompat.iter().map(|f| (f.name.as_str(), f.release.as_str(), f.known)).collect();
    assert_eq!(names, [(HASH_SHA256_FLAG, HASH_SHA256_RELEASE, true)]);
}

/// A BLAKE3-256 store from `seed` with one 100 KiB file `old.bin`.
fn blake3_store() -> TempDir {
    let src = TempDir::new().unwrap();
    std::fs::write(src.path().join("old.bin"), data(1, 100_000)).unwrap();
    let dir = TempDir::new().unwrap();
    run_seed(dir.path(), src.path(), None).unwrap();
    assert_eq!(marker(dir.path()), format!("{FORMAT}hash blake3-256\n"));
    dir
}

/// D-11, the `mount --hash` path: the hash line and the flag reach disk in
/// one header write before the first SHA-256 object; new files get tag
/// 0x12, old files keep 0x1e, both read back, scrub passes. Then a change
/// back to blake3-256 keeps the flag.
#[test]
fn sha256_mount_hash_change_writes_the_flag_with_the_hash_line() {
    let dir = blake3_store();
    let store = dir.path();
    content::select_hash(store, Some("sha256")).unwrap();
    assert_eq!(marker(store), format!("{FORMAT}hash sha256\n{}", sha256_flag()));

    let fs = open_fs(store);
    let new = write_file(&fs, 1, "new.bin", &data(2, 50_000));
    let (old, _) = fs.test_lookup(1, "old.bin").unwrap();
    assert_eq!((tag(&fs, new), tag(&fs, old)), (0x12, 0x1e));
    assert_eq!(read_file(&fs, 1, "new.bin"), data(2, 50_000));
    assert_eq!(read_file(&fs, 1, "old.bin"), data(1, 100_000));
    close_fs(fs);
    slicefs_cli::scrub::run_scrub(store, false).unwrap();
}

/// D-11: after the write hash goes back to blake3-256 the flag stays (the
/// store still holds SHA-256 objects), and the store opens to write.
#[test]
fn sha256_flag_stays_after_the_hash_goes_back() {
    let dir = blake3_store();
    let store = dir.path();
    content::select_hash(store, Some("sha256")).unwrap();
    let fs = open_fs(store);
    write_file(&fs, 1, "new.bin", &data(2, 50_000));
    close_fs(fs);

    content::select_hash(store, Some("blake3-256")).unwrap();
    assert_eq!(marker(store), format!("{FORMAT}hash blake3-256\n{}", sha256_flag()));
    slicefs_cli::header::check(store, slicefs_cli::header::Want::Write).unwrap();
    let fs = open_fs(store);
    assert_eq!(read_file(&fs, 1, "new.bin"), data(2, 50_000));
    close_fs(fs);
}

/// D-18 step 2: `ensure_hash_flag` writes only the flag line (the hash
/// line does not change), once; a hash with no flag writes nothing.
#[test]
fn sha256_ensure_hash_flag_writes_only_the_flag() {
    let dir = blake3_store();
    let store = dir.path();
    let before = marker(store);
    assert!(content::ensure_hash_flag(store, &chunk_store::hash::Sha256).unwrap());
    assert_eq!(marker(store), format!("{before}{}", sha256_flag()));
    let after = marker(store);
    assert!(!content::ensure_hash_flag(store, &chunk_store::hash::Sha256).unwrap());
    assert_eq!(marker(store), after, "a second call writes nothing");
    assert!(!content::ensure_hash_flag(store, &chunk_store::Blake3_256).unwrap());
    assert_eq!(marker(store), after);
}
