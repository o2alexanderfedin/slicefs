---
gsd_state_version: "1.0"
milestone: v2.2
milestone_name: Concurrency & Store Evolution
current_phase: 18
current_phase_name: Spikes, baseline and stress harness
status: executing
stopped_at: Completed 18-06-PLAN.md
last_updated: "2026-11-21T09:10:21.692Z"
last_activity: 2026-11-20
last_activity_desc: Phase 18 execution started
state_head: 6f4290292dc7e2d97a9e439776936b0f44c05509
progress:
  total_phases: 7
  completed_phases: 0
  total_plans: 7
  completed_plans: 6
  percent: 0
---

# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-11-20)

**Core value:** High-ratio data deduplication that works transparently as a real, daily-driver POSIX filesystem
**Current focus:** Phase 18 — Spikes, baseline and stress harness

## Current Position

Phase: 18 (Spikes, baseline and stress harness) — EXECUTING
Plan: 7 of 7
Status: Ready to execute
Last activity: 2026-11-20 — Phase 18 execution started

Progress: [░░░░░░░░░░] 0%

## Performance Metrics

**Velocity:**
- Total plans completed: 0 (v2.2)

*Updated after each plan completion*
**Per-Plan Metrics:**

| Plan | Duration | Tasks | Files |
|------|----------|-------|-------|
| Phase 18 P01 | 25min | 2 tasks | 6 files |
| Phase 18 P04 | 45min | 2 tasks | 6 files |
| Phase 18 P02 | 35min | 2 tasks | 7 files |
| Phase 18 P03 | 45min | 3 tasks | 1 files |
| Phase 18 P05 | 60min | 3 tasks | 7 files |
| Phase 18 P06 | 75min | 3 tasks | 8 files |

## Accumulated Context

### Decisions

Full log: Key Decisions in .planning/PROJECT.md; per-milestone detail in .planning/milestones/.
Owner decisions for v2.2 (also in ROADMAP.md):

- [v2.2]: SHA-256 is a production second hash; BLAKE3-256 stays the default; rehash is offline only
- [v2.2]: A worker panic aborts the daemon; the write log replays at the next mount
- [v2.2]: Each log record is appended under the same locks as its apply (supersedes the #21 table)
- [v2.2]: Chunk profile is fixed at seed; the default stays 4/8/32 KiB
- [v2.2]: Prolly = prototype and written decision, no migration; macOS worker default is 1 until S1 is recorded
- [Phase 18]: Plan 01: lock-stats uses optional hdrhistogram in chunk-store (D-02); lock_stats::Guard is the std guard when the feature is off
- [Phase 18]: S1: FUSE-T 1.2.9 on macOS FSKit overlaps requests (max_inflight=2) and accepts out-of-order replies; macOS worker default may exceed 1 (Phase 21 to show throughput)
- [Phase 18]: 18-02: lock report test is its own binary (process-global recorder); read_root readers guard left as is for S3
- [Phase 18]: 18-03: short stress runs sized 4x150 ops (one-file) and 4x40 ops (many-files) to fit the 5 s budget; fsync and release cost 20-40 ms each in debug
- [Phase 18]: 18-03: serialize-off target fails today on the shared file (reads match no allowed version); many-files with one owner per file passed with serialize off
- [Phase 18]: 18-05: S4 fusectl mounts with SYS_ADMIN alone (no --privileged); S6/S4/S2 numbers saved in runs/ for Plan 07; S2 on overlayfs, 4 GiB max write 1353 ms in one of three runs
- [Phase 18]: S3 MB/s comes from a feature-off pass and lock rows from a feature-on pass; throughput counts time inside the wrapped calls only
- [Phase 18]: S7 runs a 30 s commit thread like the daemon and reports the 256 MiB cap without asserting it
- [Phase 18]: S3 safe subset (own file per thread, 4 MiB dirty limit, N x 4 MiB below the budget) ran clean with serialize off on macOS and Linux

### Pending Todos

None.

### Blockers/Concerns

- FUSE requests run on one thread; the shared dirty budget is safe only with one thread (#22, #127). Phases 19-21 remove this.
- SPIKE-03 (S1 on macOS FSKit) is a by-hand or macOS CI run; the macOS worker default stays 1 until it is recorded. It also gates POOL-04 in Phase 21.
- `keep_budget` is the last cross-file take-out of `open_files`; it stays until Phase 20 (LOCK-02). Phase 19 concurrency tests must stay below the dirty budget.
- Phase 20 (budget, flusher and gate) has the highest deadlock risk; run research-phase before planning it.
- Idle index merge can rewrite GBs after a short pause and is not stopped by new writes (#141). Not in v2.2.

## Deferred Items

| Category | Item | Status | Deferred At | Milestone |
|----------|------|--------|-------------|-----------|
| *(none)* | | | | |

## Session Continuity

Last session: 2026-11-21T09:10:21.652Z
Stopped at: Completed 18-06-PLAN.md
Resume file: None

## Operator Next Steps

- Plan the first phase with /gsd-plan-phase 18 (research-phase flagged: S5 gate fairness, S1 by hand)
- Phases 22-24 can start on separate branches in parallel with Phases 18-21
