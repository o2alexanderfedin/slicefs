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
fn packs_of_an_earlier_process_are_cached_from_their_exact_idx() {
    let mem = MemIo::new();
    let s = store(&mem, true);
    let ids = fill(&s, 3 * PER_PACK);
    s.close().unwrap();
    drop(s);
    let s = store(&mem, true);
    for i in 0..3 * PER_PACK {
        assert_eq!(s.put(&data(i)).unwrap(), ids[i as usize]);
    }
    let st = s.locality_stats();
    assert!(st.fills >= 2 && st.hits * 10 >= 3 * PER_PACK * 6, "{st:?}");
    assert!(cache_matches_index(&s) > 0);
}

/// Every `.idx` that is marked exact, of a pack at or above the trust
/// floor, lists only current index entries (the rule of `locality.rs`).
fn exact_idx_files_match_the_index(mem: &MemIo, s: &ChunkStore) -> usize {
    let floor = crate::read_trust_floor(mem).unwrap_or(u64::MAX);
    let mut exact = 0;
    for num in mem.list().unwrap() {
        let Ok(b) = mem.read(num, Kind::Idx) else { continue };
        if num < floor || !crate::idx_exact(&b) {
            continue;
        }
        exact += 1;
        for (id, l) in crate::decode_index(&b, num).unwrap().unwrap() {
            assert_eq!(s.index.get(&id).unwrap(), Some(l), "the exact .idx of pack {num} lists an object that is not there");
        }
    }
    exact
}

/// Put every object of `0..n` again with the cache on, in pack order, then
/// check that each one is readable: a cache hit on an object that a GC
/// removed would skip its write and lose it.
fn rewrite_and_read_all(s: &ChunkStore, n: u64) {
    for i in 0..n {
        s.put(&data(i)).unwrap();
    }
    for i in 0..n {
        let id = s.opts.hasher.id(&data(i));
        assert_eq!(s.get(&id), Some(data(i)), "object {i} is lost");
    }
}

const SWEEP_PACKS: u64 = 6;

/// A store for the GC crash tests, and the IDs that the GC keeps: it
/// punches X out of the first pack, deletes the second pack (all dead) and
/// compacts the third (mostly dead, so its live objects are copied).
fn sweep_store(mem: &MemIo) -> (ChunkStore, HashSet<ObjectId>) {
    let s = store(mem, true);
    let n = SWEEP_PACKS * PER_PACK;
    let ids = fill(&s, n);
    let pack_of = |i: u64| s.index.get(&ids[i as usize]).unwrap().unwrap().pack;
    let (p1, p2, p3) = (pack_of(0), pack_of(PER_PACK + PER_PACK / 2), pack_of(2 * PER_PACK + PER_PACK / 2));
    assert!(p1 < p2 && p2 < p3, "{p1} {p2} {p3}");
    let mut kept_in_p3 = 0;
    let live: HashSet<ObjectId> = (0..n)
        .filter(|&i| {
            let p = pack_of(i);
            if i == XI || p == p2 {
                return false;
            }
            if p == p3 {
                kept_in_p3 += 1;
                return kept_in_p3 <= 3;
            }
            true
        })
        .map(|i| ids[i as usize])
        .collect();
    (s, live)
}

/// A power loss at each sync of a GC (pack syncs, directory syncs and
/// durable writes: the `.idx` clears, the pending runs, the MANIFEST, the
/// punch). After the reopen, no exact `.idx` lists a removed or moved
/// object, and a re-write of everything in pack order loses nothing.
#[test]
fn a_crash_at_each_step_of_a_gc_never_leaves_a_stale_exact_idx() {
    let n = SWEEP_PACKS * PER_PACK;
    let mut done = false;
    for k in 0..400 {
        let mem = MemIo::new();
        let (s, live) = sweep_store(&mem);
        s.sync().unwrap();
        let b = s.gc_begin().unwrap();
        mem.fail_sync_after(k);
        let r = s.gc_finish(&live, b);
        mem.fail_sync_after(u32::MAX);
        if let Ok(st) = &r {
            assert!(st.packs_deleted >= 1 && st.packs_compacted >= 1 && st.packs_punched >= 1, "{st:?}");
        }
        drop(s);
        mem.crash();
        let s = store(&mem, true);
        exact_idx_files_match_the_index(&mem, &s);
        rewrite_and_read_all(&s, n);
        exact_idx_files_match_the_index(&mem, &s);
        if r.is_ok() {
            // Past the last sync: the packs that the GC did not change are
            // still trusted, and so is the punched pack (its new `.idx`).
            let st = s.locality_stats();
            assert!(st.fills >= 2, "k={k}: {st:?}");
            done = true;
            break;
        }
    }
    assert!(done, "the GC never ended without a fault");
}

/// A GC whose pack removals fail leaves the deleted and compacted packs
/// with their `.idx` (it lists the removed objects); their `.idx` files are
/// no longer exact.
#[test]
fn a_gc_that_cannot_change_its_packs_leaves_no_exact_idx_of_them() {
    let n = SWEEP_PACKS * PER_PACK;
    let mem = MemIo::new();
    let (s, live) = sweep_store(&mem);
    let exact_before = exact_idx_files_match_the_index(&mem, &s);
    let b = s.gc_begin().unwrap();
    mem.set_fail_removes(true);
    assert!(s.gc_finish(&live, b).is_err());
    mem.set_fail_removes(false);
    drop(s);
    mem.crash();
    let s = store(&mem, true);
    let exact_after = exact_idx_files_match_the_index(&mem, &s);
    assert!(exact_after < exact_before, "{exact_after} of {exact_before}");
    rewrite_and_read_all(&s, n);
}

// The copies of a compaction are made before the install. A failed install
// (the MANIFEST write) then leaves the source pack with its records and the
// copies with newer entries: its `.idx` must not be exact, or a later GC
// that removes a copy would leave the old place in the cache of a later
// mount.
#[test]
fn a_failed_install_after_the_copies_leaves_the_source_pack_untrusted() {
    let n = SWEEP_PACKS * PER_PACK;
    let mem = MemIo::new();
    let (s, live) = sweep_store(&mem);
    let b = s.gc_begin().unwrap();
    mem.fail_next_write_durable(Kind::Manifest, 1);
    assert!(s.gc_finish(&live, b).is_err());
    drop(s);
    mem.crash();
    let s = store(&mem, true);
    exact_idx_files_match_the_index(&mem, &s);
    // A later GC removes everything that the first one did not keep.
    let b = s.gc_begin().unwrap();
    s.gc_finish(&live, b).unwrap();
    drop(s);
    let s = store(&mem, true);
    exact_idx_files_match_the_index(&mem, &s);
    rewrite_and_read_all(&s, n);
}

// The clear of the exact marks fails: the GC ends before any change.
#[test]
fn a_failed_clear_ends_the_gc_before_any_change() {
    let mem = MemIo::new();
    let (s, live) = sweep_store(&mem);
    let objects = s.stats().objects;
    let b = s.gc_begin().unwrap();
    mem.fail_write_durable_after(Kind::Idx, 1);
    assert!(s.gc_finish(&live, b).is_err());
    assert_eq!(s.stats().objects, objects, "nothing removed");
    exact_idx_files_match_the_index(&mem, &s);
    drop(s);
    mem.crash();
    let s = store(&mem, true);
    exact_idx_files_match_the_index(&mem, &s);
    rewrite_and_read_all(&s, SWEEP_PACKS * PER_PACK);
}

// A failed lookup writes X again; the trust floor that it moves is durable
// before that write. After a power loss and a reopen, a GC removes X (it
// selects only the pack of the new copy); the pack of the first copy is
// below the floor, so a streak in it does not cache X.
#[test]
fn the_trust_floor_of_a_failed_lookup_outlives_a_crash() {
    let mem = MemIo::new();
    let s = store(&mem, true);
    let ids = fill(&s, 3 * PER_PACK);
    let xi = XI;
    let x = ids[xi as usize];
    let p = s.index.get(&x).unwrap().unwrap().pack;
    mem.fail_read_once_after(0);
    assert_eq!(s.put(&data(xi)).unwrap(), x);
    mem.clear_fail_reads_after();
    assert_ne!(s.index.get(&x).unwrap().unwrap().pack, p, "the failed lookup wrote X again");
    assert!(crate::read_trust_floor(&mem).unwrap() > p);
    s.sync().unwrap();
    drop(s);
    mem.crash();
    let s = store(&mem, true);
    let live: HashSet<ObjectId> = ids.iter().copied().filter(|&i| i != x).collect();
    let b = s.gc_begin().unwrap();
    s.gc_finish(&live, b).unwrap();
    assert!(!s.contains(&x));
    exact_idx_files_match_the_index(&mem, &s);
    drop(s);
    let s = store(&mem, true);
    for i in 0..3 * PER_PACK {
        if i != xi {
            s.put(&data(i)).unwrap();
        }
    }
    assert_eq!(s.put(&data(xi)).unwrap(), x);
    assert_eq!(s.get(&x), Some(data(xi)), "X is readable");
}

// The trust floor cannot be written: the put fails and writes nothing.
#[test]
fn a_put_whose_trust_floor_fails_writes_nothing() {
    let mem = MemIo::new();
    let s = store(&mem, true);
    let ids = fill(&s, 3 * PER_PACK);
    let x = ids[XI as usize];
    let at = s.index.get(&x).unwrap();
    let objects = s.stats().objects;
    mem.fail_read_once_after(0);
    mem.fail_next_write_durable(Kind::Trust, 1);
    assert!(s.put(&data(XI)).is_err());
    mem.clear_fail_reads_after();
    assert_eq!(s.index.get(&x).unwrap(), at, "no second copy");
    assert_eq!(s.stats().objects, objects);
}

// A failed index lookup makes put write X again (into the active pack), so
// the pack P of the first copy still lists X in its `.idx`. A GC that
// removes X then selects only the pack of the new copy. If P stayed
// trusted, a streak in P would cache X, and a put of X would skip the
// write and lose X.
#[test]
fn a_failed_lookup_ends_the_trust_in_the_packs_so_far() {
    let mem = MemIo::new();
    let s = store(&mem, true);
    let ids = fill(&s, 3 * PER_PACK);
    let xi = XI;
    let x = ids[xi as usize];
    let p = s.index.get(&x).unwrap().unwrap().pack;
    mem.fail_read_once_after(0);
    assert_eq!(s.put(&data(xi)).unwrap(), x);
    mem.clear_fail_reads_after();
    let p2 = s.index.get(&x).unwrap().unwrap().pack;
    assert_ne!(p, p2, "the failed lookup wrote X again");
    let live: HashSet<ObjectId> = ids.iter().copied().filter(|&i| i != x).collect();
    let b = s.gc_begin().unwrap();
    s.gc_finish(&live, b).unwrap();
    assert!(!s.contains(&x));
    let in_p: Vec<u64> =
        (0..3 * PER_PACK).filter(|&i| i != xi && s.index.get(&ids[i as usize]).unwrap().unwrap().pack == p).collect();
    assert!(in_p.len() > 2 * FILL_STREAK as usize);
    for &i in &in_p {
        s.put(&data(i)).unwrap();
    }
    assert_eq!(s.locality_stats().fills, 0);
    assert_eq!(s.put(&data(xi)).unwrap(), x);
    assert_eq!(s.get(&x), Some(data(xi)), "X is readable");
}

// Objects that are still in the active pack (no `.idx` yet) are written
// again: no fill tries to read a `.idx` that does not exist.
#[test]
fn a_rewrite_in_the_active_pack_tries_no_fill() {
    let mem = MemIo::new();
    let s = store(&mem, true);
    let n = PER_PACK / 2;
    for _ in 0..2 {
        for i in 0..n {
            s.put(&data(i)).unwrap();
        }
    }
    let st = s.locality_stats();
    assert_eq!((st.fills, st.fills_dropped), (0, 0), "{st:?}");
    assert_eq!(mem.reads(Kind::Idx), 0);
}

// The open cannot read the `TRUST` file (here: damaged). It trusts no pack,
// and the next failed lookup still writes a new floor durably (a floor
// that is not known on disk is never taken as high enough).
#[test]
fn an_unreadable_trust_floor_is_written_again_at_the_next_failed_lookup() {
    let mem = MemIo::new();
    let s = store(&mem, true);
    let ids = fill(&s, 3 * PER_PACK);
    mem.fail_read_once_after(0);
    s.put(&data(XI)).unwrap();
    mem.clear_fail_reads_after();
    s.close().unwrap();
    drop(s);
    mem.flip(0, Kind::Trust, 5);
    assert_eq!(crate::read_trust_floor(&mem), None);
    let s = store(&mem, true);
    for i in 0..3 * PER_PACK {
        s.put(&data(i)).unwrap();
    }
    assert_eq!(s.locality_stats().fills, 0, "no pack is trusted");
    let y = ids[(2 * PER_PACK + 3) as usize];
    s.gc_begin().unwrap(); // seal: the put of Y makes a new pack
    s.gc_abort();
    let next = s.inner.lock().unwrap().next_pack;
    mem.fail_read_once_after(0);
    assert_eq!(s.put(&data(2 * PER_PACK + 3)).unwrap(), y);
    mem.clear_fail_reads_after();
    assert_eq!(crate::read_trust_floor(&mem), Some(next), "the floor is on disk again");
}
