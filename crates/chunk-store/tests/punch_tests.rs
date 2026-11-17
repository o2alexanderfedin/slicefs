//! GC hole punching: a mostly live pack frees the blocks of its dead gaps.

use chunk_store::mem::MemIo;
use chunk_store::pack_io::{FileIo, Kind, PackIo};
use chunk_store::{ChunkStore, ObjectId, Options};
use std::collections::HashSet;
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

fn opts() -> Options {
    Options { pack_limit: 256 * 1024, ..Options::default() }
}

fn put_many(s: &ChunkStore, n: u64, seed: u64) -> Vec<(ObjectId, Vec<u8>)> {
    (0..n).map(|i| { let d = random(8000, seed + i); (s.put(&d).unwrap(), d) }).collect()
}

/// Every pack 3/4 live: GC punches, it does not compact.
fn three_of_four(objs: &[(ObjectId, Vec<u8>)]) -> HashSet<ObjectId> {
    objs.iter().enumerate().filter(|(i, _)| i % 4 != 0).map(|(_, (id, _))| *id).collect()
}

fn check(s: &ChunkStore, objs: &[(ObjectId, Vec<u8>)], live: &HashSet<ObjectId>) {
    for (id, d) in objs {
        let got = s.get(id);
        if live.contains(id) {
            assert_eq!(got.as_deref(), Some(d.as_slice()), "live object lost");
        } else {
            assert!(got.is_none() || got.as_deref() == Some(d.as_slice()), "wrong data");
        }
    }
}

/// Files and the mock; the bool says whether the strategy can punch for sure.
fn on_each_io(f: impl Fn(&dyn Fn() -> ChunkStore, &dyn PackIo, bool)) {
    let dir = TempDir::new().unwrap();
    let files: Arc<dyn PackIo> = Arc::new(FileIo::new(dir.path().join("packs")));
    // APFS can punch holes; other file systems (Docker overlays) may not.
    f(&|| ChunkStore::open_with(Arc::clone(&files), opts()).unwrap(), &*files, cfg!(target_os = "macos"));
    let mem: Arc<dyn PackIo> = Arc::new(MemIo::new());
    f(&|| ChunkStore::open_with(Arc::clone(&mem), opts()).unwrap(), &*mem, true);
}

#[test]
fn a_mostly_live_pack_is_punched_and_the_live_objects_stay() {
    on_each_io(|open, _, punches| {
        let s = open();
        let objs = put_many(&s, 120, 1);
        let live = three_of_four(&objs);
        let b = s.gc_begin().unwrap();
        let st = s.gc_finish(&live, b).unwrap();
        assert_eq!(st.packs_compacted, 0, "3/4 live packs are not compacted");
        assert_eq!(st.packs_deleted, 0);
        if punches {
            assert!(st.packs_punched > 0 && st.punched_bytes > 0, "{st:?}");
            // A dead record that does not hold a whole block stays (a later
            // GC punches it when a neighbour dies too).
            assert!((25..=30).contains(&st.removed_objects), "{st:?}");
        }
        check(&s, &objs, &live);
        drop(s);
        let s = open();
        check(&s, &objs, &live);
        assert_eq!(s.stats().objects as usize, 120 - st.removed_objects as usize);
    });
}

#[test]
fn a_punched_pack_without_its_index_is_resynced_not_cut() {
    on_each_io(|open, io, _| {
        let s = open();
        let objs = put_many(&s, 120, 2);
        let live = three_of_four(&objs);
        let b = s.gc_begin().unwrap();
        s.gc_finish(&live, b).unwrap();
        drop(s);
        // Lose every index, also of the newest pack: none of them may be cut.
        for n in io.list().unwrap() {
            let _ = io.remove(n, Kind::Idx);
        }
        let s = open();
        check(&s, &objs, &live);
        // The store still takes new objects (into a new pack).
        let more = put_many(&s, 10, 500);
        s.sync().unwrap();
        drop(s);
        let s = open();
        check(&s, &objs, &live);
        for (id, d) in &more {
            assert_eq!(&s.get(id).unwrap(), d);
        }
    });
}

#[test]
fn a_dead_object_put_again_after_the_punch_is_stored_again() {
    on_each_io(|open, _, _| {
        let s = open();
        let objs = put_many(&s, 120, 3);
        let live = three_of_four(&objs);
        let b = s.gc_begin().unwrap();
        s.gc_finish(&live, b).unwrap();
        // The bytes of every dead object, again: they must not dedup onto zeros.
        for (id, d) in objs.iter().filter(|(id, _)| !live.contains(id)) {
            assert_eq!(s.put(d).unwrap(), *id);
            assert_eq!(&s.get(id).unwrap(), d);
        }
        s.sync().unwrap();
        drop(s);
        let s = open();
        for (id, d) in &objs {
            assert_eq!(&s.get(id).unwrap(), d);
        }
    });
}

#[test]
fn punch_twice_is_safe() {
    on_each_io(|open, _, _| {
        let s = open();
        let objs = put_many(&s, 120, 4);
        let live = three_of_four(&objs);
        for _ in 0..2 {
            let b = s.gc_begin().unwrap();
            s.gc_finish(&live, b).unwrap();
        }
        check(&s, &objs, &live);
        drop(s);
        let s = open();
        check(&s, &objs, &live);
    });
}

// ── Faults (mock only) ────────────────────────────────────────────────────────

fn mem_store(mem: &MemIo) -> ChunkStore {
    ChunkStore::open_with(Arc::new(mem.clone()), opts()).unwrap()
}

#[test]
fn a_failed_punch_loses_no_live_object() {
    let mem = MemIo::new();
    let s = mem_store(&mem);
    let objs = put_many(&s, 120, 5);
    s.sync().unwrap();
    let live = three_of_four(&objs);
    let b = s.gc_begin().unwrap();
    mem.set_fail_punch(true);
    assert!(s.gc_finish(&live, b).is_err());
    drop(s);
    mem.set_fail_punch(false);
    mem.crash();
    let s = mem_store(&mem);
    check(&s, &objs, &live);
}

#[test]
fn a_crash_after_the_punch_loses_no_live_object() {
    let mem = MemIo::new();
    let s = mem_store(&mem);
    let objs = put_many(&s, 120, 6);
    let live = three_of_four(&objs);
    let b = s.gc_begin().unwrap();
    s.gc_finish(&live, b).unwrap();
    drop(s);
    mem.crash();
    let s = mem_store(&mem);
    check(&s, &objs, &live);
}

#[test]
fn a_failed_index_write_before_the_punch_punches_nothing() {
    let mem = MemIo::new();
    let s = mem_store(&mem);
    let objs = put_many(&s, 120, 7);
    let live = three_of_four(&objs);
    let b = s.gc_begin().unwrap();
    mem.set_fail_syncs(true);
    assert!(s.gc_finish(&live, b).is_err());
    drop(s);
    mem.set_fail_syncs(false);
    mem.crash();
    // Nothing was punched, so every object (dead ones too) is still there.
    let s = mem_store(&mem);
    for (id, d) in &objs {
        assert_eq!(&s.get(id).unwrap(), d);
    }
}

#[test]
fn without_hole_support_the_dead_records_only_leave_the_index() {
    let mem = MemIo::new();
    mem.set_no_punch(true);
    let s = mem_store(&mem);
    let objs = put_many(&s, 120, 8);
    let live = three_of_four(&objs);
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&live, b).unwrap();
    assert_eq!(st.packs_punched, 0);
    assert!((25..=30).contains(&st.removed_objects), "{st:?}");
    check(&s, &objs, &live);
    drop(s);
    let s = mem_store(&mem);
    check(&s, &objs, &live);
}

#[cfg(target_os = "macos")]
#[test]
fn on_apfs_the_punch_frees_disk_blocks() {
    use std::os::unix::fs::MetadataExt;
    let dir = TempDir::new().unwrap();
    let blocks = || -> u64 {
        std::fs::read_dir(dir.path().join("packs")).unwrap()
            .map(|e| e.unwrap().metadata().unwrap().blocks())
            .sum()
    };
    let s = ChunkStore::open(dir.path(), opts()).unwrap();
    let objs = put_many(&s, 120, 9);
    s.close().unwrap();
    let before = blocks();
    let live = three_of_four(&objs);
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&live, b).unwrap();
    let after = blocks();
    // st_blocks counts 512-byte units.
    assert!(before - after >= st.punched_bytes / 512 / 2, "before {before}, after {after}, {st:?}");
    check(&s, &objs, &live);
}
