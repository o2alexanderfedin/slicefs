---
phase: 21-worker-pool
plan: 03
subsystem: concurrency
status: complete
tags: [pool-03, worker-pool, panic, abort, write-log-replay]

requires:
  - phase: 21-worker-pool
    provides: "21-01: Pool (workers slicefs-worker-<k>, with_limits, submit that aborts at the submit limit); 21-02: all request callbacks through run and serve"
provides:
  - "pool::install_abort_hook: a process-wide panic hook (installed once, std::sync::Once) that calls the previous hook, then aborts when the thread name starts with fuser- or slicefs-worker-"
  - "run_mount installs the hook before mount()"
  - "Debug builds only: SLICEFS_TEST_PANIC_CREATE=k makes the k-th new entry of a mount panic at its half-done point (set after the write-log replay)"
  - "crates/slicefs-cli/tests/pool_panic_tests.rs: child-process tests of the abort and the replay (the binary runs itself, ignored test panic_child)"
  - "scripts/docker-linux-test/panic-replay.sh: the POOL-03 live check at 4 and 1 workers; verify-phases.sh runs it after FUSET-05"
affects: [21-07, 21-09]

estimate:
  tokens: 80000
  tasks: 2
actuals:
  tokens: 9100    # chars/4 over the added lines of the diff (code, tests, scripts, two Docker logs)
  tasks: 2
  commits: 3

plan_head_before: 86bbdf305a617d0f94bdecff5ff455dec9a069d9
plan_head_after: 5e57d3edc76a380af2ac662c9d9d1a72bd280226
commits: 3

tech-stack:
  added: []
  patterns:
    - "Child-process test: the test binary runs itself with --exact panic_child --ignored --nocapture --test-threads 1 and an env value; output to files; kill at a limit"
    - "Abort from the panic hook, not catch_unwind: the hook runs before the unwind, so the locks stay held and no reply is sent"

key-files:
  created:
    - crates/slicefs-cli/tests/pool_panic_tests.rs
    - scripts/docker-linux-test/panic-replay.sh
    - .planning/phases/21-worker-pool/runs/panic-replay-red.log
    - .planning/phases/21-worker-pool/runs/verify-21-03.log
  modified:
    - crates/slicefs-cli/src/pool.rs
    - crates/slicefs-cli/src/mount.rs
    - scripts/docker-linux-test/verify-phases.sh

decisions:
  - "Only request threads abort (names fuser- and slicefs-worker-); the flusher, the GC and the commit timer keep the Phase 20 behavior (Open Question 2; abort on a flusher panic is a deferred item for 21-09)"
  - "The panic mount of the live check uses --commit-interval 0, so only the write log can keep a.bin"
  - "The full-queue test measures from a time stamp that the child prints before the third submit, not from the child start"

metrics:
  duration: "about 66 min"
  completed: 2026-11-23
---

# Phase 21 Plan 03: A request panic aborts the daemon; the remount replays the write log Summary

A panic on a request thread (the fuser reader `fuser-<i>` or a pool worker `slicefs-worker-<k>`) now aborts the daemon from the panic hook, before the unwind. The locks of the half-done request stay held, no reply is sent, and fuser cannot drop its file system holder, so `destroy()` does not commit the half-done state. The next mount replays the write log. Child-process tests show this in process, and a Docker Linux mount shows it with 4 workers and with 1 worker.

## What was built

- **Hook (`pool.rs`).** `install_abort_hook()` takes the previous hook and sets a new one. The new hook calls the previous hook first (the message and the backtrace go to stderr). When the thread name starts with `fuser-` or `slicefs-worker-`, it prints `request thread <name> panicked: abort; the write log replays at the next mount` and calls `std::process::abort()`. A `std::sync::Once` makes a second call do nothing. The doc comment gives the reasons for a hook (not `catch_unwind`) and why the flusher is not matched.
- **Mount (`mount.rs`).** `run_mount` calls the hook just before the pool and `mount(...)`. Under `#[cfg(debug_assertions)]` only, `set_test_panic_create` reads `SLICEFS_TEST_PANIC_CREATE`. A value k of 1 or more prints one WARNING line and sets the mkdir hook to a closure that counts its calls (an atomic) and panics on call k. Any other value prints a warning and is ignored. The hook is set after the write-log replay. A release build has no code for it (`cargo check --release` has only the 6 old fuser warnings).
- **Live check (`panic-replay.sh`).** For W in 4 and 1: a fresh seeded store, a mount with `SLICEFS_TEST_PANIC_CREATE=2`, `--workers $W` and `--commit-interval 0`, `dd ... conv=fsync` of a 1 MB file to `a.bin` (create 1), `timeout 10 touch b.txt` (create 2, the panic), a wait of at most 10 s for the daemon, then `wait $MP`. Checks: `panic-aborts-w$W` (status 134), `panic-line-w$W`, and after `fusermount -uz` and a remount, `fsynced-file-replayed-w$W` and `half-done-entry-absent-w$W`. `verify-phases.sh` runs it after the FUSET-05 block and sets F=1 when it fails.

## Tests (`pool_panic_tests.rs`)

The parent tests run the binary again as a child (`--exact panic_child --ignored --nocapture --test-threads 1`, `POOL_PANIC_CHILD=<mode>:<store dir>`, stdin from null, stdout and stderr to files). The parent kills a child that has not ended after 60 s.

- `a_worker_panic_aborts_and_the_remount_replays`: the child creates A, writes 64 KiB, fsyncs and releases. It installs the hook, sets the mkdir hook to a planted panic, and submits a job to `Pool::new(2)`. The job holds the gate (as `serve` does), creates B, then prints `REPLY B`. The parent asserts: SIGABRT; the planted panic message and `panicked: abort` on stderr; no `REPLY B` and no `CHILD END` on stdout. Then it reloads the store and replays the log: A has its 64 KiB, and B is not there (ENOENT).
- `a_reader_thread_panic_aborts_and_the_remount_replays`: the same, but the job runs on a thread named `fuser-0`.
- `a_full_queue_that_stays_full_aborts`: `Pool::with_limits(1, 1, 1 MiB, 300 ms)`, one job parked (it signals that it started), one job in the slot, then a third submit. SIGABRT within 10 s of the third submit, `queue stayed full` on stderr, no `SUBMIT RETURNED`.
- `a_panic_on_another_thread_does_not_abort`: a thread named `slicefs-flusher` panics; the join gives `Err`; the child prints `ALIVE` and exits with 0.

The planted panic message is checked, so an abort for another cause (for example the rank checker) cannot make the test pass.

## Red runs (not committed)

| # | Test | Red tree | Failing assert |
|---|------|----------|----------------|
| 1 | `a_worker_panic_aborts_and_the_remount_replays` | the stub `install_abort_hook() {}` | `the child ended by SIGABRT: status ExitStatus(unix_wait_status(0))`, left `None`, right `Some(6)`. The worker unwound, and the child printed `CHILD END` and exited with 0. |
| 2 | `a_reader_thread_panic_aborts_and_the_remount_replays` | the same stub | the same assert: status 0, left `None`, right `Some(6)`. |
| 3 | `a_full_queue_that_stays_full_aborts` | guard mutation: `exit(1)` in place of `abort()` in `submit` | `the child ended by SIGABRT: status ExitStatus(unix_wait_status(256))` (exit code 1) |
| 4 | `a_panic_on_another_thread_does_not_abort` | guard mutation: the hook matches `slicefs-` | `the child ended with status 0: status ExitStatus(unix_wait_status(6))`; stderr had `request thread slicefs-flusher panicked: abort` |
| 5 | Docker `panic-replay.sh` alone (`runs/panic-replay-red.log`) | `install_abort_hook` returns at once (working tree at a79f39f) | `panic-aborts-w4: FAIL (daemon still running 10s after the panic)`, `panic-aborts-w1: FAIL (exit status 1, not 134)`, `half-done-entry-absent-w1: FAIL`, `RESULT panic-replay: FAIL`, exit status 1 |

Run 5 shows the gaps that the plan named. With 4 workers and no hook, the daemon goes on after the panic. `touch b.txt` then succeeded, because the half-done B was linked in memory and the next request served it. With 1 worker and no hook, the reader panic ended the session, and `destroy()` committed the half-done B (it was there after the remount).

The mutations of runs 3 and 4 were reverted from a saved copy, and the plant of run 5 with `git checkout` of pool.rs. `git diff --quiet HEAD -- crates/slicefs-cli/src/pool.rs` printed `hook-restored`.

## Green runs

- `pool_panic_tests`: 4 passed, 1 ignored (`panic_child`).
- `cargo test --workspace` exited 0 before each code commit: 8b1049f (72 `test result: ok` lines, 0 `FAILED`) and a79f39f (72 lines, 0 `FAILED`).
- VALIDATION extras on a79f39f: `cargo check --workspace --all-targets --features slicefs-cli/lock-stats` exit 0; `cargo check -p slicefs-cli --all-targets --features deadlock-detect` exit 0; `--features deadlock-detect` `pool_panic_tests` 4 passed and `pool_tests` 9 passed.
- Gates: `pub fn install_abort_hook` 1 line in pool.rs; `slicefs-worker-` 3 lines in pool.rs; `install_abort_hook` 1 line in mount.rs; the `SLICEFS_TEST_PANIC_CREATE` read has `#[cfg(debug_assertions)]` within 3 lines above it; `panic-replay.sh` 1 line in verify-phases.sh; `SIGABRT` is in the test file.

## Docker runs

Both logs have `rustc 1.100.0 (0ef1bc386 2026-11-09)`, the same as the `RUSTC = ...` line of the 20-01 notes (`same-toolchain`). Both runs used the tar stream of the working-tree files, the mount form, volume `slicefs-target-p21`, a debug build, and the 45 s wait. One container ran at a time. Neither log has a user or home path.

- **Red, `runs/panic-replay-red.log`.** See run 5 above.
- **Live checks, `runs/verify-21-03.log`.** The committed tree at a79f39f, no `SLICEFS_WORKERS` (the other blocks use the container default of 8 workers). `RESULT verify-phases: PASS`, 64 `: ok` (56 in 21-02, plus the 8 new checks), 0 `: FAIL`, 0 `NOT MOUNTED`. The POOL-03 block: `panic-aborts-w4`, `panic-line-w4`, `fsynced-file-replayed-w4`, `half-done-entry-absent-w4`, and the same 4 checks for w1, all `ok`. bash printed `Aborted` for both panic mounts.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Flaky test] Kill limit 60 s, and the full-queue time from the third submit**
- **Found during:** Task 1
- **Issue:** On this host (load 50 to 130), one start of the test binary took 4 to 20 s (`--list` alone took 7 s). The time from the child start included that start, so the "within 10 s" check failed when the abort itself was correct (12.6 s, of which the child's own work was well under 1 s). The 20 s kill limit was also near: one child took 18.5 s.
- **Fix:** the child prints `THIRD SUBMIT AT <ms>` just before the third submit, and the parent measures from that time to the end that it saw. The kill limit is 60 s. It applies only to a hang; a normal child ends much earlier.
- **Files modified:** crates/slicefs-cli/tests/pool_panic_tests.rs
- **Commit:** 8b1049f

### Other notes

- **One commit per task.** The stub and the tests were not committed apart: a commit is allowed only when `cargo test --workspace` exits 0, and the stub tree is red. The red runs are in the table above.
- **Red runs for all 4 tests.** The plan asks for the red run of the worker test. The reader test is also red on the stub. The other two tests pass on the stub by design, so guard mutations (runs 3 and 4) give their red runs.
- **The gate in the child job.** A worker sets `require_gate(true)`, so the job holds `gate_shared()` around the create, as `serve` does. With no gate, the rank checker could panic first, and the test would pass for the wrong reason.
- **Docker form.** The runs used the tar-stream mount form of 21-01, with the docker options of run.sh, and not `run.sh` itself. `run.sh` uses the bind mount, which the 21-01 method does not allow (stale files, 19-16). It also has no mode that runs `panic-replay.sh` alone, and it prints no `rustc -V` line for the toolchain check.
- **`--commit-interval 0` on the panic mount** (not in the plan text). With no commit timer, only the write log can keep `a.bin`, so the name `fsynced-file-replayed` is correct.
- **Message with a short submit limit.** The abort line of `submit` gives whole seconds, so at the 300 ms test limit it says `stayed full for 0 s`. The test checks only `queue stayed full`. At the production limit of 150 s the line is correct.
- **Crash reports on macOS.** Each run of `pool_panic_tests` on macOS writes 3 crash reports (`.ips`) to the user's DiagnosticReports folder, one for each child that aborts. This is a side effect of the abort, and it has no other effect.
- POOL-03 is not marked complete: plan 21-09 owns the marks.

## Known Stubs

None.

## Threat Flags

None. No new surface outside the plan's threat model. T-21-09 and T-21-10: the abort comes from the hook before the unwind; the child tests and the Docker block show it for a worker and for the reader, and the red run shows the `destroy()` commit with no hook. T-21-11: the env read is under `#[cfg(debug_assertions)]` (the source gate). T-21-12: the name match is on `fuser-` and `slicefs-worker-` only; `a_panic_on_another_thread_does_not_abort` guards it.

## Open points for a decision

- **Test time on a loaded macOS host.** `pool_panic_tests` took 23 to 60 s in total here. The children's own work is well under 1 s each, and the rest is the start of the test binary at load 50 to 130. On an idle host or on Linux CI, it should be a few seconds. A smaller child binary (for example an example or a bin target) would start faster, but it is more code. No change was made.
- **Abort line at a short submit limit.** The line could give milliseconds (`{:?}` of the Duration) so that it is correct at the 300 ms test limit. That changes the production line from `150 s` to `150s`. No change was made.

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/tests/pool_panic_tests.rs
- FOUND: scripts/docker-linux-test/panic-replay.sh
- FOUND: .planning/phases/21-worker-pool/runs/panic-replay-red.log
- FOUND: .planning/phases/21-worker-pool/runs/verify-21-03.log
- FOUND: 8b1049f, a79f39f, 5e57d3e (ancestors of HEAD)
