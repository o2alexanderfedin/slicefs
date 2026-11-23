//! The flusher and the write throttle (Phase 20, ROADMAP criteria 2 and 3).
//!
//! A writer over the shared dirty budget waits before it takes any lock
//! (`throttle_write`), and a flusher lowers the total with steps of other
//! files, one step per shared gate hold. After the wait limit the writer
//! writes and pays with a step of its own file.
//!
//! The tests use event waits only (`wait_until` with a limit, channels), no
//! fixed sleeps for order and no upper bounds on durations: the machine can
//! run at a high load. Each file is checked against a block-version model,
//! never against a byte copy of the file. A watchdog aborts a test that
//! does not end.
//!
//! Throttle tests fill with the default budget, then lower it: with a low
//! budget during the fill, the fill writes would pay at once and no writer
//! would ever wait.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod common;

use metadata::store::DictMetadataStore;
use metadata::wal::WalConfig;
use metadata::store_io::StoreIo;
use slicefs_cli::commit_timer::timed_commit;
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::flusher;
use slicefs_cli::handlers::{self as h, WriteResult};
use tempfile::TempDir;

const KIB: usize = 1 << 10;
const MIB: usize = 1 << 20;
/// One block of the model.
const BLOCK: usize = 4 * KIB;
/// One test write; pieces sit on a grid of `GRID`, so no two writes touch.
const PIECE: usize = 32 * KIB;
const GRID: usize = 64 * KIB;
const WAIT: Duration = Duration::from_secs(5);

fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let meta = DictMetadataStore::new(io.clone());
    let fs = SliceFsFilesystem::new(meta, io, None);
    (fs, dir)
}

/// No unbounded wait: if the test does not end in `limit`, abort the
/// process. Dropping the sender (the end of the test) stops the thread.
fn watchdog(limit: Duration) -> mpsc::Sender<()> {
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(limit) {
            eprintln!("FLUSHER watchdog: no finish after {} s", limit.as_secs());
            std::process::abort();
        }
    });
    tx
}

/// True when `f` became true within `limit`.
fn wait_until(limit: Duration, f: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if f() {
            return true;
        }
        if start.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Stops the flusher when the scope ends. On a failed assert it also lifts
/// the budget, so a throttled writer leaves at its next slice and the scope
/// join does not hang.
struct StopGuard<'a>(&'a SliceFsFilesystem);

impl Drop for StopGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.test_set_dirty_budget(usize::MAX);
        }
        self.0.stop_flusher();
    }
}

/// A write in the order of the FUSE callback: the throttle with no lock
/// held, then the commit gate, then the handler. Returns the result and
/// the rebase bytes of this thread's write (0: it ran no step).
fn throttled_write(fs: &SliceFsFilesystem, fh: u64, offset: u64, data: &[u8]) -> (WriteResult, u64) {
    let admit = fs.throttle_write();
    let r = {
        let _gate = fs.gate_shared();
        h::handle_write_admitted(fs, fh, offset, data, 0, admit)
    };
    (r, fs.test_last_write_rebase_bytes())
}

/// The bytes of one block at one version.
fn block_bytes(file: u64, block: usize, version: u32, out: &mut [u8]) {
    let mut x = 0x5EED_u64
        ^ file.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (block as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ u64::from(version).wrapping_mul(0x1656_67B1_9E37_79F9);
    x |= 1;
    for word in out.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        word.copy_from_slice(&x.to_le_bytes()[..word.len()]);
    }
}

/// A file and the version of each of its blocks.
struct Model {
    file: u64,
    ino: u64,
    fh: u64,
    versions: Vec<u32>,
}

impl Model {
    /// The bytes of blocks `[first, first + n)` at the given versions.
    fn bytes(&self, first: usize, versions: &[u32]) -> Vec<u8> {
        let mut out = vec![0u8; versions.len() * BLOCK];
        for (i, v) in versions.iter().enumerate() {
            block_bytes(self.file, first + i, *v, &mut out[i * BLOCK..(i + 1) * BLOCK]);
        }
        out
    }

    /// The offset and the bytes of the next write of grid slot `slot`.
    fn piece(&self, slot: usize) -> (u64, Vec<u8>) {
        let first = slot * GRID / BLOCK;
        let next: Vec<u32> = self.versions[first..first + PIECE / BLOCK].iter().map(|v| v + 1).collect();
        ((slot * GRID) as u64, self.bytes(first, &next))
    }

    /// Record that the write of `piece(slot)` is in the file.
    fn applied(&mut self, slot: usize) {
        let first = slot * GRID / BLOCK;
        for v in &mut self.versions[first..first + PIECE / BLOCK] {
            *v += 1;
        }
    }

    /// Write `piece(slot)` with the in-process write path.
    fn write(&mut self, fs: &SliceFsFilesystem, slot: usize) {
        let (off, data) = self.piece(slot);
        fs.test_write(self.fh, off, &data).unwrap();
        self.applied(slot);
    }

    /// Read the file in 1 MiB pieces and compare each with the model.
    fn check(&self, fs: &SliceFsFilesystem, when: &str) {
        let per = MIB / BLOCK;
        for (i, versions) in self.versions.chunks(per).enumerate() {
            let got = fs.test_read(self.ino, (i * MIB) as u64, (versions.len() * BLOCK) as u32).unwrap();
            assert!(got == self.bytes(i * per, versions), "file {} differs from its model in MiB {i} ({when})", self.file);
        }
    }

    fn release(&self, fs: &SliceFsFilesystem) {
        fs.test_release(self.ino, self.fh).unwrap();
    }
}

/// A file of `size` bytes (every block at version 0), written and closed,
/// then open again `O_RDWR` and made an overlay by one write at slot 1.
fn overlay_file(fs: &SliceFsFilesystem, file: u64, size: usize) -> Model {
    let (ino, fh) = fs.test_create(1, &format!("f{file}"), 0o644, 0, 0, 0).unwrap();
    let mut m = Model { file, ino, fh, versions: vec![0; size / BLOCK] };
    let per = MIB / BLOCK;
    for i in 0..size.div_ceil(MIB) {
        let end = ((i + 1) * per).min(m.versions.len());
        fs.test_write(fh, (i * MIB) as u64, &m.bytes(i * per, &m.versions[i * per..end])).unwrap();
    }
    fs.test_release(ino, fh).unwrap();
    m.fh = fs.test_open(ino, libc::O_RDWR).unwrap().0;
    m.write(fs, 1);
    assert!(!fs.test_is_streaming(m.fh), "file {file} is an overlay");
    m
}

/// An overlay file with `pieces` dirty pieces (slots 1 to `pieces`).
fn dirty_file(fs: &SliceFsFilesystem, file: u64, size: usize, pieces: usize) -> Model {
    let mut m = overlay_file(fs, file, size);
    for slot in 2..=pieces {
        m.write(fs, slot);
    }
    m
}

#[test]
fn a_writer_over_the_budget_waits_for_the_flusher() {
    let _dog = watchdog(Duration::from_secs(90));
    let (fs, _dir) = fresh_fs();
    fs.test_set_dirty_limit(2 * MIB);
    // With the default budget: Q 1.5 MiB dirty, W 256 KiB.
    let q = dirty_file(&fs, 1, 4 * MIB, 48);
    let mut w = dirty_file(&fs, 2, MIB, 8);
    let q_cost = fs.test_dirty_cost(q.ino);
    assert!(q_cost >= 3 * MIB / 2, "Q is dirty: {q_cost}");
    fs.test_pause_flusher(true);
    fs.test_set_flush_pass(20);
    fs.test_set_throttle_wait(30_000);
    let budget = 3 * MIB / 2;
    let (off, data) = w.piece(10);
    let w_fh = w.fh;
    std::thread::scope(|s| {
        let _stop = StopGuard(&fs);
        s.spawn(|| flusher::run_flusher(&fs));
        assert!(wait_until(WAIT, || fs.test_flusher_running()), "the flusher runs");
        fs.test_set_dirty_budget(budget);
        assert!(fs.test_dirty_total() >= budget, "the total is at or above the budget");
        let (tx, rx) = mpsc::channel();
        let (fs_ref, data) = (&fs, &data);
        let writer = s.spawn(move || tx.send(throttled_write(fs_ref, w_fh, off, data)).unwrap());
        assert!(wait_until(WAIT, || fs.test_throttled_now() == 1), "the writer waits in the throttle");
        assert!(!writer.is_finished(), "the throttled writer has not returned");
        fs.test_pause_flusher(false);
        let (r, rebase) = rx.recv_timeout(Duration::from_secs(10)).expect("the writer returns after the flusher step");
        assert!(matches!(r, WriteResult::Ok(n) if n as usize == PIECE), "the write is in: {r:?}");
        assert_eq!(rebase, 0, "the writer did not pay with a step of its own file");
        let st = fs.test_flusher_stats();
        assert!(st.published >= 1, "the flusher published a step: {st:?}");
        assert!(fs.test_dirty_cost(q.ino) < q_cost, "the flusher stepped Q");
        assert!(fs.test_dirty_total() < budget, "the total is below the budget");
    });
    w.applied(10);
    q.check(&fs, "handles open");
    w.check(&fs, "handles open");
    q.release(&fs);
    w.release(&fs);
    q.check(&fs, "after release");
    w.check(&fs, "after release");
}

#[test]
fn the_flusher_lowers_the_total() {
    let _dog = watchdog(Duration::from_secs(60));
    let (fs, _dir) = fresh_fs();
    fs.test_set_dirty_limit(2 * MIB);
    let a = dirty_file(&fs, 1, 4 * MIB, 48);
    let b = dirty_file(&fs, 2, 4 * MIB, 32);
    let (a_cost, b_cost) = (fs.test_dirty_cost(a.ino), fs.test_dirty_cost(b.ino));
    fs.test_set_dirty_budget(4 * MIB);
    assert!(fs.test_dirty_total() >= 2 * MIB, "the total is at or above half the budget");
    let st = flusher::flush_pass(&fs);
    assert!(fs.test_dirty_total() < 2 * MIB, "the pass lowered the total below half the budget: {st:?}");
    assert!(fs.test_dirty_cost(a.ino) < a_cost, "the largest file was stepped");
    assert_eq!(fs.test_dirty_cost(b.ino), b_cost, "the pass stopped below half the budget");
    a.check(&fs, "handles open");
    b.check(&fs, "handles open");
    a.release(&fs);
    b.release(&fs);
    a.check(&fs, "after release");
    b.check(&fs, "after release");
}

#[test]
fn a_pass_ends_when_no_step_frees() {
    let _dog = watchdog(Duration::from_secs(60));
    let (fs, _dir) = fresh_fs();
    fs.test_set_dirty_limit(2 * MIB);
    let a = dirty_file(&fs, 1, 4 * MIB, 48);
    let b = dirty_file(&fs, 2, 4 * MIB, 32);
    fs.test_set_dirty_budget(4 * MIB);
    assert!(fs.test_dirty_total() >= 2 * MIB, "the total is at or above half the budget");
    fs.test_drop_every(1);
    let st = std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        let fs_ref = &fs;
        s.spawn(move || tx.send(flusher::flush_pass(fs_ref)).unwrap());
        rx.recv_timeout(WAIT).expect("the pass returns")
    });
    assert_eq!(st.published, 0, "no step published: {st:?}");
    assert!(st.drops >= 1, "the dropped steps were counted: {st:?}");
    assert!(st.steps <= 2, "at most one try per dirty cell: {st:?}");
    fs.test_drop_every(0);
    a.check(&fs, "handles open");
    b.check(&fs, "handles open");
    a.release(&fs);
    b.release(&fs);
    a.check(&fs, "after release");
    b.check(&fs, "after release");
}

#[test]
fn a_throttled_writer_writes_after_the_limit() {
    let _dog = watchdog(Duration::from_secs(60));
    let (fs, _dir) = fresh_fs();
    fs.test_set_dirty_limit(2 * MIB);
    let mut w = dirty_file(&fs, 1, 4 * MIB, 48);
    fs.test_pause_flusher(true);
    fs.test_set_flush_pass(20);
    fs.test_set_throttle_wait(300);
    let (off, data) = w.piece(50);
    let w_fh = w.fh;
    std::thread::scope(|s| {
        let _stop = StopGuard(&fs);
        s.spawn(|| flusher::run_flusher(&fs));
        assert!(wait_until(WAIT, || fs.test_flusher_running()), "the flusher runs");
        fs.test_set_dirty_budget(MIB);
        let (tx, rx) = mpsc::channel();
        let (fs_ref, data) = (&fs, &data);
        s.spawn(move || {
            let start = Instant::now();
            let r = throttled_write(fs_ref, w_fh, off, data);
            tx.send((r, start.elapsed())).unwrap();
        });
        let ((r, rebase), took) = rx.recv_timeout(Duration::from_secs(10)).expect("the writer returns");
        assert!(matches!(r, WriteResult::Ok(n) if n as usize == PIECE), "the write is in: {r:?}");
        assert!(took >= Duration::from_millis(300), "the writer waited for the limit: {took:?}");
        assert_eq!(fs.test_flusher_stats().timeouts, 1, "one throttle wait reached the limit");
        assert!(rebase > 0, "the writer paid with a step of its own file");
    });
    w.applied(50);
    w.check(&fs, "handle open");
    w.release(&fs);
    w.check(&fs, "after release");
}

#[test]
fn a_write_past_half_the_budget_kicks_the_flusher() {
    let _dog = watchdog(Duration::from_secs(60));
    let (fs, _dir) = fresh_fs();
    fs.test_set_dirty_limit(2 * MIB);
    let mut a = dirty_file(&fs, 1, 4 * MIB, 16);
    // With a pass interval of 60 s the flusher makes no pass by itself.
    fs.test_set_flush_pass(60_000);
    std::thread::scope(|s| {
        let _stop = StopGuard(&fs);
        s.spawn(|| flusher::run_flusher(&fs));
        assert!(wait_until(WAIT, || fs.test_flusher_running()), "the flusher runs");
        // Half the budget sits just above the total: the next piece crosses it.
        let budget = 2 * (fs.test_dirty_total() + PIECE / 2);
        fs.test_set_dirty_budget(budget);
        let passes = fs.test_flusher_stats().passes;
        let mut write = |slot: usize| {
            let (off, data) = a.piece(slot);
            let (r, _) = throttled_write(&fs, a.fh, off, &data);
            assert!(matches!(r, WriteResult::Ok(n) if n as usize == PIECE), "the write is in: {r:?}");
            a.applied(slot);
        };
        write(17);
        assert!(fs.test_dirty_total() >= budget / 2, "the first write crossed half the budget");
        assert_eq!(fs.test_flusher_stats().passes, passes, "no pass before a write finds the total at half");
        write(18);
        assert!(wait_until(WAIT, || fs.test_flusher_stats().passes > passes), "the second write kicked the flusher");
    });
    a.check(&fs, "handle open");
    a.release(&fs);
    a.check(&fs, "after release");
}

/// Receive `n` write results within `limit` in total, and check that each
/// write put one piece in.
fn all_writes_ok(rx: &mpsc::Receiver<(usize, WriteResult)>, n: usize, limit: Duration, what: &str) {
    let deadline = Instant::now() + limit;
    for _ in 0..n {
        let left = deadline.saturating_duration_since(Instant::now());
        let (i, r) = rx.recv_timeout(left).unwrap_or_else(|_| panic!("{what}: the throttled writes return Ok within {limit:?}"));
        assert!(matches!(r, WriteResult::Ok(k) if k as usize == PIECE), "{what}: write {i} is in: {r:?}");
    }
}

/// Four overlay files of 1 MiB with 8 dirty pieces each (files 1 to 4).
fn four_writers(fs: &SliceFsFilesystem) -> Vec<Model> {
    (1..=4).map(|f| dirty_file(fs, f, MIB, 8)).collect()
}

/// Start one throttled write of slot `slot` per file on scoped threads; the
/// results come on the returned channel.
fn spawn_writes<'s, 'e: 's>(
    s: &'s std::thread::Scope<'s, 'e>,
    fs: &'e SliceFsFilesystem,
    writes: &'e [(u64, u64, Vec<u8>)],
) -> mpsc::Receiver<(usize, WriteResult)> {
    let (tx, rx) = mpsc::channel();
    for (i, (fh, off, data)) in writes.iter().enumerate() {
        let tx = tx.clone();
        s.spawn(move || {
            let _ = tx.send((i, throttled_write(fs, *fh, *off, data).0));
        });
    }
    rx
}

#[test]
fn stop_wakes_throttled_writers() {
    let _dog = watchdog(Duration::from_secs(60));
    let (fs, _dir) = fresh_fs();
    fs.test_set_dirty_limit(2 * MIB);
    let mut files = four_writers(&fs);
    fs.test_pause_flusher(true);
    fs.test_set_flush_pass(20);
    fs.test_set_throttle_wait(10_000);
    let writes: Vec<(u64, u64, Vec<u8>)> = files.iter().map(|m| {
        let (off, data) = m.piece(10);
        (m.fh, off, data)
    }).collect();
    std::thread::scope(|s| {
        let _stop = StopGuard(&fs);
        let flusher = s.spawn(|| flusher::run_flusher(&fs));
        assert!(wait_until(WAIT, || fs.test_flusher_running()), "the flusher runs");
        fs.test_set_dirty_budget(fs.test_dirty_total());
        let rx = spawn_writes(s, &fs, &writes);
        assert!(wait_until(WAIT, || fs.test_throttled_now() == 4), "the four writers wait in the throttle");
        assert!(!fs.test_flusher_stopping(), "no stop was asked before stop_flusher");
        fs.stop_flusher();
        assert!(fs.test_flusher_stopping(), "stop_flusher asked the stop");
        all_writes_ok(&rx, 4, WAIT, "after the stop");
        let st = fs.test_flusher_stats();
        assert_eq!(st.timeouts, 0, "no throttle wait reached the limit: {st:?}");
        assert_eq!(st.stop_exits, 4, "the four waits left at the stop: {st:?}");
        assert!(wait_until(WAIT, || flusher.is_finished()), "the flusher thread ended");
    });
    for m in &mut files {
        m.applied(10);
        m.check(&fs, "handles open");
        m.release(&fs);
        m.check(&fs, "after release");
    }
}

#[test]
fn a_flusher_panic_wakes_throttled_writers() {
    const NAME: &str = "test-flusher";
    let _dog = watchdog(Duration::from_secs(60));
    let (fs, _dir) = fresh_fs();
    fs.test_set_dirty_limit(2 * MIB);
    // Q is the largest dirty file, so the first flusher step is on Q.
    let mut q = dirty_file(&fs, 5, 4 * MIB, 24);
    let mut files = four_writers(&fs);
    // After the fill, so no fill write meets the hook.
    let fired = Arc::new(AtomicBool::new(false));
    {
        let fired = Arc::clone(&fired);
        fs.test_set_rebase_hook(Some(Arc::new(move || {
            if std::thread::current().name() == Some(NAME) && !fired.swap(true, Ordering::SeqCst) {
                panic!("test: a panic in a flusher step");
            }
        })));
    }
    fs.test_pause_flusher(true);
    fs.test_set_flush_pass(20);
    fs.test_set_throttle_wait(10_000);
    let writes: Vec<(u64, u64, Vec<u8>)> = files.iter().map(|m| {
        let (off, data) = m.piece(10);
        (m.fh, off, data)
    }).collect();
    let (q_off, q_data) = q.piece(30);
    std::thread::scope(|s| {
        let _stop = StopGuard(&fs);
        let flusher = std::thread::Builder::new()
            .name(NAME.into())
            .spawn_scoped(s, || flusher::run_flusher(&fs))
            .unwrap();
        assert!(wait_until(WAIT, || fs.test_flusher_running()), "the flusher runs");
        fs.test_set_dirty_budget(fs.test_dirty_total());
        let rx = spawn_writes(s, &fs, &writes);
        assert!(wait_until(WAIT, || fs.test_throttled_now() == 4), "the four writers wait in the throttle");
        fs.test_pause_flusher(false);
        assert!(wait_until(WAIT, || flusher.is_finished()), "the flusher thread ended");
        assert!(flusher.join().is_err(), "the join of the flusher gave Err (the panic)");
        assert!(fired.load(Ordering::SeqCst), "the hook panicked");
        all_writes_ok(&rx, 4, WAIT, "after the flusher panic");
        assert!(!fs.test_flusher_running(), "no flusher runs");
        // The step token of Q was cleared, so a write to Q does not wait for it.
        let (r, _) = throttled_write(&fs, q.fh, q_off, &q_data);
        assert!(matches!(r, WriteResult::Ok(k) if k as usize == PIECE), "the write to Q is in: {r:?}");
    });
    fs.test_set_rebase_hook(None);
    q.applied(30);
    for m in &mut files {
        m.applied(10);
    }
    for m in std::iter::once(&q).chain(files.iter()) {
        m.check(&fs, "handles open");
        m.release(&fs);
        m.check(&fs, "after release");
    }
}

/// The text of a caught panic.
#[cfg(debug_assertions)]
fn panic_text(e: Box<dyn std::any::Any + Send>) -> String {
    match e.downcast::<String>() {
        Ok(s) => *s,
        Err(e) => e.downcast::<&'static str>().map(|s| s.to_string()).unwrap_or_default(),
    }
}

#[cfg(debug_assertions)]
#[test]
fn a_throttle_with_a_lock_held_panics() {
    let (fs, _dir) = fresh_fs();
    let r = {
        let _gate = fs.gate_shared();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fs.throttle_write()))
    };
    let e = r.expect_err("throttle_write with the gate held gave Err");
    let text = panic_text(e);
    assert!(text.contains("with a lock held"), "the panic names the held lock: {text}");
}

#[cfg(debug_assertions)]
#[test]
fn a_stop_with_a_lock_held_panics() {
    let (fs, _dir) = fresh_fs();
    let r = {
        let _gate = fs.gate_shared();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fs.stop_flusher()))
    };
    let e = r.expect_err("stop_flusher with the gate held gave Err");
    let text = panic_text(e);
    assert!(text.contains("with a lock held"), "the panic names the held lock: {text}");
}

#[test]
fn a_queued_commit_is_seen() {
    let _dog = watchdog(Duration::from_secs(60));
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "c", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, b"commit").unwrap();
    fs.test_release(ino, fh).unwrap();
    std::thread::scope(|s| {
        let gate = fs.gate_shared();
        let (tx, rx) = mpsc::channel();
        let fs_ref = &fs;
        s.spawn(move || {
            let _ = tx.send(timed_commit(fs_ref, &fs_ref.commit_gate()).map(|_| ()).map_err(|e| e.to_string()));
        });
        assert!(wait_until(WAIT, || fs.test_gate_exclusive_waiters() == 1), "the commit is queued for the exclusive gate");
        assert!(rx.try_recv().is_err(), "the commit has not returned while the gate is held");
        drop(gate);
        let r = rx.recv_timeout(Duration::from_secs(10)).expect("the commit returns after the gate is released");
        assert!(r.is_ok(), "the commit is Ok: {r:?}");
        assert_eq!(fs.test_gate_exclusive_waiters(), 0, "no commit is queued now");
    });
}

/// Lets the parked flusher step go and stops the flusher when dropped, so a
/// failed assert does not leave a thread parked or a flusher running.
struct EndGuard {
    go: Option<mpsc::Sender<()>>,
    fs: Arc<SliceFsFilesystem>,
}

impl Drop for EndGuard {
    fn drop(&mut self) {
        self.go.take();
        self.fs.stop_flusher();
    }
}

#[test]
fn shutdown_with_a_parked_flusher_step_and_a_queued_commit_ends() {
    let _dog = watchdog(Duration::from_secs(60));
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let fs = Arc::new(common::new_fs_with(&store, WalConfig::PerOp));
    fs.test_set_dirty_limit(2 * MIB);
    let a = dirty_file(&fs, 1, 4 * MIB, 48);
    let b = dirty_file(&fs, 2, 4 * MIB, 32);
    // After the fill: the budget and the hook.
    fs.test_set_dirty_budget(4 * MIB);
    assert!(fs.test_dirty_total() >= 2 * MIB, "the total is at or above half the budget");
    // The first rebase (the flusher step, with the gate held shared) tells
    // the test and waits until `go` sends or drops (30 s at most).
    let (parked_tx, parked_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let park = Mutex::new(Some((parked_tx, go_rx)));
    fs.test_set_rebase_hook(Some(Arc::new(move || {
        let Some((parked, go)) = park.lock().unwrap().take() else { return };
        let _ = parked.send(());
        let _ = go.recv_timeout(Duration::from_secs(30));
    })));
    fs.start_flusher();
    let mut end = EndGuard { go: Some(go_tx), fs: Arc::clone(&fs) };
    parked_rx.recv_timeout(Duration::from_secs(10)).expect("the flusher step is parked");

    let (commit_tx, commit_rx) = mpsc::channel();
    {
        let fs = Arc::clone(&fs);
        std::thread::spawn(move || {
            let r = timed_commit(&fs, &fs.commit_gate()).map(|_| ()).map_err(|e| e.to_string());
            drop(fs);
            let _ = commit_tx.send(r);
        });
    }
    assert!(wait_until(Duration::from_secs(10), || fs.test_gate_exclusive_waiters() == 1),
        "the commit is queued for the exclusive gate");

    let (down_tx, down_rx) = mpsc::channel();
    {
        let fs = Arc::clone(&fs);
        std::thread::spawn(move || {
            let r = fs.shutdown();
            drop(fs);
            let _ = down_tx.send(r);
        });
    }
    assert!(wait_until(Duration::from_secs(10), || fs.test_flusher_stopping()), "the shutdown asked the stop");
    // Only now does the parked step go on.
    end.go.take();
    let c = commit_rx.recv_timeout(Duration::from_secs(10)).expect("the commit ends");
    assert!(c.is_ok(), "the commit is Ok: {c:?}");
    let down = down_rx.recv_timeout(Duration::from_secs(10)).expect("the shutdown ends");
    assert!(down, "shutdown returns true");
    assert!(wait_until(Duration::from_secs(10), || !fs.test_flusher_running()), "the flusher ended");

    fs.test_set_rebase_hook(None);
    drop(end);
    drop(fs);
    let fs = common::reload_and_replay(&store);
    a.check(&fs, "after reload and replay");
    b.check(&fs, "after reload and replay");
}

// ROADMAP criterion 3: all writers throttled while a commit waits.

/// The hook type of `test_set_rebase_hook`.
type Hook = Arc<dyn Fn() + Send + Sync>;

/// A rebase hook that parks its first call until `go` sends or drops (at
/// most 30 s) and tells `parked` when it gets there. Later calls do not park.
fn park_first_rebase() -> (Hook, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (parked_tx, parked_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let park = Mutex::new(Some((parked_tx, go_rx)));
    let hook: Hook = Arc::new(move || {
        let Some((parked, go)) = park.lock().unwrap().take() else { return };
        let _ = parked.send(());
        let _ = go.recv_timeout(Duration::from_secs(30));
    });
    (hook, parked_rx, go_tx)
}

/// Writes per writer thread in the criterion 3 tests, and the first grid
/// slot they write (clean slots, so each write adds its full size).
const WRITES: usize = 8;
const FIRST_SLOT: usize = 40;

/// A logged file system (a commit works) with four overlay files of 4 MiB,
/// each with 35 dirty pieces (about 1.1 MiB, about 4.4 MiB in total), all
/// filled with the default budget. Dirty limit 2 MiB, throttle wait limit
/// 30 s (a writer that leaves early is a bug, not a timeout), pass 20 ms,
/// and the flusher paused. The budget is not lowered yet.
fn throttle_setup(store: &std::path::Path) -> (SliceFsFilesystem, Vec<Model>) {
    let fs = common::new_fs_with(store, WalConfig::PerOp);
    fs.test_set_dirty_limit(2 * MIB);
    let files: Vec<Model> = (1..=4).map(|f| dirty_file(&fs, f, 4 * MIB, 35)).collect();
    for m in &files {
        let cost = fs.test_dirty_cost(m.ino);
        assert!(cost >= MIB && cost < 2 * MIB, "file {} has about 1.1 MiB dirty: {cost}", m.file);
    }
    fs.test_pause_flusher(true);
    fs.test_set_flush_pass(20);
    fs.test_set_throttle_wait(30_000);
    (fs, files)
}

/// The writes of one writer thread: `WRITES` pieces from `FIRST_SLOT`.
type Writes = Vec<(u64, Vec<u8>)>;

fn writer_pieces(m: &Model) -> Writes {
    (FIRST_SLOT..FIRST_SLOT + WRITES).map(|slot| m.piece(slot)).collect()
}

/// One scoped writer thread per file; each makes its throttled writes in
/// order and sends its results at the end.
fn spawn_writers<'s, 'e: 's>(
    s: &'s std::thread::Scope<'s, 'e>,
    fs: &'e SliceFsFilesystem,
    writes: &'e [(u64, Writes)],
) -> mpsc::Receiver<(usize, Vec<WriteResult>)> {
    let (tx, rx) = mpsc::channel();
    for (i, (fh, pieces)) in writes.iter().enumerate() {
        let tx = tx.clone();
        s.spawn(move || {
            let r = pieces.iter().map(|(off, data)| throttled_write(fs, *fh, *off, data).0).collect();
            let _ = tx.send((i, r));
        });
    }
    rx
}

/// Receive the results of `n` writer threads within `limit` in total, and
/// check that each write put one piece in.
fn all_writers_ok(rx: &mpsc::Receiver<(usize, Vec<WriteResult>)>, n: usize, limit: Duration) {
    let deadline = Instant::now() + limit;
    for _ in 0..n {
        let left = deadline.saturating_duration_since(Instant::now());
        let (i, rs) = rx.recv_timeout(left).unwrap_or_else(|_| panic!("all writers return within {limit:?}"));
        assert_eq!(rs.len(), WRITES, "writer {i} made all its writes");
        for r in rs {
            assert!(matches!(r, WriteResult::Ok(k) if k as usize == PIECE), "a write of writer {i} is in: {r:?}");
        }
    }
}

/// Mark the writer pieces as written, check the files with the handles
/// open and after release, then drop the file system and check them again
/// after a reload and a replay of the write log.
fn check_after_writers(fs: SliceFsFilesystem, mut files: Vec<Model>, store: &std::path::Path) {
    for m in &mut files {
        for slot in FIRST_SLOT..FIRST_SLOT + WRITES {
            m.applied(slot);
        }
        m.check(&fs, "handles open");
    }
    for m in &files {
        m.release(&fs);
        m.check(&fs, "after release");
    }
    drop(fs);
    let fs = common::reload_and_replay(store);
    for m in &files {
        m.check(&fs, "after reload and replay");
    }
}

#[test]
fn all_writers_throttled_while_a_commit_waits_flusher_paused() {
    let _dog = watchdog(Duration::from_secs(60));
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let (fs, files) = throttle_setup(&store);
    let budget = 4 * MIB;
    let writes: Vec<(u64, Writes)> = files.iter().map(|m| (m.fh, writer_pieces(m))).collect();
    std::thread::scope(|s| {
        let _stop = StopGuard(&fs);
        s.spawn(|| flusher::run_flusher(&fs));
        assert!(wait_until(WAIT, || fs.test_flusher_running()), "the flusher runs");
        fs.test_set_dirty_budget(budget);
        assert!(fs.test_dirty_total() >= budget, "the total is at or above the budget");
        let rx = spawn_writers(s, &fs, &writes);
        assert!(wait_until(WAIT, || fs.test_throttled_now() == 4), "the four writers wait in the throttle");

        let (commit_tx, commit_rx) = mpsc::channel();
        let fs_ref = &fs;
        s.spawn(move || {
            let _ = commit_tx.send(timed_commit(fs_ref, &fs_ref.commit_gate()).map(|_| ()).map_err(|e| e.to_string()));
        });
        let c = commit_rx.recv_timeout(Duration::from_secs(10)).expect("the commit returns within 10 s");
        assert!(c.is_ok(), "the commit is Ok: {c:?}");
        assert_eq!(fs.test_throttled_now(), 4, "the commit did not need a writer to move");

        fs.test_pause_flusher(false);
        all_writers_ok(&rx, 4, Duration::from_secs(10));
        let bound = budget + 4 * (PIECE + slicefs_cli::overlay::EXTENT_OVERHEAD);
        let total = fs.test_dirty_total();
        assert!(total < bound, "the total {total} is below the budget plus one write per writer ({bound})");
    });
    check_after_writers(fs, files, &store);
}

#[test]
fn all_writers_throttled_while_a_commit_waits_flusher_in_a_step() {
    let _dog = watchdog(Duration::from_secs(60));
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let (fs, files) = throttle_setup(&store);
    let budget = 4 * MIB;
    let writes: Vec<(u64, Writes)> = files.iter().map(|m| (m.fh, writer_pieces(m))).collect();
    // After the fill, so no fill write meets the hook. The writers wait in
    // the throttle and do not pay, so the first rebase is a flusher step.
    let (hook, parked, go) = park_first_rebase();
    fs.test_set_rebase_hook(Some(hook));
    std::thread::scope(|s| {
        let _stop = StopGuard(&fs);
        // Dropped before the scope joins (also on a failed assert): the
        // parked step goes on.
        let mut go = Some(go);
        s.spawn(|| flusher::run_flusher(&fs));
        assert!(wait_until(WAIT, || fs.test_flusher_running()), "the flusher runs");
        fs.test_set_dirty_budget(budget);
        assert!(fs.test_dirty_total() >= budget, "the total is at or above the budget");
        let rx = spawn_writers(s, &fs, &writes);
        assert!(wait_until(WAIT, || fs.test_throttled_now() == 4), "the four writers wait in the throttle");

        fs.test_pause_flusher(false);
        // The flusher's first step is in its I/O phase and holds the gate shared.
        parked.recv_timeout(WAIT).expect("the flusher step is parked");

        let (commit_tx, commit_rx) = mpsc::channel();
        let fs_ref = &fs;
        s.spawn(move || {
            let _ = commit_tx.send(timed_commit(fs_ref, &fs_ref.commit_gate()).map(|_| ()).map_err(|e| e.to_string()));
        });
        assert!(wait_until(Duration::from_secs(10), || fs.test_gate_exclusive_waiters() == 1),
            "the commit is queued for the exclusive gate");
        assert!(commit_rx.try_recv().is_err(), "the commit has not returned while the step holds the gate");

        go.take();
        let c = commit_rx.recv_timeout(Duration::from_secs(10)).expect("the commit returns within 10 s");
        assert!(c.is_ok(), "the commit is Ok: {c:?}");
        all_writers_ok(&rx, 4, Duration::from_secs(10));
        let st = fs.test_flusher_stats();
        assert!(st.published >= 1, "the flusher published a step: {st:?}");
    });
    fs.test_set_rebase_hook(None);
    check_after_writers(fs, files, &store);
}

#[test]
fn the_flusher_skips_a_cell_with_a_step_in_flight() {
    let _dog = watchdog(Duration::from_secs(60));
    let (fs, _dir) = fresh_fs();
    fs.test_set_dirty_limit(2 * MIB);
    // With the default budget: X about 1.9 MiB dirty in its first half
    // (slots 1 to 60 of 128), Y 1 MiB. Neither passes its limit.
    let mut x = dirty_file(&fs, 1, 8 * MIB, 60);
    let y = dirty_file(&fs, 2, 8 * MIB, 32);
    let (x_cost, y_cost) = (fs.test_dirty_cost(x.ino), fs.test_dirty_cost(y.ino));
    assert!(x_cost > 3 * MIB / 2 && x_cost < 2 * MIB, "X is dirty below its limit: {x_cost}");
    fs.test_set_dirty_budget(4 * MIB);
    assert!(fs.test_dirty_total() >= 2 * MIB, "the total is at or above half the budget");
    // 256 KiB in the clean second half of X: the write adds its full size,
    // so X passes its limit and the writer runs its own step on X.
    let (first, n) = (4 * MIB / BLOCK, 256 * KIB / BLOCK);
    let next: Vec<u32> = x.versions[first..first + n].iter().map(|v| v + 1).collect();
    let data = x.bytes(first, &next);
    let (hook, parked, go) = park_first_rebase();
    fs.test_set_rebase_hook(Some(hook));
    std::thread::scope(|s| {
        // Dropped before the scope joins (also on a failed assert): the
        // parked step goes on, so the writer and the pass end.
        let mut go = Some(go);
        let (fs_ref, data, x_fh) = (&fs, &data, x.fh);
        let (w_tx, w_rx) = mpsc::channel();
        s.spawn(move || {
            let _ = w_tx.send(throttled_write(fs_ref, x_fh, (4 * MIB) as u64, data).0);
        });
        // The writer holds the gate shared and the step token of X.
        parked.recv_timeout(WAIT).expect("the writer's own step on X is parked");
        let x_parked = fs.test_dirty_cost(x.ino);
        assert!(x_parked > 2 * MIB, "X is over its limit with the write: {x_parked}");

        let (p_tx, p_rx) = mpsc::channel();
        s.spawn(move || {
            let _ = p_tx.send(flusher::flush_pass(fs_ref));
        });
        let st = p_rx.recv_timeout(WAIT).expect("the pass returns within 5 s");
        assert!(w_rx.try_recv().is_err(), "the step on X is still parked when the pass returns");
        assert!(st.skips >= 1, "the pass skipped X: {st:?}");
        assert!(fs.test_dirty_cost(y.ino) < y_cost, "the pass stepped Y: {st:?}");
        assert_eq!(fs.test_dirty_cost(x.ino), x_parked, "the pass did not step X: {st:?}");

        go.take();
        let r = w_rx.recv_timeout(Duration::from_secs(10)).expect("the writer returns after the park");
        assert!(matches!(r, WriteResult::Ok(k) if k as usize == 256 * KIB), "the write is in: {r:?}");
    });
    fs.test_set_rebase_hook(None);
    for v in &mut x.versions[first..first + n] {
        *v += 1;
    }
    x.check(&fs, "handles open");
    y.check(&fs, "handles open");
    x.release(&fs);
    y.release(&fs);
    x.check(&fs, "after release");
    y.check(&fs, "after release");
}
