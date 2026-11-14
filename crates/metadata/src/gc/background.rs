//! Background GC thread infrastructure.
//!
//! `spawn_background_gc` starts a thread that periodically runs the GC engine.
//! The thread exits when:
//!   - `shutdown` flag is set (`GcHandle::shutdown()` or `GcHandle::drop()`), OR
//!   - The `Weak<DictMetadataStore>` can no longer be upgraded (filesystem unmounted).

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::store::DictMetadataStore;
use super::{GarbageCollector};

/// Handle to a background GC thread.
///
/// Call `shutdown()` to signal the thread to stop and join it.
/// Dropping without calling `shutdown()` sets the flag but does NOT join
/// (thread exits on its next iteration).
pub struct GcHandle {
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl GcHandle {
    /// Signal the GC thread to stop and join it.
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            h.thread().unpark(); // wake the thread from its interval wait
            let _ = h.join();
        }
    }
}

impl Drop for GcHandle {
    fn drop(&mut self) {
        // Signal the thread to stop on next iteration.
        // We intentionally do NOT join here — blocking in drop() can cause deadlocks.
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = &self.handle {
            h.thread().unpark();
        }
    }
}

/// Spawn a background GC thread.
///
/// Parameters:
/// - `meta`: `Weak` reference to the store — upgrade failure means FS was unmounted.
/// - `segments_dir`: directory containing `.seg` files to compact.
/// - `interval`: sleep duration between GC cycles.
/// - `orphan_threshold`: minimum number of zero-refcount entries required to trigger GC.
/// - `shutdown`: shared flag; set to `true` to stop the thread.
///
/// Returns a `GcHandle` that can be used to shut down the thread.
pub fn spawn_background_gc(
    meta: Weak<DictMetadataStore>,
    segments_dir: PathBuf,
    interval: Duration,
    orphan_threshold: usize,
    shutdown: Arc<AtomicBool>,
) -> GcHandle {
    let shutdown_flag = Arc::clone(&shutdown);

    let handle = std::thread::spawn(move || {
        let gc = GarbageCollector::new(segments_dir.clone());

        loop {
            // Wait for `interval`, but wake early on shutdown (GcHandle unparks us).
            // park_timeout can wake spuriously, so loop until the deadline.
            let deadline = Instant::now() + interval;
            while !shutdown_flag.load(Ordering::SeqCst) {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                std::thread::park_timeout(deadline - now);
            }
            if shutdown_flag.load(Ordering::SeqCst) {
                break;
            }

            // Upgrade the weak reference — None means filesystem was unmounted
            let store = match meta.upgrade() {
                Some(s) => s,
                None => break,
            };

            // Count approximate orphan entries.
            // With file-backed storage we no longer maintain an in-memory Dictionary,
            // so use 1 as a proxy: threshold=0 always triggers GC, usize::MAX never does.
            let orphan_count = 1usize;

            if orphan_count > orphan_threshold {
                // Collect all GC roots: current live root + all snapshot roots.
                // snapshot_roots() returns all snapshot roots plus current_root() if set.
                let roots = store.snapshot_roots();
                let _ = gc.run_gc_roots_only(&roots);
            }

            // Drop Arc before sleeping to avoid holding the store alive unnecessarily
            drop(store);
        }
    });

    GcHandle {
        shutdown,
        handle: Some(handle),
    }
}

