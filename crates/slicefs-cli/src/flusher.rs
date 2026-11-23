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

use crate::filesystem::{FlushCtl, FlushStep, SliceFsFilesystem, FLUSH_STEP};
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

/// One flusher pass (also the entry of the tests). Budget-full mode: while
/// the shared total is at or above half the budget, step the candidate
/// with the most dirty cost, overlays before streams, one step of
/// `FLUSH_STEP` per gate hold (`SliceFsFilesystem::flush_step`, which calls
/// `free_dirty` on the locked cell). After each step the throttled writers
/// are woken. A cell whose step freed nothing (skipped, dropped or failed)
/// is not tried again in this pass, so the pass ends when the total is
/// below half the budget or no candidate is left.
pub fn flush_pass(fs: &SliceFsFilesystem) -> PassStats {
    let ctl = fs.flush_ctl();
    ctl.passes.fetch_add(1, Ordering::Relaxed);
    let mut st = PassStats::default();
    // Cells whose step freed nothing in this pass.
    let mut tried: HashSet<u64> = HashSet::new();
    loop {
        let (total, budget) = fs.dirty_now();
        if total < budget / 2 || ctl.flags.lock().stop {
            break;
        }
        // Overlays first, then the largest; a stream with pending writes
        // only when no overlay candidate is left (its writer would lose
        // streaming).
        let Some(target) = fs
            .flush_targets()
            .into_iter()
            .filter(|t| !tried.contains(&t.ino()))
            .max_by_key(|t| (!t.is_stream(), t.dirty()))
        else {
            break;
        };
        // The gate and the cell are released when this returns.
        let step = fs.flush_step(&target, FLUSH_STEP);
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
    st
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
