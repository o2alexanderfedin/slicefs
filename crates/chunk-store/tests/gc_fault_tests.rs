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
    ChunkStore::open_with(Arc::new(mem.clone()), Options { pack_limit: 256 * 1024, index_background_merge: false, ..Options::default() }).unwrap()
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

// ── The GC on the run index (17-07) ──────────────────────────────────────────

mod on_runs {
    use super::{random, readable};
    use chunk_store::mem::MemIo;
    use chunk_store::pack_io::{Kind, PackIo};
    use chunk_store::{ChunkStore, Error, ObjectId, Options};
    use std::collections::HashSet;
    use std::sync::Arc;

    type Obj = (ObjectId, Vec<u8>);

    /// 64 KiB packs, the memtable limit `limit`, no background merges.
    fn opts(limit: usize) -> Options {
        Options { pack_limit: 64 << 10, index_memtable_limit: limit, index_background_merge: false, ..Options::default() }
    }

    fn open(mem: &MemIo, o: &Options) -> ChunkStore {
        ChunkStore::open_with(Arc::new(mem.clone()), o.clone()).unwrap()
    }

    /// 300 synced objects of 1,000 bytes; one in four is live.
    fn setup(mem: &MemIo, o: &Options) -> (ChunkStore, Vec<Obj>, HashSet<ObjectId>) {
        let s = open(mem, o);
        let objs: Vec<Obj> = (0..300).map(|i| { let d = random(1000, 50_000 + i); (s.put(&d).unwrap(), d) }).collect();
        s.sync().unwrap();
        let live = objs.iter().step_by(4).map(|(id, _)| *id).collect();
        (s, objs, live)
    }

    fn pack_lens(mem: &MemIo) -> Vec<(u64, usize)> {
        let mut nums = mem.list().unwrap();
        nums.sort_unstable();
        nums.into_iter().map(|n| (n, mem.len(n, Kind::Pack).unwrap())).collect()
    }

    fn run_list(mem: &MemIo) -> Vec<u64> {
        let mut r = mem.list_runs().unwrap();
        r.sort_unstable();
        r
    }

    fn all(objs: &[Obj]) -> Vec<&Obj> {
        objs.iter().collect()
    }

    /// Removed (not live) objects are missing; live ones read back.
    fn only_live(s: &ChunkStore, objs: &[Obj], live: &HashSet<ObjectId>, what: &str) {
        for (i, (id, d)) in objs.iter().enumerate() {
            if live.contains(id) {
                assert_eq!(s.get(id).as_ref(), Some(d), "{what}: object {i}");
            } else {
                assert!(!s.contains(id) && s.get(id).is_none(), "{what}: removed object {i}");
            }
        }
    }

    // A damaged page of a run (its CRC fails in the verified reads of the
    // GC): the GC ends with `Corrupt` before any change and asks for a
    // rebuild of the index from the packs.
    #[test]
    fn a_damaged_page_in_the_gc_passes_requests_a_rebuild() {
        let mem = MemIo::new();
        let o = opts(30);
        let (s, objs, live) = setup(&mem, &o);
        assert_eq!(s.index_info().runs, 10);
        let packs = pack_lens(&mem);
        mem.flip(run_list(&mem)[0], Kind::Run, 100);
        let b = s.gc_begin().unwrap();
        let r = s.gc_finish(&live, b);
        assert!(matches!(r, Err(Error::Corrupt(_))), "{r:?}");
        assert_eq!(pack_lens(&mem), packs, "no pack is deleted, punched or cut");
        let manifest = mem.read(0, Kind::Manifest).unwrap();
        let flags = u32::from_le_bytes(manifest[6..10].try_into().unwrap());
        assert_eq!(flags & 1, 1, "the MANIFEST asks for a rebuild");
        // A read-only open (a scrub or `slicefs stats` next to the mount)
        // reads every pack into RAM.
        let ro = open(&mem, &Options { read_only: true, ..o.clone() });
        readable(&ro, &all(&objs), "read-only open");
        assert_eq!(ro.stats().objects, 300);
        drop(ro);
        drop(s);
        let s = open(&mem, &o);
        readable(&s, &all(&objs), "after the rebuild");
        assert_eq!(s.stats().objects, 300);
        assert!(run_list(&mem).iter().all(|r| *r > 10), "the old runs are gone");
    }

    // The install MANIFEST cannot be written: the store fails, no pack
    // changes, and after a power loss every object (live and dead) is
    // there and no pending run is left. A later GC works.
    #[test]
    fn a_failed_install_changes_no_pack() {
        let mem = MemIo::new();
        let o = opts(1 << 18);
        let (s, objs, live) = setup(&mem, &o);
        let packs = pack_lens(&mem);
        let runs = run_list(&mem);
        let b = s.gc_begin().unwrap();
        mem.fail_next_write_durable(Kind::Manifest, 1);
        assert!(s.gc_finish(&live, b).is_err(), "the install fails");
        for &(n, len) in &packs {
            assert_eq!(mem.len(n, Kind::Pack), Some(len), "pack {n} is not deleted, punched or cut");
        }
        assert!(s.put(&random(1000, 1)).is_err(), "the store is failed");
        drop(s);
        mem.crash();
        let s = open(&mem, &o);
        readable(&s, &all(&objs), "after a crash and a reopen");
        assert_eq!(s.stats().objects, 300);
        assert_eq!(run_list(&mem), runs, "no pending run is left");
        let b = s.gc_begin().unwrap();
        let st = s.gc_finish(&live, b).unwrap();
        assert_eq!(st.removed_objects, 225, "{st:?}");
        only_live(&s, &objs, &live, "after a later GC");
        assert_eq!(s.stats().objects, 75);
    }

    // The tombstones are durable but no pack can be removed: the removed
    // objects stay missing (also after a power loss), and a later GC
    // removes the packs that are left.
    #[test]
    fn a_failed_pack_removal_after_the_install() {
        let mem = MemIo::new();
        let o = opts(30);
        let (s, objs, live) = setup(&mem, &o);
        let before = mem.list().unwrap().len();
        let b = s.gc_begin().unwrap();
        mem.set_fail_removes(true);
        assert!(s.gc_finish(&live, b).is_err(), "the removal fails");
        only_live(&s, &objs, &live, "after the failed removal");
        s.sync().unwrap();
        drop(s);
        mem.crash();
        let s = open(&mem, &o);
        only_live(&s, &objs, &live, "after a crash and a reopen");
        assert_eq!(s.stats().objects, 75);
        mem.set_fail_removes(false);
        let left = mem.list().unwrap().len();
        assert!(left > before, "the old packs are still there, next to the copies");
        let b = s.gc_begin().unwrap();
        s.gc_finish(&live, b).unwrap();
        assert!(mem.list().unwrap().len() < left, "the later GC removes the packs that are left");
        only_live(&s, &objs, &live, "after the later GC");
        drop(s);
        let s = open(&mem, &o);
        only_live(&s, &objs, &live, "after a reopen");
        assert_eq!(s.stats().objects, 75);
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
