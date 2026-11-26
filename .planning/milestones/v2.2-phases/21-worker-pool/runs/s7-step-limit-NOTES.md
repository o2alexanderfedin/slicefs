# S7 with the 21-05 allocator setting at 1, 2, 4, 8 and 16 writers (plan 21-06)

The file name keeps "step-limit" because plans 21-08, 21-09 and 21-VALIDATION refer to it. No step limit was built. Per the 21-06 Amendment, these runs measure the 21-05 setting: `mallopt(M_MMAP_THRESHOLD, 128 KiB)` at the start of S7 (`alloc_tune::tune_at_start`).

- System: Docker Linux (OrbStack), `Linux 7.2.12-orbstack-00394-gbecdd6130068 x86_64`, `nproc` 32, harness form (in process, no mount), release build, volume `slicefs-target-p21`. The host load at the start was 14.39 and in the container it was 1.15.
- Tree: 839713b (21-05 allocator setting plus the #170 busy-store fix), tar stream of the committed tree.
- Date: 2026-11-24, 09:38 to 10:10 UTC, one container.
- Settings: `S7_FLUSHER=1`, default sizes (1024 MiB file and 128 MiB mix for each writer, a commit every 30 s), `S7_STORE_WAIT_MS` unset (`store_wait_ms=0`, the store default of 10 s).
- Order: three rounds of 4, 8, 16, 1, 2, 4 writers, then one more 4 (19 runs).
- Log: `runs/s7-step-limit.log`.

RUSTC = rustc 1.100.0 (0ef1bc386 2026-11-09)

## Result by the pass rule (written before the runs)

R1=pass
R2=miss
R3=miss

- **R1 (hard bar) passes.** All 16 runs at 1, 2, 4 and 8 writers passed their model checks (exit 0, 0 mismatches, 0 write errors) with VmHWM at or below 320 MiB. The largest values are 227 MiB at 4 writers and 290 MiB at 8 writers. Every run printed `mmap_threshold=131072`.
- **R2 (report only) misses.** All 3 runs at 16 writers are above 320 MiB (412, 413 and 426 MiB). The model checks passed (exit 0). The harness does not assert the cap above 8 writers (`cap_checked=false`).
- **R3 (report only) misses at 16 writers only.** Pm(8) - Pm(4) = 290 - 227 = 63, which passes (<= 64, but by only 1 MiB). Pm(16) - Pm(4) = 426 - 227 = 199, which misses.
- No EIO and no busy store: `write_busy_retries=0`, `flusher_busy=0` and `flusher_errors=0` in all 19 runs.

## Table

All memory values are in MiB. `live@pk` is `heap_live_at_peak_rss_mib` and `free@pk` is `malloc_free_at_peak_rss_mib`. `busy_retries` is `write_busy_retries`. `t_out` is `throttle_timeouts`, `t_wait` is `throttle_wait_max_ms` and `slowest` is `slowest_write_ms`. `s` is the "finished in" time of the test.

| threads | run | vmhwm_mib | within_cap | exit | s | mmap_threshold | live@pk | free@pk | save_steps | freed_bytes | busy_retries | flusher_busy | flusher_errors | t_out (of throttled) | t_wait | slowest | max_dirty | flusher_steps | flusher_drops |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 4 | 1 | 205 | true | 0 | 76.5 | 131072 | 135 | 56 | 226 | 532359552 | 0 | 0 | 0 | 175 (482) | 202 | 1569.7 | 67 | 113 | 13 |
| 8 | 1 | 271 | true | 0 | 124.9 | 131072 | 163 | 80 | 732 | 1074296896 | 0 | 0 | 0 | 776 (1513) | 308 | 1400.9 | 67 | 179 | 38 |
| 16 | 1 | 426 | false | 0 | 247.2 | 131072 | 240 | 128 | 1790 | 2154355328 | 0 | 0 | 0 | 1904 (4303) | 460 | 2315.3 | 67 | 405 | 59 |
| 1 | 1 | 106 | true | 0 | 28.8 | 131072 | 64 | 15 | 99 | 132702272 | 0 | 0 | 0 | 0 (0) | 0 | 720.4 | 31 | 0 | 0 |
| 2 | 1 | 170 | true | 0 | 26.3 | 131072 | 118 | 44 | 148 | 262049600 | 0 | 0 | 0 | 0 (41) | 199 | 335.5 | 65 | 50 | 3 |
| 4 | 2 | 217 | true | 0 | 56.1 | 131072 | 141 | 43 | 173 | 533277440 | 0 | 0 | 0 | 90 (448) | 201 | 421.8 | 67 | 110 | 2 |
| 4 | 3 | 212 | true | 0 | 55.7 | 131072 | 123 | 75 | 153 | 533959232 | 0 | 0 | 0 | 44 (431) | 201 | 495.5 | 66 | 122 | 1 |
| 8 | 2 | 281 | true | 0 | 118.6 | 131072 | 162 | 90 | 648 | 1071554456 | 0 | 0 | 0 | 633 (1577) | 203 | 1505.8 | 67 | 209 | 34 |
| 16 | 2 | 413 | false | 0 | 291.9 | 131072 | 244 | 116 | 2258 | 2153808744 | 0 | 0 | 0 | 2542 (4559) | 442 | 7105.2 | 69 | 413 | 58 |
| 1 | 2 | 106 | true | 0 | 40.0 | 131072 | 79 | 4 | 98 | 131635328 | 0 | 0 | 0 | 0 (0) | 0 | 1568.7 | 32 | 0 | 0 |
| 2 | 2 | 168 | true | 0 | 34.6 | 131072 | 112 | 37 | 131 | 264223616 | 0 | 0 | 0 | 2 (63) | 200 | 273.2 | 65 | 64 | 1 |
| 4 | 4 | 199 | true | 0 | 75.8 | 131072 | 139 | 47 | 215 | 533953536 | 0 | 0 | 0 | 151 (466) | 273 | 517.5 | 67 | 115 | 9 |
| 4 | 5 | 205 | true | 0 | 69.0 | 131072 | 129 | 58 | 256 | 532756928 | 0 | 0 | 0 | 224 (497) | 215 | 433.8 | 67 | 109 | 15 |
| 8 | 3 | 290 | true | 0 | 153.5 | 131072 | 161 | 89 | 793 | 1073239784 | 0 | 0 | 0 | 853 (1535) | 328 | 2062.5 | 68 | 210 | 42 |
| 16 | 3 | 412 | false | 0 | 275.0 | 131072 | 206 | 137 | 1841 | 2153318784 | 0 | 0 | 0 | 1959 (4125) | 639 | 7453.3 | 67 | 349 | 60 |
| 1 | 3 | 101 | true | 0 | 48.9 | 131072 | 68 | 13 | 98 | 131645632 | 0 | 0 | 0 | 0 (0) | 0 | 781.2 | 32 | 12 | 0 |
| 2 | 3 | 173 | true | 0 | 28.0 | 131072 | 123 | 39 | 136 | 263537216 | 0 | 0 | 0 | 1 (61) | 200 | 388.8 | 65 | 59 | 2 |
| 4 | 6 | 213 | true | 0 | 59.1 | 131072 | 135 | 55 | 190 | 534245760 | 0 | 0 | 0 | 111 (462) | 202 | 424.4 | 66 | 113 | 11 |
| 4 | 7 | 227 | true | 0 | 59.7 | 131072 | 125 | 88 | 203 | 532153920 | 0 | 0 | 0 | 56 (381) | 202 | 775.9 | 66 | 151 | 3 |

## Pm(N), Spread(N) and CAP(N)

Pm(N) is the largest VmHWM at N writers. Spread(N) is the largest minus the smallest.

| N | VmHWM values | Pm(N) | Spread(N) |
|---|---|---|---|
| 1 | 106, 106, 101 | 106 | 5 |
| 2 | 170, 168, 173 | 173 | 5 |
| 4 | 205, 217, 212, 199, 205, 213, 227 | 227 | 28 |
| 8 | 271, 281, 290 | 290 | 19 |
| 16 | 426, 413, 412 | 426 | 14 |

CAP(N) = Pm(N) + F + R, rounded up to a multiple of 8. F = 8. R = the largest of 16, ceil(Pm(N) / 10) and 2 x Spread(N).

- CAP(4): R = max(16, 23, 56) = 56, so 227 + 8 + 56 = 291, rounded up to 296.
- CAP(8): R = max(16, 29, 38) = 38, so 290 + 8 + 38 = 336, rounded up to 336.
- CAP(16): R = max(16, 43, 28) = 43, so 426 + 8 + 43 = 477, rounded up to 480.

CAP(4) = 296
CAP(8) = 336
CAP(16) = 480
CAP_MIB = 320

These values are recorded only. `CAP_MIB` stays 320 (`const CAP_MIB: u64 = 320` in `stress_harness.rs`). CAP(8) = 336 is above 320 because of the research margin rule. The measured Pm(8) = 290 is 30 MiB below the cap.

## Before and after the setting, at the same writer count

The "before" runs had no allocator setting. The 4-writer runs are in `runs/s7-throttle-200ms.log` (262 to 340 MiB in 7 runs, as the plan states; that log also has runs of an earlier commit). The 1-, 2-, 8- and 16-writer runs are in `runs/s7-before-limit.log` (2dc170d).

| N | Before: VmHWM (Pm) | After: VmHWM (Pm) | Pm after - before |
|---|---|---|---|
| 1 | 90, 95, 97 (97) | 106, 106, 101 (106) | +9 |
| 2 | 198, 208, 216 (216) | 170, 168, 173 (173) | -43 |
| 4 | 262 to 340 (340) | 199 to 227 (227) | -113 |
| 8 | 419, 423, 441 (441) | 271, 281, 290 (290) | -151 |
| 16 | 688, 700, 730 (730) | 412, 413, 426 (426) | -304 |

The growth for each writer above 2 fell from about 35 MiB to about 19 MiB from 2 to 8 writers ((290 - 173) / 6 = 19.5 by Pm, (281 - 170) / 6 = 18.5 by median). From 8 to 16 writers it is about 17 MiB for each writer ((426 - 290) / 8). At 1 writer the value is about 9 MiB higher. A possible cause is the page rounding of mmapped blocks; this was not examined.

## Where the memory is at 16 writers (S7 heap peak sample)

These values are from the RSS peak sample of the `S7 heap` line (sampled every 50 ms): RSS = anon + file. Live is `mallinfo2` uordblks + hblkhd. All values are in MiB, as median (range).

| N | RSS at peak | Live heap | Anon - live | File-backed |
|---|---|---|---|---|
| 1 | 85 (84-87) | 68 (64-79) | 12 (3-15) | 4 |
| 2 | 165 (155-168) | 118 (112-123) | 38 (37-45) | 4 |
| 4 | 207 (198-227) | 135 (123-141) | 57 (42-87) | 14 |
| 8 | 286 (273-292) | 162 (161-163) | 89 (78-96) | 31-34 |
| 16 | 417 (415-431) | 240 (206-244) | 127 (111-136) | 61-72 |

- The free memory that malloc keeps (anon - live) is now 89 MiB at 8 writers. Before the setting it was 253 MiB (diagnosis row 3). This is the effect of the setting.
- Above 8 writers, most of the rise comes from two parts that the setting does not change. The live heap rises by about 78 MiB (about 10 MiB for each writer). The file-backed RSS rises by about 30 MiB and follows the stored bytes (16 GiB at 16 writers; inferred to be the mapped index run files, as in the diagnosis). The kept free memory rises by about 38 MiB.
- So the 16-writer miss (R2) and the R3 rise are now mostly live data and mapped files, not arena retention. A step limit or a buffer reuse pool would act on the live part. Neither was built (the plan rule).

## The harness term

The S7 process itself holds this memory for each writer (from the code, `stress_harness.rs`):

- The block-version model `ver`: one `u32` for each 4 KiB block, so 1 MiB for a 1024 MiB file. It is kept until the check ends.
- The write buffer `buf`: 1 MiB.
- In the check phase, one 1 MiB read result at a time (after the write peak).
- The thread stack: a few touched pages.

That is about 2 MiB for each writer at the write peak, plus 1 MiB in the check phase, so about 3 MiB for each writer at most. The 1- and 2-writer runs agree with this. The step from 1 to 2 writers (+67 MiB by Pm) is mostly the second writer's dirty data (`max_dirty_mib` 31-32 to 65) and the live heap that the store and the flusher use (diagnosis part B). It is not the harness term.

Pm(N) minus the harness term (3 MiB x N): 103 (1), 167 (2), 215 (4), 266 (8), 378 (16). The harness term does not explain the rise. The bar applies to the measured VmHWM as it is.

## Slowest write

Research Q9 names a slowest write above 500 ms, with time limits in most throttle waits, as a reason for a delay curve (a Phase 20 deferred item). Here, the slowest write is above 500 ms in 12 of 19 runs (up to 2.1 s at 8 writers and 7.5 s at 16 writers). But the throttle waits that reach their time limit are a minority at 4 writers (10 to 45 %) and about half at 8 and 16 writers (40 to 56 %). So the Q9 condition holds only as a borderline case at 8 and 16 writers, and does not hold at 4 writers. The cause of the 7.1 s and 7.5 s writes at 16 writers is not identified. In those runs the largest throttle wait is 442 and 639 ms, the largest commit gate wait is 519 and 227 ms, and the largest commit is 1443 and 713 ms. These do not add up to 7 s. `write_busy_retries=0` shows only that no write met a busy store.

## Open points for the owner (R2 and R3, report only)

- **R2=miss:** 16 writers give 412 to 426 MiB, above 320. Before the setting the value was 688 to 730 MiB.
- **R3=miss:** Pm(16) - Pm(4) = 199 MiB (rule: <= 64). Pm(8) - Pm(4) = 63 MiB passes by 1 MiB.
- The cause above 8 writers is mostly the live heap (about 10 MiB for each writer) and mapped files that follow the stored bytes. It is not malloc retention. If the owner wants 16 writers under 320 MiB, the options are the step limit, the byte step pool (commit 964e13e) or a buffer reuse pool, measured on the live part. 16 writers is above the default worker cap of 8.
- No R1 miss, so the `MALLOC_ARENA_MAX=2` branch run did not apply and was not made.
- A 7.5 s write at 16 writers is near the 10 s store wait. A write that meets a busy store now retries (#170), but the margin is small at 16 writers.
- CAP(8) = 336 is above CAP_MIB 320. Pm(8) = 290 is 30 MiB below the cap, from only 3 runs (Spread 19 at 8 writers, 28 at 4). A later 8-writer run (for example in 21-08) can miss the assert.
