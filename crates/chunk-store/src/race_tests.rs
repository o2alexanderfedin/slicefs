//! The races of a lookup outside the store lock (17-05): `put` and `read`
//! look up an ID, then take the store lock. A whole GC can run in between.
//! The test hook `ChunkStore::after_lookup` runs that GC at exactly that
//! point, so the tests need no threads and no timing. Each hook sets a flag
//! that the test checks: a test whose hook never ran fails.

use crate::mem::MemIo;
use crate::pack_io::{Kind, PackIo};
use crate::{ChunkStore, Options};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Every put goes to a run at once (memtable limit 1).
fn store(mem: &MemIo) -> ChunkStore {
    let o = Options { index_memtable_limit: 1, index_background_merge: false, ..Options::default() };
    ChunkStore::open_with(Arc::new(mem.clone()), o).unwrap()
}

fn data(i: u8) -> Vec<u8> {
    (0..1000u32).map(|j| (j as u8).wrapping_mul(31).wrapping_add(i)).collect()
}

fn set_hook(s: &ChunkStore, ran: &Arc<AtomicBool>, f: impl FnOnce(&ChunkStore) + Send + 'static) {
    let ran = Arc::clone(ran);
    let mut f = Some(f);
    *s.after_lookup.lock().unwrap() = Some(Box::new(move |s: &ChunkStore| {
        (f.take().unwrap())(s);
        ran.store(true, Ordering::SeqCst);
    }));
}

fn pack_bytes(mem: &MemIo) -> usize {
    mem.list().unwrap().into_iter().map(|n| mem.len(n, Kind::Pack).unwrap()).sum()
}

// The lookup of put finds X (in a run). Before put takes the lock, a full
// GC with an empty live set removes X and deletes its pack. The put must
// see that under the lock and write X again: a dedup hit there would lose
// X without an error.
#[test]
fn a_put_hit_that_gc_removed_before_the_lock_writes_again() {
    let mem = MemIo::new();
    let s = store(&mem);
    let d = data(1);
    let x = s.put(&d).unwrap();
    assert_eq!(s.index_info().memtable_entries, 0, "X is in a run");
    let ran = Arc::new(AtomicBool::new(false));
    set_hook(&s, &ran, |s| {
        let b = s.gc_begin().unwrap();
        let st = s.gc_finish(&HashSet::new(), b).unwrap();
        assert_eq!((st.removed_objects, st.packs_deleted), (1, 1), "{st:?}");
        assert!(!s.contains(&s.hasher().id(&data(1))));
    });
    assert_eq!(s.put(&d).unwrap(), x);
    assert!(ran.load(Ordering::SeqCst), "the hook ran between the lookup and the lock");
    assert_eq!(s.get(&x), Some(d.clone()));
    assert_eq!(s.stats().objects, 1);
    s.sync().unwrap();
    drop(s);
    mem.crash();
    let s = store(&mem);
    assert_eq!(s.get(&x), Some(d), "after a crash and a reopen");
}

// The lookup of read finds X in pack P (mostly dead). Before read takes the
// lock, a GC compacts P: it copies X to a new pack and deletes P. The read
// must use the new place.
#[test]
fn a_read_whose_loc_gc_moved_returns_the_bytes_from_the_new_pack() {
    let mem = MemIo::new();
    let s = store(&mem);
    let objs: Vec<_> = (0..10).map(|i| (s.put(&data(i)).unwrap(), data(i))).collect();
    let (x, dx) = objs[4].clone();
    let old_packs = mem.list().unwrap();
    assert_eq!(old_packs.len(), 1);
    let ran = Arc::new(AtomicBool::new(false));
    set_hook(&s, &ran, move |s| {
        let b = s.gc_begin().unwrap();
        let st = s.gc_finish(&HashSet::from([x]), b).unwrap();
        assert_eq!((st.packs_compacted, st.kept_objects, st.removed_objects), (1, 1, 9), "{st:?}");
    });
    assert_eq!(s.read(&x).unwrap(), dx);
    assert!(ran.load(Ordering::SeqCst), "the hook ran between the lookup and the lock");
    assert!(!mem.exists(old_packs[0], Kind::Pack), "the GC deleted the old pack");
}

// Control: no GC between the lookup and the lock. A dedup hit writes
// nothing.
#[test]
fn a_put_hit_without_a_gc_writes_nothing() {
    let mem = MemIo::new();
    let s = store(&mem);
    let d = data(7);
    let x = s.put(&d).unwrap();
    let before = pack_bytes(&mem);
    let gen = s.index.gen();
    let ran = Arc::new(AtomicBool::new(false));
    set_hook(&s, &ran, |_| {});
    assert_eq!(s.put(&d).unwrap(), x);
    assert!(ran.load(Ordering::SeqCst), "the hook ran");
    assert_eq!(pack_bytes(&mem), before, "a dedup hit writes nothing");
    assert_eq!(s.index.gen(), gen, "and flushes nothing");
    assert_eq!(s.stats().objects, 1);
}

fn set_merge_hook(s: &ChunkStore, ran: &Arc<AtomicBool>, f: impl FnOnce(&ChunkStore) + Send + 'static) {
    let ran = Arc::clone(ran);
    let mut f = Some(f);
    *s.after_merge_write.lock().unwrap() = Some(Box::new(move |s: &ChunkStore| {
        (f.take().unwrap())(s);
        ran.store(true, Ordering::SeqCst);
    }));
}

// A merge reads and writes with no store lock (17-06). Two flushes between
// its write and its install add two newer runs: the install must keep them
// (it builds the run list and the MANIFEST from the state at the install,
// not from the state when the merge started).
#[test]
fn a_flush_during_a_merge_keeps_the_new_runs() {
    let mem = MemIo::new();
    let s = store(&mem);
    let mut objs: Vec<_> = (0..4).map(|i| (s.put(&data(i)).unwrap(), data(i))).collect();
    assert_eq!(s.index_info().runs, 4);
    let ran = Arc::new(AtomicBool::new(false));
    set_merge_hook(&s, &ran, |s| {
        for i in 4..6 {
            s.put(&data(i)).unwrap();
        }
        assert_eq!(s.index_info().runs, 6);
    });
    assert!(s.merge_index_now().unwrap());
    assert!(ran.load(Ordering::SeqCst), "the hook ran between the write and the install");
    objs.extend((4..6).map(|i| (s.hasher().id(&data(i)), data(i))));
    assert_eq!(s.index_run_entries(), vec![1, 1, 4]);
    for (id, d) in &objs {
        assert_eq!(s.get(id).as_ref(), Some(d));
    }
    s.sync().unwrap();
    drop(s);
    mem.crash();
    let s = store(&mem);
    assert_eq!(s.index_run_entries(), vec![1, 1, 4], "the MANIFEST lists the new runs");
    assert_eq!(mem.list_runs().unwrap().len(), 3, "the inputs are gone");
    for (id, d) in &objs {
        assert_eq!(s.get(id).as_ref(), Some(d), "after a crash and a reopen");
    }
    assert_eq!(s.stats().objects, 6);
}

// A GC between the write and the install of a merge removes X and moves
// the other objects (their old pack is deleted). The merged run still has
// the old entries; the GC run is newer, so it wins: X stays missing and the
// others read from their new places.
#[test]
fn a_gc_during_a_merge_stays_newer_than_the_merged_run() {
    let mem = MemIo::new();
    let s = store(&mem);
    let objs: Vec<_> = (0..4).map(|i| (s.put(&data(i)).unwrap(), data(i))).collect();
    let x = objs[0].0;
    let live: HashSet<_> = objs[1..].iter().map(|(id, _)| *id).collect();
    let ran = Arc::new(AtomicBool::new(false));
    set_merge_hook(&s, &ran, move |s| {
        let b = s.gc_begin().unwrap();
        let st = s.gc_finish(&live, b).unwrap();
        assert_eq!((st.removed_objects, st.packs_compacted), (1, 1), "{st:?}");
    });
    assert!(s.merge_index_now().unwrap());
    assert!(ran.load(Ordering::SeqCst), "the hook ran between the write and the install");
    // The GC tombstone run, the run of its copies (the memtable limit is 1,
    // so the copies flush during the GC, 17-07), and the merged run.
    assert_eq!(s.index_info().runs, 3);
    let check = |s: &ChunkStore, what: &str| {
        assert!(!s.contains(&x), "{what}");
        assert!(s.get(&x).is_none(), "{what}");
        for (id, d) in &objs[1..] {
            assert_eq!(s.get(id).as_ref(), Some(d), "{what}");
        }
        assert_eq!(s.stats().objects, 3, "{what}");
    };
    check(&s, "after the merge");
    s.sync().unwrap();
    drop(s);
    mem.crash();
    let s = store(&mem);
    check(&s, "after a crash and a reopen");
    // No stale dedup hit: X is written again.
    assert_eq!(s.put(&objs[0].1).unwrap(), x);
    assert_eq!(s.get(&x).as_ref(), Some(&objs[0].1));
}

// ── gc_finish outside the store lock (17-07) ─────────────────────────────────
//
// `gc_finish` calls `gc_pass_hook` at the start of its pass 1 (the totals
// over the run snapshot) and of its pass 2 (one selected pack at a time),
// with no store lock held. The hooks set faults or put objects there.

type Obj = (crate::ObjectId, Vec<u8>);

fn gc_opts() -> Options {
    Options { pack_limit: 64 << 10, index_memtable_limit: 30, index_background_merge: false, ..Options::default() }
}

/// 1,000 bytes that differ for each `i`.
fn blob(i: u64) -> Vec<u8> {
    let mut x = (i + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..1000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// 300 synced objects in 64 KiB packs, ten flushes of 30 (so the memtable
/// is empty when the GC starts and every entry is in a run).
fn gc_store(mem: &MemIo) -> (ChunkStore, Vec<Obj>) {
    let s = ChunkStore::open_with(Arc::new(mem.clone()), gc_opts()).unwrap();
    let objs: Vec<Obj> = (0..300).map(|i| (s.put(&blob(i)).unwrap(), blob(i))).collect();
    s.sync().unwrap();
    let info = s.index_info();
    assert_eq!((info.runs, info.memtable_entries), (10, 0), "{info:?}");
    (s, objs)
}

/// One object in four.
fn one_in_four(objs: &[Obj]) -> HashSet<crate::ObjectId> {
    objs.iter().step_by(4).map(|(id, _)| *id).collect()
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

/// The files that a GC may change, before it.
struct Files {
    packs: Vec<(u64, usize)>,
    manifest: Vec<u8>,
    runs: Vec<u64>,
}

impl Files {
    fn of(mem: &MemIo) -> Self {
        Files { packs: pack_lens(mem), manifest: mem.read(0, Kind::Manifest).unwrap(), runs: run_list(mem) }
    }

    /// The GC failed and changed nothing: every pack has its length, the
    /// MANIFEST and the run files are the same (no pending run is left and
    /// no tombstone is durable), every object is there, a put of a victim
    /// is a dedup hit, the store takes new objects, and a reopen finds
    /// every object with an exact count.
    fn unchanged(&self, mem: &MemIo, s: ChunkStore, objs: &[Obj], what: &str) {
        assert_eq!(pack_lens(mem), self.packs, "{what}: no pack is deleted, punched or cut");
        assert_eq!(mem.read(0, Kind::Manifest).unwrap(), self.manifest, "{what}: no tombstone is durable");
        assert_eq!(run_list(mem), self.runs, "{what}: no pending run is left");
        for (i, (id, _)) in objs.iter().enumerate() {
            assert!(s.contains(id), "{what}: object {i}");
        }
        let (v, dv) = &objs[1];
        assert_eq!(s.put(dv).unwrap(), *v, "{what}");
        assert_eq!(s.get(v).as_ref(), Some(dv), "{what}");
        assert_eq!(pack_lens(mem), self.packs, "{what}: the put of a victim is a dedup hit");
        let new = blob(10_000);
        let nid = s.put(&new).expect("the store is not failed");
        assert_eq!(s.get(&nid), Some(new.clone()), "{what}");
        s.sync().unwrap();
        drop(s);
        let s = ChunkStore::open_with(Arc::new(mem.clone()), gc_opts()).unwrap();
        for (i, (id, d)) in objs.iter().enumerate() {
            assert_eq!(s.get(id).as_ref(), Some(d), "{what}: object {i} after a reopen");
        }
        assert_eq!(s.get(&nid), Some(new), "{what}");
        assert_eq!(s.stats().objects, objs.len() as u64 + 1, "{what}");
    }
}

/// Run `f` at pass `pass` of the next `gc_finish` (once); `ran` is set
/// after it.
fn at_pass(s: &ChunkStore, pass: u32, ran: &Arc<AtomicBool>, f: impl FnOnce(&ChunkStore) + Send + 'static) {
    let ran = Arc::clone(ran);
    let mut f = Some(f);
    *s.gc_pass_hook.lock().unwrap() = Some(Box::new(move |s: &ChunkStore, p: u32| {
        if p == pass {
            if let Some(f) = f.take() {
                f(s);
                ran.store(true, Ordering::SeqCst);
            }
        }
    }));
}

// A read error in the merged pass over the runs: the GC ends with the
// error before it decides anything.
#[test]
fn a_read_error_in_the_totals_pass_changes_nothing() {
    let mem = MemIo::new();
    let (s, objs) = gc_store(&mem);
    let before = Files::of(&mem);
    let ran = Arc::new(AtomicBool::new(false));
    let m = mem.clone();
    // One run page reads, the next one fails.
    at_pass(&s, 1, &ran, move |_| m.fail_reads_after(1));
    let b = s.gc_begin().unwrap();
    let r = s.gc_finish(&one_in_four(&objs), b);
    mem.clear_fail_reads_after();
    assert!(ran.load(Ordering::SeqCst), "the hook ran at pass 1");
    assert!(matches!(r, Err(crate::Error::Io(_))), "{r:?}");
    before.unchanged(&mem, s, &objs, "totals pass");
}

// A read error in the per-pack pass: the `.idx` of the first pack reads,
// the first currentness lookup fails. The fault is set at the start of
// pass 2, before the first copy, so no copy is appended and no copy flush
// runs: the "nothing changed" check then holds by design. (Later failures,
// after copies, are in tests/gc_fault_tests.rs.)
#[test]
fn a_read_error_in_the_per_pack_pass_changes_nothing() {
    let mem = MemIo::new();
    let (s, objs) = gc_store(&mem);
    let before = Files::of(&mem);
    let ran = Arc::new(AtomicBool::new(false));
    let m = mem.clone();
    at_pass(&s, 2, &ran, move |_| m.fail_reads_after(1));
    let b = s.gc_begin().unwrap();
    let r = s.gc_finish(&one_in_four(&objs), b);
    mem.clear_fail_reads_after();
    assert!(ran.load(Ordering::SeqCst), "the hook ran at pass 2");
    assert!(matches!(r, Err(crate::Error::Io(_))), "{r:?}");
    before.unchanged(&mem, s, &objs, "per-pack pass");
}

// Nothing live: every pack is deleted, so the GC makes no copy, and a read
// error at any point of pass 2 (also after pending runs were written)
// must change nothing and leave no pending run.
#[test]
fn a_read_error_after_pending_runs_changes_nothing() {
    let mut failed_after_a_pending_run = false;
    // Pass 2 reads about 330 times here (five `.idx` files, one run page
    // per entry, the tables of five pending runs).
    for after in [70, 150, 250] {
        let mem = MemIo::new();
        let (s, objs) = gc_store(&mem);
        let before = Files::of(&mem);
        let ran = Arc::new(AtomicBool::new(false));
        let m = mem.clone();
        at_pass(&s, 2, &ran, move |_| m.fail_reads_after(after));
        let b = s.gc_begin().unwrap();
        let next_run = s.index.take_run_num();
        let r = s.gc_finish(&HashSet::new(), b);
        mem.clear_fail_reads_after();
        assert!(ran.load(Ordering::SeqCst));
        assert!(r.is_err(), "after {after} reads: {r:?}");
        failed_after_a_pending_run |= s.index.take_run_num() > next_run + 2;
        before.unchanged(&mem, s, &objs, &format!("failure after {after} reads"));
    }
    assert!(failed_after_a_pending_run, "some failure came after a pending run was written");
}

// One read fails and the reads after it work (a transient error), at many
// points of both passes. Nothing live, so no copies: the GC must end with
// the error and change nothing. A lookup error taken as "not current"
// would let the GC go on and change other packs.
#[test]
fn a_transient_read_error_in_a_gc_pass_changes_nothing() {
    for (pass, skip) in [(1, 0), (1, 6), (2, 0), (2, 5), (2, 70), (2, 150), (2, 250), (2, 320)] {
        let what = format!("pass {pass}, after {skip} reads");
        let mem = MemIo::new();
        let (s, objs) = gc_store(&mem);
        let before = Files::of(&mem);
        let ran = Arc::new(AtomicBool::new(false));
        let m = mem.clone();
        at_pass(&s, pass, &ran, move |_| m.fail_read_once_after(skip));
        let b = s.gc_begin().unwrap();
        let r = s.gc_finish(&HashSet::new(), b);
        assert!(ran.load(Ordering::SeqCst), "{what}");
        assert!(matches!(r, Err(crate::Error::Io(_))), "{what}: {r:?}");
        before.unchanged(&mem, s, &objs, &what);
    }
}

// The passes run with the store lock free: puts go on, and an object that
// the GC planned to remove and that is put again during pass 2 is kept
// (the plan of its pack is dropped at the install).
#[test]
fn the_store_lock_is_free_during_the_gc_passes() {
    let mem = MemIo::new();
    let (s, objs) = gc_store(&mem);
    let live = one_in_four(&objs);
    let pack_of = |id: &crate::ObjectId| s.index.get(id).unwrap().unwrap().pack;
    let (x, dx) = objs[1].clone();
    assert!(!live.contains(&x));
    let x_pack = pack_of(&x);
    let packs: Vec<u64> = objs.iter().map(|(id, _)| pack_of(id)).collect();
    let (ran1, ran2) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (r1, r2) = (Arc::clone(&ran1), Arc::clone(&ran2));
    let dx2 = dx.clone();
    *s.gc_pass_hook.lock().unwrap() = Some(Box::new(move |s: &ChunkStore, pass: u32| {
        assert!(s.inner.try_lock().is_ok(), "pass {pass}: the store lock is free");
        assert_eq!(s.put(&blob(20_000 + pass as u64)).unwrap(), s.hasher().id(&blob(20_000 + pass as u64)));
        if pass == 2 {
            // A dedup hit on a planned victim: it touches X.
            assert_eq!(s.put(&dx2).unwrap(), x);
        }
        [&r1, &r2][pass as usize - 1].store(true, Ordering::SeqCst);
    }));
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&live, b).unwrap();
    assert!(ran1.load(Ordering::SeqCst) && ran2.load(Ordering::SeqCst), "the hook ran at both passes");
    for pass in 1..=2u64 {
        assert_eq!(s.get(&s.hasher().id(&blob(20_000 + pass))), Some(blob(20_000 + pass)), "put at pass {pass}");
    }
    assert_eq!(s.get(&x).as_ref(), Some(&dx), "the plan of X's pack was dropped");
    for (i, (id, d)) in objs.iter().enumerate() {
        let kept = live.contains(id) || packs[i] == x_pack;
        assert_eq!(s.get(id).as_ref() == Some(d), kept, "object {i} in pack {}", packs[i]);
    }
    let in_x_pack = packs.iter().filter(|&&p| p == x_pack).count() as u64;
    let removed = objs.iter().zip(&packs).filter(|((id, _), &p)| !live.contains(id) && p != x_pack).count() as u64;
    assert_eq!(st.removed_objects, removed, "{st:?}");
    assert!(st.kept_objects >= in_x_pack, "{st:?}");
    let t = s.last_gc_timing();
    assert!(t.lock_hold_max <= t.wall && t.lock_hold_max <= t.lock_hold_total, "{t:?}");
    assert!(t.lock_hold_total > std::time::Duration::ZERO, "{t:?}");
}

// A run that the MANIFEST lists is gone: the open rebuilds the index from
// the packs (17-04), it does not fail, and every synced object is there.
#[test]
fn a_listed_but_missing_run_requests_a_rebuild() {
    let mem = MemIo::new();
    let (s, objs) = gc_store(&mem);
    drop(s);
    let runs = run_list(&mem);
    mem.remove(runs[3], Kind::Run).unwrap();
    let s = ChunkStore::open_with(Arc::new(mem.clone()), gc_opts()).expect("the open rebuilds, it does not fail");
    assert!(run_list(&mem).iter().all(|r| !runs.contains(r)), "the index was rebuilt");
    for (i, (id, d)) in objs.iter().enumerate() {
        assert_eq!(s.get(id).as_ref(), Some(d), "object {i}");
    }
    assert_eq!(s.stats().objects, 300);
}
