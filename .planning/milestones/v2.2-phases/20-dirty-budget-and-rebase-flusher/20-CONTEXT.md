# Phase 20: Dirty budget and rebase flusher - Context

**Gathered:** 2026-11-23
**Status:** Ready for planning

<domain>
## Phase Boundary

Phase 20 makes the shared dirty budget safe with N writers (issue #21). It covers LOCK-02 and LOCK-06.

- No request takes another file's state out of `open_files`. The dirty-budget path uses the per-cell dirty counters (Phase 19) and a background flusher. A test-suite check finds no take-out site, `keep_budget` included.
- A background flusher lowers the shared dirty total. A writer over the limit sleeps before it takes any lock (commit gate, stripe, cell). Every wait has a time limit.
- A test with all writers throttled while a commit waits finishes with no deadlock.
- A Docker run with parallel random writers on large files keeps peak RSS (VmHWM) under one fixed cap.

Out of scope: the worker pool (Phase 21), WAL group commit (deferred), chunk profiles (Phase 22).

</domain>

<decisions>
## Implementation Decisions

### When dirty data is written (user answer, 2026-11-23)
- The flusher writes (rebase steps to the chunk store) a file's dirty data when one of these is true:
  1. the file was idle (no write) for some time — default `--dirty-idle` 5 s;
  2. its oldest dirty data reached a maximum age — default `--dirty-expire` 30 s (the same as the default `--commit-interval`);
  3. the buffer is full — the shared dirty total is at or above the background threshold (half of `--dirty-budget`), or one file is at its own limit. The flusher then steps the largest dirty file first.
- A writer over the budget sleeps (with no lock held) until the flusher frees memory. After the wait limit (default 5 s), the writer does not fail: it writes, and pays for it by one unlocked step of its own file (the Phase 19 path). No new error for applications. Memory stays bounded by the per-file hard cap and the budget plus one step per writer.
- **Reversibility:** reversible — internal policy; the defaults can change.

### Mount options (user answer, 2026-11-23)
- Add mount options with default values: `--dirty-budget` (bytes, default 64 MiB = today's `DIRTY_BUDGET`), `--dirty-limit` (bytes per file, default 32 MiB = today's `DIRTY_LIMIT`), `--dirty-idle` (seconds, default 5), `--dirty-expire` (seconds, default 30). Accept size suffixes (K, M, G) for the byte options if a parser exists; otherwise plain bytes. Validate: `--dirty-limit` <= `--dirty-budget`; both at least a few rebase steps; reject 0.
- **Reversibility:** costly — public CLI options; later releases must keep them or deprecate them.

### Decided autonomously
- [Flusher] Q: "How many flusher threads?" → Selected: "One thread, spawned with the mount like the commit timer; it takes the commit gate shared for each step (never across a sleep) and steps one file per pass under the Phase 19 step token" — `.planning/research/ARCHITECTURE.md` A5; reversible.
- [Flusher] Q: "How does it pick a file?" → Selected: "Budget full: the largest dirty cell (scan of the cell map shards, or a small max-heap rebuilt per pass); idle/expire: cells whose last-write / first-dirty time passed the limit. Per-cell `last_write` and `dirty_since` timestamps (atomics)" — internal.
- [Throttle] Q: "Where does a writer wait?" → Selected: "In the write entry, before the commit gate, stripes and cell: if the shared total is over the budget, wait on a condvar that the flusher signals after each step, in slices, up to the wait limit; then write and step own file" — the research deadlock rule: never wait for budget while holding the gate.
- [Deadlock] Q: "Commit vs throttled writers?" → Selected: "A commit never waits for budget. The flusher releases the gate between steps, so an exclusive gate waiter (commit cut) gets it. Test: all writers throttled, a commit waiting; it ends within a time limit" — ROADMAP criterion 3.
- [Take-out] Q: "How is 'no take-out' checked?" → Selected: "Delete the cross-file path (the remains of `keep_budget` that step other files, if any) and add a test that greps the source for `open_files` removal outside release/destroy, or a structural test that no function takes another inode's cell while holding a cell" — criterion 1.
- [RSS] Q: "RSS cap value?" → Selected: "Fixed cap = budget + per-writer step + a measured fixed overhead (the planner sets it from a Phase 18/19 baseline run); the Docker test reads VmHWM from /proc/self/status" — criterion 4.
- [Waits] All new waits have a time limit and exit when the mount shuts down (unmount wakes the flusher and the throttled writers).

### Claude's Discretion
- Flusher pass interval (for example 100-500 ms when idle-scanning), background threshold ratio, exact condvar shape, option value parser.

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- Phase 19 `FileCell` with per-cell dirty cost and `SliceFsFilesystem::dirty_total` (`AtomicUsize`), `dirty_budget` / `dirty_limit` atomics (tests set them lower).
- Step token, unlocked rebase and stream steps, `rebase_cell`, `free_dirty`, hard cap (`limit + limit/GROW_DIV + 2*REBASE_STEP`).
- `keep_budget` (`crates/slicefs-cli/src/filesystem.rs`): today the writer steps only its own file inline; its doc says "Moving work to other files is Phase 20 (flusher)".
- `commit_timer.rs`: the pattern for a mount-time background thread with a clean shutdown.
- `cli.rs`: clap `#[arg(long, value_name = ..., default_value_t = ...)]` mount options.
- `crates/slicefs-cli/tests/stress_harness.rs` (model check, `long_run` env knobs), Docker scripts from Phase 18 (S7 budget spike).

### Integration Points
- Write entry in `handlers.rs` / `filesystem.rs` write path: the throttle goes before the gate.
- Mount setup: spawn and stop the flusher next to the commit timer.
- `docs/research/multithreaded-request-handling.md` S7 results.

</code_context>

<specifics>
## Specific Ideas

- Keep tests short. Never hold whole files in RAM.
- Do not grow `stress_harness.rs` without need; prefer a focused throttle test.

</specifics>

<deferred>
## Deferred Ideas

- Phase 19 deferred items stay open (see `.planning/phases/19-fine-grained-locks/deferred-items.md`): the locked re-lookup in chunk-store `put`, WAL group commit, closed-file size change races, cross-directory rename ancestor check, a kept cell after a failed release (the flusher may retry it in this phase if it is cheap).

</deferred>
</content>
</invoke>

## Answers to the research open questions (decided autonomously, 2026-11-23)
- [A10] "Buffer full" steps the largest *quiet* dirty file first (no write in the last few hundred ms). A file that is written all the time is not stepped by the flusher below the hard limits; its writer pays with its own step (Phase 19 path) when it is over its limit or over the budget after the wait. Reason: every overlay write moves the generation, so a flusher step on a busy file is always dropped. "Largest first" was an autonomous choice, not the user's rule; the user's three triggers (idle, max age, full) stay. Reversible.
- [A3] Minimum value 4 MiB for `--dirty-budget` and `--dirty-limit`. A later release can lower a minimum without breaking a user; raising it would break one, so the minimum starts low enough. Reversible in that direction.
- [A11] Run the P0 probe (serialize off, above the budget, Phase 19 code) in Wave 0 before the flusher work.
- [Harness] The default harness `run()` does not start the flusher; a focused flusher/throttle test does. A small opt-in knob in the harness is allowed only if a criterion needs it.

## Orchestrator correction after 20-01 (2026-11-23)
- [Docker image check] Docker here uses the containerd image store, so each `docker build` gives a new image ID even when every step comes from the cache. The IMAGE_ID check in 20-03, 20-07 and 20-08 is replaced: those plans may run `docker build` from the same Dockerfile, and must instead check that `rustc -V` inside the image equals the `RUSTC = ...` line in `runs/s7-wave0-NOTES.md`. A different `rustc -V` is a stop (report to the orchestrator). Record the new image ID in the log for reference only.
- [CAP_MIB] 320 MiB, derived by the stated rule from P_old max 260 and P0 max 267 (a `MALLOC_ARENA_MAX=2` run gave 230). Accepted. 20-07 asserts VmHWM (not the getrusage peak) against CAP_MIB.
