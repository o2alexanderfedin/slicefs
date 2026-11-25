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

mod common;

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
