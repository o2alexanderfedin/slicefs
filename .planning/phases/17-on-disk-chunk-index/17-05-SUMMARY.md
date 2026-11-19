---
phase: 17-on-disk-chunk-index
plan: 05
subsystem: chunk-store index
tags: [index, bloom, mmap, interpolation, concurrency, gen, race]
status: complete

requires: [17-04]
provides:
  - bloom.rs (blocked Bloom filter over digest bytes, filter type 1 in the run footer)
  - Options::index_bloom_bits; IndexInfo::filter_bytes and fence_bytes
  - PackIo::map_run (required) and pack_io::RunMap; MADV_RANDOM_MIN
  - Run::get on the mapping (no read_at, no page CRC on a hit), Run::get_verified, Index::get_verified
  - Index shared as Arc<Index>: 16 memtable shards, Arc run snapshot, gen counter, Snapshot::iter_all
  - put and read look up outside the store lock with revalidation; contains and stored_len take no store lock
  - MemIo reads, bytes_read, reset_read_counters
  - src/race_tests.rs (cfg(test) after_lookup hook)
affects: [17-06, 17-07, 17-08]

actuals:
  tokens: 29000
  tasks: 2
  commits: 4
plan_head_before: 3f4aff30f3e78ff1b335ae95e6af8dcb80e7fe9f
plan_head_after: a514c34 (code and tests; the docs commits follow)

tech-stack:
  added: []
  patterns:
    - lookups read run pages in place from a read-only mapping; scrub-like, merge and GC reads keep read_at with every page CRC checked
    - no page CRC on a hit; the CRC is checked on a filter-positive miss, so damage is never a miss that falls through to an older run
    - a lookup outside the store lock is used under the lock only if the index gen is the same and the memtable has no entry of the ID
    - a flush installs the new run before it clears the memtable shards

key-files:
  created:
    - crates/chunk-store/src/bloom.rs
    - crates/chunk-store/src/race_tests.rs
  modified:
    - crates/chunk-store/src/run.rs
    - crates/chunk-store/src/index.rs
    - crates/chunk-store/src/lib.rs
    - crates/chunk-store/src/pack_io.rs
    - crates/chunk-store/src/retry.rs
    - crates/chunk-store/src/mem.rs
    - crates/chunk-store/tests/index_tests.rs

key-decisions:
  - "Read path from the mmap spike: each sealed run is mapped read-only (PROT_READ, MAP_SHARED, libc only); a hit reads its page in place with no read_at and no copy"
  - "Interpolation search in f64 at both levels (fences, then inside the page), with a binary search after 6 steps and the 17-02 step-back rule; no Eytzinger"
  - "No page CRC on a hit; the CRC is checked at write, in get_verified, iter (merge, GC) and on a filter-positive miss"
  - "A read whose record is rejected checks the entry with get_verified and asks for a rebuild if the run page is damaged"
  - "Each run is an Arc<Run> that owns its RunMap; unmapped when the last Arc drops"
  - "PackIo::map_run is a required method: FileIo maps, RetryingIo passes it on, MemIo gives none (lookups use read_at there, so counters and faults reach them)"
  - "MADV_RANDOM only for runs of MADV_RANDOM_MIN = 256 MiB or more (needs a measurement)"
  - "SIGBUS on a disk read error inside a mapping is an accepted risk (documented on RunMap)"
  - "The index RAM (filters + fences) is anonymous memory; mapped run pages are file-backed page cache and are not counted"

duration: about 40 minutes (16:56, the build of the before binary, to 17:35, the summary commit; local time)
completed: 2026-11-18
---

# Phase 17 Plan 05: Bloom filters, mapped run reads and lookups outside the store lock Summary

**Every run now has a blocked Bloom filter (10 bits per key, about 1.4 B per object of RAM with the fences). Lookups read the run page in place from a read-only mapping, with interpolation search and no page CRC on a hit. `put` and `read` look up outside the store lock and check under it, with a generation counter, that no GC or flush made the answer stale. A `contains` hit at 1M objects went from about 5.1 µs to about 0.95 µs, and a miss from about 8.7 µs to about 0.64 µs.**

## Read-path decisions (from the orchestrator, after the mmap spike)

These were added to the plan at launch. What was built for each:

1. **Mapping.** `PackIo::map_run(num) -> io::Result<Option<RunMap>>`. `FileIo` maps the whole run file with `mmap(PROT_READ, MAP_SHARED)` (libc, no new crate). `Run::get` reads the page straight from `RunMap::bytes()`: no `pread`, no copy.
2. **Interpolation search** at both levels, in f64:
   - `upper_bound` (fences): at most 6 interpolation steps while more than 8 fences are left, then a binary search of the rest. It returns exactly what `partition_point(|f| f <= p)` returns. A unit test checks this on uniform, cubic, all-equal, `u64::MAX` and empty sets.
   - `search_page` (inside the page): a guess from the page fence and the next fence, then a walk of at most `count` entries.
   - The 17-02 step-back rule (keys that share an 8-byte prefix across a page boundary) is kept.
3. **No page CRC on the hot path.** The CRC is checked:
   - at write (the writer computes it);
   - in `Run::get_verified` and `Run::iter`, which read with `read_at` and check the CRC and the zero bytes (merge in 17-06, GC phase A, and the check of a rejected entry);
   - on a filter-positive miss (see deviation 1).

   The reason is in the `run.rs` module comment and in the `Run::get` doc.
4. **Lifetime.** `Run` owns `Option<RunMap>`. Runs are `Arc<Run>` in an `Arc<Vec<…>>` snapshot. `RunMap::drop` unmaps. A reader that holds a snapshot keeps the mapping after a merge or GC removes the file.
5. **Read interface.** The mapped slice for `FileIo`. `MemIo::map_run` returns `None`, and `Run::get` then reads the page with `read_at` into a stack buffer (`Pages` trait: `Mapped` or `Buffered`). The search code is the same for both, so the MemIo damage and fault tests test the production search, and the MemIo read counters count run pages. `map_run` is a required trait method (no default), so a wrapper that forgets it does not compile.
6. **`MADV_RANDOM`** only when the mapped length is at least `MADV_RANDOM_MIN = 256 << 20`. The doc says the value comes from the spike (58 MB: default readahead better; 585 MB: MADV_RANDOM better) and needs its own measurement.
7. **SIGBUS** is documented on `RunMap` as an accepted risk. A run shorter than its MANIFEST length is refused at open (`Corrupt`) before any page access. A short mapping would be SIGBUS, not a short read.
8. **Memory accounting.** The plan's RAM test is arithmetic: `(filter_bytes + fence_bytes) / run_entries`, which is anonymous memory. No counting allocator was needed. The `IndexInfo` docs and the test comment say that the mapped run pages are file-backed page cache and are not counted.

**Hot-path guard (added test):** `a_hit_on_files_reads_the_run_page_in_place`.
- It builds the `ChunkStore::open` stack (`RetryingIo` over `FileIo`) on a tempdir, with a `PackIo` wrapper that counts run `read_at` calls and whole-file run reads.
- After the open, 3,000 hits, 3,000 misses and one `get` make 0 run reads.
- The same flow on MemIo reads 100 to 118 pages for 100 hits.
- Mutant: `RetryingIo::map_run` returns `None`. The test fails.

## Hit and miss timing, before and after

`index-bench lookup` (release build, 1M hits sampled from the store, 1M misses; warm page cache). Each store was made with its own binary at 1,000,000 objects: 3 runs of 262,144 entries plus 213,568 entries replayed into the memtable at open. Intel Xeon W-3245, macOS 26.2, APFS. Load average about 18.5 during the run. Three rounds, before and after interleaved:

| Build | contains hit | contains miss | put dedup-hit |
|---|---|---|---|
| before (3f4aff3: pread + CRC, no filter, binary search, store lock) | 4.86 / 5.12 / 5.42 µs | 8.59 / 8.70 / 8.86 µs | 5.07 / 5.21 / 5.54 µs |
| after (8648206) | 0.96 / 0.87 / 1.08 µs | 0.61 / 0.62 / 0.69 µs | 1.34 / 1.51 / 1.54 µs |
| after, 1,048,576 objects (4 runs, empty memtable) | 0.54 / 0.69 / 0.69 µs | 0.20 / 0.26 / 0.27 µs | 0.66 / 0.82 / 0.83 µs |

In the 1M store, the 213,568 memtable entries (16 BTreeMap shards) cost about 0.3 µs on every lookup. A store with an empty memtable comes close to the spike values (0.43 to 0.66 µs). The intermediate build after the read-path commit only (e844cdd, store lock still taken by every lookup) measured hit 1.15 to 1.31 µs, miss 0.78 to 0.94 µs, and put hit 1.25 to 1.36 µs.

## What was built

### Task 1: Bloom filter (fc1012e)
- `bloom.rs`:
  - 512-bit blocks;
  - block = high 64 bits of (u64 LE digest[0..8]) × blocks;
  - h1 = LE digest[8..16], h2 = LE digest[16..24] | 1;
  - k = round(0.693 × bits), clamped to 1..=16;
  - LE words on disk.

  `from_bytes` refuses a length that is zero or not a whole number of blocks, and 0 bits per key.
- `RunWriter::create(io, num, expected, bits_per_key)` sizes the filter from `expected` and inserts each digest (tombstones too) while the entries stream. It keeps no digests.
- `Run::open` checks the filter:
  - a whole number of blocks;
  - at least `blocks_for(entries, bits) × 64` (checked arithmetic; the writer sizes from an upper bound, so the length can be larger);
  - the CRC;
  - type none must have no bytes and 0 bits.
- `Run::get` returns `None` on a filter negative before any page access.
- `Options::index_bloom_bits` (default 10, 0 = filter type none). `IndexInfo::filter_bytes`, `fence_bytes`.

### Read path (e844cdd)
- See the decisions above.
- `decode_checked` makes these `Corrupt`:
  - flags other than 0, 1 and 0x80;
  - a put at offset 0 (what a tombstone that lost its flag bit looks like).
- `ChunkStore::read` passes a record error (`Corrupt`, `NotFound`, `UnexpectedEof`) to `check_entry`. `check_entry` calls `Index::get_verified`. If that is `Corrupt`, it calls `Index::request_rebuild` (the 17-04 crate-private API; once, on a writable store) and returns `Corrupt`.

### Task 2: concurrency (8648206)
- `Index` (shared as `Arc<Index>` in both `ChunkStore::index` and `Inner::index`):
  - `shards: [Mutex<BTreeMap>; 16]` (shard = digest byte 0 >> 4);
  - `mem_len: AtomicUsize`;
  - `runs: RwLock<Arc<Vec<Arc<Run>>>>`;
  - `man: Mutex<Manifest>`;
  - `gen: AtomicU64`.

  `get` reads the shard first, then clones the run snapshot. No lock is held during the page read.
- `flush`:
  1. sorted copy of the shards (sorted as a whole: IDs sort by tag first, shards split by digest);
  2. write the run and the MANIFEST;
  3. install the run in the run list;
  4. clear the shards;
  5. `gen += 1`.
- `gc_finish` bumps `gen` right after its durable point (D), before any pack changes. Phase A uses `snapshot().iter_all()` (a memtable copy plus the run snapshot).
- `put`:
  1. `gen`, then the lookup outside the lock, then the hook.
  2. On a hit: take the lock and `touch`. If `unchanged(gen, id)` or a new lookup under the lock still hits, return. Else go on to the write path.
  3. Write path: compress, take the lock, `touch`. Look up again if `!unchanged`. Then `check`, append, add.
- `read`:
  1. `gen`, then the lookup outside the lock, then the hook.
  2. Take the lock. Look up again if `!unchanged`.
  3. `touch`, take the pack reader, release the lock.
- `contains`, `stored_len`: no store lock.
- Lock order: the store lock, then one shard or the run-list lock. A lookup never holds two locks.
- `MemIo`: per-kind `reads`, `bytes_read` (whole-file reads and `read_at` calls that worked), `reset_read_counters`.

## Tests

- `cargo test -q --locked -p chunk-store --lib bloom::`: 4 passed (`no_false_negatives`, `the_false_positive_rate_is_about_one_percent`, `the_size_is_ten_bits_per_key_in_whole_blocks`, `bytes_round_trip`).
- `--lib run::`: 13 passed. The new ones:
  - `a_run_with_a_filter_answers_misses_without_a_page_read` (1,000 absent hashed IDs: at most 30 page reads; 5,000 entries);
  - `a_run_without_a_filter_round_trips`;
  - `tombstones_are_in_the_filter`;
  - `a_damaged_page_gives_no_false_miss_and_the_verified_reads_are_corrupt`;
  - `a_damaged_tombstone_never_reads_as_a_miss_or_a_put`;
  - `the_interpolation_search_finds_what_a_binary_search_finds`.

  `a_run_round_trips_through_each_io` now also checks decision 4: on FileIo, after the run file is removed, the open run still reads through its mapping (commit a514c34). It asserts 0 `read_at` per hit on FileIo, 1 on MemIo and RetryingIo(MemIo), and that `map_run` is `Some` only for files. `a_damaged_footer_or_table_is_refused` has five new filter cases.
- `--lib race_tests::`: 3 passed (`a_put_hit_that_gc_removed_before_the_lock_writes_again`, `a_read_whose_loc_gc_moved_returns_the_bytes_from_the_new_pack`, `a_put_hit_without_a_gc_writes_nothing`).
- `--test index_tests`: 30 passed. The new ones:
  - `hits_read_one_page_and_misses_read_almost_none`;
  - `a_cold_open_reads_no_entry_page`;
  - `the_index_ram_is_about_two_bytes_per_object`;
  - `a_hit_on_files_reads_the_run_page_in_place`;
  - `a_damaged_run_entry_place_is_never_read_and_asks_for_a_rebuild`;
  - `a_damaged_tombstone_never_shows_the_older_entry`;
  - `lookups_outside_the_lock_never_miss_a_stored_object`.

  Two were rewritten to the new contract (see deviation 2).
- `cargo test -q --locked -p chunk-store`: every target passed (42, 10, 4, 5, 7, 30, 10, 9, 6, 11, 0).
- `cargo test -q --locked -p slicefs-cli --test gc_online_tests --test gc_sweep_tests`: 9 and 2 passed.
- Downstream, `cargo test -q --locked -p slicefs-cli -p metadata -p index-bench` (once): 39 test targets all ok, 976 passed, 0 failed, 6 ignored; the output has no `FAILED`, `panicked` or `error` line.
- `cargo clippy -p chunk-store --all-targets`: no new warning. The three warnings that are left were there before: `lib.rs` `verified_record` type complexity, `file_tests.rs:69`, and `index_tests.rs:511`.

### Mutation checks (each run, then reverted)

| Mutant | Tests that failed |
|---|---|
| No revalidation (`unchanged` always true) | `a_put_hit_that_gc_removed_before_the_lock_writes_again` (get is `None`: X lost), `a_read_whose_loc_gc_moved_returns_the_bytes_from_the_new_pack` (`Io(NotFound)`) |
| The `after_lookup` call removed from `put` (the acceptance mutant) | `a_put_hit_that_gc_removed_before_the_lock_writes_again` and `a_put_hit_without_a_gc_writes_nothing`, both on the hook flag assertion |
| A flush clears the shards before it installs the run (plus a `yield_now` between) | `lookups_outside_the_lock_never_miss_a_stored_object`, 3 of 3 runs ("object 13 of 34") |
| No CRC check on a filter-positive miss | `a_damaged_tombstone_never_shows_the_older_entry`, `a_damaged_run_page_gives_no_wrong_data`, and two run.rs damage tests |
| No rebuild request in `check_entry` | `a_damaged_run_entry_place_is_never_read_and_asks_for_a_rebuild` |
| No offset-0 check in `decode_checked` | `a_damaged_tombstone_never_reads_as_a_miss_or_a_put` |
| `DEFAULT_BLOOM_BITS = 0` | `hits_read_one_page_and_misses_read_almost_none` (6,495 page reads for 1,000 hits), `the_index_ram_is_about_two_bytes_per_object` |
| `RetryingIo::map_run` returns `None` | `a_hit_on_files_reads_the_run_page_in_place` |

## TDD Gate Compliance

- The tests and the code of each task are in one commit. The new tests use APIs that did not exist before (`RunWriter::create` with bits per key, `filter_bytes`, `map_run`, `get_verified`, `reads`, `after_lookup`), so their RED state was a compile error.
- The race tests cannot run on the old under-lock-only code: it has no point between the lookup and the lock where a hook can run. That was the reason the plan expected. The mutants above show what the tests catch: without the revalidation, the put loses X and the read gets `NotFound`. Without the hook call, the flag assertion fails.

## Acceptance checks

- `grep -c 'fn may_contain' crates/chunk-store/src/bloom.rs`: 1
- `grep -c 'fastbloom' crates/chunk-store/Cargo.toml`: 0
- `grep -c 'index_bloom_bits' crates/chunk-store/src/lib.rs`: 3
- The three index_tests functions: 3
- `grep -c 'fn reads\|fn bytes_read\|fn reset_read_counters' crates/chunk-store/src/mem.rs`: 3
- `grep -c 'cfg(test)' crates/chunk-store/src/lib.rs`: 6 (module, field, hook body, type alias and others)
- `grep -c 'sleep' crates/chunk-store/src/race_tests.rs`: 0
- The hook-removal mutant fails on the flag assertion (table above).
- No dependency change: no fjall, no fastbloom, no new crate. `blake3` in the bloom and run tests is a normal dependency.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 2 - Missing critical functionality] The page CRC on a filter-positive miss**
- **Found during:** the read path (the advisor review pointed to it)
- **Issue:** with no page CRC on lookups, a damaged tombstone (a damaged ID byte) makes its run miss. The search then goes on to an older run, and the older put entry points to a pack that GC deleted. `put` takes this as a dedup hit and writes nothing, so the object is lost with no error. Decision 3 said that a damaged page gives "a false miss (the object is stored again)". That is not true when an older run has an entry.
- **Fix:**
  - The page CRC is checked before `Run::get` returns `None` for an ID that the filter let through. This is about 1 % of the misses per run, and nothing on a hit.
  - `decode_checked` makes bad flags `Corrupt`, and also a put at offset 0 (a tombstone whose flag bit was lost).
- **Tests:** `a_damaged_tombstone_never_shows_the_older_entry`, `a_damaged_tombstone_never_reads_as_a_miss_or_a_put`.
- **Commit:** e844cdd

**2. [Rule 2] A rejected record checks its entry and asks for a rebuild; the 17-04 damage tests were rewritten**
- **Issue:** a damaged place (pack, offset, length) in an entry is now a hit with the wrong place. Before 17-05, the lookup gave `Corrupt`, and the put wrote again. Now:
  - `read` rejects the record (CRC, ID, hash), so it never returns wrong bytes;
  - `put` takes the entry as a dedup hit. The object's bytes are still in their pack.
- **Fix:** `ChunkStore::check_entry` runs on a rejected record. It looks the entry up through the verified pages. If the run page is damaged, it calls `Index::request_rebuild` and returns `Corrupt`. The next writable open rebuilds the index from the packs, and the object reads back.
- **Tests:**
  - `a_damaged_run_page_gives_no_wrong_data` (17-04) now flips an ID byte, not a pack byte, and checks the new contract: only the objects whose search crosses the damage fail, a put writes them again, and the reopen rebuilds.
  - `a_damaged_newer_page_never_shows_an_older_entry` accepts `Missing` or `Corrupt` for the removed object; its key check, "never the older entry", is unchanged.
  - New: `a_damaged_run_entry_place_is_never_read_and_asks_for_a_rebuild`, for the pack, offset and stored fields.
- **Accepted and documented:** until the next writable open, `contains` is true for that one object, and a put of it is a dedup hit.
- **Commit:** e844cdd

**3. [Plan refinement] Filter length check**
- **What changed:** `Run::open` checks that the filter is at least the size for the real entry count. The plan asked for "block count × 64 for the stored parameters".
- **Why:** the writer sizes the filter from an upper bound (a merge passes the sum of its inputs), so an exact check would refuse good merge runs.

**4. [Plan refinement] `gen` is also bumped at the GC install point**
- `gc_finish` calls `bump_gen` after its durable point (D), even when the flush wrote nothing. The flush bumps `gen` too. The extra call keeps the rule independent of whether GC had memtable entries.

**5. [Plan refinement] Lookup tests use the bench's object IDs**
- The run unit tests use hashed digests for the filter tests: the `key()` digests are not uniform. The index tests use `small(i)`.
- The MemIo control in `a_hit_on_files_reads_the_run_page_in_place` allows the filter false positives of newer runs (100 to 118 page reads for 100 hits).

**6. [Rule 3] `Run::open` and `Index::load` take the mapping**
- `Run::open(file, map, num, len)`.
- A `NotFound` from `map_run` on a listed run is `Damage::MissingRun`, so a read-only open reads the MANIFEST again, as for `open_run`.

**7. [Orchestrator instruction] The read path replaces two entries of the LOCKED 17-CONTEXT.md**
- The 17-CONTEXT.md entry "[Disk I/O path] pread through the PackIo strategy" and the deferred "mmap read path" are replaced by the read-path decisions of the launch message (decision 1: map each sealed run). The orchestrator made this choice after the mmap spike; the user delegated it. PackIo stays the only way to the files (`map_run` is a PackIo method), and MemIo fault injection still reaches the lookups.

**8. [Process] Commits on `feature/17-on-disk-index`**
- The launch message said to commit there, as 17-02 to 17-04 did. STATE.md and ROADMAP.md are not updated, as the launch message said.

## Not covered by these tests

- **SIGBUS:** a disk read error inside a mapping. It is accepted and documented, not tested.
- **The cold page cache:** not measured in this plan. The `MADV_RANDOM` limit stays at 256 MiB until it is measured.
- **Linux:** the timing is macOS only. The Linux numbers are in the spike.
- **The `madvise` call:** no test checks that it runs. It is advice only.
- **Read-only open while a writer replaces runs:** a run can be replaced between `open_run` and `map_run`. The window is small, and the result is the same as the 17-04 MissingRun path.

## Notes for later plans

- **17-06 (merges):**
  - Install a merged run through the `runs` RwLock (new `Arc<Vec>`), then `bump_gen()`, all under the store lock. Remove the input files after the MANIFEST is durable. Readers keep their mappings through their `Arc<Run>`.
  - A merge must read inputs with `Run::iter` (verified). Pass the sum of the input counts as `expected` to `RunWriter::create`.
- **17-07 (GC outside the lock):** `put` and `read` already revalidate on `gen`. The GC install must bump `gen` under the store lock before any pack is deleted or punched.
- **17-08 (bench):** report the memtable size next to the lookup numbers. In a store whose last flush was long ago, the replayed memtable adds about 0.3 µs to each lookup at 213k entries.

## Known Stubs

None.

## Threat Flags

None beyond the plan's threat model.
- T-17-09: gen + memtable revalidation; three hook tests and the concurrent test.
- T-17-10: filter length, parameters and CRC checked; unit tests.
- T-17-11: one lock order; a lookup holds one lock at a time.
- T-17-SC: no dependency change.
- T-17-06 (17-04) changes because there is no page CRC on a hit: see deviations 1 and 2.

## Self-Check: PASSED

- FOUND: crates/chunk-store/src/bloom.rs, crates/chunk-store/src/race_tests.rs and the modified files
- FOUND: fc1012e, e844cdd, 8648206, b4fa3c7, a514c34 (in `git log 3f4aff3..HEAD`)
