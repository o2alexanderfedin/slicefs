//! GC of the chunk store: pack deletion and compaction.

use chunk_store::{ChunkStore, ObjectId, Options};
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
    Options { pack_limit: 256 * 1024, ..Options::default() }
}

fn packs(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir.join("packs")).unwrap().filter(|e| {
        e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".pack")
    }).count()
}

fn put_many(s: &ChunkStore, n: u64, seed: u64) -> Vec<(ObjectId, Vec<u8>)> {
    (0..n).map(|i| { let d = random(8000, seed + i); (s.put(&d).unwrap(), d) }).collect()
}

#[test]
fn a_gc_with_nothing_live_deletes_every_pack() {
    let dir = TempDir::new().unwrap();
    let s = ChunkStore::open(dir.path(), opts()).unwrap();
    let objs = put_many(&s, 100, 1);
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&HashSet::new(), b).unwrap();
    assert_eq!(st.removed_objects, 100);
    assert_eq!(s.stats().objects, 0);
    assert!(objs.iter().all(|(id, _)| !s.contains(id)));
    assert_eq!(packs(dir.path()), 0, "no pack file left");
}

#[test]
fn live_objects_stay_readable_and_mostly_dead_packs_are_compacted() {
    let dir = TempDir::new().unwrap();
    let s = ChunkStore::open(dir.path(), opts()).unwrap();
    let objs = put_many(&s, 100, 100);
    // Keep one object in four: every pack is mostly dead.
    let live: HashSet<ObjectId> = objs.iter().step_by(4).map(|(id, _)| *id).collect();
    let before_packs = packs(dir.path());
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&live, b).unwrap();
    assert_eq!(st.removed_objects, 75);
    assert!(st.packs_compacted > 0);
    assert!(packs(dir.path()) < before_packs, "{} -> {}", before_packs, packs(dir.path()));
    for (id, d) in &objs {
        assert_eq!(s.get(id).as_deref() == Some(d.as_slice()), live.contains(id));
    }
    // And after a reopen.
    drop(s);
    let s = ChunkStore::open(dir.path(), opts()).unwrap();
    assert_eq!(s.stats().objects, 25);
    for id in &live {
        assert!(s.get(id).is_some());
    }
}

#[test]
fn an_object_put_during_the_gc_is_kept_even_if_the_mark_missed_it() {
    let dir = TempDir::new().unwrap();
    let s = ChunkStore::open(dir.path(), opts()).unwrap();
    let old = random(8000, 500);
    let id = s.put(&old).unwrap();
    let b = s.gc_begin().unwrap();
    // The mark found no use of `old`, but a writer stores the same bytes now:
    // the put returns at once (dedup), so GC must not delete the object.
    assert_eq!(s.put(&old).unwrap(), id);
    let fresh = random(8000, 501);
    let fid = s.put(&fresh).unwrap();
    s.gc_finish(&HashSet::new(), b).unwrap();
    assert_eq!(s.get(&id).unwrap(), old);
    assert_eq!(s.get(&fid).unwrap(), fresh);
}

#[test]
fn a_gc_that_keeps_everything_changes_nothing() {
    let dir = TempDir::new().unwrap();
    let s = ChunkStore::open(dir.path(), opts()).unwrap();
    let objs = put_many(&s, 100, 900);
    let live: HashSet<ObjectId> = objs.iter().map(|(id, _)| *id).collect();
    let b = s.gc_begin().unwrap();
    let st = s.gc_finish(&live, b).unwrap();
    assert_eq!(st.removed_objects, 0);
    assert_eq!(s.stats().objects, 100);
    for (id, d) in &objs {
        assert_eq!(&s.get(id).unwrap(), d);
    }
}

#[test]
fn compaction_into_an_empty_active_pack_from_open_keeps_every_live_object() {
    // Spike A found this case in its arena code: the pack that is active at
    // gc_begin is inside the GC boundary. If compaction fills it with live
    // copies, the GC must not delete it.
    let dir = TempDir::new().unwrap();
    let s = ChunkStore::open(dir.path(), opts()).unwrap();
    let objs = put_many(&s, 100, 300);
    s.close().unwrap();
    drop(s);
    // A crash just after a new pack was made: an empty file is the newest pack.
    let last = std::fs::read_dir(dir.path().join("packs")).unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".pack"))
        .max().unwrap();
    let next: u64 = last.trim_end_matches(".pack").parse::<u64>().unwrap() + 1;
    std::fs::write(dir.path().join("packs").join(format!("{next:016}.pack")), b"").unwrap();

    let s = ChunkStore::open(dir.path(), opts()).unwrap();
    // Keep 2 of 5: every old pack is less than half live, so all are
    // compacted. The copies (about 320 KB) overflow the empty active pack
    // (limit 256 KiB), so compaction seals it and starts a new one.
    let live: HashSet<ObjectId> = objs.iter().enumerate().filter(|(i, _)| i % 5 < 2).map(|(_, (id, _))| *id).collect();
    let b = s.gc_begin().unwrap();
    assert!(b >= next, "the empty active pack is inside the boundary");
    let st = s.gc_finish(&live, b).unwrap();
    assert_eq!(st.removed_objects, 60);
    assert!(st.packs_compacted > 0);
    for (id, d) in &objs {
        assert_eq!(s.get(id).as_deref() == Some(d.as_slice()), live.contains(id));
    }
    drop(s);
    let s = ChunkStore::open(dir.path(), opts()).unwrap();
    assert_eq!(s.stats().objects, 40);
    for id in &live {
        assert!(s.get(id).is_some());
    }
}
