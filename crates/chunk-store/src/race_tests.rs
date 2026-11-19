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
