//! Race (b) of Phase 19: a cross-directory rename had no ancestor check, so
//! a directory could move into its own subtree (`rename(a, a/b/c)`) or into
//! itself (`rename(a, a/x)`). That leaves a detached cycle: the directory
//! leaves the tree with all its children. The daemon must refuse it with
//! `EINVAL` and change nothing.
//!
//! The tests call the daemon path in process (`simulate_rename` and
//! `handlers::handle_rename`), not a mount: the Linux VFS may refuse this
//! rename before FUSE sees it, but FUSE-T, FSKit and in-process callers
//! reach the daemon (D-28).
//!
//! - The refusal tests show `EINVAL` and that no entry, no `..` and no link
//!   count changed.
//! - `the_root_is_its_own_parent`: the walk of `..` ends at the root.
//! - `moves_that_make_no_cycle_still_work`: the check refuses only cycles.
//! - `a_refused_rename_writes_no_record`: the write log gets no record.
//! - `stress_cross_directory_directory_renames_keep_one_tree`: threads move
//!   directories at the same time; the tree stays one tree (the check of
//!   this race that stays in CI, D-26).

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::{log_len, new_fs_with};
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::WalConfig;
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::handlers::{self as h, EmptyResult};
use slicefs_traits::metadata::MetadataStore;
use tempfile::TempDir;

const ROOT: u64 = 1;

fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let meta = DictMetadataStore::new(io.clone());
    let fs = SliceFsFilesystem::new(meta, io, None);
    (fs, dir)
}

fn mkdir(fs: &SliceFsFilesystem, parent: u64, name: &str) -> u64 {
    fs.simulate_mkdir(parent, name, 0o755, 0, 0, 0).unwrap()
}

fn lookup(fs: &SliceFsFilesystem, parent: u64, name: &str) -> Option<u64> {
    fs.meta().lookup(parent, name).ok()
}

fn nlinks(fs: &SliceFsFilesystem, ino: u64) -> u32 {
    fs.meta().get_inode(ino).unwrap().nlinks
}

/// `a` in the root, `b` in `a`. Returns `(a, b)`.
fn a_and_b(fs: &SliceFsFilesystem) -> (u64, u64) {
    let a = mkdir(fs, ROOT, "a");
    let b = mkdir(fs, a, "b");
    (a, b)
}

#[test]
fn a_directory_cannot_move_into_its_own_subtree() {
    let (fs, _dir) = fresh_fs();
    let (a, b) = a_and_b(&fs);
    let links = [nlinks(&fs, ROOT), nlinks(&fs, a), nlinks(&fs, b)];

    assert_eq!(fs.simulate_rename(ROOT, "a", b, "c", 0), Err(libc::EINVAL));

    assert_eq!(lookup(&fs, ROOT, "a"), Some(a));
    assert_eq!(lookup(&fs, a, "b"), Some(b));
    assert_eq!(lookup(&fs, b, "c"), None);
    assert_eq!(lookup(&fs, a, ".."), Some(ROOT));
    assert_eq!([nlinks(&fs, ROOT), nlinks(&fs, a), nlinks(&fs, b)], links);
}

#[test]
fn a_directory_cannot_move_into_itself() {
    let (fs, _dir) = fresh_fs();
    let (a, b) = a_and_b(&fs);
    let links = [nlinks(&fs, ROOT), nlinks(&fs, a), nlinks(&fs, b)];

    assert_eq!(fs.simulate_rename(ROOT, "a", a, "x", 0), Err(libc::EINVAL));

    assert_eq!(lookup(&fs, ROOT, "a"), Some(a));
    assert_eq!(lookup(&fs, a, "x"), None);
    assert_eq!(lookup(&fs, a, ".."), Some(ROOT));
    assert_eq!([nlinks(&fs, ROOT), nlinks(&fs, a), nlinks(&fs, b)], links);
}

#[test]
fn a_deep_move_into_the_subtree_is_refused() {
    let (fs, _dir) = fresh_fs();
    let (a, b) = a_and_b(&fs);
    let c = mkdir(&fs, b, "c");
    let d = mkdir(&fs, c, "d");

    assert_eq!(fs.simulate_rename(ROOT, "a", d, "e", 0), Err(libc::EINVAL));

    assert_eq!(lookup(&fs, ROOT, "a"), Some(a));
    assert_eq!(lookup(&fs, d, "e"), None);
    assert_eq!(lookup(&fs, a, ".."), Some(ROOT));
}

#[test]
fn the_root_is_its_own_parent() {
    let (fs, _dir) = fresh_fs();
    assert_eq!(lookup(&fs, ROOT, ".."), Some(ROOT));
}

#[test]
fn moves_that_make_no_cycle_still_work() {
    let (fs, _dir) = fresh_fs();
    let (a, b) = a_and_b(&fs);
    let c = mkdir(&fs, ROOT, "c");

    // A directory to a directory that is not under it.
    assert_eq!(fs.simulate_rename(a, "b", c, "b", 0), Ok(()));
    assert_eq!(lookup(&fs, c, "b"), Some(b));
    assert_eq!(lookup(&fs, b, ".."), Some(c));

    // A directory up to the root.
    assert_eq!(fs.simulate_rename(c, "b", ROOT, "b2", 0), Ok(()));
    assert_eq!(lookup(&fs, b, ".."), Some(ROOT));

    // A file across directories.
    let (f, _fh) = fs.test_create(a, "f", 0o644, 0, 0, 0).unwrap();
    assert_eq!(fs.simulate_rename(a, "f", c, "f", 0), Ok(()));
    assert_eq!(lookup(&fs, c, "f"), Some(f));

    // A directory renamed in the same directory.
    assert_eq!(fs.simulate_rename(ROOT, "a", ROOT, "a2", 0), Ok(()));
    assert_eq!(lookup(&fs, ROOT, "a2"), Some(a));
    assert_eq!(lookup(&fs, a, ".."), Some(ROOT));
}

#[test]
fn a_refused_rename_writes_no_record() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path();
    let fs = new_fs_with(store, WalConfig::PerOp);
    let (a, b) = a_and_b(&fs);
    let before = log_len(&fs, store);

    let r = h::handle_rename(&fs, ROOT, "a", b, "c", 0);
    assert!(matches!(r, EmptyResult::Error(libc::EINVAL)), "{r:?}");

    assert_eq!(log_len(&fs, store), before);
    assert_eq!(lookup(&fs, ROOT, "a"), Some(a));
    assert_eq!(lookup(&fs, b, "c"), None);
}

/// Seed of the stress test (printed in its assert messages).
const SEED: u64 = 0x21_02;
/// Directories of the stress tree, threads and renames per thread.
const DIRS: usize = 12;
const THREADS: u64 = 4;
const RENAMES: usize = 150;

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A small random source: the next value below `n`.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 = splitmix(self.0);
        (self.0 % n as u64) as usize
    }
}

/// The child directories of `dir` (no `.` and `..`); empty if `dir` is gone.
fn child_dirs(fs: &SliceFsFilesystem, dir: u64) -> Vec<u64> {
    let Ok(entries) = fs.meta().list_directory(dir) else { return Vec::new() };
    entries.into_iter()
        .filter(|e| e.name != "." && e.name != "..")
        .filter(|e| fs.meta().get_inode(e.ino).is_ok_and(|m| m.mode & 0o170_000 == 0o040_000))
        .map(|e| e.ino)
        .collect()
}

/// The parent of `dir` and the name of `dir` in it, read now.
fn place(fs: &SliceFsFilesystem, dir: u64) -> Option<(u64, String)> {
    let parent = fs.meta().lookup(dir, "..").ok()?;
    let entries = fs.meta().list_directory(parent).ok()?;
    entries.into_iter().find(|e| e.ino == dir && e.name != "." && e.name != "..").map(|e| (parent, e.name))
}

/// Many threads move directories across directories at the same time; half
/// of the targets are in the subtree of the moved directory, so the refused
/// case is common. After the join the tree is still one tree: every
/// directory is reachable from the root once, and its `..` chain ends at the
/// root. Without the ancestor check a directory leaves the tree as a cycle.
#[test]
fn stress_cross_directory_directory_renames_keep_one_tree() {
    let (fs, _dir) = fresh_fs();
    let fs = Arc::new(fs);

    // A seeded tree of `DIRS` directories, depth 1 to 3.
    let mut rng = Rng(SEED);
    let mut dirs: Vec<(u64, usize)> = Vec::new(); // (ino, depth)
    for i in 0..DIRS {
        let shallow: Vec<(u64, usize)> = dirs.iter().copied().filter(|&(_, d)| d < 3).collect();
        let (parent, depth) = if i < 3 || rng.below(4) == 0 { (ROOT, 0) } else { shallow[rng.below(shallow.len())] };
        dirs.push((mkdir(&fs, parent, &format!("d{i}")), depth + 1));
    }
    let inos: Arc<Vec<u64>> = Arc::new(dirs.iter().map(|&(ino, _)| ino).collect());

    let workers: Vec<_> = (0..THREADS).map(|t| {
        let (fs, inos) = (fs.clone(), inos.clone());
        std::thread::spawn(move || {
            let mut rng = Rng(SEED ^ (t + 1).wrapping_mul(0x9E37_79B9));
            let mut odd = Vec::new();
            for k in 0..RENAMES {
                let x = inos[rng.below(inos.len())];
                let Some((parent, name)) = place(&fs, x) else { continue };
                let y = if rng.below(2) == 0 {
                    // A target in the subtree of `x` (or `x` itself).
                    let mut y = x;
                    for _ in 0..rng.below(4) {
                        let kids = child_dirs(&fs, y);
                        if kids.is_empty() { break; }
                        y = kids[rng.below(kids.len())];
                    }
                    y
                } else {
                    let pick = rng.below(inos.len() + 1);
                    if pick == inos.len() { ROOT } else { inos[pick] }
                };
                let r = fs.simulate_rename(parent, &name, y, &format!("t{t}r{k}"), 0);
                match r {
                    Ok(()) => {}
                    Err(e) if [libc::EINVAL, libc::ENOENT, libc::EEXIST, libc::ENOTEMPTY].contains(&e) => {}
                    Err(e) => odd.push((x, y, e)),
                }
            }
            odd
        })
    }).collect();
    for w in workers {
        let odd = w.join().unwrap();
        assert!(odd.is_empty(), "seed {SEED:#x}: unexpected errno (x, y, errno): {odd:?}");
    }

    // Every directory is reachable from the root, each exactly once.
    let mut seen: HashMap<u64, usize> = HashMap::new();
    let mut todo = vec![ROOT];
    while let Some(dir) = todo.pop() {
        for kid in child_dirs(&fs, dir) {
            let n = seen.entry(kid).or_insert(0);
            *n += 1;
            if *n == 1 { todo.push(kid); }
        }
    }
    for &ino in inos.iter() {
        assert_eq!(seen.get(&ino).copied(), Some(1), "seed {SEED:#x}: directory {ino} is reached from the root {:?} times", seen.get(&ino));
    }

    // From each directory the `..` chain ends at the root.
    for &ino in inos.iter() {
        let mut cur = ino;
        for _ in 0..DIRS {
            if cur == ROOT { break; }
            cur = fs.meta().lookup(cur, "..").unwrap();
        }
        assert_eq!(cur, ROOT, "seed {SEED:#x}: the `..` chain of directory {ino} does not reach the root in {DIRS} steps");
    }
}
