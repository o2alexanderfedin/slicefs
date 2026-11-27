---
phase: 21-worker-pool
reviewed: 2026-11-24T17:06:28Z
depth: standard
files_reviewed: 30
files_reviewed_list:
  - crates/slicefs-cli/src/pool.rs
  - crates/slicefs-cli/src/fuse_callbacks.rs
  - crates/slicefs-cli/src/handlers.rs
  - crates/slicefs-cli/src/filesystem.rs
  - crates/slicefs-cli/src/flusher.rs
  - crates/slicefs-cli/src/mount.rs
  - crates/slicefs-cli/src/alloc_tune.rs
  - crates/slicefs-cli/src/cli.rs
  - crates/slicefs-cli/src/main.rs
  - crates/slicefs-cli/src/lib.rs
  - crates/slicefs-cli/src/fuset_watch.rs
  - crates/slicefs-cli/src/snapshot.rs
  - crates/chunk-store/src/lib.rs
  - crates/chunk-store/src/lock_stats.rs
  - crates/metadata/src/store.rs
  - crates/metadata/src/segment/writer.rs
  - crates/slicefs-cli/tests/reply_after_locks.rs
  - .github/workflows/ci.yml
  - .github/workflows/pool-bench-macos.yml
  - scripts/docker-linux-test/pool-bench.sh
  - scripts/docker-linux-test/panic-replay.sh
  - scripts/docker-linux-test/run.sh
  - scripts/docker-linux-test/mt-linux.sh
  - scripts/docker-linux-test/dedup-probe.sh
  - scripts/docker-linux-test/pjdfstest.sh
  - scripts/docker-linux-test/verify-phases.sh
  - scripts/docker-linux-test/s4-waiting.sh
  - scripts/macos-mount-test/pool-bench.sh
  - scripts/macos-mount-test/boost-test.sh
  - scripts/macos-mount-test/large-copy.sh
findings:
  critical: 0
  warning: 3
  info: 6
  total: 9
  necessity: 0
status: issues_found
---

# Phase 21: Code Review Report

**Reviewed:** 2026-11-24T17:06:28Z
**Depth:** standard
**Files Reviewed:** 30
**Status:** issues_found

## Summary

Scope: `git diff f4856c3^..HEAD -- crates vendor scripts .github`. The range changes no file under `vendor/`, so there are no vendored fuser findings. The review also read the vendored fuser code that the pool relies on: the session loop in `session.rs` (`destroy` runs after the reader threads join) and the stream send lock in `channel.rs` (FUSE-T replies from many threads are sent under one lock). Both are correct for the pool.

For the source files I read the whole changed code. For `filesystem.rs`, `handlers.rs` and `mount.rs` I read the diff and the write, step, gate and shutdown paths it calls. Of the test files I read `reply_after_locks.rs` in full. For `pool_tests.rs`, `store_busy_tests.rs` and `pool_panic_tests.rs` I checked only the timing asserts and the asserts that show the case ran, so these files are not in the list. I found no timing assert there that is not already listed in `deferred-items.md`. `retry_tests.rs`, `alloc_tune_tests.rs`, `stress_harness.rs` and the changed older tests were not reviewed.

I checked these points and found no defect:

- **Pool.** The queue lock is never held while a job runs. Every condvar wait has a time slice and a limit, or ends at the close. `drain_and_join` runs every queued job before the join. `Drop` does not wait again after a failed drain. `Exit` counts a worker that unwinds. `destroy` takes the pool out of `MountedFs`, so a request that comes after it (none is expected, because the reader has ended) runs inline. No request is lost in a closed queue.
- **Reply after locks.** Every callback sends its reply after `serve` returns. `write_job` sends it after `handle_write_retrying`, and that function holds no gate between tries. `readdir` fills the reply buffer outside `serve`.
- **Abort hook.** It matches `fuser-*` and `slicefs-worker-*` threads. No `catch_unwind` on a request path needs the unwind: the non-test `catch_unwind` calls run on the `pack-seal` and `pack-sync` threads, which the hook does not abort.
- **#170 retry safety.** I checked the claim that "nothing of the write was applied" against `write_in_cell`, `stream_step_in`, `to_overlay`, `overlay_base`, `cap_step` and `rebase_unlocked`. Every store error before the apply returns through `?` before any visible change. Every store error after the apply is swallowed (`after_err`), so the result stays `Ok` and that try is not repeated. An `O_APPEND` write cannot get into the file twice by this path.
- **`is_store_busy`.** `content.rs` wraps every chunk-store error that the write path sees with `map_err(std::io::Error::other)`, so the downcast finds the `StoreWaitTimeout`. A search for a conversion that takes the inner `io::Error` out of `Error::Io` found none.
- **Memory.** Queued payload is at most `QUEUE_BYTES` (8 MiB), plus one oversize job when the queue is empty, plus one write payload for each worker. This does not depend on file size.
- **`alloc_tune` FFI.** `mallopt` and `mallinfo2` take no pointers. `malloc_zone_statistics(NULL, &mut s)` uses a valid out pointer, and NULL means all zones. The unsafe code is sound.

The main risk is in liveness, not in data. The 200 s abort limits rest on a per-request time bound that the code does not enforce (WR-01). The #170 retry can stop the whole mount when there is only one request thread (WR-02).

`necessity-review.md` and `cicd-review-patterns.md` were both read. No necessity finding met the evidence rule.

## Narrative Findings (AI reviewer)

## Warnings

### WR-01: The 200 s submit and drain aborts rest on a per-request bound that the code does not enforce

**File:** `crates/slicefs-cli/src/pool.rs:25-35`, `pool.rs:235-243`, `crates/slicefs-cli/src/fuse_callbacks.rs:180-191`
**Relation to recorded items:** `deferred-items.md` lists the stale text of the `SUBMIT_WAIT` comment and says "The value 200 s is correct". This finding disputes the value and the bound behind it, not the comment text. The abort itself is the recorded mitigation T-21-02 (`21-01-PLAN.md:269`). This finding says that its premise ("a full queue for this long means that the workers are stuck") is not true.
**Issue:** `SUBMIT_WAIT` and `DRAIN_WAIT` (200 s) assume that one request ends within 60 + 120 + 10 + 0.2 + 0.2 = 190.4 s. This bound covers only one form of the write path. Other paths can be slower and still be correct:
- `CellGuard::wait_step` (`filesystem.rs:638-656`) starts its 120 s clock again for each new step token (`since` resets when `token.progress` is a different `Arc`). A writer that waits for two steps in a row can wait 240 s.
- A write try that starts just before the 60 s retry limit can wait on the store more than once after its apply. The hard-cap loop calls `cap_step` (`filesystem.rs:3312-3323`), then `rebase_cell` runs (`3329-3334`), then `pay_budget` (`3343-3347`). Each step can wait `STORE_WAIT` (10 s) before it fails, and the write still returns `Ok`.
- `handle_fsync` and `handle_fsyncdir` call `flush_wal()` inside `serve` (`handlers.rs:229`, `handlers.rs:240`). That is an fsync with no time limit.
- `gate_shared()` (`filesystem.rs:1847-1850`) is a `RwLock::read` with no time limit. It waits behind `gate_exclusive`, and the GC start does store I/O while it holds the gate exclusively (`filesystem.rs:2176-2200`: `state.finish` for each open stream).

Failure scenario (drain): the store is on a slow disk, and a WAL fsync takes more than 200 s. A lazy unmount (`fusermount -uz`) or the FUSE-T unmount watcher ends the session while an `fsync` job is in that fsync. `destroy` calls `drain_and_join(DRAIN_WAIT)`, gets `Err(1)` and calls `abort()`. There is no final commit, `mount.lock` stays, and the next mount must replay. Failure scenario (submit): on the same disk, all N workers are in such waits, so the queue (max(2N, 4) slots) stays full for 200 s and the reader aborts the daemon. All users of the mount then get `ENOTCONN`. Before Phase 21 the single thread only stalled. The write log keeps the data, so no data is lost, but a slow daemon that is still alive becomes a crash.
**Fix:**
- In `Pool::submit`, remove the abort on time alone. Keep the warning and repeat it every `SUBMIT_WARN`. The kernel holds the next requests, and each worker wait has a limit of its own or ends at the stop, so the reader cannot wait forever on a live daemon. A real deadlock is a bug for the debug rank checker, not for a 200 s timer.
- In `destroy`, compute `DRAIN_WAIT` from every wait that one request can make (the retry window, plus one store wait for each step a try can run, plus `STEP_WAIT_MAX` for each token wait), and add a WAL-fsync margin. Or abort only when `s.jobs` did not change for the full window, so a drain that still finishes jobs is not cut. Update the doc comment of both constants to the new rule.

### WR-02: The busy-store retry blocks the only request thread for up to about 70 s (macOS default) and can take all workers

**File:** `crates/slicefs-cli/src/fuse_callbacks.rs:634-636`, `crates/slicefs-cli/src/handlers.rs:387-416`
**Issue:** With no pool (`--workers 1`, which is the macOS default), `write` runs `write_job` on the FUSE reader thread. `handle_write_retrying` then sleeps on that thread between tries, for up to `STORE_BUSY_RETRY_MAX` (60 s). A try that starts just before the limit adds one more store wait of up to 10 s, plus the after-apply waits of WR-01. During this time the reader reads no request, so every `getattr`, `lookup` and `read` on the mount (Finder and Spotlight requests too) stops for about 70 s. Before #170 the same write failed with EIO after one 10 s wait. With a pool of N workers, N parallel writers on a busy store hold all N workers in their sleeps. Reads then queue behind them in the 2N-slot queue, and the reader blocks in `submit`. `deferred-items.md` lists only the drain case (a retry during the drain), not this stall in normal use.
**Fix:** Make the retry window a parameter of `handle_write_retrying` (today it reads `fs.store_busy_limit()` inside the function, `handlers.rs:389`). `write_job` then passes a short window on the inline path, for example one more try or 2 x the 10 s store wait, and the full window with a pool. With a pool, also limit the number of workers that can be in a busy-retry sleep at one time to N - 1 (an atomic counter on `SliceFsFilesystem`), so one worker stays free for other requests. A writer over that limit gets EIO at once, as before #170. If the long stall is the intended trade-off, record it as a decision, and record that it applies to every FSKit mount by default.

### WR-03: The Linux CI job has no job-level `timeout-minutes`

**File:** `.github/workflows/ci.yml:15-17`
**Issue:** This phase changed the `linux` job: it now runs three steps with 4-worker mounts. The job has `runs-on: ubuntu-latest` and no `timeout-minutes`, so a hang holds the runner until the 6-hour default. pjdfstest has its own 3000 s limit, and the daemon aborts at its 200 s drain limit. But nothing limits a `cargo test --workspace` that hangs (for example a pool or busy-store test that deadlocks), a `docker build` or `docker run` that does not return, or a script step with no `timeout` around it. The new `pool-bench-macos.yml` sets `timeout-minutes: 45`, but `ci.yml` does not. (The `macos` job at line 45 has the same gap. This phase did not change that job.)
**Fix:** Add a job-level limit above the normal run time, for example:
```yaml
  linux:
    runs-on: ubuntu-latest
    timeout-minutes: 120
```

## Info

### IN-01: The throttle answer is taken once and used again in tries up to 60 s later

**File:** `crates/slicefs-cli/src/fuse_callbacks.rs:125`, `fuse_callbacks.rs:130-139`
**Issue:** `admit = fs.throttle_write()` runs once, before `handle_write_retrying`. If it returns `Admit { pay: false }`, every try skips `pay_budget` (`filesystem.rs:3343`), even a try 60 s later when the total can be far above the budget. Each retrying worker can then add up to its write size above the budget without paying. The hard cap of each file still holds, so the overshoot is bounded (workers x the largest write).
**Fix:** Call `throttle_write()` again before each new try (it holds no lock), or use `Admit { pay: true }` for all tries after the first.

### IN-02: `gate_wait_ms` in the slow-write line includes the busy-retry sleeps

**File:** `crates/slicefs-cli/src/fuse_callbacks.rs:128-145`
**Issue:** `gate_wait = t_serve.elapsed() - handle`. `handle` adds up only the handler time of each try, so the sleeps of `handle_write_retrying` (50 ms to 1 s each, up to 60 s in total) are reported as `gate_wait_ms`. A busy-store stall then looks like gate contention in the logs and in the bench analysis.
**Fix:** Measure the time outside `serve` separately. Print it as `busy_wait_ms` (or count it in the retry loop), and subtract it from `gate_wait_ms`.

### IN-03: A busy step marks the flusher pass as stuck, so throttled writers step against the busy store

**File:** `crates/slicefs-cli/src/flusher.rs:218`, `flusher.rs:252-255`; `crates/slicefs-cli/src/filesystem.rs:3444-3446`
**Issue:** The comment in `step()` says the back-off "keeps the flusher from adding load" to a busy store. But `stuck` now includes `st.busy > 0`, and `throttle_write` then ends every throttled writer's wait with `pay: true` (`no_room_exits`). Each of those writers then runs its own steps against the same busy store, and each step can wait up to `STORE_WAIT` while it holds the commit gate shared. The 200 ms throttle limit bounds the effect, so it is small. But the code does the opposite of what the comment says, and no plan or summary records it as a decision.
**Fix:** Leave `busy` out of `stuck` (a busy store can recover, so more waiting can help), or change the comment to say that writers pay for themselves while the store is busy.

### IN-04: The inline request path never turns on the debug check for a lock taken outside the gate

**File:** `crates/slicefs-cli/src/pool.rs:349`, `crates/slicefs-cli/src/fuse_callbacks.rs:81-86`
**Issue:** `rank::require_gate(true)` is set on the pool workers (`pool.rs:349`) and on the flusher (`flusher.rs:91`). The per-thread flag is off by default (`chunk-store/src/sync.rs:64`, `NEED_GATE = false`). The FUSE reader thread, which runs the requests inline with `--workers 1`, never sets it. So with one worker (the macOS default, and the only mode the macOS FSKit CI tests), a debug build does not catch a cell or stripe lock taken outside `serve`.
**Fix:** Call `chunk_store::sync::rank::require_gate(true)` in the `None` branch of `MountedFs::run` and in the inline branch of `write`. The call only sets a thread-local flag, so it costs almost nothing.

### IN-05: A very large `--workers` value panics or aborts after the replay instead of returning an error

**File:** `crates/slicefs-cli/src/pool.rs:187`, `pool.rs:203`, `pool.rs:208`; `crates/slicefs-cli/src/mount.rs:519`
**Issue:** The missing upper limit is an owner decision. But `Pool::new(n)` computes `2 * n` (a debug build panics on overflow when n > usize::MAX / 2), collects `n` atomics, and calls `Vec::with_capacity(n)` (an allocation failure aborts). This runs on the main thread after the write-log replay and after the flusher starts. So a value such as `--workers 99999999999` gives a panic or an abort, not the clean error "cannot start N request workers".
**Fix:** Use `n.checked_mul(2)`, and let the vectors grow as the threads start. Or refuse, in `run_mount` before the replay, values that no system can run (for example above 65,536). The decision of "no upper bound" for real worker counts stays the same.

### IN-06: The reply-after-locks scan does not see a reply that is sent inside the `serve` closure

**File:** `crates/slicefs-cli/tests/reply_after_locks.rs:139-157`
**Issue:** `every_request_callback_uses_serve` checks only that the callback body contains `serve(`. A callback written as `serve(&fs, || { ...; reply.ok() })` passes the scan but sends its reply with the gate held. The debug assert in `serve` (`fuse_callbacks.rs:100-104`) does not catch this either: it runs after the gate is dropped and checks only the rank stack. I read every current callback, and none does this. The test is the only evidence for POOL-02, but it cannot detect a future regression of this kind.
**Fix:** In the scan, extract the closure text of each `serve(` call (to the matching `)`) and fail if it contains `reply.`. Or have `serve` set a thread-local "in serve" flag, and check it in a debug-only wrapper of the reply types.

---

_Reviewed: 2026-11-24T17:06:28Z_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_

## Follow-up

All Warning and Info findings are tracked in issue #174 (orchestrator decision, 2026-11-24: no fix in Phase 21, so the CI evidence for 7853542 stays current; no data loss in any case, because the write log replays).
