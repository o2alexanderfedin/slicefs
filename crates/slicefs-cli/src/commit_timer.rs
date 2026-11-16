//! Periodic commit of the metadata store while mounted (#18).
//!
//! Metadata reaches disk only at `commit()`. Without this timer, commit runs
//! only at unmount, so a crash loses every change since mount. The timer
//! commits every `interval` when something changed, so a crash loses at most
//! about one interval of changes. Files still open at the crash keep the
//! content they had at their last release.
//!
//! A commit holds the filesystem's commit gate exclusively. Every FUSE callback
//! holds it shared, so a commit never sees half of an operation (for example
//! an inode created but not yet linked). While a commit runs (O(files), about
//! 1.3 s at 78k files), FUSE requests wait.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::filesystem::SliceFsFilesystem;

use crate::util::ts;

/// Default `--commit-interval` in seconds.
pub const DEFAULT_COMMIT_INTERVAL_SECS: u64 = 30;

/// Start the timer thread. It stops when `shutdown` is set or the store is dropped.
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
            let _all = gate.write().unwrap_or_else(|e| e.into_inner());
            let t = Instant::now();
            // A clean cut: open files' content goes into the root (#20).
            match fs.commit_cut() {
                Ok(_) => eprintln!("[{}][commit-timer] committed in {} ms", ts(), t.elapsed().as_millis()),
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
        let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
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
}
