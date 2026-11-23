//! The dirty-data flusher (Phase 20, LOCK-06).
//!
//! One flusher thread lowers the shared dirty total with steps of the files
//! that have dirty data, so a writer over the budget does not have to pay
//! with steps of its own file. A writer over the budget waits in
//! `SliceFsFilesystem::throttle_write`, before it takes any lock, until the
//! flusher has freed memory or the wait limit passed.
//!
//! Lock rules: the flusher takes the commit gate shared for one step only,
//! never across a wait, and never for two files. It never waits for the
//! step token of a cell with the gate held; it skips that cell. It never
//! takes a cell out of the cell map.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::filesystem::{FlushCtl, FlushStep, FlushTarget, SliceFsFilesystem, FLUSH_STEP};
use crate::util::ts;

/// The counts of one flusher pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassStats {
    /// Steps tried (skipped ones too); at most one failed try per cell.
    pub steps: u64,
    /// Steps that lowered the cost of their file.
    pub published: u64,
    /// Steps that ran and freed nothing (a dropped result).
    pub drops: u64,
    /// Steps not started (a step in flight, a writer at the cap, retired).
    pub skips: u64,
    /// Steps that failed.
    pub errors: u64,
}

/// Clears `running` when the loop ends (also on a panic) and wakes every
/// throttled writer, so a writer does not wait for a flusher that is gone.
struct Running<'a>(&'a FlushCtl);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Release);
        chunk_store::sync::rank::require_gate(false);
        // A writer checks `running` under the flags lock (see `notify_freed`).
        self.0.notify_freed();
    }
}

/// The flusher loop on the calling thread (tests run it in a scoped
/// thread): see `flusher_loop`.
pub fn run_flusher(fs: &SliceFsFilesystem) {
    flusher_loop(fs.flush_ctl(), || {
        flush_pass(fs);
        true
    });
}

/// The flusher thread of a mount (`slicefs-flusher`), with the loop of
/// `run_flusher`. It holds the flush control block and a `Weak` of the
/// file system: it sleeps with no `Arc` of the file system, upgrades the
/// `Weak` for one pass only, and ends when the upgrade fails or at
/// `stop_flusher`.
pub fn spawn_flusher(fs: Weak<SliceFsFilesystem>) -> JoinHandle<()> {
    let ctl = fs.upgrade().map(|fs| Arc::clone(fs.flush_ctl()));
    std::thread::Builder::new()
        .name("slicefs-flusher".into())
        .spawn(move || {
            let Some(ctl) = ctl else { return };
            flusher_loop(&ctl, || match fs.upgrade() {
                Some(fs) => {
                    flush_pass(&fs);
                    true
                }
                None => false,
            });
        })
        .expect("spawn flusher thread")
}

/// Wait for the pass interval, a kick or a stop (the first wait comes
/// before the first pass), then run `pass`, until `stop_flusher` or until
/// `pass` returns false. A paused flusher (a test hook) makes no pass.
fn flusher_loop(ctl: &FlushCtl, mut pass: impl FnMut() -> bool) {
    chunk_store::sync::rank::require_gate(true);
    ctl.running.store(true, Ordering::Release);
    let _running = Running(ctl);
    loop {
        {
            let mut f = ctl.flags.lock();
            if !f.stop && !f.kicked {
                let pass = Duration::from_millis(ctl.pass_ms.load(Ordering::Relaxed));
                ctl.wake.wait_for(&mut f, pass);
            }
            if f.stop {
                return;
            }
            f.kicked = false;
            if f.paused {
                continue;
            }
        }
        if !pass() {
            return;
        }
    }
}

/// One flusher pass (also the entry of the tests), in two modes.
///
/// Budget-full mode: while the shared total is at or above half the
/// budget, or a quiet cell is at or above its own limit, step one
/// candidate: overlays before streams, quiet cells before hot ones, then
/// the largest. Below half the budget only a quiet cell at its limit is a
/// candidate. Idle and expire mode: then step each quiet dirty cell once
/// whose last write is at least the idle time old, or whose dirty data is
/// at least the maximum age old (`FileCell::dirty_since`); later passes go
/// on until the cell is clean. In this mode a stream with pending writes
/// may become an overlay (it is idle or old).
///
/// The hot rule: a cell with a write in the hot window is hot. Below the
/// budget the flusher never steps a hot cell, in any mode; if no quiet
/// candidate is left, the pass ends. It is "never" and not "prefer quiet":
/// at the default values the per-file limit is half the budget, so one
/// busy file at its limit keeps the budget-full mode on, with no other
/// candidate. Every write to an overlay moves its generation, so a step
/// of a file that is written during the step's I/O is dropped; and while
/// the flusher holds the step token, the writer's own limit step is
/// skipped. The writer of a hot file pays with its own limit steps, which
/// keep the file under the hard cap. At or above the budget, hot cells are
/// candidates of the budget-full mode: their writers wait in the throttle,
/// so the step publishes.
///
/// Each step is `FLUSH_STEP` in one gate hold
/// (`SliceFsFilesystem::flush_step`, which calls `free_dirty` on the
/// locked cell); the pick reads only atomics. After each step the
/// throttled writers are woken. A cell whose step freed nothing (skipped,
/// dropped or failed) is not tried again in this pass. A stop ends the
/// pass. At its end the pass records whether it tried no step
/// (`FlushCtl::empty_pass`): then writes below the budget do not kick the
/// flusher until a later pass tries a step.
pub fn flush_pass(fs: &SliceFsFilesystem) -> PassStats {
    let ctl = fs.flush_ctl();
    ctl.passes.fetch_add(1, Ordering::Relaxed);
    let mut st = PassStats::default();
    // Cells whose step freed nothing in this pass.
    let mut tried: HashSet<u64> = HashSet::new();
    let hot_ms = ctl.hot_ms.load(Ordering::Relaxed);
    let hot = |t: &FlushTarget, now: u64| now.saturating_sub(t.last_write()) < hot_ms;
    loop {
        if ctl.flags.lock().stop {
            return st;
        }
        let (total, budget) = fs.dirty_now();
        let limit = fs.dirty_limit_now();
        let now = ctl.now_ms();
        let (at_budget, full) = (total >= budget, total >= budget / 2);
        // A stream with pending writes only when no overlay candidate is
        // left (its writer would lose streaming).
        let Some(target) = fs
            .flush_targets()
            .into_iter()
            .filter(|t| !tried.contains(&t.ino()))
            .filter(|t| at_budget || !hot(t, now))
            .filter(|t| full || t.dirty() >= limit)
            .max_by_key(|t| (!t.is_stream(), !hot(t, now), t.dirty()))
        else {
            break;
        };
        step(fs, &target, &mut st, &mut tried);
    }
    let now = ctl.now_ms();
    let idle = ctl.idle_ms.load(Ordering::Relaxed);
    let expire = ctl.expire_ms.load(Ordering::Relaxed);
    for target in fs.flush_targets() {
        if ctl.flags.lock().stop {
            return st;
        }
        if tried.contains(&target.ino()) || hot(&target, now) {
            continue;
        }
        let since = target.dirty_since();
        let is_idle = now.saturating_sub(target.last_write()) >= idle;
        let is_old = since != 0 && now.saturating_sub(since) >= expire;
        if is_idle || is_old {
            step(fs, &target, &mut st, &mut tried);
        }
    }
    ctl.empty_pass.store(st.steps == 0, Ordering::Relaxed);
    st
}

/// One flusher step of `target` (the gate and the cell are released when
/// it returns), then wake the throttled writers and count the result. A
/// step that freed nothing puts the cell in `tried`.
fn step(fs: &SliceFsFilesystem, target: &FlushTarget, st: &mut PassStats, tried: &mut HashSet<u64>) {
    let ctl = fs.flush_ctl();
    let step = fs.flush_step(target, FLUSH_STEP);
    ctl.notify_freed();
    st.steps += 1;
    ctl.steps.fetch_add(1, Ordering::Relaxed);
    let counter = match step {
        FlushStep::Freed(_) => {
            st.published += 1;
            &ctl.published
        }
        FlushStep::Nothing => {
            st.drops += 1;
            &ctl.drops
        }
        FlushStep::Skipped => {
            st.skips += 1;
            &ctl.skips
        }
        FlushStep::Failed(e) => {
            st.errors += 1;
            eprintln!("[{}][FLUSH] step of ino={} failed (errno {e}); the file keeps its dirty data", ts(), target.ino());
            &ctl.errors
        }
    };
    counter.fetch_add(1, Ordering::Relaxed);
    if !matches!(step, FlushStep::Freed(_)) {
        tried.insert(target.ino());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metadata::store::DictMetadataStore;
    use metadata::store_io::StoreIo;
    use std::sync::{mpsc, Mutex};
    use std::time::Instant;

    const WAIT: Duration = Duration::from_secs(5);

    fn arc_fs(dir: &std::path::Path) -> Arc<SliceFsFilesystem> {
        let io = Arc::new(Mutex::new(StoreIo::open(dir).unwrap()));
        Arc::new(SliceFsFilesystem::new(DictMetadataStore::new(io.clone()), io, None))
    }

    fn wait_until(limit: Duration, f: impl Fn() -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < limit {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        f()
    }

    #[test]
    fn a_spawned_flusher_stops_and_joins() {
        let dir = tempfile::tempdir().unwrap();
        let fs = arc_fs(dir.path());
        fs.start_flusher();
        assert!(wait_until(WAIT, || fs.test_flusher_running()), "the flusher runs");
        let (tx, rx) = mpsc::channel();
        let fs2 = Arc::clone(&fs);
        std::thread::spawn(move || {
            fs2.stop_flusher();
            let _ = tx.send(());
        });
        rx.recv_timeout(WAIT).expect("stop_flusher returns within 5 s (the join ended)");
        assert!(!fs.test_flusher_running(), "no flusher runs after the join");
    }

    #[test]
    fn a_spawned_flusher_ends_when_the_fs_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let fs = arc_fs(dir.path());
        let h = spawn_flusher(Arc::downgrade(&fs));
        assert!(wait_until(WAIT, || fs.test_flusher_running()), "the flusher runs");
        drop(fs);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(h.join().is_ok());
        });
        let ok = rx.recv_timeout(WAIT).expect("the flusher thread ends within 5 s after the drop");
        assert!(ok, "the flusher thread ended with no panic");
    }
}
