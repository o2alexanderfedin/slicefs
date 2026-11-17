//! #63: a GC that fails in `gc_finish` loses nothing. Live objects and the
//! objects put or read during the GC stay readable, a later GC works, and a
//! reopen after a power loss finds every durable live object.

use chunk_store::mem::MemIo;
use chunk_store::{ChunkStore, ObjectId, Options};
use std::collections::HashSet;
use std::sync::Arc;

fn random(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn store(mem: &MemIo) -> ChunkStore {
    ChunkStore::open_with(Arc::new(mem.clone()), Options { pack_limit: 256 * 1024, ..Options::default() }).unwrap()
}

fn put_many(s: &ChunkStore, n: u64, seed: u64) -> Vec<(ObjectId, Vec<u8>)> {
    (0..n).map(|i| { let d = random(8000, seed + i); (s.put(&d).unwrap(), d) }).collect()
}

fn readable(s: &ChunkStore, objs: &[&(ObjectId, Vec<u8>)], what: &str) {
    for (id, d) in objs {
        assert_eq!(s.get(id).as_deref(), Some(d.as_slice()), "{what}: {id:?}");
    }
}

/// Objects with one live in four (so GC compacts packs, which reads them),
/// plus `t`, put during the GC, and `r`, a dead object read during the GC.
struct Case {
    objs: Vec<(ObjectId, Vec<u8>)>,
    live: HashSet<ObjectId>,
    t: (ObjectId, Vec<u8>),
    r: (ObjectId, Vec<u8>),
    boundary: u64,
}

fn begin(s: &ChunkStore, seed: u64) -> Case {
    let objs = put_many(s, 120, seed);
    s.sync().unwrap();
    let live: HashSet<ObjectId> = objs.iter().step_by(4).map(|(id, _)| *id).collect();
    let boundary = s.gc_begin().unwrap();
    let td = random(8000, seed + 10_000);
    let t = (s.put(&td).unwrap(), td);
    let r = objs[1].clone(); // not live
    assert!(!live.contains(&r.0));
    assert_eq!(s.get(&r.0).as_deref(), Some(r.1.as_slice()));
    s.sync().unwrap();
    Case { objs, live, t, r, boundary }
}

impl Case {
    fn live_objs(&self) -> Vec<&(ObjectId, Vec<u8>)> {
        self.objs.iter().filter(|(id, _)| self.live.contains(id)).collect()
    }
    /// What the next mark sees: the live set plus the object put during the
    /// failed GC (a commit after the next gc_begin makes it reachable).
    fn next_live(&self) -> HashSet<ObjectId> {
        let mut l = self.live.clone();
        l.insert(self.t.0);
        l
    }
}

fn retry_and_reopen(mem: &MemIo, s: ChunkStore, c: &Case) {
    let b = s.gc_begin().unwrap();
    s.gc_finish(&c.next_live(), b).expect("a later GC works");
    readable(&s, &c.live_objs(), "live after the retried GC");
    readable(&s, &[&c.t], "object put during the failed GC");
    drop(s);
    mem.crash();
    let s = store(mem);
    readable(&s, &c.live_objs(), "live after a reopen");
    readable(&s, &[&c.t], "object put during the failed GC, after a reopen");
}

#[test]
fn a_gc_that_fails_on_a_read_loses_nothing() {
    let mem = MemIo::new();
    let s = store(&mem);
    let c = begin(&s, 1);
    mem.set_fail_reads(true);
    assert!(s.gc_finish(&c.live, c.boundary).is_err(), "the compaction read fails");
    mem.set_fail_reads(false);
    readable(&s, &c.live_objs(), "live after the failed GC");
    readable(&s, &[&c.t], "put during the GC");
    readable(&s, &[&c.r], "read during the GC");
    retry_and_reopen(&mem, s, &c);
}

#[test]
fn a_gc_that_fails_on_a_sync_loses_nothing() {
    let mem = MemIo::new();
    let s = store(&mem);
    let c = begin(&s, 2);
    mem.fail_next_syncs(1);
    assert!(s.gc_finish(&c.live, c.boundary).is_err(), "the sync of the copies fails");
    // A failed sync fails the store: reopen it, as a mount would.
    drop(s);
    mem.crash();
    let s = store(&mem);
    readable(&s, &c.live_objs(), "live after the failed GC and a reopen");
    readable(&s, &[&c.t, &c.r], "put or read during the GC, after a reopen");
    retry_and_reopen(&mem, s, &c);
}

#[test]
fn a_gc_that_fails_in_the_middle_of_compaction_loses_nothing() {
    // Each record copy is one read: fail after some copies, at many points.
    for after in [1, 3, 7, 12, 20] {
        let mem = MemIo::new();
        let s = store(&mem);
        let c = begin(&s, 100 + after as u64);
        mem.fail_reads_after(after);
        let res = s.gc_finish(&c.live, c.boundary);
        mem.clear_fail_reads_after();
        assert!(res.is_err(), "fails after {after} reads");
        readable(&s, &c.live_objs(), &format!("live, failure after {after} reads"));
        readable(&s, &[&c.t, &c.r], &format!("put or read during the GC, failure after {after} reads"));
        retry_and_reopen(&mem, s, &c);
    }
}

#[test]
fn a_crash_in_the_middle_of_gc_finish_loses_nothing() {
    // Power loss right after the copies are synced and before every old pack
    // is gone: the next open finds each live object at least once.
    let mem = MemIo::new();
    let s = store(&mem);
    let c = begin(&s, 3);
    mem.fail_reads_after(5);
    let _ = s.gc_finish(&c.live, c.boundary);
    mem.clear_fail_reads_after();
    drop(s);
    mem.crash();
    let s = store(&mem);
    readable(&s, &c.live_objs(), "live after a crash in gc_finish");
    readable(&s, &[&c.t, &c.r], "put or read during the GC, after a crash");
    retry_and_reopen(&mem, s, &c);
}
