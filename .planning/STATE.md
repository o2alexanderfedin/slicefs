---
gsd_state_version: "1.0"
milestone: v2.3
milestone_name: Prolly Metadata & Robustness
status: planning
last_updated: "2026-11-27T00:56:14.117Z"
last_activity: 2026-11-26
progress:
  total_phases: 0
  completed_phases: 0
  total_plans: 0
  completed_plans: 0
  percent: 0
---

# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-11-26)

**Core value:** High-ratio data deduplication that works transparently as a real, daily-driver POSIX filesystem
**Current focus:** Planning next milestone (v2.2 shipped; release tag `v2.3.0`)

## Current Position

Phase: Not started (defining requirements)
Plan: —
Status: Defining requirements
Last activity: 2026-11-26 — Milestone v2.3 started


## Performance Metrics

**Velocity:**
- Total plans completed: 69 (v2.2)

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
| Phase 18 P07 | 35 min | 2 tasks | 1 files |
| Phase 19 P01 | 60min | 3 tasks | 23 files |
| Phase 19 P02 | 52min | 3 tasks | 6 files |
| Phase 19 P03 | 50min | 3 tasks | 7 files |
| Phase 19 P04 | 45min | 3 tasks | 11 files |
| Phase 19 P05 | 41min | 3 tasks | 5 files |
| Phase 19 P06 | 25min | 2 tasks | 4 files |
| Phase 19 P07 | 95min | 3 tasks | 4 files |
| Phase 19 P08 | 45min | 3 tasks | 5 files |
| Phase 19 P09 | 55min | 3 tasks | 11 files |
| Phase 19 P10 | 55min | 3 tasks | 7 files |
| Phase 19 P11 | 52min | 3 tasks | 4 files |
| Phase 19 P12 | 70min | 3 tasks | 6 files |
| Phase 19 P13 | 59min | 3 tasks | 10 files |
| Phase 19 P14 | 320min | 2 tasks | 3 files |
| Phase 19 P15 | 85 min | 4 tasks | 4 files |
| Phase 19 P16 | 35 min | 3 tasks | 8 files |
| Phase 20 P01 | 60min | 3 tasks | 7 files |
| Phase 20 P02 | 50min | 2 tasks | 6 files |
| Phase 20 P03 | 45 min | 2 tasks | 7 files |
| Phase 20 P04 | 40 min | 2 tasks | 1 files |
| Phase 20 P05 | 39 min | 2 tasks | 3 files |
| Phase 20 P06 | 36 min | 2 tasks | 6 files |
| Phase 20 P07 | 55 min | 3 tasks | 4 files |
| Phase 20 P08 | 75min | 2 tasks | 7 files |
| Phase 21 P01 | 70 min | 3 tasks | 10 files |
| Phase 21 P02 | 55 min | 3 tasks | 5 files |
| Phase 21 P03 | 66 min | 2 tasks | 7 files |
| Phase 21 P04 | 46 min | 2 tasks | 3 files |
| Phase 21 P05 | 40 min | 2 tasks | 11 files |
| Phase 21 P06 | 45 min | 2 tasks | 2 files |
| Phase 21 P07 | 75 min | 3 tasks | 10 files |
| Phase 21 P08 | 50 min | 3 tasks | 12 files |
| Phase 21 P09 | 60 min | 2 tasks | 8 files |
| Phase 21.1 P01 | 74min | 3 tasks | 7 files |
| Phase 21.1 P02 | 57min | 3 tasks | 16 files |
| Phase 21.1 P03 | 50min | 3 tasks | 7 files |
| Phase 21.1 P04 | 45min | 2 tasks | 6 files |
| Phase 21.2 P01 | 11h (gates on a loaded host) | 3 tasks | 13 files |
| Phase 21.2 P02 | about 2 h | 2 tasks | 5 files |
| Phase 21.2 P03 | about 6 h | 3 tasks | 48 files |
| Phase 21.2 P04 | about 3 h | 2 tasks | 5 files |
| Phase 21.2 P05 | 3h35m | 3 tasks | 28 files |
| Phase 21.2 P06 | about 2 h | 2 tasks | 6 files |
| Phase 21.2 P07 | 25 min | 2 tasks | 6 files |
| Phase 21.2 P09 | about 50 min | 2 tasks | 3 files |
| Phase 21.2 P08 | about 2 h | 2 tasks | 7 files |
| Phase 21.2 P10 | about 40 min | 3 tasks | 12 files |

## Accumulated Context

### Decisions

Full log: Key Decisions in .planning/PROJECT.md; per-phase detail in .planning/milestones/v2.2-phases/. Cleared at the v2.2 close.

### Pending Todos

None.

### Blockers/Concerns

- Idle index merge can rewrite GBs after a short pause and is not stopped by new writes (#141).
- Writes do not scale past 2 workers (the single pack appender is the likely cause); see the Phase 21 deferred list.
- Closeout of v2.2 was `override_closeout`: 22 deferred items acknowledged (table below); Phases 18-23 read as `stale` in the GSD tool because later phases changed covered source files (all reports say `passed`).

### Roadmap Evolution

- Phase 21.1 inserted after Phase 21: Store header with compat, ro-compat and incompat feature flags, and a written format policy (#154); owner agreed #154 joins v2.2 after Phase 21
- Phase 21.2 inserted after Phase 21.1: Thread-safety verification and CI gates (user asked for formal verification and static analysis on every build) (URGENT)

## Deferred Items

Items acknowledged and deferred at milestone close, most recent first:

| Category | Item | Status | Deferred At | Milestone |
|----------|------|--------|-------------|-----------|
| deferred_items | 19/deferred-items.md: From 19-09 | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 19/deferred-items.md: From 19-10 | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 19/deferred-items.md: From 19-16 | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 19/deferred-items.md: From the 19-REVIEW fixes | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 19/deferred-items.md: Closed by the 19-REVIEW fixes | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 19/deferred-items.md: Closed by 19-12..19-15 | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 19/deferred-items.md: From the Phase 19 verification (2026-11-23) | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 20/deferred-items.md: Open | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 20/deferred-items.md: Decided (closed for Phase 20) | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 21/deferred-items.md: Open | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 21/deferred-items.md: Closed in Phase 21 | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 21.1/deferred-items.md: Open | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 21.2/deferred-items.md: 1. Flaky pool_panic_tests child start on a loaded Mac (found in 21.2-01, Task 1 gate) | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 21.2/deferred-items.md: 2. Follow-ups of the unsafe review (found in 21.2-09) | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 21.2/deferred-items.md: 3. The kill(pid) guard of unmount.rs (D-33, from 21.2-09) | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 21.2/deferred-items.md: 4. Owner question: the loom preemption bound (from 21.2-01 and 21.2-06) | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 21.2/deferred-items.md: 5. Race (a) uses the research variant of D-27 (from 21.2-04) | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 21.2/deferred-items.md: 6. Pitfall 10 lock wait (from 21.2-04) | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 21.2/deferred-items.md: 7. D-29 outcome of each race | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 21.2/deferred-items.md: 8. Owner step: required checks and the release gate (D-06) | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 21.2/deferred-items.md: 9. Owner choices on the time limits (D-03) | acknowledged | 2026-11-26 | v2.2 |
| deferred_items | 21.2/deferred-items.md: 10. Items of the CONTEXT Deferred list that stay deferred | acknowledged | 2026-11-26 | v2.2 |

## Session Continuity

Last session: 2026-11-26T17:19:43.607Z
Stopped at: Milestone v2.2 complete and archived
Resume file: None

## Operator Next Steps

- Start the next milestone with /gsd-new-milestone
