//! The `slicefs rehash` command: an offline store converts to another
//! hash. Every live object gets the target tag, the current tree and the
//! snapshot trees do not change, scrub passes, and the old objects are
//! freed by the GC of the run.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chunk_store::file::file_objects;
use chunk_store::{ChunkProfile, ObjectId};
use metadata::segment::{load_store_from_segments, scan_store_with_log};
use metadata::store::{content_keys, referenced_roots, DictMetadataStore};
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::content::{self, FORMAT, FORMAT_FILE, REHASH_TMP_DIR};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::header::{self, HASH_SHA256_FLAG, HASH_SHA256_RELEASE};
use slicefs_cli::rehash::{rehash_report_with, run_rehash, RehashArgs, Step};
use slicefs_cli::seed::{run_seed, run_seed_with};
use slicefs_cli::snapshot::run_snapshot_create;
use slicefs_traits::key::ObjectKey;
use slicefs_traits::metadata::MetadataStore;
use tempfile::TempDir;

fn data(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

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

/// Open the store as a mount does, with a write log per operation.
fn open_fs(store: &Path) -> SliceFsFilesystem {
    let segs = store.join("segments");
    let io = Arc::new(Mutex::new(content::store_io(store).unwrap()));
    let (root, snaps) = load_store_from_segments(&segs).unwrap();
    let mut meta = DictMetadataStore::load_from_root(io.clone(), &root.unwrap()).unwrap();
    meta.set_snapshots(snaps);
    meta.set_wal(create_wal(WalConfig::PerOp, store, next_segment(store)).unwrap());
    SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()))
}

fn close_fs(fs: SliceFsFilesystem) {
    fs.meta().commit().unwrap();
    fs.meta().shutdown_wal().unwrap();
    fs.content().close().unwrap();
}

fn write_file(fs: &SliceFsFilesystem, parent: u64, name: &str, bytes: &[u8]) {
    let (ino, fh) = fs.test_create(parent, name, 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, bytes).unwrap();
    fs.test_release(ino, fh).unwrap();
}

fn marker(store: &Path) -> String {
    std::fs::read_to_string(store.join(FORMAT_FILE)).unwrap()
}

/// Names, sizes and BLAKE3 sums of every file of the store tree.
fn tree(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut todo = vec![root.to_path_buf()];
    let mut buf = vec![0u8; 64 << 10];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            let m = std::fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                out.insert(format!("{rel}/"), String::new());
                todo.push(p);
                continue;
            }
            let mut f = std::fs::File::open(&p).unwrap();
            let mut h = blake3::Hasher::new();
            loop {
                let n = f.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            out.insert(rel, format!("{} {}", m.len(), h.finalize().to_hex()));
        }
    }
    out
}

/// A BLAKE3-256 store at `<dir>/store` seeded from a few files (one with a
/// zero run), with snapshot `s1`, then one more file written by a mount.
fn seeded_with_snapshot() -> TempDir {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("big.bin"), data(1, 300 << 10)).unwrap();
    std::fs::write(src.join("small.txt"), b"ten bytes!").unwrap();
    std::fs::write(src.join("empty"), b"").unwrap();
    let mut sparse = data(3, 256 << 10);
    sparse.extend(std::iter::repeat_n(0u8, 512 << 10));
    sparse.extend(data(4, 256 << 10));
    std::fs::write(src.join("sparse.bin"), sparse).unwrap();
    let store = dir.path().join("store");
    run_seed(&store, &src, None).unwrap();
    run_snapshot_create(&store, Some("s1".into())).unwrap();
    let fs = open_fs(&store);
    write_file(&fs, 1, "new.bin", &data(5, 100 << 10));
    close_fs(fs);
    dir
}

/// The tree under `root`, read through a read-only node store.
fn dump_root(store: &Path, root: &ObjectKey) -> BTreeMap<String, String> {
    let io = Arc::new(Mutex::new(content::store_io_read_only(store).unwrap()));
    let meta = DictMetadataStore::load_from_root(io.clone(), root).unwrap();
    let fs = SliceFsFilesystem::new(meta, io, None);
    common::dump(&fs)
}

/// The current root and the snapshot roots, as the loader gives them.
fn loader_roots(store: &Path) -> Vec<ObjectKey> {
    let (root, snaps) = load_store_from_segments(&store.join("segments")).unwrap();
    root.into_iter().chain(snaps.iter().map(|s| s.root)).collect()
}

/// Every object of the current root and of each snapshot root: the
/// metadata nodes, the file contents, and their manifest trees and chunks.
fn live_ids(store: &Path) -> BTreeSet<ObjectId> {
    let mut io = content::store_io_read_only(store).unwrap();
    let cs = Arc::clone(io.store());
    let mut ids = BTreeSet::new();
    for r in loader_roots(store) {
        let mut keys = referenced_roots(&mut io, &r).unwrap();
        keys.extend(content_keys(&mut io, &r).unwrap());
        for k in keys.iter().filter(|k| **k != ObjectKey::ZERO) {
            ids.extend(file_objects(&cs, content::to_id(k)).unwrap());
        }
    }
    ids
}

/// The tags of every object of the current root and of each snapshot root.
fn live_tags(store: &Path) -> BTreeSet<u8> {
    live_ids(store).iter().map(ObjectId::tag).collect()
}

fn all_tags(store: &Path) -> BTreeSet<u8> {
    live_tags(store)
}

fn rehash(store: &Path, to: &str, dry_run: bool) -> String {
    rehash_report_with(&RehashArgs::new(store, to, dry_run), &mut |_| {}).unwrap()
}

/// The number after `prefix` on the report line that starts with it.
fn number_after(report: &str, prefix: &str) -> u64 {
    let line = report.lines().find(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("no {prefix:?} line in:\n{report}"));
    let digits: String = line[prefix.len()..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or_else(|_| panic!("no number in {line:?}"))
}

/// HASH-02 tracer: `rehash --to sha256` converts a seeded BLAKE3-256 store
/// with a snapshot. The dry-run before it changes no file.
#[test]
fn a_blake3_store_rehashes_to_sha256() {
    let dir = seeded_with_snapshot();
    let store = dir.path().join("store");
    let segs = store.join("segments");
    let (root, snaps) = load_store_from_segments(&segs).unwrap();
    let root = root.unwrap();
    assert_eq!(snaps.len(), 1);
    let (cur_dump, snap_dump) = (dump_root(&store, &root), dump_root(&store, &snaps[0].root));
    assert!(cur_dump.contains_key("/new.bin") && !snap_dump.contains_key("/new.bin"));
    assert_eq!(all_tags(&store), BTreeSet::from([0x1e]));

    let files = tree(&store);
    let dry = rehash(&store, "sha256", true);
    assert!(dry.contains("dry run"), "{dry}");
    assert_eq!(tree(&store), files, "the dry-run changes no file");

    let report = rehash(&store, "sha256", false);
    assert_eq!(marker(&store), format!("{FORMAT}hash sha256\nincompat {HASH_SHA256_FLAG} {HASH_SHA256_RELEASE}\n"));
    let (new_root, new_snaps) = load_store_from_segments(&segs).unwrap();
    let new_root = new_root.unwrap();
    assert_ne!(new_root, root);
    assert_eq!(new_snaps.len(), 1);
    let (old, new) = (&snaps[0], &new_snaps[0]);
    assert_eq!((new.version, &new.name, new.created_at), (old.version, &old.name, old.created_at));
    assert_ne!(new.root, old.root);
    assert_eq!(all_tags(&store), BTreeSet::from([0x12]));
    assert_eq!(dump_root(&store, &new_root), cur_dump);
    assert_eq!(dump_root(&store, &new.root), snap_dump);
    slicefs_cli::scrub::run_scrub(&store, false).unwrap();
    assert!(!store.join(REHASH_TMP_DIR).exists());
    assert!(number_after(&report, "Bytes freed: ") > 0, "{report}");
}

/// D-16, R-02: a writable open removes a `rehash-tmp` that a crash left; a
/// read-only open leaves it.
#[test]
fn a_writable_open_removes_a_stale_rehash_tmp() {
    let dir = seeded_with_snapshot();
    let store = dir.path().join("store");
    let tmp = store.join(REHASH_TMP_DIR);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("x"), b"left by a crash").unwrap();
    drop(content::open_read_only(&store).unwrap());
    assert!(tmp.join("x").exists(), "a read-only open writes nothing");
    content::open(&store).unwrap().close().unwrap();
    assert!(!tmp.exists());
}

/// A BLAKE3-256 store at `<dir>/store` with one small file.
fn seeded_small() -> TempDir {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.bin"), data(7, 20 << 10)).unwrap();
    run_seed(&dir.path().join("store"), &src, None).unwrap();
    dir
}

/// The error text of a run that must fail.
fn refused(store: &Path, to: &str, dry_run: bool) -> String {
    match rehash_report_with(&RehashArgs::new(store, to, dry_run), &mut |_| {}) {
        Ok(report) => panic!("rehash is refused, but it ran:\n{report}"),
        Err(e) => e.to_string(),
    }
}

/// Assert that `f` gives an error with every part of `parts` and that the
/// store tree does not change.
fn assert_refused_and_unchanged(store: &Path, parts: &[&str], f: impl FnOnce() -> String) {
    let files = tree(store);
    let e = f();
    assert!(parts.iter().all(|p| e.contains(p)), "{e}");
    assert_eq!(tree(store), files, "a refusal changes no file");
}

/// D-02: a mount or a command holds the store lock exclusive.
#[test]
fn refuse_while_the_store_lock_is_held_exclusive() {
    let dir = seeded_small();
    let store = dir.path().join("store");
    let lock = header::lock_offline(&store).unwrap();
    assert_refused_and_unchanged(&store, &["in use"], || refused(&store, "sha256", false));
    drop(lock);
}

/// D-02: a read-only mount holds the store lock shared.
#[test]
fn refuse_while_the_store_lock_is_held_shared() {
    let dir = seeded_small();
    let store = dir.path().join("store");
    let lock = header::lock_read_mount(&store).unwrap();
    assert_refused_and_unchanged(&store, &["in use"], || refused(&store, "sha256", false));
    drop(lock);
}

/// R-01: a `mount.lock` file with a free store lock (a crashed mount).
#[test]
fn refuse_with_a_mount_lock_file() {
    let dir = seeded_small();
    let store = dir.path().join("store");
    std::fs::write(store.join("mount.lock"), b"").unwrap();
    assert_refused_and_unchanged(&store, &["mount.lock", "Mount and unmount the store once"], || refused(&store, "sha256", false));
}

/// Write-log records after the last root, and the store open with no
/// commit: the store as a crashed mount leaves it.
fn store_with_log_records() -> TempDir {
    let dir = seeded_small();
    let store = dir.path().join("store");
    let fs = open_fs(&store);
    // A logged request, as a mount runs it.
    let r = slicefs_cli::handlers::handle_mkdir(&fs, 1, "not-in-a-root", 0o040_755, 0o022, 0, 0);
    assert!(matches!(r, slicefs_cli::handlers::EntryResult::Ok(..)));
    fs.meta().flush_wal().unwrap();
    fs.meta().shutdown_wal().unwrap();
    fs.content().close().unwrap();
    drop(fs);
    let (_, _, log) = scan_store_with_log(&store.join("segments")).unwrap();
    assert!(!log.is_empty(), "the write log has records");
    dir
}

/// D-17: write-log records after the last root.
#[test]
fn refuse_with_write_log_records_after_the_last_root() {
    let dir = store_with_log_records();
    let store = dir.path().join("store");
    assert_refused_and_unchanged(&store, &["write-log records that are not in a root"], || refused(&store, "sha256", false));
}

/// D-23: an unknown target.
#[test]
fn refuse_an_unknown_target() {
    let dir = seeded_small();
    let store = dir.path().join("store");
    assert_refused_and_unchanged(&store, &["unknown hash \"md5\""], || run_rehash(&store, "md5", false).unwrap_err().to_string());
}

/// A missing path and an empty directory are no store; nothing is made.
#[test]
fn refuse_a_path_with_no_store() {
    let dir = TempDir::new().unwrap();
    let (missing, empty) = (dir.path().join("missing"), dir.path().join("empty"));
    std::fs::create_dir_all(&empty).unwrap();
    for store in [&missing, &empty] {
        let e = refused(store, "sha256", false);
        assert!(e.contains("store not found"), "{e}");
    }
    assert!(!missing.exists(), "no directory is made");
    assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0, "nothing is made");
}

/// Names of the segment files.
fn segment_names(store: &Path) -> BTreeSet<String> {
    std::fs::read_dir(store.join("segments")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect()
}

/// D-22: the dry-run writes no file and reports an estimate.
#[test]
fn dry_run_changes_nothing_and_reports_an_estimate() {
    let dir = seeded_with_snapshot();
    let store = dir.path().join("store");
    let files = tree(&store);
    let report = rehash(&store, "sha256", true);
    assert_eq!(tree(&store), files, "the dry-run changes no file");
    assert!(!store.join(REHASH_TMP_DIR).exists());
    assert!(!marker(&store).contains(HASH_SHA256_FLAG), "no flag");
    assert!(report.starts_with("Rehash to sha256 (dry run, nothing written)"), "{report}");
    assert!(number_after(&report, "Objects of another hash: ") > 0, "{report}");
    assert!(report.contains("estimate"), "{report}");
}

/// D-22: the dry-run makes the checks of a real run and refuses with the
/// same text.
#[test]
fn dry_run_refuses_like_a_real_run() {
    let dir = seeded_small();
    let store = dir.path().join("store");
    let lock = header::lock_offline(&store).unwrap();
    assert_eq!(refused(&store, "sha256", true), refused(&store, "sha256", false));
    drop(lock);

    std::fs::write(store.join("mount.lock"), b"").unwrap();
    assert_eq!(refused(&store, "sha256", true), refused(&store, "sha256", false));
    std::fs::remove_file(store.join("mount.lock")).unwrap();

    let dir = store_with_log_records();
    let store = dir.path().join("store");
    let real = refused(&store, "sha256", false);
    assert!(real.contains("write-log records"), "{real}");
    assert_eq!(refused(&store, "sha256", true), real);
}

/// D-21: the report of a real run.
#[test]
fn report_has_the_counts_and_bytes_freed() {
    let dir = seeded_with_snapshot();
    let store = dir.path().join("store");
    let report = rehash(&store, "sha256", false);
    for prefix in ["Rehash to sha256:", "Read:", "Rewritten:", "Kept as is:", "GC (full):", "Bytes freed:", "store disk usage: before"] {
        assert!(report.lines().any(|l| l.starts_with(prefix)), "no {prefix:?} line in:\n{report}");
    }
    assert!(number_after(&report, "Bytes freed: ") > 0, "{report}");
}

/// D-21: a progress line at each step, in order.
#[test]
fn progress_lines_come_at_each_step() {
    let dir = seeded_with_snapshot();
    let store = dir.path().join("store");
    let mut lines = Vec::new();
    let args = RehashArgs { progress_every: Duration::ZERO, ..RehashArgs::new(&store, "sha256", false) };
    rehash_report_with(&args, &mut |l| lines.push(l.to_string())).unwrap();
    let at = |part: &str| lines.iter().position(|l| l.contains(part)).unwrap_or_else(|| panic!("no {part:?} line in {lines:#?}"));
    for root in 1..=2 {
        at(&format!("rewrite: root {root} of 2"));
    }
    let order = [at("checks passed"), at("rewrite: "), at("rehash: switch"), at("rehash: gc")];
    assert!(order.is_sorted(), "{order:?} {lines:#?}");
}

/// R-08: a second run finds every object with the target hash, writes no
/// segment, still runs the GC and keeps the header line.
#[test]
fn a_second_run_finds_nothing_to_rewrite() {
    let dir = seeded_with_snapshot();
    let store = dir.path().join("store");
    rehash(&store, "sha256", false);
    let names = segment_names(&store);
    let report = rehash(&store, "sha256", false);
    assert!(report.contains("Every live object already has hash sha256"), "{report}");
    assert!(report.contains("GC (full):"), "{report}");
    assert_eq!(segment_names(&store), names, "no new segment");
    assert!(marker(&store).contains("hash sha256\n"));
}

/// D-23: a store whose objects all have the target tag, with another hash
/// line, only gets the header line.
#[test]
fn a_store_with_only_target_objects_gets_the_header_line() {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.bin"), data(9, 50 << 10)).unwrap();
    let store = dir.path().join("store");
    run_seed(&store, &src, Some("sha256")).unwrap();
    content::select_hash(&store, Some("blake3-256")).unwrap();
    assert!(marker(&store).contains("hash blake3-256\n"));
    let names = segment_names(&store);
    let report = rehash(&store, "sha256", false);
    // The key-carrying nodes are always translated (and counted as
    // rewritten), but they give the same keys: no new object, no new root.
    assert!(report.contains("; new objects: 0 (0 bytes stored)."), "{report}");
    assert!(report.contains("Every live object already has hash sha256"), "{report}");
    assert_eq!(segment_names(&store), names, "no new segment");
    assert!(marker(&store).contains("hash sha256\n"));
}

/// Write `bytes` at `off` into the existing file `name` of `parent`: a
/// spliced file, whose old chunks around the range stay.
fn overwrite(fs: &SliceFsFilesystem, parent: u64, name: &str, off: u64, bytes: &[u8]) {
    let (ino, _) = fs.test_lookup(parent, name).unwrap();
    let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    fs.test_write(fh, off, bytes).unwrap();
    fs.test_release(ino, fh).unwrap();
}

/// A store at `<dir>/store` with write hash `hash` and chunk profile
/// `profile`. Seeded: a 400 KiB random file, a 10-byte file, an empty file,
/// a 3 MiB sparse file with 64 KiB of data at 1 MiB, and a subdirectory with
/// two files of equal content. Then by mounts: an xattr, a spliced edit of
/// the 400 KiB file and a new file; snapshot `s1`; a new file and an edit;
/// snapshot `s2`; one more edit.
fn rich_store(hash: Option<&str>, profile: Option<ChunkProfile>) -> TempDir {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("big.bin"), data(11, 400 << 10)).unwrap();
    std::fs::write(src.join("small.txt"), b"ten bytes!").unwrap();
    std::fs::write(src.join("empty"), b"").unwrap();
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::File::create(src.join("sparse.bin")).unwrap();
        f.set_len(3 << 20).unwrap();
        f.seek(SeekFrom::Start(1 << 20)).unwrap();
        f.write_all(&data(12, 64 << 10)).unwrap();
    }
    std::fs::write(src.join("sub/a.bin"), data(13, 30 << 10)).unwrap();
    std::fs::write(src.join("sub/b.bin"), data(13, 30 << 10)).unwrap();
    let store = dir.path().join("store");
    run_seed_with(&store, &src, hash, profile).unwrap();

    let fs = open_fs(&store);
    let (big, _) = fs.test_lookup(1, "big.bin").unwrap();
    fs.test_setxattr(big, "user.note", b"rehash keeps me").unwrap();
    overwrite(&fs, 1, "big.bin", 150 << 10, &data(14, 8 << 10));
    write_file(&fs, 1, "new1.bin", &data(15, 40 << 10));
    close_fs(fs);
    run_snapshot_create(&store, Some("s1".into())).unwrap();

    let fs = open_fs(&store);
    write_file(&fs, 1, "new2.bin", &data(16, 20 << 10));
    overwrite(&fs, 1, "small.txt", 0, b"TEN");
    close_fs(fs);
    run_snapshot_create(&store, Some("s2".into())).unwrap();

    let fs = open_fs(&store);
    overwrite(&fs, 1, "big.bin", 300 << 10, &data(17, 4 << 10));
    close_fs(fs);
    dir
}

/// The snapshot list (version, name, `created_at`) and the dumps of the
/// current tree and of each snapshot tree.
#[derive(Debug, PartialEq)]
struct Recorded {
    snapshots: Vec<(u64, Option<String>, u64)>,
    dumps: Vec<BTreeMap<String, String>>,
}

fn record(store: &Path) -> Recorded {
    let (_, snaps) = load_store_from_segments(&store.join("segments")).unwrap();
    let snapshots = snaps.iter().map(|s| (s.version, s.name.clone(), s.created_at)).collect();
    let dumps = loader_roots(store).iter().map(|r| dump_root(store, r)).collect();
    Recorded { snapshots, dumps }
}

/// The files and the snapshots are as recorded, and scrub passes.
fn same_store(before: &Recorded, store: &Path) {
    let now = record(store);
    assert_eq!(now.snapshots, before.snapshots, "the snapshot list");
    assert_eq!(now.dumps.len(), before.dumps.len());
    for (i, (a, b)) in before.dumps.iter().zip(&now.dumps).enumerate() {
        assert!(a == b, "root {i}: {:#?}", common::first_differences(a, b, 5));
    }
    slicefs_cli::scrub::run_scrub(store, false).unwrap();
}

/// D-05, HASH-02: the key of each file content of each root is the key of
/// a fresh write of its bytes in the store opened as a mount opens it (the
/// write hash and the chunk profile of the header).
fn assert_fresh_write_keys(store: &Path) {
    let keys: BTreeSet<ObjectKey> = {
        let mut io = content::store_io_read_only(store).unwrap();
        loader_roots(store).iter().flat_map(|r| content_keys(&mut io, r).unwrap()).filter(|k| *k != ObjectKey::ZERO).collect()
    };
    assert!(keys.len() > 3, "{} keys", keys.len());
    let cs = content::open(store).unwrap();
    for k in &keys {
        let bytes = content::get_all(&cs, k).unwrap();
        assert_eq!(content::put_all(&cs, &bytes).unwrap(), *k, "a fresh write of {} bytes", bytes.len());
    }
    cs.close().unwrap();
}

/// Rehash `store` to `to` (tag `tag`), then check: every live object has
/// the tag; no object of another tag that the old roots used is left; the
/// GC left no dead object; files, snapshots and scrub are as before; the
/// keys are fresh-write keys.
fn rehash_and_check(store: &Path, to: &str, tag: u8) {
    let old_ids = live_ids(store);
    let before = record(store);
    let report = rehash(store, to, false);
    assert_eq!(live_tags(store), BTreeSet::from([tag]), "{report}");
    let cs = content::open_read_only(store).unwrap();
    let left = old_ids.iter().filter(|id| id.tag() != tag && cs.contains(id)).count();
    assert_eq!(left, 0, "old objects left, of {}", old_ids.len());
    drop(cs);
    assert!(report.contains("dead left: 0"), "{report}");
    same_store(&before, store);
    assert_fresh_write_keys(store);
    assert!(marker(store).contains(&format!("hash {to}\n")), "{}", marker(store));
    assert!(!store.join(REHASH_TMP_DIR).exists());
}

/// HASH-06: BLAKE3-256 to SHA-256 and back.
#[test]
fn round_trip_blake3_to_sha256_and_back() {
    let dir = rich_store(None, None);
    let store = dir.path().join("store");
    assert_eq!(live_tags(&store), BTreeSet::from([0x1e]));
    rehash_and_check(&store, "sha256", 0x12);
    rehash_and_check(&store, "blake3-256", 0x1e);
    // D-11: the flag stays after the way back.
    let flag = format!("incompat {HASH_SHA256_FLAG} {HASH_SHA256_RELEASE}\n");
    assert!(marker(&store).contains(&flag), "{}", marker(&store));
}

/// HASH-06: SHA-256 to BLAKE3-256 and back.
#[test]
fn round_trip_sha256_to_blake3_and_back() {
    let dir = rich_store(Some("sha256"), None);
    let store = dir.path().join("store");
    assert_eq!(live_tags(&store), BTreeSet::from([0x12]));
    rehash_and_check(&store, "blake3-256", 0x1e);
    rehash_and_check(&store, "sha256", 0x12);
}

/// The second hash of the tests (`test-hasher` feature, tag 0xf0).
const TEST_HASH: &str = "test-keyed-blake3";

/// HASH-06, D-14: a store with live objects of three hashes (BLAKE3-256,
/// the test keyed hash after a hash change with a spliced edit of an old
/// file, and SHA-256) ends with one hash.
#[test]
fn round_trip_mixed_store_ends_with_one_hash() {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("old.bin"), data(21, 1 << 20)).unwrap();
    std::fs::write(src.join("keep.bin"), data(22, 60 << 10)).unwrap();
    let store = dir.path().join("store");
    run_seed(&store, &src, None).unwrap();

    content::select_hash(&store, Some(TEST_HASH)).unwrap();
    let fs = open_fs(&store);
    overwrite(&fs, 1, "old.bin", 512 << 10, &data(23, 8 << 10));
    write_file(&fs, 1, "keyed.bin", &data(24, 50 << 10));
    close_fs(fs);
    run_snapshot_create(&store, Some("mixed".into())).unwrap();

    content::select_hash(&store, Some("sha256")).unwrap();
    let fs = open_fs(&store);
    write_file(&fs, 1, "sha.bin", &data(25, 50 << 10));
    // Now the file root of old.bin has the SHA-256 tag over chunks of all
    // three tags (D-14).
    overwrite(&fs, 1, "old.bin", 100 << 10, &data(26, 8 << 10));
    close_fs(fs);
    assert_eq!(live_tags(&store), BTreeSet::from([0x1e, 0xf0, 0x12]));
    let copy = dir.path().join("copy");
    copy_dir(&store, &copy);

    rehash_and_check(&store, "blake3-256", 0x1e);
    rehash_and_check(&store, "sha256", 0x12);
    // The target is the write hash of the header: the key-carrying nodes
    // of the last commit already have the target tag but list old keys.
    rehash_and_check(&copy, "sha256", 0x12);
}

/// A copy of the directory tree `from` at `to` (a store that no one holds).
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to.join(e.file_name()));
        } else {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

/// HASH-02, D-05: with the 16/64/256 KiB profile, the rehashed keys are the
/// keys of fresh writes under that profile.
#[test]
fn round_trip_large_profile_keys_equal_fresh_writes() {
    let large = ChunkProfile::new(16384, 65536, 262144).unwrap();
    let dir = rich_store(None, Some(large));
    let store = dir.path().join("store");
    assert_eq!(content::open_read_only(&store).unwrap().profile(), large);
    rehash_and_check(&store, "sha256", 0x12);
    assert_eq!(content::open_read_only(&store).unwrap().profile(), large);
}

/// A run to SHA-256 that a hook stops; the error text.
fn stopped_run(store: &Path, stop_after: Option<Step>) -> String {
    let args = RehashArgs { stop_after, ..RehashArgs::new(store, "sha256", false) };
    match rehash_report_with(&args, &mut |_| {}) {
        Ok(report) => panic!("the hook stops the run, but it ran:\n{report}"),
        Err(e) => e.to_string(),
    }
}

/// The root set that a crash leaves.
enum Expect {
    /// The old current root (and the old snapshot roots).
    Old(ObjectKey),
    /// New roots: the current root and every snapshot root have the
    /// SHA-256 tag.
    New,
}

/// The store after a crash opens with the old or the new roots, reads the
/// same files, keeps the snapshot list and passes scrub. The writable open
/// is the one of a mount (it removes `rehash-tmp`); nothing is committed,
/// so the segments that the crash left stay for the rerun.
fn assert_opens(store: &Path, before: &Recorded, expect: &Expect) {
    let roots = loader_roots(store);
    match expect {
        Expect::Old(old) => assert_eq!(roots[0], *old, "the old root"),
        Expect::New => assert!(roots.iter().all(|r| r.tag() == 0x12), "new roots: {roots:?}"),
    }
    content::open(store).unwrap().close().unwrap();
    same_store(before, store);
}

/// Files ending in `.seg.tmp` in `segments/`.
fn seg_tmps(store: &Path) -> Vec<String> {
    segment_names(store).into_iter().filter(|n| n.ends_with(".seg.tmp")).collect()
}

/// After the rerun: one tag, the same files and snapshots, the header line
/// and the flag, no temp files.
fn assert_finished(store: &Path, before: &Recorded) {
    assert_eq!(live_tags(store), BTreeSet::from([0x12]));
    same_store(before, store);
    let m = marker(store);
    assert!(m.contains("hash sha256\n") && m.contains(&format!("incompat {HASH_SHA256_FLAG} ")), "{m}");
    assert!(!store.join(REHASH_TMP_DIR).exists());
    assert_eq!(seg_tmps(store), Vec::<String>::new());
}

/// HASH-04, D-06, D-18: a crash after each step leaves the old roots
/// (before the switch) or the new roots (after it); a rerun finishes.
#[test]
fn crash_after_each_step_then_rerun() {
    let steps = [
        (Step::Checked, false),
        (Step::FlagWritten, false),
        (Step::Rewritten, false),
        (Step::Synced, false),
        (Step::Switched, true),
        (Step::HeaderWritten, true),
        (Step::GcDone, true),
    ];
    for (step, new) in steps {
        let dir = seeded_with_snapshot();
        let store = dir.path().join("store");
        let old = loader_roots(&store)[0];
        let before = record(&store);
        let e = stopped_run(&store, Some(step));
        assert!(e.contains("test hook"), "{step:?}: {e}");
        let m = marker(&store);
        match step {
            Step::Checked => assert!(!m.contains(HASH_SHA256_FLAG), "{m}"),
            Step::FlagWritten | Step::Rewritten | Step::Synced | Step::Switched => {
                // The flag is there; the hash line is still the old one (after
                // the switch: a legal mixed store).
                assert!(m.contains(HASH_SHA256_FLAG) && m.contains("hash blake3-256\n"), "{step:?}: {m}");
            }
            Step::HeaderWritten | Step::GcDone => assert!(m.contains("hash sha256\n"), "{step:?}: {m}"),
        }
        assert_opens(&store, &before, &if new { Expect::New } else { Expect::Old(old) });
        rehash(&store, "sha256", false);
        assert_finished(&store, &before);
    }
}

/// D-16, D-18: a crash inside the rewrite leaves `rehash-tmp` and the old
/// roots; the writable open removes `rehash-tmp`; a rerun finishes.
#[test]
fn crash_inside_the_rewrite_then_rerun() {
    let dir = seeded_with_snapshot();
    let store = dir.path().join("store");
    let old = loader_roots(&store)[0];
    let before = record(&store);
    let e = rehash_report_with(&RehashArgs { stop_after_values: Some(3), ..RehashArgs::new(&store, "sha256", false) }, &mut |_| {}).unwrap_err().to_string();
    assert!(e.contains("stopped after 3 rewritten values") && e.contains("Nothing was switched"), "{e}");
    assert!(store.join(REHASH_TMP_DIR).exists());
    assert_opens(&store, &before, &Expect::Old(old));
    assert!(!store.join(REHASH_TMP_DIR).exists(), "the writable open removes it");
    rehash(&store, "sha256", false);
    assert_finished(&store, &before);
}

/// R-06: a stale `.seg.tmp` (a switch that a crash cut before its rename)
/// is ignored by the loaders and removed by the rerun.
#[test]
fn crash_with_a_stale_seg_tmp_then_rerun() {
    let dir = seeded_with_snapshot();
    let store = dir.path().join("store");
    let old = loader_roots(&store)[0];
    let before = record(&store);
    stopped_run(&store, Some(Step::Synced));
    let junk = store.join("segments").join("segment-999999.seg.tmp");
    std::fs::write(&junk, b"a switch segment cut by a crash").unwrap();
    assert_opens(&store, &before, &Expect::Old(old));
    rehash(&store, "sha256", false);
    assert!(!junk.exists());
    assert_finished(&store, &before);
}

/// R-06: old segments that a crash left after the rename of the switch
/// lose to the switch records (the loaders keep the last record of each
/// snapshot version); a rerun finishes.
#[test]
fn crash_with_old_segments_left_after_the_switch() {
    let dir = seeded_with_snapshot();
    let store = dir.path().join("store");
    let segs = store.join("segments");
    let before = record(&store);
    let old_segments: Vec<(String, Vec<u8>)> =
        segment_names(&store).into_iter().map(|n| { let b = std::fs::read(segs.join(&n)).unwrap(); (n, b) }).collect();
    stopped_run(&store, Some(Step::Switched));
    for (n, b) in &old_segments {
        assert!(!segs.join(n).exists(), "the switch removed {n}");
        std::fs::write(segs.join(n), b).unwrap();
    }
    assert!(segment_names(&store).len() > old_segments.len());
    assert_opens(&store, &before, &Expect::New);
    rehash(&store, "sha256", false);
    assert_finished(&store, &before);
}

/// The segment files of `store`: names and bytes.
fn segment_files(store: &Path) -> BTreeMap<String, Vec<u8>> {
    segment_names(store).into_iter().map(|n| { let b = std::fs::read(store.join("segments").join(&n)).unwrap(); (n, b) }).collect()
}

/// The verify error of a direct read of `id`, which must be `Corrupt`.
fn corrupt_cause(store: &Path, id: &ObjectId) -> String {
    let cs = content::open_read_only(store).unwrap();
    assert!(cs.contains(id), "the open keeps the damaged object in its index");
    match cs.read(id) {
        Err(e @ chunk_store::Error::Corrupt(_)) => e.to_string(),
        other => panic!("a read of the damaged object gives {other:?}, not Corrupt"),
    }
}

/// D-15, HASH-04: a damaged old object stops the run before the switch,
/// with the node kind and the verify error of that object; the dry-run
/// stops with it too.
#[test]
fn verify_a_damaged_object_stops_before_the_switch() {
    use chunk_store::ObjectHasher;
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let bytes = data(31, 256 << 10);
    std::fs::write(src.join("data"), &bytes).unwrap();
    let store = dir.path().join("store");
    run_seed(&store, &src, None).unwrap();

    // The largest chunk, in the largest pack file (the seed sealed it with
    // an .idx, so an open takes its entries from the .idx and does not cut
    // or drop the damaged record).
    let r = chunk_store::chunk_ranges(&bytes).into_iter().max_by_key(|r| r.len()).unwrap();
    assert!(r.len() >= 4096, "{r:?}");
    let id = chunk_store::Blake3_256.id(&bytes[r]);
    let pack = std::fs::read_dir(store.join("packs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "pack"))
        .max_by_key(|p| std::fs::metadata(p).unwrap().len())
        .unwrap();
    let mut p = std::fs::read(&pack).unwrap();
    let at = p.windows(id.0.len()).position(|w| w == id.0).expect("the record of the chunk");
    p[at + 100] ^= 0x5a;
    std::fs::write(&pack, &p).unwrap();
    let cause = corrupt_cause(&store, &id);
    assert!(cause.starts_with("damaged data: object"), "{cause}");
    let has_cause = |e: &str| e.contains(&cause);

    let segments = segment_files(&store);
    let e = refused(&store, "sha256", false);
    assert!(e.contains("Nothing was switched") && e.contains("file content") && has_cause(&e), "{e}");
    assert_eq!(segment_files(&store), segments, "no segment changes");
    assert!(marker(&store).contains("hash blake3-256\n"), "{}", marker(&store));
    assert_eq!(corrupt_cause(&store, &id), cause, "the run did not repair or drop the object");

    let e = refused(&store, "sha256", true);
    assert!(e.contains("file content") && has_cause(&e), "{e}");
}
