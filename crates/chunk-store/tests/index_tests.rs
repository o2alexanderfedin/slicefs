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
