---
gsd_state_version: "1.0"
milestone: v2.3
milestone_name: Prolly Metadata & Robustness
current_phase: 25
current_phase_name: Prolly format and layout
status: executing
stopped_at: Phase 25 context gathered
last_updated: "2026-11-27T05:16:57.846Z"
last_activity: 2026-11-26
last_activity_desc: Phase 25 execution started
state_head: 2f06b3bc082ad513607207e43bfbffd3ed7a2f29
progress:
  total_phases: 6
  completed_phases: 0
  total_plans: 8
  completed_plans: 0
  percent: 0
---

# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-11-26)

**Core value:** High-ratio data deduplication that works transparently as a real, daily-driver POSIX filesystem
**Current focus:** Phase 25 — Prolly format and layout

## Current Position

Phase: 25 (Prolly format and layout) — EXECUTING
Plan: 1 of 8
Status: Executing Phase 25
Last activity: 2026-11-26 — Phase 25 execution started

Progress: [░░░░░░░░░░] 0%

## Performance Metrics

**Velocity:**
- Plans completed in v2.3: 0
- Plans completed in v2.2: 69 (per-plan rows are in the v2.2 archive)

*Updated after each plan completion*

## Accumulated Context

### Decisions

Full log: Key Decisions in .planning/PROJECT.md; per-phase detail in .planning/milestones/v2.2-phases/.
Owner-approved order of work for v2.3: prolly maps (#215), migration (#155), crash and soak tests (#157), scrub report (#159), limits page (#158). Serial track; each phase depends on the one before it.

### Pending Todos

None.

### Blockers/Concerns

- Phase 25: Q-2 is decided by a bench test of both layouts at the start of the phase (owner delegated the choice; see 25-CONTEXT.md D-02).
- Phase 26: the flat-map store code goes away. The "1/5 of the flat time" check (PMAP-07) needs a flat baseline; measure it with release `v2.3.0`.
- Phase 27: `rsync -aHAX` copies the live tree only. Snapshots of the old store are not carried over; `docs/user/upgrade.md` must say this.
- Phase 29: with prolly maps, a bad metadata node can damage many files or a whole snapshot. The scrub report must handle a bad node, not only a bad chunk.
- Idle index merge can rewrite GBs after a short pause and is not stopped by new writes (#141).
- Writes do not scale past 2 workers (the single pack appender is the likely cause); see the Phase 21 deferred list.
- Closeout of v2.2 was `override_closeout`: 22 deferred items acknowledged (table below); Phases 18-23 read as `stale` in the GSD tool because later phases changed covered source files (all reports say `passed`).

### Roadmap Evolution

- Phase 21.1 inserted after Phase 21: Store header with compat, ro-compat and incompat feature flags, and a written format policy (#154); owner agreed #154 joins v2.2 after Phase 21
- Phase 21.2 inserted after Phase 21.1: Thread-safety verification and CI gates (user asked for formal verification and static analysis on every build) (URGENT)
- v2.3 roadmap: #215 is split into two phases (25 format and layout; 26 maps in the paths, GC, crash replay and the benchmark) instead of three, so the milestone has 6 phases and the benchmark (PMAP-07) is not a single-requirement phase

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

Last session: 2026-11-27T04:07:09.154Z
Stopped at: Phase 25 context gathered
Resume file: .planning/phases/25-prolly-format-and-layout/25-CONTEXT.md

## Operator Next Steps

- Plan Phase 25 with /gsd-plan-phase 25
