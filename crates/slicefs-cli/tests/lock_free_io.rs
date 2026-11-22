//! Readers and writers of a file do not wait for the I/O of a rebase step
//! (Phase 19, D-02, ROADMAP criterion 1): the step copies its extents under
//! the cell lock, chunks, hashes and puts with no cell lock held, and
//! publishes only if the overlay did not change meanwhile.
//!
//! The parked tests hold no commit gate and run no commit thread while a
//! step is parked (RESEARCH Pitfall 8).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_cli::filesystem::SliceFsFilesystem;
use tempfile::TempDir;

const KIB: usize = 1 << 10;
const WAIT: Duration = Duration::from_secs(5);

fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let meta = DictMetadataStore::new(io.clone());
    let fs = SliceFsFilesystem::new(meta, io, None);
    (fs, dir)
}

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

fn put(model: &mut Vec<u8>, off: usize, bytes: &[u8]) {
    if model.len() < off + bytes.len() {
        model.resize(off + bytes.len(), 0);
    }
    model[off..off + bytes.len()].copy_from_slice(bytes);
}

/// A 1 MiB file, closed, then open read-write in overlay mode with a dirty
/// limit of 64 KiB. Returns the inode, the handle and the model.
fn overlay_file(fs: &SliceFsFilesystem) -> (u64, u64, Vec<u8>) {
    let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
    let mut model = seeded(1, 1 << 20);
    fs.test_write(fh, 0, &model).unwrap();
    fs.test_release(ino, fh).unwrap();
    fs.test_set_dirty_limit(64 * KIB);
    let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let a = seeded(2, 4 * KIB);
    fs.test_write(fh, 512 * KIB as u64, &a).unwrap();
    put(&mut model, 512 * KIB, &a);
    (ino, fh, model)
}

/// A rebase hook that parks its first call until `go` (at most 10 s) and
/// tells `parked` when it gets there. Later calls do not park.
fn park_first(fs: &SliceFsFilesystem) -> (Receiver<()>, Sender<()>) {
    let (parked_tx, parked_rx) = channel();
    let (go_tx, go_rx) = channel::<()>();
    let (first, go_rx) = (AtomicBool::new(true), Mutex::new(go_rx));
    fs.test_set_rebase_hook(Some(Arc::new(move || {
        if first.swap(false, Ordering::SeqCst) {
            parked_tx.send(()).ok();
            let _ = go_rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
        }
    })));
    (parked_rx, go_tx)
}

#[test]
fn reader_does_not_wait_for_rebase_io() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh, mut model) = overlay_file(&fs);
    let (parked, go) = park_first(&fs);
    let big = seeded(3, 128 * KIB);
    put(&mut model, 256 * KIB, &big);
    std::thread::scope(|s| {
        let w = s.spawn(|| fs.test_write(fh, 256 * KIB as u64, &big));
        parked.recv_timeout(WAIT).expect("the rebase reached its I/O phase");
        let (tx, rx) = channel();
        let fs = &fs;
        s.spawn(move || tx.send(fs.test_read(ino, 256 * KIB as u64, 4096)).ok());
        let got = match rx.recv_timeout(WAIT) {
            Ok(r) => r,
            Err(e) => {
                go.send(()).ok();
                panic!("the read waited for the parked rebase: {e:?}");
            }
        };
        assert!(got.unwrap() == model[256 * KIB..260 * KIB], "the read sees the whole write");
        go.send(()).unwrap();
        assert_eq!(w.join().unwrap(), Ok(big.len() as u32));
    });
    fs.test_set_rebase_hook(None);
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content with the handle open");
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content after release");
}

#[test]
fn writer_does_not_wait_for_rebase_io() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh, mut model) = overlay_file(&fs);
    let (other, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let (parked, go) = park_first(&fs);
    let big = seeded(4, 128 * KIB);
    let patch = seeded(5, 4 * KIB);
    put(&mut model, 256 * KIB, &big);
    put(&mut model, 768 * KIB, &patch);
    std::thread::scope(|s| {
        let w = s.spawn(|| fs.test_write(fh, 256 * KIB as u64, &big));
        parked.recv_timeout(WAIT).expect("the rebase reached its I/O phase");
        let (tx, rx) = channel();
        let (fs, patch) = (&fs, &patch);
        s.spawn(move || tx.send(fs.test_write(other, 768 * KIB as u64, patch)).ok());
        let got = match rx.recv_timeout(WAIT) {
            Ok(r) => r,
            Err(e) => {
                go.send(()).ok();
                panic!("the write waited for the parked rebase: {e:?}");
            }
        };
        assert_eq!(got, Ok(patch.len() as u32));
        go.send(()).unwrap();
        assert_eq!(w.join().unwrap(), Ok(big.len() as u32));
    });
    fs.test_set_rebase_hook(None);
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "both writes with the handles open");
    fs.test_release(ino, other).unwrap();
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "both writes after release");
}

/// One rebase per cell: a second writer over the limit while a step is in
/// its I/O phase skips its own step and returns at once.
#[test]
fn a_second_writer_skips_its_step_while_one_is_in_flight() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh, mut model) = overlay_file(&fs);
    let (other, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let (parked, go) = park_first(&fs);
    let big = seeded(6, 128 * KIB);
    let more = seeded(7, 128 * KIB);
    put(&mut model, 0, &big);
    put(&mut model, 640 * KIB, &more);
    std::thread::scope(|s| {
        let w = s.spawn(|| fs.test_write(fh, 0, &big));
        parked.recv_timeout(WAIT).expect("the rebase reached its I/O phase");
        let before = fs.test_rebase_count();
        let (tx, rx) = channel();
        let (fs, more) = (&fs, &more);
        s.spawn(move || tx.send(fs.test_write(other, 640 * KIB as u64, more)).ok());
        let got = match rx.recv_timeout(WAIT) {
            Ok(r) => r,
            Err(e) => {
                go.send(()).ok();
                panic!("the second writer waited: {e:?}");
            }
        };
        assert_eq!(got, Ok(more.len() as u32));
        assert_eq!(fs.test_rebase_count(), before, "the second writer ran no step");
        assert!(fs.test_dirty_cost(ino) > 64 * KIB, "the second writer stays over the limit for now");
        go.send(()).unwrap();
        assert_eq!(w.join().unwrap(), Ok(big.len() as u32));
    });
    fs.test_set_rebase_hook(None);
    fs.test_release(ino, other).unwrap();
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content after release");
}

/// A dropped step (the overlay changed in its I/O phase) is tried again at
/// once with the cell locked, and that attempt publishes.
#[test]
fn a_dropped_step_is_retried_with_the_cell_locked() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh, mut model) = overlay_file(&fs);
    let (other, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let (parked, go) = park_first(&fs);
    let big = seeded(8, 128 * KIB);
    let patch = seeded(9, 4 * KIB);
    put(&mut model, 256 * KIB, &big);
    put(&mut model, 768 * KIB, &patch);
    let before = fs.test_rebase_count();
    std::thread::scope(|s| {
        let w = s.spawn(|| fs.test_write(fh, 256 * KIB as u64, &big));
        parked.recv_timeout(WAIT).expect("the rebase reached its I/O phase");
        // A change in the I/O phase: the parked result must be dropped.
        let r = fs.test_write(other, 768 * KIB as u64, &patch);
        go.send(()).unwrap();
        assert_eq!(r, Ok(patch.len() as u32));
        assert_eq!(w.join().unwrap(), Ok(big.len() as u32));
    });
    assert_eq!(fs.test_rebase_count(), before + 2, "the dropped attempt and the locked retry");
    assert!(fs.test_dirty_cost(ino) <= 64 * KIB, "the retry published: cost {}", fs.test_dirty_cost(ino));
    fs.test_set_rebase_hook(None);
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content with the handles open");
    fs.test_release(ino, other).unwrap();
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content after release");
}

/// RESEARCH Pitfall 1 (#98): 4 threads write one file at a dirty limit of
/// 256 KiB; the steps keep dropping each other's results, but the dirty
/// cost of the file never passes `limit + 2 * REBASE_STEP`.
#[test]
fn dirty_cost_stays_under_the_hard_cap() {
    use slicefs_cli::filesystem::REBASE_STEP;
    const LIMIT: usize = 256 * KIB;
    const BLOCK: usize = 4 * KIB;
    const BLOCKS: usize = (2 << 20) / BLOCK;
    let cap = LIMIT + 2 * REBASE_STEP;
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
    let mut model = seeded(10, BLOCKS * BLOCK);
    fs.test_write(fh, 0, &model).unwrap();
    fs.test_release(ino, fh).unwrap();
    fs.test_set_dirty_limit(LIMIT);
    // A short sleep in each I/O phase widens the window for concurrent writes.
    fs.test_set_rebase_hook(Some(Arc::new(|| std::thread::sleep(Duration::from_millis(2)))));
    let handles: Vec<u64> = (0..4).map(|_| fs.test_open(ino, libc::O_RDWR).unwrap().0).collect();
    let last: Vec<Vec<(usize, Vec<u8>)>> = std::thread::scope(|s| {
        let fs = &fs;
        let runs: Vec<_> = handles.iter().enumerate().map(|(t, &h)| s.spawn(move || {
            // Thread t owns the blocks b with b % 4 == t.
            let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ t as u64;
            let mut done = Vec::new();
            for i in 0..200 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let b = (x as usize % (BLOCKS / 4)) * 4 + t;
                let data = seeded(1000 * t as u64 + i, BLOCK);
                assert_eq!(fs.test_write(h, (b * BLOCK) as u64, &data), Ok(BLOCK as u32));
                let cost = fs.test_dirty_cost(ino);
                assert!(cost <= cap, "thread {t} write {i}: dirty cost {cost} > cap {cap}");
                done.push((b, data));
            }
            done
        })).collect();
        runs.into_iter().map(|r| r.join().unwrap()).collect()
    });
    fs.test_set_rebase_hook(None);
    for (b, data) in last.into_iter().flatten() {
        model[b * BLOCK..(b + 1) * BLOCK].copy_from_slice(&data);
    }
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content with the handles open");
    for h in handles {
        fs.test_release(ino, h).unwrap();
    }
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content after release");
}
