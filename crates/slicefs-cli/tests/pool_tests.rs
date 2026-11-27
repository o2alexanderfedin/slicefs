//! Tests of the request worker pool (Phase 21): the parallel run, the
//! queue bounds in slots and in bytes, the drain and its time limit, and
//! the counters. No fuser type and no file system: only `pool` and std.
//! Every wait has a deadline; a parked job ends after 10 s at most.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use slicefs_cli::pool::{Job, Pool};

/// A job parks on this until `open` (or for 10 s at most).
#[derive(Clone, Default)]
struct Release(Arc<(Mutex<bool>, Condvar)>);

impl Release {
    fn open(&self) {
        *self.0 .0.lock().unwrap() = true;
        self.0 .1.notify_all();
    }

    fn wait(&self) {
        let g = self.0 .0.lock().unwrap();
        let _ = self.0 .1.wait_timeout_while(g, Duration::from_secs(10), |open| !*open).unwrap();
    }
}

/// Submit a job that signals its start and then parks until `rel` opens.
/// Return the receiver of the start signal.
fn park(pool: &Pool, rel: &Release) -> Receiver<()> {
    let (tx, rx) = mpsc::channel();
    let rel = rel.clone();
    pool.submit(
        0,
        Box::new(move || {
            let _ = tx.send(());
            rel.wait();
        }),
    );
    rx
}

/// Wait for the start signal of a parked job (5 s at most).
fn started(rx: &Receiver<()>) {
    rx.recv_timeout(Duration::from_secs(5)).expect("the parked job started");
}

/// A job that adds 1 to `ran`.
fn count(ran: &Arc<AtomicUsize>) -> Job {
    let ran = Arc::clone(ran);
    Box::new(move || {
        ran.fetch_add(1, Ordering::SeqCst);
    })
}

/// Wait until `ran` is `n` (2 s at most); return the last value.
fn wait_count(ran: &AtomicUsize, n: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(2);
    while ran.load(Ordering::SeqCst) < n && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    ran.load(Ordering::SeqCst)
}

const LARGE: usize = 1 << 30;

#[test]
fn jobs_run_in_parallel() {
    let pool = Pool::new(4).unwrap();
    let both = Arc::new((Mutex::new(0usize), Condvar::new()));
    let (tx, rx) = mpsc::channel();
    for _ in 0..4 {
        let both = Arc::clone(&both);
        let tx = tx.clone();
        pool.submit(
            0,
            Box::new(move || {
                let (m, cv) = &*both;
                let mut n = m.lock().unwrap();
                *n += 1;
                cv.notify_all();
                let (n, _) = cv.wait_timeout_while(n, Duration::from_secs(5), |n| *n < 4).unwrap();
                let _ = tx.send(*n);
            }),
        );
    }
    for _ in 0..4 {
        let seen = rx.recv_timeout(Duration::from_secs(6)).expect("a job ended");
        assert_eq!(seen, 4, "each job saw all 4 jobs started at the same time");
    }
    pool.drain_and_join(Duration::from_secs(5)).expect("drain");
}

#[test]
fn jobs_run_on_named_workers() {
    let pool = Pool::new(2).unwrap();
    let (tx, rx) = mpsc::channel();
    pool.submit(
        0,
        Box::new(move || {
            let _ = tx.send(std::thread::current().name().unwrap_or("").to_owned());
        }),
    );
    let name = rx.recv_timeout(Duration::from_secs(2)).expect("the job ran");
    assert!(name.starts_with("slicefs-worker-"), "thread name {name:?}");
    pool.drain_and_join(Duration::from_secs(5)).expect("drain");
}

#[test]
fn submit_blocks_at_the_slot_bound() {
    let pool = Pool::with_limits(1, 2, LARGE, Duration::from_secs(10)).unwrap();
    let rel = Release::default();
    let ran = Arc::new(AtomicUsize::new(0));
    started(&park(&pool, &rel));
    pool.submit(0, count(&ran));
    pool.submit(0, count(&ran));
    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        let pool = &pool;
        let job = count(&ran);
        s.spawn(move || {
            pool.submit(0, job);
            let _ = tx.send(());
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(200)),
            Err(RecvTimeoutError::Timeout),
            "a submit to a queue with no free slot blocks"
        );
        rel.open();
        rx.recv_timeout(Duration::from_secs(2)).expect("the submit returns after the release");
    });
    assert_eq!(wait_count(&ran, 3), 3, "the queued jobs ran");
    let st = pool.drain_and_join(Duration::from_secs(5)).expect("drain");
    assert_eq!(st.jobs, 4, "all 4 jobs ran");
}

#[test]
fn submit_blocks_at_the_byte_bound() {
    let pool = Pool::with_limits(1, 16, 1000, Duration::from_secs(10)).unwrap();
    let rel = Release::default();
    let ran = Arc::new(AtomicUsize::new(0));
    started(&park(&pool, &rel));
    pool.submit(600, count(&ran));
    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        let pool = &pool;
        let job = count(&ran);
        s.spawn(move || {
            pool.submit(600, job);
            let _ = tx.send(());
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(200)),
            Err(RecvTimeoutError::Timeout),
            "a submit over the byte bound blocks"
        );
        rel.open();
        rx.recv_timeout(Duration::from_secs(2)).expect("the submit returns after the release");
    });
    assert_eq!(wait_count(&ran, 2), 2, "both jobs ran");
    let st = pool.drain_and_join(Duration::from_secs(5)).expect("drain");
    assert_eq!(st.max_weight, 600);
}

#[test]
fn an_oversize_job_passes_when_the_queue_is_empty() {
    let pool = Pool::with_limits(1, 16, 1000, Duration::from_secs(10)).unwrap();
    let ran = Arc::new(AtomicUsize::new(0));
    let t = Instant::now();
    pool.submit(5000, count(&ran));
    assert!(t.elapsed() < Duration::from_secs(1), "the submit returned at once: {:?}", t.elapsed());
    assert_eq!(wait_count(&ran, 1), 1, "the oversize job ran");
    let st = pool.drain_and_join(Duration::from_secs(5)).expect("drain");
    assert_eq!(st.max_weight, 5000);
}

#[test]
fn drain_runs_every_queued_job() {
    let pool = Pool::with_limits(1, 8, LARGE, Duration::from_secs(10)).unwrap();
    let rel = Release::default();
    let ran = Arc::new(AtomicUsize::new(0));
    started(&park(&pool, &rel));
    for _ in 0..5 {
        pool.submit(0, count(&ran));
    }
    let r = std::thread::scope(|s| {
        let h = s.spawn(move || pool.drain_and_join(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(50));
        rel.open();
        h.join().unwrap()
    });
    let st = r.expect("the drain ends in its limit");
    assert_eq!(st.jobs, 6, "the drain returned after all 6 jobs");
    assert_eq!(ran.load(Ordering::SeqCst), 5, "every queued job ran");
}

#[test]
fn drain_limit_reports_a_stuck_worker() {
    let pool = Pool::with_limits(1, 8, LARGE, Duration::from_secs(10)).unwrap();
    let rel = Release::default();
    started(&park(&pool, &rel));
    let t = Instant::now();
    let r = pool.drain_and_join(Duration::from_millis(200));
    let took = t.elapsed();
    rel.open();
    assert!(matches!(r, Err(1)), "one worker is still busy: {r:?}");
    assert!(took < Duration::from_secs(2), "the drain ended at its limit: {took:?}");
}

#[test]
fn stats_count_the_queue_and_the_jobs_per_worker() {
    let pool = Pool::with_limits(2, 8, LARGE, Duration::from_secs(10)).unwrap();
    let rel = Release::default();
    let ran = Arc::new(AtomicUsize::new(0));
    let a = park(&pool, &rel);
    let b = park(&pool, &rel);
    started(&a);
    started(&b);
    for _ in 0..3 {
        pool.submit(0, count(&ran));
    }
    let now = pool.stats();
    assert_eq!(now.queue_now, 3, "3 jobs wait: {now:?}");
    assert!(now.queue_max >= 3, "the largest depth: {now:?}");
    rel.open();
    let st = pool.drain_and_join(Duration::from_secs(5)).expect("drain");
    assert_eq!(st.queue_now, 0, "{st:?}");
    assert_eq!(st.jobs, 5, "{st:?}");
    assert_eq!(st.jobs_per_worker.len(), 2, "{st:?}");
    assert_eq!(st.jobs_per_worker.iter().sum::<u64>(), st.jobs, "{st:?}");
    let line = st.to_string();
    assert!(line.starts_with("pool: workers=2 jobs=5 "), "{line}");
    assert!(line.contains("queue_max=") && line.contains("jobs_per_worker="), "{line}");
}

#[cfg(debug_assertions)]
#[test]
fn a_submit_with_a_lock_held_panics() {
    use chunk_store::sync::{rank, Rank};
    let pool = Pool::new(1).unwrap();
    let marker = rank::enter(Rank::Short);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pool.submit(0, Box::new(|| {}))));
    drop(marker);
    assert!(r.is_err(), "a submit with a ranked lock held panics");
    pool.drain_and_join(Duration::from_secs(5)).expect("drain");
}
