---
gsd_state_version: "1.0"
milestone: v2.2
milestone_name: Concurrency & Store Evolution
current_phase: 18
current_phase_name: Spikes, baseline and stress harness
status: executing
stopped_at: Completed 18-01-PLAN.md
last_updated: "2026-11-21T07:44:46.909Z"
last_activity: 2026-11-20
last_activity_desc: Phase 18 execution started
state_head: 52744c47835981881bef07ab55e3d1534b44d5a5
progress:
  total_phases: 7
  completed_phases: 0
  total_plans: 7
  completed_plans: 1
  percent: 0
---

# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-11-20)

**Core value:** High-ratio data deduplication that works transparently as a real, daily-driver POSIX filesystem
**Current focus:** Phase 18 — Spikes, baseline and stress harness

## Current Position

Phase: 18 (Spikes, baseline and stress harness) — EXECUTING
Plan: 2 of 7
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

Last session: 2026-11-21T07:44:46.873Z
Stopped at: Completed 18-01-PLAN.md
Resume file: None

## Operator Next Steps

- Plan the first phase with /gsd-plan-phase 18 (research-phase flagged: S5 gate fairness, S1 by hand)
- Phases 22-24 can start on separate branches in parallel with Phases 18-21
