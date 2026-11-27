# Phase 21.2 deferred items

Out-of-scope findings during execution. Not fixed in this phase unless a plan says so.

## 1. Flaky `pool_panic_tests` child start on a loaded Mac (found in 21.2-01, Task 1 gate)

- Tests: `slicefs-cli --test pool_panic_tests`:
  `a_worker_panic_aborts_and_the_remount_replays` (gate runs 2 and 3),
  `a_reader_thread_panic_aborts_and_the_remount_replays` (gate run 2 and a
  run of the binary alone) and `a_panic_on_another_thread_does_not_abort`
  (a run of the binary alone). `a_full_queue_that_stays_full_aborts` also
  starts a child; it has not failed yet.
- Symptom: `the child (worker|reader) did not end in 60s; killed`, with an
  empty child stdout and stderr. The child (the same test binary run again
  with `--exact panic_child`) printed nothing at all, not even the libtest
  line `running 1 test`, so it hung at process start, before any pool code.
- Load average: about 11 in run 2 (a 15-minute value of 10.2 right after it),
  about 7 in run 3 (5.8 / 5.6 / 7.2 right after it), on the developer Mac.
- Not caused by the 21.2-01 change: in a normal build `chunk_store::sync::raw`
  re-exports the same std types that `pool.rs` used before, so the pool code is
  unchanged. The same binary passed in gate run 1 (4 of 4, 26.7 s) and passed
  alone between runs 2 and 3 (4 of 4, 34 s, load 6.9).
- Start time of a test binary on this Mac (`<binary> --list`, no test code
  runs), measured right after the failures, load average 7.8 to 22:

  Start times: `pool_panic_tests` (6 runs) 11.7 to 16.8 s; `statfs_tests`
  15.4 s; `statfs_tests` built before the 21.2-01 change 61 s (load rose to
  22); `/bin/ls` (control) 0.07 s.

  A binary built before the change was even slower, so the slow start is a
  property of the host, not of the change. The load came from other projects
  on the host and Spotlight indexing, not from this repository. Each child pays
  this start cost inside the 60 s `CHILD_LIMIT`.
- Gate rule for the rest of 21.2-01 (orchestrator decision): `cargo test
  --workspace` with the 3 failing child tests skipped must exit 0, and
  `pool_panic_tests` must pass in CI on Linux and macOS.
- Logs (not committed; the executor scratch directory of the 21.2-01 run, outside
  the repository): `test-t1b.log` (gate run 2), `test-t1c.log` (gate run 3), both
  `cargo test --workspace` on the default toolchain, and `test-t1d-pool.log`
  (`pool_panic_tests` alone).
- Possible follow-up (not done): find why a fresh test-binary start can stall for
  60 s on macOS under load (an exec scan of a new binary is one guess), or raise
  `CHILD_LIMIT`, or make the child a small separate binary.
  status: acknowledged

## 2. Follow-ups of the unsafe review (found in 21.2-09)

None of these is a memory-safety bug. Not done in this phase (D-33, and
plan 21.2-09 changes no file under crates). See docs/design/unsafe-review.md.

- `unmount.rs` `kill_processes_at_mountpoint` and `fuset_watch.rs`
  `spawn_unmount_watcher`: skip a pid that is not `pid > 1` before `kill`
  (0 signals the process group, -1 every process of the user), with a unit
  test on `fuse_t_servers_of`.
- `signals.rs` `on_signal`: save and restore `errno` around `write`.
- Comment-only: a `SAFETY` comment on the four `unsafe` blocks of
  `filesystem.rs` `compute_statfs`; the `alloc_tune.rs` macOS comment on the
  call line; the `needless_range_loop` reason in
  `crates/slicefs-cli/tests/lock_free_io.rs` says "the block number", but the
  index there is the write number of the thread (from 21.2-03).
  status: acknowledged

## 3. The `kill(pid)` guard of `unmount.rs` (D-33, from 21.2-09)

- `unmount.rs` `kill_processes_at_mountpoint` (and `fuset_watch.rs`
  `spawn_unmount_watcher`) should skip any pid that is not `pid > 1` before
  `kill`, with a unit test on `fuse_t_servers_of`. Item 2 has the details.
- Not done here: Phase 21.2 has no product code change for this (D-33). The
  entry is in docs/design/unsafe-review.md.
  status: acknowledged

## 4. Owner question: the loom preemption bound (from 21.2-01 and 21.2-06)

- CI runs the models with `LOOM_MAX_PREEMPTIONS=3`. The models named
  `*_all_orders` run with no bound.
- Times of the first 4 models on the developer Mac: bound 3, 6.94 s in sum;
  bound 4, 41.64 s in sum (the two-worker drain model alone: 34.31 s).
  All 7 models at bound 3: 7 s. The CI job `model` took 1.48 min (run 45755055229).
- Question for the owner (ask once): is a higher bound wanted, on main or by
  hand (for example `LOOM_MAX_PREEMPTIONS=4 scripts/safety/model.sh`)?
  status: acknowledged

## 5. Race (a) uses the research variant of D-27 (from 21.2-04)

- D-27 says: re-check the inode under its stripe before the install, and move
  the install out of the cell hold. The fix instead claims the inode under its
  stripe and pins it with an open count; the install stays in the cell hold.
- Why: with the install out of the cell hold, an open or a second size change
  could enter between the splice and the install. The pin gives the same
  guarantee (the inode cannot be reclaimed during the install). Reversible
  (one function).
  status: acknowledged

## 6. Pitfall 10 lock wait (from 21.2-04)

- A fields setattr that finds the transient cell of a closed-file size change
  waits for the splice with its stripe held. The wait is at most one splice.
  About 1/256 of inodes share a stripe.
- Not measured. `lock_wait_rows` can show it if the owner wants numbers.
  status: acknowledged

## 7. D-29 outcome of each race

- Race (a), closed-file size change (21.2-04): fixed, 40 of 40 repeat runs pass.
- Race (b), rename into the own subtree (21.2-02): fixed, 20 of 20 repeat runs pass.
- So there is no `--workers 1` fallback on Linux. Both race tests run under
  TSan in the Safety job `tsan` and in the ci.yml test runs.
  status: acknowledged

## 8. Owner step: required checks and the release gate (D-06)

- The Safety check names: `clippy-linux`, `clippy-macos`, `miri`, `tsan`,
  `model`, `kani`, `deadlock`, `coverage`.
- Safety runs only from release.yml (the release gate), on a v* tag push, and
  by manual run. It does not run on PRs. So these 8 names must not be made
  required checks on PRs: they would never report, and every PR would stay
  blocked. Only the two ci.yml job names can be required on PRs to main and
  develop.
- No agent changes the repository settings.
  status: acknowledged

## 9. Owner choices on the time limits (D-03)

- `kani` stays at 30 min. Its first runner time is 1.21 min (run 45755055229),
  so the rule gives 10. A change of safety.yml needs a new gate run.
- `miri` is 40 min, above the start value 30: the longest green run took
  17.6 min (run 45733734958).
  status: acknowledged

## 10. Items of the CONTEXT Deferred list that stay deferred

- TSan and Miri on macOS in CI.
- Miri on slicefs-cli.
- A deadlock watchdog in the daemon.
- Coverage of `fuse_callbacks.rs` beyond the FUSE smoke (tests only 9.60 %,
  with the smoke 51.40 %).
- clippy for `vendor/fuser`.
  status: acknowledged
