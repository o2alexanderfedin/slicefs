---
phase: 20-dirty-budget-and-rebase-flusher
plan: 03
subsystem: concurrency
status: complete
tags: [lock-06, flusher, throttle, shutdown, lifecycle]

requires:
  - phase: 20-dirty-budget-and-rebase-flusher
    plan: 02
    provides: "throttle_write, run_flusher, flush_pass, FlushCtl, the flusher_tests.rs helpers"
provides:
  - "flusher::spawn_flusher(Weak) -> JoinHandle: thread slicefs-flusher, no Arc while it sleeps"
  - "SliceFsFilesystem::start_flusher, stop_flusher (lock assert, wake of writers, join), shutdown"
  - "throttle_write: lock assert; leaves the wait with pay at a stop or when no flusher runs (stop_exits)"
  - "the running guard of the flusher loop wakes throttled writers (also on a panic)"
  - "test hooks: test_gate_exclusive_waiters, test_flusher_stopping; FlusherStats::stop_exits"
  - "FUSE write: throttle_write before the gate; FUSE destroy: shutdown(); run_mount starts and stops the flusher"
affects: [20-04, 20-05, 20-06, 20-07, 20-08]

estimate:
  tokens: 120000
  tasks: 2
actuals:
  tokens: 5484      # chars/4 over the added lines of the diff 1b390c9..aa853f2
  tasks: 2
  commits: 2

plan_head_before: 1b390c9da4447be443af9e3d853387e3f7dca3f9
plan_head_after: aa853f2b6704215219e0dba33b3d90187ce5bc31
commits: 2

tech-stack:
  added: []
  patterns:
    - "Stop order: stop and join the flusher with no lock held, then take the gate"
    - "A background thread holds a Weak and the Arc of its control block; it upgrades the Weak for one pass only"
    - "A count of the waiters for the exclusive gate (a drop guard) lets tests wait for a queued commit"

key-files:
  created:
    - .planning/phases/20-dirty-budget-and-rebase-flusher/runs/smoke-20-03.log
  modified:
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/flusher.rs
    - crates/slicefs-cli/src/commit_timer.rs
    - crates/slicefs-cli/src/fuse_callbacks.rs
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/tests/flusher_tests.rs

decisions:
  - "A throttle exit at a stop or with no flusher is counted in a new counter stop_exits, not in timeouts"
  - "run_flusher and spawn_flusher share one private loop, flusher_loop(ctl, pass)"
  - "run_mount keeps an Arc clone only for the stop after mount() and drops it right after the stop"

metrics:
  duration: "about 45 min"
  completed: 2026-11-23
---

# Phase 20 Plan 03: Flusher lifecycle Summary

A mount now starts one flusher thread after the write-log replay, and the FUSE `write` callback calls `throttle_write` before it takes the commit gate. The FUSE `destroy` callback calls `shutdown`: it stops and joins the flusher with no lock held, then takes the gate, then does the destroy work. A stop or a panic of the flusher wakes every throttled writer at once. Each writer then writes and pays with steps of its own file, and no write gets an error.

## Tasks

| Task | Name | Commit | Files |
|---|---|---|---|
| 1 | Stop and panic wake the throttled writers; no throttle and no stop with a lock held | 04ef0a8 | filesystem.rs, flusher.rs, commit_timer.rs, tests/flusher_tests.rs |
| 2 | Flusher thread with the mount; throttle in the FUSE write callback; shutdown order; Docker smoke mount | aa853f2 (code), docs commit (smoke log) | filesystem.rs, flusher.rs, fuse_callbacks.rs, mount.rs, tests/flusher_tests.rs, runs/smoke-20-03.log |

## What was built

- `throttle_write`: the first statement is a `debug_assert!` that `chunk_store::sync::rank::held()` is empty (message "throttle_write with a lock held: ..."). It runs on every call. In the wait loop, after the "below the budget" check, the writer leaves with `pay: true` when `stop` is set or `running` is false. These exits go to `stop_exits`, not to `timeouts`.
- `stop_flusher`: the same assert first. It sets `stop` under the flags lock, calls `notify_all` on `wake` and `freed`, takes the `JoinHandle` out of `flusher_thread` (`sync::Mutex`, `Rank::Short`), releases that lock, and joins. It does not join when it runs on the flusher thread. A second call does nothing new. Its doc states what bounds the join (one step; `STORE_WAIT` 10 s per store wait; `STEP_WAIT` slices with progress, at most `STEP_WAIT_MAX` 120 s).
- The running guard of the flusher loop clears `running`, then calls `notify_freed()` (take and release the flags lock, `freed.notify_all()`). This occurs at a normal end and at a panic.
- `test_gate_exclusive_waiters()`: `timed_commit` and `gate_exclusive()` take an `ExclusiveWait` guard just before they block on the exclusive gate and drop it when they have the gate. One atomic add and one sub; a panic lowers the count too. `test_flusher_stopping()` reads the stop flag.
- `flusher::spawn_flusher(Weak)`: thread `slicefs-flusher`. It takes the `Arc` of the flush control block once at spawn. It upgrades the `Weak` for one pass only and ends when the upgrade fails or at a stop. `run_flusher` and `spawn_flusher` share `flusher_loop(ctl, pass)`, which sets `require_gate(true)` and the running guard.
- `start_flusher(self: &Arc<Self>)` keeps the handle. `shutdown(&self) -> bool` runs `stop_flusher()`, then `gate_shared()`, then `test_destroy()`. `test_destroy` starts with `debug_assert!` that no flusher runs.
- `fuse_callbacks.rs`: `destroy` calls `self.shutdown()` and keeps its two debug lines. `write` calls `throttle_write()` first and measures it, then takes the gate, then calls `handle_write_admitted(..., admit)`. The `[slow-write]` line has `throttle_ms=`, and the throttle time counts in its 50 ms rule.
- `mount.rs`: after `Arc::new(fs)`, when `snapshot_ref.is_none()`, `start_flusher()` and an `Arc` clone. After `mount()` returns (also when it failed, before `mounted?`), `stop_flusher()` on the clone, then the clone is dropped. This is before the commit timer stops.

## Red and green runs

Each new test failed before its code, at an assert with a time limit (no watchdog abort). No red tree was committed (project rule).

Task 1, on 1b390c9 plus test hook stubs (`test_flusher_stopping` real, `test_gate_exclusive_waiters` returned 0, a `stop_exits` field that nothing raised):

| Test | Failed at |
|---|---|
| `stop_wakes_throttled_writers` | line 365, `after the stop: the throttled writes return Ok within 5s` |
| `a_flusher_panic_wakes_throttled_writers` | line 365, `after the flusher panic: the throttled writes return Ok within 5s` (the flusher thread panicked and ended first) |
| `a_throttle_with_a_lock_held_panics` | line 503, `throttle_write with the gate held gave Err: Admit { pay: false }` |
| `a_stop_with_a_lock_held_panics` | line 516, `stop_flusher with the gate held gave Err: ()` |
| `a_queued_commit_is_seen` | line 533, `the commit is queued for the exclusive gate` (5 s wait on the stub) |

The acceptance line asks for four red runs; the plan's action names five, so all five are here.

Task 2, on 04ef0a8 plus a stub `spawn_flusher` whose loop sleeps for the pass interval and ignores the stop and a failed upgrade:

| Test | Failed at |
|---|---|
| `a_spawned_flusher_stops_and_joins` | flusher.rs line 197, `stop_flusher returns within 5 s (the join ended): Timeout` |
| `a_spawned_flusher_ends_when_the_fs_is_dropped` | flusher.rs line 212, `the flusher thread ends within 5 s after the drop: Timeout` |

Guard mutation for `shutdown_with_a_parked_flusher_step_and_a_queued_commit_ends` (not committed): the order in `shutdown` was changed to the gate first, then the stop. The test failed at line 610, `the shutdown asked the stop`, after its 10 s wait. The plan expected the failure at "shutdown returns true". The real cause: with the gate first, the shutdown thread blocks in `gate_shared()` behind the queued commit, so it does not get to `stop_flusher` (and its lock assert) before the park is let go. In a release build this order is the deadlock that the test is for. The file was restored from a copy; `shutdown` has `stop_flusher()` before `gate_shared()` again.

Green:
- Task 1 tree (04ef0a8): `cargo test -p slicefs-cli --test flusher_tests`: 10 passed. `--test dirty_budget_tests --test rebase_step_tests --test lock_free_io --test no_take_out`: 1, 33, 4, 4 passed. `cargo test --workspace` exited 0 (69 `test result: ok` lines) before the commit.
- Task 2 tree (aa853f2): `cargo test -p slicefs-cli --lib flusher::`: 2 passed. `--test flusher_tests`: 11 passed. The shutdown test passed 8 more times in a row (0.88 s to 1.48 s each). `--test mount_lifecycle_tests --test signal_tests --test crash_recovery_tests`: 6, 41, 1 passed. `cargo check -p slicefs-cli --all-targets --features deadlock-detect` exited 0 with no warning outside `vendor/fuser`. `cargo test --workspace` exited 0 (69 `test result: ok` lines) before the commit.

## Docker smoke mount

- Image: `docker build` from the same Dockerfile (orchestrator correction). `rustc -V` in the image: `rustc 1.100.0 (0ef1bc386 2026-11-09)`. This is the same as the `RUSTC =` line of `runs/s7-wave0-NOTES.md`. Image ID, for reference only: `sha256:7bf7acd215cb12ec822a04b81ec3ab351bc6098cd5e2e8f32c8562b1fe4c25fe`.
- Tree: `git archive` of aa853f2 as a tar stream, target volume `slicefs-target-p20`, wait until the container is 45 s old, then `scripts/docker-linux-test/mt-linux.sh /target/debug/slicefs`.
- Result lines (`runs/smoke-20-03.log`):
  - `RUSTC = rustc 1.100.0 (0ef1bc386 2026-11-09)`
  - `Linux bf74650e4b26 7.2.12-orbstack-00394-gbecdd6130068 #1 SMP PREEMPT Thu Oct 22 22:17:17 UTC 2026 x86_64 GNU/Linux`
  - read, echo-write, cp-3MB, cp-again, readback, mkdir-mv, chmod, truncate, xattr, rm: ok
  - `unmount: ok`
  - `daemon-exit: ok`
  - daemon log: `destroy: ENTRY` at 07:36:49.040, `destroy: EXIT — commit_ok=true` at 07:36:49.096
  - script exit 0. Host load at the start: 18.23 / 24.16 / 22.54.

## Acceptance checks

- `grep -c 'with a lock held' crates/slicefs-cli/src/filesystem.rs` = 2
- `grep -c 'fn stop_wakes_throttled_writers' crates/slicefs-cli/tests/flusher_tests.rs` = 1
- In `write`, the first match of `throttle_write|gate_shared` is `throttle_write` (the awk check prints 1)
- In `destroy`, `self.shutdown()` occurs one time (the awk check prints 1)
- `grep -c 'start_flusher' crates/slicefs-cli/src/mount.rs` = 1
- `grep -c 'pub fn spawn_flusher' crates/slicefs-cli/src/flusher.rs` = 1

## Deviations from Plan

### Auto-fixed Issues

None.

### Other deviations

**1. No separate RED commit.** The project rule is to commit only when `cargo test --workspace` passes. Each task has one code commit with its tests. The red runs are recorded above.

**2. Docker image built again.** The plan said not to run `docker build` and to compare image IDs. The orchestrator correction replaced that check: the image was built from the same Dockerfile and `rustc -V` was compared with the `RUSTC =` line (the same). The image ID is recorded for reference only.

**3. A new counter `stop_exits`.** The plan says to count the stop exits of the throttle wait apart from the timeouts. `FlushCtl` and `FlusherStats` have a new field `stop_exits`. `stop_wakes_throttled_writers` asserts that it is 4.

**4. Guard mutation failed at a different assert.** See "Red and green runs": the test failed at `the shutdown asked the stop` (10 s), not at `shutdown returns true`. The test was not changed to match the plan.

**5. Test thread name in the panic test.** The scoped flusher thread of `a_flusher_panic_wakes_throttled_writers` has the name `test-flusher`, and the hook panics only on that thread and only one time.

## Open points

- **Commit timer and destroy (older than this plan).** After a commit cut, `commit_cut_finish` runs with the gate released. During an unmount, the commit timer stops only after `mount()` returns, so a timed commit can still finish while `destroy` runs its own commit. The shutdown test puts a commit in this window, and `shutdown()` returned true in all 10 runs (one `flusher_tests` run, 8 repeats, the workspace run). No change was made; this note is for a decision if it is a concern.
- **Requirements.** LOCK-06 is in the plan frontmatter, but it is not marked complete (orchestrator rule). REQUIREMENTS.md did not change.
- **State tool.** `state.advance-plan` cannot read the position (the same as in 20-01 and 20-02). The position line in STATE.md was set by hand.

## Threat model check

- T-20-SC: no new crate.
- T-20-08: `shutdown` stops and joins before the gate; `stop_flusher` asserts that no lock is held; `shutdown_with_a_parked_flusher_step_and_a_queued_commit_ends` with its guard mutation.
- T-20-09: the stop and the running guard notify `freed`; the throttle leaves at a stop or when no flusher runs; `stop_wakes_throttled_writers` and `a_flusher_panic_wakes_throttled_writers`.
- T-20-10: the Docker smoke mount gave `daemon-exit: ok`.
- T-20-11: `stop_flusher` does not join on the flusher thread; the thread holds no `Arc` while it sleeps; `a_spawned_flusher_ends_when_the_fs_is_dropped`.

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/src/flusher.rs (`pub fn spawn_flusher`), crates/slicefs-cli/tests/flusher_tests.rs, runs/smoke-20-03.log
- FOUND: 04ef0a8 and aa853f2 in HEAD
- `commits: 2` is measured with `git rev-list --count` from `plan_head_before`; the docs commit (this file, the smoke log, STATE.md, ROADMAP.md) comes after it, as in 20-02.
