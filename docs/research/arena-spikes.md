# Arena spikes A-E: final report

Branches: `spike/arena-a` (23dded8), `spike/arena-b` (84d4613), `spike/arena-c` (fe6ca22), `spike/arena-d` (292d400), `spike/arena-e` (f2bb8bf). These branches are not merged. The code is in `spikes/arena` on each branch. The raw data is in `spikes/arena/results/{a,b,c,d,e}`.

Machine: macOS 26.2, Intel, APFS. Other processes put load on it the whole time: load1 was 12-51 on 32 threads. Because of this, the timings are only indicative. Byte counts and F_FULLFSYNC counts do not change with load, so they are firm.

## 1. Summary

| Spike | Verdict |
|---|---|
| A. Preallocated fixed-size arenas | **Inconclusive.** It is correct and crash-safe. No speed gain was measured. The put that crosses an arena boundary stalls for 44-79 ms. |
| B. Reads through mmap | **Adopt with changes.** Warm and dense reads are much faster. Sparse cold random reads are slower. There is a SIGBUS risk on I/O errors. |
| C. Reuse of free space inside arenas | **Reject.** It saves space and write volume, but it needs too many F_FULLFSYNC calls per commit. |
| D. GC hole punching | **Adopt with changes**, as a hybrid with compaction. The hybrid rule is not measured yet. |
| E. Writes through mmap | **Reject for now.** The CPU gain is small. On a full disk, APFS loses mmap writes without an error until F_FULLFSYNC. |

## 2. Results

### A. Fixed-size arenas

Variants: V0 = grow (today), V1 = sparse ftruncate, V2 = F_PREALLOCATE + ftruncate, V3 = zero-filled. RS = the real `ChunkStore`.

The A/A noise floor N is 11-32 %. This is larger than every planned threshold. The A-B1 K=1 runs are partial: 14 of 25 runs finished. A-B1 K=8 and K=64, A-B2, A-B3, A-B4, A-B6 and A-B7 did not run.

| Metric | Baseline | Spike |
|---|---|---|
| Sync p50, K=1 (ms) | V0 24.68, RS 24.71 | V1 22.38, V2 24.56, V3 23.64 (inside the noise) |
| CPU per put, K=1 (us) | V0 357, RS 359 | V1 452, V2 339, V3 306 (inside 2N) |
| Max latency of the put that crosses an arena boundary (ms) | V0 0.63, RS 0.61 | V1 44.4, V2 74.1, V3 78.6 |
| Creation of one arena, smoke runs only (ms) | n/a | V2 p50 51.8 (n=5), V3 p50 54.3 (n=2) |
| F_FULLFSYNC, 8 KiB write (ms) | p50 20.6, p99 39.4 | n/a |
| kill -9 crash loop, 50 kills (root checks / bad) | V0 46,704 / 0 | V1 39,324 / 0; V2 25,698 / 0; V3 41,742 / 0 |

`prepare_next` makes the next arena in the background, to remove the boundary stall. It is built and tested. Its effect is not measured.

### B. mmap reads (on V1 arenas)

The read noise floor is 1.4-3.8 %, so treat 2N as about 5-8 %.

| Metric | Baseline (pread) | Spike (mmap, V1M) |
|---|---|---|
| Random get, warm, incompressible: CPU per get (us) | V1 11.64, RS 11.60 | 7.86 (-32.5 %) |
| Random get, warm, 2:1 zstd: CPU per get (us) | V1 25.41, RS 25.68 | 21.32 (-16.1 %) |
| Sequential read, warm, incompressible (MiB/s) | V1 772, RS 780 | 1121 (+45 %) |
| Sequential read, warm, 2:1 (MiB/s) | V1 356, RS 352 | 412 (+16 %) |
| Warm cache, new map: CPU per get (us) | V1 11.46, RS 10.87 | 9.76 (-14.8 %) |
| Cold, dense random, 84 % of data read: wall per get (us) | V1 115.9, RS 116.6 | 55.8 (-52 %) |
| Cold, sparse random, 9 % of data read: wall per get (us) | V1 257, RS 271 | 307 (+19.3 %), CPU +35.2 % |
| Raw read + CRC, warm: CPU per read (us) | 4.57 | 1.50 (-67 %) |
| Gets/s at 1 / 4 / 16 threads | 87.4k / 259k / 460k | 138k / 375k / 466k |
| Cold, MADV_RANDOM against Default (s) | Default 11.3 random, 5.8 sequential | Random 41.9, 26.7 |
| RSS after a full pass (MiB) | 41 (footprint 24) | 1071 (footprint 21.5) |
| 2,000 arenas with RLIMIT_NOFILE 256: failed gets | RS 15,715 of 17,992; V1 12,223 of 13,994 | 0 (3 fds) |
| kill -9 crash loop, 50 kills (root checks / bad) | V1 39,324 / 0 | 29,328 / 0 |

### C. Free-space reuse (compared with compaction)

V1 = spike A with compaction. C1 = the planned design. C4d4 = arena best fit (V4) with up to 4 dirty arenas.

| Metric, fill 50 / 70 / 85 / 95 % | Compaction (V1) | C1 | C4d4 |
|---|---|---|---|
| Peak space amplification (x live) | 2.04 / 1.80 / 1.56 / 1.45 | 2.05 / 1.46 / 1.33 / 1.10 | 2.05 / 1.46 / 1.32 / 1.09 |
| Write amplification | 1.34 / 1.40 / 1.63 / 1.61 | 1.018 / 1.032 / 1.055 / 1.146 | 1.013 / 1.023 / 1.037 / 1.0999 |
| F_FULLFSYNC per 8 MiB commit | 1.47 / 1.47 / 1.49 / 1.30 | 2.00 / 2.89 / 3.27 / 6.40 | 1.55 / 2.18 / 2.01 / 3.11 |
| Mean sync() (ms) | 27.6 / 30.0 / 27.1 / 27.9 | 42.5 / 57.7 / 61.6 / 107 | 34.2 / 45.0 / 37.7 / 40.0 |
| Put throughput (MB/s) | 43.7 / 52.9 / 44.1 / 60.0 | 44.4 / 43.5 / 40.3 / 34.2 | 47.4 / 48.3 / 46.4 / 48.4 |
| GC wall / CPU at 85 % (s) | 2.09 / 1.09 | 3.53 / 0.43 | 3.63 / 0.41 |

More results:

- Sync of N dirty files, N = 1/2/4/8: 21.1 / 43.4 / 85.5 / 189 ms. The cost is linear, about 21 ms for each file.
- Reopen after a crash: about 0.105 s for each dirty 64 MiB arena. With 5 dirty arenas, it is 0.577 s.
- Sequential read after churn: no difference from a new store (inside the noise).
- Crash loop, 50 kills: 0 bad roots in C1, C3, C4, C4s and C4d4 (13,909-14,294 checks each).

### D. Hole punching (compared with A's compaction and with C's reuse)

GC frees space in each case:

| Metric | Compaction (RS / A) | Punch (D) |
|---|---|---|
| Freed / dead, whole-file delete, 10-90 % dead | A: 0 at 10-30 %, 0.59 at 50 %, about 1.0 at 70-90 % | 0.995-1.002 at every fraction |
| Freed / dead, random-object delete, 10/30/50/70/90 % | A: 0 / 0 / 0.336 / 0.969 / 0.988 | 0.549 / 0.642 / 0.739 / 0.837 / 0.936 |
| Space freed, 30 % dead, file delete (MiB) | RS -0.1, A -0.1 | 65.9 of 65.9 |
| Space amp after GC, 90 % dead, object delete | A 1.17, RS 1.005 | 1.64 |
| Data written by GC, 50 % dead, file delete (MiB) | RS 60.6, A 60.6 | 0.0 (+0.6 idx) |
| GC wall, one GC, 30 / 50 / 90 % dead (s) | A 0.049 / 0.430 / 0.421 | 0.856 / 0.560 / 0.862 |
| GC CPU, one GC, object delete, 30 / 50 / 90 % dead (s) | A 0.006 / 0.068 / 0.056 | 0.163 / 0.207 / 0.109 |

Churn, 6 rounds (192 MiB live, 25 % deleted and 25 % new each round):

| Metric | RS | A | D |
|---|---|---|---|
| Space amp, file delete, rounds 1..6 | 1.27-1.52 | 1.24-1.51 | 1.012-1.013 |
| Space amp, object delete, rounds 1..6 | 1.25-1.52 | 1.28-1.57 | 1.106 rising to 1.226 |
| Write amplification, file / object delete | 1.588 / 1.619 | 1.588 / 1.619 | 1.021 / 1.023 |
| Total GC wall / CPU, file delete (s) | n/a | 0.91 / 0.33 | 2.24 / 0.26 |
| Total GC wall / CPU, object delete (s) | n/a | 0.95 / 0.35 | 3.06 / 1.07 |
| Arena files at the end, file delete | 4 | 4 | 9 |

Raw cost of a punch on APFS: an 8 KiB F_PUNCHHOLE has p50 21-26 us and costs 26-31 us of CPU. An 8 KiB write into a hole plus F_FULLFSYNC has p50 21.01-21.08 ms, the same as an overwrite (20.96-21.06 ms).

**D compared with C.** The two do different jobs:

- At high fill, D gets C's space gain. C's space amp is 1.10-1.46. D's is 1.012 with whole-file deletes and up to 1.226 with object deletes.
- D's write amplification (1.02) is close to C4d4's (1.01-1.10).
- D adds no F_FULLFSYNC on the put path. C adds 1.05-4.9x per commit.
- D's extra cost falls in GC wall time (1.3-17x compaction).
- With scattered deletes, D frees less than C, because block alignment wastes 6-45 % of the dead bytes.

### E. Writes through mmap (separate spike, on A's arenas)

The question: can a put write its record through a map of the active arena, and so remove the temporary buffers? The baseline is the best pwrite path (`pwbuf`): one zstd context and one record buffer for each writer, with no allocation for each put.

CPU for each put, in us, median of 10 runs. The sync calls are not included. V2 = reserved arena, V3 = zero-filled arena.

| Workload, sync | today | `pwbuf` V2 | mmap V2 | `pwbuf` V3 | mmap V3 |
|---|---|---|---|---|---|
| random, none | 95.2 | 80.9 | 48.3 | 41.3 | 39.4 |
| random, every 64 puts | 94.2 | 84.3 | 49.5 | 43.5 | 40.8 |
| compressible, every 1 MiB | 159.0 | 148.7 | 113.6 | 112.1 | 104.4 |

- Most of the mmap gain on V2 comes from the cost of pwrite into reserved but unwritten blocks. On V3, mmap saves only 2-10 us for each put.
- One F_FULLFSYNC takes 21-42 ms (median). The mmap gain is 3 % or less of one sync interval.
- mmap on V2 has 2.49 major faults for each random put. mmap on V3 has no major faults.

Full disk, on a 64 MiB APFS image:

| Case | Result |
|---|---|
| Reserved (V2) or zero-filled (V3) arena, then volume full, then mmap write | ok |
| Punched hole filled again with pwrite of zeros, then mmap write | ok |
| Sparse arena or punched hole, then mmap write | no SIGBUS, msync ok, F_FULLFSYNC fails with ENOSPC, data lost after remount |
| Punched hole, then pwrite | ENOSPC at the write |
| F_PREALLOCATE after a punch | does not fill the hole (F_PEOFPOSMODE) or EINVAL (F_VOLPOSMODE) |
| Cloned arena (clonefile) | mmap and pwrite both lose data; the error comes only at F_FULLFSYNC |
| Forced detach while mapped | SIGBUS; pread and pwrite give EIO |

Crash loop: 240 kill -9 runs with mmap writes, 0 bad or missing synced records.

mmap writes are possible only if all of these are true: every arena has its own blocks before the first write; no mapped writes into punched holes (so no stack with C or D on the active arena); no clone or snapshot of the active arena; only F_FULLFSYNC counts as a good sync; compression moves under the store lock; and a gain shows on Apple Silicon. The gain is too small for these conditions.

## 3. Crash safety

Only process crashes (kill -9) were tested. A power-loss test is not possible on this machine.

| Suite | What it covers | Result |
|---|---|---|
| A `tests/ported_store.rs` | The chunk-store store, GC and file tests, ported to V0-V3 with 1 MiB arenas: torn tail, flipped byte with and without an idx, objects larger than an arena | 84 / 84 pass |
| A `tests/crash_a.rs` | Zero tail (record v1 and v2), torn record followed by a valid record, bad header, records after the idx hwm, crash in trim_on_seal, interrupted recovery cut, read-only open | 26 / 26 pass |
| B `tests/map_b.rs` | A write is visible through an existing map (200k objects, 113 arenas, 0 mismatches), unlink while mapped, SIGBUS in a child after a cut, 6 corrupt-Loc cases return None, torn tail cut before the map exists, V0 falls back to pread, wrong-length map refused | 7 / 7 pass |
| B with `ARENA_TEST_READ=mmap` | The A suites through the mmap path | 84 / 84 and 26 / 26 pass |
| C `tests/reuse_c.rs` | Reopen after close and after a crash at G = 1/64/512, torn record in a hole, inner fake record (good CRC, wrong BLAKE3) rejected, RULE W and RULE C fault injection, damaged dirty list, put and read during GC, gc_abort, a freed object does not come back | 15 / 15 pass |
| D `tests/punch_d.rs` (macOS and Linux) | Punch on V0-V3, punched arena with a missing or bad idx, chain walk without the mark (shows the loss), fake inner record rejected by BLAKE3, faults PunchAfterIdx and PunchMidway, put and read during GC, gc_abort, stale mark, punched record still in the idx (shows the loss) | 13 / 13 pass |
| D with `ARENA_GC=punch` | The A suites with the punch GC (2 asserts that only apply to compaction are skipped) | 84 / 84 pass |
| kill -9 crash loops, 50 kills each | A: V0-V3. B: V1M. C: C1, C3, C4, C4s, C4d4, and V1 rerun. D: V1D, V0D, at GC every 2 and every 16 rounds | 0 bad roots in every run |

Defects that the spikes found and fixed:

- **A:** `gc_finish` could delete the arena that was active at `gc_begin` after compaction had filled it with live copies. This was real data loss. A regression test now covers it.
- **C:** an empty arena was unlinked outside `reuse_gate`.
- **C:** the crash loop seldom reached its GC, because it ran GC every 16 rounds and a child is killed after about 3 rounds. This also applied to A's reported loops. The default is now GC after open and every 3 rounds. V1 rerun: 80,814 checks, 0 bad.

Facts that a design must respect:

- **Preallocation removes the "end of data" signal.** With a fixed file size, the end of the valid data is found by the record marker, CRC and decode. An all-zero tail fails at the marker in record v2.
- **Holes break the chain walk.** If a punched arena has no idx, the scan stops at the first hole and the cut destroys live data. A durable `.punched` mark and a resync scan (SEEK_DATA, marker search, CRC, BLAKE3 == id) fix this.
- **BLAKE3 is required in every scan that searches inside a file** (the C granule scan and the D resync scan). A fake inner record in old payload can pass the CRC.
- **In D, the idx must be rewritten before the punch.** If a durable idx still lists a punched record, get returns None, contains stays true, and a later put dedups onto the zeros. The object is then lost for good.
- **mmap has no SIGBUS handler.** A cut file or a media error kills the process. With pread, get returns None. The store itself never shrinks a mapped file.

## 4. Interactions

- **B depends on A, in the form that was built.** `MmapPath` maps only arenas with a fixed, known length. V0 grow files fall back to pread. Sealed packs also have a fixed length, but mapping them was not tested.
- **C depends on A.** It writes into fixed-size arenas with `write_at` and uses A's recovery arm.
- **D does not depend on A.** Punching works on V0 grow files (D0, V0D: crash loop clean). D and V2 conflict: a punch returns V2's reservation, so V2's "space is reserved" property is lost.
- **B + C:** C needs `reuse_gate`, a read guard for each get. B measured its uncontended cost as zero. The contended cost was not measured.
- **B + D:** they combine. A punch does not change the file length, so a punched page reads as zeros and gives no SIGBUS. A get with a stale Loc returns None.
- **C and D stack, but they conflict in three places:**
  1. D's `.punched` mark means "never written again". With reuse, C's dirty list and granule scan must replace it.
  2. The allocator must get a freed extent only after the punch, under the `reuse_gate` write lock.
  3. C's G = 64 granule leaves D's 4 KiB alignment waste. A 4 KiB granule would cost padding (C-sim: 67.5 % at G = 4096).
  The sync cost of a write into a hole is the same as for an overwrite, so the stack adds no new sync cost. But it keeps all of C's F_FULLFSYNC cost per commit, and that cost is why C was rejected.
- **D + compaction:** they complement each other. Compaction is good where D is weak: mostly-dead packs, small packs, and scattered deletes at 70-90 % dead. D is good where compaction frees nothing: 10-50 % dead.

## 5. Recommendation for `docs/design/chunk-store.md`

### Adopt, in this order

1. **Hybrid GC: punch plus compaction (D).** This changes the "GC" section.
   - Delete packs with no live object, as now.
   - Compact packs that are small, or below a live fraction to be set (start near 1/4).
   - Punch the block-aligned gaps between live records in all other sealed packs.

   Reasons: write amplification falls from about 1.6 to about 1.02. Space is freed at 10-50 % dead, where compaction frees nothing today. It works on today's grow packs, so it does not need A.

   Rules for the design doc:
   - Order: drop the dead entries from the index, then write the durable idx with a "punched" state, then punch.
   - Merge the mark into the idx (one `write_durable`). Write a state bit in idx v2 and a copy in the header.
   - Never cut a punched pack, and never make it the active pack. Recover it with the resync scan, not with the chain walk.
   - Run the punch syscalls outside the store mutex.
   - The GC API must say that a punch runs only after a durable root commit, and that the root and every snapshot must no longer refer to the records.
   - Change the sentence "A pack that a reader has open stays readable after its file is deleted": a get that looked up its Loc before `gc_begin` can now return None.
2. **fd LRU for the pread path.** With about 2,000 packs and ulimit 256, pread fails with EMFILE (B-B7). This is independent of the other changes. It is a small change.
3. **Sharded or RwLock index.** It is not in the spike scope, but B found it. At 16 threads the single index Mutex is the limit (11.7 us of CPU per lookup against 0.27 us at 1 thread), and the mmap gain falls to +1 %.
4. **mmap reads (B), after a decision on sparse cold reads.** This changes the "Read path" section.
   - Use MAP_SHARED, PROT_READ, the Default advice (never MADV_RANDOM), and only files with a fixed length.
   - Keep the no-shrink rules: the cut only in open(), and read-only opens use pread.
   - Document that RSS includes the mapped pages.
   - Adopt it only if sparse cold random reads are not important for SliceFS, or if a hybrid (pread when the page is not resident) is measured and works.

### Keep conditional

- **A (fixed-size arenas).** Adopt V1 (sparse) with `prepare_next` only if B is adopted, and only if B needs fixed-length active files. Measure `prepare_next` first: without it, the boundary put stalls 44-79 ms under the store mutex. A alone showed no measured gain. If B maps only sealed packs, A is not necessary.

### Drop

- **C (free-space reuse).** F_FULLFSYNC per commit rises 1.05-4.9x, put stalls of 20-40 ms come from the dirty-list writes, GC wall time rises up to 3x, and recovery is slower. D gets most of C's space gain, with no cost on the put path. If space becomes critical later, revisit C4d4 with the changes in the C result (V4, G = 64, no post-GC checkpoints, dirty-list writes batched per commit, a limit on arenas written per commit).
- **V2 (F_PREALLOCATE) and V3 (zero-filled).** No gain was measured. V2's reservation is lost after a punch. V3 makes slower arenas.
- **MADV_RANDOM.** Cold reads are 3.7-4.6x slower with it.
- **E (writes through mmap).** Use the `pwbuf` write path instead: one reused zstd context and record buffer for each writer.

### Open questions

1. What live-fraction threshold for punch or compact is best? The hybrid was not measured. Rerun `dgc` and `dchurn` with it.
2. How does D's GC time change with the pack count? Only 4 arenas were measured. With object churn, does D's space amp stop rising (1.106 to 1.226 over 6 rounds)?
3. How fast are cold reads next to holes (fragmentation)? Only warm reads were measured.
4. Are sparse cold random reads important for SliceFS? They decide B.
5. What to do about SIGBUS on media errors under mmap: accept it, or add a handler?
6. What is the effect of `prepare_next` on the boundary put (V1P, V2P in A-B1)?
7. All timings need a rerun on a quieter machine. N was 11-32 % for writes. Rerun A-B1 (all K), A-B2 to A-B6, B-B2 and B-B3.
8. Linux ext4: D ran only on OrbStack overlayfs, and B did not run on Linux. A-B7 was not run. On Linux, an unaligned punch succeeds but frees nothing, so `aligned_inside()` is needed on both systems.
9. Power-loss behaviour of the punch order and of the resync scan is untested.
10. Mapping sealed grow packs (B without A) is untested.