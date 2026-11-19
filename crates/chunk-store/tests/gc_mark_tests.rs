//! GC with a filter mark (#113): a Bloom filter of the live IDs instead of a
//! set of them. A false positive only keeps a dead object; no live object
//! and no object touched during the GC is ever removed.

use chunk_store::{ChunkStore, LiveFilter, LiveSet, ObjectId, Options};
use std::collections::HashSet;
use tempfile::TempDir;

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

fn opts() -> Options {
    Options { pack_limit: 64 * 1024, ..Options::default() }
}

/// `n` objects of 2,000 bytes; one in `k` is live.
fn fill(s: &ChunkStore, n: u64, k: usize) -> (Vec<(ObjectId, Vec<u8>)>, HashSet<ObjectId>) {
    let objs: Vec<_> = (0..n).map(|i| { let d = random(2000, 7 + i); (s.put(&d).unwrap(), d) }).collect();
    let live = objs.iter().step_by(k).map(|(id, _)| *id).collect();
    (objs, live)
}

fn filter_of(live: &HashSet<ObjectId>, n: u64, bits: u8) -> LiveFilter {
    let mut f = LiveFilter::with_bits_per_key(n, bits);
    for id in live {
        f.insert(id);
    }
    f
}

/// Every live object is readable, now and after a reopen; returns the dead
/// objects that are still there.
fn check_live(dir: &std::path::Path, s: ChunkStore, objs: &[(ObjectId, Vec<u8>)], live: &HashSet<ObjectId>) -> usize {
    for (id, d) in objs.iter().filter(|(id, _)| live.contains(id)) {
        assert_eq!(s.get(id).as_deref(), Some(d.as_slice()), "live object {id:?} lost");
    }
    drop(s);
    let s = ChunkStore::open(dir, opts()).unwrap();
    for (id, d) in objs.iter().filter(|(id, _)| live.contains(id)) {
        assert_eq!(s.get(id).as_deref(), Some(d.as_slice()), "live object {id:?} lost after reopen");
    }
    objs.iter().filter(|(id, _)| !live.contains(id) && s.contains(id)).count()
}

#[test]
fn a_filter_mark_keeps_every_live_object_and_removes_most_dead_ones() {
    let dir = TempDir::new().unwrap();
    let s = ChunkStore::open(dir.path(), opts()).unwrap();
    let (objs, live) = fill(&s, 400, 4);
    let mut f = s.live_filter();
    for id in &live {
        f.insert(id);
    }
    assert!(live.iter().all(|id| f.may_contain(id)));
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&f, b).unwrap();
    assert!(st.removed_objects >= 250, "{st:?}");
    let left = check_live(dir.path(), s, &objs, &live);
    assert_eq!(left as u64, 300 - st.removed_objects);
}

#[test]
fn a_saturated_filter_keeps_every_live_object() {
    // One 512-bit block at 1 bit per key for 100 keys: about one dead
    // object in five is a false positive, also with every bit set.
    for (n, bits) in [(1u64, 1u8), (10, 2), (0, 1)] {
        let dir = TempDir::new().unwrap();
        let s = ChunkStore::open(dir.path(), opts()).unwrap();
        let (objs, live) = fill(&s, 400, 4);
        let f = filter_of(&live, n, bits);
        let b = s.gc_begin().unwrap();
        let st = s.gc_finish(&f, b).unwrap();
        let left = check_live(dir.path(), s, &objs, &live);
        assert_eq!(left as u64, 300 - st.removed_objects, "n {n} bits {bits}");
        assert!(left > 0, "n {n} bits {bits}: the filter gave no false positive");
    }
}

#[test]
fn an_empty_filter_removes_everything() {
    let dir = TempDir::new().unwrap();
    let s = ChunkStore::open(dir.path(), opts()).unwrap();
    let (objs, _) = fill(&s, 100, 4);
    let f = s.live_filter();
    let b = s.gc_begin().unwrap();
    assert_eq!(s.gc_finish(&f, b).unwrap().removed_objects, 100);
    assert!(objs.iter().all(|(id, _)| !s.contains(id)));
}

#[test]
fn a_filter_mark_removes_a_subset_of_what_an_exact_mark_removes() {
    let run = |exact: bool, bits: u8| -> HashSet<ObjectId> {
        let dir = TempDir::new().unwrap();
        let s = ChunkStore::open(dir.path(), opts()).unwrap();
        let (objs, live) = fill(&s, 400, 3);
        let b = s.gc_begin().unwrap();
        if exact {
            s.gc_finish(&live, b).unwrap();
        } else {
            s.gc_finish(&filter_of(&live, 134, bits), b).unwrap();
        }
        objs.iter().map(|(id, _)| *id).filter(|id| !s.contains(id)).collect()
    };
    let exact = run(true, 0);
    for bits in [1, 4, 10] {
        let filtered = run(false, bits);
        assert!(filtered.is_subset(&exact), "bits {bits}: the filter mark removed an object the exact mark keeps");
    }
}

#[test]
fn objects_read_before_gc_finish_survive_when_the_touched_set_spills_to_a_filter() {
    // At most 4 exact touched IDs: the 50 reads below spill into a filter.
    let o = Options { gc_touched_exact: 4, ..opts() };
    let dir = TempDir::new().unwrap();
    let s = ChunkStore::open(dir.path(), o).unwrap();
    let (objs, _) = fill(&s, 200, 4);
    let b = s.gc_begin().unwrap();
    // The mark found nothing live, but a reader took these objects.
    let read: HashSet<ObjectId> = objs.iter().take(50).map(|(id, _)| *id).collect();
    for id in &read {
        s.read(id).unwrap();
    }
    let st = s.gc_finish(&s.live_filter(), b).unwrap();
    assert!(st.removed_objects >= 100, "{st:?}");
    for (id, d) in objs.iter().filter(|(id, _)| read.contains(id)) {
        assert_eq!(s.get(id).as_deref(), Some(d.as_slice()), "touched object {id:?} lost");
    }
}

/// The live objects are all there; returns the dead objects still there.
fn dead_left(s: &ChunkStore, objs: &[(ObjectId, Vec<u8>)], live: &HashSet<ObjectId>) -> HashSet<ObjectId> {
    for (id, d) in objs.iter().filter(|(id, _)| live.contains(id)) {
        assert_eq!(s.get(id).as_deref(), Some(d.as_slice()), "live object {id:?} lost");
    }
    objs.iter().map(|(id, _)| *id).filter(|id| !live.contains(id) && s.contains(id)).collect()
}

fn filter_of_seeded(live: &HashSet<ObjectId>, bits: u8, seed: u64) -> LiveFilter {
    let mut f = LiveFilter::with_seed(live.len() as u64, bits, seed);
    for id in live {
        f.insert(id);
    }
    f
}

#[test]
fn a_false_positive_of_one_gc_is_removed_by_a_later_gc_with_another_salt() {
    let dir = TempDir::new().unwrap();
    let s = ChunkStore::open(dir.path(), opts()).unwrap();
    // Records of 9,000 bytes: each one covers a whole 4 KiB block, so a
    // pack that is punched (most of it live after the first GC) frees
    // every dead record, and only a false positive keeps one.
    let objs: Vec<_> = (0..200).map(|i| { let d = random(9000, 7 + i); (s.put(&d).unwrap(), d) }).collect();
    let live: HashSet<ObjectId> = objs.iter().step_by(4).map(|(id, _)| *id).collect();
    // One 512-bit block, one probe, 50 keys: about 9 % false positives.
    let gc = |seed: u64| {
        let f = filter_of_seeded(&live, 2, seed);
        let b = s.gc_begin().unwrap();
        s.gc_finish(&f, b).unwrap()
    };
    gc(1);
    let first = dead_left(&s, &objs, &live);
    assert!(!first.is_empty(), "the first GC gave no false positive");
    // The same salt again: the same false positives, nothing removed.
    assert_eq!(gc(1).removed_objects, 0);
    assert_eq!(dead_left(&s, &objs, &live), first);
    // Other salts: after a few GCs no dead object is left.
    let mut seed = 2;
    while !dead_left(&s, &objs, &live).is_empty() {
        assert!(seed < 20, "dead objects stay after {seed} salts: {:?}", dead_left(&s, &objs, &live));
        gc(seed);
        seed += 1;
    }
    drop(s);
    let s = ChunkStore::open(dir.path(), opts()).unwrap();
    assert!(dead_left(&s, &objs, &live).is_empty());
    assert_eq!(s.stats().objects, live.len() as u64);
}
