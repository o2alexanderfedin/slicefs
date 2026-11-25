//! The on-disk index (#85): objects are found through run files after a
//! reopen, GC removals are durable tombstones, and a GC that cannot read the
//! index changes nothing. In-process on MemIo; no mounts.

use chunk_store::mem::MemIo;
use chunk_store::hash::{Blake3_256, ObjectHasher};
use chunk_store::pack_io::{FileIo, Kind, PackFile, PackIo, PackRead, RunMap};
use chunk_store::retry::RetryingIo;
use chunk_store::{ChunkStore, ObjectId, Options};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
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
    // A fixed run list: merges only where a test asks for them.
    Options { pack_limit, index_memtable_limit: memtable_limit, index_background_merge: false, ..Options::default() }
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
fn a_tombstone_hides_a_put_in_an_older_run_after_a_crash() {
    let mem = MemIo::new();
    let o = opts(30, 64 << 10);
    let s = open(&mem, &o);
    let objs = put_random(&s, 300, 500);
    s.sync().unwrap();
    // Every put entry is in a run before the GC.
    assert_eq!(s.index_info().memtable_entries, 0);
    let b = s.gc_begin().unwrap();
    assert_eq!(s.gc_finish(&HashSet::new(), b).unwrap().removed_objects, 300);
    let (id0, d0) = &objs[0];
    assert_eq!(s.put(d0).unwrap(), *id0);
    s.sync().unwrap();
    drop(s);
    mem.crash();

    let s = open(&mem, &o);
    assert_eq!(s.stats().objects, 1);
    assert_eq!(s.get(id0).as_ref(), Some(d0));
    for (i, (id, _)) in objs.iter().enumerate().skip(1) {
        assert!(!s.contains(id), "object {i}");
        assert!(s.get(id).is_none(), "object {i}");
    }
    // No stale dedup hit: a removed object is written again.
    let (id1, d1) = &objs[1];
    assert_eq!(s.put(d1).unwrap(), *id1);
    assert_eq!(s.get(id1).as_ref(), Some(d1));
    assert_eq!(s.stats().objects, 2);
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
    let o = opts(30, 64 << 10);
    // The GC reads the runs (pass 1), the `.idx` of the first pack and a
    // run page per entry (pass 2), then copies the live records of that
    // pack. Fail reads at the first point after some copies: the copies
    // sit above the hwm, their originals in runs. (17-07: the point moved
    // when pass 2 came before the copies; it is searched, not fixed.)
    let (mem, s, objs) = (15..1000)
        .step_by(5)
        .find_map(|after| {
            let mem = MemIo::new();
            let s = open(&mem, &o);
            let objs = put_random(&s, 300, 2000);
            s.sync().unwrap();
            assert_eq!(s.index_info().runs, 10);
            let live: HashSet<ObjectId> = objs.iter().step_by(4).map(|(id, _)| *id).collect();
            let b = s.gc_begin().unwrap();
            mem.fail_reads_after(after);
            let r = s.gc_finish(&live, b);
            mem.clear_fail_reads_after();
            assert!(r.is_err(), "the GC fails after {after} reads");
            (s.index_info().memtable_entries > 0).then_some((mem, s, objs))
        })
        .expect("a failure point after some copies");
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

// ── Damage below the high-water mark (17-04) ─────────────────────────────────

/// Bytes of the record of a `small` object: header (ID, flags, raw and
/// stored lengths), 8 payload bytes, CRC.
const SMALL_REC: usize = 33 + 9 + 8 + 4;

/// Offset of the record of object `i` in a pack of `small` objects.
fn small_at(i: usize) -> usize {
    14 + i * SMALL_REC
}

/// 120 small objects in one active pack with two flushes, so the hwm is
/// inside the active pack. No close.
fn hwm_inside_the_active_pack(mem: &MemIo) -> (Options, Vec<ObjectId>) {
    let o = opts(50, 64 << 20);
    let s = open(mem, &o);
    let ids: Vec<ObjectId> = (0..120).map(|i| s.put(&small(i)).unwrap()).collect();
    s.sync().unwrap();
    let info = s.index_info();
    assert_eq!((info.hwm_pack, info.hwm_offset, info.runs), (1, small_at(100) as u64, 2), "{info:?}");
    drop(s);
    (o, ids)
}

/// After damage below the hwm of the active pack: the `lost` objects are not
/// found and a put writes them again; every other object reads back; the
/// stats count only what reads back; and puts after the reopen survive a
/// power loss (they must not land below the hwm, where replay skips them).
fn check_damage_below_the_hwm(mem: &MemIo, o: &Options, ids: &[ObjectId], lost: &[usize]) {
    let s = open(mem, o);
    for (i, id) in ids.iter().enumerate() {
        if lost.contains(&i) {
            assert!(!s.contains(id), "object {i}: a run points to bytes that are gone");
            assert!(s.get(id).is_none(), "object {i}");
        } else {
            assert_eq!(s.get(id), Some(small(i as u64)), "object {i}");
        }
    }
    assert_eq!(s.stats().objects, (ids.len() - lost.len()) as u64);
    for &i in lost {
        assert_eq!(s.put(&small(i as u64)).unwrap(), ids[i]);
        assert_eq!(s.get(&ids[i]), Some(small(i as u64)), "object {i} is written again");
    }
    let more: Vec<ObjectId> = (1000..1010).map(|i| s.put(&small(i)).unwrap()).collect();
    s.sync().unwrap();
    drop(s);
    mem.crash();

    let s = open(mem, o);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(s.get(id), Some(small(i as u64)), "object {i} after a power loss");
    }
    for (i, id) in more.iter().enumerate() {
        assert_eq!(s.get(id), Some(small(1000 + i as u64)), "new object {i} after a power loss");
    }
    assert_eq!(s.stats().objects, ids.len() as u64 + 10);
}

#[test]
fn an_active_pack_cut_below_the_hwm_is_rebuilt_from_the_packs() {
    let mem = MemIo::new();
    let (o, ids) = hwm_inside_the_active_pack(&mem);
    // The pack loses its bytes from the middle of record 30 on.
    let mut f = mem.open_rw(1).unwrap();
    f.set_len(small_at(30) as u64 + 10).unwrap();
    f.sync().unwrap();
    drop(f);
    let lost: Vec<usize> = (30..120).collect();
    check_damage_below_the_hwm(&mem, &o, &ids, &lost);
}

#[test]
fn a_damaged_record_below_the_hwm_costs_only_itself() {
    let mem = MemIo::new();
    let (o, ids) = hwm_inside_the_active_pack(&mem);
    // One payload byte of record 30: its CRC fails. The records after it
    // are intact, and the runs point to them.
    mem.flip(1, Kind::Pack, small_at(30) + 45);
    let manifest = mem.read(0, Kind::Manifest).unwrap();
    let runs = run_list(&mem);
    let packs = pack_lens(&mem);

    // A read-only open sees the same and writes nothing.
    let ro = Options { read_only: true, ..o.clone() };
    let s = ChunkStore::open_with(Arc::new(mem.clone()), ro).unwrap();
    for (i, id) in ids.iter().enumerate() {
        if i == 30 {
            assert!(!s.contains(id));
            assert!(s.get(id).is_none());
        } else {
            assert_eq!(s.get(id), Some(small(i as u64)), "read-only: object {i}");
        }
    }
    assert_eq!(s.stats().objects, 119);
    drop(s);
    assert_eq!(mem.read(0, Kind::Manifest).unwrap(), manifest);
    assert_eq!(run_list(&mem), runs);
    assert_eq!(pack_lens(&mem), packs);
    assert!(!mem.exists(1, Kind::Idx));

    check_damage_below_the_hwm(&mem, &o, &ids, &[30]);
}

#[test]
fn a_repair_that_stops_after_the_idx_is_done_again_at_the_next_open() {
    // A writable open makes the damaged pack sealed (a durable `.idx`), then
    // fails to remove the old index. The next open finds the pack through
    // its `.idx` and must still see that the runs point to missing bytes.
    let cut = |mem: &MemIo| {
        let mut f = mem.open_rw(1).unwrap();
        f.set_len(small_at(30) as u64 + 10).unwrap();
        f.sync().unwrap();
    };
    let flip = |mem: &MemIo| mem.flip(1, Kind::Pack, small_at(30) + 45);
    type Case<'a> = (&'a dyn Fn(&MemIo), Vec<usize>);
    let cases: [Case<'_>; 2] = [(&cut, (30..120).collect()), (&flip, vec![30])];
    for (damage, lost) in cases {
        let mem = MemIo::new();
        let (o, ids) = hwm_inside_the_active_pack(&mem);
        damage(&mem);
        mem.set_fail_removes(true);
        assert!(ChunkStore::open_with(Arc::new(mem.clone()), o.clone()).is_err(), "the MANIFEST removal fails");
        mem.set_fail_removes(false);
        assert!(mem.exists(1, Kind::Idx), "the pack was sealed first");
        assert!(mem.exists(0, Kind::Manifest));
        check_damage_below_the_hwm(&mem, &o, &ids, &lost);
    }
}

// ── Orphans, damage and the rebuild flag (17-04) ─────────────────────────────

/// 300 objects of 1,000 bytes in 64 KiB packs (five sealed packs) with a
/// memtable limit of 32 (several runs), closed.
fn closed_store(mem: &MemIo) -> (Options, Vec<(ObjectId, Vec<u8>)>) {
    let o = opts(32, 64 << 10);
    let s = open(mem, &o);
    let objs = put_random(&s, 300, 4242);
    s.close().unwrap();
    assert!(s.index_info().runs >= 2, "{:?}", s.index_info());
    drop(s);
    (o, objs)
}

fn all_read_back(s: &ChunkStore, objs: &[(ObjectId, Vec<u8>)], what: &str) {
    for (i, (id, d)) in objs.iter().enumerate() {
        assert_eq!(s.get(id).as_ref(), Some(d), "{what}: object {i}");
    }
    assert_eq!(s.stats().objects, objs.len() as u64, "{what}");
}

/// Pack lengths, which packs have an `.idx`, run numbers, MANIFEST bytes.
type Snapshot = (Vec<(u64, usize)>, Vec<bool>, Vec<u64>, Option<Vec<u8>>);

/// Every file of the store, to check that an open wrote nothing.
fn snapshot(mem: &MemIo) -> Snapshot {
    let packs = pack_lens(mem);
    let idx = packs.iter().map(|&(n, _)| mem.exists(n, Kind::Idx)).collect();
    (packs, idx, run_list(mem), mem.read(0, Kind::Manifest).ok())
}

fn read_only(mem: &MemIo, o: &Options) -> ChunkStore {
    ChunkStore::open_with(Arc::new(mem.clone()), Options { read_only: true, ..o.clone() }).unwrap()
}

/// A run file that no MANIFEST lists (the leftover of a failed flush).
fn make_orphan(mem: &MemIo) -> u64 {
    let num = run_list(mem).last().map_or(1, |r| r + 1);
    let mut f = mem.create_run(num).unwrap();
    f.write_at(b"a run that no MANIFEST lists", 0).unwrap();
    f.sync().unwrap();
    drop(f);
    mem.sync_dir().unwrap();
    num
}

#[test]
fn an_orphan_run_is_removed_at_a_writable_open() {
    let mem = MemIo::new();
    let (o, objs) = closed_store(&mem);
    let runs = run_list(&mem);
    let orphan = make_orphan(&mem);

    // A failed removal does not fail the open; a later open removes it.
    mem.set_fail_removes(true);
    let s = open(&mem, &o);
    all_read_back(&s, &objs, "the removal failed");
    drop(s);
    mem.set_fail_removes(false);
    assert!(mem.exists(orphan, Kind::Run));

    let s = open(&mem, &o);
    all_read_back(&s, &objs, "after the removal");
    drop(s);
    assert_eq!(run_list(&mem), runs);
}

#[test]
fn a_read_only_open_keeps_an_orphan_run() {
    let mem = MemIo::new();
    let (o, objs) = closed_store(&mem);
    make_orphan(&mem);
    let before = snapshot(&mem);
    let s = read_only(&mem, &o);
    all_read_back(&s, &objs, "read-only");
    drop(s);
    assert_eq!(snapshot(&mem), before);
}

/// After `damage` to the index of a closed store: a read-only open replays
/// every pack into RAM and writes nothing; a writable open removes the runs
/// and the MANIFEST and rebuilds the index from the packs, with exact stats;
/// the next open uses the new index.
fn rebuilt_after(damage: impl Fn(&MemIo, &[u64])) {
    let mem = MemIo::new();
    let (o, objs) = closed_store(&mem);
    let old_runs = run_list(&mem);
    damage(&mem, &old_runs);

    let before = snapshot(&mem);
    let s = read_only(&mem, &o);
    all_read_back(&s, &objs, "read-only");
    let info = s.index_info();
    assert_eq!((info.runs, info.memtable_entries, info.hwm_pack), (0, 300, 0), "{info:?}");
    drop(s);
    assert_eq!(snapshot(&mem), before, "a read-only open writes nothing");

    let s = open(&mem, &o);
    all_read_back(&s, &objs, "after the rebuild");
    let info = s.index_info();
    assert!(info.runs >= 1, "{info:?}");
    drop(s);
    let runs = run_list(&mem);
    assert!(runs.iter().all(|r| !old_runs.contains(r)), "old runs {old_runs:?}, now {runs:?}");
    assert_eq!(runs.len() as u64, info.runs, "no orphan");
    let manifest = mem.read(0, Kind::Manifest).expect("a new MANIFEST");
    assert_ne!(Some(manifest), before.3);

    let s = open(&mem, &o);
    all_read_back(&s, &objs, "after one more reopen");
    assert_eq!(s.index_info(), info, "no second rebuild");
    drop(s);
    assert_eq!(run_list(&mem), runs);
}

#[test]
fn a_damaged_manifest_is_rebuilt_from_the_packs() {
    rebuilt_after(|mem, _| mem.flip(0, Kind::Manifest, 20));
}

#[test]
fn a_manifest_of_another_version_is_rebuilt_from_the_packs() {
    rebuilt_after(|mem, _| {
        let mut b = mem.read(0, Kind::Manifest).unwrap();
        b[4..6].copy_from_slice(&2u16.to_le_bytes());
        let n = b.len() - 4;
        let crc = crc32c::crc32c(&b[..n]);
        b[n..].copy_from_slice(&crc.to_le_bytes());
        mem.write_durable(0, Kind::Manifest, &b).unwrap();
    });
}

#[test]
fn a_manifest_that_names_a_missing_run_is_rebuilt() {
    rebuilt_after(|mem, runs| mem.remove(runs[0], Kind::Run).unwrap());
}

#[test]
fn a_damaged_run_footer_is_rebuilt() {
    rebuilt_after(|mem, runs| {
        let len = mem.len(runs[0], Kind::Run).unwrap();
        mem.flip(runs[0], Kind::Run, len - 10);
    });
}

#[test]
fn a_read_error_at_open_is_not_damage() {
    let mem = MemIo::new();
    let (o, objs) = closed_store(&mem);
    let before = snapshot(&mem);
    // The MANIFEST read works; the read of the first run footer fails.
    mem.fail_reads_after(1);
    let r = ChunkStore::open_with(Arc::new(mem.clone()), o.clone());
    mem.clear_fail_reads_after();
    assert!(r.is_err(), "an I/O error fails the open");
    assert_eq!(snapshot(&mem), before, "and does not remove the index");
    all_read_back(&open(&mem, &o), &objs, "after the error");
}

/// Run page layout (see run.rs): a 12-byte page header (entry count u16,
/// tag u8, shared u8, CRC32C u32, then the byte widths of the pack, the
/// offset, the stored length and the raw length; raw width 0xff: no raw
/// field), then entries of one size: digest bytes `shared..32` of the ID,
/// the four fields, flags u8.
const RUN_PAGE_HEADER: usize = 12;

/// The layout of page 0 of `run`: (shared, [pack, offset, stored, raw]
/// widths in bytes, entry size).
fn run_page_layout(mem: &MemIo, run: u64) -> (usize, [usize; 4], usize) {
    let b = mem.read(run, Kind::Run).unwrap();
    let shared = b[3] as usize;
    let raw = if b[11] == 0xff { 0 } else { b[11] as usize };
    let widths = [b[8] as usize, b[9] as usize, b[10] as usize, raw];
    (shared, widths, 32 - shared + widths.iter().sum::<usize>() + 1)
}

// Lookups do not check the CRC of a run page on a hit (17-05): a damaged
// ID is never a miss (the CRC is checked before a miss is returned), so it
// costs only the objects whose search crosses it, and a put writes them
// again.
#[test]
fn a_damaged_run_page_gives_no_wrong_data() {
    let mem = MemIo::new();
    let (o, objs) = closed_store(&mem);
    let s = open(&mem, &o);
    // A digest byte of entry 1 of page 0 of run 1 (the first 32 objects).
    let r = run_list(&mem)[0];
    let (_, _, entry) = run_page_layout(&mem, r);
    mem.flip(r, Kind::Run, RUN_PAGE_HEADER + entry + 12);
    let mut bad = Vec::new();
    for (i, (id, d)) in objs.iter().enumerate() {
        match s.read(id) {
            Ok(got) => assert_eq!(&got, d, "object {i}: never wrong bytes"),
            Err(e) => {
                assert!(matches!(e, chunk_store::Error::Corrupt(_)), "object {i}: {e}");
                assert!(s.get(id).is_none(), "object {i}");
                assert!(!s.contains(id), "object {i}");
                bad.push(i);
            }
        }
    }
    assert!(!bad.is_empty() && bad.len() <= 3 && bad.iter().all(|&i| i < 32), "{bad:?}");
    // A put writes the object again; its new entry hides the damaged page.
    for &i in &bad {
        let (id, d) = &objs[i];
        assert_eq!(s.put(d).unwrap(), *id);
        assert_eq!(s.get(id).as_ref(), Some(d));
    }
    s.sync().unwrap();
    drop(s);

    // The replay of the new records looks up their IDs, finds the damaged
    // page, and the open rebuilds the index from the packs.
    let s = open(&mem, &o);
    all_read_back(&s, &objs, "after the reopen");
}

// A damaged place (pack, offset, length) in a run entry: the lookup gives
// the wrong place, and the read rejects the record there (CRC, ID, hash),
// checks the entry through the verified page read, and asks for a rebuild.
// The bytes of the object are still in its pack: the rebuild finds them.
#[test]
fn a_damaged_run_entry_place_is_never_read_and_asks_for_a_rebuild() {
    // Field 0: pack, 1: offset, 2: stored length.
    for field in [0, 1, 2] {
        let mem = MemIo::new();
        let (o, objs) = closed_store(&mem);
        let s = open(&mem, &o);
        let r = run_list(&mem)[0];
        let (shared, w, entry) = run_page_layout(&mem, r);
        assert!(w[field] > 0, "field {field}: {w:?}");
        mem.flip(r, Kind::Run, RUN_PAGE_HEADER + entry + 32 - shared + w[..field].iter().sum::<usize>());
        let mut bad = Vec::new();
        for (i, (id, d)) in objs.iter().enumerate() {
            match s.read(id) {
                Ok(got) => assert_eq!(&got, d, "field {field}, object {i}: never wrong bytes"),
                Err(e) => {
                    assert!(matches!(e, chunk_store::Error::Corrupt(_)), "field {field}, object {i}: {e}");
                    bad.push(i);
                }
            }
        }
        assert_eq!(bad.len(), 1, "field {field}: {bad:?}");
        let (x, dx) = &objs[bad[0]];
        // Accepted (see run.rs): contains is true, and a put is a dedup hit.
        assert!(s.contains(x));
        assert_eq!(s.put(dx).unwrap(), *x);
        drop(s);
        let before = run_list(&mem);

        let s = open(&mem, &o);
        all_read_back(&s, &objs, &format!("field {field}: after the rebuild"));
        drop(s);
        assert!(run_list(&mem).iter().all(|r| !before.contains(r)), "field {field}: the index was rebuilt");
    }
}

#[test]
fn a_damaged_newer_page_never_shows_an_older_entry() {
    let mem = MemIo::new();
    let o = opts(10, 64 << 20);
    let s = open(&mem, &o);
    let objs = put_random(&s, 10, 777);
    let r1 = run_list(&mem);
    assert_eq!(r1.len(), 1, "run R1 holds the 10 put entries");
    let x = objs[3].0;
    let live: HashSet<ObjectId> = objs.iter().map(|(id, _)| *id).filter(|id| *id != x).collect();
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&live, b).unwrap();
    assert_eq!((st.packs_compacted, st.removed_objects), (1, 1), "{st:?}");
    let newer: Vec<u64> = run_list(&mem).into_iter().filter(|r| !r1.contains(r)).collect();
    assert!(!newer.is_empty());
    for &r in &newer {
        mem.flip(r, Kind::Run, 100);
    }
    assert!(!s.contains(&x), "a fall-through to R1 would find the removed object");
    let r = s.read(&x);
    assert!(matches!(r, Err(chunk_store::Error::Missing(_) | chunk_store::Error::Corrupt(_))), "{r:?}");

    s.request_index_rebuild().unwrap();
    drop(s);
    let s = open(&mem, &o);
    for (i, (id, d)) in objs.iter().enumerate().filter(|(_, (id, _))| *id != x) {
        assert_eq!(s.get(id).as_ref(), Some(d), "object {i}");
    }
    assert!(!s.contains(&x));
    assert_eq!(s.stats().objects, 9);
}

// The tombstone itself damaged (a digest byte of its ID, or its flags): the
// lookup must not go on to the older run, whose put entry points to a pack
// that GC deleted. A put then writes the object again (no silent loss).
#[test]
fn a_damaged_tombstone_never_shows_the_older_entry() {
    // 0: a digest byte of the key, 1: the flags.
    for what in [0, 1] {
        let mem = MemIo::new();
        let o = opts(10, 64 << 20);
        let s = open(&mem, &o);
        let objs = put_random(&s, 10, 777);
        let r1 = run_list(&mem);
        let (x, dx) = objs[3].clone();
        let live: HashSet<ObjectId> = objs.iter().map(|(id, _)| *id).filter(|id| *id != x).collect();
        let b = s.gc_begin().unwrap();
        assert_eq!(s.gc_finish(&live, b).unwrap().removed_objects, 1);
        let (r, at) = run_list(&mem)
            .into_iter()
            .filter(|r| !r1.contains(r))
            .find_map(|r| {
                let (shared, w, _) = run_page_layout(&mem, r);
                let key = &x.0[1 + shared..];
                let pos = mem.read(r, Kind::Run).unwrap().windows(key.len()).position(|k| k == key)?;
                Some((r, if what == 0 { pos + 10 } else { pos + key.len() + w.iter().sum::<usize>() }))
            })
            .expect("the tombstone of x is in a newer run");
        mem.flip(r, Kind::Run, at);
        assert!(!s.contains(&x), "byte {at}");
        let got = s.read(&x);
        assert!(matches!(got, Err(chunk_store::Error::Corrupt(_))), "byte {at}: {got:?}");
        assert_eq!(s.put(&dx).unwrap(), x, "byte {at}");
        assert_eq!(s.get(&x).as_ref(), Some(&dx), "byte {at}: the put wrote the object again");
    }
}

#[test]
fn a_rebuild_request_rebuilds_the_index_at_the_next_writable_open() {
    let mem = MemIo::new();
    let o = opts(32, 64 << 10);
    let s = open(&mem, &o);
    let mut objs = put_random(&s, 200, 31_337);
    s.request_index_rebuild().unwrap();
    // Later flushes keep the flag.
    let runs = s.index_info().runs;
    objs.extend(put_random(&s, 100, 41_337));
    assert!(s.index_info().runs > runs);
    s.sync().unwrap();
    drop(s);

    let before = snapshot(&mem);
    let s = read_only(&mem, &o);
    all_read_back(&s, &objs, "read-only");
    let info = s.index_info();
    assert_eq!((info.runs, info.memtable_entries), (0, 300), "{info:?}");
    drop(s);
    assert_eq!(snapshot(&mem), before, "a read-only open writes nothing");

    let s = open(&mem, &o);
    all_read_back(&s, &objs, "after the rebuild");
    assert!(s.index_info().runs >= 1);
    drop(s);
    let runs = run_list(&mem);
    assert!(runs.iter().all(|r| !before.2.contains(r)), "the old runs are gone");

    // The flag is clear: no second rebuild.
    let s = open(&mem, &o);
    all_read_back(&s, &objs, "one more reopen");
    drop(s);
    assert_eq!(run_list(&mem), runs);
}

/// A `PackIo` that counts the `read_at` calls on run handles and the whole
/// file reads of runs, and passes everything else on.
struct CountRunReads {
    inner: Arc<dyn PackIo>,
    reads: Arc<AtomicU64>,
}

struct CountedRun {
    inner: Arc<dyn PackRead>,
    reads: Arc<AtomicU64>,
}

impl PackRead for CountedRun {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        self.reads.fetch_add(1, AtomicOrdering::SeqCst);
        self.inner.read_at(buf, offset)
    }
}

impl PackIo for CountRunReads {
    fn prepare(&self, read_only: bool) -> std::io::Result<bool> {
        self.inner.prepare(read_only)
    }
    fn list(&self) -> std::io::Result<Vec<u64>> {
        self.inner.list()
    }
    fn exists(&self, num: u64, kind: Kind) -> bool {
        self.inner.exists(num, kind)
    }
    fn read(&self, num: u64, kind: Kind) -> std::io::Result<Vec<u8>> {
        if kind == Kind::Run {
            self.reads.fetch_add(1, AtomicOrdering::SeqCst);
        }
        self.inner.read(num, kind)
    }
    fn write_durable(&self, num: u64, kind: Kind, data: &[u8]) -> std::io::Result<()> {
        self.inner.write_durable(num, kind, data)
    }
    fn remove(&self, num: u64, kind: Kind) -> std::io::Result<()> {
        self.inner.remove(num, kind)
    }
    fn sync_dir(&self) -> std::io::Result<()> {
        self.inner.sync_dir()
    }
    fn create(&self, num: u64) -> std::io::Result<Box<dyn PackFile>> {
        self.inner.create(num)
    }
    fn open_rw(&self, num: u64) -> std::io::Result<Box<dyn PackFile>> {
        self.inner.open_rw(num)
    }
    fn open_read(&self, num: u64) -> std::io::Result<Arc<dyn PackRead>> {
        self.inner.open_read(num)
    }
    fn can_punch(&self) -> bool {
        self.inner.can_punch()
    }
    fn punch(&self, num: u64, ranges: &[(u64, u64)]) -> std::io::Result<()> {
        self.inner.punch(num, ranges)
    }
    fn list_runs(&self) -> std::io::Result<Vec<u64>> {
        self.inner.list_runs()
    }
    fn create_run(&self, num: u64) -> std::io::Result<Box<dyn PackFile>> {
        self.inner.create_run(num)
    }
    fn open_run(&self, num: u64) -> std::io::Result<Arc<dyn PackRead>> {
        Ok(Arc::new(CountedRun { inner: self.inner.open_run(num)?, reads: Arc::clone(&self.reads) }))
    }
    fn map_run(&self, num: u64) -> std::io::Result<Option<RunMap>> {
        self.inner.map_run(num)
    }
}

// The lookup path on real files (the stack of `ChunkStore::open`: RetryingIo
// over FileIo) reads run pages in place from the mapping: a hit or a miss
// makes no `read_at` call on a run. A return to pread on the hot path fails
// this test. MemIo (no mapping) reads one page per hit, as a control.
#[test]
fn a_hit_on_files_reads_the_run_page_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts(500, 64 << 20);
    {
        let s = ChunkStore::open(dir.path(), o.clone()).unwrap();
        for i in 0..3000 {
            s.put(&small(i)).unwrap();
        }
        s.close().unwrap();
    }
    let files = || Arc::new(RetryingIo::new(Arc::new(FileIo::new(dir.path().join("packs"))))) as Arc<dyn PackIo>;
    let runs = files().list_runs().unwrap();
    assert_eq!(runs.len(), 6);
    assert!(files().map_run(runs[0]).unwrap().is_some(), "ChunkStore::open maps the runs");

    let reads = Arc::new(AtomicU64::new(0));
    let io = Arc::new(CountRunReads { inner: files(), reads: Arc::clone(&reads) });
    let s = ChunkStore::open_with(io, o.clone()).unwrap();
    assert_eq!(s.index_info().memtable_entries, 0, "every lookup goes to a run");
    let opened = reads.swap(0, AtomicOrdering::SeqCst);
    assert!(opened > 0, "the open reads the footers and tables with read_at");
    let t = std::time::Instant::now();
    for i in 0..3000 {
        assert!(s.contains(&Blake3_256.id(&small(i))), "object {i}");
    }
    let hit_ns = t.elapsed().as_nanos() / 3000;
    let t = std::time::Instant::now();
    for i in 3000..6000 {
        assert!(!s.contains(&Blake3_256.id(&small(i))), "object {i}");
    }
    let miss_ns = t.elapsed().as_nanos() / 3000;
    assert_eq!(s.get(&Blake3_256.id(&small(7))), Some(small(7)));
    assert_eq!(reads.load(AtomicOrdering::SeqCst), 0, "a lookup on files made a run read_at call");
    eprintln!("files, 6 runs of 500: contains hit {hit_ns} ns, miss {miss_ns} ns (debug build)");
    drop(s);

    // The same store on MemIo: each hit reads one page with read_at.
    let mem = MemIo::new();
    let s = open(&mem, &o);
    for i in 0..3000 {
        s.put(&small(i)).unwrap();
    }
    s.close().unwrap();
    drop(s);
    let reads = Arc::new(AtomicU64::new(0));
    let s = ChunkStore::open_with(Arc::new(CountRunReads { inner: Arc::new(mem.clone()), reads: Arc::clone(&reads) }), o).unwrap();
    reads.store(0, AtomicOrdering::SeqCst);
    for i in 0..100 {
        assert!(s.contains(&Blake3_256.id(&small(i))));
    }
    // One page for each hit, plus the filter false positives of newer runs.
    let n = reads.load(AtomicOrdering::SeqCst);
    assert!((100..=100 + 100 * 6 * 3 / 100).contains(&n), "MemIo: {n} page reads for 100 hits");
}

/// 6,000 small objects with a memtable limit of 500 (12 runs) in 64 KiB
/// packs, closed, and the IDs.
fn twelve_runs(mem: &MemIo) -> (Options, Vec<ObjectId>) {
    let o = opts(500, 64 << 10);
    let s = open(mem, &o);
    let ids: Vec<ObjectId> = (0..6000).map(|i| s.put(&small(i)).unwrap()).collect();
    s.close().unwrap();
    let info = s.index_info();
    assert_eq!((info.runs, info.memtable_entries), (12, 0), "{info:?}");
    (o, ids)
}

// IDX-01: a hit reads one run page (a filter false positive of a newer run
// adds about 1 % per run), a miss almost none.
#[test]
fn hits_read_one_page_and_misses_read_almost_none() {
    let mem = MemIo::new();
    let (o, ids) = twelve_runs(&mem);
    let s = open(&mem, &o);
    let runs = s.index_info().runs;
    assert_eq!(s.index_info().memtable_entries, 0, "every lookup goes to the runs");
    let fp_bound = 1000 * runs * 3 / 100;
    mem.reset_read_counters();
    for id in ids.iter().step_by(6) {
        assert!(s.contains(id));
    }
    let hits = mem.reads(Kind::Run);
    assert!((1000..=1000 + fp_bound).contains(&hits), "{hits} run page reads for 1,000 hits ({runs} runs)");
    mem.reset_read_counters();
    for i in 6000..7000 {
        assert!(!s.contains(&Blake3_256.id(&small(i))));
    }
    let misses = mem.reads(Kind::Run);
    assert!(misses <= fp_bound, "{misses} run page reads for 1,000 misses ({runs} runs)");
    assert_eq!(mem.bytes_read(Kind::Run), misses * 4096, "a lookup reads whole pages only");
}

// IDX-02: a cold open reads the MANIFEST, the run footers, tables and
// filters, and the `.idx` of the packs at or above the hwm; no entry page
// and no `.idx` of a pack below the hwm.
#[test]
fn a_cold_open_reads_no_entry_page() {
    let mem = MemIo::new();
    let (o, _) = twelve_runs(&mem);
    mem.reset_read_counters();
    let s = open(&mem, &o);
    let info = s.index_info();
    let packs = mem.list().unwrap();
    assert!(packs.len() >= 4, "{packs:?}");
    let entry_bytes = info.run_entries * 58;
    let run_bytes = mem.bytes_read(Kind::Run);
    assert!(run_bytes * 20 < entry_bytes, "{run_bytes} bytes read of runs with {entry_bytes} entry bytes");
    assert!(run_bytes >= info.filter_bytes + info.fence_bytes, "{run_bytes} < filters and fences {info:?}");
    let above = packs.iter().filter(|&&n| n >= info.hwm_pack).count() as u64;
    assert_eq!(mem.reads(Kind::Idx), above, "only the .idx of the packs at or above the hwm {info:?}");
    assert!(above < packs.len() as u64, "some packs are below the hwm");
    assert_eq!(mem.reads(Kind::Manifest), 1);
}

// The RAM of the index: Bloom filters (about 1.25 B per object at 10 bits
// per key) and fence tables (8 bytes per page), at most 2 B per
// object. This is anonymous memory of the process. The run pages that
// lookups read are file-backed (mapped page cache pages on files) and are
// not counted here.
#[test]
fn the_index_ram_is_about_two_bytes_per_object() {
    let mem = MemIo::new();
    let (o, _) = twelve_runs(&mem);
    let s = open(&mem, &o);
    let info = s.index_info();
    let per_object = (info.filter_bytes + info.fence_bytes) as f64 / info.run_entries as f64;
    assert!(per_object <= 2.0, "{per_object} B per object: {info:?}");
    assert!(info.filter_bytes > 0 && info.fence_bytes > 0);
}

// Lookups run outside the store lock while other threads put and flush:
// once a put of an object has returned, every later lookup finds it, also
// while a flush moves it from the memtable to a run.
#[test]
fn lookups_outside_the_lock_never_miss_a_stored_object() {
    lookups_never_miss(false);
}

// The same while background merges replace runs (17-06).
#[test]
fn lookups_never_miss_a_stored_object_during_background_merges() {
    lookups_never_miss(true);
}

fn lookups_never_miss(background: bool) {
    let mem = MemIo::new();
    let o = Options { index_background_merge: background, ..opts(50, 64 << 10) };
    let s = Arc::new(open(&mem, &o));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Objects 0..k are stored: k only goes up after the put returned.
    let stored = Arc::new(AtomicU64::new(0));
    let readers: Vec<_> = (0..3)
        .map(|t| {
            let (s, done, stored) = (Arc::clone(&s), Arc::clone(&done), Arc::clone(&stored));
            std::thread::spawn(move || {
                let mut checks = 0u64;
                let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ t;
                while !done.load(AtomicOrdering::SeqCst) {
                    let k = stored.load(AtomicOrdering::SeqCst);
                    if k == 0 {
                        continue;
                    }
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let i = x % k;
                    assert!(s.contains(&Blake3_256.id(&small(i))), "object {i} of {k}");
                    checks += 1;
                }
                checks
            })
        })
        .collect();
    let writers: Vec<_> = (0..2)
        .map(|_| {
            let s = Arc::clone(&s);
            std::thread::spawn(move || {
                for i in 0..3000 {
                    s.put(&small(i)).unwrap();
                }
            })
        })
        .collect();
    // A third writer that publishes what is stored.
    for i in 0..3000 {
        s.put(&small(i)).unwrap();
        stored.store(i + 1, AtomicOrdering::SeqCst);
    }
    for w in writers {
        w.join().unwrap();
    }
    done.store(true, AtomicOrdering::SeqCst);
    let checks: u64 = readers.into_iter().map(|r| r.join().unwrap()).sum();
    assert!(checks > 1000, "{checks} lookups");
    if background {
        s.wait_index_idle();
        assert!(s.index_info().runs <= 9, "{:?}", s.index_info());
    } else {
        assert!(s.index_info().runs >= 50, "{:?}", s.index_info());
    }
    assert_eq!(s.stats().objects, 3000);
    for i in 0..3000 {
        assert_eq!(s.get(&Blake3_256.id(&small(i))), Some(small(i)), "object {i}");
    }
}

// ── Merges (17-06) ────────────────────────────────────────────────────────────

/// The tier of a run, as the merge policy computes it at fan-in 4: the
/// integer floor of log4(max(1, entries / limit)).
fn tier(entries: u64, limit: u64) -> u32 {
    let mut n = (entries / limit).max(1);
    let mut t = 0;
    while n >= 4 {
        n /= 4;
        t += 1;
    }
    t
}

fn no_four_adjacent_runs_share_a_tier(s: &ChunkStore, limit: u64) {
    let tiers: Vec<u32> = s.index_run_entries().iter().map(|&e| tier(e, limit)).collect();
    assert!(tiers.windows(4).all(|w| w.iter().any(|&t| t != w[0])), "tiers {tiers:?}");
}

#[test]
fn merges_keep_the_run_count_small() {
    let mem = MemIo::new();
    let o = opts(50, 64 << 10);
    let s = open(&mem, &o);
    let ids: Vec<ObjectId> = (0..2000).map(|i| s.put(&small(i)).unwrap()).collect();
    assert_eq!(s.index_info().runs, 40);
    let stats = s.stats();
    let mut steps = 0;
    while s.merge_index_now().unwrap() {
        steps += 1;
        assert!(steps < 100, "the policy must end");
    }
    let check = |s: &ChunkStore, what: &str| {
        let info = s.index_info();
        no_four_adjacent_runs_share_a_tier(s, 50);
        assert!(info.runs <= 9 && (info.runs as f64) < 10.0 + 2000f64.log(4.0), "{what}: {info:?}");
        // 40 runs of tier 0 merge as base-4 digits: 2 x 800 + 2 x 200.
        assert_eq!(s.index_run_entries(), vec![200, 200, 800, 800], "{what}");
        assert_eq!(info.run_entries, 2000, "{what}: {info:?}");
        assert_eq!(run_list(&mem).len() as u64, info.runs, "{what}: the inputs are removed");
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(s.get(id), Some(small(i as u64)), "{what}: object {i}");
        }
        assert_eq!(s.stats(), stats, "{what}");
    };
    check(&s, "after the merges");
    let info = s.index_info();
    drop(s);
    let s = open(&mem, &o);
    check(&s, "after a reopen");
    assert_eq!(s.index_info(), info);
}

type Obj = (ObjectId, Vec<u8>);

/// 400 objects of 1,000 bytes; every run merged into one (the oldest run,
/// of a higher tier); then four GC rounds that each remove up to 50
/// objects, so four newer, smaller runs hold tombstones. Returns the
/// objects and which of them the GC removed.
fn store_with_tombstone_runs(mem: &MemIo) -> (Options, Vec<Obj>, Vec<bool>) {
    let o = opts(64, 64 << 10);
    let s = open(mem, &o);
    let objs = put_random(&s, 400, 77);
    s.sync().unwrap();
    assert!(s.merge_index_all().unwrap());
    assert_eq!(s.index_info().runs, 1);
    let mut removed = 0;
    for round in 0..4 {
        let gone = round * 100..round * 100 + 50;
        let live: HashSet<ObjectId> =
            objs.iter().enumerate().filter(|(i, _)| !gone.contains(i)).map(|(_, (id, _))| *id).collect();
        let b = s.gc_begin().unwrap();
        removed += s.gc_finish(&live, b).unwrap().removed_objects;
    }
    assert!(removed >= 100, "{removed} removed");
    let info = s.index_info();
    assert_eq!(info.runs, 5, "{info:?}");
    assert!(info.run_tombstones > 0, "{info:?}");
    let missing: Vec<bool> = objs.iter().map(|(id, _)| !s.contains(id)).collect();
    assert_eq!(missing.iter().filter(|&&m| m).count() as u64, removed);
    (o, objs, missing)
}

/// Every removed object is missing (and a put would write it again);
/// every other object reads back byte-exact.
fn same_content(s: &ChunkStore, objs: &[Obj], missing: &[bool], what: &str) {
    for (i, ((id, d), &gone)) in objs.iter().zip(missing).enumerate() {
        if gone {
            assert!(!s.contains(id), "{what}: removed object {i}");
            assert!(s.get(id).is_none(), "{what}: removed object {i}");
        } else {
            assert_eq!(s.get(id).as_ref(), Some(d), "{what}: object {i}");
        }
    }
    let live = missing.iter().filter(|&&m| !m).count() as u64;
    assert_eq!(s.stats().objects, live, "{what}");
}

#[test]
fn a_merge_of_newer_runs_keeps_the_tombstones() {
    let mem = MemIo::new();
    let (o, objs, missing) = store_with_tombstone_runs(&mem);
    let s = open(&mem, &o);
    let oldest = run_list(&mem)[0];
    let tombs = s.index_info().run_tombstones;
    // The four small GC runs merge; the oldest run is not in the window.
    assert!(s.merge_index_now().unwrap());
    assert!(!s.merge_index_now().unwrap());
    let info = s.index_info();
    assert_eq!(info.runs, 2, "{info:?}");
    assert_eq!(run_list(&mem)[0], oldest, "the oldest run was not merged");
    assert_eq!(info.run_tombstones, tombs, "a merge without the oldest run keeps every tombstone: {info:?}");
    same_content(&s, &objs, &missing, "after the merge");
    drop(s);
    let s = open(&mem, &o);
    same_content(&s, &objs, &missing, "after a reopen");
}

#[test]
fn merge_index_all_drops_the_tombstones() {
    let mem = MemIo::new();
    let (o, objs, missing) = store_with_tombstone_runs(&mem);
    let s = open(&mem, &o);
    assert!(s.merge_index_all().unwrap());
    let info = s.index_info();
    assert_eq!((info.runs, info.run_tombstones), (1, 0), "{info:?}");
    same_content(&s, &objs, &missing, "after the merge");
    assert!(!s.merge_index_all().unwrap(), "one run without tombstones: nothing to do");
    drop(s);
    assert_eq!(run_list(&mem).len(), 1);
    let s = open(&mem, &o);
    assert_eq!(s.index_info().run_tombstones, 0);
    same_content(&s, &objs, &missing, "after a reopen");
    // No stale dedup hit: a removed object is written again.
    let i = missing.iter().position(|&m| m).unwrap();
    assert_eq!(s.put(&objs[i].1).unwrap(), objs[i].0);
    assert_eq!(s.get(&objs[i].0).as_ref(), Some(&objs[i].1));
}

// GC removes every object, then a merge of every run: nothing is left, so
// no run replaces the inputs.
#[test]
fn a_merge_with_nothing_left_writes_no_run() {
    let mem = MemIo::new();
    let o = opts(32, 64 << 10);
    let s = open(&mem, &o);
    let objs = put_random(&s, 100, 9);
    let b = s.gc_begin().unwrap();
    assert_eq!(s.gc_finish(&HashSet::new(), b).unwrap().removed_objects, 100);
    assert!(s.index_info().runs >= 2);
    assert!(s.merge_index_all().unwrap());
    let info = s.index_info();
    assert_eq!((info.runs, info.run_entries), (0, 0), "{info:?}");
    assert!(run_list(&mem).is_empty());
    same_content(&s, &objs, &[true; 100], "after the merge");
    drop(s);
    let s = open(&mem, &o);
    same_content(&s, &objs, &[true; 100], "after a reopen");
}

// A merge reads its inputs through the verified page reads. A damaged page
// ends the merge with Corrupt, keeps no output, and asks for a rebuild:
// the next open rebuilds the index from the packs (a read-only open in RAM).
#[test]
fn a_damaged_page_found_by_a_merge_leads_to_a_rebuild() {
    let mem = MemIo::new();
    let o = opts(32, 64 << 10);
    let s = open(&mem, &o);
    let mut objs = put_random(&s, 300, 4242);
    s.sync().unwrap();
    let runs = run_list(&mem);
    assert!(runs.len() >= 4, "{runs:?}");
    mem.flip(runs[0], Kind::Run, 100);
    let e = s.merge_index_all().unwrap_err();
    assert!(matches!(e, chunk_store::Error::Corrupt(_)), "{e}");
    assert_eq!(run_list(&mem), runs, "no output was kept");
    assert!(!s.merge_index_now().unwrap(), "no merge after a rebuild request");
    // The store is not failed.
    let d = random(1000, 1);
    let id = s.put(&d).unwrap();
    assert_eq!(s.get(&id).as_ref(), Some(&d));
    objs.push((id, d));
    s.sync().unwrap();
    drop(s);

    let before = snapshot(&mem);
    let ro = read_only(&mem, &o);
    let info = ro.index_info();
    assert_eq!((info.runs, info.memtable_entries), (0, 301), "the read-only open replays every pack: {info:?}");
    all_read_back(&ro, &objs, "read-only");
    drop(ro);
    assert_eq!(snapshot(&mem), before, "a read-only open writes nothing");

    let s = open(&mem, &o);
    all_read_back(&s, &objs, "after the rebuild");
    assert!(run_list(&mem).iter().all(|r| !runs.contains(r)), "the old runs are gone");
}

// A merge removes its input runs once its MANIFEST is durable. A store
// that has them open keeps reading them (a read-only tool next to a
// mount); a store opened after the merge reads the new run.
#[test]
fn a_read_only_store_keeps_reading_after_a_merge_removed_its_runs() {
    let mem = MemIo::new();
    let (o, objs) = closed_store(&mem);
    let a = open(&mem, &o);
    let b = read_only(&mem, &o);
    let old = run_list(&mem);
    assert!(a.merge_index_all().unwrap());
    assert!(old.iter().all(|&r| !mem.exists(r, Kind::Run)), "the inputs are removed");
    all_read_back(&b, &objs, "the read-only store opened before the merge");
    let c = read_only(&mem, &o);
    assert_eq!(c.index_info().runs, 1);
    all_read_back(&c, &objs, "a read-only store opened after the merge");
    all_read_back(&a, &objs, "the writer");
}

#[test]
fn a_read_only_store_refuses_a_merge() {
    let mem = MemIo::new();
    let (o, _) = closed_store(&mem);
    let ro = read_only(&mem, &o);
    assert!(ro.merge_index_now().is_err());
    assert!(ro.merge_index_all().is_err());
}
