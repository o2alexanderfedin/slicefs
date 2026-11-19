//! The on-disk index (#85): objects are found through run files after a
//! reopen, GC removals are durable tombstones, and a GC that cannot read the
//! index changes nothing. In-process on MemIo; no mounts.

use chunk_store::mem::MemIo;
use chunk_store::pack_io::{Kind, PackIo};
use chunk_store::retry::RetryingIo;
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

/// Object `i` of the index benchmark: the 8 bytes of `i`.
fn small(i: u64) -> Vec<u8> {
    i.to_le_bytes().to_vec()
}

fn opts(memtable_limit: usize, pack_limit: u64) -> Options {
    Options { pack_limit, index_memtable_limit: memtable_limit, ..Options::default() }
}

fn open(mem: &MemIo, o: &Options) -> ChunkStore {
    ChunkStore::open_with(Arc::new(mem.clone()), o.clone()).unwrap()
}

/// Pack numbers and lengths, sorted.
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

fn put_random(s: &ChunkStore, n: u64, seed: u64) -> Vec<(ObjectId, Vec<u8>)> {
    (0..n).map(|i| {
        let d = random(1000, seed + i);
        (s.put(&d).unwrap(), d)
    })
    .collect()
}

/// Put 2,000 small objects, close, reopen: every object is found through
/// the runs; then again without the `.idx` files of the covered packs.
fn found_through_runs(mem: &MemIo, io: &dyn Fn() -> Arc<dyn PackIo>) {
    let o = opts(64, 64 << 10);
    let s = ChunkStore::open_with(io(), o.clone()).unwrap();
    let ids: Vec<ObjectId> = (0..2000).map(|i| s.put(&small(i)).unwrap()).collect();
    let never = s.hasher().id(b"never put");
    s.close().unwrap();
    drop(s);

    let check = |s: &ChunkStore| {
        let info = s.index_info();
        assert!(info.runs >= 1, "{info:?}");
        assert!(info.hwm_pack > 1, "{info:?}");
        // Only the records above the hwm are replayed.
        assert!(info.memtable_entries < 64, "{info:?}");
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(s.get(id), Some(small(i as u64)), "object {i}");
        }
        assert_eq!(s.stats().objects, 2000);
        assert!(!s.contains(&never));
        assert!(s.get(&never).is_none());
    };
    let s = ChunkStore::open_with(io(), o.clone()).unwrap();
    check(&s);
    let info = s.index_info();
    drop(s);

    let covered: Vec<u64> = mem.list().unwrap().into_iter().filter(|&n| n < info.hwm_pack).collect();
    assert!(!covered.is_empty());
    for &num in &covered {
        mem.remove(num, Kind::Idx).unwrap();
    }
    let s = ChunkStore::open_with(io(), o).unwrap();
    check(&s);
    assert_eq!(s.index_info(), info, "the same runs, hwm and memtable");
    for &num in &covered {
        // A writable open writes the `.idx` of a sealed pack that it scans:
        // none here, because the covered packs are not read at all.
        assert!(!mem.exists(num, Kind::Idx), "pack {num} was read at open");
    }
}

#[test]
fn objects_are_found_through_runs_after_a_reopen() {
    let mem = MemIo::new();
    let m = mem.clone();
    found_through_runs(&mem, &move || Arc::new(m.clone()) as Arc<dyn PackIo>);
}

#[test]
fn the_same_flow_works_through_retrying_io() {
    let mem = MemIo::new();
    let m = mem.clone();
    found_through_runs(&mem, &move || {
        Arc::new(RetryingIo::with_backoff(Arc::new(m.clone()), Vec::new())) as Arc<dyn PackIo>
    });
}

#[test]
fn a_gc_removal_is_a_durable_tombstone() {
    let mem = MemIo::new();
    let o = opts(1000, 64 << 10);
    let s = open(&mem, &o);
    let objs = put_random(&s, 300, 1);
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&HashSet::new(), b).unwrap();
    assert_eq!(st.removed_objects, 300);
    assert_eq!(s.stats().objects, 0);
    assert!(s.index_info().runs >= 1, "the tombstones are in a run");

    let gone = |s: &ChunkStore, skip: usize, what: &str| {
        for (i, (id, _)) in objs.iter().enumerate().filter(|(i, _)| *i != skip) {
            assert!(!s.contains(id), "{what}: object {i}");
            assert!(s.get(id).is_none(), "{what}: object {i}");
            assert!(s.stored_len(id).is_none(), "{what}: object {i}");
        }
    };
    gone(&s, usize::MAX, "after the GC");

    // A put of a removed object stores it again (no stale dedup hit).
    let (id0, d0) = &objs[0];
    assert_eq!(s.put(d0).unwrap(), *id0);
    assert_eq!(s.get(id0).as_ref(), Some(d0));
    assert_eq!(s.stats().objects, 1);
    gone(&s, 0, "after the new put");
    drop(s);

    let s = open(&mem, &o);
    assert_eq!(s.get(id0).as_ref(), Some(d0));
    assert_eq!(s.stats().objects, 1);
    gone(&s, 0, "after a reopen");
}

#[test]
fn a_gc_that_cannot_read_the_index_changes_nothing() {
    let mem = MemIo::new();
    let o = opts(30, 64 << 10);
    let s = open(&mem, &o);
    let objs = put_random(&s, 300, 1000);
    s.sync().unwrap();
    // Ten flushes: every object is in a run, so the GC must read the runs.
    let info = s.index_info();
    assert_eq!((info.runs, info.memtable_entries), (10, 0), "{info:?}");
    let packs = pack_lens(&mem);
    assert!(packs.len() > 1);
    let manifest = mem.read(0, Kind::Manifest).unwrap();
    let runs = run_list(&mem);

    let b = s.gc_begin().unwrap();
    mem.fail_reads_after(3);
    let r = s.gc_finish(&HashSet::new(), b);
    mem.clear_fail_reads_after();
    assert!(r.is_err(), "the index read fails, so the GC fails");

    assert_eq!(pack_lens(&mem), packs, "no pack is deleted, punched or cut");
    assert_eq!(mem.read(0, Kind::Manifest).unwrap(), manifest, "no tombstone is durable");
    assert_eq!(run_list(&mem), runs);
    for (i, (id, _)) in objs.iter().enumerate() {
        assert!(s.contains(id), "object {i}");
    }
    assert_eq!(s.stats().objects, 300);
    // A dedup hit: nothing was removed, so nothing is written.
    let (id5, d5) = &objs[5];
    assert_eq!(s.put(d5).unwrap(), *id5);
    assert_eq!(s.get(id5).as_ref(), Some(d5));
    assert_eq!(pack_lens(&mem), packs);
    // The store is not failed.
    let new = random(1000, 99_999);
    let nid = s.put(&new).unwrap();
    assert_eq!(s.get(&nid), Some(new.clone()));
    drop(s);

    let s = open(&mem, &o);
    assert_eq!(s.stats().objects, 301);
    for (i, (id, d)) in objs.iter().enumerate() {
        assert_eq!(s.get(id).as_ref(), Some(d), "object {i} after a reopen");
    }
    assert_eq!(s.get(&nid), Some(new));
}

// ── Replay above the high-water mark and exact stats ─────────────────────────

#[test]
fn a_failed_compaction_is_counted_once_after_a_reopen() {
    let mem = MemIo::new();
    let o = opts(30, 64 << 10);
    let s = open(&mem, &o);
    let objs = put_random(&s, 300, 2000);
    s.sync().unwrap();
    assert_eq!(s.index_info().runs, 10);
    let live: HashSet<ObjectId> = objs.iter().step_by(4).map(|(id, _)| *id).collect();
    let b = s.gc_begin().unwrap();
    // The index pass reads ten pages; then five record copies work and the
    // sixth fails. The copies sit above the hwm, their originals in runs.
    mem.fail_reads_after(15);
    let r = s.gc_finish(&live, b);
    mem.clear_fail_reads_after();
    assert!(r.is_err());
    assert!(s.index_info().memtable_entries > 0, "the copies are in the memtable");
    s.sync().unwrap();
    assert_eq!(s.stats().objects, 300);
    drop(s);

    let s = open(&mem, &o);
    assert_eq!(s.stats().objects, 300, "a copy above the hwm is not counted again");
    for (id, d) in &objs {
        assert_eq!(s.get(id).as_ref(), Some(d));
    }
}

#[test]
fn a_flush_inside_the_active_pack_is_replayed_from_its_offset() {
    let mem = MemIo::new();
    let o = opts(50, 64 << 20);
    let s = open(&mem, &o);
    let ids: Vec<ObjectId> = (0..120).map(|i| s.put(&small(i)).unwrap()).collect();
    s.sync().unwrap();
    drop(s); // no close: the pack stays active, without its `.idx`

    let s = open(&mem, &o);
    let info = s.index_info();
    assert_eq!(info.hwm_pack, 1, "{info:?}");
    assert!(info.hwm_offset > 14, "{info:?}");
    assert_eq!(info.runs, 2, "{info:?}");
    assert_eq!(info.memtable_entries, 20, "only the records above the offset are replayed");
    assert_eq!(s.stats().objects, 120);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(s.get(id), Some(small(i as u64)), "object {i}");
    }
    // The active pack goes on; a later reopen is still exact.
    let more: Vec<ObjectId> = (120..150).map(|i| s.put(&small(i)).unwrap()).collect();
    s.sync().unwrap();
    drop(s);
    let s = open(&mem, &o);
    assert_eq!(s.stats().objects, 150);
    for (i, id) in ids.iter().chain(&more).enumerate() {
        assert_eq!(s.get(id), Some(small(i as u64)), "object {i}");
    }
}

#[test]
fn stats_are_the_same_after_a_reopen() {
    let mem = MemIo::new();
    let o = opts(40, 64 << 10);
    let s = open(&mem, &o);
    let objs = put_random(&s, 200, 1);
    assert!(s.index_info().runs >= 1);
    // A GC that compacts: one object in four is live.
    let live: HashSet<ObjectId> = objs.iter().step_by(4).map(|(id, _)| *id).collect();
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&live, b).unwrap();
    assert!(st.packs_compacted > 0, "{st:?}");
    assert_eq!(st.removed_objects, 150);
    // More puts: new objects, a dedup hit, and a removed object again.
    let more = put_random(&s, 30, 5000);
    assert_eq!(s.put(&objs[0].1).unwrap(), objs[0].0);
    assert_eq!(s.put(&objs[1].1).unwrap(), objs[1].0);
    s.sync().unwrap();
    let before = s.stats();
    assert_eq!(before.objects, 50 + 30 + 1);
    drop(s);

    let s = open(&mem, &o);
    assert_eq!(s.stats(), before);
    for (id, d) in more.iter().chain(objs.iter().filter(|(id, _)| live.contains(id))).chain(&objs[1..2]) {
        assert_eq!(s.get(id).as_ref(), Some(d));
    }
    // And after a close (seal) and one more reopen.
    s.close().unwrap();
    drop(s);
    assert_eq!(open(&mem, &o).stats(), before);
}

#[test]
fn a_read_only_open_writes_nothing() {
    let mem = MemIo::new();
    let o = opts(100, 64 << 10);
    let s = open(&mem, &o);
    let mut objs = put_random(&s, 200, 7);
    // Records above the hwm, not in a run: a sealed pack and the active pack.
    objs.extend(put_random(&s, 80, 9000));
    s.sync().unwrap();
    let info = s.index_info();
    assert_eq!(info.memtable_entries, 80);
    drop(s); // no close

    let packs = pack_lens(&mem);
    let idx: Vec<bool> = packs.iter().map(|&(n, _)| mem.exists(n, Kind::Idx)).collect();
    assert!(idx.contains(&false), "the active pack has no .idx");
    assert!(
        packs.iter().zip(&idx).any(|(&(n, _), &has)| n >= info.hwm_pack && has),
        "a sealed pack above the hwm"
    );
    let manifest = mem.read(0, Kind::Manifest).unwrap();
    let runs = run_list(&mem);

    // A limit of 1: a writable open would write a run after each pack.
    let ro = Options { read_only: true, index_memtable_limit: 1, ..o.clone() };
    let s = ChunkStore::open_with(Arc::new(mem.clone()), ro).unwrap();
    assert_eq!(s.index_info().memtable_entries, 80);
    assert_eq!(s.stats().objects, 280);
    for (i, (id, d)) in objs.iter().enumerate() {
        assert_eq!(s.get(id).as_ref(), Some(d), "object {i}");
    }
    assert!(s.put(b"no").is_err());
    drop(s);

    assert_eq!(pack_lens(&mem), packs);
    assert_eq!(packs.iter().map(|&(n, _)| mem.exists(n, Kind::Idx)).collect::<Vec<_>>(), idx);
    assert_eq!(mem.read(0, Kind::Manifest).unwrap(), manifest);
    assert_eq!(run_list(&mem), runs);
}

#[test]
fn a_store_without_a_manifest_is_indexed_at_its_first_writable_open() {
    let mem = MemIo::new();
    let s = open(&mem, &opts(1 << 18, 64 << 10));
    let objs = put_random(&s, 500, 3);
    s.close().unwrap();
    drop(s);
    assert!(!mem.exists(0, Kind::Manifest), "no flush happened");
    assert!(run_list(&mem).is_empty());
    assert!(pack_lens(&mem).len() > 2);

    let o = opts(64, 64 << 10);
    let s = open(&mem, &o);
    let info = s.index_info();
    assert!(info.runs >= 1, "{info:?}");
    assert_eq!(info.hwm_offset, 0, "replay flushes only between packs");
    assert!(info.memtable_entries < 64 + 70, "at most one pack over the limit: {info:?}");
    assert_eq!(s.stats().objects, 500);
    for (i, (id, d)) in objs.iter().enumerate() {
        assert_eq!(s.get(id).as_ref(), Some(d), "object {i}");
    }
    drop(s);

    // The next open replays only above the hwm and still counts exactly.
    let s = open(&mem, &o);
    assert_eq!(s.index_info().runs, info.runs);
    assert_eq!(s.stats().objects, 500);
    for (id, d) in &objs {
        assert_eq!(s.get(id).as_ref(), Some(d));
    }
}
