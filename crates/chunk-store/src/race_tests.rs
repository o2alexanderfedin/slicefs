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
    let o = Options { index_memtable_limit: 1, ..Options::default() };
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
