//! A busy chunk store does not fail a write with EIO (issue #170).
//!
//! When a background pack sync is slow (a disk under load: 10 to 40 s in the
//! S7 runs), a put at the unsynced-bytes cap waits for it, and after
//! `Options::store_wait` (10 s in a mount) it gives a busy error. A write used
//! to turn that into EIO, and the flusher counted it as a failed step. Now a
//! write request tries again, with the commit gate released between the
//! attempts, until the store is free or `store_busy_limit` has passed; the
//! flusher treats the busy step as "try later".
//!
//! The tests use the in-memory pack storage of the chunk store, with its
//! syncs held (the slow disk), a short `store_wait`, and the real file system
//! core. No fixed sleeps decide an order: a thread frees the disk when the
//! file system has tried a write again twice, and the writer must go on after it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chunk_store::mem::MemIo;
use chunk_store::pack_io::PackIo;
use chunk_store::retry::{RetryingIo, KEEP_CAP};
use chunk_store::{ChunkStore, Options};
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_traits::metadata::MetadataStore;
use slicefs_cli::handlers::{self as h, CreateResult, DataResult, EmptyResult, WriteResult};

const MIB: usize = 1 << 20;
/// A store call waits this long for a held sync before it gives a busy error.
const STORE_WAIT: Duration = Duration::from_millis(150);
/// Pieces of one MiB, more than the unsynced cap of a pack: the writes meet it.
const PIECES: usize = KEEP_CAP / MIB + 6;
const WAIT: Duration = Duration::from_secs(60);

struct Rig {
    fs: SliceFsFilesystem,
    mem: MemIo,
}

fn rig() -> Rig {
    let mem = MemIo::new();
    let packs: Arc<dyn PackIo> = Arc::new(RetryingIo::with_backoff(Arc::new(mem.clone()), vec![Duration::ZERO; 3]));
    let opts = Options { store_wait: STORE_WAIT, zstd_level: 0, index_background_merge: false, ..Options::default() };
    let cs = ChunkStore::open_with(packs, opts).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new("/unused", Arc::new(cs))));
    let fs = SliceFsFilesystem::new(DictMetadataStore::new(io.clone()), io, None);
    fs.meta().commit().unwrap();
    Rig { fs, mem }
}

/// No unbounded wait: abort the process if the test does not end.
fn watchdog() -> mpsc::Sender<()> {
    let (tx, rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(WAIT) {
            eprintln!("store_busy_tests watchdog: no finish after {} s", WAIT.as_secs());
            std::process::abort();
        }
    });
    tx
}

/// Incompressible bytes (the store does not compress them), different for each `i`.
fn piece(i: usize) -> Vec<u8> {
    let mut x = (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..MIB)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// The file type bits of a regular file (libc gives `mode_t`: u16 on macOS, u32 on Linux).
const S_IFREG: u32 = 0o100_000;

fn create(fs: &SliceFsFilesystem, name: &str, flags: i32) -> (u64, u64) {
    match h::handle_create(fs, 1, name, S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR | flags) {
        CreateResult::Ok(ino, fh, _) => (ino, fh),
        CreateResult::Error(e) => panic!("create {name}: errno {e}"),
    }
}

/// One write request as the FUSE job does it: the throttle with no lock, then
/// the attempts, each one under the commit gate.
fn request_write(fs: &SliceFsFilesystem, fh: u64, offset: u64, data: &[u8], flags: i32) -> WriteResult {
    let admit = fs.throttle_write();
    h::handle_write_retrying(fs, || {
        let _op = fs.gate_shared();
        h::handle_write_admitted(fs, fh, offset, data, flags, admit)
    })
}

/// Sets the flag when it drops (also on a panic).
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Hold the syncs of the disk and run `body`. The disk finishes its syncs as
/// soon as the file system has tried a write again twice after a busy store:
/// an event, not a timer, so a slow machine cannot miss the busy case. If
/// that never happens, the disk is freed after 30 s (the test then fails on
/// its own checks).
fn with_slow_disk<R>(r: &Rig, body: impl FnOnce() -> R) -> R {
    r.mem.hold_syncs(true);
    let done = AtomicBool::new(false);
    thread::scope(|s| {
        s.spawn(|| {
            let t = Instant::now();
            while r.fs.store_busy_retries() < 2 && !done.load(Ordering::SeqCst) && t.elapsed() < Duration::from_secs(30) {
                thread::sleep(Duration::from_millis(5));
            }
            r.mem.hold_syncs(false);
        });
        let _end = SetOnDrop(&done);
        body()
    })
}

/// The file holds `pieces` MiB in order: every MiB is read and compared.
fn check_content(fs: &SliceFsFilesystem, ino: u64, pieces: usize) {
    for i in 0..pieces {
        let _op = fs.gate_shared();
        match h::handle_read(fs, ino, (i * MIB) as u64, MIB as u32) {
            DataResult::Ok(d) => assert!(d == piece(i), "MiB {i} differs"),
            DataResult::Error(e) => panic!("read MiB {i}: errno {e}"),
        }
    }
}

#[test]
fn a_write_waits_for_a_busy_store_and_succeeds() {
    let _dog = watchdog();
    let r = rig();
    let (ino, fh) = create(&r.fs, "big", 0);
    let t = Instant::now();
    with_slow_disk(&r, || {
        for i in 0..PIECES {
            match request_write(&r.fs, fh, (i * MIB) as u64, &piece(i), 0) {
                WriteResult::Ok(n) => assert_eq!(n as usize, MIB, "short write {i}"),
                WriteResult::Error(e) => panic!("write {i} of {PIECES} MiB failed with errno {e} after {:?}", t.elapsed()),
            }
        }
    });
    assert!(r.fs.store_busy_retries() >= 2, "no write met a busy store: the test did not run the case");
    check_content(&r.fs, ino, PIECES);
    let _op = r.fs.gate_shared();
    assert!(matches!(h::handle_release(&r.fs, ino, fh), EmptyResult::Ok));
}

#[test]
fn an_append_that_meets_a_busy_store_is_in_the_file_once() {
    let _dog = watchdog();
    let r = rig();
    let (ino, fh) = create(&r.fs, "log", libc::O_APPEND);
    with_slow_disk(&r, || {
        for i in 0..PIECES {
            // The offset is ignored: the write goes to the end of file.
            match request_write(&r.fs, fh, 0, &piece(i), libc::O_APPEND) {
                WriteResult::Ok(n) => assert_eq!(n as usize, MIB, "short append {i}"),
                WriteResult::Error(e) => panic!("append {i} of {PIECES} MiB failed with errno {e}"),
            }
        }
    });
    assert!(r.fs.store_busy_retries() >= 2, "no write met a busy store: the test did not run the case");
    check_content(&r.fs, ino, PIECES);
    let _op = r.fs.gate_shared();
    match h::handle_getattr(&r.fs, ino) {
        h::AttrResult::Ok(a) => assert_eq!(a.size, (PIECES * MIB) as u64, "an append was applied twice or lost"),
        h::AttrResult::Error(e) => panic!("getattr: errno {e}"),
    }
}

#[test]
fn a_store_that_stays_busy_ends_the_write_with_eio_at_the_limit() {
    let _dog = watchdog();
    let r = rig();
    let (_ino, fh) = create(&r.fs, "stuck", 0);
    let limit = Duration::from_millis(500);
    r.fs.test_set_store_busy_limit(limit);
    r.mem.hold_syncs(true);
    let stop = AtomicBool::new(false);
    let mut failed = None;
    let t = Instant::now();
    thread::scope(|s| {
        // A held sync never ends: only the limit ends the write.
        s.spawn(|| {
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(20));
            }
            r.mem.hold_syncs(false);
        });
        for i in 0..PIECES {
            let started = Instant::now();
            if let WriteResult::Error(e) = request_write(&r.fs, fh, (i * MIB) as u64, &piece(i), 0) {
                failed = Some((i, e, started.elapsed()));
                break;
            }
        }
        stop.store(true, Ordering::SeqCst);
    });
    let (i, e, took) = failed.expect("every write passed with a store that stays busy: the cap was not met");
    assert_eq!(e, libc::EIO, "write {i}: the errno at the limit");
    assert!(took >= limit, "write {i} gave up after {took:?}, before the limit {limit:?}");
    // The held sync never ends: only the limit can end the write, so a wide bound is enough.
    assert!(took < limit + Duration::from_secs(10), "write {i} took {took:?}: the limit does not bound it");
    assert!(r.fs.store_busy_retries() >= 1, "the write did not try again before the limit");
    eprintln!("limit test: write {i} gave up after {took:?}, total {:?}", t.elapsed());
}

/// Fill the unsynced bytes of the open pack up to the cap with small objects
/// (the syncs are held): the next put of a few KiB waits for the sync.
fn fill_to_the_cap(fs: &SliceFsFilesystem) {
    let mut x = 0x1234_5678_9abc_def1u64;
    for i in 0u64.. {
        let obj: Vec<u8> = (0..4096)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        if let Err(e) = fs.content().put(&obj) {
            assert!(chunk_store::is_store_busy(&e), "put {i}: {e}");
            return;
        }
        assert!(i < (KEEP_CAP / 4096) as u64 + 8, "the store never met the cap");
    }
}

#[test]
fn a_flusher_step_that_meets_a_busy_store_is_not_an_error_and_goes_again() {
    use slicefs_cli::flusher;
    let _dog = watchdog();
    let r = rig();
    let (ino, fh) = create(&r.fs, "dirty", 0);
    r.mem.hold_syncs(true);
    // A streamed MiB, then a small write inside it: the file becomes an
    // overlay with dirty data that the flusher stores (a rebase step).
    let base = piece(0);
    match request_write(&r.fs, fh, 0, &base, 0) {
        WriteResult::Ok(n) => assert_eq!(n as usize, MIB),
        WriteResult::Error(e) => panic!("the first write: errno {e}"),
    }
    let patch = vec![0xA5u8; 4096];
    match request_write(&r.fs, fh, 8192, &patch, 0) {
        WriteResult::Ok(n) => assert_eq!(n as usize, patch.len()),
        WriteResult::Error(e) => panic!("the patch: errno {e}"),
    }
    fill_to_the_cap(&r.fs);
    // Any dirty byte is over this budget: the flusher takes the file.
    r.fs.test_set_dirty_budget(1);
    let st = flusher::flush_pass(&r.fs);
    let stats = r.fs.test_flusher_stats();
    assert_eq!((st.errors, stats.errors), (0, 0), "a busy store counted as a failed step: {st:?}");
    assert_eq!((st.busy, stats.busy), (1, 1), "the busy step was not counted: {st:?}");
    // The disk finishes its syncs; the cell leaves its back-off and the step publishes.
    r.mem.hold_syncs(false);
    let t = Instant::now();
    while r.fs.test_flusher_stats().published == 0 {
        assert!(t.elapsed() < Duration::from_secs(20), "the flusher did not step the file again: {:?}", r.fs.test_flusher_stats());
        flusher::flush_pass(&r.fs);
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(r.fs.test_flusher_stats().errors, 0);
    // The file is as written, with the patch.
    let mut want = base;
    want[8192..8192 + patch.len()].copy_from_slice(&patch);
    let _op = r.fs.gate_shared();
    match h::handle_read(&r.fs, ino, 0, MIB as u32) {
        DataResult::Ok(d) => assert!(d == want, "the file differs after the busy step"),
        DataResult::Error(e) => panic!("read: errno {e}"),
    }
    assert!(matches!(h::handle_release(&r.fs, ino, fh), EmptyResult::Ok));
}
