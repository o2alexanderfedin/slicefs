//! Spike C tests: reuse mode (`ReuseStore`). A "crash" is a drop without
//! `close` (the store has no `Drop`), so the files are as a kill -9 leaves
//! them; fault hooks stop a multi-step change in the middle.

use arena_spike::codec::{self, HEADER_PAGE};
use arena_spike::reuse::{self, ReuseStore};
use arena_spike::sys::{self, Fault};
use arena_spike::workload::{compressible, random};
use arena_spike::{arena, AllocKind, GcToken, ObjectId, Options, Prealloc, ReuseCfg};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const KINDS: [AllocKind; 4] = [AllocKind::NextFit, AllocKind::SizeClasses, AllocKind::Hybrid, AllocKind::ArenaBest];

fn opts(kind: AllocKind, g: u32) -> Options {
    let mut o = Options::with_prealloc(Prealloc::Sparse);
    o.arena.size = 1 << 20;
    o.arena.granule = g;
    o.reuse = Some(ReuseCfg {
        alloc: kind,
        evacuate_below: (kind == AllocKind::Hybrid).then_some(0.10),
        ..Default::default()
    });
    o
}

fn obj(i: u64, len: usize) -> Vec<u8> {
    if i % 2 == 0 {
        random(len, i + 1)
    } else {
        compressible(len, i + 1)
    }
}

fn put_n(s: &ReuseStore, from: u64, n: u64) -> Vec<(ObjectId, Vec<u8>)> {
    (from..from + n)
        .map(|i| {
            let d = obj(i, 2048 + (i as usize * 977) % 14000);
            (s.put(&d).unwrap(), d)
        })
        .collect()
}

fn gc(s: &ReuseStore, live: &HashSet<ObjectId>) {
    let t = s.gc_begin().unwrap();
    s.gc_finish(live, t).unwrap();
}

fn arena_files(dir: &Path) -> usize {
    fs::read_dir(dir.join("packs")).unwrap().filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "arena")).count()
}

fn check_all(s: &ReuseStore, objs: &[(ObjectId, Vec<u8>)]) {
    for (id, d) in objs {
        assert_eq!(s.get(id).as_deref(), Some(&d[..]), "object lost or wrong");
    }
}

#[test]
fn put_get_reopen_after_close_and_after_crash() {
    for k in KINDS {
        for g in [1u32, 64, 512] {
            let d = tempfile::tempdir().unwrap();
            let s = ReuseStore::open(d.path(), opts(k, g)).unwrap();
            let a = put_n(&s, 0, 300);
            s.sync().unwrap();
            s.close().unwrap();
            drop(s);
            let (s, rep) = ReuseStore::open_report(d.path(), opts(k, g)).unwrap();
            assert_eq!(rep.scanned, 0, "{k:?} G={g}: a clean close needs no scan");
            check_all(&s, &a);
            let b = put_n(&s, 1000, 100);
            s.sync().unwrap();
            drop(s); // crash
            let (s, rep) = ReuseStore::open_report(d.path(), opts(k, g)).unwrap();
            assert!(rep.scanned >= 1, "{k:?} G={g}");
            check_all(&s, &a);
            check_all(&s, &b);
        }
    }
}

#[test]
fn freed_space_is_reused_and_the_store_does_not_grow() {
    for k in KINDS {
        let d = tempfile::tempdir().unwrap();
        let s = ReuseStore::open(d.path(), opts(k, 64)).unwrap();
        let mut keep: Vec<(ObjectId, Vec<u8>)> = put_n(&s, 0, 400);
        s.sync().unwrap();
        let files0 = arena_files(d.path());
        for round in 0..6u64 {
            // Drop every other object, put as many new ones.
            let (gone, kept): (Vec<_>, Vec<_>) = keep.into_iter().enumerate().partition(|(i, _)| i % 2 == 0);
            let mut kept: Vec<_> = kept.into_iter().map(|x| x.1).collect();
            let live: HashSet<ObjectId> = kept.iter().map(|x| x.0).collect();
            gc(&s, &live);
            for (_, (id, _)) in &gone {
                assert!(!s.contains(id));
            }
            kept.extend(put_n(&s, 10_000 * (round + 1), gone.len() as u64));
            s.sync().unwrap();
            check_all(&s, &kept);
            keep = kept;
        }
        let files = arena_files(d.path());
        assert!(files <= files0 + 2, "{k:?}: {files0} arenas grew to {files}");
        s.close().unwrap();
        let s = ReuseStore::open(d.path(), opts(k, 64)).unwrap();
        check_all(&s, &keep);
    }
}

#[test]
fn a_torn_record_in_a_hole_is_not_found_and_its_space_is_free_again() {
    let d = tempfile::tempdir().unwrap();
    // V2 (size classes) puts a small record into a hole, not the tail.
    let o = opts(AllocKind::SizeClasses, 64);
    let s = ReuseStore::open(d.path(), o).unwrap();
    let a = put_n(&s, 0, 50);
    s.close().unwrap();
    // Free the first 10 objects.
    let live: HashSet<ObjectId> = a[10..].iter().map(|x| x.0).collect();
    gc(&s, &live);
    let c = obj(777, 3000);
    let cid = s.put(&c).unwrap();
    let cl = s.loc(&cid).unwrap();
    let first = s.loc(&a[10].0).unwrap();
    assert!(cl.arena == first.arena && cl.off < first.off, "C went into a hole");
    s.sync().unwrap();
    drop(s);
    // Tear C: its last bytes never reached the disk.
    let f = fs::OpenOptions::new().write(true).open(arena::arena_path(&d.path().join("packs"), cl.arena)).unwrap();
    let end = cl.off + codec::rec_len(cl.stored) as u64;
    f.write_all_at(&[0u8; 100], end - 100).unwrap();
    drop(f);
    let s = ReuseStore::open(d.path(), o).unwrap();
    assert!(!s.contains(&cid), "a torn record must not be indexed");
    assert_eq!(s.get(&cid), None);
    check_all(&s, &a[10..]);
    // Put again: stored, readable.
    assert_eq!(s.put(&c).unwrap(), cid);
    s.sync().unwrap();
    assert_eq!(s.get(&cid).as_deref(), Some(&c[..]));
}

/// A user payload that holds a record with a good CRC but the wrong BLAKE3
/// (and one with a correct BLAKE3). The outer record is damaged, so the
/// granule scan walks through its payload.
#[test]
fn a_crafted_inner_record_with_the_wrong_hash_is_rejected() {
    for g in [1u32, 64] {
        let d = tempfile::tempdir().unwrap();
        let s = ReuseStore::open(d.path(), opts(AllocKind::NextFit, g)).unwrap();
        let fake_id = ObjectId::of(b"not these bytes");
        let fake = codec::encode_record(&fake_id, 0, 5, b"hello", 1);
        let real_inner = b"inner object bytes".to_vec();
        let real_id = ObjectId::of(&real_inner);
        let good = codec::encode_record(&real_id, 0, real_inner.len() as u32, &real_inner, 1);
        let mut payload = random(4096, 9);
        // Place both inner records at granule-aligned positions of the arena
        // (the outer record starts at a granule; its payload at +41).
        let pad = |at: usize| (at + 41).div_ceil(g as usize) * g as usize - 41;
        let p1 = pad(100);
        payload[p1..p1 + fake.len()].copy_from_slice(&fake);
        let p2 = pad(2000);
        payload[p2..p2 + good.len()].copy_from_slice(&good);
        let outer = s.put(&payload).unwrap();
        let ol = s.loc(&outer).unwrap();
        assert_eq!(ol.flags, 0, "the payload must be stored uncompressed");
        s.sync().unwrap();
        drop(s);
        // Damage the outer record's marker (as if overwritten).
        let f = fs::OpenOptions::new().write(true).open(arena::arena_path(&d.path().join("packs"), ol.arena)).unwrap();
        f.write_all_at(&[0u8; 4], ol.off).unwrap();
        drop(f);
        let s = ReuseStore::open(d.path(), opts(AllocKind::NextFit, g)).unwrap();
        assert!(!s.contains(&outer));
        assert!(!s.contains(&fake_id), "G={g}: a record with the wrong BLAKE3 was accepted");
        // The inner record with the right hash is correct bytes: harmless.
        if s.contains(&real_id) {
            assert_eq!(s.get(&real_id).as_deref(), Some(&real_inner[..]));
        }
        // Re-put of the outer object works.
        s.put(&payload).unwrap();
        assert_eq!(s.get(&outer).as_deref(), Some(&payload[..]));
    }
}

#[test]
fn rule_w_the_dirty_list_is_durable_before_the_first_write() {
    let d = tempfile::tempdir().unwrap();
    let s = ReuseStore::open(d.path(), opts(AllocKind::NextFit, 64)).unwrap();
    let a = put_n(&s, 0, 20);
    s.close().unwrap(); // checkpoints everything, dirty list empty
    let num = s.loc(&a[0].0).unwrap().arena;
    let path = arena::arena_path(&d.path().join("packs"), num);
    let before = fs::read(&path).unwrap();
    sys::arm(Fault::DirtyWrite);
    assert!(s.put(&obj(5000, 3000)).is_err(), "the put must fail when the dirty list cannot be written");
    assert_eq!(fs::read(&path).unwrap(), before, "the arena was written before the dirty list");
    assert_eq!(reuse::load_dirty(&d.path().join("packs")), Some(Default::default()));
    // The next put goes through and lists the arena.
    let id = s.put(&obj(5000, 3000)).unwrap();
    assert!(reuse::load_dirty(&d.path().join("packs")).unwrap().contains(&s.loc(&id).unwrap().arena));
    s.sync().unwrap();
    drop(s);
    let s = ReuseStore::open(d.path(), opts(AllocKind::NextFit, 64)).unwrap();
    check_all(&s, &a);
    assert!(s.contains(&id));
}

#[test]
fn rule_c_a_failed_checkpoint_keeps_the_arena_dirty() {
    let d = tempfile::tempdir().unwrap();
    let packs = d.path().join("packs");
    let s = ReuseStore::open(d.path(), opts(AllocKind::NextFit, 64)).unwrap();
    let a = put_n(&s, 0, 20);
    s.close().unwrap();
    let b = put_n(&s, 100, 5); // the arena goes into the dirty list
    let num = s.loc(&b[0].0).unwrap().arena;
    assert!(reuse::load_dirty(&packs).unwrap().contains(&num));
    sys::arm(Fault::CheckpointWrite);
    assert!(s.close().is_err());
    assert!(reuse::load_dirty(&packs).unwrap().contains(&num), "RULE C");
    drop(s);
    // A torn checkpoint file on top (as if the rename were not atomic).
    fs::write(arena::idx_path(&packs, num), b"SFIX garbage").unwrap();
    fs::write(packs.join("dirty.tmp"), b"half").unwrap();
    let (s, rep) = ReuseStore::open_report(d.path(), opts(AllocKind::NextFit, 64)).unwrap();
    assert!(rep.scanned >= 1);
    check_all(&s, &a);
    check_all(&s, &b);
    assert!(!packs.join("dirty.tmp").exists());
}

#[test]
fn a_damaged_dirty_list_means_every_arena_is_scanned() {
    let d = tempfile::tempdir().unwrap();
    let packs = d.path().join("packs");
    let s = ReuseStore::open(d.path(), opts(AllocKind::NextFit, 64)).unwrap();
    let a = put_n(&s, 0, 200);
    s.close().unwrap();
    let b = put_n(&s, 1000, 5);
    s.sync().unwrap();
    drop(s);
    fs::write(reuse::dirty_path(&packs), b"SFDL\x01\x00").unwrap();
    let (s, rep) = ReuseStore::open_report(d.path(), opts(AllocKind::NextFit, 64)).unwrap();
    assert_eq!(rep.scanned, rep.arenas);
    check_all(&s, &a);
    check_all(&s, &b);
    // And a missing one.
    s.close().unwrap();
    drop(s);
    fs::remove_file(reuse::dirty_path(&packs)).unwrap();
    let (s, rep) = ReuseStore::open_report(d.path(), opts(AllocKind::NextFit, 64)).unwrap();
    assert_eq!(rep.scanned, rep.arenas);
    check_all(&s, &a);
}

#[test]
fn an_object_put_or_read_during_gc_is_kept() {
    for k in KINDS {
        let d = tempfile::tempdir().unwrap();
        let s = ReuseStore::open(d.path(), opts(k, 64)).unwrap();
        let a = put_n(&s, 0, 100);
        s.sync().unwrap();
        let t = s.gc_begin().unwrap();
        assert!(matches!(t, GcToken::Seq(_)));
        let x = obj(9999, 4000);
        let xid = s.put(&x).unwrap(); // put during GC
        assert_eq!(s.get(&a[3].0).as_deref(), Some(&a[3].1[..])); // read during GC
        assert_eq!(s.put(&a[4].1).unwrap(), a[4].0); // dedup put during GC
        let st = s.gc_finish(&HashSet::new(), t).unwrap();
        assert_eq!(st.removed_objects, 98, "{k:?}");
        assert_eq!(s.get(&xid).as_deref(), Some(&x[..]));
        assert_eq!(s.get(&a[3].0).as_deref(), Some(&a[3].1[..]));
        assert_eq!(s.get(&a[4].0).as_deref(), Some(&a[4].1[..]));
        assert!(!s.contains(&a[5].0));
        s.sync().unwrap();
        drop(s);
        let s = ReuseStore::open(d.path(), opts(k, 64)).unwrap();
        assert_eq!(s.get(&xid).as_deref(), Some(&x[..]));
    }
}

#[test]
fn after_gc_abort_nothing_is_freed() {
    let d = tempfile::tempdir().unwrap();
    let s = ReuseStore::open(d.path(), opts(AllocKind::NextFit, 64)).unwrap();
    let a = put_n(&s, 0, 100);
    s.sync().unwrap();
    let free0 = s.alloc_stats(0).free_bytes;
    let t = s.gc_begin().unwrap();
    s.gc_abort();
    assert_eq!(s.alloc_stats(0).free_bytes, free0);
    check_all(&s, &a);
    // A later GC with a fresh token works.
    let live: HashSet<ObjectId> = a[50..].iter().map(|x| x.0).collect();
    let t2 = s.gc_begin().unwrap();
    assert!(t2 != t || matches!(t2, GcToken::Seq(_)));
    s.gc_finish(&live, t2).unwrap();
    assert!(s.alloc_stats(0).free_bytes > free0);
    check_all(&s, &a[50..]);
}

/// The case that matters for dedup: a freed object whose space was reused,
/// then a crash before any checkpoint. Its stale bytes must not come back,
/// and a re-put must store it again.
#[test]
fn a_freed_and_overwritten_object_does_not_come_back_after_a_crash() {
    for k in KINDS {
        let d = tempfile::tempdir().unwrap();
        let s = ReuseStore::open(d.path(), opts(k, 64)).unwrap();
        let a = put_n(&s, 0, 60);
        s.close().unwrap();
        let live: HashSet<ObjectId> = a[1..].iter().map(|x| x.0).collect();
        gc(&s, &live); // frees a[0]
        // Fill the hole with new objects (some land in a[0]'s space).
        let b = put_n(&s, 500, 30);
        s.sync().unwrap();
        drop(s); // crash
        let s = ReuseStore::open(d.path(), opts(k, 64)).unwrap();
        if s.contains(&a[0].0) {
            // Not overwritten: the old record is intact and correct.
            assert_eq!(s.get(&a[0].0).as_deref(), Some(&a[0].1[..]), "{k:?}");
        }
        assert_eq!(s.put(&a[0].1).unwrap(), a[0].0);
        s.sync().unwrap();
        assert_eq!(s.get(&a[0].0).as_deref(), Some(&a[0].1[..]), "{k:?}");
        check_all(&s, &a[1..]);
        check_all(&s, &b);
    }
}

#[test]
fn a_large_object_gets_its_own_arena_which_gc_deletes() {
    let d = tempfile::tempdir().unwrap();
    let s = ReuseStore::open(d.path(), opts(AllocKind::NextFit, 64)).unwrap();
    let big = random(3 << 20, 4);
    let id = s.put(&big).unwrap();
    let a = put_n(&s, 0, 10);
    s.sync().unwrap();
    drop(s);
    let s = ReuseStore::open(d.path(), opts(AllocKind::NextFit, 64)).unwrap();
    assert_eq!(s.get(&id).as_deref(), Some(&big[..]));
    let n0 = arena_files(d.path());
    gc(&s, &a.iter().map(|x| x.0).collect());
    assert_eq!(arena_files(d.path()), n0 - 1);
    assert!(!s.contains(&id));
    check_all(&s, &a);
}

#[test]
fn v3_evacuates_a_sparse_arena() {
    let d = tempfile::tempdir().unwrap();
    let s = ReuseStore::open(d.path(), opts(AllocKind::Hybrid, 64)).unwrap();
    let a = put_n(&s, 0, 300); // several arenas
    s.sync().unwrap();
    let first = s.loc(&a[0].0).unwrap().arena;
    // Keep 3 objects of the first arena, everything else of later arenas.
    let in_first: Vec<_> = a.iter().filter(|x| s.loc(&x.0).unwrap().arena == first).collect();
    let mut live: HashSet<ObjectId> = a.iter().filter(|x| s.loc(&x.0).unwrap().arena != first).map(|x| x.0).collect();
    for x in in_first.iter().take(3) {
        live.insert(x.0);
    }
    let st = {
        let t = s.gc_begin().unwrap();
        s.gc_finish(&live, t).unwrap()
    };
    assert!(st.packs_compacted >= 1, "{st:?}");
    assert!(!arena::arena_path(&d.path().join("packs"), first).exists());
    for x in in_first.iter().take(3) {
        assert_eq!(s.get(&x.0).as_deref(), Some(&x.1[..]));
    }
    drop(s);
    let s = ReuseStore::open(d.path(), opts(AllocKind::Hybrid, 64)).unwrap();
    for id in &live {
        assert!(s.get(id).is_some());
    }
}

/// Readers `get()` live objects while puts, GCs and reuse run: never a miss,
/// never wrong bytes.
#[test]
fn readers_during_gc_and_reuse_never_miss_a_live_object() {
    let d = tempfile::tempdir().unwrap();
    let s = Arc::new(ReuseStore::open(d.path(), opts(AllocKind::NextFit, 64)).unwrap());
    let stable = put_n(&s, 0, 200);
    s.sync().unwrap();
    let stable = Arc::new(stable);
    let stop = Arc::new(AtomicBool::new(false));
    let mut hs = Vec::new();
    for t in 0..4 {
        let (s, stable, stop) = (s.clone(), stable.clone(), stop.clone());
        hs.push(std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let (id, data) = &stable[(n as usize * 7 + t) % stable.len()];
                assert_eq!(s.get(id).as_deref(), Some(&data[..]), "a live object was missed");
                n += 1;
            }
            n
        }));
    }
    let mut churn: Vec<(ObjectId, Vec<u8>)> = Vec::new();
    for round in 0..30u64 {
        churn.extend(put_n(&s, 100_000 + round * 100, 40));
        s.sync().unwrap();
        // keep the newest half of the churn objects
        let keep_from = churn.len() / 2;
        churn.drain(..keep_from);
        let live: HashSet<ObjectId> = stable.iter().chain(churn.iter()).map(|x| x.0).collect();
        gc(&s, &live);
    }
    stop.store(true, Ordering::Relaxed);
    let reads: u64 = hs.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(reads > 0);
    check_all(&s, &churn);
}

#[test]
fn records_are_padded_to_the_granule() {
    let d = tempfile::tempdir().unwrap();
    let s = ReuseStore::open(d.path(), opts(AllocKind::NextFit, 512)).unwrap();
    let a = put_n(&s, 0, 50);
    for (id, _) in &a {
        assert_eq!((s.loc(id).unwrap().off - HEADER_PAGE) % 512, 0);
    }
}
