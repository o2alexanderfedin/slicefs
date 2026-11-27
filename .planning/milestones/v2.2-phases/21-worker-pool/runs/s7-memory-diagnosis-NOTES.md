# S7 memory diagnosis: why VmHWM grows by about 35 MiB for each writer

Question: in the S7 flusher form, VmHWM grows by about 35 MiB for each writer above 2 (21-04: 95, 208, about 295, 423, 700 MiB at 1, 2, 4, 8, 16 writers), but `max_dirty_mib` stays at 64 to 66. What holds the memory?

## Verdict

Mostly A. B explains the step from 1 to 2 writers, not the slope above it.

The shares come from the RSS peak sample, where the parts add up to the VmHWM: VmRSS = live heap + free memory kept by malloc + file-backed RSS.

| Writers | RSS at peak | Live heap | Free kept (anon - live) | File-backed |
|---|---|---|---|---|
| 1 | 92 | 68 | 19 | 4 |
| 2 | 180 | 119 | 56 | 4 |
| 4 (x 2048 MiB, row 7) | 303 | 114 | 154 | 35 |
| 8 | 409 | 110 | 253 | 45 |

- **A (about 33 of the 37 MiB for each writer from 2 to 8): glibc malloc keeps freed memory in one arena for each writer thread.** The free kept memory grows from 56 to 253 MiB. Each of two environment settings alone removes about 140 MiB at 8 writers (403 -> 262 and 264 MiB, below the 320 MiB cap), and the peak live heap does not change with them (173 to 176 MiB). This was one run for each setting, at 8 writers only.
- **B (live heap): flat above 2 writers.** The live heap at the RSS peak is 119, 114 and 110 MiB at 2, 4 and 8 writers. B gives the step from 1 to 2 writers (live +51 MiB, of which 32 MiB is dirty data: 1 writer reaches only 32 MiB of dirty data). The largest live heap of each run (any sample) is 69, 129 and 174 MiB at 1, 2 and 8 writers, but that peak is at another time than the RSS peak. When A is fixed, B is the largest part (row 5: 267 = live 175 + free 57 + file 34).
- **File-backed RSS (about 7 MiB for each writer from 2 to 8; inferred to follow the stored data).** `RssFile` is 4 MiB at 1 and 2 writers and 32 to 45 MiB at 4 and 8 writers. Inferred, not measured: these are the read-only mappings of the index run files. Support: `crates/chunk-store/src/pack_io.rs:263` is the only `mmap` in the production code, and `RssFile` is about the same at the same stored bytes (32 and 35 at 4 x 2048, 34 to 45 at 8 x 1024).

The growth follows the writer count, not the bytes: 4 writers with 2048 MiB files (row 7) give 297 MiB, the same as the 21-04 result for 4 writers with 1024 MiB files (262 to 340, median 295). Two times the bytes at the same writer count gave no change. 8 writers with the same bytes as row 7 give 403 to 441 MiB. Row 6 is not a valid point (write errors, 124 MiB dirty; see below).

`rss_after_writes_mib` of the S7 line comes from `getrusage` `ru_maxrss`, which is a peak. It is equal to the peak because it is the same value, not because memory does not come back. The current RSS after the writes (`end_rss_mib`) is close to the peak, and `malloc_trim(0)` then gives back about 100 MiB at 8 writers.

## Allocator

The `stress_harness` test binary has no `#[global_allocator]` (only `memory_bound_tests`, `dirty_budget_tests`, `replay_memory_tests` and `gc_full_memory_tests` have one, a counting wrapper), and no crate of the workspace uses jemalloc or mimalloc. So S7 uses glibc malloc. The probe confirms it: `mallinfo2` shows 58 to 197 MiB in use.

## Runs

Docker Linux, harness form (in process, no mount), release, tar stream of the working tree at 1a0c354 plus the test-only probe, volume `slicefs-target-p21`, `rustc 1.100.0 (0ef1bc386 2026-11-09)`, `nproc` 32, one container at a time, `S7_SERIALIZE=0 S7_FLUSHER=1`, default sizes (1024 MiB file and 128 MiB mix for each writer, commit every 30 s) unless the table says other. One run for each row. Log: `runs/s7-memory-diagnosis.log`.

All values in MiB. "At RSS peak" is the 50 ms sample with the largest VmRSS during the writes: VmRSS (RssAnon, RssFile, live heap). Live heap = `mallinfo2` uordblks + hblkhd. "Free kept" = RssAnon minus live heap at that sample. "End" is after the writers are joined, before and after `malloc_trim(0)`.

| Run | Writers x file | Environment | VmHWM | max_dirty | slowest_write_ms | Wall s | Peak live | At RSS peak: RSS (anon, file, live) | Free kept | End RSS -> after trim (anon after trim, end live) |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 1 x 1024 | none | 95 | 32 | 112.5 | 14.7 | 69 | 92 (87, 4, 68) | 19 | 88 -> 72 (68, 58) |
| 2 | 2 x 1024 | none | 180 | 64 | 99.8 | 23.3 | 129 | 180 (175, 4, 119) | 56 | 174 -> 131 (126, 107) |
| 3 | 8 x 1024 | none | 403 | 66 | 1168.1 | 125.5 | 174 | 409 (363, 45, 110) | 253 | 405 -> 304 (259, 93) |
| 0 | 8 x 1024 | none (first probe form, end sample only) | 433 | 65 | 780.1 | 134.8 | - | - | - | 436 -> 319 (-, 88) |
| 4 | 8 x 1024 | `MALLOC_ARENA_MAX=1` | 262 | 65 | 799.0 | 167.9 | 173 | 268 (225, 42, 127) | 98 | 250 -> 149 (106, 94) |
| 5 | 8 x 1024 | `MALLOC_MMAP_THRESHOLD_=131072` | 264 | 66 | 643.3 | 111.2 | 176 | 267 (232, 34, 175) | 57 | 237 -> 136 (111, 96) |
| 6 | 4 x 2048 | none | 278 | 124 | 16995.3 | 244.5 | 197 | 284 (251, 32, 126) | 125 | failed, see below |
| 7 | 4 x 2048 | none (repeat) | 297 | 65 | 498.9 | 106.2 | 151 | 303 (268, 35, 114) | 154 | 301 -> 182 (147, 101) |

The earlier 8-writer runs (21-04) were 419 to 441 MiB, so a single run has a spread of about 30 MiB. Only differences above that are real. Rows 3 and 0 (403 and 433) are in that range. Wall time at 8 writers without a setting is 101 to 135 s (21-04 and rows 0 and 3): row 5 (111 s) is in that spread, so it shows no speed gain; row 4 (168 s) is above it. Some RSS samples are a few MiB above the VmHWM (409 against 403); VmRSS and VmHWM are read at different times, and differences of this size are read noise.

## Why A scales with the writers

- glibc gives each thread that allocates its own arena (up to 8 x the CPU count). In S7 each writer thread calls the write handler directly, so each writer has its own arena. A block freed by another thread (for example the flusher) goes back to the arena that gave it.
- glibc moves the mmap threshold up after the first free of an mmapped block (dynamic threshold, up to 32 MiB). After that, the 1 MiB class buffers of a write come from the arena of the thread, not from mmap. Freed arena memory stays in the arena: only the top above the trim threshold (also moved up) goes back, and holes in the middle go back only with `malloc_trim`.
- So the RSS is about the sum of the peak of each arena, not the peak of the sum. Each writer arena keeps about 30 MiB.
- Evidence: with one arena (row 4) or with a fixed 128 KiB threshold (row 5) the free kept memory at the peak falls from 253 to 98 and 57 MiB. At 8 writers without a setting, `malloc_trim(0)` leaves 259 MiB anonymous for 93 MiB live (166 MiB in pages that have live blocks in them: fragments across 8 arenas). With one arena it leaves 106 for 94 live.
- Not measured: which call sites make the blocks of 128 KiB and more. The likely ones are the copies of the write data, the base reads of the rebase steps and the buffers of a store put.

## What B is (estimates from the code, not measured)

The live heap that each writer adds is not dirty data. From the code, for each writer:

- Test model: `ver`, one `u32` for each 4 KiB block, 1 MiB for a 1024 MiB file (`crates/slicefs-cli/tests/stress_harness.rs:2113`). Test only.
- Test write buffer: 1 MiB (`stress_harness.rs:2115`). Test only.
- The per-thread `Scratch` of the chunk store: a zstd context and two buffers kept up to `SCRATCH_KEEP` = 1 MiB each (`crates/chunk-store/src/lib.rs:359` and `:363`). One for each thread that puts objects.

That is about 4 to 6 MiB for each writer. Other live state follows the stored data, not the threads: the index memtable (up to `index_memtable_limit` = 1<<18 entries, `crates/chunk-store/src/lib.rs:186`, and a frozen copy while a flush runs). The mapped run files (`crates/chunk-store/src/pack_io.rs:263`) count in RssFile, not in the heap. A heap profile was not made, because the live heap is far below the RSS (row 3: 110 live at 409 RSS) and does not grow above 2 writers; B is not the main cause.

## Fix it points to

1. **Set the allocator at daemon start (main cause, A).** In `main` of `slicefs-cli`, before any thread starts, call `mallopt(M_MMAP_THRESHOLD, 128 KiB)` on Linux glibc. A fixed threshold also stops the dynamic threshold and trim threshold. In row 5 this gave 264 MiB at 8 writers and was not slower (111 s wall, in the spread of 101 to 135 s without a setting). It costs an mmap and munmap call for each block of 128 KiB or more. `M_ARENA_MAX` (row 4) gave the same memory but a longer wall time (168 s, above the spread); a small arena count (2 to 4) may be a middle point, not measured. Both are reversible and need no format change.
2. **Reuse buffers (A, the root of it).** The large blocks that a write makes and frees could come from a pool or be kept per thread (as `Scratch` already does for the store put). This makes the RSS less dependent on the allocator. More work; do it after a measurement of which call sites make the blocks.
3. **B needs no fix now.** The per-thread `Scratch` (up to about 2 MiB plus the zstd context) is by design. The test model and buffer (2 MiB for each writer) are test-only and count in the S7 VmHWM; the line already says that.

The cap (320 MiB) and the production defaults were not changed.

## Other finding: write EIO in row 6

Row 6 (4 x 2048 MiB, seed 1795492903707110156) failed: 4 writes returned errno 5 (EIO), `flusher_errors=3`, `commit_total_max_ms=45634`, `slowest_write_ms=16995.3`, `max_dirty_mib=124` (above the budget). The read check then found one block for each file that did not have version 1 (the block of the failed write). The repeat (row 7) with the same sizes passed with no error. The store wait limit is 10 s (`STORE_WAIT`, `crates/chunk-store/src/lib.rs:310`), so a store wait that timed out under a long commit is a possible cause. Not examined further: the debug lines of that run were not kept. This is a separate issue from the memory question.

## Probe

`crates/slicefs-cli/tests/stress_harness.rs`, module `s7mem` (Linux glibc only; empty on other systems): a sampler thread in the S7 scope reads `mallinfo2` and `/proc/self/status` every 50 ms during the writes, then one `S7 heap` line gives the two peak samples, the state at the end of the writes and the RSS after `malloc_trim(0)`. At 2 writers and more the trim runs after the RSS peak (`rss_after_writes_mib` equals `vmhwm_mib`), so it does not change VmHWM. At 1 writer the check phase sets the peak after the trim (91 against 95); 95 is also the 21-04 median without the trim. It is test-only and costs one thread that sleeps.
