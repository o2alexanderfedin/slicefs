//! The lock API of the hot crates (`chunk-store`, `metadata`, `slicefs-cli`):
//! `parking_lot` locks with a rank, a debug-build order checker and the
//! `lock-stats` timing (Phase 19).
//!
//! - [`Mutex`], [`RwLock`] and [`Condvar`] wrap the `parking_lot` types. There
//!   is no poison: a lock is free again after a panic.
//! - Every lock has a [`Rank`]. In a debug build a thread-local stack holds the
//!   ranks the thread has locked; taking a lock against the rank panics with
//!   `lock rank violation` (see the rules at [`Rank`]). A release build has no
//!   stack and no checks.
//! - A lock built with `.timed(site)` records its wait and hold time into
//!   [`crate::lock_stats`] when the `lock-stats` feature is on; with the feature
//!   off there is no timing code.
//! - The commit gate is a std lock that this shim does not own. [`rank::enter`]
//!   puts a rank-1 entry on the stack while the thread holds the gate, and
//!   [`rank::require_gate`] turns on the rule that a Stripe or Cell lock needs
//!   that entry.
//!
//! The std `Mutex<StoreIo>` and the commit gate stay std types (Phase 19, D-18).

use std::ops::{Deref, DerefMut};
use std::time::Duration;

use crate::lock_stats::Site;

pub use parking_lot::WaitTimeoutResult;

/// Std-shaped primitives for the code that uses std locks on purpose (the
/// request pool, the commit gate, the flusher atomics). This is the one cfg
/// point of the models (Phase 21.2): with `--cfg loom` the same names are the
/// loom types, so a loom model runs the real code.
///
/// `Arc` and `Weak` are not here: callers keep `std::sync::Arc` and `Weak`.
/// loom 0.7.3 has no `Weak`, and the flusher and the commit timer use it. A
/// std `Arc` around loom locks is sound for the models: they check the lock
/// and condvar protocol, not the reference count.
pub mod raw {
    #[cfg(not(loom))]
    pub use std::sync::{
        atomic, Condvar, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
    };
    #[cfg(not(loom))]
    pub use std::thread;

    #[cfg(loom)]
    pub use loom::sync::{
        atomic, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
    };
    #[cfg(loom)]
    pub use loom::thread;
    /// loom's locks return the std `LockResult`.
    #[cfg(loom)]
    pub use std::sync::PoisonError;
}

/// Where a lock sits in the one global lock order. A thread takes locks in
/// ascending rank. Rules, checked in debug builds:
///
/// - A lower rank than the lock last taken panics.
/// - The same rank panics for `Gate`, `Rename` and `Cell` (one at a time).
/// - The same rank for `Stripe` needs a strictly greater key (stripes in
///   ascending order).
/// - The same rank may nest for `Short` and `Store`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rank {
    /// The commit gate (std, modelled through [`rank::enter`]).
    Gate = 1,
    /// The cross-directory rename mutex, and the commit lock of the metadata
    /// store (held from the commit cut to the root; the cut takes cell and map
    /// locks with it held). One thread never holds both: the commit lock is
    /// taken by the commit and GC threads, the rename mutex by requests, and
    /// the checker refuses two locks of this rank at once.
    Rename = 2,
    /// A namespace stripe.
    Stripe = 3,
    /// The state of one open file.
    Cell = 4,
    /// A short map lock (handles, orphans, cell map shards, readers map).
    Short = 5,
    /// Store internals (metadata store, WAL, chunk store).
    Store = 6,
}

/// The debug-build stack of locks held by this thread.
#[cfg(debug_assertions)]
mod track {
    use super::Rank;
    use std::cell::{Cell, RefCell};

    thread_local! {
        static HELD: RefCell<Vec<(Rank, u64)>> = const { RefCell::new(Vec::new()) };
        static NEED_GATE: Cell<bool> = const { Cell::new(false) };
    }

    /// Panic if taking `rank`/`key` now would break the rank rules.
    pub fn check(rank: Rank, key: u64) {
        HELD.with(|h| {
            let h = h.borrow();
            if matches!(rank, Rank::Stripe | Rank::Cell)
                && NEED_GATE.with(Cell::get)
                && !h.iter().any(|e| e.0 == Rank::Gate)
            {
                panic!("lock rank violation: {rank:?}/{key} taken without the commit gate");
            }
            if let Some(&(top, top_key)) = h.last() {
                let bad = if rank != top {
                    rank < top
                } else {
                    match rank {
                        Rank::Gate | Rank::Rename | Rank::Cell => true,
                        Rank::Stripe => key <= top_key,
                        Rank::Short | Rank::Store => false,
                    }
                };
                if bad {
                    panic!(
                        "lock rank violation: {rank:?}/{key} taken while holding {top:?}/{top_key}"
                    );
                }
            }
        });
    }

    pub fn push(rank: Rank, key: u64) {
        HELD.with(|h| h.borrow_mut().push((rank, key)));
    }

    /// Remove the newest entry equal to `(rank, key)`; return its position.
    pub fn remove(rank: Rank, key: u64) -> Option<usize> {
        // A thread that is ending may have dropped its stack already.
        HELD.try_with(|h| {
            let mut h = h.borrow_mut();
            let at = h.iter().rposition(|e| *e == (rank, key))?;
            h.remove(at);
            Some(at)
        })
        .ok()
        .flatten()
    }

    pub fn insert(at: usize, rank: Rank, key: u64) {
        HELD.with(|h| {
            let mut h = h.borrow_mut();
            let at = at.min(h.len());
            h.insert(at, (rank, key));
        });
    }

    pub fn held() -> Vec<(Rank, u64)> {
        HELD.with(|h| h.borrow().clone())
    }

    pub fn set_need_gate(on: bool) {
        NEED_GATE.with(|c| c.set(on));
    }
}

/// One entry of the rank stack, removed when dropped. Empty in a release build.
struct Entry {
    #[cfg(debug_assertions)]
    rank: Rank,
    #[cfg(debug_assertions)]
    key: u64,
}

impl Entry {
    /// Check the rules, before a lock is taken.
    #[inline(always)]
    fn check(_rank: Rank, _key: u64) {
        #[cfg(debug_assertions)]
        track::check(_rank, _key);
    }

    /// Record a lock that is now held.
    #[inline(always)]
    fn push(_rank: Rank, _key: u64) -> Entry {
        #[cfg(debug_assertions)]
        track::push(_rank, _key);
        Entry {
            #[cfg(debug_assertions)]
            rank: _rank,
            #[cfg(debug_assertions)]
            key: _key,
        }
    }

    /// Take the entry off the stack while the thread waits on a condvar.
    #[inline(always)]
    fn suspend(&self) -> Option<usize> {
        #[cfg(debug_assertions)]
        {
            track::remove(self.rank, self.key)
        }
        #[cfg(not(debug_assertions))]
        {
            None
        }
    }

    /// Put the entry back where it was.
    #[inline(always)]
    fn resume(&self, _at: Option<usize>) {
        #[cfg(debug_assertions)]
        if let Some(at) = _at {
            track::insert(at, self.rank, self.key);
        }
    }
}

impl Drop for Entry {
    #[inline(always)]
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        track::remove(self.rank, self.key);
    }
}

/// The rank stack of this thread, for the commit gate (a std lock) and tests.
pub mod rank {
    use super::{Entry, Rank};

    /// A rank entry for a lock this shim does not own. Dropping it ends the
    /// entry.
    #[must_use = "the entry ends when the marker is dropped"]
    pub struct Marker {
        _entry: Entry,
    }

    /// Record that this thread now holds a lock of `rank` that is not a shim
    /// lock (the std commit gate is `Rank::Gate`). The same order rules apply.
    pub fn enter(rank: Rank) -> Marker {
        Entry::check(rank, 0);
        Marker { _entry: Entry::push(rank, 0) }
    }

    /// Turn the strict-gate rule on or off for this thread. While on, taking a
    /// Stripe or Cell lock with no `Gate` entry held panics with `taken without
    /// the commit gate`. A no-op in a release build.
    pub fn require_gate(_on: bool) {
        #[cfg(debug_assertions)]
        super::track::set_need_gate(_on);
    }

    /// The ranks and keys this thread holds, oldest first. Empty in a release
    /// build.
    pub fn held() -> Vec<(Rank, u64)> {
        #[cfg(debug_assertions)]
        {
            super::track::held()
        }
        #[cfg(not(debug_assertions))]
        {
            Vec::new()
        }
    }
}

// ── Timing ───────────────────────────────────────────────────────────────────

/// Wait and hold of one acquisition; records into `lock_stats` when dropped.
#[cfg(feature = "lock-stats")]
struct Timing {
    site: Site,
    wait_ns: u64,
    acquired: std::time::Instant,
}

#[cfg(feature = "lock-stats")]
impl Drop for Timing {
    fn drop(&mut self) {
        let hold_ns = self.acquired.elapsed().as_nanos() as u64;
        crate::lock_stats::record(self.site, self.wait_ns, hold_ns);
    }
}

/// Take a lock: `fast` is the try, `slow` the blocking call. The wait is 0
/// when the lock was free.
#[cfg(feature = "lock-stats")]
fn acquire<G>(
    site: Option<Site>,
    fast: impl FnOnce() -> Option<G>,
    slow: impl FnOnce() -> G,
) -> (G, Option<Timing>) {
    let Some(site) = site else { return (slow(), None) };
    let (g, wait_ns) = match fast() {
        Some(g) => (g, 0),
        None => {
            let t = std::time::Instant::now();
            let g = slow();
            (g, t.elapsed().as_nanos() as u64)
        }
    };
    (g, Some(Timing { site, wait_ns, acquired: std::time::Instant::now() }))
}

/// Rank, key and timing site of one lock.
#[derive(Clone, Copy)]
struct Meta {
    rank: Rank,
    key: u64,
    #[cfg_attr(not(feature = "lock-stats"), allow(dead_code))]
    site: Option<Site>,
}

impl Meta {
    fn new(rank: Rank, key: u64) -> Self {
        Meta { rank, key, site: None }
    }
}

// ── Mutex ────────────────────────────────────────────────────────────────────

/// A `parking_lot` mutex with a rank. No poison.
pub struct Mutex<T> {
    m: parking_lot::Mutex<T>,
    meta: Meta,
}

/// The guard of a [`Mutex`]. Derefs to the value.
pub struct MutexGuard<'a, T> {
    // Field order is drop order: unlock, then leave the rank stack, then record.
    g: parking_lot::MutexGuard<'a, T>,
    rank: Entry,
    #[cfg(feature = "lock-stats")]
    _timing: Option<Timing>,
}

impl<T> Mutex<T> {
    pub fn new(rank: Rank, value: T) -> Self {
        Mutex { m: parking_lot::Mutex::new(value), meta: Meta::new(rank, 0) }
    }

    /// `key` orders locks of the same rank: the stripe index, or the inode
    /// number.
    pub fn new_keyed(rank: Rank, key: u64, value: T) -> Self {
        Mutex { m: parking_lot::Mutex::new(value), meta: Meta::new(rank, key) }
    }

    /// Record the wait and hold of this lock as `site` (with `lock-stats`).
    pub fn timed(mut self, site: Site) -> Self {
        self.meta.site = Some(site);
        self
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        Entry::check(self.meta.rank, self.meta.key);
        #[cfg(feature = "lock-stats")]
        let (g, timing) = acquire(self.meta.site, || self.m.try_lock(), || self.m.lock());
        #[cfg(not(feature = "lock-stats"))]
        let g = self.m.lock();
        MutexGuard {
            g,
            rank: Entry::push(self.meta.rank, self.meta.key),
            #[cfg(feature = "lock-stats")]
            _timing: timing,
        }
    }

    /// Lock if free. The rank rules apply as for `lock`.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        Entry::check(self.meta.rank, self.meta.key);
        let g = self.m.try_lock()?;
        Some(MutexGuard {
            g,
            rank: Entry::push(self.meta.rank, self.meta.key),
            #[cfg(feature = "lock-stats")]
            _timing: self.meta.site.map(|site| Timing {
                site,
                wait_ns: 0,
                acquired: std::time::Instant::now(),
            }),
        })
    }

    pub fn into_inner(self) -> T {
        self.m.into_inner()
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.m.get_mut()
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.g
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.g
    }
}

// ── RwLock ───────────────────────────────────────────────────────────────────

/// A `parking_lot` read-write lock with a rank. No poison.
pub struct RwLock<T> {
    l: parking_lot::RwLock<T>,
    meta: Meta,
}

/// The read guard of an [`RwLock`].
pub struct RwLockReadGuard<'a, T> {
    g: parking_lot::RwLockReadGuard<'a, T>,
    _rank: Entry,
    #[cfg(feature = "lock-stats")]
    _timing: Option<Timing>,
}

/// The write guard of an [`RwLock`].
pub struct RwLockWriteGuard<'a, T> {
    g: parking_lot::RwLockWriteGuard<'a, T>,
    _rank: Entry,
    #[cfg(feature = "lock-stats")]
    _timing: Option<Timing>,
}

impl<T> RwLock<T> {
    pub fn new(rank: Rank, value: T) -> Self {
        RwLock { l: parking_lot::RwLock::new(value), meta: Meta::new(rank, 0) }
    }

    pub fn new_keyed(rank: Rank, key: u64, value: T) -> Self {
        RwLock { l: parking_lot::RwLock::new(value), meta: Meta::new(rank, key) }
    }

    /// Record the wait and hold of this lock as `site` (with `lock-stats`).
    pub fn timed(mut self, site: Site) -> Self {
        self.meta.site = Some(site);
        self
    }

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        Entry::check(self.meta.rank, self.meta.key);
        #[cfg(feature = "lock-stats")]
        let (g, timing) = acquire(self.meta.site, || self.l.try_read(), || self.l.read());
        #[cfg(not(feature = "lock-stats"))]
        let g = self.l.read();
        RwLockReadGuard {
            g,
            _rank: Entry::push(self.meta.rank, self.meta.key),
            #[cfg(feature = "lock-stats")]
            _timing: timing,
        }
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        Entry::check(self.meta.rank, self.meta.key);
        #[cfg(feature = "lock-stats")]
        let (g, timing) = acquire(self.meta.site, || self.l.try_write(), || self.l.write());
        #[cfg(not(feature = "lock-stats"))]
        let g = self.l.write();
        RwLockWriteGuard {
            g,
            _rank: Entry::push(self.meta.rank, self.meta.key),
            #[cfg(feature = "lock-stats")]
            _timing: timing,
        }
    }
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.g
    }
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.g
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.g
    }
}

// ── Condvar ──────────────────────────────────────────────────────────────────

/// A `parking_lot` condition variable for a shim [`Mutex`]. The wait takes the
/// guard by `&mut`; the rank entry is off the stack while the thread waits.
#[derive(Default)]
pub struct Condvar(parking_lot::Condvar);

impl Condvar {
    pub fn new() -> Self {
        Condvar(parking_lot::Condvar::new())
    }

    pub fn wait<T>(&self, guard: &mut MutexGuard<'_, T>) {
        let at = guard.rank.suspend();
        self.0.wait(&mut guard.g);
        guard.rank.resume(at);
    }

    pub fn wait_for<T>(&self, guard: &mut MutexGuard<'_, T>, timeout: Duration) -> WaitTimeoutResult {
        let at = guard.rank.suspend();
        let r = self.0.wait_for(&mut guard.g, timeout);
        guard.rank.resume(at);
        r
    }

    pub fn notify_one(&self) {
        self.0.notify_one();
    }

    pub fn notify_all(&self) {
        self.0.notify_all();
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::sync::Arc;

    const BAD: &str = "lock rank violation";

    #[test]
    fn short_then_store_passes() {
        let a = Mutex::new(Rank::Short, 1);
        let b = Mutex::new(Rank::Store, 2);
        let _a = a.lock();
        let _b = b.lock();
        assert_eq!(rank::held(), [(Rank::Short, 0), (Rank::Store, 0)]);
    }

    #[test]
    #[should_panic(expected = "lock rank violation: Short/0 taken while holding Store/0")]
    fn store_then_short_panics() {
        let a = Mutex::new(Rank::Short, ());
        let b = Mutex::new(Rank::Store, ());
        let _b = b.lock();
        let _a = a.lock();
    }

    #[test]
    #[should_panic(expected = "lock rank violation")]
    fn cell_then_stripe_panics() {
        let s = Mutex::new_keyed(Rank::Stripe, 1, ());
        let c = Mutex::new_keyed(Rank::Cell, 7, ());
        let _c = c.lock();
        let _s = s.lock();
    }

    #[test]
    #[should_panic(expected = "lock rank violation: Stripe/1 taken while holding Stripe/3")]
    fn stripe_descending_panics() {
        let s1 = Mutex::new_keyed(Rank::Stripe, 1, ());
        let s3 = RwLock::new_keyed(Rank::Stripe, 3, ());
        let _s3 = s3.write();
        let _s1 = s1.lock();
    }

    #[test]
    fn stripe_ascending_passes() {
        let s1 = Mutex::new_keyed(Rank::Stripe, 1, ());
        let s3 = Mutex::new_keyed(Rank::Stripe, 3, ());
        let _a = s1.lock();
        let _b = s3.lock();
    }

    #[test]
    #[should_panic(expected = "lock rank violation")]
    fn same_stripe_twice_panics() {
        let s = Mutex::new_keyed(Rank::Stripe, 2, ());
        let _a = s.lock();
        let _b = s.lock();
    }

    #[test]
    #[should_panic(expected = "lock rank violation")]
    fn two_cells_panic_even_in_key_order() {
        let a = Mutex::new_keyed(Rank::Cell, 1, ());
        let b = Mutex::new_keyed(Rank::Cell, 2, ());
        let _a = a.lock();
        let _b = b.lock();
    }

    #[test]
    #[should_panic(expected = "lock rank violation")]
    fn rename_then_rename_panics() {
        let a = Mutex::new(Rank::Rename, ());
        let b = Mutex::new(Rank::Rename, ());
        let _a = a.lock();
        let _b = b.lock();
    }

    #[test]
    fn same_rank_short_and_store_nest() {
        let (a, b) = (Mutex::new(Rank::Short, ()), RwLock::new(Rank::Short, ()));
        let _a = a.lock();
        let _b = b.read();
        drop((_a, _b));
        let (c, d) = (Mutex::new(Rank::Store, ()), Mutex::new(Rank::Store, ()));
        let _c = c.lock();
        let _d = d.lock();
    }

    #[test]
    fn guards_drop_out_of_order() {
        let a = Mutex::new(Rank::Short, ());
        let b = Mutex::new(Rank::Store, ());
        let ga = a.lock();
        let gb = b.lock();
        drop(ga);
        assert_eq!(rank::held(), [(Rank::Store, 0)]);
        drop(gb);
        assert!(rank::held().is_empty());
    }

    #[test]
    fn try_lock_busy_leaves_no_entry() {
        let m = Arc::new(Mutex::new(Rank::Short, ()));
        let g = m.lock();
        let m2 = m.clone();
        // Another thread sees the lock busy.
        assert!(std::thread::spawn(move || m2.try_lock().is_none()).join().unwrap());
        drop(g);
        assert!(rank::held().is_empty());
        assert!(m.try_lock().is_some());
    }

    #[test]
    fn condvar_wait_takes_entry_off_the_stack() {
        let a = Mutex::new(Rank::Short, ());
        let b = Mutex::new(Rank::Store, ());
        let cv = Condvar::new();
        let mut ga = a.lock();
        let _gb = b.lock();
        // The suspend and resume that `wait` does, seen from the test.
        let at = ga.rank.suspend();
        assert_eq!(rank::held(), [(Rank::Store, 0)]);
        ga.rank.resume(at);
        assert_eq!(rank::held(), [(Rank::Short, 0), (Rank::Store, 0)]);
        // A real timed wait puts the entry back at the same place.
        let r = cv.wait_for(&mut ga, Duration::from_millis(5));
        assert!(r.timed_out());
        assert_eq!(rank::held(), [(Rank::Short, 0), (Rank::Store, 0)]);
    }

    #[test]
    fn condvar_notify_wakes_waiter() {
        let pair = Arc::new((Mutex::new(Rank::Short, false), Condvar::new()));
        let p2 = pair.clone();
        let t = std::thread::spawn(move || {
            let mut g = p2.0.lock();
            while !*g {
                p2.1.wait(&mut g);
            }
            rank::held()
        });
        std::thread::sleep(Duration::from_millis(10));
        *pair.0.lock() = true;
        pair.1.notify_all();
        assert_eq!(t.join().unwrap(), [(Rank::Short, 0)]);
    }

    #[test]
    fn gate_marker_then_cell_passes() {
        let c = Mutex::new_keyed(Rank::Cell, 1, ());
        let _gate = rank::enter(Rank::Gate);
        let _c = c.lock();
        assert_eq!(rank::held(), [(Rank::Gate, 0), (Rank::Cell, 1)]);
    }

    #[test]
    #[should_panic(expected = "taken without the commit gate")]
    fn strict_gate_cell_without_gate_panics() {
        rank::require_gate(true);
        let c = Mutex::new_keyed(Rank::Cell, 1, ());
        let _c = c.lock();
    }

    #[test]
    #[should_panic(expected = "taken without the commit gate")]
    fn strict_gate_stripe_without_gate_panics() {
        rank::require_gate(true);
        let s = Mutex::new_keyed(Rank::Stripe, 1, ());
        let _s = s.lock();
    }

    #[test]
    fn strict_gate_with_gate_passes_and_short_needs_none() {
        rank::require_gate(true);
        let s = Mutex::new_keyed(Rank::Stripe, 1, ());
        let c = Mutex::new_keyed(Rank::Cell, 1, ());
        let m = Mutex::new(Rank::Short, ());
        drop(m.lock());
        {
            let _gate = rank::enter(Rank::Gate);
            let _s = s.lock();
            let _c = c.lock();
        }
        rank::require_gate(false);
        drop(c.lock());
    }

    #[test]
    #[should_panic(expected = "lock rank violation")]
    fn gate_after_a_lock_panics() {
        let m = Mutex::new(Rank::Short, ());
        let _m = m.lock();
        let _gate = rank::enter(Rank::Gate);
    }

    #[test]
    fn exclusive_gate_holder_locks_cells_one_at_a_time() {
        rank::require_gate(true);
        let cells: Vec<_> = (1..=3).map(|i| Mutex::new_keyed(Rank::Cell, i, i)).collect();
        let _gate = rank::enter(Rank::Gate);
        for c in &cells {
            let g = c.lock();
            assert!(*g > 0);
        }
        rank::require_gate(false);
    }

    #[test]
    fn mutex_counts_across_threads() {
        let m = Arc::new(Mutex::new(Rank::Short, 0u32));
        let ts: Vec<_> = (0..4)
            .map(|_| {
                let m = m.clone();
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        *m.lock() += 1;
                    }
                })
            })
            .collect();
        for t in ts {
            t.join().unwrap();
        }
        assert_eq!(*m.lock(), 800);
    }

    #[test]
    fn rwlock_read_write() {
        let l = RwLock::new(Rank::Short, 1);
        assert_eq!(*l.read(), 1);
        *l.write() = 2;
        assert_eq!(*l.read(), 2);
        assert!(rank::held().is_empty());
    }

    #[cfg(feature = "lock-stats")]
    #[test]
    fn timed_lock_records_its_site() {
        // `Readers` is not used by chunk-store itself.
        let count = |label: &str| {
            crate::lock_stats::snapshot().into_iter().find(|s| s.label == label).unwrap().count
        };
        let before = count("readers");
        let m = Mutex::new(Rank::Short, ()).timed(Site::Readers);
        drop(m.lock());
        let l = RwLock::new(Rank::Short, ()).timed(Site::Readers);
        drop(l.read());
        drop(l.write());
        assert!(count("readers") >= before + 3);
    }

    #[test]
    fn rank_names_in_message_are_checked() {
        // The message names both locks.
        let r = std::panic::catch_unwind(|| {
            let a = Mutex::new_keyed(Rank::Cell, 9, ());
            let b = Mutex::new_keyed(Rank::Stripe, 4, ());
            let _a = a.lock();
            let _b = b.lock();
        });
        let msg = *r.unwrap_err().downcast::<String>().unwrap();
        assert!(msg.contains(BAD) && msg.contains("Stripe/4") && msg.contains("Cell/9"), "{msg}");
    }
}
