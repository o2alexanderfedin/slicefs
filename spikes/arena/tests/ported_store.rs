//! Correctness gate of spike A (plan section 4): the `chunk-store` tests
//! `store_tests`, `gc_tests` and `file_tests` ported to `SpikeStore`, each run
//! for V0-V3 at an arena size of 1 MiB.
//!
//! Changes against the originals, and why:
//! - Object counts are larger, so the data crosses several 1 MiB arenas
//!   (the originals use 64 KiB / 256 KiB packs).
//! - The ported tests use today's rules (`SealRule::Always`, `close_seals`).
//!   The new rules (size/8 seal, resumable idx) have their own tests below.
//! - Torn tail and flipped byte find the record with `store.loc(id)`: in a
//!   fixed-size arena the end of the file is zero padding, not the record.
//!   A torn record in a fixed arena is modelled by zeroing its last bytes
//!   (the file length does not change); V0 keeps the original truncation.

use arena_spike::codec::rec_len;
use arena_spike::file_fork::{file_objects, FileReader, FileWriter, ObjStore, LEAF_ENTRIES};
use arena_spike::{chunk_ranges, ObjectId, Options, Prealloc, SealRule, SpikeStore};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::FileExt;
use std::path::Path;
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

/// Today's rules (as `chunk-store`), 1 MiB arenas.
fn today(p: Prealloc) -> Options {
    let mut o = Options::with_prealloc(p);
    o.arena.size = MIB;
    o.gc_seal = SealRule::Always;
    o.close_seals = true;
    o
}

/// The new rules: seal at gc_begin only at size/8; close writes a resumable idx.
fn new_rules(p: Prealloc) -> Options {
    let mut o = Options::with_prealloc(p);
    o.arena.size = MIB;
    o
}

fn files(dir: &Path, ext: &str) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir.join("packs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(ext))
        .collect();
    v.sort();
    v
}

fn arena_file(dir: &Path, num: u64) -> std::path::PathBuf {
    dir.join("packs").join(format!("{num:016x}.arena"))
}

macro_rules! per_variant {
    ($($name:ident),* $(,)?) => {
        mod v0 { $( #[test] fn $name() { super::$name(arena_spike::Prealloc::Grow) } )* }
        mod v1 { $( #[test] fn $name() { super::$name(arena_spike::Prealloc::Sparse) } )* }
        mod v2 { $( #[test] fn $name() { super::$name(arena_spike::Prealloc::Reserve) } )* }
        mod v3 { $( #[test] fn $name() { super::$name(arena_spike::Prealloc::Zeroed) } )* }
    };
}

per_variant!(
    put_then_get_returns_the_bytes,
    the_same_bytes_are_stored_once,
    compressible_data_is_compressed_and_random_data_is_not,
    objects_survive_a_reopen_after_sync,
    a_sealed_pack_without_its_index_is_scanned_and_indexed,
    a_corrupt_index_is_ignored_and_rebuilt,
    a_torn_tail_of_the_active_pack_is_cut_and_the_rest_kept,
    a_flipped_byte_in_a_record_is_detected,
    a_flipped_byte_in_a_record_is_detected_with_an_idx,
    a_gc_with_nothing_live_deletes_every_pack,
    live_objects_stay_readable_and_mostly_dead_packs_are_compacted,
    an_object_put_during_the_gc_is_kept_even_if_the_mark_missed_it,
    a_gc_that_keeps_everything_changes_nothing,
    reads_return_the_written_bytes_at_any_offset,
    a_large_file_uses_a_manifest_tree,
    a_missing_object_is_an_error_not_wrong_data,
    an_object_larger_than_an_arena_gets_its_own_arena,
    eighth_rule_compaction_into_the_unsealed_active_arena_keeps_live_data,
    eighth_rule_gc_begin_does_not_seal_a_small_active_arena,
    close_writes_a_resumable_idx_and_appends_continue_in_the_same_arena,
);

// ── store_tests ──────────────────────────────────────────────────────────────

fn put_then_get_returns_the_bytes(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let store = SpikeStore::open(dir.path(), today(p)).unwrap();
    let data = b"hello chunk store".to_vec();
    let id = store.put(&data).unwrap();
    assert_eq!(id, ObjectId::of(&data));
    assert_eq!(store.get(&id).unwrap(), data);
    assert!(store.contains(&id));
    assert!(store.get(&ObjectId::of(b"other")).is_none());
}

fn the_same_bytes_are_stored_once(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let store = SpikeStore::open(dir.path(), today(p)).unwrap();
    let data = random(8192, 1);
    store.put(&data).unwrap();
    let before = store.stats();
    store.put(&data).unwrap();
    assert_eq!(store.stats(), before, "a second put of the same bytes writes nothing");
    assert_eq!(before.objects, 1);
}

fn compressible_data_is_compressed_and_random_data_is_not(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let store = SpikeStore::open(dir.path(), today(p)).unwrap();
    let text = b"#include <boost/config.hpp>\n".repeat(300);
    let noise = random(8192, 2);
    let t = store.put(&text).unwrap();
    let n = store.put(&noise).unwrap();
    let st = store.stats();
    assert!(st.stored_bytes < (text.len() + noise.len()) as u64);
    assert!(store.stored_len(&t).unwrap() < text.len() as u32 / 8);
    assert_eq!(store.stored_len(&n).unwrap(), noise.len() as u32, "no gain: kept raw");
    assert_eq!(store.get(&t).unwrap(), text);
    assert_eq!(store.get(&n).unwrap(), noise);
}

fn objects_survive_a_reopen_after_sync(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let ids: Vec<(ObjectId, Vec<u8>)>;
    {
        let store = SpikeStore::open(dir.path(), today(p)).unwrap();
        ids = (0..600).map(|i| {
            let d = random(4000, i + 10);
            (store.put(&d).unwrap(), d)
        }).collect();
        store.sync().unwrap();
    }
    assert!(files(dir.path(), ".idx").len() >= 2, "1 MiB arenas: several sealed");
    let store = SpikeStore::open(dir.path(), today(p)).unwrap();
    for (id, d) in &ids {
        assert_eq!(&store.get(id).unwrap(), d);
    }
    assert_eq!(store.stats().objects, 600);
}

fn a_sealed_pack_without_its_index_is_scanned_and_indexed(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let ids: Vec<ObjectId>;
    {
        let store = SpikeStore::open(dir.path(), today(p)).unwrap();
        ids = (0..600).map(|i| store.put(&random(4000, i + 100)).unwrap()).collect();
        store.close().unwrap();
    }
    assert!(files(dir.path(), ".idx").len() >= 2);
    for idx in files(dir.path(), ".idx") {
        fs::remove_file(dir.path().join("packs").join(idx)).unwrap();
    }
    let store = SpikeStore::open(dir.path(), today(p)).unwrap();
    assert!(ids.iter().all(|id| store.contains(id)));
    assert!(ids.iter().all(|id| store.get(id).is_some()));
    drop(store);
    assert!(!files(dir.path(), ".idx").is_empty(), "the index files are written again");
}

fn a_corrupt_index_is_ignored_and_rebuilt(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let ids: Vec<ObjectId>;
    {
        let store = SpikeStore::open(dir.path(), today(p)).unwrap();
        ids = (0..600).map(|i| store.put(&random(4000, i + 200)).unwrap()).collect();
        store.close().unwrap();
    }
    let idx = dir.path().join("packs").join(&files(dir.path(), ".idx")[0]);
    let mut b = fs::read(&idx).unwrap();
    let mid = b.len() / 2;
    b[mid] ^= 0xff;
    fs::write(&idx, b).unwrap();
    let store = SpikeStore::open(dir.path(), today(p)).unwrap();
    assert!(ids.iter().all(|id| store.contains(id)));
    assert!(ids.iter().all(|id| store.get(id).is_some()));
}

/// Tear the record of `id`: V0 drops its last 10 bytes (the original test);
/// a fixed arena zeroes them (the length does not change).
fn tear(dir: &Path, store: &SpikeStore, id: &ObjectId, p: Prealloc) {
    let l = store.loc(id).unwrap();
    let path = arena_file(dir, l.arena);
    let end = l.off + rec_len(l.stored) as u64;
    let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
    if p == Prealloc::Grow {
        assert_eq!(fs::metadata(&path).unwrap().len(), end);
        f.set_len(end - 10).unwrap();
    } else {
        f.write_all_at(&[0u8; 10], end - 10).unwrap();
    }
}

fn a_torn_tail_of_the_active_pack_is_cut_and_the_rest_kept(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let (a, b) = (random(3000, 300), random(3000, 301));
    let (ia, ib);
    {
        let store = SpikeStore::open(dir.path(), today(p)).unwrap();
        ia = store.put(&a).unwrap();
        store.sync().unwrap();
        ib = store.put(&b).unwrap();
        store.sync().unwrap();
        tear(dir.path(), &store, &ib, p);
    }
    let store = SpikeStore::open(dir.path(), today(p)).unwrap();
    assert_eq!(store.get(&ia).unwrap(), a);
    assert!(!store.contains(&ib), "the torn record is gone");
    if p.fixed() {
        assert_eq!(fs::metadata(arena_file(dir.path(), 1)).unwrap().len(), MIB, "fixed length again");
    }
    // New writes go after the cut and read back.
    let ib2 = store.put(&b).unwrap();
    store.sync().unwrap();
    drop(store);
    let store = SpikeStore::open(dir.path(), today(p)).unwrap();
    assert_eq!(store.get(&ib2).unwrap(), b);
    assert_eq!(store.get(&ia).unwrap(), a);
}

fn flip_in_record(dir: &Path, store: &SpikeStore, id: &ObjectId) {
    let l = store.loc(id).unwrap();
    let path = arena_file(dir, l.arena);
    let mut b = fs::read(&path).unwrap();
    let at = (l.off + rec_len(l.stored) as u64 - 100) as usize;
    b[at] ^= 1;
    fs::write(&path, b).unwrap();
}

/// No idx: the recovery walk stops at the damaged record and cuts it.
fn a_flipped_byte_in_a_record_is_detected(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let d = random(3000, 400);
    let id;
    {
        let store = SpikeStore::open(dir.path(), today(p)).unwrap();
        id = store.put(&d).unwrap();
        store.sync().unwrap();
        flip_in_record(dir.path(), &store, &id);
    }
    let store = SpikeStore::open(dir.path(), today(p)).unwrap();
    assert!(store.get(&id).is_none(), "a damaged record is not returned as data");
}

/// A sealed idx: the record is indexed, the CRC check at read time catches it.
fn a_flipped_byte_in_a_record_is_detected_with_an_idx(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let d = random(3000, 401);
    let id;
    {
        let store = SpikeStore::open(dir.path(), today(p)).unwrap();
        id = store.put(&d).unwrap();
        store.close().unwrap();
        flip_in_record(dir.path(), &store, &id);
    }
    assert_eq!(files(dir.path(), ".idx").len(), 1);
    let store = SpikeStore::open(dir.path(), today(p)).unwrap();
    assert!(store.contains(&id), "the idx still lists it");
    assert!(store.get(&id).is_none(), "a damaged record is not returned as data");
}

// ── gc_tests ─────────────────────────────────────────────────────────────────

fn put_many(s: &SpikeStore, n: u64, seed: u64) -> Vec<(ObjectId, Vec<u8>)> {
    (0..n).map(|i| {
        let d = random(8000, seed + i);
        (s.put(&d).unwrap(), d)
    }).collect()
}

fn a_gc_with_nothing_live_deletes_every_pack(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let s = SpikeStore::open(dir.path(), today(p)).unwrap();
    let objs = put_many(&s, 400, 1);
    assert!(files(dir.path(), ".arena").len() >= 3);
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&HashSet::new(), b).unwrap();
    assert_eq!(st.removed_objects, 400);
    assert_eq!(s.stats().objects, 0);
    assert!(objs.iter().all(|(id, _)| !s.contains(id)));
    assert_eq!(files(dir.path(), ".arena").len(), 0, "no arena file left");
}

fn live_objects_stay_readable_and_mostly_dead_packs_are_compacted(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let s = SpikeStore::open(dir.path(), today(p)).unwrap();
    let objs = put_many(&s, 400, 100);
    let live: HashSet<ObjectId> = objs.iter().step_by(4).map(|(id, _)| *id).collect();
    let before = files(dir.path(), ".arena").len();
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&live, b).unwrap();
    assert_eq!(st.removed_objects, 300);
    assert!(st.packs_compacted > 0);
    let after = files(dir.path(), ".arena").len();
    assert!(after < before, "{before} -> {after}");
    for (id, d) in &objs {
        assert_eq!(s.get(id).as_deref() == Some(d.as_slice()), live.contains(id));
    }
    drop(s);
    let s = SpikeStore::open(dir.path(), today(p)).unwrap();
    assert_eq!(s.stats().objects, 100);
    for id in &live {
        assert!(s.get(id).is_some());
    }
}

fn an_object_put_during_the_gc_is_kept_even_if_the_mark_missed_it(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let s = SpikeStore::open(dir.path(), today(p)).unwrap();
    let old = random(8000, 500);
    let id = s.put(&old).unwrap();
    let b = s.gc_begin().unwrap();
    assert_eq!(s.put(&old).unwrap(), id);
    let fresh = random(8000, 501);
    let fid = s.put(&fresh).unwrap();
    s.gc_finish(&HashSet::new(), b).unwrap();
    assert_eq!(s.get(&id).unwrap(), old);
    assert_eq!(s.get(&fid).unwrap(), fresh);
}

fn a_gc_that_keeps_everything_changes_nothing(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let s = SpikeStore::open(dir.path(), today(p)).unwrap();
    let objs = put_many(&s, 400, 900);
    let live: HashSet<ObjectId> = objs.iter().map(|(id, _)| *id).collect();
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&live, b).unwrap();
    assert_eq!(st.removed_objects, 0);
    assert_eq!(s.stats().objects, 400);
    for (id, d) in &objs {
        assert_eq!(&s.get(id).unwrap(), d);
    }
}

// ── file_tests (through file_fork) ───────────────────────────────────────────

fn write_in_pieces(s: &dyn ObjStore, data: &[u8], piece: usize) -> ObjectId {
    let mut w = FileWriter::new();
    for p in data.chunks(piece) {
        w.write(s, p).unwrap();
    }
    assert_eq!(w.len(), data.len() as u64);
    w.finish(s).unwrap()
}

fn reads_return_the_written_bytes_at_any_offset(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let s = SpikeStore::open(dir.path(), today(p)).unwrap();
    let data = random(1 << 20, 2);
    let key = write_in_pieces(&s, &data, 65536);
    let mut r = FileReader::open(&s, key).unwrap();
    assert_eq!(r.len(), data.len() as u64);
    for &(off, n) in &[(0usize, 10usize), (8000, 20000), (1 << 19, 131072), ((1 << 20) - 5, 5), (12345, 1)] {
        assert_eq!(r.read(&s, off as u64, n).unwrap(), &data[off..off + n], "at {off}+{n}");
    }
    assert_eq!(r.read(&s, (1 << 20) - 3, 100).unwrap(), &data[(1 << 20) - 3..], "short at EOF");
    assert!(r.read(&s, 1 << 20, 10).unwrap().is_empty(), "nothing after EOF");
}

fn a_large_file_uses_a_manifest_tree(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let s = SpikeStore::open(dir.path(), today(p)).unwrap();
    let data = random(12 << 20, 3);
    let key = write_in_pieces(&s, &data, 1 << 20);
    let objs = file_objects(&s, key).unwrap();
    let nodes = objs.len() - chunk_ranges(&data).len();
    assert!(nodes >= 3, "a root and at least two leaves, got {nodes} nodes");
    assert!(LEAF_ENTRIES <= 1024);
    let mut r = FileReader::open(&s, key).unwrap();
    let mid = 6 << 20;
    assert_eq!(r.read(&s, mid as u64, 300_000).unwrap(), &data[mid..mid + 300_000]);
    assert_eq!(r.read(&s, 0, data.len()).unwrap(), data);
    assert!(files(dir.path(), ".arena").len() >= 12, "12 MiB in 1 MiB arenas");
}

fn a_missing_object_is_an_error_not_wrong_data(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let s = SpikeStore::open(dir.path(), today(p)).unwrap();
    let other = TempDir::new().unwrap();
    let s2 = SpikeStore::open(other.path(), today(p)).unwrap();
    let key = write_in_pieces(&s2, &random(100_000, 6), 100_000);
    assert!(FileReader::open(&s, key).is_err());
}

// ── New for spike A ──────────────────────────────────────────────────────────

fn an_object_larger_than_an_arena_gets_its_own_arena(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let big = random(3 * MIB as usize / 2, 700);
    let small = random(5000, 701);
    let (ib, is);
    {
        let s = SpikeStore::open(dir.path(), today(p)).unwrap();
        is = s.put(&small).unwrap();
        ib = s.put(&big).unwrap();
        assert_eq!(s.get(&ib).unwrap(), big);
        s.sync().unwrap();
    }
    let s = SpikeStore::open(dir.path(), today(p)).unwrap();
    assert_eq!(s.get(&ib).unwrap(), big);
    assert_eq!(s.get(&is).unwrap(), small);
}

/// Data-loss check for the size/8 seal rule: the active arena is small at
/// gc_begin, so it stays active with a boundary that includes it; the
/// compacted copies fill and seal it. It must not be deleted afterwards.
fn eighth_rule_compaction_into_the_unsealed_active_arena_keeps_live_data(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let s = SpikeStore::open(dir.path(), new_rules(p)).unwrap();
    let mut objs = put_many(&s, 750, 3000);
    // Put until a new arena has just started (holds less than size/8).
    let mut i = 0;
    loop {
        if let Some((_, hwm)) = s.active() {
            if hwm - 4096 < MIB / 8 && hwm > 4096 {
                break;
            }
        }
        let d = random(8000, 9000 + i);
        objs.push((s.put(&d).unwrap(), d));
        i += 1;
    }
    let (start_active, _) = s.active().unwrap();
    let live: HashSet<ObjectId> = objs.iter().step_by(4).map(|(id, _)| *id).collect();
    let b = s.gc_begin().unwrap();
    assert_eq!(s.active().unwrap().0, start_active, "not sealed: under size/8");
    let st = s.gc_finish(&live, b).unwrap();
    assert!(st.packs_compacted > 0);
    assert!(s.active().is_none_or(|a| a.0 > start_active), "compaction filled the start arena");
    for (id, d) in &objs {
        if live.contains(id) {
            assert_eq!(s.get(id).as_deref(), Some(d.as_slice()), "live object lost");
        }
    }
    s.close().unwrap();
    drop(s);
    let s = SpikeStore::open(dir.path(), new_rules(p)).unwrap();
    for (id, d) in &objs {
        if live.contains(id) {
            assert_eq!(s.get(id).as_deref(), Some(d.as_slice()), "live object lost after reopen");
        }
    }
}

fn eighth_rule_gc_begin_does_not_seal_a_small_active_arena(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let s = SpikeStore::open(dir.path(), new_rules(p)).unwrap();
    let objs = put_many(&s, 5, 50);
    let (a0, _) = s.active().unwrap();
    for _ in 0..20 {
        let t = s.gc_begin().unwrap();
        let live = objs.iter().map(|(id, _)| *id).collect();
        s.gc_finish(&live, t).unwrap();
    }
    assert_eq!(s.active().unwrap().0, a0);
    assert_eq!(files(dir.path(), ".arena").len(), 1);
    // Above size/8 it seals.
    let more = put_many(&s, 20, 60);
    let t = s.gc_begin().unwrap();
    assert!(s.active().is_none());
    let live = objs.iter().chain(&more).map(|(id, _)| *id).collect();
    s.gc_finish(&live, t).unwrap();
    for (id, d) in objs.iter().chain(&more) {
        assert_eq!(&s.get(id).unwrap(), d);
    }
}

fn close_writes_a_resumable_idx_and_appends_continue_in_the_same_arena(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let mut all = Vec::new();
    for c in 0..5u64 {
        let s = SpikeStore::open(dir.path(), new_rules(p)).unwrap();
        for (id, d) in &all {
            assert_eq!(&s.get(id).unwrap(), d);
        }
        all.extend(put_many(&s, 10, 100 * c));
        s.close().unwrap();
    }
    assert_eq!(files(dir.path(), ".arena").len(), 1, "5 close/open cycles share one arena");
    let idx = arena_spike::arena::load_idx(&dir.path().join("packs"), 1).unwrap();
    assert_eq!(idx.state, arena_spike::codec::IdxState::Open);
    assert_eq!(idx.entries.len(), 50);
    // Records after the idx's hwm (a crash after close + more puts) are found.
    {
        let s = SpikeStore::open(dir.path(), new_rules(p)).unwrap();
        all.extend(put_many(&s, 10, 7000));
        s.sync().unwrap();
    }
    let s = SpikeStore::open(dir.path(), new_rules(p)).unwrap();
    assert_eq!(s.stats().objects, 60);
    for (id, d) in &all {
        assert_eq!(&s.get(id).unwrap(), d);
    }
}
