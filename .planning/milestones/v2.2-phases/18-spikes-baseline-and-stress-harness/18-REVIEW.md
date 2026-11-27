---
phase: 18-spikes-baseline-and-stress-harness
reviewed: 2026-11-21T00:00:00Z
depth: standard
files_reviewed: 18
files_reviewed_list:
  - .github/workflows/s1-overlap.yml
  - crates/chunk-store/Cargo.toml
  - crates/chunk-store/src/lib.rs
  - crates/chunk-store/src/lock_stats.rs
  - crates/metadata/Cargo.toml
  - crates/metadata/src/store.rs
  - crates/metadata/src/wal/buffered.rs
  - crates/metadata/src/wal/per_op.rs
  - crates/slicefs-cli/Cargo.toml
  - crates/slicefs-cli/examples/s1_probe.rs
  - crates/slicefs-cli/src/filesystem.rs
  - crates/slicefs-cli/tests/lock_stats_report.rs
  - crates/slicefs-cli/tests/stress_harness.rs
  - scripts/docker-linux-test/bigwrite.sh
  - scripts/docker-linux-test/run.sh
  - scripts/docker-linux-test/s4-waiting.sh
  - scripts/docker-linux-test/s6-smallfsync.sh
  - scripts/macos-mount-test/s1-overlap.sh
findings:
  critical: 0
  warning: 6
  info: 5
  total: 11
  necessity: 0
status: issues_found
---

# Phase 18: Code Review Report

**Reviewed:** 2026-11-21
**Depth:** standard
**Files Reviewed:** 18
**Status:** issues_found

## Summary

The four focus areas were checked.

1. **`lock-stats` off: no change to product behavior or cost.** Confirmed.
   - `lock()` is `#[inline(always)] m.lock().unwrap()` and `Guard` is a type alias of `std::sync::MutexGuard`, so there is no extra type, no timing and no extra drop.
   - `hdrhistogram` is `optional = true`, `default-features = false`, and only reachable through `dep:hdrhistogram`.
   - The dev-dependency in `slicefs-cli` does not reach the `slicefs` binary.
   - `ChunkStore::drop` keeps the std lock, so a poisoned lock still cannot panic in drop.
   - With the feature on, `cargo check -p slicefs-cli --features lock-stats --tests --examples` compiles.
   - Poison handling is equal: `try_lock` -> `Poisoned` falls back to `m.lock().unwrap()`, which panics as before. The `Drop` of `TimedGuard` releases the std guard first, so poisoning on unwind is unchanged.
   - I found no blocker in the feature itself.
2. **Harness holds no whole file bytes in RAM.** Confirmed.
   - The model is version arrays (`AtomicU32` per block, `ver` per file).
   - Block bytes are a pure function of (seed, file, block, version).
   - Per-thread buffers are at most 1 MiB (S3, S7) or 64 KiB (one-file).
3. **Scripts and workflow.**
   - The workflow has `permissions: contents: read`, a job and a step timeout, and no secrets.
   - `shell: bash` gives `pipefail`, so `| tee` does not hide a client failure.
   - Wait loops in `s1-overlap.sh`, `s4-waiting.sh` and `s6-smallfsync.sh` are bounded and check `kill -0` on the process.
   - Findings are below.
4. **Probe and harness correctness.** The S1 probe orders its counters in a way that can inflate `max_inflight` (WR-01). The client `fast_stats_during_slow` number cannot prove overlap (WR-02). Two S6/S7 numbers are defined or sampled inconsistently (WR-03, WR-04).

The S1 conclusion in the results ("macOS default can be more than one worker") is not at risk from WR-01 or WR-02: it also rests on `reply_order_inversions=5`, which can only happen when a later request is answered while the slow READ is pending. The two defects still make the printed numbers weaker than the text says.

## Warnings

### WR-01: S1 probe decrements `inflight` after the reply, so a serial transport can show `max_inflight` of 2

**File:** `crates/slicefs-cli/examples/s1_probe.rs:140-153`
**Issue:** In `worker`, the reply is sent first (`reply.data`, `reply.entry`, `reply.attr`). Only then do `last_replied.fetch_max` and `inflight.fetch_sub` run. A client that waits for a reply before it sends the next request can send the next request before the worker reaches `fetch_sub`. The reader thread then runs `submit`, and `inflight` goes to 2 with no real overlap. `max_inflight` is a headline number of spike S1, and this is the exact case S1 must tell apart (a transport that serializes). The same ordering makes `reply_order_inversions` depend on when a worker reaches `fetch_max`, not on when the reply left, so two workers on fast requests can record in the wrong order. The race is rare, so the measured values (2 on macOS, 3 on Linux) are probably right. The probe still cannot rule it out.
**Fix:** Update both counters before the reply, so that an observed `inflight` can only be too low:
```rust
fn finish(st: &Stats, seq: u64) {
    if seq < st.last_replied.fetch_max(seq, SeqCst) {
        st.inversions.fetch_add(1, SeqCst);
    }
    st.inflight.fetch_sub(1, SeqCst);
}
// in the worker: compute the reply first, call finish(&st, job.seq), then reply.xxx(..)
```
For a proof that does not depend on ordering, also count a request that arrived while another was unanswered, using the ARR timestamps already logged.

### WR-02: S1 client metric `fast_stats_during_slow` can be 50 without any overlap

**File:** `scripts/macos-mount-test/s1-overlap.sh:75-98`
**Issue:** The client starts `dd`, sleeps a fixed 50 ms, then runs 50 stats. It does not check that the slow READ has reached the server. It then counts a stat as "during slow" when it ended before `dd` ended. If `dd` starts late (fork/exec, open, lookup), the stats can finish before the READ is issued, and `dd` still ends 200 ms later, so the count is 50 anyway. The logged runs show 50 in every round on both platforms, which this design would give with and without overlap. `max_stat_ms=2` has the same weakness. The probe's `inversions` is the real evidence (see Summary).
**Fix:** Start the stats only after the READ is in flight. Poll `probe.err` for the `ARR` line of the slow inode, or run `dd` with a `stat`-able marker. Or count only stats that start after the probe logged the slow READ, and print the count of stats that ran while `dd` was still alive at their end, with the time of the first stat relative to the READ arrival.

### WR-03: S6 `fsyncs_per_s` has two different definitions (Linux script vs in-process test)

**File:** `scripts/docker-linux-test/s6-smallfsync.sh:50` and `crates/slicefs-cli/tests/stress_harness.rs:1639`
**Issue:** The Linux script prints `fsyncs_per_s = n / sum(fsync times)`. The in-process test prints `files / wall`, where wall includes create, write and release. Both use the key name `fsyncs_per_s`, and the results are compared in the spike notes. For the same run the two values can differ by 10 to 40 percent (for example Linux flush-on-fsync pass 2: 358 vs `2000 / 6.4 s` = 312). Comparing macOS in-process and Linux numbers by this key mixes two quantities.
**Fix:** Use one definition. Print both in both places, with distinct names: `fsyncs_per_s_fsync_only` (`n / sum(fsync)`) and `files_per_s_wall` (`n / wall`).

### WR-04: S7 `max_dirty_mib` is sampled outside the serialization and can miss a file's dirty bytes

**File:** `crates/slicefs-cli/tests/stress_harness.rs:1704`
**Issue:** `put` calls `ctx.fs.test_dirty_total()` after `gated` returns, so it runs while other threads hold the harness mutex and run handlers. The write path removes a file's state from `open_files` for the duration of the write (`open_files.lock().remove(&fh)` ... `insert(fh, state)` in `filesystem.rs`), and `test_dirty_total` only sums what is in the map. A sample taken during another thread's write therefore leaves out that file's dirty bytes. The reported maximum can be low by up to one file's overlay. The same sample is not taken in `run()` (there it runs after the workers are joined).
**Fix:** Take the sample inside the `gated` closure, after `handle_write`, where the harness mutex is held and no state is taken out:
```rust
ctx.gated(hs, Kind::Write, || {
    let r = h::handle_write(&ctx.fs, fh, off, buf);
    max_dirty.fetch_max(ctx.fs.test_dirty_total(), Ordering::SeqCst);
    r
})
```

### WR-05: Watchdog margin does not cover the verify phase for large `STRESS_FILE_MIB`

**File:** `crates/slicefs-cli/tests/stress_harness.rs:148` (with `run`, `verify_files` at 790-804 and 879-983)
**Issue:** `watchdog` is `secs + 60` and is dropped only when `run` returns. `run` includes `verify_files`, which reads the file one 4 KiB block at a time through the gated handler. `STRESS_FILE_MIB` accepts up to 4096, which is 1,048,576 reads plus one `Vec` allocation per block and per candidate version. At tens of microseconds each, this is 30 to 100 seconds, so the watchdog can call `process::abort()` during a correct verify. The report from the finished timed run is lost, and the abort message reports a hang.
**Fix:** Size the watchdog from the file: `secs + 60 + file_bytes / (BLOCK as u64) / 5_000`. Or verify in 64 KiB reads (`read_blocks(.., b, 16, ..)` in steps of 16). Or lower the limit of `STRESS_FILE_MIB`.

### WR-06: `mnt` failure leaves the daemon running in `s6-smallfsync.sh` and `s4-waiting.sh`

**File:** `scripts/docker-linux-test/s6-smallfsync.sh:17-23` (the same function is in `s4-waiting.sh:18-24`)
**Issue:** When the mount is not seen in 10 s, `mnt` prints "NOT MOUNTED", sets `F=1` and returns 1, but the daemon (`$MP`) keeps running. In `s6-smallfsync.sh` the loop then runs `rm -rf $ST $M` under that daemon, and the next iteration overwrites `MP`. A late mount then leaves a stale mount and an orphan daemon that holds the old store for the rest of the container, and the following strategy runs share the CPU and disk with it.
**Fix:** On the failure path, stop the daemon before returning:
```sh
echo "  NOT MOUNTED: $m"; tail -5 $R/mount.log; F=1
kill -9 $MP 2>/dev/null; wait $MP 2>/dev/null; return 1
```

## Info

### IN-01: `lock_stats::on::record` runs inside an outer site lock; the doc comment says it never does

**File:** `crates/chunk-store/src/lock_stats.rs:159-162`
**Issue:** The comment says "the recorder never runs inside a site lock". That holds for one guard. With nested guards (`pending` then `writer` in `buffered.rs`, `inner` then others), the inner guard's `Drop` runs `record` while the outer site lock is still held. The outer site's measured hold then includes the recorder cost of the inner one (one recorder lock and two histogram writes). With `lock-stats` on, the numbers of the outer lock are slightly high, and the comment is wrong. The recorder is a leaf lock, so there is no deadlock.
**Fix:** Correct the comment ("not inside the guard's own lock"), and add to the S3 note that outer-site holds include the recorder cost of nested sites.

### IN-02: The recorder array length is a literal 7 in three places

**File:** `crates/chunk-store/src/lock_stats.rs:38` and `123-124`
**Issue:** `Site::ALL: [Site; 7]` and `[Mutex<SiteHists>; 7]` repeat the number of variants. A new `Site` variant that is added to the enum but not to both arrays makes `recorders()[site as usize]` panic at run time (only with `lock-stats` on).
**Fix:** Define `const SITES: usize = Site::ALL.len();` and use it for both arrays, or add a unit test that `Site::ALL.len() == recorders().len()` and `ALL[i] as usize == i`.

### IN-03: Parallel tests in `stress_harness` share the process-global recorder

**File:** `crates/slicefs-cli/tests/stress_harness.rs:881` (in `run`)
**Issue:** `run` calls `lock_stats::reset()`, and `short_one_file`, `short_many_files` and `model_detects_planted_bug` run in parallel in one binary. With `--features lock-stats`, one test clears the histograms of another, and `print_report` shows mixed lock rows. Nothing asserts on them, so this only makes the printed rows wrong. The module doc and `run.sh harness` already say to run one test at a time for the measured runs.
**Fix:** Skip the lock rows (and the `reset`) in `check_short`, or print "lock rows are only valid for a single test" next to them when `ENABLED`.

### IN-04: `run.sh harness` with no test name runs zero tests and reports success

**File:** `scripts/docker-linux-test/run.sh` (the `harness)` case)
**Issue:** `cargo test ... -- --ignored --exact --nocapture` with no name matches no test and exits 0 ("0 passed"). The usage line says `harness <test-name>`, but nothing enforces it.
**Fix:** Add `[ $# -ge 1 ] || { echo "usage: $0 harness <test-name>"; exit 2; }` after the `shift`.

### IN-05: Duplicated helpers across the new scripts and tests

**File:** `scripts/docker-linux-test/s4-waiting.sh:15-30`, `scripts/docker-linux-test/s6-smallfsync.sh:14-29`, `crates/slicefs-cli/tests/lock_stats_report.rs:24-33`, `crates/slicefs-cli/tests/stress_harness.rs:243-245`
**Issue:** `ok`, `bad`, `step`, `mnt` and `umnt` are copied between the two Docker scripts (and `bigwrite.sh` has a third copy), so WR-06 has to be fixed in each copy. `new_fs` is copied between the two test files. `hist()` in the harness repeats `lock_stats::hist` (that one is private by design). The unquoted `$R`, `$M`, `$B` are safe only because the paths are fixed and have no spaces.
**Fix:** Move the shell helpers to a sourced `scripts/docker-linux-test/lib.sh`, and the test `new_fs` to a `tests/common/mod.rs`. Quote the variables when moving them.

---

_Reviewed: 2026-11-21_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_
