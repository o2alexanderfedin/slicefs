---
phase: 18-spikes-baseline-and-stress-harness
fixed_at: 2026-11-21T00:00:00Z
review_path: .planning/phases/18-spikes-baseline-and-stress-harness/18-REVIEW.md
iteration: 1
findings_in_scope: 6
fixed: 6
skipped: 0
status: all_fixed
---

# Phase 18: Code Review Fix Report

**Fixed at:** 2026-11-21
**Source review:** .planning/phases/18-spikes-baseline-and-stress-harness/18-REVIEW.md
**Iteration:** 1

**Summary:**
- Findings in scope: 6 (WR-01 to WR-06; Info findings are out of scope for `critical_warning`)
- Fixed: 6
- Skipped: 0

**Where verification ran:** the main checkout (`workflow.use_worktrees` is `false`), branch `feature/phase-18-spikes-harness`. No spike was re-run and no mount was made. The recorded run logs and the numbers in `docs/research/multithreaded-request-handling.md` were not edited.

**Checks (all exit 0):**
- `cargo test -p slicefs-cli --test stress_harness` (3 passed, 6 ignored)
- `cargo check --workspace --all-targets`
- `cargo check -p slicefs-cli --features lock-stats --tests --examples`
- `bash -n` on the changed scripts, and `ast.parse` on the embedded Python of `s1-overlap.sh` and `s6-smallfsync.sh`
- Two tiny smoke runs of the ignored tests (not measurements): `S6_FILES=10 s6_small_fsync` and `S7_FILE_MIB=4 S7_WRITE_MIB=2 S7_COMMIT_SECS=1 s7_budget_baseline`. Both pass and print the new fields.

## Fixed Issues

### WR-01: S1 probe decrements `inflight` after the reply

**Files modified:** `crates/slicefs-cli/examples/s1_probe.rs`
**Commit:** 93cf284
**Applied fix:** Added `finish(st, seq)`, which updates `last_replied` (inversion count) and `inflight` as the reviewer suggested. The worker calls it before `reply.data`, `reply.entry` and `reply.attr`. The REP log line still prints after the reply.
**Can it change a recorded spike number:** Yes, in one direction only. The S1 `max_inflight` (2 on macOS, 3 on Linux) can only stay the same or go down on a re-run, because the fix removes the case where `inflight` is too high. It cannot raise it. `reply_order_inversions` is now counted when the reply is about to leave, so a re-run could differ by a small amount. The recorded values were probably right (the race is rare), but they were measured with the old ordering. The doc can say: "measured with the probe before commit 93cf284; max_inflight may have been inflated by at most the rare reply/next-request race".
**Note (logic change):** fixed: requires human verification (ordering of counters).

### WR-02: S1 client metric `fast_stats_during_slow` can be 50 without any overlap

**Files modified:** `crates/slicefs-cli/examples/s1_probe.rs`, `scripts/macos-mount-test/s1-overlap.sh`
**Commit:** 5e737b0
**Applied fix:** The probe now adds ` slow_read` to the ARR line of the slow READ. The client round waits (up to 5 s, polling every 5 ms) until `probe.err` has a new `slow_read` line, then starts the 50 stats. The `round=` line has a new field `first_stat_after_arr_ms`. If the probe never logs the slow READ, the round fails with exit 5. The probe's counters and the `S1 RESULT` line are unchanged.
**Can it change a recorded spike number:** It does not change the probe's `S1 RESULT` numbers (`max_inflight`, `reply_order_inversions`). It changes how the client metrics `fast_stats_during_slow` and `max_stat_ms` are produced, so the recorded 50 per round was measured with the old client (fixed 50 ms sleep) and a re-run uses the new rule. The old 50 per round was not proof of overlap; the doc should rest the S1 conclusion on `reply_order_inversions`, as the review says.

### WR-03: S6 `fsyncs_per_s` has two definitions

**Files modified:** `scripts/docker-linux-test/s6-smallfsync.sh`, `crates/slicefs-cli/tests/stress_harness.rs`
**Commit:** 3efc359
**Applied fix:** Both places now print `fsyncs_per_s_fsync_only` (n / sum of fsync times) and `files_per_s_wall` (n / wall). The key `fsyncs_per_s` no longer exists. In the in-process test the sum of fsync times comes from the fsync histogram (`mean * len`, 3 significant digits).
**Can it change a recorded spike number:** Not the measured values, but the key name. The recorded S6 numbers used `fsyncs_per_s` with two meanings: Linux script = fsync-only, in-process = wall. The doc should say that the in-process `fsyncs_per_s` recorded earlier is `files_per_s_wall`, and that the two columns were not the same quantity. A re-run gives both for both platforms.

### WR-04: S7 `max_dirty_mib` sampled outside the serialization

**Files modified:** `crates/slicefs-cli/tests/stress_harness.rs`
**Commit:** cdf70cc
**Applied fix:** The `test_dirty_total` sample moved into the `gated` closure of `put`, right after `handle_write`, as the reviewer suggested. The old sample after `gated` is removed.
**Can it change a recorded spike number:** Yes. The recorded S7 `max_dirty_mib` could be low by up to one file's overlay, so a re-run can show a higher value. The sample now runs inside the timed handler call, so it adds the cost of one `open_files` lock and sum to each S7 write latency (`slowest_write_ms`, gate hold). The cost is small next to a 1 MiB write, but it is not zero for the 4 KiB writes.
**Note (logic change):** fixed: requires human verification (sampling point).

### WR-05: Watchdog margin does not cover the verify phase

**Files modified:** `crates/slicefs-cli/tests/stress_harness.rs`
**Commit:** b6e281a
**Applied fix:** In `Config::from_env`, the watchdog is now `secs + 60 + file_bytes / BLOCK / 5000` seconds (the reviewer's formula). The default 64 MiB adds 3 s; the maximum 4096 MiB adds about 209 s.
**Can it change a recorded spike number:** No. It only moves the abort limit; no measured value depends on it.

### WR-06: `mnt` failure leaves the daemon running

**Files modified:** `scripts/docker-linux-test/s6-smallfsync.sh`, `scripts/docker-linux-test/s4-waiting.sh`, `scripts/docker-linux-test/bigwrite.sh`
**Commit:** ba75077
**Applied fix:** On the NOT MOUNTED path, `mnt` now runs `kill -9 $MP; wait $MP; fusermount -uz $m` before `return 1`. The review named the first two scripts; `bigwrite.sh` has the same function with the same flaw, so it got the same change (the review lists it under IN-05). The `fusermount -uz` is an addition to the reviewer's fix, to clear a stale mount if the mount came up late.
**Can it change a recorded spike number:** No. It runs only on the failed-mount path and changes cleanup only; I did not check the recorded logs for a NOT MOUNTED line.

---

_Fixed: 2026-11-21_
_Fixer: Claude (gsd-code-fixer)_
_Iteration: 1_
