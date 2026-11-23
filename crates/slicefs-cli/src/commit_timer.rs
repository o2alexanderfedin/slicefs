//! Periodic commit of the metadata store while mounted (#18).
//!
//! Metadata reaches disk only at `commit()`. Without this timer, commit runs
//! only at unmount, so a crash loses every change since mount. The timer
//! commits every `interval` when something changed, so a crash loses at most
//! about one interval of changes. Files still open at the crash keep the
//! content they had at their last release.
//!
//! A commit holds the filesystem's commit gate exclusively for its cut only.
//! Every FUSE callback holds it shared, so the cut never sees half of an
//! operation (for example an inode created but not yet linked). The cut
//! switches the generation (#108): it takes the metadata state and cuts the
//! write log, and requests after it go to the next generation. The slow part
//! (the metadata nodes, O(files), about 1.3 s at 78k files; the syncs; the
//! root record) runs after the gate is released, so FUSE requests wait only
//! for the cut.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::filesystem::SliceFsFilesystem;
use slicefs_traits::metadata::MetaError;

use crate::util::ts;

/// Default `--commit-interval` in seconds.
pub const DEFAULT_COMMIT_INTERVAL_SECS: u64 = 30;

/// How long a timed commit took.
#[derive(Debug, Clone, Copy)]
pub struct CommitTimes {
    /// Wait for the exclusive commit gate.
    pub gate_wait: Duration,
    /// Time the gate was held exclusively (FUSE requests wait this long).
    pub hold: Duration,
    /// Whole commit, from the gate request to the durable root.
    pub total: Duration,
}

/// One timed commit: a clean cut of the open files (#20) under the exclusive
/// gate, then the rest of the commit.
pub fn timed_commit(fs: &SliceFsFilesystem, gate: &RwLock<()>) -> Result<CommitTimes, MetaError> {
    Ok(commit(fs, gate, false)?.expect("a commit that does not check the shutdown always runs"))
}

/// The commit of the timer: `timed_commit` that does no commit and returns
/// `Ok(None)` when the file system is shut down
/// (`SliceFsFilesystem::shutdown`). The check runs again with the
/// exclusive gate held: a timer that waited for the gate while the destroy
/// work held it shared gets the gate only after that work, when the write
/// log is shut down and the active pack is sealed.
pub fn commit_unless_shut_down(fs: &SliceFsFilesystem, gate: &RwLock<()>) -> Result<Option<CommitTimes>, MetaError> {
    commit(fs, gate, true)
}

/// `timed_commit`; with `check_shutdown`, `Ok(None)` and no commit when the
/// file system is shut down (see `commit_unless_shut_down`).
fn commit(fs: &SliceFsFilesystem, gate: &RwLock<()>, check_shutdown: bool) -> Result<Option<CommitTimes>, MetaError> {
    if check_shutdown && fs.is_shut_down() {
        return Ok(None);
    }
    // Sync the objects so far before the gate: writes go on while
    // it runs, and the sync inside the commit (pre_root_sync) then
    // has only the bytes written since (#108). An error here is
    // seen again by that sync, which fails the commit.
    let _ = fs.content().sync();
    let w = Instant::now();
    // The gate's rank entry, for the span of the hold (D-16).
    let rank = chunk_store::sync::rank::enter(chunk_store::sync::Rank::Gate);
    // Counted while it waits for the gate (a test hook sees the queued commit).
    let waiting = fs.exclusive_wait();
    let all = gate.write().unwrap_or_else(|e| e.into_inner());
    drop(waiting);
    if check_shutdown && fs.is_shut_down() {
        return Ok(None);
    }
    let gate_wait = w.elapsed();
    let t = Instant::now();
    // Only the generation switch runs under the gate (#108).
    let cut = fs.commit_cut_begin();
    drop(all);
    drop(rank);
    let hold = t.elapsed();
    fs.commit_cut_finish(cut?)?;
    Ok(Some(CommitTimes { gate_wait, hold, total: w.elapsed() }))
}

/// Start the timer thread. It stops when `shutdown` is set, the store is
/// dropped, or a commit finds the file system shut down.
pub fn spawn_commit_timer(
    fs: Weak<SliceFsFilesystem>,
    gate: Arc<RwLock<()>>,
    interval: Duration,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("slicefs-commit".into())
        .spawn(move || loop {
            let start = Instant::now();
            while start.elapsed() < interval {
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100).min(interval));
            }
            let Some(fs) = fs.upgrade() else { return };
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            if !fs.meta().has_uncommitted_changes() {
                continue;
            }
            match commit_unless_shut_down(&fs, &gate) {
                // The file system is shut down: no commit is left to do.
                Ok(None) => return,
                Ok(Some(t)) => eprintln!("[{}][t={}][commit-timer] committed: gate_wait_ms={} hold_ms={} total_ms={} relog_bytes={}",
                    ts(), crate::util::epoch(), t.gate_wait.as_millis(), t.hold.as_millis(), t.total.as_millis(),
                    fs.test_last_relog_bytes()),
                Err(e) => eprintln!("[{}][commit-timer] commit failed: {}", ts(), e),
            }
        })
        .expect("spawn commit timer thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::SliceFsFilesystem;
    use metadata::store::DictMetadataStore;
    use slicefs_traits::metadata::MetadataStore;
    use metadata::store_io::StoreIo;
    use std::sync::Mutex;

    fn wait_until(limit: Duration, f: impl Fn() -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < limit {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        f()
    }

    #[test]
    fn timer_commits_changes_and_waits_for_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
        let fs = Arc::new(SliceFsFilesystem::new(DictMetadataStore::new(io.clone()), io.clone(), None));
        let stop = Arc::new(AtomicBool::new(false));
        let h = spawn_commit_timer(Arc::downgrade(&fs), fs.commit_gate(), Duration::from_millis(200), stop.clone());

        let (ino, fh) = fs.test_create(1, "f", 0o100_644, 0o022, 0, 0).unwrap();
        // An operation in progress holds the gate: no commit may happen meanwhile.
        {
            let gate = fs.commit_gate();
            let _op = gate.read().unwrap();
            fs.test_write(fh, 0, b"timer").unwrap();
            fs.test_release(ino, fh).unwrap();
            std::thread::sleep(Duration::from_millis(600));
            assert!(fs.meta().current_root().is_none(), "commit must wait for the operation");
        }
        assert!(wait_until(Duration::from_secs(5), || !fs.meta().has_uncommitted_changes()), "no timed commit");
        let root = fs.meta().current_root().unwrap();

        // The committed root has the file, as a reload after a crash would see it.
        let reloaded = DictMetadataStore::load_from_root(io.clone(), &root).unwrap();
        let fs2 = SliceFsFilesystem::new(reloaded, io, None);
        assert_eq!(fs2.meta().lookup(1, "f").unwrap(), ino);
        assert_eq!(fs2.test_read(ino, 0, 16).unwrap(), b"timer");

        // No change: no new commit.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(fs.meta().current_root(), Some(root));
        stop.store(true, Ordering::SeqCst);
        h.join().unwrap();
    }

    /// A file system on a disk store with a buffered write log, as a mount has.
    fn fs_on_disk(dir: &std::path::Path) -> Arc<SliceFsFilesystem> {
        use metadata::wal::{create_wal, WalConfig};
        std::fs::create_dir_all(dir.join("segments")).unwrap();
        let io = Arc::new(Mutex::new(StoreIo::open(dir).unwrap()));
        let mut meta = DictMetadataStore::new(io.clone());
        meta.set_wal(create_wal(WalConfig::FlushOnFsync, dir, 1).unwrap());
        let fs = Arc::new(SliceFsFilesystem::new(meta, io, Some(dir.to_path_buf())));
        fs.meta().commit().unwrap();
        fs
    }

    // IN-01 of the Phase 20 review: a commit of the timer that gets the gate
    // after `shutdown()` (it waited for the gate while the destroy work ran)
    // does no commit. The destroy work committed, then shut down the write
    // log and sealed the active pack. Before the fix the timer called
    // `timed_commit`, which made a full commit here.
    #[test]
    fn no_timed_commit_after_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let fs = fs_on_disk(dir.path());
        let (ino, fh) = fs.test_create(1, "f", 0o100_644, 0o022, 0, 0).unwrap();
        fs.test_write(fh, 0, b"down").unwrap();
        fs.test_release(ino, fh).unwrap();
        assert!(fs.shutdown(), "the destroy commit succeeded");
        let root = fs.meta().current_root();
        let r = commit_unless_shut_down(&fs, &fs.commit_gate());
        assert!(matches!(r, Ok(None)), "no timed commit after shutdown: {:?}", r.map(|t| t.map(|t| t.total)));
        assert_eq!(fs.meta().current_root(), root, "the root of the destroy work stays");
    }

    // IN-01: the timer loop after `shutdown()`. A change after the destroy
    // work makes the timer try a commit; it makes none, and the timer ends.
    #[test]
    fn the_timer_does_not_commit_after_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let fs = fs_on_disk(dir.path());
        assert!(fs.shutdown(), "the destroy commit succeeded");
        let root = fs.meta().current_root();
        fs.test_create(1, "late", 0o100_644, 0o022, 0, 0).unwrap();
        assert!(fs.meta().has_uncommitted_changes(), "a change after the destroy work");
        let stop = Arc::new(AtomicBool::new(false));
        let h = spawn_commit_timer(Arc::downgrade(&fs), fs.commit_gate(), Duration::from_millis(50), stop.clone());
        let ended = wait_until(Duration::from_secs(10), || h.is_finished());
        stop.store(true, Ordering::SeqCst);
        h.join().unwrap();
        assert!(ended, "the timer ended when it found the file system shut down");
        assert!(fs.meta().has_uncommitted_changes(), "the timer made no commit");
        assert_eq!(fs.meta().current_root(), root, "the root of the destroy work stays");
    }

    // #108 / S5: a timed commit holds the gate exclusively only to switch the
    // generation. Writing the metadata nodes and the syncs run with the gate
    // released, so a write that comes during the commit does not wait for it.
    #[test]
    fn a_timed_commit_holds_the_gate_for_a_small_part_of_the_commit() {
        const FILES: usize = 20_000;
        let dir = tempfile::tempdir().unwrap();
        let fs = fs_on_disk(dir.path());
        let gate = fs.commit_gate();
        for i in 0..FILES {
            let (ino, fh) = fs.test_create(1, &format!("f{i}"), 0o100_644, 0o022, 0, 0).unwrap();
            fs.test_write(fh, 0, &(i as u64).to_le_bytes()).unwrap();
            fs.test_release(ino, fh).unwrap();
        }
        let (ino, fh) = fs.test_create(1, "busy", 0o100_644, 0o022, 0, 0).unwrap();

        // A writer that takes the gate shared for each write, as a FUSE
        // callback does, and records its slowest write.
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (fs, gate, stop) = (Arc::clone(&fs), Arc::clone(&gate), Arc::clone(&stop));
            std::thread::spawn(move || {
                let (mut off, mut slowest) = (0u64, Duration::ZERO);
                let block = vec![7u8; 4096];
                while !stop.load(Ordering::SeqCst) {
                    let t = Instant::now();
                    {
                        let _op = gate.read().unwrap();
                        fs.test_write(fh, off, &block).unwrap();
                    }
                    slowest = slowest.max(t.elapsed());
                    off += block.len() as u64;
                    std::thread::sleep(Duration::from_millis(1));
                }
                slowest
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        let t = timed_commit(&fs, &gate).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        stop.store(true, Ordering::SeqCst);
        let slowest = writer.join().unwrap();
        fs.test_release(ino, fh).unwrap();
        eprintln!("commit of {FILES} files: hold {:?}, total {:?}, gate wait {:?}, slowest write {:?}",
            t.hold, t.total, t.gate_wait, slowest);
        assert!(t.hold * 4 < t.total, "the gate was held {:?} of a {:?} commit", t.hold, t.total);
        assert!(slowest * 3 < t.total, "a write waited {:?} during a {:?} commit", slowest, t.total);
    }
}
