//! GC of the stored objects while the store is mounted (#25).
//!
//! The mount runs a GC thread. It runs a GC:
//! - every `--gc-interval` seconds (0 = only on request), and
//! - when `slicefs gc <store>` asks for one.
//!
//! A request is a file in the store directory:
//! 1. `slicefs gc` writes `gc.request`.
//! 2. The GC thread renames it to `gc.running` (within about one second),
//!    runs the GC, writes the outcome to `gc.result`, and removes
//!    `gc.running`.
//! 3. `slicefs gc` waits for `gc.result`. If no mount takes the request in
//!    time, it stops with an error (for example, the daemon is dead).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::filesystem::SliceFsFilesystem;

/// Default seconds between two automatic GC runs on a mounted store.
pub const DEFAULT_GC_INTERVAL_SECS: u64 = 3600;

pub const REQUEST_FILE: &str = "gc.request";
pub const RUNNING_FILE: &str = "gc.running";
pub const RESULT_FILE: &str = "gc.result";

/// How often the GC thread looks for a request (and for shutdown).
const POLL: Duration = Duration::from_millis(500);

/// How long `request_gc` waits for a mount to take the request.
const PICKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Run one GC on `fs` and give a one-line outcome for `gc.result`.
fn run_once(fs: &SliceFsFilesystem, stop: &AtomicBool) -> String {
    let t = Instant::now();
    match fs.gc_online(stop) {
        Ok(Some(s)) => format!(
            "ok: kept {} objects, removed {} ({} bytes) in {:.1} s",
            s.objects_kept, s.objects_removed, s.bytes_removed, t.elapsed().as_secs_f64()
        ),
        Ok(None) => "stopped: the mount is shutting down".to_string(),
        Err(e) => format!("error: {e}"),
    }
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
    std::thread::Builder::new()
        .name("slicefs-gc".into())
        .spawn(move || {
            let mut last = Instant::now();
            loop {
                std::thread::sleep(POLL);
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let requested = std::fs::rename(store.join(REQUEST_FILE), store.join(RUNNING_FILE)).is_ok();
                let due = !interval.is_zero() && last.elapsed() >= interval;
                if !requested && !due {
                    continue;
                }
                let Some(fs) = fs.upgrade() else { break };
                let outcome = run_once(&fs, &shutdown);
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
    let (request, running, result) = (store.join(REQUEST_FILE), store.join(RUNNING_FILE), store.join(RESULT_FILE));
    let _ = std::fs::remove_file(&result);
    std::fs::write(&request, b"").map_err(|e| format!("cannot write {}: {e}", request.display()))?;
    let t = Instant::now();
    while request.exists() {
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
    fn a_request_without_a_mount_times_out_and_is_removed() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = request_gc(dir.path()).unwrap_err();
        assert!(err.contains("no running mount"), "{err}");
        assert!(!dir.path().join(REQUEST_FILE).exists());
    }
}
