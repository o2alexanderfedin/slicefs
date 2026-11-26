//! Race (a) of Phase 19 (items 19-09 and 19-10): a size change of a closed
//! file against an unlink and against a setattr of other fields.
//!
//! Window 1: an unlink that reclaims the inode while the size change splices
//! its content. Measured: the result of the size change, the metadata of the
//! deleted inode (no manifest, no inode, no orphan), the manifest and the
//! bytes of another file that shares the old root, and the replay of the log
//! against the live state. The refcount asserts are stats evidence only: GC
//! is reachability, so a wrong count frees no content that a file uses.
//!
//! Window 2 (the fields setattr in the claim window) needs the log closures of
//! private functions: it is the in-crate test
//! `a_fields_setattr_in_the_claim_window_keeps_the_record_order`.
//!
//! `stress_size_changes_and_unlinks_of_closed_files` is the short stress test
//! of window 1 that stays in the default suite (D-21, D-26).

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use common::{dump, new_fs_with, reload_and_replay};
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::WalConfig;
use slicefs_cli::filesystem::{RebaseHook, SliceFsFilesystem};
use slicefs_cli::handlers::{self as h, AttrResult, CreateResult, EmptyResult, WriteResult};
use slicefs_cli::oplog;
use slicefs_traits::metadata::MetadataStore;
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;
const WAIT: Duration = Duration::from_secs(10);
/// Size of the shared content: the size change splices it.
const LEN: usize = 64 << 10;

fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let meta = DictMetadataStore::new(io.clone());
    (SliceFsFilesystem::new(meta, io, None), dir)
}

/// Seeded bytes (xorshift).
fn seeded(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

/// A closed file `name` in the root with `data`.
fn closed_file(fs: &SliceFsFilesystem, name: &str, data: &[u8]) -> u64 {
    let (ino, fh) = fs.test_create(1, name, 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, data).unwrap();
    fs.test_release(ino, fh).unwrap();
    ino
}

/// A hook that parks its first call until `go` (at most 10 s) and tells
/// `parked` when it gets there. Later calls do not park.
fn parker() -> (RebaseHook, Receiver<()>, Sender<()>) {
    let (parked_tx, parked_rx) = channel();
    let (go_tx, go_rx) = channel::<()>();
    let (n, go_rx) = (AtomicUsize::new(0), Mutex::new(go_rx));
    let hook: RebaseHook = Arc::new(move || {
        if n.fetch_add(1, Ordering::SeqCst) == 0 {
            parked_tx.send(()).ok();
            let _ = go_rx.lock().unwrap().recv_timeout(WAIT);
        }
    });
    (hook, parked_rx, go_tx)
}

/// The ops of the write log of `store` so far (the log is synced first).
fn logged_ops(fs: &SliceFsFilesystem, store: &Path) -> Vec<oplog::Op> {
    fs.meta().flush_wal().unwrap();
    let (_, _, log) = metadata::segment::scan_store_with_log(&store.join("segments")).unwrap();
    log.records().map(|r| oplog::Record::decode(r.as_ref().unwrap()).unwrap().op).collect()
}

/// Window 1: the unlink of `f` runs while the size change of `f` is parked
/// after its splice (the rebase hook, before the install). `g` has the same
/// content, so both files share one root.
#[test]
fn an_unlink_inside_the_splice_keeps_the_shared_content() {
    let (fs, _dir) = fresh_fs();
    let data = seeded(7, LEN);
    let f = closed_file(&fs, "f", &data);
    let g = closed_file(&fs, "g", &data);
    let root = fs.meta().get_manifest(f).unwrap()[0];
    assert_eq!(fs.meta().get_manifest(g).unwrap(), vec![root], "precondition: one shared root");
    assert_eq!(fs.meta().get_refcount(&root), 2, "precondition: two references");

    let (hook, parked, go) = parker();
    fs.test_set_rebase_hook(Some(hook));
    let r = std::thread::scope(|sc| {
        let a = sc.spawn(|| fs.test_setattr_size(f, None, 1024));
        parked.recv_timeout(WAIT).expect("the size change reaches its install");
        // A second thread: the size change holds its cell (Pitfall 9).
        fs.simulate_unlink(1, "f").unwrap();
        go.send(()).unwrap();
        a.join().unwrap()
    });
    fs.test_set_rebase_hook(None);

    assert_eq!(r, Ok(()), "the size change of a file unlinked in its splice");
    assert!(fs.meta().get_manifest(f).is_err(), "no manifest of the deleted inode");
    assert!(fs.meta().get_inode(f).is_err(), "the deleted inode is gone");
    assert_eq!(fs.test_orphan_count(), 0, "no orphan is left");
    assert_eq!(fs.meta().get_manifest(g).unwrap(), vec![root], "g keeps its manifest");
    assert!(fs.test_read(g, 0, LEN as u32 + 1).unwrap() == data, "g reads back");
    // Stats evidence only (the table goes with #165): one release of the
    // old root by the size change, none by the reclaim of its new root.
    assert_eq!(fs.meta().get_refcount(&root), 1, "refcount stats of the shared root");
}

/// The window 1 sequence through the logging handlers, then a crash: the
/// log (the `Unlink` before the size `Setattr`) replays to the live state.
#[test]
fn an_unlink_inside_the_splice_replays_to_the_live_state() {
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs_with(dir.path(), WalConfig::PerOp);
    let data = seeded(8, LEN);
    let make = |name: &str| {
        let (ino, fh) = match h::handle_create(&fs, 1, name, S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR) {
            CreateResult::Ok(ino, fh, _) => (ino, fh),
            CreateResult::Error(e) => panic!("create errno {e}"),
        };
        assert!(matches!(h::handle_write(&fs, fh, 0, &data), WriteResult::Ok(_)));
        assert!(matches!(h::handle_release(&fs, ino, fh), EmptyResult::Ok));
        ino
    };
    let f = make("f");
    let g = make("g");
    fs.meta().commit().unwrap();

    let (hook, parked, go) = parker();
    fs.test_set_rebase_hook(Some(hook));
    let r = std::thread::scope(|sc| {
        let a = sc.spawn(|| h::handle_setattr(&fs, f, None, None, None, Some(1024), None, None, None));
        parked.recv_timeout(WAIT).expect("the size change reaches its install");
        assert!(matches!(h::handle_unlink(&fs, 1, "f"), EmptyResult::Ok));
        go.send(()).unwrap();
        a.join().unwrap()
    });
    fs.test_set_rebase_hook(None);

    // The size part succeeds; the fields part of the same request then finds
    // the inode reclaimed (ENOENT). Before the fix the size part gave EIO.
    assert!(matches!(r, AttrResult::Ok(_) | AttrResult::Error(libc::ENOENT)), "setattr result {r:?}");
    let ops = logged_ops(&fs, dir.path());
    let unlink = ops.iter().position(|op| matches!(op, oplog::Op::Unlink { name, .. } if name == "f"));
    let size = ops.iter().position(|op| matches!(op, oplog::Op::Setattr { ino, size: Some(1024), .. } if *ino == f));
    assert!(matches!((unlink, size), (Some(u), Some(s)) if u < s), "Unlink before the size Setattr: {ops:?}");

    let live = dump(&fs);
    std::mem::forget(fs); // a crash: no commit
    let fs = reload_and_replay(dir.path());
    assert_eq!(dump(&fs), live, "replay gives the live state");
    assert!(fs.test_read(g, 0, LEN as u32 + 1).unwrap() == data, "g reads back after replay");
}

/// Seed of the stress test (printed in its assert messages).
const STRESS_SEED: u64 = 0x21_0204;

/// Short stress test that stays in the default suite (D-21, D-26): size
/// changes of a closed file against one unlink of it. Each round has a file
/// `f<k>` and a file `g<k>` with the same content (one shared root); thread X
/// changes the size of `f<k>` 20 times, thread Y unlinks it once.
#[test]
fn stress_size_changes_and_unlinks_of_closed_files() {
    let (fs, _dir) = fresh_fs();
    let mut x = STRESS_SEED;
    for k in 0..8u64 {
        let (fname, gname) = (format!("f{k}"), format!("g{k}"));
        let data = seeded(STRESS_SEED ^ k, LEN);
        let f = closed_file(&fs, &fname, &data);
        let g = closed_file(&fs, &gname, &data);
        let root = fs.meta().get_manifest(g).unwrap()[0];
        let sizes: Vec<u64> = (0..20).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x % (2 * LEN as u64) }).collect();
        let barrier = Barrier::new(2);
        std::thread::scope(|sc| {
            let xs = sc.spawn(|| {
                barrier.wait();
                sizes.iter().map(|&s| fs.test_setattr_size(f, None, s)).collect::<Vec<_>>()
            });
            barrier.wait();
            fs.simulate_unlink(1, &fname).unwrap();
            for (i, r) in xs.join().unwrap().into_iter().enumerate() {
                assert!(matches!(r, Ok(()) | Err(libc::ENOENT)), "seed {STRESS_SEED:#x} round {k} change {i}: {r:?}");
            }
        });
        let at = format!("seed {STRESS_SEED:#x} round {k}");
        assert_eq!(fs.test_orphan_count(), 0, "{at}: no orphan is left");
        assert!(fs.meta().get_manifest(f).is_err(), "{at}: no manifest of the deleted inode");
        assert!(fs.meta().get_inode(f).is_err(), "{at}: the deleted inode is gone");
        assert_eq!(fs.meta().get_manifest(g).unwrap(), vec![root], "{at}: g keeps its manifest");
        assert!(fs.test_read(g, 0, LEN as u32 + 1).unwrap() == data, "{at}: g reads back");
        // Stats evidence only (#165): g still counts its root.
        assert!(fs.meta().get_refcount(&root) >= 1, "{at}: refcount stats of the shared root");
    }
}
