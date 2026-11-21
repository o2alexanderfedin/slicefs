//! Lock hold and wait times, for the stress harness and the spikes (Phase 18).
//!
//! Every timed lock site calls [`lock`] with its [`Site`] instead of
//! `mutex.lock().unwrap()`.
//!
//! - Feature `lock-stats` off (default): [`lock`] is `m.lock().unwrap()` and
//!   [`Guard`] is the std guard. No timing, no `hdrhistogram` dependency.
//! - Feature `lock-stats` on: [`Guard`] times the wait (zero when the lock was
//!   free) and the hold of each acquisition, and records both in one
//!   `hdrhistogram` histogram per site, in nanoseconds.
//!
//! The recorder is process-global, so tests that run in parallel in one binary
//! share it. Call [`reset`] at the start of a measured run.

use std::sync::{Mutex, MutexGuard};

/// A lock site. All sites are defined here; a site is timed once its code
/// calls [`lock`] with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Site {
    /// `ChunkStore.inner`, the store lock.
    Store,
    /// The `StoreIo` lock of the metadata crate.
    Io,
    /// The WAL state lock.
    Wal,
    /// A WAL strategy `writer` lock.
    WalWriter,
    /// The WAL pending-append lock.
    WalPending,
    /// `open_files` of the file system core.
    OpenFiles,
    /// `readers` of the file system core.
    Readers,
}

impl Site {
    pub const ALL: [Site; 7] = [
        Site::Store,
        Site::Io,
        Site::Wal,
        Site::WalWriter,
        Site::WalPending,
        Site::OpenFiles,
        Site::Readers,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Site::Store => "store",
            Site::Io => "io",
            Site::Wal => "wal",
            Site::WalWriter => "wal_writer",
            Site::WalPending => "wal_pending",
            Site::OpenFiles => "open_files",
            Site::Readers => "readers",
        }
    }
}

/// True when this build records lock times.
pub const ENABLED: bool = cfg!(feature = "lock-stats");

/// Times of one site, in nanoseconds. `count` is the number of acquisitions.
#[derive(Clone, Debug)]
pub struct SiteStats {
    pub label: &'static str,
    pub count: u64,
    pub wait_p50_ns: u64,
    pub wait_p99_ns: u64,
    pub wait_max_ns: u64,
    pub hold_p50_ns: u64,
    pub hold_p99_ns: u64,
    pub hold_max_ns: u64,
}

#[cfg(not(feature = "lock-stats"))]
pub type Guard<'a, T> = MutexGuard<'a, T>;

/// Lock `m` (panics on poison, as `unwrap` does).
#[cfg(not(feature = "lock-stats"))]
#[inline(always)]
pub fn lock<T>(m: &Mutex<T>, _site: Site) -> Guard<'_, T> {
    m.lock().unwrap()
}

/// One entry per site, in `Site::ALL` order. Empty when the feature is off.
#[cfg(not(feature = "lock-stats"))]
pub fn snapshot() -> Vec<SiteStats> {
    Vec::new()
}

/// Clear all histograms. A no-op when the feature is off.
#[cfg(not(feature = "lock-stats"))]
pub fn reset() {}

#[cfg(feature = "lock-stats")]
pub use on::{lock, reset, snapshot, TimedGuard as Guard};

#[cfg(feature = "lock-stats")]
mod on {
    use super::{Mutex, MutexGuard, Site, SiteStats};
    use hdrhistogram::Histogram;
    use std::ops::{Deref, DerefMut};
    use std::sync::{OnceLock, TryLockError};
    use std::time::Instant;

    fn hist() -> Histogram<u64> {
        Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3).unwrap()
    }

    struct SiteHists {
        wait: Histogram<u64>,
        hold: Histogram<u64>,
    }

    impl SiteHists {
        fn new() -> Self {
            Self { wait: hist(), hold: hist() }
        }
    }

    fn recorders() -> &'static [Mutex<SiteHists>; 7] {
        static R: OnceLock<[Mutex<SiteHists>; 7]> = OnceLock::new();
        R.get_or_init(|| std::array::from_fn(|_| Mutex::new(SiteHists::new())))
    }

    fn record(site: Site, wait_ns: u64, hold_ns: u64) {
        // A poisoned recorder still holds valid histograms.
        let mut r = recorders()[site as usize].lock().unwrap_or_else(|e| e.into_inner());
        r.wait.saturating_record(wait_ns);
        r.hold.saturating_record(hold_ns);
    }

    /// The std guard plus the times of this acquisition. Derefs to `T`.
    pub struct TimedGuard<'a, T> {
        g: Option<MutexGuard<'a, T>>,
        site: Site,
        wait_ns: u64,
        acquired: Instant,
    }

    impl<T> Deref for TimedGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            self.g.as_ref().expect("guard is present until drop")
        }
    }

    impl<T> DerefMut for TimedGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            self.g.as_mut().expect("guard is present until drop")
        }
    }

    impl<T> Drop for TimedGuard<'_, T> {
        fn drop(&mut self) {
            let hold_ns = self.acquired.elapsed().as_nanos() as u64;
            // Release the site lock first: the recorder never runs inside a
            // site lock and its cost is not in the measured hold.
            drop(self.g.take());
            record(self.site, self.wait_ns, hold_ns);
        }
    }

    /// Lock `m` (panics on poison, as `unwrap` does) and time the wait and the
    /// hold. The wait is 0 when the lock was free.
    pub fn lock<T>(m: &Mutex<T>, site: Site) -> TimedGuard<'_, T> {
        let (g, wait_ns) = match m.try_lock() {
            Ok(g) => (g, 0),
            Err(TryLockError::WouldBlock) => {
                let t = Instant::now();
                let g = m.lock().unwrap();
                (g, t.elapsed().as_nanos() as u64)
            }
            // Same panic as `m.lock().unwrap()`.
            Err(TryLockError::Poisoned(_)) => (m.lock().unwrap(), 0),
        };
        TimedGuard { g: Some(g), site, wait_ns, acquired: Instant::now() }
    }

    /// One entry per site, in `Site::ALL` order.
    pub fn snapshot() -> Vec<SiteStats> {
        Site::ALL
            .iter()
            .map(|&site| {
                let r = recorders()[site as usize].lock().unwrap_or_else(|e| e.into_inner());
                SiteStats {
                    label: site.label(),
                    count: r.hold.len(),
                    wait_p50_ns: r.wait.value_at_quantile(0.50),
                    wait_p99_ns: r.wait.value_at_quantile(0.99),
                    wait_max_ns: r.wait.max(),
                    hold_p50_ns: r.hold.value_at_quantile(0.50),
                    hold_p99_ns: r.hold.value_at_quantile(0.99),
                    hold_max_ns: r.hold.max(),
                }
            })
            .collect()
    }

    /// Clear all histograms.
    pub fn reset() {
        for r in recorders() {
            let mut r = r.lock().unwrap_or_else(|e| e.into_inner());
            r.wait.reset();
            r.hold.reset();
        }
    }
}
