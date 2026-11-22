//! The slow tails of the store lock (Phase 19, LOCK-01): while one thread
//! waits in a slow operation (a seal join, a pack create, the KEEP_CAP wait,
//! an index run write, a trust-floor write, a pack open), another thread's
//! `put` and `read` finish. A `MemIo` hold knob parks the slow operation, and
//! the test sees that the first thread is in it (`MemIo::wait_parked`, or the
//! store's test counter of waits outside the lock) before it starts the
//! second one. Every wait has a time limit.

use crate::mem::{Hold, MemIo};
use crate::{ChunkStore, ObjectId, Options};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// The longest wait of a test for one step.
const LIMIT: Duration = Duration::from_secs(5);

/// `len` bytes that differ for each `i`.
fn data(i: u32, len: usize) -> Vec<u8> {
    let seed = i.wrapping_mul(2_654_435_761);
    (0..len as u32).map(|j| (j.wrapping_mul(31) ^ seed ^ (seed >> 8)) as u8).collect()
}

/// Small packs (a few records fill one), no compression (fixed record
/// sizes), no background merge.
fn store(mem: &MemIo, o: Options) -> Arc<ChunkStore> {
    Arc::new(ChunkStore::open_with(Arc::new(mem.clone()), o).unwrap())
}

fn small_packs() -> Options {
    Options { pack_limit: 4096, zstd_level: 0, index_background_merge: false, locality_cache: false, ..Options::default() }
}

/// Wait until `cond` is true, at most `LIMIT`.
fn wait_until(what: &str, cond: impl Fn() -> bool) {
    let until = Instant::now() + LIMIT;
    while !cond() {
        assert!(Instant::now() < until, "timed out: {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Run `f` on a new thread; its result comes on the returned channel.
fn spawn<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> mpsc::Receiver<T> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx
}

/// B: a put of a new small object and a read of `old`, on a new thread.
fn put_and_read(s: &Arc<ChunkStore>, new: Vec<u8>, old: ObjectId) -> mpsc::Receiver<(ObjectId, Vec<u8>)> {
    let s = Arc::clone(s);
    spawn(move || (s.put(&new).unwrap(), s.read(&old).unwrap()))
}

// A's put fills the second pack, so its seal must first join the seal of
// the first pack, whose fsync is held. B's put and read must not wait for
// that join.
#[test]
fn put_and_read_pass_a_held_seal_join() {
    let mem = MemIo::new();
    let s = store(&mem, small_packs());
    mem.hold_syncs(true);
    // Records of about 3 KiB: the second one fills pack 1 and seals it.
    let x1 = s.put(&data(1, 3000)).unwrap();
    let x2 = s.put(&data(2, 3000)).unwrap();
    assert!(mem.wait_parked(Hold::Syncs, 1, LIMIT), "the seal of pack 1 is in its held sync");
    let x3 = s.put(&data(3, 3000)).unwrap();
    assert_eq!(s.stats().packs, 2);

    let a = {
        let s = Arc::clone(&s);
        spawn(move || s.put(&data(4, 3000)).unwrap())
    };
    wait_until("A waits for the seal of pack 1", || s.test_waits() >= 1);
    // A small record that does not fill pack 2.
    let small = data(5, 100);
    let b = put_and_read(&s, small.clone(), x1).recv_timeout(LIMIT);
    let a_waited = a.try_recv().is_err();
    mem.hold_syncs(false);
    let (x5, got) = b.expect("B's put and read finished while A waited for the seal join");
    assert!(a_waited, "A was still in the seal join while B ran");
    assert_eq!(got, data(1, 3000));
    let x4 = a.recv_timeout(LIMIT).expect("A ends after the sync is released");
    for (x, d) in [(x1, data(1, 3000)), (x2, data(2, 3000)), (x3, data(3, 3000)), (x4, data(4, 3000)), (x5, small)] {
        assert_eq!(s.read(&x).unwrap(), d);
    }
    s.sync().unwrap();
}
