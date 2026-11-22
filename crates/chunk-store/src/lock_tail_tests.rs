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

/// `len` bytes (at least 4) that differ for each `i`: `i` comes first.
fn data(i: u32, len: usize) -> Vec<u8> {
    let mut d: Vec<u8> = (0..len as u32).map(|j| (j.wrapping_mul(31) ^ (j >> 7)) as u8).collect();
    d[..4].copy_from_slice(&i.to_le_bytes());
    d
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

// A's put needs a new pack (the last put sealed pack 1), and the create of
// its file is held. B's read of an older object must not wait for it.
#[test]
fn put_and_read_pass_a_held_pack_create() {
    let mem = MemIo::new();
    let s = store(&mem, small_packs());
    let x1 = s.put(&data(1, 3000)).unwrap();
    s.put(&data(2, 3000)).unwrap();
    s.sync().unwrap();
    assert_eq!(s.stats().packs, 1);
    mem.hold_creates(true);
    let a = {
        let s = Arc::clone(&s);
        spawn(move || s.put(&data(3, 3000)).unwrap())
    };
    assert!(mem.wait_parked(Hold::Creates, 1, LIMIT), "A is in the held pack create");
    let b = {
        let s = Arc::clone(&s);
        spawn(move || s.read(&x1).unwrap())
    }
    .recv_timeout(LIMIT);
    let a_waited = a.try_recv().is_err();
    mem.hold_creates(false);
    assert_eq!(b.expect("B's read finished while A created a pack"), data(1, 3000));
    assert!(a_waited, "A was still in the pack create while B ran");
    let x3 = a.recv_timeout(LIMIT).expect("A ends after the create is released");
    assert_eq!(s.read(&x3).unwrap(), data(3, 3000));
    assert_eq!(s.stats().packs, 2);
    s.sync().unwrap();
}

// A's puts pass KEEP_LIMIT (a background sync starts and is held), then
// reach KEEP_CAP: the next put of A waits for that sync. B's small put (it
// stays below the cap) and B's read must not wait for it.
#[test]
fn put_and_read_pass_a_held_keep_cap_wait() {
    use crate::retry::{RetryingIo, KEEP_CAP};
    const MIB: usize = 1 << 20;
    let mem = MemIo::new();
    let io = Arc::new(RetryingIo::with_backoff(Arc::new(mem.clone()), Vec::new()));
    let o = Options { pack_limit: 1 << 30, ..small_packs() };
    let s = Arc::new(ChunkStore::open_with(io, o).unwrap());
    mem.hold_syncs(true);
    let n = KEEP_CAP / MIB + 2;
    let a = {
        let s = Arc::clone(&s);
        spawn(move || (0..n as u32).map(|i| s.put(&data(i, MIB)).unwrap()).collect::<Vec<_>>())
    };
    assert!(mem.wait_parked(Hold::Syncs, 1, LIMIT), "the background sync at KEEP_LIMIT is held");
    wait_until("A waits for the background sync at KEEP_CAP", || s.test_waits() >= 1);
    let first = s.hasher().id(&data(0, MIB));
    let small = data(1000, 100);
    let b = put_and_read(&s, small.clone(), first).recv_timeout(LIMIT);
    let a_waited = a.try_recv().is_err();
    mem.hold_syncs(false);
    let (x, got) = b.expect("B's put and read finished while A waited at KEEP_CAP");
    assert!(a_waited, "A was still at KEEP_CAP while B ran");
    assert!(got == data(0, MIB));
    let xs = a.recv_timeout(LIMIT).expect("A ends after the sync is released");
    assert_eq!(xs.len(), n);
    assert_eq!(s.read(&x).unwrap(), small);
    assert!(s.read(&xs[n - 1]).unwrap() == data(n as u32 - 1, MIB));
    s.sync().unwrap();
}

// A's put fills the memtable (limit 4), so it writes an index run; the run
// file create is held. B's put and its reads (one of an object that is only
// in the memtable that A writes out) must not wait for it.
#[test]
fn put_and_read_pass_a_held_run_write() {
    let mem = MemIo::new();
    let o = Options { pack_limit: 1 << 20, index_memtable_limit: 4, ..small_packs() };
    let s = store(&mem, o);
    let x1 = s.put(&data(1, 300)).unwrap();
    s.put(&data(2, 300)).unwrap();
    s.put(&data(3, 300)).unwrap();
    assert_eq!(s.index_info().runs, 0);
    mem.hold_creates(true);
    let x4 = s.hasher().id(&data(4, 300));
    let a = {
        let s = Arc::clone(&s);
        spawn(move || s.put(&data(4, 300)).unwrap())
    };
    assert!(mem.wait_parked(Hold::Creates, 1, LIMIT), "A is in the held run create");
    let b = {
        let s = Arc::clone(&s);
        spawn(move || (s.put(&data(5, 300)).unwrap(), s.read(&x1).unwrap(), s.read(&x4).unwrap()))
    }
    .recv_timeout(LIMIT);
    let a_waited = a.try_recv().is_err();
    mem.hold_creates(false);
    let (x5, got1, got4) = b.expect("B's put and reads finished while A wrote a run");
    assert!(a_waited, "A was still in the run write while B ran");
    assert_eq!((got1, got4), (data(1, 300), data(4, 300)));
    assert_eq!(a.recv_timeout(LIMIT).expect("A ends after the create is released"), x4);
    assert_eq!(s.index_info().runs, 1);
    assert_eq!(s.index_info().memtable_entries, 1, "B's object stays in the memtable");
    for (x, i) in [(x1, 1), (x4, 4), (x5, 5)] {
        assert_eq!(s.read(&x).unwrap(), data(i, 300));
    }
    s.sync().unwrap();
}
