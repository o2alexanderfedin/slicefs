# Phase 21: Worker pool - Discussion Log

> **Audit trail only.** Do not use as input to planning, research, or execution agents.
> Decisions are captured in CONTEXT.md — this log preserves the alternatives considered.

**Date:** 2026-11-23
**Phase:** 21-worker-pool
**Areas discussed:** Worker count option

---

## Worker count option

| Option | Description | Selected |
|--------|-------------|----------|
| --workers, default 4 | Fixed default on Linux; CI uses 4 | |
| --workers, default = CPU count | Scales with the machine (capped) | |
| --workers, default 1 | Opt-in parallelism | |
| Other | "--workers, default = CPU count / 2 or 1" | ✓ |

**User's choice:** `--workers`, default CPU count / 2, at least 1. macOS default stays 1 (roadmap).

## Decided autonomously

[auto] [Pool] — own pool behind one fuser reader (vendored fuser refuses n_threads != 1 off Linux)
[auto] [Pool] — bounded queue, reader blocks when full
[auto] [Order] — no global order; special requests (FORGET, INTERRUPT, DESTROY) checked in research
[auto] [Shutdown] — drain, join, then destroy(); timed waits
[auto] [Panic] — worker panic aborts the daemon; remount replays the log
[auto] [Locks] — reply after all locks are released; rank-checker assert at the reply site
[auto] [Tests] — Linux CI with --workers 4; macOS CI with the default
[auto] [Bench] — Docker Linux run at 1/2/4/8 workers; macOS CI run at 1 and 2
