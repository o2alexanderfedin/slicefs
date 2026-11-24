---
status: resolved
trigger: "GitHub issue #170: write returns EIO under heavy S7 load (4 x 2 GiB): flusher errors and a 45 s commit"
created: 2026-11-23
updated: 2026-11-24
---

## Symptoms

- **Expected:** under load, a write waits (throttle) or succeeds. It never returns EIO only because the store is busy.
- **Actual:** one S7 stress run (in-process harness, flusher on, 4 writers x 2048 MiB, seed `1795492903707110156`, Docker Linux) had 4 writes return EIO (errno 5); the read check then found the blocks of those failed writes. The same run had `flusher_errors=3`, one commit of 45.6 s, and 124 MiB dirty (budget 64 MiB).
- **Errors:** errno 5 (EIO) from write; flusher_errors=3. The debug lines of that run were not kept.
- **Timeline:** seen once during Phase 21 (worker pool) S7 runs, 2026-11-23. A repeat with the same sizes and another seed passed. Not known if it happens before Phase 21.
- **Reproduction:** S7 harness with `S7_THREADS=4`, 2048 MiB per writer, seed `1795492903707110156`, in Docker Linux (see `scripts/docker-linux-test/`, `runs/s7-wave0-NOTES.md`, `.planning/phases/21-worker-pool/runs/s7-memory-diagnosis-NOTES.md`).
- **Possible cause (not confirmed):** a store wait with a 10 s limit times out while a long commit holds the store, and the flusher and the write path turn the timeout into EIO.

## Current Focus

- hypothesis: CONFIRMED (H1). During a disk stall a stream write (`stream_step_in` -> `Stream::push` -> chunk-store `put`) waits at `KEEP_CAP` for the background pack sync; the wait passes the store wait limit and gives a busy error (`TimedOut` with `StoreWaitTimeout`); `stream_step_in` (and `step_io`, `overlay_base`) map it to EIO. FIXED in commits 76427b8 and d0c0bd0 (+ a test-timing commit).
- test: done (see Evidence and Resolution.verification).
- expecting: n/a
- next_action: the human confirms the fix in a real workflow (a mounted run under a disk stall, or the S7 line with `S7_STORE_WAIT_MS` unset on a loaded host).

reasoning_checkpoint:
  hypothesis: "A store-wait timeout (10 s wait at the 24 MiB unsynced cap of the open pack, behind a slow background sync) makes a stream push, a rebase step and a flusher step fail with EIO, because the CLI wraps the TimedOut store error in io::Error::other (kind Other) and maps every store error to EIO."
  confirming_evidence:
    - "r2 (before the fix): 12 `timed_out` calls `a pack create, a record write, a seal or a background sync`, backtrace put <- splice <- step_io <- flush_step; `[FLUSH] step of ino=N failed (errno 5)`; `flusher_errors=3`; a 22 s background sync in the sync log."
    - "store_busy_tests before the fix: `[FUSE] stream: content store error: ... waited 150 ms ...` then `write 23 of 30 MiB failed with errno 5` (the 24 MiB cap)."
    - "S7 in Docker with a 150 ms store wait and the old write behavior: 4 x `op=write errno 5` and 4 x `block differs from version 1`, the signature of the failed run; with the fix: 0 mismatches, 44 retries."
  falsification_test: "If the fix were wrong, S7 with S7_STORE_WAIT_MS=150 would still show EIO or flusher_errors>0 after the fix, or the red tests would stay red; or the retry would double an O_APPEND write (the append test checks size and content)."
  fix_rationale: "The error is a wait that ended, not a failure: nothing was applied and the store is not failed. So the write request waits again with the gate released (bounded by 60 s and shutdown), and the flusher puts the cell in a back-off without counting an error. This removes the cause (EIO on busy), not the symptom (no change of the check or the model)."
  blind_spots: "The fix was not seen under a real 10 s stall (d2 had no stall; the 150 ms runs and the in-process tests stand in for it). `flusher_busy` was 0 in the S7 run, so the flusher arm is covered by the in-process test only. fsync, release, replay and the commit still map a busy store to EIO (not seen in S7). `prepare_manifest` puts a manifest node and keeps a failed put as ObjectKey::ZERO plus a deferred commit error (from code reading, not tested). The shutdown check does not fire during the pool drain (`shut_down` is set after it); the 200 s drain limit bounds it."
  candidate_causes:
    - "code: busy timeout mapped to EIO in the write path (confirmed)"
    - "environment: a stalled disk (dd + sync load in the same container, or host load) that makes a background sync last 10 to 40 s (confirmed as the trigger; not a bug of SliceFS)"
    - "config: STORE_WAIT 10 s too short for the host (a tuning question; the fix makes the value less critical)"
    - "data: a seed-bound input (eliminated: another seed and a quiet disk pass; the seed does not matter)"
  and_gate: "yes, two conditions together: a disk stall longer than the store wait AND a write path that turns the busy error into EIO. Without the stall nothing fails (r1, row 7). With the stall and the fix nothing fails. Both are recorded as root causes: the stall is the trigger, the EIO mapping is the defect."

## Evidence

- timestamp: 2026-11-24
  checked: stress_harness.rs S7 write phase, filesystem.rs write_in_cell / stream_step_in
  found: the 4 failed writes are at 202-205 MiB, in the first phase (1 MiB sequential writes), so the file is in `WriteMode::Streaming`; the write goes `stream_step_in` -> `Stream::push` (chunk store puts) and line 2367 maps every store error to EIO with the cell unchanged ("nothing changes"). A failed write is not applied. Later writes then have `offset > next_expected_offset`, so they go to `pending` (reorder window 64 MiB), then `to_overlay`; this would explain max_dirty 124 MiB over the 64 MiB budget and "block differs from version 1" at the first block of the failed MiB.
  implication: the EIO source is probably the stream push, not a flusher step; each thread failed exactly once (one stall event). Not yet confirmed by a log.
- timestamp: 2026-11-24
  checked: commit_timer.rs `commit`, chunk-store lib.rs wait sites, retry.rs
  found: `commit` takes `w = Instant::now()` AFTER `fs.content().sync()`, so `commit_total_max_ms=45634` is `commit_cut_finish` (`meta.finish_commit`: node writes, syncs, root record) and not the early content sync. Hold of the gate was 182 ms. `timed_out()` (10 s) has four sites in chunk-store: put wait (record write / pack create / seal / background sync at KEEP_CAP 24 MiB), `lock_when`, GC install, `lock_no_seal` (called from `flush_frozen` and `sync`). `flush_if_full` at the end of `put` can also return the TimedOut of `lock_no_seal` AFTER the record was published.
  implication: a disk stall longer than 10 s on one pack sync makes every put wait 10 s and fail; the data of that put may already be in the store (flush_if_full case).

- timestamp: 2026-11-24
  checked: run r1-seedA (same seed, 4 x 2048 MiB, Docker Linux, DBG170 instrumentation, quiet host; log in the scratchpad r1-seedA.log)
  found: passed, no EIO. max pack sync 0.78 s, `flusher_errors=0`, `commit_total_max_ms=577.8`. Does not reproduce on a quiet disk (same as the earlier repeat, row 7).
  implication: the failure needs a disk stall; it is not seed-bound.
- timestamp: 2026-11-24
  checked: run r2-stress (same seed and sizes, plus a dd conv=fsync loop and a `sync` loop in the same container, log r2-stress.log)
  found: one pack background sync took 22018 ms (LOG_SYNCS line `sync bytes=8403404 ms=22018.1 bg=1`), another 10883 ms; `commit_total_max_ms=20065.6`; `slowest_write_ms=10032.8`; `flusher_errors=3` (the same count as the failed run). 12 `timed_out` calls, all `what="a pack create, a record write, a seal or a background sync"` = the wait at KEEP_CAP in `ChunkStore::put` (backtrace: `chunk_store::timed_out` <- `ChunkStore::put` <- `file::splice` <- `step_io` <- `rebase_unlocked` <- `free_dirty` <- `flush_step` <- `flusher::step`). Error text: `rebase: content store error: chunk store I/O: the chunk store waited 10 s for a pack create, a record write, a seal or a background sync | debug=Custom { kind: Other, error: Io(Custom { kind: TimedOut, error: StoreWaitTimeout(..) }) } kind=Other`; then `[FLUSH] step of ino=N failed (errno 5)`. No stream-write EIO in this run (the stall hit the rebase phase, where a write's failed own step is only a note: `after_err`).
  implication: CONFIRMED for the flusher: store-wait TimedOut -> `step_io` `store_err` -> EIO -> `FlushStep::Failed` -> `flusher_errors`. The `io::Error::other(chunk_store::Error)` wrapper in `content.rs` hides the TimedOut kind (kind=Other), so the CLI cannot tell busy from a disk failure without a downcast; `is_store_wait` is private. The write EIO at 202-205 MiB is the same TimedOut at the stream push (`stream_step_in` line 2367 -> EIO, nothing changes); the code path is certain, a log line of it is not (needs the red test).
- timestamp: 2026-11-24
  checked: where the retry must live
  found: `write_job` (FUSE) and the harness `put` call `throttle_write` with no lock, then the handler under the shared commit gate (`serve` / `ctx.gated`). A retry inside the handler would hold the shared gate through the back-off, and a queued commit (exclusive gate request) then blocks every other request for as long as the stalled writer waits.
  implication: the back-off must be outside the gate: retry loop around `serve`, not inside `write_in_cell`.

- timestamp: 2026-11-24
  checked: red test `store_busy_tests::a_write_waits_for_a_busy_store_and_succeeds` on the unfixed write path (MemIo with held syncs, `Options::store_wait` 150 ms, file in streaming mode, 1 MiB writes through `handle_write_admitted`)
  found: `[FUSE] stream: content store error: chunk store I/O: the chunk store waited 150 ms for a pack create, a record write, a seal or a background sync` then `write 23 of 30 MiB failed with errno 5` (the 24 MiB unsynced cap, `KEEP_CAP`, of the open pack). The append test and the limit test were red too.
  implication: the write half is CONFIRMED in process: the stream push `put` gives a store-wait timeout and `stream_step_in` maps it to EIO. Same mechanism as the 4 EIO writes at 202-205 MiB (each writer hit the held background sync of the same pack once, at the same time).
- timestamp: 2026-11-24
  checked: the 45.6 s commit (`commit_total_max_ms`), r2 log around `finish_ms`
  found: `commit` measures `total` after its first `content().sync()`, so the 20 s (r2) or 45 s (failed run) is `meta.finish_commit`: node puts plus the pre-root `ChunkStore::sync` (`fsync` of the active pack and the directory) on the same disk that was stalled. In r2 the `[pack] sync bytes=8403404 ms=22018.1 bg=1` line and the commit end are inside the same 20 s window; the gate hold was 281 ms. The commit waited on the disk (no ranked lock held: hold_max 0.28 s), not on a lock.
  implication: the long commit and the busy errors have one cause, a stalled disk (here made by a dd + sync loop in the same container; on the failed run an unknown host load). The 124 MiB dirty is the effect: each failed stream write leaves a hole, the later writes go to `pending` (reorder window) and then to an overlay.

- timestamp: 2026-11-24
  checked: S7 on real disk in Docker Linux with `S7_STORE_WAIT_MS=150` (new harness knob: `Options::store_wait` 150 ms instead of 10 s, so the ordinary 0.2 to 0.8 s background syncs make busy stores often), seed 1795492903707110156, 4 x 2048 MiB, flusher on. d4 = write retry limit 0 (the old write behavior), d3 = the fix.
  found: d4 (no retry): `S7 MISMATCH op=write errno 5` four times, one for each writer, then four `op=read ... block differs from version 1` (blocks 107008, 107264, 108544, 107520): the same signature as the failed run (4 EIO, one block of each file off-model), test FAILED. d3 (fix): `S7 threads=4 ... write_busy_retries=44 flusher_errors=0`, no mismatch, test ok (96.7 s). The log has 44 `[FUSE] stream: content store error: ... waited 150 ms ...` lines, each followed by the retry.
  implication: CONFIRMED end to end on the real harness: a store-wait timeout in the stream push is the EIO of #170 and the fix removes it. (Run files in the scratchpad: d3-fix-wait150.log, d4-nofix-wait150.log.)

## Eliminated

- hypothesis: the EIO comes from the WAL wait (`metadata/src/wal/mod.rs`) or from a failed store (`inner.failed`)
  evidence: all 12 timeouts in r2 are the put wait; the store is not failed (later puts succeed, run ends with the data checked); the error is `TimedOut` with `StoreWaitTimeout`. Not seen in any log.
  timestamp: 2026-11-24

## Resolution

- root_cause: A slow pack background sync (a stalled disk: 22 s sync and a 20 s commit seen under a dd + sync load; 45 s commit in the failed run) keeps a put waiting at the 24 MiB unsynced cap (`KEEP_CAP`) of the open pack. The wait ends after `STORE_WAIT` (10 s) with a store-wait timeout (`io::ErrorKind::TimedOut`, inner `StoreWaitTimeout`). The CLI wraps store errors with `io::Error::other` (kind becomes `Other`) and maps every one to EIO: `stream_step_in` (the write path of a streaming file: EIO to the writer, once for each of the 4 writers at the same time), and `step_io` / `overlay_base` (the flusher step: `flusher_errors`, and the writer's own rebase step, which is only a note). A failed stream write applies nothing, so the file has a hole of that MiB, and the later writes go to `pending`, then an overlay (124 MiB dirty). The 45 s commit is the same stalled disk (`finish_commit`: node puts and the pre-root `sync`, no lock held; gate hold 0.18 s).
- fix: (1) chunk-store: `is_store_busy()` (public classifier by the `StoreWaitTimeout` downcast, also through `io::Error::other`), `Options::store_wait` (test knob, default 10 s). (2) CLI: a per-thread busy mark set at the three places where a store error becomes EIO (`stream_step_in`, `overlay_base`, `step_io`); `handlers::handle_write_retrying` (used by the FUSE write job and the S7 harness) tries a write again after an EIO with the busy mark, with the commit gate released and a pause (50 ms doubling to 1 s) between the tries, until `STORE_BUSY_RETRY_MAX` (60 s) or shutdown, then EIO; `FlushStep::Busy` (no error, counted as `busy`, same back-off); `SUBMIT_WAIT` / `DRAIN_WAIT` 150 s -> 200 s (one request now ends within 190.4 s). Logs: `[FUSE] write: the chunk store is busy ...`, `[FLUSH] step of ino=N met a busy chunk store ...`; counters `write_busy_retries`, `flusher_busy` in the S7 line.
- verification: red before the fix: `store_busy_tests` (3 of 4 tests red, the 4th red after disabling the Busy arm): `write 23 of 30 MiB failed with errno 5` with the line `[FUSE] stream: content store error: ... waited 150 ms ...`; S7 in Docker Linux with `S7_STORE_WAIT_MS=150` and the write retry limit 0 (old behavior): 4 x `op=write errno 5` and 4 x `block differs from version 1`, FAILED. Green after: the 4 tests pass on macOS and Docker Linux; S7 4 x 2048 MiB seed 1795492903707110156, store wait 150 ms: 44 write retries, 0 mismatches, `flusher_errors=0`, ok. `cargo test --workspace`: macOS 1526 passed, 0 failed (exit 0); Docker Linux 1513 passed, 0 failed (exit 0). Natural repro under a disk stall: one run (r2, before the fix) gave `flusher_errors=3` and 12 busy timeouts; the run with the fix and the dd stressor (d2) had no stall (max sync 0.6 s), so it did not exercise the fix; the 150 ms store wait runs do.
- fix_acceptance (per `debugger-fix-acceptance.md`):
  target_test: { result: pass }  # store_busy_tests (4) and retry_tests::a_put_that_waits_too_long (1); 20 runs in a row on the host: 0 failures
  mutation_check: { result: skipped, reason_if_skipped: "Stryker is for JS; no cargo-mutants is configured for this repo. Two manual mutants at the fix sites were run instead", mutant_killed: true }
    # mutant 1: `note_store_busy` removed at the stream site -> 3 of 4 CLI tests red (write 23 of 30 MiB errno 5). mutant 2: `is_store_busy` always false -> 4 of 4 CLI tests and the chunk-store test red. Third mutant: the flusher Busy arm disabled -> the flusher test red (`errors: 1`, `[FLUSH] ... failed (errno 5)`). The write-retry limit 0 (S7 d4) -> S7 red.
  no_op_deletion: { result: pass, deletion_justified_by_rca: false }  # the diff is additive; the only removals are `STORE_WAIT` uses replaced by `self.opts.store_wait` and the old single call in write_job
  adjacent_tests: { result: pass, suites_run: ["cargo test --workspace on macOS (1526 passed, 0 failed)", "cargo test --workspace in Docker Linux (1513 passed, 0 failed)", "S7 4 x 2048 MiB seed 1795492903707110156, flusher on, S7_STORE_WAIT_MS=150 (Docker, ok) and unset (Docker r1, ok, before the fix)", "final tree (commit d189550): cargo test --workspace macOS 1526 passed 0 failed; Docker Linux 1513 passed 0 failed; Docker Linux mount smoke (mt-linux.sh + dedup-probe.sh, 4 workers, the pool runs write_job: jobs=163) all ok, but with no busy store, so the retry inside write_job is not exercised by a mount"] }
  revert_and_reconfirm: { result: pass, bug_returned_on_revert: true, fixed_on_reapply: true }  # revert = write retry limit 0 inside the container (S7 d4: 4 EIO, 4 blocks off-model) and the flusher Busy arm off (flusher test red); reapply = d3 (0 mismatches) and the green tests
  guardrail_verdict: accepted
- files_changed: crates/chunk-store/src/lib.rs, crates/chunk-store/tests/retry_tests.rs, crates/slicefs-cli/src/filesystem.rs, crates/slicefs-cli/src/flusher.rs, crates/slicefs-cli/src/fuse_callbacks.rs, crates/slicefs-cli/src/handlers.rs, crates/slicefs-cli/src/pool.rs, crates/slicefs-cli/tests/common/mod.rs, crates/slicefs-cli/tests/stress_harness.rs, crates/slicefs-cli/tests/store_busy_tests.rs, scripts/docker-linux-test/run.sh (commits 76427b8 and d0c0bd0 on debug/170-eio)

## Closure (orchestrator, 2026-11-24)

- Merged into `feature/phase-21-worker-pool`; `cargo test --workspace` on macOS after the merge: 1527 passed, 0 failed.
- Accepted on test evidence (the 150 ms store-wait S7 run and the red/green tests); a real 10 s stall was not seen with the fix.
- Open follow-ups: fsync, release, replay and the commit still map a busy store to EIO; `prepare_manifest` keeps a failed put as `ObjectKey::ZERO` plus a deferred error (code reading only).
