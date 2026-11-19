//! The store on its storage strategies: the same contract on real files and
//! on the in-memory mock, then faults that only the mock can make on demand.

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
    Options { pack_limit: 256 * 1024, index_background_merge: false, ..Options::default() }
}

/// Run `f` once on files and once on the mock. `f` gets a function that
/// opens the store again on the same storage.
fn on_each_io(f: impl Fn(&dyn Fn() -> ChunkStore)) {
    let dir = TempDir::new().unwrap();
    let files: Arc<dyn PackIo> = Arc::new(FileIo::new(dir.path().join("packs")));
    f(&|| ChunkStore::open_with(Arc::clone(&files), opts()).unwrap());
    let mem: Arc<dyn PackIo> = Arc::new(MemIo::new());
    f(&|| ChunkStore::open_with(Arc::clone(&mem), opts()).unwrap());
}

fn put_many(s: &ChunkStore, n: u64, seed: u64) -> Vec<(ObjectId, Vec<u8>)> {
    (0..n).map(|i| { let d = random(8000, seed + i); (s.put(&d).unwrap(), d) }).collect()
}

// ── Contract: the same on every strategy ──────────────────────────────────────

#[test]
fn objects_survive_close_and_reopen() {
    on_each_io(|open| {
        let s = open();
        let objs = put_many(&s, 100, 1);
        s.close().unwrap();
        drop(s);
        let s = open();
        assert_eq!(s.stats().objects, 100);
        for (id, d) in &objs {
            assert_eq!(&s.get(id).unwrap(), d);
        }
    });
}

#[test]
fn objects_synced_without_close_survive_reopen() {
    on_each_io(|open| {
        let s = open();
        let objs = put_many(&s, 50, 2);
        s.sync().unwrap();
        drop(s);
        let s = open();
        for (id, d) in &objs {
            assert_eq!(&s.get(id).unwrap(), d);
        }
    });
}

#[test]
fn gc_keeps_the_live_set_and_frees_the_rest() {
    on_each_io(|open| {
        let s = open();
        let objs = put_many(&s, 100, 3);
        let live: HashSet<ObjectId> = objs.iter().step_by(3).map(|(id, _)| *id).collect();
        let b = s.gc_begin().unwrap();
        let st = s.gc_finish(&live, b).unwrap();
        assert_eq!(st.removed_objects, 100 - live.len() as u64);
        drop(s);
        let s = open();
        for (id, d) in &objs {
            assert_eq!(s.get(id).as_deref() == Some(d.as_slice()), live.contains(id));
        }
    });
}

// ── Faults (mock only) ────────────────────────────────────────────────────────

fn mem_store(mem: &MemIo) -> ChunkStore {
    ChunkStore::open_with(Arc::new(mem.clone()), opts()).unwrap()
}

#[test]
fn a_crash_keeps_every_synced_object_and_no_wrong_data() {
    let mem = MemIo::new();
    let s = mem_store(&mem);
    let synced = put_many(&s, 40, 10);
    s.sync().unwrap();
    let unsynced = put_many(&s, 40, 50);
    drop(s);
    mem.crash();
    let s = mem_store(&mem);
    for (id, d) in &synced {
        assert_eq!(&s.get(id).unwrap(), d);
    }
    for (id, d) in &unsynced {
        if let Some(got) = s.get(id) {
            assert_eq!(&got, d, "an unsynced object may be lost, but never wrong");
        }
    }
}

#[test]
fn a_damaged_record_at_the_tail_is_cut_and_the_rest_stays() {
    let mem = MemIo::new();
    let s = mem_store(&mem);
    let objs = put_many(&s, 10, 20);
    s.sync().unwrap();
    drop(s);
    // The newest pack has no index: damage its last byte (the last CRC).
    let num = (1..100).rev().find(|n| mem.len(*n, Kind::Pack).is_some()).unwrap();
    let len = mem.len(num, Kind::Pack).unwrap();
    mem.flip(num, Kind::Pack, len - 1);
    let s = mem_store(&mem);
    let (last_id, _) = objs.last().unwrap();
    assert!(s.get(last_id).is_none());
    for (id, d) in &objs[..objs.len() - 1] {
        assert_eq!(&s.get(id).unwrap(), d);
    }
}

#[test]
fn a_full_disk_fails_the_put_and_the_store_works_again_after() {
    let mem = MemIo::new();
    let s = mem_store(&mem);
    let before = put_many(&s, 20, 30);
    s.sync().unwrap();
    mem.set_space_left(Some(100));
    assert!(s.put(&random(8000, 999)).is_err(), "ENOSPC must reach the caller");
    mem.set_space_left(None);
    let after = put_many(&s, 20, 60);
    s.sync().unwrap();
    drop(s);
    mem.crash();
    let s = mem_store(&mem);
    for (id, d) in before.iter().chain(&after) {
        assert_eq!(&s.get(id).unwrap(), d);
    }
}

#[test]
fn a_read_error_gives_no_data_not_wrong_data() {
    let mem = MemIo::new();
    let s = mem_store(&mem);
    let (id, d) = put_many(&s, 1, 40).pop().unwrap();
    mem.set_fail_reads(true);
    assert!(s.get(&id).is_none());
    assert!(s.contains(&id), "the object is still in the store");
    mem.set_fail_reads(false);
    assert_eq!(s.get(&id).unwrap(), d);
}

#[test]
fn after_a_failed_sync_no_later_sync_reports_success() {
    // Linux can mark pages clean after a failed fsync, so a retry can
    // succeed without the data. The store must not report it durable.
    let mem = MemIo::new();
    let s = mem_store(&mem);
    put_many(&s, 5, 70);
    mem.set_fail_syncs(true);
    assert!(s.sync().is_err());
    mem.set_fail_syncs(false);
    assert!(s.sync().is_err(), "a retry after a failed sync must fail too");
    assert!(s.put(&random(8000, 171)).is_err(), "no new writes after a failed sync");
    drop(s);
    // A reopen finds the true durable state.
    let s = mem_store(&mem);
    s.put(&random(8000, 172)).unwrap();
    s.sync().unwrap();
}

#[test]
fn a_gc_that_fails_before_its_copies_are_durable_loses_nothing() {
    let mem = MemIo::new();
    let s = mem_store(&mem);
    let objs = put_many(&s, 100, 80);
    s.sync().unwrap();
    let live: HashSet<ObjectId> = objs.iter().step_by(4).map(|(id, _)| *id).collect();
    let b = s.gc_begin().unwrap();
    mem.set_fail_syncs(true);
    assert!(s.gc_finish(&live, b).is_err());
    drop(s);
    mem.set_fail_syncs(false);
    mem.crash();
    let s = mem_store(&mem);
    for id in &live {
        let d = &objs.iter().find(|(i, _)| i == id).unwrap().1;
        assert_eq!(&s.get(id).unwrap(), d, "a live object was lost");
    }
}

#[test]
fn reads_from_many_packs_keep_few_files_open() {
    // Spike B: with about 2,000 packs and a limit of 256 open files, reads
    // failed with EMFILE, because the store kept one open file per pack.
    let mem = MemIo::new();
    let small = Options { pack_limit: 16 * 1024, ..Options::default() };
    let s = ChunkStore::open_with(Arc::new(mem.clone()), small.clone()).unwrap();
    let objs = put_many(&s, 300, 90); // about 2 objects per pack: 150 packs
    s.close().unwrap();
    drop(s);
    let s = ChunkStore::open_with(Arc::new(mem.clone()), small).unwrap();
    for (id, d) in &objs {
        assert_eq!(&s.get(id).unwrap(), d);
    }
    assert!(mem.open_handles() <= chunk_store::MAX_OPEN_PACKS, "{} pack handles open", mem.open_handles());
    // And the cache still serves every pack again.
    for (id, d) in objs.iter().rev() {
        assert_eq!(&s.get(id).unwrap(), d);
    }
    assert!(mem.open_handles() <= chunk_store::MAX_OPEN_PACKS);
}
