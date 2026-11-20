//! Pacing of the online GC: the GC gives way to writes, depending on the
//! free space of the file system under the store.
//!
//! - More than [`PLENTY_FREE`] free: [`Mode::Yield`]. At each safe point,
//!   while writes came in the last [`WRITE_QUIET`], the GC sleeps
//!   [`YIELD_SLEEP`] and looks again.
//! - Between [`LOW_FREE`] and [`PLENTY_FREE`]: [`Mode::Throttle`]. Once in
//!   each [`THROTTLE_PERIOD`] of GC work, a short sleep that grows with the
//!   write rate of that period (at most [`THROTTLE_MAX_SLEEP`]). Never a full
//!   stop.
//! - Less than [`LOW_FREE`] free: [`Mode::FullSpeed`]. No waits, and the GC
//!   thread starts a GC at once when one is due by changes (see
//!   `online_gc`), without the interval or the duty gap.
//!
//! The waits of one phase (mark or sweep) add up to at most its cap
//! ([`MARK_YIELD_CAP`], [`SWEEP_YIELD_CAP`]); after that the phase runs at
//! full speed. So a GC on a store with endless writes still finishes. A wait
//! also ends when the mount shuts down.
//!
//! Why the mark continues after its cap (it does not abandon and retry):
//! the mark deletes nothing, and the objects put or read during it go to the
//! touched set, so a long mark is safe; a retry under endless writes could
//! never finish. The sweep cap exists because the exact touched set of the
//! sweep grows with the writes during it.
//!
//! The constants are first guesses, to be tuned (#121). Offline `slicefs gc`
//! does not pace (no writes).

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Above this free ratio: yield to writes.
pub const PLENTY_FREE: f64 = 0.20;
/// Below this free ratio: full speed, and start a GC at once.
pub const LOW_FREE: f64 = 0.05;
/// Writes within this time count as "writes now" (yield mode).
pub const WRITE_QUIET: Duration = Duration::from_millis(100);
/// One sleep of the yield mode.
pub const YIELD_SLEEP: Duration = Duration::from_millis(50);
/// The throttle mode sleeps at most once in this much GC time.
pub const THROTTLE_PERIOD: Duration = Duration::from_millis(100);
/// The longest throttle sleep (at or above [`THROTTLE_FULL_RATE`]).
pub const THROTTLE_MAX_SLEEP: Duration = Duration::from_millis(50);
/// The write rate (requests per second) that gets the longest throttle sleep.
pub const THROTTLE_FULL_RATE: f64 = 1000.0;
/// The most time the mark waits for writes in total.
pub const MARK_YIELD_CAP: Duration = Duration::from_secs(60);
/// The most time the sweep waits for writes in total.
pub const SWEEP_YIELD_CAP: Duration = Duration::from_secs(30);
/// The free ratio is read again after this time.
pub const FREE_SPACE_REFRESH: Duration = Duration::from_secs(1);
/// With low space, a GC starts at once only if the last GC freed at least
/// this many bytes; else the duty gap applies (at least
/// [`LOW_SPACE_MIN_GAP`]), so GCs that free nothing do not loop.
pub const LOW_SPACE_MIN_FREED: u64 = 1024 * 1024;
/// The shortest gap after a GC that freed little, with low space.
pub const LOW_SPACE_MIN_GAP: Duration = Duration::from_secs(10);
/// The first wait after a failed GC; it doubles with each failure in a row.
pub const FAILURE_BACKOFF_FIRST: Duration = Duration::from_secs(1);
/// The longest wait after failed GCs when the interval is 0 (only on
/// request; then it does not delay requests). Else the interval caps it.
pub const FAILURE_BACKOFF_MAX: Duration = Duration::from_secs(300);

/// How the GC treats writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Yield,
    Throttle,
    FullSpeed,
}

/// The mode for a free ratio (free / total bytes, 0.0 to 1.0).
pub fn mode_for(free: f64) -> Mode {
    if free > PLENTY_FREE {
        Mode::Yield
    } else if free >= LOW_FREE {
        Mode::Throttle
    } else {
        Mode::FullSpeed
    }
}

/// The free ratio (available / total blocks) of the file system that holds
/// `path`. On an error, or a file system that reports no blocks (some do on
/// macOS), 1.0: "plenty", so a bad reading never makes the GC run more.
pub fn free_ratio(path: &Path) -> f64 {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return 1.0 };
    let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `sv` a valid out buffer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut sv) } != 0 || sv.f_blocks == 0 {
        return 1.0;
    }
    (sv.f_bavail as f64 / sv.f_blocks as f64).clamp(0.0, 1.0)
}

/// A reader of the free ratio (injected in tests).
pub type FreeRatio = Arc<dyn Fn() -> f64 + Send + Sync>;

/// The free ratio of the file system under `store`.
pub fn store_free_ratio(store: &Path) -> FreeRatio {
    let store = store.to_path_buf();
    Arc::new(move || free_ratio(&store))
}

/// Time for the pacer (a fake one in tests).
pub trait Clock {
    /// Time since some fixed start.
    fn now(&self) -> Duration;
    fn sleep(&self, d: Duration);
}

/// The real clock.
pub struct RealClock(Instant);

impl RealClock {
    pub fn new() -> Self {
        RealClock(Instant::now())
    }
}

impl Default for RealClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for RealClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d)
    }
}

struct State {
    /// The write counter at the last look.
    count: u64,
    /// When the write counter last changed (as seen by the pacer).
    changed: Option<Duration>,
    /// Total time waited so far.
    waited: Duration,
    free: f64,
    free_at: Option<Duration>,
    /// Start of the current throttle period, and the counter then.
    period: (Duration, u64),
}

/// The pacer of one GC phase (see the module docs).
pub struct WritePacer<'a> {
    writes: Arc<AtomicU64>,
    free: FreeRatio,
    clock: Box<dyn Clock + 'a>,
    cap: Duration,
    stop: &'a AtomicBool,
    state: Mutex<State>,
}

impl<'a> WritePacer<'a> {
    /// A pacer that watches the write counter `writes`, reads the free ratio
    /// with `free`, waits at most `cap` in total, and stops waiting when
    /// `stop` is set.
    pub fn new(
        writes: Arc<AtomicU64>,
        free: FreeRatio,
        clock: Box<dyn Clock + 'a>,
        cap: Duration,
        stop: &'a AtomicBool,
    ) -> Self {
        let count = writes.load(Ordering::Relaxed);
        let now = clock.now();
        let state = State { count, changed: None, waited: Duration::ZERO, free: 1.0, free_at: None, period: (now, count) };
        WritePacer { writes, free, clock, cap, stop, state: Mutex::new(state) }
    }

    /// The total time this pacer waited.
    pub fn waited(&self) -> Duration {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).waited
    }

    fn mode(&self, s: &mut State, now: Duration) -> Mode {
        if s.free_at.is_none_or(|t| now.saturating_sub(t) >= FREE_SPACE_REFRESH) {
            s.free = (self.free)();
            s.free_at = Some(now);
        }
        mode_for(s.free)
    }

    fn done(&self, s: &State) -> bool {
        s.waited >= self.cap || self.stop.load(Ordering::Relaxed)
    }

    fn sleep(&self, s: &mut State, d: Duration) {
        let d = d.min(self.cap.saturating_sub(s.waited));
        if !d.is_zero() {
            self.clock.sleep(d);
            s.waited += d;
        }
    }

    fn yield_to_writes(&self, s: &mut State) {
        while !self.done(s) {
            let now = self.clock.now();
            // The space can run low during a wait: then stop waiting.
            if self.mode(s, now) != Mode::Yield {
                return;
            }
            let c = self.writes.load(Ordering::Relaxed);
            if c != s.count {
                s.count = c;
                s.changed = Some(now);
            }
            match s.changed {
                Some(t) if now.saturating_sub(t) < WRITE_QUIET => self.sleep(s, YIELD_SLEEP),
                _ => return,
            }
        }
    }

    fn throttle(&self, s: &mut State, now: Duration) {
        let (start, c0) = s.period;
        let work = now.saturating_sub(start);
        if work < THROTTLE_PERIOD {
            return;
        }
        let c = self.writes.load(Ordering::Relaxed);
        let rate = c.saturating_sub(c0) as f64 / work.as_secs_f64();
        let share = (rate / THROTTLE_FULL_RATE).min(1.0);
        if !self.done(s) {
            self.sleep(s, Duration::from_nanos((THROTTLE_MAX_SLEEP.as_nanos() as f64 * share) as u64));
        }
        // The next period counts GC work only, not this sleep.
        s.period = (self.clock.now(), c);
        s.count = c;
    }
}

impl chunk_store::GcPacer for WritePacer<'_> {
    fn at_safe_point(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = self.clock.now();
        match self.mode(&mut s, now) {
            Mode::FullSpeed => {}
            Mode::Yield => self.yield_to_writes(&mut s),
            Mode::Throttle => self.throttle(&mut s, now),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chunk_store::GcPacer;
    use std::cell::Cell;

    #[test]
    fn the_mode_follows_the_free_ratio() {
        assert_eq!(mode_for(1.0), Mode::Yield);
        assert_eq!(mode_for(0.21), Mode::Yield);
        assert_eq!(mode_for(0.20), Mode::Throttle);
        assert_eq!(mode_for(0.10), Mode::Throttle);
        assert_eq!(mode_for(0.05), Mode::Throttle);
        assert_eq!(mode_for(0.049), Mode::FullSpeed);
        assert_eq!(mode_for(0.0), Mode::FullSpeed);
    }

    #[test]
    fn the_free_ratio_of_a_real_path_is_a_ratio_and_an_error_is_plenty() {
        let r = free_ratio(Path::new("/"));
        assert!((0.0..=1.0).contains(&r), "{r}");
        assert_eq!(free_ratio(Path::new("/no/such/path/for/slicefs")), 1.0);
    }

    /// A fake clock: a sleep moves the time, and while `writes_left` is not
    /// zero each sleep also adds a write.
    struct Fake<'a> {
        now: Cell<Duration>,
        writes: Arc<AtomicU64>,
        writes_left: &'a Cell<u64>,
    }

    impl Clock for Fake<'_> {
        fn now(&self) -> Duration {
            self.now.get()
        }
        fn sleep(&self, d: Duration) {
            self.now.set(self.now.get() + d);
            if self.writes_left.get() > 0 {
                self.writes_left.set(self.writes_left.get() - 1);
                self.writes.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn pacer<'a>(free: f64, writes: &Arc<AtomicU64>, left: &'a Cell<u64>, cap: Duration, stop: &'a AtomicBool) -> WritePacer<'a> {
        let clock = Fake { now: Cell::new(Duration::ZERO), writes: Arc::clone(writes), writes_left: left };
        WritePacer::new(Arc::clone(writes), Arc::new(move || free), Box::new(clock), cap, stop)
    }

    #[test]
    fn with_plenty_of_space_it_waits_while_writes_come_and_stops_at_the_cap() {
        let stop = AtomicBool::new(false);
        let writes = Arc::new(AtomicU64::new(0));
        let left = Cell::new(u64::MAX); // endless writes
        let cap = Duration::from_secs(2);
        let p = pacer(0.9, &writes, &left, cap, &stop);
        // No write since the pacer started: no wait.
        p.at_safe_point();
        assert_eq!(p.waited(), Duration::ZERO);
        writes.fetch_add(1, Ordering::Relaxed);
        p.at_safe_point();
        assert_eq!(p.waited(), cap, "waits while writes come, up to the cap");
        p.at_safe_point();
        assert_eq!(p.waited(), cap, "no more waits after the cap");
    }

    #[test]
    fn with_plenty_of_space_it_waits_until_the_writes_stop() {
        let stop = AtomicBool::new(false);
        let writes = Arc::new(AtomicU64::new(0));
        let left = Cell::new(5); // writes during the first 5 sleeps
        let p = pacer(0.9, &writes, &left, Duration::from_secs(30), &stop);
        writes.fetch_add(1, Ordering::Relaxed);
        p.at_safe_point();
        // 5 sleeps with writes, then quiet for WRITE_QUIET (2 more sleeps).
        assert_eq!(p.waited(), YIELD_SLEEP * 7);
        p.at_safe_point();
        assert_eq!(p.waited(), YIELD_SLEEP * 7, "no writes: no wait");
    }

    #[test]
    fn a_stop_ends_the_wait() {
        let stop = AtomicBool::new(true);
        let writes = Arc::new(AtomicU64::new(0));
        let left = Cell::new(u64::MAX);
        let p = pacer(0.9, &writes, &left, Duration::from_secs(30), &stop);
        writes.fetch_add(1, Ordering::Relaxed);
        p.at_safe_point();
        assert_eq!(p.waited(), Duration::ZERO);
    }

    #[test]
    fn with_low_space_it_never_waits() {
        let stop = AtomicBool::new(false);
        let writes = Arc::new(AtomicU64::new(0));
        let left = Cell::new(u64::MAX);
        let p = pacer(0.01, &writes, &left, Duration::from_secs(30), &stop);
        for _ in 0..100 {
            writes.fetch_add(1000, Ordering::Relaxed);
            p.at_safe_point();
        }
        assert_eq!(p.waited(), Duration::ZERO);
    }

    #[test]
    fn with_medium_space_it_sleeps_briefly_by_the_write_rate() {
        let stop = AtomicBool::new(false);
        let writes = Arc::new(AtomicU64::new(0));
        let clock_now = Cell::new(Duration::ZERO);
        // A clock the test moves (GC work) by hand.
        struct Hand<'a>(&'a Cell<Duration>);
        impl Clock for Hand<'_> {
            fn now(&self) -> Duration {
                self.0.get()
            }
            fn sleep(&self, d: Duration) {
                self.0.set(self.0.get() + d);
            }
        }
        let p = WritePacer::new(Arc::clone(&writes), Arc::new(|| 0.10), Box::new(Hand(&clock_now)), Duration::from_secs(30), &stop);
        // No writes: no sleep.
        clock_now.set(clock_now.get() + THROTTLE_PERIOD);
        p.at_safe_point();
        assert_eq!(p.waited(), Duration::ZERO);
        // Half the full rate over one period: half the longest sleep.
        writes.fetch_add((THROTTLE_FULL_RATE * THROTTLE_PERIOD.as_secs_f64() / 2.0) as u64, Ordering::Relaxed);
        clock_now.set(clock_now.get() + THROTTLE_PERIOD);
        p.at_safe_point();
        assert_eq!(p.waited(), THROTTLE_MAX_SLEEP / 2);
        // Safe points within a period do not sleep again.
        writes.fetch_add(1000, Ordering::Relaxed);
        p.at_safe_point();
        assert_eq!(p.waited(), THROTTLE_MAX_SLEEP / 2);
        // A high rate: the longest sleep, never more.
        clock_now.set(clock_now.get() + THROTTLE_PERIOD);
        p.at_safe_point();
        assert_eq!(p.waited(), THROTTLE_MAX_SLEEP / 2 + THROTTLE_MAX_SLEEP);
    }
}
