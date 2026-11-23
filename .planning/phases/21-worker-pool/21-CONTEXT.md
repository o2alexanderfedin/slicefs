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
- Mount option `--workers N`. Linux default: the CPU count / 2, at least 1 (`max(1, available_parallelism / 2)`). No upper cap was asked for; the planner may add a sanity check (reject 0) but no cap.
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
- [Bench] Q: "How to show scaling?" → Selected: "A Docker Linux run of parallel I/O on different files at 1, 2, 4 and 8 workers, recorded in `docs/research/multithreaded-request-handling.md`; also one macOS CI run at 1 and 2 workers, recorded, to decide whether the macOS default can be raised later" — criterion 5; reversible.
- [Waits] Every new wait (queue push, drain, join) has a time limit or ends at unmount; no wait while a ranked lock is held (Phase 19/20 rules).

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

- Raising the macOS default above 1: only after a measured FSKit throughput gain (a later decision on the Phase 21 numbers).
- WAL group commit; Phase 19 and 20 deferred items stay open.

</deferred>
