//! Lock wait and hold rows for ROADMAP Phase 19 criteria 1 and 2 (D-11).
//!
//! A writer's rebase step on file F is parked in its I/O phase (Phase B, no
//! cell lock) for at least 300 ms: the rebase hook is the injected writer
//! I/O. While it is parked, readers of F (criterion 1) or requests on another
//! file Y (criterion 2) run through the request handlers, and every one of
//! them ends before the writer goes on. Then the lock rows must show:
//! - a wait under 100 ms at each checked site (a lock held across the parked
//!   I/O would make a waiter wait about 300 ms or more), and
//! - a hold under 300 ms at every site (no lock is held across the I/O).
//!
//! A separate test binary: the lock recorder is process-global. The two
//! tests take one static mutex, so their counts do not mix.
#![cfg(feature = "lock-stats")]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chunk_store::lock_stats::{self, SiteStats};
use metadata::wal::WalConfig;
use slicefs_cli::filesystem::{RebaseHook, SliceFsFilesystem};
use slicefs_cli::handlers::{self as h, CreateResult, DataResult, EmptyResult, OpenResult, WriteResult};
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;
const ROOT: u64 = 1;
const KIB: usize = 1 << 10;
/// The injected writer I/O: the parked step lasts at least this long.
const HELD: Duration = Duration::from_millis(300);
/// The bound on a wait (a third of `HELD`).
const BOUND: Duration = Duration::from_millis(100);
/// No unbounded wait: the hook goes on by itself after this.
const WAIT: Duration = Duration::from_secs(5);

static ALONE: Mutex<()> = Mutex::new(());

fn seeded(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn create(fs: &SliceFsFilesystem, name: &str) -> (u64, u64) {
    match h::handle_create(fs, ROOT, name, S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR) {
        CreateResult::Ok(ino, fh, _) => (ino, fh),
        CreateResult::Error(e) => panic!("create {name} errno {e}"),
    }
}

fn write(fs: &SliceFsFilesystem, fh: u64, off: usize, data: &[u8]) {
    match h::handle_write(fs, fh, off as u64, data) {
        WriteResult::Ok(n) => assert_eq!(n as usize, data.len(), "short write"),
        WriteResult::Error(e) => panic!("write errno {e}"),
    }
}

fn read(fs: &SliceFsFilesystem, ino: u64, off: usize, n: usize) -> Vec<u8> {
    match h::handle_read(fs, ino, off as u64, n as u32) {
        DataResult::Ok(d) => d,
        DataResult::Error(e) => panic!("read errno {e}"),
    }
}

fn release(fs: &SliceFsFilesystem, ino: u64, fh: u64) {
    assert!(matches!(h::handle_release(fs, ino, fh), EmptyResult::Ok), "release");
}

/// A store with a per-operation write log. File `f`: 1 MiB, closed, then
/// open read-write in overlay mode with a dirty limit of 64 KiB, so a 128
/// KiB write runs a rebase step. Returns the store dir, the fs, the inode
/// and the handle.
fn overlay_file() -> (TempDir, SliceFsFilesystem, u64, u64) {
    let dir = TempDir::new().unwrap();
    let fs = common::new_fs_with(dir.path(), WalConfig::PerOp);
    let (ino, fh) = create(&fs, "f");
    write(&fs, fh, 0, &seeded(1, 1 << 20));
    release(&fs, ino, fh);
    fs.test_set_dirty_limit(64 * KIB);
    let fh = match h::handle_open(&fs, ino, libc::O_RDWR) {
        OpenResult::Ok(fh, _) => fh,
        OpenResult::Error(e) => panic!("open errno {e}"),
    };
    write(&fs, fh, 512 * KIB, &seeded(2, 4 * KIB));
    (dir, fs, ino, fh)
}

/// A rebase hook that parks its first call until `go` (at most `WAIT`),
/// tells `parked` when it gets there, and sets `out` when it goes on. Later
/// calls (finalize, with the cell locked) do not park.
fn held_hook() -> (RebaseHook, Receiver<()>, Sender<()>, Arc<AtomicBool>) {
    let (parked_tx, parked_rx) = channel();
    let (go_tx, go_rx) = channel::<()>();
    let out = Arc::new(AtomicBool::new(false));
    let (first, go_rx, out2) = (AtomicBool::new(true), Mutex::new(go_rx), out.clone());
    let hook: RebaseHook = Arc::new(move || {
        if first.swap(false, Ordering::SeqCst) {
            parked_tx.send(()).ok();
            let _ = go_rx.lock().unwrap().recv_timeout(WAIT);
            out2.store(true, Ordering::SeqCst);
        }
    });
    (hook, parked_rx, go_tx, out)
}

/// Let the parked step go on, but not before it has been parked `HELD`.
fn go_after_held(parked_at: Instant, go: &Sender<()>) {
    if let Some(rest) = HELD.checked_sub(parked_at.elapsed()) {
        std::thread::sleep(rest);
    }
    go.send(()).ok();
}

fn print_rows(test: &str, snap: &[SiteStats]) {
    println!("rows {test}");
    for s in snap {
        println!(
            "lock site={} n={} wait_max_us={:.1} hold_max_us={:.1}",
            s.label,
            s.count,
            s.wait_max_ns as f64 / 1000.0,
            s.hold_max_ns as f64 / 1000.0
        );
    }
}

fn site<'a>(snap: &'a [SiteStats], label: &str) -> &'a SiteStats {
    snap.iter().find(|s| s.label == label).unwrap_or_else(|| panic!("no site {label}"))
}

/// No site was held across the parked I/O.
fn assert_no_hold_across(snap: &[SiteStats]) {
    for s in snap {
        assert!(
            s.hold_max_ns < HELD.as_nanos() as u64,
            "lock site {} was held {} us, across the parked writer I/O",
            s.label,
            s.hold_max_ns / 1000
        );
    }
}

/// Criterion 1: two readers of F read 50 times each while the writer's
/// rebase I/O on F is parked; no read waits for it.
#[test]
fn readers_do_not_wait_for_writer_io_rows() {
    let _alone = ALONE.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, fs, ino, fh) = overlay_file();
    let base = seeded(1, 1 << 20);
    let big = seeded(3, 128 * KIB);
    let (hook, parked, go, out) = held_hook();
    fs.test_set_rebase_hook(Some(hook));
    lock_stats::reset();
    let slowest = std::thread::scope(|s| {
        let w = s.spawn(|| write(&fs, fh, 256 * KIB, &big));
        parked.recv_timeout(WAIT).expect("the rebase reached its I/O phase");
        let parked_at = Instant::now();
        let readers: Vec<_> = (0..2)
            .map(|_| {
                s.spawn(|| {
                    let mut slowest = Duration::ZERO;
                    for i in 0..50 {
                        // Even reads: the range of the parked write (overlay).
                        // Odd reads: stored content (through `readers`).
                        let (off, want) = if i % 2 == 0 { (256 * KIB, &big[..4 * KIB]) } else { (768 * KIB, &base[768 * KIB..772 * KIB]) };
                        let t0 = Instant::now();
                        let got = read(&fs, ino, off, 4 * KIB);
                        slowest = slowest.max(t0.elapsed());
                        assert!(got == want, "read at {off}: the whole write or the stored bytes");
                    }
                    slowest
                })
            })
            .collect();
        let slowest = readers.into_iter().map(|r| r.join()).collect::<Vec<_>>();
        let parked_still = !out.load(Ordering::SeqCst);
        go_after_held(parked_at, &go);
        w.join().expect("writer panicked");
        assert!(parked_still, "the reads did not end while the writer's I/O was parked");
        slowest.into_iter().map(|r| r.expect("reader panicked")).max().unwrap()
    });
    let snap = lock_stats::snapshot();
    fs.test_set_rebase_hook(None);
    print_rows("readers_do_not_wait_for_writer_io", &snap);
    println!("slowest_read_us={:.1}", slowest.as_nanos() as f64 / 1000.0);
    assert!(slowest < BOUND, "a read took {slowest:?} while the writer's I/O was parked");
    for label in ["file_cell", "readers"] {
        let s = site(&snap, label);
        assert!(s.count > 0, "no samples for lock site {label}");
        assert!(s.wait_max_ns < BOUND.as_nanos() as u64, "lock site {label} waited {} us", s.wait_max_ns / 1000);
    }
    assert_no_hold_across(&snap);
    release(&fs, ino, fh);
}

/// Criterion 2: while the rebase I/O of file X is parked, one thread writes,
/// reads and fsyncs file Y 20 times; no lock site waits for X.
#[test]
fn other_files_do_not_wait_rows() {
    let _alone = ALONE.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, fs, x, fx) = overlay_file();
    let (y, fy) = create(&fs, "y");
    let big = seeded(4, 128 * KIB);
    let (hook, parked, go, out) = held_hook();
    fs.test_set_rebase_hook(Some(hook));
    lock_stats::reset();
    let slowest = std::thread::scope(|s| {
        let w = s.spawn(|| write(&fs, fx, 256 * KIB, &big));
        parked.recv_timeout(WAIT).expect("the rebase of X reached its I/O phase");
        let parked_at = Instant::now();
        let other = s.spawn(|| {
            let mut slowest = Duration::ZERO;
            for i in 0..20 {
                let piece = seeded(100 + i as u64, 4 * KIB);
                let t0 = Instant::now();
                write(&fs, fy, i * 4 * KIB, &piece);
                let got = read(&fs, y, i * 4 * KIB, 4 * KIB);
                assert!(matches!(h::handle_fsync(&fs, y, fy), EmptyResult::Ok), "fsync of Y");
                slowest = slowest.max(t0.elapsed());
                assert!(got == piece, "Y reads back its write");
            }
            slowest
        });
        let slowest = other.join();
        let parked_still = !out.load(Ordering::SeqCst);
        go_after_held(parked_at, &go);
        w.join().expect("writer of X panicked");
        assert!(parked_still, "the requests on Y did not end while the I/O of X was parked");
        slowest.expect("the thread on Y panicked")
    });
    let snap = lock_stats::snapshot();
    fs.test_set_rebase_hook(None);
    print_rows("other_files_do_not_wait", &snap);
    // Printed only: a round includes a disk sync, whose time is not a lock wait.
    println!("slowest_y_round_us={:.1}", slowest.as_nanos() as f64 / 1000.0);
    for s in &snap {
        assert!(s.wait_max_ns < BOUND.as_nanos() as u64, "lock site {} waited {} us", s.label, s.wait_max_ns / 1000);
    }
    for label in ["file_cell", "wal_queue", "inode_live"] {
        assert!(site(&snap, label).count > 0, "no samples for lock site {label}");
    }
    assert_no_hold_across(&snap);
    release(&fs, x, fx);
    release(&fs, y, fy);
}
