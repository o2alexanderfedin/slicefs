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

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
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
