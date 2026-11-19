//! The locality cache of `put` (#112) in a store: hits give the same places
//! as the index, and a GC never lets the cache answer for a removed object
//! (also when the GC leaves the `.idx` of a pack stale).

use crate::locality::FILL_STREAK;
use crate::mem::MemIo;
use crate::pack_io::{Kind, PackIo};
use crate::{ChunkStore, ObjectId, Options};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Records per pack (about): the packs are far more than one.
const PER_PACK: u64 = 40;
const REC: usize = 300;
/// The object that the GC tests remove: in the first pack, after a streak
/// of other objects of that pack, and big enough to span a whole punch
/// block, so a GC punches it out of a mostly live pack that stays.
const XI: u64 = 10;
const X_LEN: usize = 5000;

fn opts(cache: bool) -> Options {
    Options {
        pack_limit: PER_PACK * (REC as u64 + 60),
        zstd_level: 0,
        index_memtable_limit: 1,
        index_background_merge: false,
        locality_cache: cache,
        ..Options::default()
    }
}

fn store(mem: &MemIo, cache: bool) -> ChunkStore {
    ChunkStore::open_with(Arc::new(mem.clone()), opts(cache)).unwrap()
}

/// Incompressible bytes, different for each `i`.
fn data(i: u64) -> Vec<u8> {
    let len = if i == XI { X_LEN } else { REC };
    let mut out = Vec::with_capacity(len);
    let mut c = 0u64;
    while out.len() < len {
        out.extend_from_slice(blake3::hash(&[i.to_le_bytes(), c.to_le_bytes()].concat()).as_bytes());
        c += 1;
    }
    out.truncate(len);
    out
}

fn fill(s: &ChunkStore, n: u64) -> Vec<ObjectId> {
    let ids = (0..n).map(|i| s.put(&data(i)).unwrap()).collect();
    // Seal the last pack (a GC begin seals; nothing is removed).
    s.gc_begin().unwrap();
    s.gc_abort();
    ids
}

/// Every cached entry is the current entry of the index.
fn cache_matches_index(s: &ChunkStore) -> usize {
    let e = s.locality.entries();
    for (id, l) in &e {
        assert_eq!(s.index.get(id).unwrap(), Some(*l), "a cached place differs from the index");
    }
    e.len()
}

#[test]
fn a_rewrite_in_pack_order_hits_the_cache_with_the_places_of_the_index() {
    let mem = MemIo::new();
    let s = store(&mem, true);
    let n = 10 * PER_PACK;
    let ids = fill(&s, n);
    assert!(s.stats().packs >= 8, "{:?}", s.stats());
    let objects = s.stats().objects;
    mem.reset_read_counters();
    for i in 0..n {
        assert_eq!(s.put(&data(i)).unwrap(), ids[i as usize]);
    }
    let run_reads_on = mem.reads(Kind::Run);
    let st = s.locality_stats();
    assert_eq!(s.stats().objects, objects, "a dedup hit writes nothing");
    assert!(st.fills >= s.stats().packs - 1, "{st:?}");
    assert!(st.hits * 10 >= n * 6, "most re-puts hit the cache: {st:?}");
    assert!(cache_matches_index(&s) > 0);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(s.get(id), Some(data(i as u64)));
    }

    // The same re-write without the cache reads a run page per put.
    drop(s);
    let mem2 = MemIo::new();
    let s = store(&mem2, false);
    fill(&s, n);
    mem2.reset_read_counters();
    for i in 0..n {
        s.put(&data(i)).unwrap();
    }
    let run_reads_off = mem2.reads(Kind::Run);
    assert_eq!(s.locality_stats().hits, 0);
    assert!(run_reads_on * 2 < run_reads_off, "run page reads: {run_reads_on} with the cache, {run_reads_off} without");
}

#[test]
fn hits_in_random_order_never_fill() {
    let mem = MemIo::new();
    let s = store(&mem, true);
    let n = 10 * PER_PACK;
    fill(&s, n);
    // Every put in another pack than the one before: no streak.
    for k in 0..n {
        let i = (k * PER_PACK + k / 10) % n;
        s.put(&data(i)).unwrap();
    }
    let st = s.locality_stats();
    assert_eq!((st.fills, st.hits), (0, 0), "{st:?}");
    assert_eq!(mem.reads(Kind::Idx), 0, "no .idx read");
}

/// GC removes X from a mostly live pack with small records: no whole block
/// to punch, so the `.idx` of the pack still lists X.
fn gc_remove(s: &ChunkStore, ids: &[ObjectId], x: ObjectId) {
    let live: HashSet<ObjectId> = ids.iter().copied().filter(|&i| i != x).collect();
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&live, b).unwrap();
    assert_eq!((st.removed_objects, st.packs_punched), (1, 1), "{st:?}");
    assert!(!s.contains(&x));
}

fn x_is_written_again(s: &ChunkStore, x: ObjectId, dx: &[u8], objects: u64) {
    assert_eq!(s.put(dx).unwrap(), x);
    assert_eq!(s.get(&x), Some(dx.to_vec()), "X is readable");
    assert_eq!(s.stats().objects, objects, "X counts again");
}

#[test]
fn a_gc_drops_the_cached_pack_of_a_removed_object() {
    let mem = MemIo::new();
    let s = store(&mem, true);
    let ids = fill(&s, 3 * PER_PACK);
    let xi = XI;
    let x = ids[xi as usize];
    for i in 0..PER_PACK {
        s.put(&data(i)).unwrap();
    }
    assert!(s.locality.entries().iter().any(|(id, _)| *id == x), "X is cached");
    let objects = s.stats().objects;
    gc_remove(&s, &ids, x);
    assert!(!s.locality.entries().iter().any(|(id, _)| *id == x), "the GC dropped X from the cache");
    x_is_written_again(&s, x, &data(xi), objects);
}

// A GC can leave the `.idx` of a pack that it changed as it was (a skipped
// punch, a failed removal, a crash before the change): it then lists X,
// which the index no longer has. The test puts the old `.idx` back to make
// that state. A fill from it would cache X, and a put of X would skip the
// write and lose X.
#[test]
fn a_pack_that_a_gc_changed_is_never_filled_from_its_stale_idx() {
    let mem = MemIo::new();
    let s = store(&mem, true);
    let ids = fill(&s, 3 * PER_PACK);
    let xi = XI;
    let x = ids[xi as usize];
    let pack = s.index.get(&x).unwrap().unwrap().pack;
    let old_idx = mem.read(pack, Kind::Idx).unwrap();
    let objects = s.stats().objects;
    gc_remove(&s, &ids, x);
    mem.write_durable(pack, Kind::Idx, &old_idx).unwrap();
    let idx = crate::decode_index(&mem.read(pack, Kind::Idx).unwrap(), pack).unwrap().unwrap();
    assert!(idx.iter().any(|(id, _)| *id == x), "the .idx is stale");
    // A streak of hits in that pack, then X.
    let in_pack: Vec<u64> =
        (0..3 * PER_PACK).filter(|&i| i != xi && s.index.get(&ids[i as usize]).unwrap().unwrap().pack == pack).collect();
    assert!(in_pack.len() > 2 * FILL_STREAK as usize);
    for &i in &in_pack {
        s.put(&data(i)).unwrap();
    }
    assert_eq!(s.locality_stats().fills, 0);
    x_is_written_again(&s, x, &data(xi), objects);
    s.sync().unwrap();
    drop(s);
    mem.crash();
    let s = store(&mem, true);
    assert_eq!(s.get(&x), Some(data(xi)), "after a crash and a reopen");
}

// A put finds X in the cache. Before it takes the store lock, a GC removes
// X (the test hook runs it). The put must see that under the lock and write
// X again.
#[test]
fn a_cache_hit_that_gc_removed_before_the_lock_writes_again() {
    let mem = MemIo::new();
    let s = store(&mem, true);
    let ids = fill(&s, 3 * PER_PACK);
    let xi = XI;
    let x = ids[xi as usize];
    for i in 0..xi {
        s.put(&data(i)).unwrap();
    }
    let hits = s.locality_stats().hits;
    let objects = s.stats().objects;
    let ran = Arc::new(AtomicBool::new(false));
    {
        let ran = Arc::clone(&ran);
        let ids = ids.clone();
        *s.after_lookup.lock().unwrap() = Some(Box::new(move |s: &ChunkStore| {
            gc_remove(s, &ids, x);
            ran.store(true, Ordering::SeqCst);
        }));
    }
    x_is_written_again(&s, x, &data(xi), objects);
    assert!(ran.load(Ordering::SeqCst));
    assert_eq!(s.locality_stats().hits, hits + 1, "the lookup of X was a cache hit");
}

// A cache hit during a GC marks the object touched, as an index hit does:
// the GC keeps it although its mark does not have it.
#[test]
fn a_cache_hit_during_a_gc_keeps_the_object() {
    let mem = MemIo::new();
    let s = store(&mem, true);
    let ids = fill(&s, 3 * PER_PACK);
    let xi = XI;
    let x = ids[xi as usize];
    for i in 0..xi {
        s.put(&data(i)).unwrap();
    }
    let b = s.gc_begin().unwrap();
    let hits = s.locality_stats().hits;
    s.put(&data(xi)).unwrap();
    assert_eq!(s.locality_stats().hits, hits + 1, "the put of X was a cache hit");
    let live: HashSet<ObjectId> = ids.iter().copied().filter(|&i| i != x).collect();
    let st = s.gc_finish(&live, b).unwrap();
    assert_eq!(st.removed_objects, 0, "{st:?}");
    assert_eq!(s.get(&x), Some(data(xi)));
}

#[test]
fn packs_of_an_earlier_process_are_never_cached() {
    let mem = MemIo::new();
    let s = store(&mem, true);
    fill(&s, 3 * PER_PACK);
    s.close().unwrap();
    drop(s);
    let s = store(&mem, true);
    for i in 0..3 * PER_PACK {
        s.put(&data(i)).unwrap();
    }
    let st = s.locality_stats();
    assert_eq!((st.fills, st.hits), (0, 0), "{st:?}");
    assert!(st.misses >= 3 * PER_PACK - u64::from(FILL_STREAK));
}
