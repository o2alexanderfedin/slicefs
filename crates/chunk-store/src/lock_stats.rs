//! Lock hold and wait times, for the stress harness and the spikes (Phase 18).
//!
//! Locks of the shim ([`crate::sync`]) built with `.timed(site)` record their
//! times here. A std mutex (the `StoreIo` lock, which stays std) calls [`lock`]
//! with its [`Site`] instead of `mutex.lock().unwrap()`.
//!
//! - Feature `lock-stats` off (default): [`lock`] is `m.lock().unwrap()` and
//!   [`Guard`] is the std guard. No timing, no `hdrhistogram` dependency.
//! - Feature `lock-stats` on: the shim guards and [`Guard`] time the wait (zero
//!   when the lock was free) and the hold of each acquisition, and record both
//!   in one `hdrhistogram` histogram per site, in nanoseconds.
//!
//! The recorder is process-global, so tests that run in parallel in one binary
//! share it. Call [`reset`] at the start of a measured run.

use std::sync::{Mutex, MutexGuard};

/// A lock site. All sites are defined here; a site is timed once a lock is
/// built with `.timed(site)` or its code calls [`lock`] with it.
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
    /// The in-memory record queue of the per-operation WAL (append takes
    /// only this lock).
    WalQueue,
    /// `readers` of the file system core.
    Readers,
    /// A shard of `dir_live`, the directory cache of the metadata store.
    DirLive,
    /// A shard of `inode_live`, the inode cache of the metadata store.
    InodeLive,
    /// The handle table of the file system core.
    Handles,
    /// The state lock of one open file (`FileCell`) of the file system core.
    FileCell,
    /// A shard of the cell map (`ino -> FileCell`) of the file system core.
    CellMap,
    /// A namespace stripe of the file system core (`NamespaceLocks`).
    Stripe,
    /// The inode number allocator of the metadata store.
    InodeAlloc,
    /// `inode_data` of the metadata store (inode -> data key as of the last flush).
    InodeData,
    /// `dir_data` of the metadata store (directory -> entry list key).
    DirData,
    /// `manifest_data` of the metadata store (file -> manifest key).
    ManifestData,
    /// `xattr_data` of the metadata store (inode -> xattr set key).
    XattrData,
    /// `refcounts` of the metadata store (content key -> reference count).
    Refcounts,
}

impl Site {
    /// Every site, in discriminant order (a test checks it).
    pub const ALL: &'static [Site] = &[
        Site::Store,
        Site::Io,
        Site::Wal,
        Site::WalWriter,
        Site::WalPending,
        Site::WalQueue,
        Site::Readers,
        Site::DirLive,
        Site::InodeLive,
        Site::Handles,
        Site::FileCell,
        Site::CellMap,
        Site::Stripe,
        Site::InodeAlloc,
        Site::InodeData,
        Site::DirData,
        Site::ManifestData,
        Site::XattrData,
        Site::Refcounts,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Site::Store => "store",
            Site::Io => "io",
            Site::Wal => "wal",
            Site::WalWriter => "wal_writer",
            Site::WalPending => "wal_pending",
            Site::WalQueue => "wal_queue",
            Site::Readers => "readers",
            Site::DirLive => "dir_live",
            Site::InodeLive => "inode_live",
            Site::Handles => "handles",
            Site::FileCell => "file_cell",
            Site::CellMap => "cell_map",
            Site::Stripe => "stripe",
            Site::InodeAlloc => "inode_alloc",
            Site::InodeData => "inode_data",
            Site::DirData => "dir_data",
            Site::ManifestData => "manifest_data",
            Site::XattrData => "xattr_data",
            Site::Refcounts => "refcounts",
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

/// Record one acquisition of `site`; the shim guards call this when dropped.
#[cfg(feature = "lock-stats")]
pub(crate) use on::record;

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

    /// One recorder per site; `Site as usize` indexes it.
    const N: usize = Site::ALL.len();

    fn recorders() -> &'static [Mutex<SiteHists>; N] {
        static R: OnceLock<[Mutex<SiteHists>; N]> = OnceLock::new();
        R.get_or_init(|| std::array::from_fn(|_| Mutex::new(SiteHists::new())))
    }

    pub(crate) fn record(site: Site, wait_ns: u64, hold_ns: u64) {
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

#[cfg(test)]
mod tests {
    use super::Site;

    #[test]
    fn all_lists_every_site_in_discriminant_order() {
        for (i, s) in Site::ALL.iter().enumerate() {
            assert_eq!(*s as usize, i, "{}", s.label());
        }
        let mut labels: Vec<_> = Site::ALL.iter().map(|s| s.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), Site::ALL.len());
    }
}
