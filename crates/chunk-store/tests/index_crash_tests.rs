//! IDX-03 (#85): a crash at each step of a run flush loses no synced object
//! and gives no wrong Loc. Each test injects a fault in one step of the
//! flush (or of the replay that rebuilds an index), then drops the store,
//! calls `MemIo::crash()` (only durable bytes stay) and reopens. After the
//! reopen: every synced object reads back byte-exact; an unsynced object
//! reads back byte-exact or is missing (never wrong); `stats().objects` is
//! the number of objects that read back; and no orphan run is left. The
//! merge crash tests are in 17-06 and the GC crash tests in 17-07.
//! In-process on MemIo; no mounts.

use chunk_store::mem::MemIo;
use chunk_store::pack_io::{Kind, PackIo};
use chunk_store::{ChunkStore, ObjectId, Options};
use std::collections::HashSet;
use std::sync::Arc;

type Obj = (ObjectId, Vec<u8>);

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

/// 64 KiB packs (about 120 objects of 500 bytes) and a run every 32 entries.
fn opts() -> Options {
    // A fixed run list and fault knobs that only the test uses: no
    // background merges.
    Options { pack_limit: 64 << 10, index_memtable_limit: 32, index_background_merge: false, ..Options::default() }
}

fn store(mem: &MemIo, o: &Options) -> ChunkStore {
    ChunkStore::open_with(Arc::new(mem.clone()), o.clone()).unwrap()
}

fn put_many(s: &ChunkStore, n: u64, seed: u64) -> Vec<Obj> {
    (0..n)
        .map(|i| {
            let d = random(500, seed + i);
            (s.put(&d).unwrap(), d)
        })
        .collect()
}

fn run_list(mem: &MemIo) -> Vec<u64> {
    let mut r = mem.list_runs().unwrap();
    r.sort_unstable();
    r
}

/// Reopen after the crash and check the four rules (see the module doc).
/// `committed`: runs that a durable MANIFEST listed before the fault. They
/// must still be there: a rebuild would hide an ordering bug.
fn reopen_and_check(mem: &MemIo, o: &Options, synced: &[Obj], unsynced: &[Obj], committed: &[u64]) -> ChunkStore {
    let s = store(mem, o);
    for (i, (id, d)) in synced.iter().enumerate() {
        assert_eq!(s.get(id).as_ref(), Some(d), "synced object {i}");
    }
    let mut found = synced.len() as u64;
    for (i, (id, d)) in unsynced.iter().enumerate() {
        match s.get(id) {
            Some(got) => {
                assert_eq!(&got, d, "unsynced object {i}: lost is allowed, wrong is not");
                found += 1;
            }
            None => assert!(!s.contains(id), "unsynced object {i}: no Loc for a lost object"),
        }
    }
    assert_eq!(s.stats().objects, found, "the stats count what reads back");
    let runs = run_list(mem);
    assert_eq!(runs.len() as u64, s.index_info().runs, "no orphan run: {runs:?}");
    for r in committed {
        assert!(runs.contains(r), "committed run {r} is gone (a rebuild?): {runs:?}");
    }
    s
}

/// 40 synced objects (one flush: one run, the hwm inside pack 1), then 23
/// more, so the memtable holds 31 entries and the next put flushes.
fn before_a_flush(mem: &MemIo, o: &Options) -> (ChunkStore, Vec<Obj>, Vec<Obj>, Vec<u64>) {
    let s = store(mem, o);
    let synced = put_many(&s, 40, 1);
    s.sync().unwrap();
    let committed = run_list(mem);
    assert_eq!(committed.len(), 1);
    let more = put_many(&s, 23, 100);
    assert_eq!(s.index_info().memtable_entries, 31);
    (s, synced, more, committed)
}

/// The object whose put flushes.
fn flushing_object() -> Vec<u8> {
    random(500, 9999)
}

/// The store is failed: no more writes or syncs until a reopen.
fn assert_failed(s: &ChunkStore) {
    assert!(s.put(&random(500, 31_337)).is_err(), "the store is failed");
    assert!(s.sync().is_err(), "the store is failed");
}

#[test]
fn a_failed_active_pack_sync_before_a_flush() {
    let mem = MemIo::new();
    let o = opts();
    let (s, synced, mut unsynced, committed) = before_a_flush(&mem, &o);
    let manifest = mem.read(0, Kind::Manifest).unwrap();
    let d = flushing_object();
    let id = s.hasher().id(&d);
    mem.fail_next_syncs(1); // the sync of the active pack, first step of the flush
    assert!(s.put(&d).is_err());
    assert_eq!(run_list(&mem), committed, "no run was written");
    assert_eq!(mem.read(0, Kind::Manifest).unwrap(), manifest);
    assert_failed(&s);
    unsynced.push((id, d));
    drop(s);
    mem.crash();
    reopen_and_check(&mem, &o, &synced, &unsynced, &committed);
}

#[test]
fn a_failed_run_sync() {
    let mem = MemIo::new();
    let o = opts();
    let (s, mut synced, more, committed) = before_a_flush(&mem, &o);
    let manifest = mem.read(0, Kind::Manifest).unwrap();
    let d = flushing_object();
    let id = s.hasher().id(&d);
    // The active pack sync passes; the next sync, of the run file, fails.
    mem.fail_sync_after(1);
    assert!(s.put(&d).is_err());
    assert_eq!(run_list(&mem), committed, "the partial run is removed");
    assert_eq!(mem.read(0, Kind::Manifest).unwrap(), manifest);
    assert_failed(&s);
    drop(s);
    mem.crash();
    // The pack sync passed, so the 24 newer objects are durable too.
    synced.extend(more);
    synced.push((id, d));
    let s = reopen_and_check(&mem, &o, &synced, &[], &committed);
    assert_eq!(s.index_info().memtable_entries, 32, "replayed above the hwm");
}

#[test]
fn a_crash_after_the_run_file_before_the_manifest() {
    let mem = MemIo::new();
    let o = opts();
    let (s, mut synced, more, committed) = before_a_flush(&mem, &o);
    let manifest = mem.read(0, Kind::Manifest).unwrap();
    let d = flushing_object();
    let id = s.hasher().id(&d);
    mem.fail_next_write_durable(Kind::Manifest, 1);
    assert!(s.put(&d).is_err());
    let runs = run_list(&mem);
    assert_eq!(runs.len(), committed.len() + 1, "the new run file stays: {runs:?}");
    assert_eq!(mem.read(0, Kind::Manifest).unwrap(), manifest, "the MANIFEST write failed");
    assert_failed(&s);
    drop(s);
    mem.crash();
    assert_eq!(run_list(&mem), runs, "the run file and its name were durable");
    // The pack and the run were synced before the MANIFEST write.
    synced.extend(more);
    synced.push((id, d));
    let s = reopen_and_check(&mem, &o, &synced, &[], &committed);
    assert_eq!(run_list(&mem), committed, "the orphan run is removed");
    assert_eq!(s.index_info().memtable_entries, 32, "replayed above the old hwm");
}

#[test]
fn a_crash_after_the_manifest() {
    let mem = MemIo::new();
    let o = opts();
    let (s, mut synced, more, _) = before_a_flush(&mem, &o);
    let d = flushing_object();
    let id = s.hasher().id(&d);
    s.put(&d).unwrap();
    assert_eq!(s.index_info().memtable_entries, 0, "the put flushed");
    let committed = run_list(&mem);
    assert_eq!(committed.len(), 2);
    // No sync after the flush: the flush itself made the pack durable up to
    // the hwm before the run pointed to it.
    let after = random(500, 7777);
    let after_id = s.put(&after).unwrap();
    drop(s);
    mem.crash();
    synced.extend(more);
    synced.push((id, d));
    let s = reopen_and_check(&mem, &o, &synced, &[(after_id, after.clone())], &committed);
    let info = s.index_info();
    assert_eq!(info.memtable_entries, 0, "no record below the hwm is replayed: {info:?}");
    assert_eq!(info.runs, 2);

    // Once more with a sync: the one record above the hwm is replayed.
    assert_eq!(s.put(&after).unwrap(), after_id);
    s.sync().unwrap();
    drop(s);
    mem.crash();
    synced.push((after_id, after));
    let s = reopen_and_check(&mem, &o, &synced, &[], &committed);
    assert_eq!(s.index_info().memtable_entries, 1, "only the record above the hwm is replayed");
}

#[test]
fn a_power_loss_with_unsynced_puts_after_a_flush() {
    let mem = MemIo::new();
    let o = opts();
    let s = store(&mem, &o);
    let synced = put_many(&s, 100, 1);
    s.sync().unwrap();
    let committed = run_list(&mem);
    assert_eq!(committed.len(), 3, "three flushes");
    let unsynced = put_many(&s, 20, 500);
    assert_eq!(run_list(&mem), committed, "no flush in the unsynced puts");
    drop(s);
    mem.crash();
    let s = reopen_and_check(&mem, &o, &synced, &unsynced, &committed);
    // The store goes on: a lost object is written again.
    for (id, d) in &unsynced {
        assert_eq!(s.put(d).unwrap(), *id);
        assert_eq!(s.get(id).as_ref(), Some(d));
    }
    assert_eq!(s.stats().objects, 120);
}

#[test]
fn a_crash_during_the_replay_of_a_rebuild() {
    let mem = MemIo::new();
    let s = store(&mem, &Options { pack_limit: 64 << 10, index_background_merge: false, ..Options::default() });
    let objs = put_many(&s, 600, 1);
    s.close().unwrap();
    drop(s);
    assert!(!mem.exists(0, Kind::Manifest), "no MANIFEST: the next open replays every pack");
    let packs = mem.list().unwrap().len();
    assert!(packs >= 5, "{packs} packs");

    let o = Options { index_memtable_limit: 64, ..opts() };
    // The first replay flush commits; the second MANIFEST write fails.
    mem.fail_write_durable_after(Kind::Manifest, 1);
    assert!(ChunkStore::open_with(Arc::new(mem.clone()), o.clone()).is_err());
    let committed = vec![run_list(&mem)[0]];
    assert_eq!(run_list(&mem).len(), 2, "the committed run and the orphan of the failed flush");
    mem.crash();

    let s = reopen_and_check(&mem, &o, &objs, &[], &committed);
    assert_eq!(s.stats().objects, 600);
    let info = s.index_info();
    assert_eq!(info.hwm_offset, 0, "no unreplayed pack is marked as covered: {info:?}");
    assert!(info.runs > 1, "{info:?}");
}

#[test]
fn gc_copies_are_not_counted_twice_after_a_failed_tombstone_flush() {
    let mem = MemIo::new();
    let o = Options { index_memtable_limit: 1000, ..opts() };
    let s = store(&mem, &o);
    let objs = put_many(&s, 200, 1);
    s.sync().unwrap();
    assert_eq!(s.index_info().runs, 0, "no size flush");
    let live: HashSet<ObjectId> = objs.iter().step_by(4).map(|(id, _)| *id).collect();
    let b = s.gc_begin().unwrap();
    let packs: Vec<(u64, usize)> = mem.list().unwrap().into_iter().map(|n| (n, mem.len(n, Kind::Pack).unwrap())).collect();
    assert!(packs.len() >= 2);
    mem.fail_next_write_durable(Kind::Manifest, 1);
    assert!(s.gc_finish(&live, b).is_err(), "the flush of the tombstones fails");
    for &(n, len) in &packs {
        assert_eq!(mem.len(n, Kind::Pack), Some(len), "pack {n} is not deleted, punched or cut");
    }
    assert!(mem.list().unwrap().len() > packs.len(), "the copies are in a new pack");
    drop(s);
    mem.crash();
    let s = reopen_and_check(&mem, &o, &objs, &[], &[]);
    assert_eq!(s.stats().objects, 200, "each copy is counted once");
}

// ── Merges (17-06) ────────────────────────────────────────────────────────────

/// `n` synced objects; with `opts()` 200 objects give six runs of one tier
/// (32 entries each), so `merge_index_now` merges the four oldest.
fn before_a_merge(mem: &MemIo, o: &Options, n: u64) -> (ChunkStore, Vec<Obj>) {
    let s = store(mem, o);
    let objs = put_many(&s, n, 1);
    s.sync().unwrap();
    assert!(s.index_info().runs >= 4, "{:?}", s.index_info());
    (s, objs)
}

fn all_exact(s: &ChunkStore, objs: &[Obj], what: &str) {
    for (i, (id, d)) in objs.iter().enumerate() {
        assert_eq!(s.get(id).as_ref(), Some(d), "{what}: object {i}");
    }
}

#[test]
fn a_crash_after_the_merge_output_before_the_manifest() {
    let mem = MemIo::new();
    let o = opts();
    let (s, mut synced) = before_a_merge(&mem, &o, 200);
    let committed = run_list(&mem);
    let manifest = mem.read(0, Kind::Manifest).unwrap();
    mem.fail_next_write_durable(Kind::Manifest, 1);
    assert!(s.merge_index_now().is_err());
    assert_eq!(mem.read(0, Kind::Manifest).unwrap(), manifest);
    assert_eq!(run_list(&mem).len(), committed.len() + 1, "the output stays: its MANIFEST write may be durable");
    assert_eq!(s.index_info().runs, committed.len() as u64, "the old runs stay in use");
    // The store is not failed: puts (with a flush) and gets work.
    synced.extend(put_many(&s, 40, 500));
    all_exact(&s, &synced, "after the failed merge");
    s.sync().unwrap();
    drop(s);
    mem.crash();
    // The orphan output is removed; the MANIFEST runs are the run files.
    let s = reopen_and_check(&mem, &o, &synced, &[], &committed);
    assert_eq!(s.stats().objects, 240);
}

#[test]
fn a_crash_after_the_merge_manifest_before_the_inputs_are_removed() {
    let mem = MemIo::new();
    let o = opts();
    let (s, synced) = before_a_merge(&mem, &o, 200);
    let before = run_list(&mem);
    mem.set_fail_removes(true);
    assert!(s.merge_index_now().unwrap());
    mem.set_fail_removes(false);
    let after = run_list(&mem);
    assert_eq!(after.len(), before.len() + 1, "the input runs are still there: {after:?}");
    // The four oldest runs merged into the newest file.
    let inputs = &before[..4];
    let mut committed = before[4..].to_vec();
    committed.push(*after.last().unwrap());
    all_exact(&s, &synced, "after the merge");
    drop(s);
    mem.crash();
    let s = reopen_and_check(&mem, &o, &synced, &[], &committed);
    assert!(inputs.iter().all(|&r| !mem.exists(r, Kind::Run)), "the obsolete inputs are removed");
    assert_eq!(s.index_info().runs, before.len() as u64 - 3);
}

#[test]
fn a_read_error_during_a_merge_keeps_the_store_working() {
    let mem = MemIo::new();
    // Runs of 150 entries (three pages): the sixth page read fails inside
    // the merge, after the first page of each input.
    let o = Options { index_memtable_limit: 150, ..opts() };
    let (s, mut synced) = before_a_merge(&mem, &o, 650);
    let runs = run_list(&mem);
    mem.fail_reads_after(5);
    let e = s.merge_index_now().unwrap_err();
    mem.clear_fail_reads_after();
    assert!(matches!(e, chunk_store::Error::Io(_)), "{e}");
    assert_eq!(run_list(&mem), runs, "the partial output is removed");
    // The store is not failed, and an I/O error asks for no rebuild.
    synced.extend(put_many(&s, 160, 5000));
    all_exact(&s, &synced, "after the failed merge");
    s.sync().unwrap();
    drop(s);
    let s = reopen_and_check(&mem, &o, &synced, &[], &runs);
    assert!(s.merge_index_now().unwrap(), "the merge works after the error");
    all_exact(&s, &synced, "after the merge");
}

#[test]
fn a_drop_during_background_merges_returns() {
    let mem = MemIo::new();
    let o = Options { index_memtable_limit: 16, index_background_merge: true, ..opts() };
    let s = store(&mem, &o);
    let objs = put_many(&s, 2000, 1);
    s.sync().unwrap();
    // The drop stops and joins the merge thread, which may be merging now.
    drop(s);
    assert_eq!(mem.open_handles(), 0, "no thread keeps the packs or runs of the store open");
    mem.crash();
    let s = reopen_and_check(&mem, &opts(), &objs, &[], &[]);
    assert_eq!(s.stats().objects, 2000);
}

#[test]
fn background_merges_keep_the_run_count_small() {
    let mem = MemIo::new();
    let o = Options { index_memtable_limit: 50, index_background_merge: true, ..opts() };
    let s = store(&mem, &o);
    let objs = put_many(&s, 2000, 1);
    s.wait_index_idle();
    let tiers: Vec<u32> = s.index_run_entries().iter().map(|&e| tier(e, 50)).collect();
    assert!(tiers.windows(4).all(|w| w.iter().any(|&t| t != w[0])), "tiers {tiers:?}");
    let info = s.index_info();
    assert!(info.runs <= 9, "{info:?}");
    assert_eq!(info.run_entries, 2000, "{info:?}");
    assert_eq!(run_list(&mem).len() as u64, info.runs, "the inputs are removed");
    all_exact(&s, &objs, "after the merges");
    assert_eq!(s.stats().objects, 2000);
    // After close the thread is gone: a flush leaves work pending, and
    // wait_index_idle still returns.
    s.close().unwrap();
    let more = put_many(&s, 100, 9000);
    s.wait_index_idle();
    all_exact(&s, &more, "after close");
}

/// The tier of a run at fan-in 4 (see `merges_keep_the_run_count_small`).
fn tier(entries: u64, limit: u64) -> u32 {
    let mut n = (entries / limit).max(1);
    let mut t = 0;
    while n >= 4 {
        n /= 4;
        t += 1;
    }
    t
}
