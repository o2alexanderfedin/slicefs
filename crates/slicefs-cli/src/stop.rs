//! The stop signal of a mount thread that waits between its passes (the
//! online GC, the commit timer, the FUSE-T unmount watcher): a flag and a
//! condvar, so a stop wakes the thread at once and does not wait for the
//! end of its tick. It uses std locks: it is in no loom model.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A stop flag with a wake. [`StopSignal::stop`] sets the flag and wakes
/// every [`StopSignal::wait`].
#[derive(Debug)]
pub struct StopSignal {
    flag: AtomicBool,
    lock: Mutex<()>,
    wake: Condvar,
}

impl StopSignal {
    /// A new signal, not stopped.
    pub fn new() -> Arc<Self> {
        Arc::new(Self { flag: AtomicBool::new(false), lock: Mutex::new(()), wake: Condvar::new() })
    }

    /// Set the flag and wake the waiters.
    pub fn stop(&self) {
        // Under the lock, so a waiter between its flag check and its wait
        // cannot miss the wake.
        let _g = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.flag.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }

    /// True after [`StopSignal::stop`].
    pub fn is_stopped(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// The flag, for code that checks a plain `AtomicBool` (the GC passes).
    pub fn flag(&self) -> &AtomicBool {
        &self.flag
    }

    /// Wait for `d` or until the stop, whichever comes first. True when
    /// stopped. A `d` too large for the clock waits for the stop only.
    pub fn wait(&self, d: Duration) -> bool {
        let deadline = Instant::now().checked_add(d);
        let mut g = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if self.is_stopped() {
                return true;
            }
            g = match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return false;
                    }
                    self.wake.wait_timeout(g, deadline - now).unwrap_or_else(PoisonError::into_inner).0
                }
                None => self.wake.wait(g).unwrap_or_else(PoisonError::into_inner),
            };
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn a_wait_ends_at_the_stop_or_at_its_time() {
        let s = StopSignal::new();
        let t = Instant::now();
        assert!(!s.wait(Duration::from_millis(20)), "no stop: the wait times out");
        assert!(t.elapsed() >= Duration::from_millis(20));
        let s2 = Arc::clone(&s);
        let h = std::thread::spawn(move || {
            let t = Instant::now();
            (s2.wait(Duration::from_secs(30)), t.elapsed())
        });
        std::thread::sleep(Duration::from_millis(20));
        s.stop();
        let (stopped, took) = h.join().unwrap();
        assert!(stopped && took < Duration::from_secs(5), "the stop woke the wait ({took:?})");
        assert!(s.wait(Duration::from_secs(30)), "a wait after the stop returns at once");
    }
}
