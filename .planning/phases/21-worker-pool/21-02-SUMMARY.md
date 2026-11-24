---
phase: 21-worker-pool
plan: 02
subsystem: concurrency
status: complete
tags: [pool-01, pool-02, worker-pool, reply-after-locks, trace-switch]

requires:
  - phase: 21-worker-pool
    provides: "21-01: Pool, MountedFs { fs, pool }, run, serve (gate only inside it, debug assert), getattr on the pool, drain in destroy, SLICEFS_WORKERS in run.sh and mt-linux.sh"
provides:
  - "All 31 request callbacks of MountedFs go through run (write: its own two-branch form) and take the gate only inside serve; the reply is sent after serve returned"
  - "write_job (free function): throttle_write with no lock held, handle_write_admitted inside serve, then the slow-write line and the reply; WriteReq holds the copied request fields"
  - "SLICEFS_FUSE_TRACE=0 (read once, OnceLock) and the fuse_trace! macro for every per-request ENTRY and EXIT line"
  - "crates/slicefs-cli/tests/reply_after_locks.rs: the_gate_is_taken_only_in_serve_and_init, every_request_callback_uses_serve"
  - "fuse_callbacks unit test serve_panics_when_a_lock_is_held_at_the_reply (debug builds)"
affects: [21-03, 21-07, 21-08, 21-09]

estimate:
  tokens: 100000
  tasks: 3
actuals:
  tokens: 12200    # chars/4 over the added lines of the diff (code, test, three Docker logs)
  tasks: 3
  commits: 3

plan_head_before: 0852a767b39ea8a139cebbce2af30d30f20642e8
plan_head_after: 925f8a3f3404ae5fa7b24675572ee2ac660f16a3
commits: 3

tech-stack:
  added: []
  patterns:
    - "Copy the arguments on the reader (to_os_string, to_path_buf, uid and gid), clone the Arc, run(weight, job); in the job: ENTRY line, serve(handler), EXIT line, reply"
    - "A callback with no handler still takes the gate through serve(&fs, || ()), as before this plan"
    - "Source scan: the nearest previous fn header encloses a line; the impl ends at the first column-0 brace"

key-files:
  created:
    - crates/slicefs-cli/tests/reply_after_locks.rs
    - .planning/phases/21-worker-pool/runs/smoke-21-02.log
    - .planning/phases/21-worker-pool/runs/verify-21-02.log
    - .planning/phases/21-worker-pool/runs/verify-21-02-before.log
  modified:
    - crates/slicefs-cli/src/fuse_callbacks.rs

decisions:
  - "Names are copied on both paths (the run job is 'static); only the write payload keeps the borrowed no-copy path with one worker"
  - "The slow-write line keeps its three fields; gate_wait_ms is the serve time less the handler time"
  - "setxattr copies its value on both paths and weighs its length; write weighs the payload length on the pool path"
  - "The fsyncdir error line stays a plain eprintln (an error line, not a trace line)"

metrics:
  duration: "about 55 min"
  completed: 2026-11-24
---

# Phase 21 Plan 02: All request callbacks on the pool, reply after the gate Summary

Every request callback of `MountedFs` (31: all overridden methods except `init` and `destroy`) now runs as a job. The job takes the commit gate only inside `serve` and sends the reply after `serve` returned. Thus no reply is sent with the gate held, also with one worker. A source test and a debug assert guard this rule. `SLICEFS_FUSE_TRACE=0` turns off the per-request trace lines. Docker Linux mounts pass with 4 workers, 1 worker, 4 workers with the trace off, and the container default of 8 workers.

## What was built

- **Read side (Task 1).** lookup, readdir, read, open, opendir, releasedir, statfs, access, getxattr, listxattr, readlink and getlk use the getattr form of 21-01. The reader copies the arguments, clones the `Arc` and calls `self.run(0, ...)`. In the job: the ENTRY line, `serve(handler)`, the EXIT line and the reply. readdir fills its reply buffer after `serve`. opendir calls `alloc_dir_handle` inside `serve`. releasedir and access have no handler and call `serve(&fs, || ())`, so they still take the gate as before.
- **Trace switch (Task 1).** `trace_on()` reads `SLICEFS_FUSE_TRACE` once (`OnceLock<bool>`). The value `0` turns the lines off. Any other value, or no value, keeps them on. The `fuse_trace!` macro prints and formats only when the flag is on. All per-request ENTRY and EXIT lines use it. The init and destroy lines (also `destroy: pool: ...`, which `mt-linux.sh` reads), the `[slow-write]` line and the fsyncdir error line stay plain `eprintln!`.
- **Mutating side (Task 2).** release, flush, fsyncdir, fsync, setxattr, removexattr, write, create, mkdir, mknod, symlink, link, unlink, rmdir, rename, setattr, setlk and fallocate use the same form. create, mkdir, mknod and symlink copy `req.uid()` and `req.gid()` before the hand-off. The EINVAL name checks are in the job, outside `serve`. fsync and fsyncdir keep the durability wait inside their handler, so inside `serve`, as before.
- **write (Task 2).** `write` matches on the pool. With no pool, it calls `write_job` with the borrowed `data` on the reader (no copy). With a pool, it copies `data` (`data.to_vec()`) and submits a job with the weight `data.len()`. `write_job` calls `throttle_write()` with no lock held, prints the ENTRY line, runs `handle_write_admitted` inside `serve`, prints the slow-write line when the sum is 50 ms or more, then the EXIT line and the reply.
- **Checks (Task 2).** `tests/reply_after_locks.rs` reads the source with `include_str!`. `the_gate_is_taken_only_in_serve_and_init` asserts that each non-comment `gate_shared(` line is inside `fn serve` or `fn init`, and that there are exactly 2. `every_request_callback_uses_serve` asserts that each method of `impl Filesystem for MountedFs` other than `init` and `destroy` contains `serve(`, or calls a `*_job(` free function that contains it, and that there are 31. The unit test `serve_panics_when_a_lock_is_held_at_the_reply` (debug builds) gives `serve` a closure that returns a `rank::enter(Rank::Short)` marker and expects the panic "reply with a lock held".

Counts on the final tree: non-comment `gate_shared(` lines: 2 (HEAD before: 32; after Task 1: 20). Plain `eprintln!` ENTRY and EXIT lines: 4 (init and destroy). `SLICEFS_FUSE_TRACE`: 2 lines. `data.to_vec()`: 1. 30 callbacks call `self.run(`, and `write` is the 31st.

## Red runs (not committed)

| # | Test | Red tree | Result |
|---|------|----------|--------|
| 1 | `the_gate_is_taken_only_in_serve_and_init` | the Task 1 tree (0d91b85) with the new test file | FAILED: `the gate is taken outside ["serve", "init"] (18 sites): ["release:300", "flush:351", "fsyncdir:380", "fsync:398", "setxattr:501", "removexattr:517", "write:555", "create:588", "mkdir:618", "mknod:649", "symlink:675", "link:729", "unlink:751", "rmdir:773", "rename:804", "setattr:850", "setlk:927", "fallocate:944"]` |
| 2 | `every_request_callback_uses_serve` | the same tree | FAILED: `callbacks that do not use serve (18): ["release", "flush", "fsyncdir", "fsync", "setxattr", "removexattr", "write", "create", "mkdir", "mknod", "symlink", "link", "unlink", "rmdir", "rename", "setattr", "setlk", "fallocate"]` |
| 3 | `serve_panics_when_a_lock_is_held_at_the_reply` | guard mutation: the `debug_assert!` removed from `serve` | FAILED: `serve returned with a lock held and did not panic: ()`. The file was restored from a saved copy; the assert is back (2 lines with "reply with a lock held"). |

Each red run had a `timeout 600` or `timeout 900`.

## Green runs

- `cargo test --workspace` exited 0 before each code commit: 0d91b85 (Task 1, 70 `test result: ok` lines) and ef8c991 (Task 2, 71 lines, one more binary: `reply_after_locks`). No `FAILED`.
- Task 1 verify: `cargo check --workspace --all-targets` exit 0 (only the 6 old `fuser` warnings); `fuse_callbacks::` 2 passed; `gate_shared(` count 20.
- Task 2 verify: `reply_after_locks` 2 passed; `serve_panics_when_a_lock_is_held_at_the_reply` passed (with the other 2 `fuse_callbacks::` tests); `no_take_out` 4 passed; `gate_shared(` count 2.
- VALIDATION full-suite extras on ef8c991: `cargo check --workspace --all-targets --features slicefs-cli/lock-stats` exit 0; `cargo check -p slicefs-cli --all-targets --features deadlock-detect` exit 0; `--features deadlock-detect` `stress_harness serialize_off_target` 1 passed, `flusher_tests` 26 passed.

## Docker runs (Task 3)

All three logs have `rustc 1.100.0 (0ef1bc386 2026-11-09)`, the same as the `RUSTC = ...` line of the 20-01 notes. The logs have no user or home path. The runs used volume `slicefs-target-p21`, a debug build, the mount form and the 45 s wait. One container ran at a time.

- **Baseline, `runs/verify-21-02-before.log`.** BASE = 0852a76 (the parent of the first 21-02 commit), streamed with `git archive`. No `SLICEFS_WORKERS`. `Workers: 8 (default)`. 56 `: ok`, 0 `: FAIL`, `RESULT verify-phases: PASS`.
- **Smoke, `runs/smoke-21-02.log`.** Tar stream of the clean tree at ef8c991. 3 runs of `mt-linux.sh`, all 11 steps `ok` each time, 3 `daemon-exit: ok`, 0 `: FAIL`, `smoke-exit=0`.
  - `workers=4 trace=on`: `Workers: 4 (set)`; `pool: workers=4 jobs=163 max_weight=131072 max_queue_wait_ms=1 queue_max=2 jobs_per_worker=35,42,43,43`; `trace-on-entry-lines=5`.
  - `workers=1 trace=on`: `Workers: 1 (set)`; `pool-line: none` (no pool, inline on the reader); `pool-lines-1-worker=0`.
  - `workers=4 trace=off`: `Workers: 4 (set)`; `pool: workers=4 jobs=163 max_weight=131072 max_queue_wait_ms=0 queue_max=2 jobs_per_worker=39,41,36,47`; `trace-off-entry-lines=0`; `lock-held-asserts=0`.
- **Live checks, `runs/verify-21-02.log`.** Tar stream of the clean tree at ef8c991. No `SLICEFS_WORKERS`: `Workers: 8 (default)`. 56 `: ok` (the same as the baseline), 0 `: FAIL`, 0 `NOT MOUNTED`, `RESULT verify-phases: PASS`, `lock-held-asserts=0` in its mount log.

jobs=163 against jobs=5 in 21-01: all requests now go through the pool. max_weight=131072 is one 128 KiB write payload (the Linux FUSE write size).

## Deviations from Plan

### Auto-fixed Issues

None. The plan was executed as written, with these notes:

- **Baseline run order.** The baseline live checks ran first, at the start of the plan, in the background while Task 1 was written. BASE is the plan's base (0852a76), and `git archive` reads only that commit, so the result is the same as a run after Tasks 1 and 2.
- **Slow-write line.** The plan text says "throttle ms and serve ms". The line keeps its three old fields (`throttle_ms`, `gate_wait_ms`, `handle_ms`), so the log format does not change. `gate_wait_ms` is now the `serve` time less the handler time. No script in the repository reads this line.
- **Name copies.** `run` takes a `'static` job, so the names (`to_os_string`, `to_path_buf`) are copied also with one worker. Only the write payload keeps the borrowed path with one worker, as the plan's truth says. A name is at most 255 bytes.
- **Extra lines in the smoke log.** The log also has `trace-on-entry-lines=` (after run 1), `pool-lines-1-worker=` (after run 2) and `lock-held-asserts=`, so the 0 after the trace-off run can be compared with a non-zero value.

## Known Stubs

None.

## Threat Flags

None. No new surface outside the plan's threat model. T-21-06: `serve` owns the gate, the scan was red on the half-converted tree, and the debug assert has a unit test. T-21-07: the pool path owns copies of all names and payloads; the compiler refuses a borrowed value in a `'static` job.

## Open points for a decision

- **POOL-01 and POOL-02 marks.** The plan lists them, but plan 21-09 owns the marks (21-01 also left them open). They are not marked complete here.
- **Doc comments of getlk and setlk.** They still say "SliceFS runs single-threaded". The lock calls are still no-ops, so the behavior is correct, but the reason in the text is now old. This plan did not change the text (out of scope); plan 21-09 (docs) can fix it.
- **No-handler callbacks take the gate.** releasedir, access, setlk and fallocate take the gate for nothing through `serve(&fs, || ())`, as they did before this plan. This keeps the scan rule simple. To remove the gate there, the scan test needs an allow-list.

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/tests/reply_after_locks.rs
- FOUND: .planning/phases/21-worker-pool/runs/smoke-21-02.log
- FOUND: .planning/phases/21-worker-pool/runs/verify-21-02.log
- FOUND: .planning/phases/21-worker-pool/runs/verify-21-02-before.log
- FOUND: 0d91b85, ef8c991, 925f8a3 (ancestors of HEAD)
