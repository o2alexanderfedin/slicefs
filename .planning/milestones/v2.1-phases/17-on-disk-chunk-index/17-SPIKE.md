# Phase 17 spike: custom sorted runs against fjall

Plan 17-01. The spike measures an index-only prototype of the locked run
layout (`spike-runs`) and the same store semantics on fjall
(`spike-fjall`) with the Phase 16 tool `benchmarks/index-bench`. The result
is input for the decision at the end of plan 17-01.

## Package check (Task 1)

The user approved fjall for the spike only ("try both"). The check was done
before the fetch (fjall and its direct dependencies) and after the fetch,
before any build (the crates that came in through them).

| Item | Result |
|---|---|
| Name | `fjall`, exact (not a look-alike name). Description: "Log-structured, embeddable key-value storage engine". |
| Version pinned | `=3.1.13` (newest stable, 2026-10-25, not yanked) |
| Owner on crates.io | one user, `marvin-j97` |
| Publication | crates.io trusted publishing from the GitHub repository `fjall-rs/fjall`, commit `cfcdce3`. The repository tag `3.1.13` points to the same commit. |
| Downloads | about 2.24 million in total, about 0.82 million in the last 90 days, 94 thousand for 3.1.13 |
| Yanked versions | 49 of 103 fjall versions are yanked, among them every version from 3.0.0 to 3.1.5. lsm-tree: 55 of 101. The yanked versions have no yank message. The newest versions (3.1.6 to 3.1.13) are not yanked. So a pinned version can be yanked later; a yanked version stays usable with an existing `Cargo.lock`. |
| lsm-tree | `3.1.10` (fjall needs `~3.1.10`), same owner, trusted publishing from `fjall-rs/lsm-tree`; the repository tag `3.1.10` is the published commit `9812163`. |
| Other new crates | 22 new packages in `Cargo.lock` (141 to 163): byteorder-lite, byteview, compare, crossbeam-epoch, crossbeam-skiplist, crossbeam-utils, dashmap, enum_dispatch, fjall, flume, hashbrown 0.14, interval-heap, lsm-tree, lz4_flex, quick_cache, rustc-hash, self_cell, sfa, spin, twox-hash, varint-rs, xxhash-rust. byteview and sfa are from the fjall owner; the others are common crates with many downloads. |
| Build scripts | only crossbeam-epoch and crossbeam-utils have a `build.rs`; enum_dispatch is a procedural macro. |

Nothing looked wrong (no look-alike name, no yanked pinned version, no
unknown publisher), so the spike went on.

## Method

- Spike code: local branch `spike/17-index`, commit `8a1f656` (not merged,
  not pushed). It adds the backends `spike-runs` and `spike-fjall` to
  `backend_by_name` in `benchmarks/index-bench/src/backend.rs`, and the
  test `tests/spike_ro.rs`. Every number below comes from one release
  build of that commit.
- `spike-runs`: an index-only prototype of the locked layout. Memtable: a
  BTreeMap, flushed to a run at 1,000,000 entries. Run file: 4 KiB pages of
  58-byte entries (33-byte ID, pack u64, offset u64, stored u32, raw u32,
  flags u8; 70 entries per page), sorted by the full ID; one 8-byte fence
  per page; one blocked Bloom filter (10 bits per key, 512-bit blocks, 7
  probes, positions from digest bytes, no hashing); a footer. Open reads
  only the footer, the fences and the filter of each run, straight into
  their final buffers. Lookup: memtable, then the runs newest first:
  filter, binary search of the fences, one `pread` of one page. Close:
  flush, then merge all runs into one (a stand-in for the merges of
  17-06), then write the exact object count.
- `spike-fjall`: the same store semantics on fjall 3.1.13. Key = the
  33-byte ID, value = the 25-byte Loc. Put = get, then insert if absent.
  The settings are in "e." below. Close: flush the memtable to a table
  (`rotate_memtable_and_wait`), a blocking major compaction
  (`major_compact`), a wait until no flush or compaction runs, then
  `persist(PersistMode::SyncAll)` and the exact object count. (The plan
  said only `persist`. That leaves the data in the journal, and then every
  open measures a journal replay. Unfinished compaction work would also
  run inside the next measured step, because the bench ends with
  `process::exit`.)
- Both backends write no pack bytes: a put only hashes the 8-byte object
  and updates the index (synthetic Loc). So the put rates are index-only
  and cannot be compared with the chunk-store create rate.
- Steps: `create`, then `open` four times (the first is a warm-up; the
  table gives the median of the last three, and the range), then `lookup`
  three times. The lookup rounds alternate the backends (spike-runs,
  spike-fjall, chunk-store, then again), and the table gives the median of
  three and the range. Each lookup ran under `/usr/bin/time -l` for its
  peak RSS. Disk bytes: the sum of the file sizes in the store. The
  `run` subcommand was not used (it calls `recover`, which these backends
  do not support).
- Page cache: warm for every step.
- The chunk-store reference rows: the Phase 16 numbers from
  `docs/design/chunk-store.md` "Index baseline (#66)", and three lookup
  rounds on the Phase 16 stores in the same session as the spike (same
  load), for a fair comparison of the lookup times.

## Machine

Intel Xeon W-3245 (16 cores, 32 threads), 192 GiB RAM, macOS 26.2
(x86_64), APFS on an internal PCIe SSD. Phase branch commit `6296eb6`
(develop with Phase 16 merged). Release build, rustc 1.95.0-nightly.

The host was loaded by other work. Load average (1, 5, 15 min):

| Step | Load before | Load after |
|---|---|---|
| 1M: create, open, lookups | 81.21 83.81 80.59 | 83.67 83.96 81.25 |
| 10M: create, open, lookups | 83.67 83.96 81.25 | 76.03 84.65 84.18 |

The wall times are about 2 times the CPU times (for example, the
spike-runs 1M lookup step: 12.65 s wall, 6.54 s user plus system). The
open CPU times and the RSS values are stable; the lookup times vary from
round to round.

## Results

All numbers: macOS, warm page cache, one thread. Open values are the
median of 3 opens; lookup values are the median of 3 rounds (ranges in the
second table). Create runs one time.

| Backend | Objects | Cache | Steady RSS B/object after open | Peak RSS B/object (open) | Open s | contains hit ns | contains miss ns | Index-only puts/s | Disk B/object | Runs or tables at open |
|---|---|---|---|---|---|---|---|---|---|---|
| spike-runs | 1M | warm | 1.454 | 1.454 | 0.001 | 5464 | 210 | 339845 | 59.9 | 1 |
| spike-runs | 10M | warm | 1.371 | 1.371 | 0.009 | 5856 | 436 | 203012 | 59.9 | 1 |
| spike-fjall | 1M | warm | 2.933 | 2.925 | 0.026 | 14342 | 798 | 40656 | 65.6 | 1 |
| spike-fjall | 10M | warm | 2.120 | 2.119 | 0.648 | 19304 | 1003 | 31522 | 67.0 | 11 |

More detail (same runs):

| Backend | Objects | Cache | Open CPU s | Open s (min-max) | Hit ns (min-max) | Miss ns (min-max) | put dedup-hit ns (median) | Puts per CPU s | Lookup step peak RSS MB (median) |
|---|---|---|---|---|---|---|---|---|---|
| spike-runs | 1M | warm | 0.001 | 0.001-0.001 | 4457-5725 | 197-310 | 6723 | 561482 | 76.7 |
| spike-runs | 10M | warm | 0.009 | 0.009-0.009 | 4610-8192 | 250-447 | 6078 | 367458 | 89.0 |
| spike-fjall | 1M | warm | 0.004 | 0.025-0.029 | 12614-15206 | 711-1042 | 14570 | 72025 | 218.0 |
| spike-fjall | 10M | warm | 0.473 | 0.593-0.676 | 19241-19922 | 878-1068 | 20170 | 55702 | 165.8 |

Notes:

- Disk: spike-runs is 58 B per entry, plus the page padding (36 bytes per
  4 KiB page), the fences and the filter. spike-fjall at 10M is 65.9 B per
  object in its tables plus a journal file of 10.4 MB (1.04 B per object)
  that was still there after close.
- The spike-fjall open CPU time grows from 0.004 s at 1M to 0.473 s at
  10M (about 118 times for 10 times the objects). The cause was not
  isolated. The likely cause is the 10.4 MB journal that stays after close
  at 10M (at 1M the journal is empty), so the 10M open may include a
  journal replay. Condition (3) of the recommendation fails at both sizes
  (at 1M: 0.026 s against 0.001 s wall, 4 times in CPU time).
- The lookup step peak RSS includes the sample of the step itself: 1M hit
  IDs, 1M miss IDs and 1M numbers, about 74 MB, the same for every
  backend. So spike-runs needs about 3 to 15 MB above the sample; fjall
  about 90 to 145 MB (16 MiB block cache, worker threads, pinned blocks).
  The fjall value at 1M is higher than at 10M; this was not explained.
- spike-fjall CPU times include the work of its own threads (flush and
  compaction). The create time of both backends includes the close (flush
  and merge, or flush and major compaction).
- spike-runs has one run at open because its close merges all runs. The
  real design keeps up to about 10 + log4(N) runs, so its open time and
  its RSS grow a little with the number of runs (each run adds a footer,
  fences and a filter; the filter bits per key stay the same).
- A spike-runs hit costs about 5 to 6 microseconds against about 0.5 to
  0.8 for the chunk-store HashMap: the hit time is mostly the `pread` of
  one page (system time is about 2 microseconds per lookup under this
  load). A miss costs about the same as the HashMap.

Chunk-store reference (the in-RAM HashMap of today):

| Backend | Objects | Cache | Steady RSS B/object after open | Peak RSS B/object (open) | Open s | Open CPU s | contains hit ns | contains miss ns | Source |
|---|---|---|---|---|---|---|---|---|---|
| chunk-store | 1M | warm | 390.185 | 390.185 | 0.964 | 0.591 | 1304.791 | 727.474 | Index baseline (#66), macOS |
| chunk-store | 10M | warm | 195.792 | 256.312 | 12.844 | 5.881 | 700.501 | 323.994 | Index baseline (#66), macOS |
| chunk-store | 1M | warm | n/a | n/a | n/a | n/a | 483 (472-830) | 224 (214-327) | this session, 3 rounds |
| chunk-store | 10M | warm | n/a | n/a | n/a | n/a | 770 (468-987) | 307 (271-526) | this session, 3 rounds |

The chunk-store disk use is 104 B per object (packs and `.idx`), and its
create rate (with the pack writes) was 64k to 68k puts/s on macOS.

## fjall questions

### a. Read-only open by a second process

Not possible. The test `tests/spike_ro.rs` (spike branch) makes a small
spike-fjall store, keeps it open in the test process as the writer, and
starts the bench `open` step on the same directory. The second process
fails with this error:

```text
index-bench: FjallError: Locked
```

fjall takes an exclusive lock on the file `lock` in its directory at open
(three tries, 100 ms apart, then `Error::Locked`). fjall 3.1.13 and
lsm-tree 3.1.10 have no read-only open mode: the database builder has no
such option, and a search of both sources for "read_only", "readonly" and
"read-only" finds only the read-only transactions (snapshots) inside one
open database. After the writer closes, the same open works.

Effect: `slicefs stats`, `scrub` and the format check (`open_read_only` in
`crates/slicefs-cli/src/content.rs`) could not open the index while a mount
runs. They would have to rebuild the index in RAM from the packs, which is
the cost this phase removes.

### b. PackIo bypass

fjall opens, writes, syncs, renames and removes its files with `std::fs`
itself. The PackIo strategy (`FileIo`, `MemIo`) cannot reach them. So
`MemIo::crash`, `flip`, `fail_next_syncs`, `set_space_left` and the new
knobs that 17-04 adds (`fail_next_write_durable`, `fail_write_durable_after`,
`fail_sync_after`, `set_fail_removes`) have no effect on the index files.

These planned tests would need real files (a temp directory on `FileIo`)
and a drop of the store in place of `MemIo::crash`. A drop cannot lose
data that is in the page cache but not on the disk, so each one is a
weaker test:

- 17-04 `index_crash_tests.rs`: a_failed_active_pack_sync_before_a_flush,
  a_crash_after_the_run_file_before_the_manifest, a_crash_after_the_manifest,
  a_power_loss_with_unsynced_puts_after_a_flush,
  a_crash_during_the_replay_of_a_rebuild,
  gc_copies_are_not_counted_twice_after_a_failed_tombstone_flush; and the
  damage tests that flip bytes of a MANIFEST, a run footer or a run page
  (they would flip bytes of fjall files, whose format is not ours).
- 17-06: a_crash_after_the_merge_output_before_the_manifest,
  a_crash_after_the_merge_manifest_before_the_inputs_are_removed,
  a_damaged_page_found_by_a_merge_leads_to_a_rebuild.
- 17-07: a_failed_install_changes_no_pack,
  a_failed_pack_removal_after_the_install,
  a_damaged_page_in_the_gc_passes_requests_a_rebuild.

A disk-full test (`set_space_left`) is not possible for the index files.

### c. Locked on-disk rules

| Rule (17-CONTEXT.md) | fjall 3.1.13 |
|---|---|
| Packs and `.idx` unchanged | Met. fjall stores only the index. |
| Runs and MANIFEST in `STORE/index` | Approximated. fjall uses a directory that we choose (for example `STORE/index`), but the layout in it is fjall's: `keyspaces/N/tables`, `version` files (its manifest), journal files `N.jnl` and `lock`. A journal is always written; it cannot be turned off (only `manual_journal_persist`). |
| Full 33-byte ID and full Loc per entry | Met for content (key = ID, value = Loc). Each entry also has fjall fields (sequence number, value type, varint lengths), and the format belongs to the crate. |
| 8-byte fence per 4 KiB block | Approximated. `data_block_size_policy(BlockSizePolicy::all(4096))` gives 4 KiB blocks. The block index keeps the full last key (33 bytes) and a block handle for each block, not an 8-byte fence. |
| Sort order (tag, digest); filters on digest bytes only | Sort order met (bytewise key order). Filters on digest bytes: not possible. lsm-tree hashes the full key with its own 64-bit hash (`hash64`). |
| Bloom about 10 bits per key | Met by config: `filter_policy(FilterPolicy::all(FilterPolicyEntry::Bloom(BloomConstructionPolicy::BitsPerKey(10.0))))` and `expect_point_read_hits(false)` (the default `FalsePositiveRate(0.0001)` on L0 is replaced). |
| Cold open reads only MANIFEST, footers and filters | Approximated. With `filter_block_pinning_policy(PinningPolicy::all(true))`, `index_block_pinning_policy(PinningPolicy::all(true))` and partitioning off, open loads every filter and every full block index. The defaults pin only L0 filters and L0-L1 indexes, and then a lookup reads them through the block cache. If the journal holds entries that were not flushed, open replays them. |
| Tombstone run before a punch or delete | Approximated. `remove(id)` writes a tombstone for the whole key, not for (id, pack). The durable point is `persist(PersistMode::SyncAll)` of the journal, not a run. GC must remove only the keys whose current value names the removed pack, and must do this under the store lock, so that a compacted copy is not removed. |

### d. New crates

`Cargo.lock` went from 141 to 163 packages: 22 new crates.

### e. Settings behind the numbers

spike-fjall (fjall 3.1.13, lsm-tree 3.1.10, default features, so lz4 is
compiled in but not used):

- Database: `Database::builder(DIR/fjall).cache_size(16 MiB)`,
  `.journal_compression(CompressionType::None)`,
  `.manual_journal_persist(true)`; worker threads: the default.
- Keyspace `idx` (`KeyspaceCreateOptions`):
  `filter_policy(FilterPolicy::all(FilterPolicyEntry::Bloom(BloomConstructionPolicy::BitsPerKey(10.0))))`,
  `data_block_size_policy(BlockSizePolicy::all(4096))`,
  `data_block_compression_policy(CompressionPolicy::disabled())`,
  `index_block_compression_policy(CompressionPolicy::disabled())`,
  `filter_block_pinning_policy(PinningPolicy::all(true))`,
  `index_block_pinning_policy(PinningPolicy::all(true))`,
  `filter_block_partitioning_policy(PartitioningPolicy::all(false))`,
  `index_block_partitioning_policy(PartitioningPolicy::all(false))`,
  `expect_point_read_hits(false)`; the default leveled compaction and
  64 MiB memtable.
- So the steady RSS after open holds every filter and every block index
  of the 1 or 11 tables. The block cache (16 MiB) fills only during the
  lookups; it is in the lookup step peak RSS, not in the open RSS.

spike-runs: 10 bits per key, 512-bit blocks, 7 probes, 4 KiB pages,
memtable flush at 1,000,000 entries, no cache (the page cache of the OS
only).

## Recommendation

The rule was fixed before the measurement: recommend fjall only if all
four conditions hold.

| Condition | Result | Held |
|---|---|---|
| (1) A second process can open the store read-only while a writer holds it | fjall refuses with `Locked` (question a.) | No |
| (2) Steady RSS after open at 10M is at most 4 B per object | 2.120 B | Yes |
| (2) and at most 1.5 times the spike-runs value | 2.120 / 1.371 = 1.55 times (range 1.54 to 1.56; the RSS values are stable) | No, by a small margin |
| (3) Open time at 10M is at most 2 times the spike-runs value | 0.648 s / 0.009 s = 72 times (CPU: 0.473 s / 0.009 s = 53 times) | No |
| (4) Hit time at 10M is at most 2 times the spike-runs value | 19304 / 5856 = 3.3 times (with the ranges: at least 2.3 times) | No |
| (4) Miss time at 10M is at most 2 times the spike-runs value | 1003 / 436 = 2.3 times (with the ranges: 1.96 to 4.3 times) | No, but within the noise |

Recommendation: the custom sorted runs. Only the 4 B limit of condition
(2) held. Condition (1) alone excludes fjall: the read-only open of
`slicefs stats`, `scrub` and the format check would not work while a
mount runs. fjall also writes its own journal, bypasses PackIo (so the
MemIo crash and fault tests cannot cover the index), and needs 22 new
crates. Its index-only put rate per CPU second is 6.6 times lower than the
spike-runs rate at 10M (55,702 against 367,458) and 7.8 times lower at 1M.

What the spike-runs numbers say about the custom design: steady RSS 1.37
to 1.45 B per object (target: a few bytes), open in 1 to 9 ms (the
chunk-store needs 1 to 13 s today), a miss at the cost of the HashMap,
but a hit 8 to 11 times slower than the HashMap in the same session (one
page read per hit).

## Decision

Decision: custom

Date: 2026-11-18. Reason given by the user: fjall cannot be opened
read-only by a second process while a writer holds it, which breaks
`slicefs stats`, `scrub` and the format check during a mount; the custom
runs met the RAM and open-time targets.

Plans 17-02 to 17-09 run as written. fjall stays only on the local spike
branch `spike/17-index`, which is not merged and not pushed.
