//! The request worker pool of a mount (Phase 21).
//!
//! The FUSE reader thread gives each request job to a pool of worker
//! threads (`slicefs-worker-<k>`). The queue is bounded in slots and in
//! payload bytes, so the memory that waits in the queue stays bounded: when
//! it is full, `submit` blocks the reader, and the kernel holds the next
//! requests. With one worker there is no pool: the request runs on the
//! reader thread.
//!
//! This module has no fuser type. The queue lock is a std mutex with no
//! rank: it is held only to move a job in or out, never while a job runs.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The most workers by default on Linux (owner answer, 2026-11-23).
/// CAP_MIB was measured with 4 writers, and each busy worker adds memory.
/// An explicit `--workers` value may be higher.
pub const DEFAULT_WORKERS_MAX: usize = 8;

/// The longest time that `submit` waits for space in the queue; then the
/// daemon aborts. A slot frees only when a job ends, and one request ends
/// within `STORE_BUSY_RETRY_MAX` 60 s (a write that meets a busy store tries
/// again, issue #170) + `STEP_WAIT_MAX` 120 s + `STORE_WAIT` 10 s + the
/// 200 ms throttle wait + the 200 ms step-slot wait (190.4 s), so a full
/// queue for this long means that the workers are stuck.
pub const SUBMIT_WAIT: Duration = Duration::from_secs(200);

/// The longest time that the drain at unmount waits for the workers; then
/// the daemon aborts. It covers the same sum as `SUBMIT_WAIT` (190.4 s).
pub const DRAIN_WAIT: Duration = Duration::from_secs(200);

/// The most payload bytes in the queue. One job that is larger is
/// admitted when the queue is empty.
pub const QUEUE_BYTES: usize = 8 << 20;

/// A waiter wakes at least this often.
const WAIT_SLICE: Duration = Duration::from_secs(1);

/// `submit` prints one warning when it waits this long.
const SUBMIT_WARN: Duration = Duration::from_secs(10);

/// One request job. It sends its own reply.
pub type Job = Box<dyn FnOnce() + Send + 'static>;

/// The name prefixes of request threads: the fuser reader (`fuser-<i>`,
/// which also runs the requests with one worker) and the pool workers.
const REQUEST_THREADS: [&str; 2] = ["fuser-", "slicefs-worker-"];

/// Install the process-wide panic hook that aborts on a request thread
/// (POOL-03). Call it once before the mount; a second call does nothing.
///
/// The hook first calls the hook that was set before (the message and the
/// backtrace go to stderr). Then, when the panic is on a request thread, it
/// prints one line and calls `std::process::abort()`. The hook runs at the
/// panic point, before the unwind: the locks of the half-done request stay
/// held (no other request can use the half-done state), no reply is sent,
/// and fuser cannot drop its file system holder, whose drop calls
/// `destroy()` and would commit the half-done state. The next mount
/// replays the write log, which has only the records of finished requests.
///
/// Why not `catch_unwind` around each job: a catch runs after the unwind.
/// The unwind releases the locks (they have no poison), so other workers
/// can serve requests on the half-done state, and the drop of the reply
/// sends EIO for the half-done request.
///
/// Other threads (the flusher, the GC, the commit timer, test threads) keep
/// the default behavior: their panic does not abort (Phase 20: a flusher
/// panic wakes the throttled writers). An abort on a flusher panic is a
/// deferred item.
pub fn install_abort_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            prev(info);
            let t = std::thread::current();
            let name = t.name().unwrap_or("");
            if REQUEST_THREADS.iter().any(|p| name.starts_with(p)) {
                eprintln!("request thread {name} panicked: abort; the write log replays at the next mount");
                std::process::abort();
            }
        }));
    });
}

/// The default worker count for `cpus` CPUs: half, at least 1 and at most
/// `DEFAULT_WORKERS_MAX`.
pub fn workers_for_cpus(cpus: usize) -> usize {
    (cpus / 2).clamp(1, DEFAULT_WORKERS_MAX)
}

/// The worker count when `--workers` is not given: 1 on macOS, else
/// `workers_for_cpus` of the available parallelism (1 if it is not known).
pub fn default_workers() -> usize {
    if cfg!(target_os = "macos") {
        1
    } else {
        std::thread::available_parallelism().map_or(1, |n| workers_for_cpus(n.get()))
    }
}

struct Queued {
    job: Job,
    weight: usize,
    submitted: Instant,
}

struct Queue {
    jobs: VecDeque<Queued>,
    bytes: usize,
    closed: bool,
    exited: usize,
}

struct Shared {
    q: Mutex<Queue>,
    not_empty: Condvar,
    not_full: Condvar,
    done: Condvar,
    slots: usize,
    max_bytes: usize,
    queue_now: AtomicUsize,
    queue_max: AtomicUsize,
    jobs: AtomicU64,
    jobs_per_worker: Vec<AtomicU64>,
    max_weight: AtomicUsize,
    max_queue_wait_ms: AtomicU64,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.q.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The counters of a pool. They are cheap to read at any time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolStats {
    /// The number of worker threads.
    pub workers: usize,
    /// The jobs that ended.
    pub jobs: u64,
    /// The jobs in the queue now (not started).
    pub queue_now: usize,
    /// The most jobs that were in the queue at one time.
    pub queue_max: usize,
    /// The jobs that each worker ran, in worker order.
    pub jobs_per_worker: Vec<u64>,
    /// The largest payload weight of one job, in bytes.
    pub max_weight: usize,
    /// The longest time from a submit to the start of its job, in ms.
    pub max_queue_wait_ms: u64,
}

impl fmt::Display for PoolStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let per: Vec<String> = self.jobs_per_worker.iter().map(u64::to_string).collect();
        write!(
            f,
            "pool: workers={} jobs={} max_weight={} max_queue_wait_ms={} queue_max={} jobs_per_worker={}",
            self.workers,
            self.jobs,
            self.max_weight,
            self.max_queue_wait_ms,
            self.queue_max,
            per.join(",")
        )
    }
}

/// A pool of request worker threads with a bounded queue.
pub struct Pool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    submit_wait: Duration,
}

impl Pool {
    /// A pool of `n` workers with max(2 x n, 4) slots, `QUEUE_BYTES` and
    /// `SUBMIT_WAIT`.
    pub fn new(n: usize) -> std::io::Result<Pool> {
        Pool::with_limits(n, (2 * n).max(4), QUEUE_BYTES, SUBMIT_WAIT)
    }

    /// A pool of `n` workers (at least 1) with the given queue limits.
    pub fn with_limits(n: usize, slots: usize, bytes: usize, submit_wait: Duration) -> std::io::Result<Pool> {
        assert!(n >= 1 && slots >= 1, "a pool needs at least one worker and one slot");
        let shared = Arc::new(Shared {
            q: Mutex::new(Queue { jobs: VecDeque::new(), bytes: 0, closed: false, exited: 0 }),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            done: Condvar::new(),
            slots,
            max_bytes: bytes,
            queue_now: AtomicUsize::new(0),
            queue_max: AtomicUsize::new(0),
            jobs: AtomicU64::new(0),
            jobs_per_worker: (0..n).map(|_| AtomicU64::new(0)).collect(),
            max_weight: AtomicUsize::new(0),
            max_queue_wait_ms: AtomicU64::new(0),
        });
        // A spawn error drops `pool`: its Drop stops the workers that started.
        let mut pool = Pool { shared, workers: Vec::with_capacity(n), submit_wait };
        for k in 0..n {
            let shared = Arc::clone(&pool.shared);
            let h = std::thread::Builder::new()
                .name(format!("slicefs-worker-{k}"))
                .spawn(move || worker_loop(&shared, k))?;
            pool.workers.push(h);
        }
        Ok(pool)
    }

    /// Put `job` in the queue. `weight` is the payload bytes that the job
    /// owns (0 for most requests). Block while the queue has no free slot
    /// or the bytes would go above the bound; an empty queue always admits
    /// the job. The caller must hold no ranked lock. After `submit_wait`
    /// with no space, the daemon aborts (the write log replays at the next
    /// mount); a valid request never gets an error from here.
    pub fn submit(&self, weight: usize, job: Job) {
        debug_assert!(
            chunk_store::sync::rank::held().is_empty(),
            "pool submit with a lock held: {:?}",
            chunk_store::sync::rank::held()
        );
        let s = &*self.shared;
        let start = Instant::now();
        let mut warned = false;
        let mut q = s.lock();
        while !(q.jobs.is_empty() || (q.jobs.len() < s.slots && q.bytes.saturating_add(weight) <= s.max_bytes)) {
            let waited = start.elapsed();
            if waited >= self.submit_wait {
                eprintln!(
                    "[pool] the request queue stayed full for {} s: abort; the write log replays at the next mount",
                    self.submit_wait.as_secs()
                );
                std::process::abort();
            }
            if !warned && waited >= SUBMIT_WARN {
                eprintln!("[pool] WARNING: the request queue is full for {} s", waited.as_secs());
                warned = true;
            }
            let left = (self.submit_wait - waited).min(WAIT_SLICE);
            q = s.not_full.wait_timeout(q, left).unwrap_or_else(PoisonError::into_inner).0;
        }
        q.jobs.push_back(Queued { job, weight, submitted: start });
        q.bytes += weight;
        let n = q.jobs.len();
        s.queue_now.store(n, Ordering::Relaxed);
        s.queue_max.fetch_max(n, Ordering::Relaxed);
        drop(q);
        s.not_empty.notify_one();
    }

    /// The counters now.
    pub fn stats(&self) -> PoolStats {
        let s = &*self.shared;
        PoolStats {
            workers: s.jobs_per_worker.len(),
            jobs: s.jobs.load(Ordering::Relaxed),
            queue_now: s.queue_now.load(Ordering::Relaxed),
            queue_max: s.queue_max.load(Ordering::Relaxed),
            jobs_per_worker: s.jobs_per_worker.iter().map(|c| c.load(Ordering::Relaxed)).collect(),
            max_weight: s.max_weight.load(Ordering::Relaxed),
            max_queue_wait_ms: s.max_queue_wait_ms.load(Ordering::Relaxed),
        }
    }

    /// Close the queue and wait until every worker has exited or `limit`
    /// passed. Return the workers still busy at the limit.
    fn close_and_wait(&self, limit: Duration) -> Result<(), usize> {
        let s = &*self.shared;
        let deadline = Instant::now() + limit;
        let n = self.workers.len();
        let mut q = s.lock();
        q.closed = true;
        s.not_empty.notify_all();
        while q.exited < n {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(n - q.exited);
            }
            q = s.done.wait_timeout(q, left.min(WAIT_SLICE)).unwrap_or_else(PoisonError::into_inner).0;
        }
        Ok(())
    }

    /// Close the queue, let the workers run every queued job (no job is
    /// dropped), and join them. Each wait is at most `limit` in total; at
    /// the limit, return `Err` with the number of workers that are still
    /// busy (they are left to run; the caller aborts).
    pub fn drain_and_join(mut self, limit: Duration) -> Result<PoolStats, usize> {
        if let Err(busy) = self.close_and_wait(limit) {
            // Detach the workers, so the drop of `self` does not wait again.
            self.workers.clear();
            return Err(busy);
        }
        for h in std::mem::take(&mut self.workers) {
            // Each worker is at its end, so this returns at once.
            let _ = h.join();
        }
        Ok(self.stats())
    }
}

impl Drop for Pool {
    /// A pool that was not drained (the mount failed before the session
    /// took the file system): close the queue and join the workers, with
    /// the limit `DRAIN_WAIT`. At the limit, print one line and leave them.
    fn drop(&mut self) {
        if self.workers.is_empty() {
            return;
        }
        match self.close_and_wait(DRAIN_WAIT) {
            Ok(()) => {
                for h in std::mem::take(&mut self.workers) {
                    let _ = h.join();
                }
            }
            Err(busy) => {
                eprintln!("[pool] {busy} worker(s) still busy {} s after the pool was dropped", DRAIN_WAIT.as_secs());
                self.workers.clear();
            }
        }
    }
}

/// Counts the exit of a worker, also when its job panics.
struct Exit<'a>(&'a Shared);

impl Drop for Exit<'_> {
    fn drop(&mut self) {
        let mut q = self.0.lock();
        q.exited += 1;
        drop(q);
        self.0.done.notify_all();
    }
}

/// One worker: run jobs until the queue is closed and empty.
fn worker_loop(s: &Shared, k: usize) {
    // As on the flusher: a cell or stripe lock taken outside the commit
    // gate panics (debug builds; a no-op in a release build).
    chunk_store::sync::rank::require_gate(true);
    let _exit = Exit(s);
    loop {
        let item = {
            let mut q = s.lock();
            loop {
                if let Some(it) = q.jobs.pop_front() {
                    q.bytes -= it.weight;
                    s.queue_now.store(q.jobs.len(), Ordering::Relaxed);
                    break Some(it);
                }
                if q.closed {
                    break None;
                }
                // Ends when a job comes or at the close (unmount).
                q = s.not_empty.wait(q).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let Some(it) = item else { return };
        s.not_full.notify_all();
        let wait_ms = u64::try_from(it.submitted.elapsed().as_millis()).unwrap_or(u64::MAX);
        s.max_queue_wait_ms.fetch_max(wait_ms, Ordering::Relaxed);
        s.max_weight.fetch_max(it.weight, Ordering::Relaxed);
        (it.job)();
        s.jobs_per_worker[k].fetch_add(1, Ordering::Relaxed);
        s.jobs.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workers_for_cpus_halves_and_clamps() {
        for (cpus, n) in [(1, 1), (2, 1), (3, 1), (4, 2), (16, 8), (32, 8)] {
            assert_eq!(workers_for_cpus(cpus), n, "{cpus} CPUs");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn default_workers_is_one_on_macos() {
        assert_eq!(default_workers(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn default_workers_is_in_range() {
        let n = default_workers();
        assert!((1..=DEFAULT_WORKERS_MAX).contains(&n), "default {n}");
    }
}
