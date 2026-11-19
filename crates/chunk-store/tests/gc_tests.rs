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

// ── GC on the run index (17-07) ──────────────────────────────────────────────

mod on_runs {
    use super::random;
    use chunk_store::mem::MemIo;
    use chunk_store::{ChunkStore, GcStats, ObjectId, Options, Stats};
    use std::collections::HashSet;
    use std::sync::Arc;

    type Obj = (ObjectId, Vec<u8>);

    fn opts(memtable_limit: usize, pack_limit: u64) -> Options {
        Options { pack_limit, index_memtable_limit: memtable_limit, index_background_merge: false, ..Options::default() }
    }

    fn open(mem: &MemIo, o: &Options) -> ChunkStore {
        ChunkStore::open_with(Arc::new(mem.clone()), o.clone()).unwrap()
    }

    fn put_n(s: &ChunkStore, n: u64, size: usize, seed: u64) -> Vec<Obj> {
        (0..n).map(|i| { let d = random(size, seed + i); (s.put(&d).unwrap(), d) }).collect()
    }

    /// 300 objects of 1,000 bytes in 64 KiB packs, keep 1 of 3, with the
    /// memtable limit `limit`: the GC result, the stats after it, and the
    /// runs that the GC added.
    fn gc_keep_one_of_three(limit: usize) -> (GcStats, Stats, u64) {
        let mem = MemIo::new();
        let s = open(&mem, &opts(limit, 64 << 10));
        let objs = put_n(&s, 300, 1000, 42);
        s.sync().unwrap();
        let live: HashSet<ObjectId> = objs.iter().step_by(3).map(|(id, _)| *id).collect();
        let runs_before = s.index_info().runs;
        let b = s.gc_begin().unwrap();
        let st = s.gc_finish(&live, b).unwrap();
        let runs_added = s.index_info().runs.saturating_sub(runs_before);
        for (id, d) in &objs {
            assert_eq!(s.get(id).as_deref() == Some(d.as_slice()), live.contains(id), "limit {limit}");
        }
        let stats = s.stats();
        drop(s);
        let s = open(&mem, &opts(limit, 64 << 10));
        assert_eq!(s.stats().objects, stats.objects, "limit {limit}: after a reopen");
        for (id, d) in &objs {
            assert_eq!(s.get(id).as_deref() == Some(d.as_slice()), live.contains(id), "limit {limit}: after a reopen");
        }
        (st, stats, runs_added)
    }

    /// A GC over many more packs than `GC_GROUP_PACKS`, with a victim in
    /// every pack (four objects in each pack, keep 1 of 4: every pack is
    /// compacted). Each
    /// pending GC run keeps a handle (a file descriptor with real files)
    /// until it is installed and merged. The handles open at one time must
    /// stay at the pack readers plus about one group, and must not grow
    /// with the number of packs: else a GC over more packs than the limit of
    /// open files ends with EMFILE and never completes (17-08).
    #[test]
    fn a_gc_over_many_packs_keeps_a_bounded_number_of_handles_open() {
        let mem = MemIo::new();
        let o = opts(1 << 18, 4096);
        let s = open(&mem, &o);
        let objs = put_n(&s, 600, 1000, 900);
        s.sync().unwrap();
        let packs = mem.list().unwrap().len();
        assert!(packs > 3 * chunk_store::GC_GROUP_PACKS, "{packs} packs");
        let live: HashSet<ObjectId> = objs.iter().step_by(4).map(|(id, _)| *id).collect();
        let b = s.gc_begin().unwrap();
        mem.reset_peak_handles();
        let base = mem.open_handles();
        let st = s.gc_finish(&live, b).unwrap();
        let peak = mem.peak_handles();
        let bound = base + chunk_store::MAX_OPEN_PACKS + chunk_store::GC_GROUP_PACKS + 8;
        assert!(peak <= bound, "{peak} handles open at one time (bound {bound}) in a GC over {packs} packs");
        assert_eq!(st.removed_objects, 450, "{st:?}");
        assert_eq!(st.kept_objects, 150, "{st:?}");
        assert_eq!(st.packs_compacted as usize, packs, "{st:?}");
        for (id, d) in &objs {
            assert_eq!(s.get(id).as_deref() == Some(d.as_slice()), live.contains(id));
        }
        assert_eq!(s.stats().objects, 150);
        drop(s);
        let s = open(&mem, &o);
        assert_eq!(s.stats().objects, 150, "after a reopen");
        for (id, d) in &objs {
            assert_eq!(s.get(id).as_deref() == Some(d.as_slice()), live.contains(id), "after a reopen");
        }
    }

    #[test]
    fn many_packs_gc_with_a_small_memtable() {
        let (st_small, stats_small, runs_small) = gc_keep_one_of_three(16);
        let (st_big, stats_big, _) = gc_keep_one_of_three(1 << 18);
        assert!(st_small.packs_compacted >= 4, "{st_small:?}");
        assert_eq!(st_small.removed_objects, 200, "{st_small:?}");
        assert_eq!(st_small, st_big);
        assert_eq!(stats_small, stats_big);
        // 100 copies with a limit of 16: the copies flush in the middle of
        // the GC, so the GC adds more than one run.
        assert!(runs_small >= 2, "{runs_small} runs added");
    }

    /// Three packs of four 8,000-byte objects (pack limit 30,000), every
    /// entry in a run before the GC (memtable limit 4). The live set makes
    /// one pack of each kind: nothing live (deleted), 1 of 4 live
    /// (compacted), 3 of 4 live (punched).
    struct Three {
        mem: MemIo,
        o: Options,
        objs: Vec<Obj>,
        live: HashSet<ObjectId>,
    }

    impl Three {
        fn new() -> (Self, ChunkStore) {
            let mem = MemIo::new();
            let o = opts(4, 30_000);
            let s = open(&mem, &o);
            let objs = put_n(&s, 12, 8000, 7_000);
            s.sync().unwrap();
            assert_eq!(mem.list().unwrap().len(), 3, "three sealed packs");
            assert_eq!(s.index_info().memtable_entries, 0, "every entry is in a run");
            // Pack 1: none live; pack 2: object 4 live; pack 3: object 9 dead.
            let live = [4, 8, 10, 11].iter().map(|&i| objs[i].0).collect();
            (Three { mem, o, objs, live }, s)
        }

        fn removed(&self) -> Vec<&Obj> {
            self.objs.iter().filter(|(id, _)| !self.live.contains(id)).collect()
        }

        fn gc(&self, s: &ChunkStore) -> GcStats {
            let b = s.gc_begin().unwrap();
            self.finish(s, b)
        }

        fn finish(&self, s: &ChunkStore, b: u64) -> GcStats {
            let st = s.gc_finish(&self.live, b).unwrap();
            assert_eq!((st.packs_deleted, st.packs_compacted, st.packs_punched), (1, 1, 1), "{st:?}");
            assert_eq!(st.removed_objects, 8, "{st:?}");
            st
        }

        /// Removed objects are missing (except `back`, put again), live
        /// objects read back.
        fn check(&self, s: &ChunkStore, back: &[ObjectId], what: &str) {
            for (i, (id, d)) in self.objs.iter().enumerate() {
                if self.live.contains(id) || back.contains(id) {
                    assert_eq!(s.get(id).as_ref(), Some(d), "{what}: object {i}");
                    assert!(s.contains(id), "{what}: object {i}");
                } else {
                    assert!(!s.contains(id), "{what}: removed object {i}");
                    assert!(s.get(id).is_none(), "{what}: removed object {i}");
                    assert!(s.stored_len(id).is_none(), "{what}: removed object {i}");
                }
            }
        }

        fn pack_bytes(&self) -> usize {
            self.mem.list().unwrap().into_iter().map(|n| self.mem.len(n, Kind::Pack).unwrap()).sum()
        }

        /// Put `obj` again: the put writes it (the pack bytes grow), it reads
        /// back, and the object count grows by one.
        fn put_again(&self, s: &ChunkStore, (id, d): &Obj) {
            let (bytes, objects) = (self.pack_bytes(), s.stats().objects);
            assert_eq!(s.put(d).unwrap(), *id);
            assert!(self.pack_bytes() > bytes, "the put of a removed object writes it");
            assert_eq!(s.get(id).as_ref(), Some(d));
            assert_eq!(s.stats().objects, objects + 1);
        }

        fn reopen(&self, s: ChunkStore) -> ChunkStore {
            s.sync().unwrap();
            drop(s);
            open(&self.mem, &self.o)
        }
    }

    use chunk_store::pack_io::{Kind, PackIo};

    #[test]
    fn removed_objects_stay_missing_for_each_kind_of_pack() {
        let (t, s) = Three::new();
        t.gc(&s);
        t.check(&s, &[], "after the GC");
        // One removed object of each pack (0: deleted, 1: compacted,
        // 9: punched) is put again; the others stay missing.
        let back: Vec<ObjectId> = [0, 1, 9].iter().map(|&i| t.objs[i].0).collect();
        for &i in &[0, 1, 9] {
            t.put_again(&s, &t.objs[i]);
        }
        t.check(&s, &back, "after the new puts");
        assert_eq!(s.stats().objects, 4 + 3);
        let s = t.reopen(s);
        t.check(&s, &back, "after a reopen");
        assert_eq!(s.stats().objects, 4 + 3);
    }

    #[test]
    fn a_merge_between_gc_begin_and_gc_finish() {
        let (t, s) = Three::new();
        assert!(s.index_info().runs >= 2);
        let b = s.gc_begin().unwrap();
        assert!(s.merge_index_all().unwrap());
        assert_eq!(s.index_info().runs, 1);
        t.finish(&s, b);
        t.check(&s, &[], "after the GC");
        assert_eq!(s.stats().objects, 4);
        let s = t.reopen(s);
        t.check(&s, &[], "after a reopen");
        assert_eq!(s.stats().objects, 4);
        for (id, d) in t.removed() {
            assert_eq!(s.put(d).unwrap(), *id);
            assert_eq!(s.get(id).as_ref(), Some(d));
        }
        assert_eq!(s.stats().objects, 12);
    }

    #[test]
    fn a_merge_after_gc_drops_the_tombstones_and_removed_objects_stay_missing() {
        let (t, s) = Three::new();
        t.gc(&s);
        assert!(s.index_info().run_tombstones > 0);
        assert!(s.merge_index_all().unwrap());
        let info = s.index_info();
        assert_eq!((info.runs, info.run_tombstones), (1, 0), "{info:?}");
        t.check(&s, &[], "after the merge");
        let s = t.reopen(s);
        assert_eq!(s.index_info().run_tombstones, 0);
        t.check(&s, &[], "after a reopen");
        let (id9, _) = &t.objs[9];
        t.put_again(&s, &t.objs[9]);
        t.check(&s, &[*id9], "after a new put");
    }

    // The last flush ends inside the last pack (hwm = that pack and an
    // offset), and the GC deletes that pack with no copy (the memtable is
    // empty at the install). The install must still move the hwm past it:
    // else the next open finds the hwm pack missing and rebuilds the index
    // from the packs, instead of using the tombstone runs.
    #[test]
    fn a_gc_with_an_empty_memtable_moves_the_hwm_past_the_changed_packs() {
        let mem = MemIo::new();
        let o = opts(30, 64 << 10);
        let s = open(&mem, &o);
        let objs = put_n(&s, 300, 1000, 9_000);
        s.sync().unwrap();
        let info = s.index_info();
        assert_eq!(info.memtable_entries, 0, "{info:?}");
        assert!(info.hwm_offset > 0, "the last flush is inside a pack: {info:?}");
        let b = s.gc_begin().unwrap();
        assert_eq!(s.gc_finish(&HashSet::new(), b).unwrap().removed_objects, 300);
        let info = s.index_info();
        assert!(info.hwm_pack > b, "the hwm is past every changed pack: {info:?}");
        let mut runs = mem.list_runs().unwrap();
        runs.sort_unstable();
        drop(s);
        let s = open(&mem, &o);
        let mut again = mem.list_runs().unwrap();
        again.sort_unstable();
        assert_eq!(again, runs, "the open uses the runs (no rebuild)");
        for (id, _) in &objs {
            assert!(!s.contains(id));
        }
        assert_eq!(s.stats().objects, 0);
    }

    #[test]
    fn the_put_dedup_check_of_a_removed_object_writes_it() {
        let (t, s) = Three::new();
        t.gc(&s);
        // Every removed entry is behind a tombstone in a run (nothing in the
        // memtable): the dedup check reads the runs and finds it.
        assert_eq!(s.index_info().memtable_entries, 0);
        for obj in t.removed() {
            t.put_again(&s, obj);
        }
        assert_eq!(s.stats().objects, 12);
        let s = t.reopen(s);
        for (id, d) in &t.objs {
            assert_eq!(s.get(id).as_ref(), Some(d));
        }
    }
}
