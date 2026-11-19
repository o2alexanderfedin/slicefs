//! GC of the stored objects while the store is mounted (#25).
//!
//! The mount runs a GC thread. It runs a GC:
//! - automatically, when something was committed since the last GC (or once
//!   more after that, to free the false positives of the last GC's live
//!   filter, #113), and at
//!   least `--gc-interval` seconds (0 = never automatically) and at least
//!   `GC_DUTY` times the duration of the last GC after it; and
//! - when `slicefs gc <store>` asks for one.
//!
//! The duty rule keeps GC at about 1/`GC_DUTY` of the time on any store size:
//! a small store gets a GC each minute, a store with a million objects less
//! often. (The time a GC waited for writes does not count as its duration.)
//!
//! Pacing (`gc_pacing`): a GC gives way to writes while the file system under
//! the store has space. When it has little (`gc_pacing::Mode::FullSpeed`), a
//! GC that is due by changes starts at once, without the interval or the
//! duty gap (still one at a time; an `--gc-interval` of 0 still means: only
//! on request).
//!
//! A request is a file in the store directory:
//! 1. `slicefs gc` writes `gc.request` (with the text `full` for a full GC,
//!    see `ChunkStore::gc_finish_full`; empty for a normal GC).
//! 2. The GC thread renames it to `gc.running` (within about one second),
//!    runs the GC, writes the outcome to `gc.result`, and removes
//!    `gc.running`.
//! 3. `slicefs gc` waits for `gc.result`. If no mount takes the request in
//!    time, it stops with an error (for example, the daemon is dead).
//!
//! The thread takes a request only between two GC runs, and an automatic GC
//! of a large store can take minutes. While a GC runs, the thread refreshes
//! the heartbeat file `gc.active`; `slicefs gc` keeps waiting for the pickup
//! while the heartbeat is fresh. (A heartbeat left by a dead daemon gets old.)

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::filesystem::SliceFsFilesystem;
use crate::gc_pacing::{self, FreeRatio, Mode, RealClock, WritePacer};

/// Default minimum seconds between two automatic GC runs on a mounted store.
pub const DEFAULT_GC_INTERVAL_SECS: u64 = 60;

/// The gap after a GC is at least this many times its duration.
const GC_DUTY: u32 = 10;

pub const REQUEST_FILE: &str = "gc.request";
pub const RUNNING_FILE: &str = "gc.running";
pub const RESULT_FILE: &str = "gc.result";
pub const ACTIVE_FILE: &str = "gc.active";

/// How often a running GC refreshes `gc.active`.
const HEARTBEAT: Duration = Duration::from_secs(1);

/// How often the GC thread looks for a request (and for shutdown).
const POLL: Duration = Duration::from_millis(500);

/// How long `request_gc` waits for a mount to take the request.
const PICKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Run `f` while refreshing the heartbeat file `active` (see the module docs).
fn with_heartbeat<T>(active: &Path, f: impl FnOnce() -> T) -> T {
    let done = AtomicBool::new(false);
    let out = std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                let _ = std::fs::write(active, b"");
                let t = Instant::now();
                while t.elapsed() < HEARTBEAT && !done.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        });
        let out = f();
        done.store(true, Ordering::SeqCst);
        out
    });
    // The heartbeat thread has ended here (end of the scope).
    let _ = std::fs::remove_file(active);
    out
}

/// True if `active` was refreshed within the pickup timeout.
fn heartbeat_is_fresh(active: &Path) -> bool {
    std::fs::metadata(active)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < PICKUP_TIMEOUT)
}

/// The text of `gc.request` (and of `gc.running`) for a full GC.
pub const FULL: &str = "full";

/// Run one GC (a full GC if `full`) on `fs`, paced by the writes and the
/// free ratio `free` (see `gc_pacing`, with the yield caps `caps` of the mark
/// and the sweep). Gives a one-line outcome for `gc.result`, with the disk
/// usage of `store` before and after, and the time the GC waited for writes.
fn run_once(
    fs: &SliceFsFilesystem,
    store: &Path,
    full: bool,
    stop: &AtomicBool,
    free: &FreeRatio,
    caps: (Duration, Duration),
) -> (String, Duration) {
    let t = Instant::now();
    let before = crate::gc::store_usage(store);
    let pacer = |cap| WritePacer::new(fs.write_activity(), Arc::clone(free), Box::new(RealClock::new()), cap, stop);
    let (mark, sweep) = (pacer(caps.0), pacer(caps.1));
    // A panic in the GC must not end the GC thread (or leave gc.running).
    let gc = || fs.gc_online_paced(full, stop, &mark, &sweep);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(gc))
        .unwrap_or_else(|p| {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown".into());
            Err(format!("GC panicked: {msg}"))
        });
    let waited = mark.waited() + sweep.waited();
    let line = match result {
        Ok(Some(s)) => format!(
            "ok{}: {}; {} in {:.1} s ({:.1} s waited for writes)",
            if full { " (full)" } else { "" },
            crate::gc::stats_line(&s),
            crate::gc::usage_line(before, crate::gc::store_usage(store)),
            t.elapsed().as_secs_f64(),
            waited.as_secs_f64()
        ),
        Ok(None) => "stopped: the mount is shutting down".to_string(),
        Err(e) => format!("error: {e}"),
    };
    (line, waited)
}

/// Whether an automatic GC is due: `since_last` after the end of the last GC,
/// which took `last_took`, in pacing mode `mode`. An `interval` of zero:
/// never. With low space (`Mode::FullSpeed`): at once.
fn gc_due(interval: Duration, since_last: Duration, last_took: Duration, mode: Mode) -> bool {
    !interval.is_zero() && (mode == Mode::FullSpeed || since_last >= interval.max(last_took * GC_DUTY))
}

/// Whether the GC thread skips a GC that is due. The mark is a Bloom filter
/// with a new random salt for each GC (#113), so a GC can keep a few dead
/// objects (false positives), and the store cannot tell which. Thus after a
/// change, the thread runs one more GC on the unchanged tree to free them, and
/// skips only after that (an object stays only if both filters keep it).
fn skip_due_gc(unchanged: bool, rechecked: bool) -> bool {
    unchanged && rechecked
}

/// Spawn the GC thread of a mount. `interval` of zero means: only on request.
/// The thread ends when `shutdown` is set or the file system is gone; a GC
/// that runs then stops at its next check.
pub fn spawn_online_gc(
    fs: Weak<SliceFsFilesystem>,
    store: PathBuf,
    interval: Duration,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    let free = gc_pacing::store_free_ratio(&store);
    let caps = (gc_pacing::MARK_YIELD_CAP, gc_pacing::SWEEP_YIELD_CAP);
    spawn_online_gc_with(fs, store, interval, shutdown, free, caps)
}

/// [`spawn_online_gc`] with the free ratio reader `free` and the yield caps
/// `caps` (mark, sweep) given (tests).
pub fn spawn_online_gc_with(
    fs: Weak<SliceFsFilesystem>,
    store: PathBuf,
    interval: Duration,
    shutdown: Arc<AtomicBool>,
    free: FreeRatio,
    caps: (Duration, Duration),
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("slicefs-gc".into())
        .spawn(move || {
            let mut last = Instant::now();
            let mut last_took = Duration::ZERO;
            let mut last_root = None;
            // The last GC ran on a tree that did not change since the GC
            // before it (see `skip_due_gc`).
            let mut rechecked = false;
            // Files left by a daemon that died during a GC.
            let _ = std::fs::remove_file(store.join(ACTIVE_FILE));
            let _ = std::fs::remove_file(store.join(RUNNING_FILE));
            loop {
                std::thread::sleep(POLL);
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let requested = std::fs::rename(store.join(REQUEST_FILE), store.join(RUNNING_FILE)).is_ok();
                let full = requested
                    && std::fs::read_to_string(store.join(RUNNING_FILE)).is_ok_and(|t| t.trim() == FULL);
                let due = !requested
                    && !interval.is_zero()
                    && gc_due(interval, last.elapsed(), last_took, gc_pacing::mode_for(free()));
                if !requested && !due {
                    continue;
                }
                let Some(fs) = fs.upgrade() else { break };
                // (With --commit-interval 0 only the GC commits, so also look
                // at changes that are not committed yet.)
                let unchanged = last_root.is_some()
                    && fs.meta().current_root() == last_root
                    && !fs.meta().has_uncommitted_changes();
                if !requested && skip_due_gc(unchanged, rechecked) {
                    last = Instant::now();
                    continue;
                }
                rechecked = unchanged;
                let t = Instant::now();
                let (outcome, waited) = with_heartbeat(&store.join(ACTIVE_FILE), || {
                    run_once(&fs, &store, full, &shutdown, &free, caps)
                });
                last_took = t.elapsed().saturating_sub(waited);
                last_root = fs.meta().current_root();
                drop(fs);
                last = Instant::now();
                eprintln!("[slicefs-gc] {outcome}");
                if requested {
                    let _ = std::fs::write(store.join(RESULT_FILE), format!("{outcome}\n"));
                    let _ = std::fs::remove_file(store.join(RUNNING_FILE));
                }
            }
        })
        .expect("spawn GC thread")
}

/// Ask the mount of `store` to run a GC, wait for it, and return its outcome
/// line. Used by `slicefs gc` when the store is mounted.
pub fn request_gc(store: &Path) -> Result<String, String> {
    request_gc_mode(store, false)
}

/// [`request_gc`], for a full GC if `full`.
pub fn request_gc_mode(store: &Path, full: bool) -> Result<String, String> {
    let (request, running, result) = (store.join(REQUEST_FILE), store.join(RUNNING_FILE), store.join(RESULT_FILE));
    let _ = std::fs::remove_file(&result);
    std::fs::write(&request, if full { FULL } else { "" }).map_err(|e| format!("cannot write {}: {e}", request.display()))?;
    let mut t = Instant::now();
    while request.exists() {
        if heartbeat_is_fresh(&store.join(ACTIVE_FILE)) {
            t = Instant::now(); // a GC runs now; the request comes next
        }
        if t.elapsed() > PICKUP_TIMEOUT {
            let _ = std::fs::remove_file(&request);
            return Err(format!(
                "the store is mounted (mount.lock), but no running mount took the GC request in {} s (is the daemon alive?)",
                PICKUP_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    loop {
        if let Ok(text) = std::fs::read_to_string(&result) {
            if !text.is_empty() {
                let _ = std::fs::remove_file(&result);
                return Ok(text.trim().to_string());
            }
        }
        if !running.exists() && !result.exists() && !store.join("mount.lock").exists() {
            return Err("the mount ended before the GC finished".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unchanged_tree_gets_one_more_gc_then_none() {
        // A change: the GC runs, and the next GC on the same tree runs too.
        assert!(!skip_due_gc(false, false));
        assert!(!skip_due_gc(true, false));
        // After that GC on the unchanged tree, the thread skips.
        assert!(skip_due_gc(true, true));
        // A new change ends the skip.
        assert!(!skip_due_gc(false, true));
    }

    #[test]
    fn low_space_makes_a_gc_due_without_the_interval_or_the_duty_gap() {
        let hour = Duration::from_secs(3600);
        let s = Duration::from_secs(1);
        // Plenty or medium space: the interval and the duty gap apply.
        assert!(!gc_due(hour, s, s, Mode::Yield));
        assert!(!gc_due(hour, s, s, Mode::Throttle));
        assert!(gc_due(hour, hour, s, Mode::Yield));
        assert!(!gc_due(Duration::from_secs(60), Duration::from_secs(60), Duration::from_secs(10), Mode::Yield));
        // Low space: at once.
        assert!(gc_due(hour, Duration::ZERO, hour, Mode::FullSpeed));
        // An interval of zero means only on request, also with low space.
        assert!(!gc_due(Duration::ZERO, hour, s, Mode::FullSpeed));
    }

    #[test]
    fn a_request_without_a_mount_times_out_and_is_removed() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = request_gc(dir.path()).unwrap_err();
        assert!(err.contains("no running mount"), "{err}");
        assert!(!dir.path().join(REQUEST_FILE).exists());
    }

    #[test]
    fn a_request_waits_while_a_gc_runs_and_is_taken_after_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = dir.path().to_path_buf();
        // A "daemon": one long GC (longer than the pickup timeout) with the
        // heartbeat, then it takes the request.
        let d = store.clone();
        let daemon = std::thread::spawn(move || {
            with_heartbeat(&d.join(ACTIVE_FILE), || std::thread::sleep(PICKUP_TIMEOUT + Duration::from_secs(3)));
            while std::fs::rename(d.join(REQUEST_FILE), d.join(RUNNING_FILE)).is_err() {
                std::thread::sleep(Duration::from_millis(50));
            }
            std::fs::write(d.join(RESULT_FILE), "ok: test\n").unwrap();
            std::fs::remove_file(d.join(RUNNING_FILE)).unwrap();
        });
        std::fs::write(store.join("mount.lock"), b"").unwrap();
        while !store.join(ACTIVE_FILE).exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(request_gc(&store).unwrap(), "ok: test");
        daemon.join().unwrap();
        assert!(!store.join(ACTIVE_FILE).exists(), "the heartbeat file is removed after the GC");
    }

    #[test]
    fn an_old_heartbeat_does_not_keep_the_request_waiting() {
        let dir = tempfile::TempDir::new().unwrap();
        let active = dir.path().join(ACTIVE_FILE);
        std::fs::write(&active, b"").unwrap();
        let old = std::time::SystemTime::now() - PICKUP_TIMEOUT - Duration::from_secs(5);
        std::fs::File::options().write(true).open(&active).unwrap().set_modified(old).unwrap();
        assert!(!heartbeat_is_fresh(&active));
        let t = Instant::now();
        assert!(request_gc(dir.path()).is_err());
        assert!(t.elapsed() < PICKUP_TIMEOUT + Duration::from_secs(3));
    }
}
