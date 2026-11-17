//! Spike D: GC that punches holes over dead records instead of compaction.
//! Correctness and crash tests. 1 MiB arenas, V1 (sparse) unless named.
//! A crash is simulated by a fault hook (`sys::Fault`) that makes the GC
//! stop at that point, then dropping the store without `close`.

use arena_spike::codec::{self, rec_len, Decoded, MAX_STORED};
use arena_spike::sys::{self, Fault};
use arena_spike::{GcMode, ObjectId, Options, Prealloc, SpikeStore};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const MIB: u64 = 1 << 20;

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

fn opts(p: Prealloc) -> Options {
    let mut o = Options::with_prealloc(p);
    o.arena.size = MIB;
    o.gc = GcMode::Punch;
    o
}

fn packs(dir: &Path) -> PathBuf {
    dir.join("packs")
}

fn arena_file(dir: &Path, num: u64) -> PathBuf {
    packs(dir).join(format!("{num:016x}.arena"))
}

fn idx_file(dir: &Path, num: u64) -> PathBuf {
    packs(dir).join(format!("{num:016x}.idx"))
}

fn mark_file(dir: &Path, num: u64) -> PathBuf {
    packs(dir).join(format!("{num:016x}.punched"))
}

fn allocated(dir: &Path) -> u64 {
    fs::read_dir(packs(dir)).unwrap().map(|e| e.unwrap().metadata().unwrap().blocks() * 512).sum()
}

/// `n` random objects of 3-12 KiB, synced; then the active arena is sealed
/// so every arena can be punched.
fn fill(s: &SpikeStore, n: u64, seed: u64) -> Vec<(ObjectId, Vec<u8>)> {
    let out: Vec<_> = (0..n)
        .map(|i| {
            let d = random(3000 + (i as usize * 7919) % 9000, seed * 100_000 + i);
            (s.put(&d).unwrap(), d)
        })
        .collect();
    s.sync().unwrap();
    s.seal_active().unwrap();
    out
}

/// Every other object dead, in runs of `run` objects.
fn split(objs: &[(ObjectId, Vec<u8>)], run: usize) -> (Vec<&(ObjectId, Vec<u8>)>, Vec<&(ObjectId, Vec<u8>)>) {
    let (mut live, mut dead) = (Vec::new(), Vec::new());
    for (i, o) in objs.iter().enumerate() {
        if (i / run) % 2 == 0 {
            live.push(o);
        } else {
            dead.push(o);
        }
    }
    (live, dead)
}

fn ids(v: &[&(ObjectId, Vec<u8>)]) -> HashSet<ObjectId> {
    v.iter().map(|o| o.0).collect()
}

fn gc(s: &SpikeStore, live: &HashSet<ObjectId>) -> arena_spike::GcStats {
    let t = s.gc_begin().unwrap();
    s.gc_finish(live, t).unwrap()
}

fn assert_all(s: &SpikeStore, v: &[&(ObjectId, Vec<u8>)]) {
    for (id, d) in v {
        assert_eq!(s.get(id).as_deref(), Some(d.as_slice()), "live object lost or wrong");
    }
}

#[test]
fn punch_frees_blocks_and_live_records_stay_in_place() {
    for p in [Prealloc::Grow, Prealloc::Sparse, Prealloc::Reserve, Prealloc::Zeroed] {
        let d = TempDir::new().unwrap();
        let s = SpikeStore::open(d.path(), opts(p)).unwrap();
        let objs = fill(&s, 400, 1);
        let (live, dead) = split(&objs, 4);
        let locs: Vec<_> = live.iter().map(|o| s.loc(&o.0).unwrap()).collect();
        let a0 = allocated(d.path());
        let st = gc(&s, &ids(&live));
        let a1 = allocated(d.path());
        assert_eq!(st.removed_objects, dead.len() as u64);
        assert!(st.packs_compacted > 0, "{p:?}: nothing punched");
        assert!(dead.iter().all(|o| s.loc(&o.0).is_none()), "dead objects are still indexed");
        assert!(a0 - a1 > 0, "{p:?}: no block freed");
        // Live records did not move.
        for (o, l) in live.iter().zip(&locs) {
            assert_eq!(s.loc(&o.0).unwrap(), *l);
        }
        assert_all(&s, &live);
        for o in &dead {
            assert!(s.get(&o.0).is_none());
        }
        drop(s);
        let s = SpikeStore::open(d.path(), opts(p)).unwrap();
        assert_all(&s, &live);
        for o in &dead {
            assert!(!s.contains(&o.0), "{p:?}: a punched record is indexed after reopen");
        }
    }
}

#[test]
fn a_punched_range_reads_as_zeros_and_seek_data_skips_it() {
    let d = TempDir::new().unwrap();
    let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
    let objs = fill(&s, 200, 2);
    let (live, dead) = split(&objs, 8);
    let dl: Vec<_> = dead.iter().map(|o| s.loc(&o.0).unwrap()).collect();
    gc(&s, &ids(&live));
    let mut zero_blocks = 0;
    for l in &dl {
        let f = fs::File::open(arena_file(d.path(), l.arena)).unwrap();
        if let Some((off, len)) = sys::aligned_inside(l.off, l.off + rec_len(l.stored) as u64, 4096) {
            let mut b = vec![0xffu8; len as usize];
            f.read_exact_at(&mut b, off).unwrap();
            assert!(b.iter().all(|&x| x == 0), "punched range is not zeros");
            // A record is not found there.
            assert_eq!(codec::decode_record(&b, MAX_STORED), Decoded::Bad);
            assert!(sys::seek_data(&f, off).unwrap().unwrap() > off, "SEEK_DATA does not skip the hole");
            zero_blocks += 1;
        }
    }
    assert!(zero_blocks > 0);
}

#[test]
fn a_put_of_a_dead_id_after_the_punch_writes_it_again() {
    let d = TempDir::new().unwrap();
    let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
    let objs = fill(&s, 200, 3);
    let (live, dead) = split(&objs, 3);
    gc(&s, &ids(&live));
    for (id, data) in &dead {
        assert_eq!(s.put(data).unwrap(), *id);
        assert_eq!(s.get(id).as_deref(), Some(data.as_slice()));
    }
    s.sync().unwrap();
    s.close().unwrap();
    drop(s);
    let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
    assert_all(&s, &dead);
    assert_all(&s, &live);
}

/// Without its idx, a sealed punched arena is read by the resync scan,
/// not by the chain walk; it is not taken for the active arena (it is the
/// newest arena here) and is not cut.
#[test]
fn a_punched_arena_without_its_idx_keeps_every_live_record() {
    for p in [Prealloc::Grow, Prealloc::Sparse] {
        let d = TempDir::new().unwrap();
        let s = SpikeStore::open(d.path(), opts(p)).unwrap();
        let objs = fill(&s, 400, 4);
        let (live, _) = split(&objs, 2);
        gc(&s, &ids(&live));
        let newest = live.iter().map(|o| s.loc(&o.0).unwrap().arena).max().unwrap();
        let older = live.iter().map(|o| s.loc(&o.0).unwrap().arena).min().unwrap();
        assert!(newest > older);
        drop(s);
        let len0 = fs::metadata(arena_file(d.path(), newest)).unwrap().len();
        let blocks0 = fs::metadata(arena_file(d.path(), newest)).unwrap().blocks();
        fs::remove_file(idx_file(d.path(), newest)).unwrap();
        fs::write(idx_file(d.path(), older), b"garbage").unwrap();
        let s = SpikeStore::open(d.path(), opts(p)).unwrap();
        assert_all(&s, &live);
        assert!(s.active().is_none(), "{p:?}: the punched arena was taken as active");
        assert_eq!(fs::metadata(arena_file(d.path(), newest)).unwrap().len(), len0, "{p:?}: cut");
        assert_eq!(fs::metadata(arena_file(d.path(), newest)).unwrap().blocks(), blocks0);
        drop(s);
        let s = SpikeStore::open(d.path(), opts(p)).unwrap();
        assert_all(&s, &live);
    }
}

/// Why the `.punched` mark is needed: without it the newest arena is taken
/// for the active one and the chain walk stops at the first hole, so the
/// recovery cut destroys the live records after it.
#[test]
fn without_the_mark_the_chain_walk_loses_the_records_after_the_first_hole() {
    let d = TempDir::new().unwrap();
    let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
    let objs = fill(&s, 400, 5);
    let (live, _) = split(&objs, 2);
    gc(&s, &ids(&live));
    let newest = live.iter().map(|o| s.loc(&o.0).unwrap().arena).max().unwrap();
    let in_newest: Vec<_> = live.iter().filter(|o| s.loc(&o.0).unwrap().arena == newest).copied().collect();
    drop(s);
    fs::remove_file(idx_file(d.path(), newest)).unwrap();
    fs::remove_file(mark_file(d.path(), newest)).unwrap();
    let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
    let lost = in_newest.iter().filter(|(id, d)| s.get(id).as_deref() != Some(d.as_slice())).count();
    assert!(lost > 0, "expected the chain walk to lose records after a hole");
}

/// A dead record whose header was punched away can leave a payload tail
/// that holds a well-formed record (marker, bounds and CRC pass). The
/// resync scan rejects it because BLAKE3 of its bytes is not its id.
#[test]
fn a_fake_inner_record_in_a_dead_fragment_is_rejected_by_blake3() {
    let d = TempDir::new().unwrap();
    let mut o = opts(Prealloc::Sparse);
    o.zstd_level = 0;
    let s = SpikeStore::open(d.path(), o).unwrap();
    let a = random(6000, 61);
    let b = random(6000, 62);
    // C: random bytes, then an inner record with the id of other bytes.
    let fake_id = ObjectId::of(b"some other bytes");
    let inner = codec::encode_record(&fake_id, 0, 500, &random(500, 63), 1);
    let mut c = random(9000, 64);
    c.extend_from_slice(&inner);
    let (ia, ic, ib) = (s.put(&a).unwrap(), s.put(&c).unwrap(), s.put(&b).unwrap());
    s.sync().unwrap();
    s.seal_active().unwrap();
    let lc = s.loc(&ic).unwrap();
    let inner_off = lc.off + codec::REC_HEADER as u64 + 9000;
    // Punch only the block-aligned part before the inner record (as a GC
    // that killed C would, if the run ended there).
    let (po, pl) = sys::aligned_inside(lc.off, inner_off, 4096).unwrap();
    assert!(po + pl <= inner_off);
    let gs = gc(&s, &[ia, ib].into_iter().collect());
    assert_eq!(gs.removed_objects, 1);
    drop(s);
    let f = fs::OpenOptions::new().read(true).write(true).open(arena_file(d.path(), lc.arena)).unwrap();
    // The inner record is still there and passes the CRC.
    let mut buf = vec![0u8; inner.len()];
    f.read_exact_at(&mut buf, inner_off).unwrap();
    assert!(matches!(codec::decode_record(&buf, MAX_STORED), Decoded::Ok(h) if h.id == fake_id));
    fs::remove_file(idx_file(d.path(), lc.arena)).unwrap();
    let mut o = opts(Prealloc::Sparse);
    o.zstd_level = 0;
    let s = SpikeStore::open(d.path(), o).unwrap();
    assert!(!s.contains(&fake_id), "a fake record was accepted");
    assert!(!s.contains(&ic));
    assert_eq!(s.get(&ia).as_deref(), Some(a.as_slice()));
    assert_eq!(s.get(&ib).as_deref(), Some(b.as_slice()));
}

/// Crash after the new idx is durable, before any punch: the dead records
/// are no longer indexed (nothing dedups onto them), the space is not yet
/// freed, and the first GC after the reopen frees it.
#[test]
fn a_crash_after_the_idx_rewrite_leaks_space_only_until_the_next_gc() {
    for fault in [Fault::PunchAfterIdx, Fault::PunchMidway] {
        let d = TempDir::new().unwrap();
        let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
        let objs = fill(&s, 400, 7);
        let (live, dead) = split(&objs, 4);
        let a0 = allocated(d.path());
        let t = s.gc_begin().unwrap();
        sys::arm(fault);
        assert!(s.gc_finish(&ids(&live), t).is_err(), "{fault:?} did not fire");
        drop(s); // crash
        let a1 = allocated(d.path());
        let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
        assert_all(&s, &live);
        let indexed = dead.iter().filter(|o| s.contains(&o.0)).count();
        // The fault hit in the first punched arena: its dead records left the
        // idx; the arenas after it were not reached and keep theirs.
        assert!(indexed < dead.len(), "{fault:?}");
        for (id, data) in &dead {
            if s.contains(id) {
                assert_eq!(s.get(id).as_deref(), Some(data.as_slice()), "an indexed dead record is not readable");
            }
        }
        // First GC after open: every gap of every sealed arena is punched.
        gc(&s, &ids(&live));
        let a2 = allocated(d.path());
        assert!(a2 < a1, "{fault:?}: the next GC freed nothing ({a1} -> {a2})");
        assert!(a1 <= a0);
        assert_all(&s, &live);
        drop(s);
        let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
        assert_all(&s, &live);
    }
}

#[test]
fn objects_put_or_read_during_the_gc_are_not_punched() {
    let d = TempDir::new().unwrap();
    let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
    let objs = fill(&s, 300, 8);
    let t = s.gc_begin().unwrap();
    // Nothing is in the live set; three objects are touched during the GC.
    let (r, w) = (&objs[10], &objs[11]);
    assert!(s.get(&r.0).is_some());
    s.put(&w.1).unwrap(); // dedup hit
    let gs = s.gc_finish(&HashSet::new(), t).unwrap();
    assert_eq!(gs.kept_objects, 2);
    assert_eq!(s.get(&r.0).as_deref(), Some(r.1.as_slice()));
    assert_eq!(s.get(&w.0).as_deref(), Some(w.1.as_slice()));
    drop(s);
    let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
    assert_eq!(s.get(&r.0).as_deref(), Some(r.1.as_slice()));
    assert_eq!(s.get(&w.0).as_deref(), Some(w.1.as_slice()));
}

#[test]
fn gc_abort_punches_nothing() {
    let d = TempDir::new().unwrap();
    let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
    let objs = fill(&s, 300, 9);
    let a0 = allocated(d.path());
    let _t = s.gc_begin().unwrap();
    s.gc_abort();
    assert_eq!(allocated(d.path()), a0);
    for (id, data) in &objs {
        assert_eq!(s.get(id).as_deref(), Some(data.as_slice()));
    }
    assert_eq!(s.punch_stats().calls, 0);
}

#[test]
fn a_read_only_open_of_a_punched_store_changes_nothing() {
    let d = TempDir::new().unwrap();
    let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
    let objs = fill(&s, 400, 10);
    let (live, _) = split(&objs, 2);
    gc(&s, &ids(&live));
    let newest = live.iter().map(|o| s.loc(&o.0).unwrap().arena).max().unwrap();
    drop(s);
    fs::remove_file(idx_file(d.path(), newest)).unwrap();
    let list = |d: &Path| {
        let mut v: Vec<_> = fs::read_dir(packs(d))
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let m = e.metadata().unwrap();
                (e.file_name(), m.len(), m.blocks(), m.mtime_nsec())
            })
            .collect();
        v.sort();
        v
    };
    let before = list(d.path());
    let mut o = opts(Prealloc::Sparse);
    o.read_only = true;
    let s = SpikeStore::open(d.path(), o).unwrap();
    assert_all(&s, &live);
    drop(s);
    assert_eq!(list(d.path()), before);
}

#[test]
fn a_mark_without_its_arena_is_removed_at_open() {
    let d = TempDir::new().unwrap();
    let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
    let objs = fill(&s, 50, 11);
    let num = s.loc(&objs[0].0).unwrap().arena;
    drop(s);
    // A crash between the unlink of a dead arena and of its mark, for the
    // arena number the next arena will get.
    fs::write(mark_file(d.path(), num + 1), b"SFPH").unwrap();
    let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
    assert!(!mark_file(d.path(), num + 1).exists());
    let x = random(5000, 99);
    let id = s.put(&x).unwrap();
    s.sync().unwrap();
    assert_eq!(s.loc(&id).unwrap().arena, num + 1);
    drop(s);
    let s = SpikeStore::open(d.path(), opts(Prealloc::Sparse)).unwrap();
    assert_eq!(s.get(&id).as_deref(), Some(x.as_slice()));
    assert!(s.active().is_some(), "the new arena must still be the active one");
}

/// Repeated GCs: a record that dies later joins the gap next to an older
/// hole, so the gap is punched as one run (the complement of the live
/// records, not the union of the dead ones).
#[test]
fn a_later_gc_joins_old_holes_with_newly_dead_neighbours() {
    let d = TempDir::new().unwrap();
    let mut o = opts(Prealloc::Sparse);
    o.zstd_level = 0;
    let s = SpikeStore::open(d.path(), o).unwrap();
    // 3000-byte objects: one dead object alone frees no whole block.
    let objs: Vec<_> = (0..300).map(|i| {
        let d = random(3000, 500 + i);
        (s.put(&d).unwrap(), d)
    }).collect();
    s.sync().unwrap();
    s.seal_active().unwrap();
    let a0 = allocated(d.path());
    // GC 1: every odd object dies: single 3 KiB gaps, almost nothing freed.
    let live1: HashSet<_> = objs.iter().step_by(2).map(|o| o.0).collect();
    gc(&s, &live1);
    let a1 = allocated(d.path());
    // GC 2: every object at 2 mod 4 dies too: gaps of 3 records (~9 KiB).
    let live2: HashSet<_> = objs.iter().step_by(4).map(|o| o.0).collect();
    gc(&s, &live2);
    let a2 = allocated(d.path());
    let freed1 = a0 - a1;
    let freed2 = a1 - a2;
    assert!(freed2 > 2 * freed1.max(1), "GC 2 did not join the gaps: {freed1} then {freed2}");
    for o in objs.iter().step_by(4) {
        assert_eq!(s.get(&o.0).as_deref(), Some(o.1.as_slice()));
    }
}

/// The index check: a punched range that an idx still lists (the wrong
/// order, punch before the idx rewrite, then a crash). `get` detects it
/// (marker fails: `None`, never wrong bytes), but `contains` stays true and
/// a `put` of the same bytes dedups onto the zeros, so the object stays
/// unreadable. This is why the idx is rewritten durably before any punch.
#[test]
fn a_punched_record_still_in_the_idx_reads_none_and_a_put_does_not_repair_it() {
    let d = TempDir::new().unwrap();
    let mut o = opts(Prealloc::Sparse);
    o.zstd_level = 0;
    let s = SpikeStore::open(d.path(), o).unwrap();
    s.put(&random(5000, 70)).unwrap();
    let x = random(20000, 71);
    let id = s.put(&x).unwrap();
    s.sync().unwrap();
    s.seal_active().unwrap();
    let l = s.loc(&id).unwrap();
    drop(s);
    let f = fs::OpenOptions::new().write(true).open(arena_file(d.path(), l.arena)).unwrap();
    // The blocks from the one that holds the record header to the last
    // whole block of the record.
    let start = l.off / 4096 * 4096;
    let end = (l.off + rec_len(l.stored) as u64) / 4096 * 4096;
    sys::punch_hole(&f, start, end - start).unwrap();
    drop(f);
    let s = SpikeStore::open(d.path(), o).unwrap();
    assert!(s.contains(&id));
    assert!(s.get(&id).is_none(), "a punched record must not read as data");
    assert_eq!(s.put(&x).unwrap(), id);
    assert!(s.get(&id).is_none(), "expected the put to dedup onto the punched record");
}
