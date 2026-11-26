//! The `slicefs rehash` command: an offline store converts to another
//! hash. Every live object gets the target tag, the current tree and the
//! snapshot trees do not change, scrub passes, and the old objects are
//! freed by the GC of the run.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chunk_store::file::file_objects;
use metadata::segment::load_store_from_segments;
use metadata::store::{content_keys, referenced_roots, DictMetadataStore};
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::content::{self, FORMAT, FORMAT_FILE, REHASH_TMP_DIR};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::header::{HASH_SHA256_FLAG, HASH_SHA256_RELEASE};
use slicefs_cli::rehash::{rehash_report_with, RehashArgs};
use slicefs_cli::seed::run_seed;
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

/// The tags of every object of the current root and of each snapshot root.
fn all_tags(store: &Path) -> BTreeSet<u8> {
    let (root, snaps) = load_store_from_segments(&store.join("segments")).unwrap();
    let mut io = content::store_io_read_only(store).unwrap();
    let cs = Arc::clone(io.store());
    let mut tags = BTreeSet::new();
    for r in root.into_iter().chain(snaps.iter().map(|s| s.root)) {
        let mut keys = referenced_roots(&mut io, &r).unwrap();
        keys.extend(content_keys(&mut io, &r).unwrap());
        for k in keys.iter().filter(|k| **k != ObjectKey::ZERO) {
            tags.extend(file_objects(&cs, content::to_id(k)).unwrap().iter().map(|id| id.tag()));
        }
    }
    tags
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
