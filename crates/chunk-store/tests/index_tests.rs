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
    let cases: [(&dyn Fn(&MemIo), Vec<usize>); 2] = [(&cut, (30..120).collect()), (&flip, vec![30])];
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

/// Run page layout (see run.rs): an 8-byte page header, then 58-byte
/// entries: the 33-byte ID, pack u64, offset u64, stored u32, raw u32,
/// flags u8.
const RUN_PAGE_HEADER: usize = 8;
const RUN_ENTRY: usize = 58;
const RUN_ID: usize = 33;

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
    mem.flip(r, Kind::Run, RUN_PAGE_HEADER + RUN_ENTRY + 20);
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
    for field in [0, 8, 16] {
        let mem = MemIo::new();
        let (o, objs) = closed_store(&mem);
        let s = open(&mem, &o);
        let r = run_list(&mem)[0];
        mem.flip(r, Kind::Run, RUN_PAGE_HEADER + RUN_ENTRY + RUN_ID + field);
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
    for at in [20, RUN_ID + 24] {
        let mem = MemIo::new();
        let o = opts(10, 64 << 20);
        let s = open(&mem, &o);
        let objs = put_random(&s, 10, 777);
        let r1 = run_list(&mem);
        let (x, dx) = objs[3].clone();
        let live: HashSet<ObjectId> = objs.iter().map(|(id, _)| *id).filter(|id| *id != x).collect();
        let b = s.gc_begin().unwrap();
        assert_eq!(s.gc_finish(&live, b).unwrap().removed_objects, 1);
        let (r, pos) = run_list(&mem)
            .into_iter()
            .filter(|r| !r1.contains(r))
            .find_map(|r| mem.read(r, Kind::Run).unwrap().windows(RUN_ID).position(|w| w == &x.0[..]).map(|p| (r, p)))
            .expect("the tombstone of x is in a newer run");
        mem.flip(r, Kind::Run, pos + at);
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
// per key) and fence tables (8 bytes per 70 entries), at most 2 B per
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
    let mem = MemIo::new();
    let s = Arc::new(open(&mem, &opts(50, 64 << 10)));
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
    assert!(s.index_info().runs >= 50, "{:?}", s.index_info());
    assert_eq!(s.stats().objects, 3000);
    for i in 0..3000 {
        assert_eq!(s.get(&Blake3_256.id(&small(i))), Some(small(i)), "object {i}");
    }
}
