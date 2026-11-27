---
phase: 21-worker-pool
plan: 05
subsystem: memory
status: complete
tags: [pool-05, allocator, mallopt, s7, stats]

requires:
  - phase: 21-worker-pool
    provides: "21-04: S7_THREADS knob; the s7mem probe (4d51fbf); runs/s7-memory-diagnosis-NOTES.md"
provides:
  - "crates/slicefs-cli/src/alloc_tune.rs: tune_at_start() (Linux glibc mallopt(M_MMAP_THRESHOLD, 128 KiB), no-op elsewhere), stats() -> Option<AllocStats> (live_bytes, free_bytes, mmap_bytes; glibc mallinfo2, macOS malloc_zone_statistics, None elsewhere), MMAP_THRESHOLD"
  - "run_mount calls tune_at_start() first and prints `Allocator: mmap threshold 131072 bytes (fixed)` (or `Allocator: system default`)"
  - "s7_budget_baseline calls tune_at_start() first; s7mem reads alloc_tune::stats(); S7 line ends with mmap_threshold= heap_live_at_peak_rss_mib= malloc_free_at_peak_rss_mib= save_steps= freed_bytes="
  - "FlusherStats.save_steps and .freed_bytes (published save steps from every thread, and the dirty cost they freed); SliceFsFilesystem::steps_line(); the unmount prints one `steps:` line"
  - "mt-linux.sh smoke checks the Allocator line and prints the steps line"
affects: [21-06, 21-08]

estimate:
  tokens: 110000
  tasks: 2
actuals:
  tokens: 5900    # chars/4 over the realized diff (23547 chars, 11 files)
  tasks: 2
  commits: 1

plan_head_before: 38910f713dfeabeb6ce7c8e2985fc3221d0d287f
plan_head_after: 9fa563a3e16e7b3d8f5990aada34495936d7360d
commits: 1

tech-stack:
  added: []
  patterns:
    - "All allocator FFI in one module, cfg-gated free functions, no trait"
    - "A process-wide allocator probe lives in its own test binary; the other test in it makes only blocks below the threshold"

key-files:
  created:
    - crates/slicefs-cli/src/alloc_tune.rs
    - crates/slicefs-cli/tests/alloc_tune_tests.rs
    - .planning/phases/21-worker-pool/runs/21-05-red.log
  modified:
    - crates/slicefs-cli/src/lib.rs
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/flusher.rs
    - crates/slicefs-cli/src/fuse_callbacks.rs
    - crates/slicefs-cli/tests/stress_harness.rs
    - crates/slicefs-cli/tests/flusher_tests.rs
    - scripts/docker-linux-test/mt-linux.sh
    - .planning/phases/21-worker-pool/21-05-PLAN.md

decisions:
  - "Amendment 2 replaces the step limit: no SAVE_STEPS, slots, StepSlot, pay_skips or byte step pool. The plan must_haves, artifacts, key_links and files_modified were rewritten to Amendment 2"
  - "tune_at_start() is the first line of run_mount (before dirty.check and before any mount thread), not in main: the mount log prints its result there"
  - "The s7mem probe no longer calls malloc_trim(0) (no trim(), no allocator call outside alloc_tune); the trim fields left the S7 heap line"
  - "AllocStats has a third field, mmap_bytes (glibc hblkhd), so the threshold test reads it through alloc_tune and not through libc"
  - "save_steps and freed_bytes count only rebase_unlocked steps that published (Ok(true) from step_publish); freed = plan cost_before minus the overlay cost after the publish"

metrics:
  duration: "about 40 min"
  completed: 2026-11-24
---

# Phase 21 Plan 05: Fixed glibc mmap threshold at daemon start, allocator numbers in S7, save-step counters Summary

The daemon now calls `mallopt(M_MMAP_THRESHOLD, 128 KiB)` on Linux glibc as the first step of `run_mount` and logs `Allocator: mmap threshold 131072 bytes (fixed)`. S7 makes the same call before its threads start, and its report line now gives the live heap and the free memory that malloc keeps (both from `alloc_tune::stats()`), plus the save-step counters. The step limit was not built (Amendment 2).

## What was built

- **`alloc_tune.rs` (new).** These are the only allocator calls in the workspace. `tune_at_start() -> Option<usize>` calls `mallopt` on `target_os = "linux", target_env = "gnu"` and returns `Some(131072)` when `mallopt` returns 1. On other targets it returns `None`. `stats() -> Option<AllocStats>` gives `live_bytes` (`uordblks + hblkhd`), `free_bytes` (`fordblks`) and `mmap_bytes` (`hblkhd`) from `mallinfo2`. On macOS it gives in use, allocated minus in use, and 0, from `malloc_zone_statistics(NULL)`. Other targets get `None`.
- **Daemon.** `run_mount` calls `tune_at_start()` first. After the `Workers:` line it prints `Allocator: mmap threshold <n> bytes (fixed)` or `Allocator: system default`.
- **S7.** `s7_budget_baseline` calls `tune_at_start()` first. The `s7mem` probe now has one cfg-free form on top of `alloc_tune::stats()`. On a system with no numbers it gives `None` and prints nothing. The `S7 heap` line lost its trim fields and `end_arena_mib`. `end_free_in_arenas_mib` is now named `end_malloc_free_mib`. These fields were added at the end of the S7 line: `mmap_threshold=`, `heap_live_at_peak_rss_mib=`, `malloc_free_at_peak_rss_mib=` (this is `fordblks`, not the NOTES "Free kept" column, which is RssAnon minus live), `save_steps=` and `freed_bytes=`.
- **[Stats] counters.** `FlushCtl.save_steps`, `freed_bytes` and `started_ms` (set when the flusher loop starts). `FlusherStats.save_steps` and `freed_bytes`. `steps_line()` gives `steps: save_steps=<n> steps_per_s=<x> freed_per_step=<bytes> freed_bytes=<n>`. `MountedFs::destroy` prints it after `shutdown()`.

## Mount-log line (Docker Linux smoke, `run.sh smoke`, volume slicefs-target-p21)

```
Allocator: mmap threshold 131072 bytes (fixed)
steps: save_steps=1 steps_per_s=0.60 freed_per_step=0 freed_bytes=0
```

The one save step of the smoke freed 0 bytes of cost. This is probably the release step of the truncated `e.txt` (an overlay with a size change and no extents). It was not examined further.

## TDD

- **Red 1 (Docker Linux):** `tune_at_start` as a stub that returns `Some(131072)` with no `mallopt`. `tune_at_start_fixes_the_mmap_threshold` failed with `a 1 MiB block after a 4 MiB free is not mmapped: hblkhd 0 -> 0`. Green after the real call: 2 passed. Log: `runs/21-05-red.log`.
- **Red 2 (macOS host):** the two counter adds were removed from `rebase_unlocked`. `the_flusher_lowers_the_total` failed at `the pass counted its save steps: ... save_steps: 0, freed_bytes: 0`. Green after the adds. In the same log.
- Neither red state was committed.

## Test results

- macOS host: `cargo test --workspace` exit 0, 1522 passed, 0 failed, 12 ignored, 73 test binaries.
- Docker Linux (slicefs-target-p21, rustc 1.100.0): `alloc_tune_tests` 2 passed; `flusher_tests` 26 passed.
- Docker Linux S7, small flusher form (`S7_THREADS=4 S7_FILE_MIB=8 S7_WRITE_MIB=4 S7_COMMIT_SECS=1 S7_FLUSHER=1`): ok in 8.7 s, with `vmhwm_mib=67 within_cap=true mmap_threshold=131072 heap_live_at_peak_rss_mib=40 malloc_free_at_peak_rss_mib=6 save_steps=4 freed_bytes=12838912`.
- Docker Linux mount smoke (`run.sh smoke`): every step ok. `allocator-line` ok, `daemon-exit` ok.

## Deviations from Plan

### Amendment 2 overrides (by the owner, not auto-fixes)

- The step-limit tasks (SAVE_STEPS, StepSlot, admit_release, Need::, NoSlot, the step-limit tests, no_take_out guard) were not built. The plan frontmatter (`files_modified`, `must_haves`, artifacts, key_links) was rewritten to Amendment 2. The acceptance greps of the old tasks (`const SAVE_STEPS`, `admit_release`, `Need::Must`, `save_steps_max`, `slot_overruns`) no longer apply.
- `steps:` line: only the counters that still apply (save steps, steps per second, freed per step). The slot fields are not there.

### Auto-handled

**1. [Rule 3 - Blocking] The s7mem probe called `malloc_trim(0)`.** "No trim()" and "no glibc call outside the module" together mean that the call goes. The probe lost `rss_after_trim_mib`, `anon_after_trim_mib` and `trim_released`. It now also measures what the daemon does (no trim). Commit 9fa563a.

**2. [Rule 2 - Missing check] Smoke check of the daemon line.** `mt-linux.sh` now fails when the Linux mount log has no `Allocator: mmap threshold 131072 bytes (fixed)` line. This is the daemon-level proof of the call. It also prints the `steps:` line. Commit 9fa563a.

**3. POOL-05 not marked complete.** The plan lists `requirements: [POOL-05]`, and `requirements.mark-complete` checked it. That change was reverted: POOL-05 asks for the scaling of parallel I/O to be measured and recorded, and 21-06 does that. 21-04 also lists POOL-05 and left it open.

**4. One code commit, not one per task.** The two parts share `stress_harness.rs` and `mt-linux.sh`. Only the combined tree was tested (workspace and Docker), so it was committed as one.

## Known Stubs

None.

## Open points for 21-06

- **21-06-PLAN.md needs its own amendment before it runs.** It still expects the step limit: `contains: "save_steps_max="`, the S7 line fields `save_steps_max=`, `slot_waits=`, `slot_timeouts=`, `pay_skips=`, `slot_skips=`, `slot_overruns=` (lines 29, 68, 110, 128), and "on the committed tree with the step limit" (line 22). The ROADMAP Wave 6 line also says "S7 with the step limit". The S7 line now has `mmap_threshold=`, `heap_live_at_peak_rss_mib=`, `malloc_free_at_peak_rss_mib=`, `save_steps=` and `freed_bytes=`.
- Measure S7 at 1, 2, 4 and 8 writers with the mallopt call. Only a run over CAP_MIB 320 brings back the step limit (gap plan; the byte step pool is in commit 964e13e).
- The macOS `stats()` path is used only by `stats_show_a_live_heap`. S7 is not run on macOS.
- Unexplained: `freed_bytes=0` for the one save step of the smoke. This does not block: the S7 run (4 steps, 12838912 bytes) and the exact assert in `the_flusher_lowers_the_total` show that the counter works.
- The full Linux workspace suite was not run in Docker. Only the touched tests were run: `alloc_tune_tests`, `flusher_tests`, the small S7 and the smoke.

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/src/alloc_tune.rs, crates/slicefs-cli/tests/alloc_tune_tests.rs, .planning/phases/21-worker-pool/runs/21-05-red.log
- FOUND: commit 9fa563a in HEAD
