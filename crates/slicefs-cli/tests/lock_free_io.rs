//! Readers and writers of a file do not wait for the I/O of a rebase step
//! (Phase 19, D-02, ROADMAP criterion 1): the step copies its extents under
//! the cell lock, chunks, hashes and puts with no cell lock held, and
//! publishes only if the overlay did not change meanwhile.
//!
//! Streaming writes (Plan 07) follow the same protocol: the stream is cloned
//! under the cell lock, pushed with no cell lock, and swapped in only if it
//! did not change meanwhile.
//!
//! The parked tests hold no commit gate and run no commit thread while a
//! step is parked (RESEARCH Pitfall 8).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_cli::filesystem::{RebaseHook, SliceFsFilesystem};
use slicefs_traits::metadata::MetadataStore;
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

/// A hook that parks each call whose number (from 1) is in `calls` until
/// `go` (at most 10 s per call) and tells `parked` when it gets there.
fn parker_at(calls: &'static [usize]) -> (RebaseHook, Receiver<()>, Sender<()>) {
    let (parked_tx, parked_rx) = channel();
    let (go_tx, go_rx) = channel::<()>();
    let (n, go_rx) = (AtomicUsize::new(0), Mutex::new(go_rx));
    let hook: RebaseHook = Arc::new(move || {
        if calls.contains(&(n.fetch_add(1, Ordering::SeqCst) + 1)) {
            parked_tx.send(()).ok();
            let _ = go_rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
        }
    });
    (hook, parked_rx, go_tx)
}

/// A hook that parks its first call (see `parker_at`). Later calls do not park.
fn parker() -> (RebaseHook, Receiver<()>, Sender<()>) {
    parker_at(&[1])
}

/// A rebase hook that parks its first call (see `parker`).
fn park_first(fs: &SliceFsFilesystem) -> (Receiver<()>, Sender<()>) {
    let (hook, parked, go) = parker();
    fs.test_set_rebase_hook(Some(hook));
    (parked, go)
}

/// A push hook that parks its first call (see `parker`).
fn park_first_push(fs: &SliceFsFilesystem) -> (Receiver<()>, Sender<()>) {
    let (hook, parked, go) = parker();
    fs.test_set_push_hook(Some(hook));
    (parked, go)
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

/// A dropped step (the overlay changed in its I/O phase) is tried again
/// with the cell unlocked: a read passes while the retry is parked in its
/// I/O phase, and the retry publishes.
#[test]
fn a_dropped_step_is_retried_unlocked() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh, mut model) = overlay_file(&fs);
    let (other, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let (hook, parked, go) = parker_at(&[1, 2]);
    fs.test_set_rebase_hook(Some(hook));
    let big = seeded(8, 128 * KIB);
    let patch = seeded(9, 4 * KIB);
    put(&mut model, 256 * KIB, &big);
    // Inside the parked copy: the newer bytes must win over the dropped copy.
    put(&mut model, 300 * KIB, &patch);
    let before = fs.test_rebase_count();
    std::thread::scope(|s| {
        let w = s.spawn(|| fs.test_write(fh, 256 * KIB as u64, &big));
        parked.recv_timeout(WAIT).expect("the rebase reached its I/O phase");
        // A change in the I/O phase: the parked result must be dropped.
        let r = fs.test_write(other, 300 * KIB as u64, &patch);
        go.send(()).unwrap();
        let retry = parked.recv_timeout(WAIT);
        let (tx, rx) = channel();
        let fs = &fs;
        s.spawn(move || tx.send(fs.test_read(ino, 300 * KIB as u64, 4096)).ok());
        let read = rx.recv_timeout(Duration::from_millis(300));
        go.send(()).ok();
        retry.expect("the retry reached its I/O phase");
        assert_eq!(r, Ok(patch.len() as u32));
        assert!(read.expect("the read waited for the retry").unwrap() == patch, "the read sees the newer bytes");
        assert_eq!(w.join().unwrap(), Ok(big.len() as u32));
    });
    assert_eq!(fs.test_rebase_count(), before + 2, "the dropped attempt and the unlocked retry");
    assert!(fs.test_dirty_cost(ino) <= 64 * KIB, "the retry published: cost {}", fs.test_dirty_cost(ino));
    fs.test_set_rebase_hook(None);
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content with the handles open");
    fs.test_release(ino, other).unwrap();
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content after release");
}

/// A rebase hook that parks every call until `release` is set (at most
/// 10 s each); `parked` gets a message at each call.
fn park_all(fs: &SliceFsFilesystem) -> (Receiver<()>, Arc<AtomicBool>) {
    let (parked_tx, parked_rx) = channel();
    let release = Arc::new(AtomicBool::new(false));
    let r = Arc::clone(&release);
    fs.test_set_rebase_hook(Some(Arc::new(move || {
        parked_tx.send(()).ok();
        let t0 = std::time::Instant::now();
        while !r.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(1));
        }
    })));
    (parked_rx, release)
}

/// The hard cap (#98, T-19-20): while one step is parked in its I/O phase,
/// a second writer applies its writes only while the dirty cost stays under
/// `limit + limit / GROW_DIV + 2 * REBASE_STEP`. The write that would pass it waits for the
/// step in flight with the cell unlocked: a read passes meanwhile.
#[test]
fn a_writer_at_the_hard_cap_waits_without_the_cell() {
    use slicefs_cli::{filesystem::REBASE_STEP, overlay::GROW_DIV};
    // The hard cap: the limit, the spare of a growing extent, two steps.
    let cap = 64 * KIB + 64 * KIB / GROW_DIV + 2 * REBASE_STEP;
    let (fs, _dir) = fresh_fs();
    let (ino, fh, mut model) = overlay_file(&fs);
    let (other, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let (parked, release) = park_all(&fs);
    let big = seeded(11, 128 * KIB);
    put(&mut model, 256 * KIB, &big);
    let pieces: Vec<Vec<u8>> = (0..20u64).map(|i| seeded(100 + i, 128 * KIB)).collect();
    std::thread::scope(|s| {
        let w = s.spawn(|| fs.test_write(fh, 256 * KIB as u64, &big));
        parked.recv_timeout(WAIT).expect("the rebase reached its I/O phase");
        let (fs, pieces) = (&fs, &pieces);
        let (tx, rx) = channel();
        let b = s.spawn(move || {
            for (i, piece) in pieces.iter().enumerate() {
                let off = (1 << 20) + i * 128 * KIB;
                let r = fs.test_write(other, off as u64, piece);
                tx.send((i, r, fs.test_dirty_cost(ino))).ok();
            }
        });
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut waiting = None;
            for _ in 0..pieces.len() {
                match rx.recv_timeout(Duration::from_millis(300)) {
                    Ok((i, r, cost)) => {
                        assert_eq!(r, Ok(128 * KIB as u32), "write {i}");
                        assert!(cost <= cap, "write {i}: dirty cost {cost} > cap {cap}");
                    }
                    Err(_) => {
                        waiting = Some(());
                        break;
                    }
                }
            }
            assert!(waiting.is_some(), "a write at the hard cap waits for the step in flight");
            let cost = fs.test_dirty_cost(ino);
            assert!(cost <= cap, "while the writer waits: dirty cost {cost} > cap {cap}");
            let (rtx, rrx) = channel();
            s.spawn(move || rtx.send(fs.test_read(ino, 256 * KIB as u64, 4096)).ok());
            let read = rrx.recv_timeout(WAIT).expect("the read waited for the writer at the cap");
            assert!(read.unwrap() == big[..4096], "the read sees the first write");
        }));
        release.store(true, Ordering::SeqCst);
        if let Err(e) = r {
            std::panic::resume_unwind(e);
        }
        assert_eq!(w.join().unwrap(), Ok(big.len() as u32));
        b.join().unwrap();
        for (i, r, cost) in rx.try_iter() {
            assert_eq!(r, Ok(128 * KIB as u32), "write {i}");
            assert!(cost <= cap, "write {i}: dirty cost {cost} > cap {cap}");
        }
    });
    fs.test_set_rebase_hook(None);
    for (i, piece) in pieces.iter().enumerate() {
        put(&mut model, (1 << 20) + i * 128 * KIB, piece);
    }
    let size = model.len() as u32;
    assert!(fs.test_read(ino, 0, size).unwrap() == model, "content with the handles open");
    fs.test_release(ino, other).unwrap();
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_read(ino, 0, size).unwrap() == model, "content after release");
}

/// D-02 "retries later": a write over the limit whose two unlocked steps
/// are both dropped returns with the cost above the limit (but at most the
/// cap); the step of the next write publishes.
#[test]
fn a_dropped_step_below_the_cap_retries_later() {
    use slicefs_cli::{filesystem::REBASE_STEP, overlay::GROW_DIV};
    // The hard cap: the limit, the spare of a growing extent, two steps.
    let cap = 64 * KIB + 64 * KIB / GROW_DIV + 2 * REBASE_STEP;
    let (fs, _dir) = fresh_fs();
    let (ino, fh, mut model) = overlay_file(&fs);
    let big = seeded(12, 128 * KIB);
    put(&mut model, 256 * KIB, &big);
    fs.test_drop_steps(2);
    let before = fs.test_rebase_count();
    assert_eq!(fs.test_write(fh, 256 * KIB as u64, &big), Ok(big.len() as u32));
    let cost = fs.test_dirty_cost(ino);
    assert_eq!(fs.test_rebase_count(), before + 2, "two attempts, both dropped");
    assert!(cost > 64 * KIB && cost <= cap, "cost {cost} after two dropped steps");
    let patch = seeded(13, 4 * KIB);
    put(&mut model, 600 * KIB, &patch);
    assert_eq!(fs.test_write(fh, 600 * KIB as u64, &patch), Ok(patch.len() as u32));
    assert_eq!(fs.test_rebase_count(), before + 3, "the next write's step");
    assert!(fs.test_dirty_cost(ino) <= 64 * KIB, "the next step published: cost {}", fs.test_dirty_cost(ino));
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content with the handle open");
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

/// A streaming write parked in its push (Phase B, cell unlocked) does not
/// block a read of the prefix that is streamed already.
#[test]
fn reader_does_not_wait_for_streaming_push() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "s", 0o644, 0, 0, 0).unwrap();
    let mut model = seeded(20, 256 * KIB);
    fs.test_write(fh, 0, &model).unwrap();
    let (parked, go) = park_first_push(&fs);
    let more = seeded(21, 128 * KIB);
    std::thread::scope(|s| {
        let w = s.spawn(|| fs.test_write(fh, 256 * KIB as u64, &more));
        parked.recv_timeout(WAIT).expect("the push reached its I/O phase");
        let (tx, rx) = channel();
        let fs = &fs;
        s.spawn(move || tx.send(fs.test_read(ino, 64 * KIB as u64, 64 * KIB as u32)).ok());
        let got = match rx.recv_timeout(WAIT) {
            Ok(r) => r,
            Err(e) => {
                go.send(()).ok();
                panic!("the read waited for the parked push: {e:?}");
            }
        };
        assert!(got.unwrap() == model[64 * KIB..128 * KIB], "the read sees the streamed prefix");
        go.send(()).unwrap();
        assert_eq!(w.join().unwrap(), Ok(more.len() as u32));
    });
    fs.test_set_push_hook(None);
    put(&mut model, 256 * KIB, &more);
    assert!(fs.test_is_streaming(fh), "the write stayed in order");
    assert!(fs.test_read(ino, 0, 1 << 20).unwrap() == model, "content with the handle open");
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_read(ino, 0, 1 << 20).unwrap() == model, "content after release");
}

/// Two writers on one stream: B writes the block after A's while A is
/// parked in its push. B waits for A's step with the cell released (a read
/// passes meanwhile), then streams in order after it; no step is dropped.
#[test]
fn a_second_stream_write_waits_for_the_push_without_the_cell() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "s", 0o644, 0, 0, 0).unwrap();
    let (other, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let mut model = seeded(22, 64 * KIB);
    fs.test_write(fh, 0, &model).unwrap();
    let (a, b) = (seeded(23, 64 * KIB), seeded(24, 64 * KIB));
    let prefix = model.clone();
    put(&mut model, 64 * KIB, &a);
    put(&mut model, 128 * KIB, &b);
    let (parked, go) = park_first_push(&fs);
    std::thread::scope(|s| {
        let w = s.spawn(|| fs.test_write(fh, 64 * KIB as u64, &a));
        parked.recv_timeout(WAIT).expect("the push reached its I/O phase");
        let (fs, b) = (&fs, &b);
        let (tx, rx) = channel();
        s.spawn(move || tx.send(fs.test_write(other, 128 * KIB as u64, b)).ok());
        let early = rx.recv_timeout(Duration::from_millis(300));
        let (rtx, rrx) = channel();
        s.spawn(move || rtx.send(fs.test_read(ino, 0, 64 * KIB as u32)).ok());
        let read = rrx.recv_timeout(WAIT);
        go.send(()).ok();
        assert!(early.is_err(), "B waited for the parked push: {early:?}");
        assert!(read.expect("the read did not wait for the push").unwrap() == prefix, "the read sees the prefix");
        assert_eq!(rx.recv_timeout(WAIT).expect("B went on after the push"), Ok(b.len() as u32));
        assert_eq!(w.join().unwrap(), Ok(a.len() as u32));
    });
    fs.test_set_push_hook(None);
    assert!(fs.test_is_streaming(fh), "both writes stayed in order");
    assert!(fs.test_read(ino, 0, 1 << 20).unwrap() == model, "both writes with the handles open");
    fs.test_release(ino, other).unwrap();
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_read(ino, 0, 1 << 20).unwrap() == model, "both writes after release");
}

/// A push whose result a test hook drops is tried again with the cell
/// unlocked: a read passes while the retry is parked in its push.
#[test]
fn a_forced_drop_of_a_push_retries_unlocked() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "s", 0o644, 0, 0, 0).unwrap();
    let mut model = seeded(28, 64 * KIB);
    fs.test_write(fh, 0, &model).unwrap();
    let prefix = model.clone();
    let more = seeded(29, 64 * KIB);
    let (hook, parked, go) = parker_at(&[2]);
    fs.test_set_push_hook(Some(hook));
    fs.test_drop_steps(1);
    std::thread::scope(|s| {
        let w = s.spawn(|| fs.test_write(fh, 64 * KIB as u64, &more));
        let reached = parked.recv_timeout(WAIT);
        let (tx, rx) = channel();
        let fs = &fs;
        s.spawn(move || tx.send(fs.test_read(ino, 0, 64 * KIB as u32)).ok());
        let read = rx.recv_timeout(Duration::from_millis(300));
        go.send(()).ok();
        reached.expect("the retry after the dropped push reached its push");
        assert!(read.expect("the read waited for the retry").unwrap() == prefix, "the read sees the prefix");
        assert_eq!(w.join().unwrap(), Ok(more.len() as u32));
    });
    fs.test_set_push_hook(None);
    put(&mut model, 64 * KIB, &more);
    assert!(fs.test_is_streaming(fh), "the write stayed in order");
    assert!(fs.test_read(ino, 0, 1 << 20).unwrap() == model, "content with the handle open");
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_read(ino, 0, 1 << 20).unwrap() == model, "content after release");
}

/// A read of a stream with pending out-of-order writes composes them from
/// memory (stream bytes, zeros in the gaps, pending bytes) and runs no
/// stream step; the pending writes stay pending.
#[test]
fn a_read_of_pending_writes_runs_no_stream_step() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "s", 0o644, 0, 0, 0).unwrap();
    let mut model = seeded(25, 16 * KIB);
    fs.test_write(fh, 0, &model).unwrap();
    let (x, y) = (seeded(26, 8 * KIB), seeded(27, 8 * KIB));
    fs.test_write(fh, 32 * KIB as u64, &x).unwrap();
    fs.test_write(fh, 48 * KIB as u64, &y).unwrap();
    put(&mut model, 32 * KIB, &x);
    put(&mut model, 48 * KIB, &y);
    let pushes = counter(&fs);
    assert!(fs.test_read(ino, 0, 1 << 20).unwrap() == model, "pending writes and zero gaps");
    assert!(fs.test_read(ino, 12 * KIB as u64, 24 * KIB as u32).unwrap() == model[12 * KIB..36 * KIB], "across a gap");
    assert!(fs.test_read(ino, 36 * KIB as u64, 16 * KIB as u32).unwrap() == model[36 * KIB..52 * KIB], "gap and pending");
    assert!(fs.test_read(ino, 52 * KIB as u64, 64 * KIB as u32).unwrap() == model[52 * KIB..], "to the end of file");
    assert!(fs.test_read(ino, 60 * KIB as u64, 4 * KIB as u32).unwrap().is_empty(), "past the end of file");
    assert_eq!(pushes.load(Ordering::SeqCst), 0, "no stream step in a read");
    fs.test_set_push_hook(None);
    assert!(fs.test_is_streaming(fh));
    assert!(fs.test_dirty_cost(ino) > 0, "the pending writes stay pending");
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_read(ino, 0, 1 << 20).unwrap() == model, "content after release");
}

/// A hook that counts its calls.
fn counter(fs: &SliceFsFilesystem) -> Arc<std::sync::atomic::AtomicUsize> {
    let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = Arc::clone(&n);
    fs.test_set_push_hook(Some(Arc::new(move || {
        c.fetch_add(1, Ordering::SeqCst);
    })));
    n
}

/// RESEARCH Pitfall 5: fsync 1 snapshots the stream and parks in its I/O
/// phase; a write and fsync 2 publish a newer root; then fsync 1 ends. The
/// manifest stays at fsync 2's root, every old root is released once, and
/// the state stays clean (no write came after the published snapshot).
#[test]
fn fsyncs_publish_in_order() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "s", 0o644, 0, 0, 0).unwrap();
    let mut model = seeded(30, 64 * KIB);
    fs.test_write(fh, 0, &model).unwrap();
    let r1 = slicefs_cli::content::put_all(fs.content(), &model).unwrap();
    let (parked, go) = park_first_push(&fs);
    let more = seeded(31, 64 * KIB);
    put(&mut model, 64 * KIB, &more);
    std::thread::scope(|s| {
        let f1 = s.spawn(|| fs.test_fsync(ino, fh));
        parked.recv_timeout(WAIT).expect("fsync 1 reached its I/O phase");
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_eq!(fs.test_write(fh, 64 * KIB as u64, &more), Ok(more.len() as u32));
            assert_eq!(fs.test_fsync(ino, fh), Ok(()));
        }));
        go.send(()).ok();
        if let Err(e) = r {
            std::panic::resume_unwind(e);
        }
        assert_eq!(f1.join().unwrap(), Ok(()));
    });
    let r2 = slicefs_cli::content::put_all(fs.content(), &model).unwrap();
    assert_ne!(r1, r2);
    assert_eq!(fs.meta().get_manifest(ino).unwrap(), vec![r2], "the newer root stays");
    assert_eq!(fs.meta().get_refcount(&r2), 1, "the published root is counted once");
    assert_eq!(fs.meta().get_refcount(&r1), 0, "the older snapshot was never installed");
    // Clean: another fsync has nothing to store (no stream step runs).
    let pushes = counter(&fs);
    fs.test_fsync(ino, fh).unwrap();
    assert_eq!(pushes.load(Ordering::SeqCst), 0, "no write after the published snapshot: clean");
    let tail = seeded(32, 4 * KIB);
    fs.test_write(fh, 128 * KIB as u64, &tail).unwrap();
    put(&mut model, 128 * KIB, &tail);
    let before = pushes.load(Ordering::SeqCst);
    fs.test_fsync(ino, fh).unwrap();
    assert!(pushes.load(Ordering::SeqCst) > before, "a write after it makes the state dirty");
    fs.test_set_push_hook(None);
    let r3 = fs.meta().get_manifest(ino).unwrap()[0];
    assert_eq!(fs.meta().get_refcount(&r2), 0, "the old root was released once");
    assert_eq!(fs.meta().get_refcount(&r3), 1);
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_read(ino, 0, 1 << 20).unwrap() == model, "content after release");
}

/// RESEARCH Pitfall 5, truncate case: an fsync parked in its I/O phase
/// must not install its snapshot after a truncate to 0 (the old content
/// would come back, and the release would not correct it).
#[test]
fn a_parked_fsync_does_not_install_over_a_truncate() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "s", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, &seeded(33, 64 * KIB)).unwrap();
    let (parked, go) = park_first_push(&fs);
    std::thread::scope(|s| {
        let f = s.spawn(|| fs.test_fsync(ino, fh));
        parked.recv_timeout(WAIT).expect("the fsync reached its I/O phase");
        let r = fs.test_setattr_size(ino, Some(fh), 0);
        go.send(()).unwrap();
        assert_eq!(r, Ok(()));
        assert_eq!(f.join().unwrap(), Ok(()));
    });
    fs.test_set_push_hook(None);
    assert!(fs.meta().get_manifest(ino).unwrap().is_empty(), "the truncate wins");
    assert_eq!(fs.test_getattr(ino).unwrap().size, 0);
    fs.test_release(ino, fh).unwrap();
    assert!(fs.meta().get_manifest(ino).unwrap().is_empty(), "still empty after release");
    assert_eq!(fs.test_getattr(ino).unwrap().size, 0);
    assert!(fs.test_read(ino, 0, 1 << 20).unwrap().is_empty());
}

/// A 1 MiB file, closed, then open with eight 32 KiB extents in its
/// overlay (cost above a final step of 64 KiB). Returns the handle and the
/// model.
fn dirty_overlay(fs: &SliceFsFilesystem, ino_out: &mut u64) -> (u64, Vec<u8>) {
    let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
    let mut model = seeded(40, 1 << 20);
    fs.test_write(fh, 0, &model).unwrap();
    fs.test_release(ino, fh).unwrap();
    fs.test_set_final_step(64 * KIB);
    let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    for i in 0..8u64 {
        let piece = seeded(41 + i, 32 * KIB);
        let off = i as usize * 128 * KIB + 16 * KIB;
        fs.test_write(fh, off as u64, &piece).unwrap();
        put(&mut model, off, &piece);
    }
    *ino_out = ino;
    (fh, model)
}

/// The last release stores an overlay dirtier than the final step in
/// several steps (never all of it at once, #98).
#[test]
fn the_last_release_finalizes_in_steps() {
    let (fs, _dir) = fresh_fs();
    let mut ino = 0;
    let (fh, model) = dirty_overlay(&fs, &mut ino);
    assert!(fs.test_dirty_cost(ino) > 3 * 64 * KIB);
    let before = fs.test_rebase_count();
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_rebase_count() - before > 1, "more than one step: {}", fs.test_rebase_count() - before);
    assert_eq!(fs.test_dirty_cost(ino), 0, "the cell is gone");
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content after release");
}

/// RESEARCH Pitfall 4: a write handle opened while the last release is in
/// a step's I/O phase keeps the cell alive, and its bytes are kept.
#[test]
fn a_writer_that_opens_during_the_finalize_keeps_the_cell() {
    let (fs, _dir) = fresh_fs();
    let mut ino = 0;
    let (fh, mut model) = dirty_overlay(&fs, &mut ino);
    let (parked, go) = park_first(&fs);
    let patch = seeded(50, 4 * KIB);
    put(&mut model, 700 * KIB, &patch);
    let fh2 = std::thread::scope(|s| {
        let rel = s.spawn(|| fs.test_release(ino, fh));
        parked.recv_timeout(WAIT).expect("the finalize reached a step's I/O phase");
        let (tx, rx) = channel();
        let (fs, patch) = (&fs, &patch);
        s.spawn(move || {
            let r = fs.test_open(ino, libc::O_RDWR).map(|(h, _)| (h, fs.test_write(h, 700 * KIB as u64, patch)));
            tx.send(r).ok();
        });
        let got = match rx.recv_timeout(WAIT) {
            Ok(r) => r,
            Err(e) => {
                go.send(()).ok();
                panic!("the open and write waited for the finalize: {e:?}");
            }
        };
        go.send(()).unwrap();
        let (fh2, w) = got.unwrap();
        assert_eq!(w, Ok(patch.len() as u32), "the cell is alive for the new writer");
        assert_eq!(rel.join().unwrap(), Ok(()));
        fh2
    });
    fs.test_set_rebase_hook(None);
    assert!(fs.test_dirty_cost(ino) > 0, "the finalize stopped: the cell lives on with dirty bytes");
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content with the new handle open");
    fs.test_release(ino, fh2).unwrap();
    assert_eq!(fs.test_dirty_cost(ino), 0, "the second release retired the cell");
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content after release");
}

/// A size change of a closed file runs in a transient cell: it shrinks and
/// extends the file, and an open of the inode meanwhile waits for it only,
/// then sees the new size.
#[test]
fn a_closed_file_truncate_runs_in_a_transient_cell() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "t", 0o644, 0, 0, 0).unwrap();
    let model = seeded(60, 256 * KIB);
    fs.test_write(fh, 0, &model).unwrap();
    fs.test_release(ino, fh).unwrap();
    let (parked, go) = park_first(&fs);
    let short = 100 * KIB as u64;
    let fh2 = std::thread::scope(|s| {
        let t = s.spawn(|| fs.test_setattr_size(ino, None, short));
        parked.recv_timeout(WAIT).expect("the truncate reached its splice");
        let (tx, rx) = channel();
        let fs = &fs;
        s.spawn(move || tx.send(fs.test_open(ino, libc::O_RDWR)).ok());
        let early = rx.recv_timeout(Duration::from_millis(300));
        go.send(()).unwrap();
        assert!(early.is_err(), "the open waited for the transient cell");
        let opened = rx.recv_timeout(WAIT).expect("the open went on after the truncate");
        assert_eq!(t.join().unwrap(), Ok(()));
        opened.unwrap().0
    });
    fs.test_set_rebase_hook(None);
    assert_eq!(fs.test_getattr(ino).unwrap().size, short, "the open sees the new size");
    assert!(fs.test_read(ino, 0, 1 << 20).unwrap() == model[..short as usize], "shrunk");
    fs.test_release(ino, fh2).unwrap();
    fs.test_setattr_size(ino, None, 300 * KIB as u64).unwrap();
    let mut want = model[..short as usize].to_vec();
    want.resize(300 * KIB, 0);
    assert!(fs.test_read(ino, 0, 1 << 20).unwrap() == want, "extended with zeros");
    assert_eq!(fs.test_dirty_cost(ino), 0, "no cell is left");
}

/// ROADMAP criterion 2: a thread parked in the I/O phase of file X does not
/// delay a write, a read or an fsync of file Y.
#[test]
fn many_files_no_cross_file_wait() {
    let (fs, _dir) = fresh_fs();
    let (x, fx, mut model_x) = overlay_file(&fs);
    let (y, fy) = fs.test_create(1, "y", 0o644, 0, 0, 0).unwrap();
    let data_y = seeded(70, 64 * KIB);
    let (parked, go) = park_first(&fs);
    let big = seeded(71, 128 * KIB);
    put(&mut model_x, 256 * KIB, &big);
    std::thread::scope(|s| {
        let w = s.spawn(|| fs.test_write(fx, 256 * KIB as u64, &big));
        parked.recv_timeout(WAIT).expect("the rebase of X reached its I/O phase");
        let (tx, rx) = channel();
        let (fs, data_y) = (&fs, &data_y);
        s.spawn(move || {
            let r = fs.test_write(fy, 0, data_y)
                .and_then(|_| fs.test_read(y, 0, 64 * KIB as u32))
                .and_then(|got| fs.test_fsync(y, fy).map(|_| got));
            tx.send(r).ok();
        });
        let got = match rx.recv_timeout(WAIT) {
            Ok(r) => r,
            Err(e) => {
                go.send(()).ok();
                panic!("file Y waited for the parked I/O of file X: {e:?}");
            }
        };
        go.send(()).unwrap();
        assert!(got.unwrap() == *data_y, "Y reads back its write");
        assert_eq!(w.join().unwrap(), Ok(big.len() as u32));
    });
    fs.test_set_rebase_hook(None);
    assert!(fs.test_read(x, 0, 2 << 20).unwrap() == model_x, "X with the handle open");
    fs.test_release(x, fx).unwrap();
    fs.test_release(y, fy).unwrap();
    assert!(fs.test_read(x, 0, 2 << 20).unwrap() == model_x, "X after release");
    assert!(fs.test_read(y, 0, 1 << 20).unwrap() == data_y, "Y after release");
}
