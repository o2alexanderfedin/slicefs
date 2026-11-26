//! The full GC (`gc_begin_full`, `gc_finish_full`): an exact mark in sorted
//! temp files and no holes, so it frees every dead object, also the false
//! positives of a filter mark and the dead records that punching left.

use chunk_store::mem::MemIo;
use chunk_store::pack_io::{Kind, PackIo};
use chunk_store::{ChunkStore, LiveFilter, ObjectId, Options};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
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

type Obj = (ObjectId, Vec<u8>);

fn opts(tmp: &Path) -> Options {
    Options {
        pack_limit: 128 * 1024,
        zstd_level: 0,
        index_background_merge: false,
        gc_tmp_dir: Some(tmp.to_path_buf()),
        ..Options::default()
    }
}

fn open(mem: &MemIo, o: &Options) -> ChunkStore {
    ChunkStore::open_with(Arc::new(mem.clone()), o.clone()).unwrap()
}

/// `n` objects of 300 to 2,300 bytes (small ones fit in a 4 KiB block with
/// live neighbours, so punching leaves them).
fn fill(s: &ChunkStore, n: u64, seed: u64) -> Vec<Obj> {
    (0..n).map(|i| { let d = random(300 + (i as usize * 997) % 2000, seed + i); (s.put(&d).unwrap(), d) }).collect()
}

fn full_gc(s: &ChunkStore, live: &HashSet<ObjectId>) -> chunk_store::GcStats {
    let b = s.gc_begin_full().unwrap();
    let mut m = s.exact_mark().unwrap();
    for id in live {
        m.insert(id).unwrap();
    }
    s.gc_finish_full(m, b).unwrap()
}

/// Every live object reads back; no dead object is left; the packs hold
/// only the headers and the live records (no dead byte).
fn exactly_live(mem: &MemIo, s: &ChunkStore, objs: &[Obj], live: &HashSet<ObjectId>, what: &str) {
    for (id, d) in objs {
        if live.contains(id) {
            assert_eq!(s.get(id).as_deref(), Some(d.as_slice()), "{what}: live {id:?} lost");
        } else {
            assert!(!s.contains(id), "{what}: dead {id:?} left");
        }
    }
    assert_eq!(s.stats().objects, live.len() as u64, "{what}");
    let packs = mem.list().unwrap();
    let bytes: u64 = packs.iter().map(|&n| mem.len(n, Kind::Pack).unwrap() as u64).sum();
    let want: u64 = packs.len() as u64 * 14 + live.iter().map(|id| 33 + 9 + 4 + s.stored_len(id).unwrap() as u64).sum::<u64>();
    assert_eq!(bytes, want, "{what}: dead bytes in the packs");
}

#[test]
fn a_full_gc_frees_what_a_filter_gc_and_punching_left() {
    let tmp = TempDir::new().unwrap();
    let o = opts(tmp.path());
    let mem = MemIo::new();
    let s = open(&mem, &o);
    let objs = fill(&s, 600, 1);
    // Runs of 4 dead among 6 live: the packs are mostly live, so the normal
    // GC punches the whole blocks of the runs and leaves the rest.
    let live: HashSet<ObjectId> = objs.iter().enumerate().filter(|(i, _)| i % 10 >= 4).map(|(_, (id, _))| *id).collect();
    // A small filter (4 bits per key) with a fixed salt: many false
    // positives.
    let mut f = LiveFilter::with_seed(live.len() as u64, 4, 42);
    live.iter().for_each(|id| f.insert(id));
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&f, b).unwrap();
    assert!(st.packs_punched > 0, "{st:?}");
    let dead_left = objs.iter().filter(|(id, _)| !live.contains(id) && s.contains(id)).count();
    assert!(dead_left > 0, "the normal GC left no dead object: {st:?}");

    let st = full_gc(&s, &live);
    assert_eq!(st.dead_left, Some(0), "{st:?}");
    assert_eq!(st.packs_punched, 0, "{st:?}");
    assert_eq!(st.removed_objects, dead_left as u64, "{st:?}");
    exactly_live(&mem, &s, &objs, &live, "after the full GC");
    assert_eq!(std::fs::read_dir(tmp.path()).map(|d| d.count()).unwrap_or(0), 0, "temp files left");
    s.sync().unwrap();
    drop(s);
    mem.crash();
    let s = open(&mem, &o);
    exactly_live(&mem, &s, &objs, &live, "after a reopen");
}

/// An exact normal GC punches mostly live packs: the index then lists no
/// dead object, but the holes and the parts of the removed records outside
/// whole blocks stay in the packs. A full GC compacts those packs too.
#[test]
fn a_full_gc_compacts_punched_packs_with_no_dead_entry_left() {
    let tmp = TempDir::new().unwrap();
    let o = opts(tmp.path());
    let mem = MemIo::new();
    let s = open(&mem, &o);
    // Chunk-sized objects (8,000 bytes), runs of 3 dead among 7 live.
    let objs: Vec<Obj> = (0..120).map(|i| { let d = random(8000, 500 + i); (s.put(&d).unwrap(), d) }).collect();
    let live: HashSet<ObjectId> = objs.iter().enumerate().filter(|(i, _)| i % 10 >= 3).map(|(_, (id, _))| *id).collect();
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&live, b).unwrap();
    assert!(st.packs_punched > 0, "{st:?}");
    let left: Vec<usize> = objs.iter().enumerate().filter(|(_, (id, _))| !live.contains(id) && s.contains(id)).map(|(i, _)| i).collect();
    // At most one dead record (at the end of a pack) has no whole block.
    assert!(left.len() <= 1, "the exact normal GC left dead objects {left:?}: {st:?}");

    let st = full_gc(&s, &live);
    assert_eq!(st.removed_objects, left.len() as u64, "{st:?}");
    // Also the punched packs with no dead entry.
    assert!(st.packs_compacted >= 5, "{st:?}");
    exactly_live(&mem, &s, &objs, &live, "punched packs");
}

#[test]
fn tiny_sort_chunks_force_several_merge_passes() {
    let tmp = TempDir::new().unwrap();
    // 8 IDs a chunk, 2 chunks a merge: about 40 chunks, 6 passes.
    let o = Options { gc_sort_chunk_ids: 8, gc_merge_fan_in: 2, ..opts(tmp.path()) };
    let mem = MemIo::new();
    let s = open(&mem, &o);
    let objs = fill(&s, 500, 2);
    let live: HashSet<ObjectId> = objs.iter().step_by(3).map(|(id, _)| *id).collect();
    let st = full_gc(&s, &live);
    assert!(st.mark_merge_passes >= 2, "{st:?}");
    assert_eq!(st.dead_left, Some(0));
    exactly_live(&mem, &s, &objs, &live, "tiny chunks");
    assert_eq!(std::fs::read_dir(tmp.path()).map(|d| d.count()).unwrap_or(0), 0, "temp files left");
}

#[test]
fn objects_put_or_read_during_a_full_gc_survive() {
    let tmp = TempDir::new().unwrap();
    // A tiny chunk: the touched set before gc_finish_full spills to disk.
    let o = Options { gc_sort_chunk_ids: 4, gc_merge_fan_in: 3, ..opts(tmp.path()) };
    let mem = MemIo::new();
    let s = open(&mem, &o);
    let objs = fill(&s, 300, 3);
    let live: HashSet<ObjectId> = objs.iter().step_by(5).map(|(id, _)| *id).collect();
    let b = s.gc_begin_full().unwrap();
    // A reader takes 40 dead objects, a writer puts a new one, during the
    // mark.
    let read: Vec<&Obj> = objs.iter().filter(|(id, _)| !live.contains(id)).take(40).collect();
    for (id, _) in &read {
        s.read(id).unwrap();
    }
    let t = random(5000, 999);
    let tid = s.put(&t).unwrap();
    let mut m = s.exact_mark().unwrap();
    live.iter().for_each(|id| m.insert(id).unwrap());
    let st = s.gc_finish_full(m, b).unwrap();
    assert_eq!(st.dead_left, Some(0), "{st:?}");
    let mut kept = live.clone();
    kept.extend(read.iter().map(|(id, _)| *id));
    kept.insert(tid);
    let mut all = objs.clone();
    all.push((tid, t));
    exactly_live(&mem, &s, &all, &kept, "touched");
}

#[test]
fn a_full_gc_that_fails_loses_nothing_and_leaves_no_temp_files() {
    for after in [1, 4, 10, 25] {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        let mem = MemIo::new();
        let s = open(&mem, &o);
        let objs = fill(&s, 300, 10 + after as u64);
        s.sync().unwrap();
        let live: HashSet<ObjectId> = objs.iter().step_by(4).map(|(id, _)| *id).collect();
        let b = s.gc_begin_full().unwrap();
        let mut m = s.exact_mark().unwrap();
        live.iter().for_each(|id| m.insert(id).unwrap());
        mem.fail_reads_after(after);
        let r = s.gc_finish_full(m, b);
        mem.clear_fail_reads_after();
        assert!(r.is_err(), "fails after {after} reads");
        assert_eq!(std::fs::read_dir(tmp.path()).map(|d| d.count()).unwrap_or(0), 0, "temp files left after an error");
        for (id, d) in objs.iter().filter(|(id, _)| live.contains(id)) {
            assert_eq!(s.get(id).as_deref(), Some(d.as_slice()), "after {after}: live {id:?} lost");
        }
        // A crash, then a full GC again: exact.
        drop(s);
        mem.crash();
        let s = open(&mem, &o);
        full_gc(&s, &live);
        exactly_live(&mem, &s, &objs, &live, &format!("retry after a failure after {after} reads"));
    }
}

#[test]
fn a_writable_open_removes_the_temp_files_of_a_crash() {
    let dir = TempDir::new().unwrap();
    let tmp = dir.path().join("gc-tmp");
    std::fs::create_dir_all(tmp.join("123-0")).unwrap();
    std::fs::write(tmp.join("123-0").join("live-000001"), [0u8; 66]).unwrap();
    let ro = Options { read_only: true, ..Options::default() };
    drop(ChunkStore::open(dir.path(), ro).unwrap());
    assert!(tmp.join("123-0").exists(), "a read-only open changes nothing");
    let s = ChunkStore::open(dir.path(), Options::default()).unwrap();
    assert!(!tmp.exists(), "the writable open removes the temp files");
    // And a full GC on the files works, with the default temp dir.
    let objs = fill(&s, 50, 7);
    let live: HashSet<ObjectId> = objs.iter().step_by(2).map(|(id, _)| *id).collect();
    let st = full_gc(&s, &live);
    assert_eq!(st.dead_left, Some(0));
    assert_eq!(st.removed_objects, 25);
    assert!(objs.iter().all(|(id, _)| live.contains(id) == s.contains(id)));
    assert_eq!(std::fs::read_dir(&tmp).map(|d| d.count()).unwrap_or(0), 0);
}

#[test]
fn a_full_gc_without_a_temp_dir_fails_before_it_changes_anything() {
    let mem = MemIo::new();
    let s = ChunkStore::open_with(Arc::new(mem.clone()), Options::default()).unwrap();
    s.put(b"x").unwrap();
    assert!(s.gc_begin_full().is_err());
    assert!(s.exact_mark().is_err());
}
