# Phase 21: Worker pool - Context

**Gathered:** 2026-11-23
**Status:** Ready for planning

<domain>
## Phase Boundary

Phase 21 runs FUSE requests on a pool of SliceFS worker threads (issue #22). It covers POOL-01 to POOL-05.

- One FUSE reader thread hands each request to a worker pool. A mount option sets the number of workers. With one worker, the handler runs inline as today.
- Unmount and SIGTERM drain the queue and join the workers before `destroy()`. Replies are sent after all locks are released.
- A worker panic aborts the daemon. The next mount replays the write log. No request is served after a half-done update.
- All tests and pjdfstest pass with 4 workers on Linux (Docker CI). The tests pass on macOS FSKit CI with the macOS default.
- Parallel I/O on different files scales with the number of workers on Linux. The result is measured and recorded.

Out of scope: WAL group commit (deferred), chunk profiles (Phase 22), the Phase 19/20 deferred items.

</domain>

<decisions>
## Implementation Decisions

### Worker count option (user answer, 2026-11-23)
- Mount option `--workers N`. Linux default: the CPU count / 2, at least 1, at most 8 (`clamp(available_parallelism / 2, 1, 8)`; the cap of 8 is an owner answer of 2026-11-23). An explicit `--workers N` may be higher (reject 0; no upper bound on the explicit value). Reason for the cap: the Phase 20 memory cap (CAP_MIB 320) was measured with 4 writers; each busy writer can add one own step and a glibc arena.
- macOS default: 1, until a measured throughput gain on FSKit (roadmap criterion 1; research S1 result "Decides" line). An explicit `--workers N` on macOS is allowed.
- `--workers 1` runs the handler inline on the reader thread, as today (no queue, no extra thread).
- **Reversibility:** costly — a public CLI option and a default; later releases must keep it or deprecate it.

### Decided autonomously
- [Pool] Q: "fuser `n_threads` or own pool?" → Selected: "One fuser reader thread plus a SliceFS pool. The vendored fuser refuses `n_threads != 1` on non-Linux (`vendor/fuser/src/session.rs`), and POOL-01 says one reader" — reversible internal structure. The research checks how to move a request plus its `Reply*` object to a worker with the vendored fuser (owned request data, `Send` replies); a small vendored-fuser change is allowed if needed.
- [Pool] Q: "Queue shape?" → Selected: "A bounded queue (for example 2 x workers or a fixed 64); the reader blocks when it is full, so memory stays bounded" — internal; never hold whole files in RAM.
- [Order] Q: "Which requests need order?" → Selected: "No global order (S1: FUSE-T 1.2.9 and Linux accept replies out of order). The research lists requests that need special care: FORGET/BATCH_FORGET, INTERRUPT, INIT, DESTROY, and any request the kernel expects in order for one inode; those run inline on the reader or keep per-inode order if research shows a need" — research item.
- [Shutdown] Q: "Shutdown order?" → Selected: "Unmount/SIGTERM: stop taking requests, drain the queue, join the workers (each wait has a time limit), then `destroy()` (which runs the Phase 20 `shutdown`: flusher stop, then the gate)" — ROADMAP criterion 2.
- [Panic] Q: "Worker panic?" → Selected: "A panic in a worker aborts the process (`std::process::abort` from a catch or a panic hook for worker threads); no reply is sent for the half-done request; the next mount replays the write log. A test shows a remount after an injected worker panic gives the replayed state" — ROADMAP criterion 3.
- [Locks] Q: "Reply after locks?" → Selected: "Each handler returns its reply data; the worker sends the reply after the handler returns and all guards are dropped. In debug builds, assert no ranked lock is held at the reply (Phase 19 rank checker)" — criterion 2.
- [Tests] Q: "How do tests run with 4 workers?" → Selected: "Linux Docker CI runs the FUSE smoke, live checks and pjdfstest with `--workers 4`; macOS FSKit CI runs with the default (1)" — criterion 4.
- [Bench] Q: "How to show scaling?" → Selected: "A Docker Linux run of parallel I/O on different files at 1, 2, 4, 8 and 16 workers, with throughput AND peak memory (VmHWM) per worker count; if 16 scales and memory stays bounded, a later change can raise the default cap of 8, recorded in `docs/research/multithreaded-request-handling.md`; also one macOS CI run at 1 and 2 workers, recorded, to decide whether the macOS default can be raised later" — criterion 5; reversible.
- [Throttle] The 200 ms throttle limit is merged in develop (PR #151, 2026-11-23). It raised the S7 peak: 5 s gave VmHWM 255-289 MiB (4 runs), 200 ms gave 262-340 MiB (7 runs, one over CAP_MIB 320); log: `runs/s7-throttle-200ms.log`.
- [Step limit] (user answer, 2026-11-23) Keep 200 ms. This phase adds a global limit on the number of save steps that run at once (flusher steps plus writers that pay with their own file), so peak memory does not grow with the number of writers or workers. A writer that cannot get a step slot waits with no lock held (time limit, ends at stop), or writes without a step only if the per-file hard cap still holds; the planner picks the exact rule. The S7 run must stay under CAP_MIB 320 again, at 4 writers and at the worker counts of the bench. Reversible internal design.
  - Orchestrator reading of the user answer, 2026-11-23 (from the user's words "keep the 320 MiB cap; peak memory no longer grows with writers or workers"): the 320 MiB cap is a hard bar at every writer or worker count the default can give: 1, 2, 4 and 8 (S7 writers, and the daemon VmHWM of the Linux bench at those worker counts). A miss at any of these keeps POOL-05 and this [Step limit] item open; the close plan does not mark them. At 16 (above the default cap of 8) a miss is recorded and reported to the owner, and does not block. The research rule "Pm(8) or Pm(16) more than 64 MiB above Pm(4)" stays a report-to-owner signal only.
- [Scale bar] (user answer, 2026-11-23) POOL-05 passes when throughput at `--workers 4` is at least 1.5x the `--workers 1` value in the Linux Docker bench (parallel I/O on different files). If it misses, the phase records the numbers and the cause, a follow-up issue is opened, and the pool still ships.
- [Waits] Every new wait (queue push, drain, join) has a time limit or ends at unmount; no wait while a ranked lock is held (Phase 19/20 rules).

- [Allocator] (owner answer 2026-11-23, replaces [Step memory]): the S7 growth per writer is glibc arena retention, not live data (`runs/s7-memory-diagnosis-NOTES.md`). Plan 21-05 sets `mallopt(M_MMAP_THRESHOLD, 128 KiB)` on Linux glibc at daemon start (and in the S7 harness); the step limit and the byte step pool are built only if plan 21-06 still misses CAP_MIB 320 at 1-8 writers. A buffer reuse pool for the large write and rebase buffers stays a later option.
- [Trace default] (2026-11-23, orchestrator decision after a review, reversible): the per-request ENTRY/EXIT lines are off by default; `SLICEFS_FUSE_TRACE=1` turns them on. Reason: stderr is unbuffered with one global lock, so the lines serialize all workers. Error, slow-write and unmount stats lines stay. The POOL-05 metric uses the default (lines off); the bench records the cost of the lines with a trace-on pair. Plan 21-09 documents the variable.
- [Metadata bench] (user answer, 2026-11-23): the Linux bench adds a small-file metadata test (create, stat, read, unlink of many 4 KiB files per client) at 1, 2, 4, 8 and 16 workers, with ops/s and lock waits (lock-stats runs at 4 and 8 workers). Recorded, not judged; it measures the metadata-lock risk named in a review, and it is the baseline for #165 (remove the unused reference-count table) and the prolly-tree evaluation (Phase 24).
- [Stats] (orchestrator, 2026-11-23, for the deferred adaptive control): the pool and the step limit keep cheap atomic counters (queue depth now and max, jobs served per worker, slot waits and their time, steps per second, bytes freed per step) next to the Phase 20 flusher stats, readable by tests and printed at unmount. No controller in this phase.

### Claude's Discretion
- Queue depth, worker thread names, the exact handoff type, how the CPU count is read.

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- `crates/slicefs-cli/src/mount.rs`: builds `fuser::Config` and runs the session; spawns the commit timer, online GC, the Phase 20 flusher and the FUSE-T unmount watcher.
- `crates/slicefs-cli/src/fuse_callbacks.rs`: the `fuser::Filesystem` impl; `write` calls `throttle_write` before the gate; `destroy` calls `shutdown`.
- `crates/slicefs-cli/src/signals.rs`: SIGTERM/SIGINT/SIGHUP end a mount cleanly (self-pipe).
- `crates/slicefs-cli/src/sync.rs`-style shim and rank checker (Phase 19) for the "no lock at reply" assert.
- Phase 18 S1 probe (one reader + 4 workers on the vendored fuser) — the closest prior art for the handoff.
- `scripts/docker-linux-test/run.sh` (smoke, verify, pjdfstest), `mt-linux.sh`.

### Integration Points
- Mount option in `cli.rs` (clap), next to the Phase 20 `--dirty-*` options.
- CI workflow: Linux job passes `--workers 4`.
- `docs/research/multithreaded-request-handling.md`: step 6 and the Phase 21 result.

</code_context>

<specifics>
## Specific Ideas

- Keep tests short. Never hold whole files in RAM.
- Do not run macOS mounts locally; macOS mounts only in CI.

</specifics>

<deferred>
## Deferred Ideas

- Adaptive control from live statistics (owner idea, 2026-11-23): tune the throttle pause, the number of save-step slots and possibly the active worker count from measured values (flusher and store bandwidth, step time, queue depth, RSS), like Linux `balance_dirty_pages` sizes its pauses from measured write bandwidth. Not in Phase 21: fixed values first, so the base behavior is measured and tested. Phase 21 must expose the counters that such a controller needs (see [Stats]); the controller is tracked in issue #152.
- Raising the macOS default above 1: only after a measured FSKit throughput gain (a later decision on the Phase 21 numbers).
- WAL group commit; Phase 19 and 20 deferred items stay open.

</deferred>
