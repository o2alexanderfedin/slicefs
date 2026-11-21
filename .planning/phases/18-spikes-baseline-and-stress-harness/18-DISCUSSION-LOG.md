# Phase 18: Spikes, baseline and stress harness - Discussion Log

> **Audit trail only.** Do not use as input to planning, research, or execution agents.
> Decisions are captured in CONTEXT.md — this log preserves the alternatives considered.

**Date:** 2026-11-20
**Phase:** 18-spikes-baseline-and-stress-harness
**Areas discussed:** Harness dependencies

---

## Harness dependencies

| Option | Description | Selected |
|--------|-------------|----------|
| hdrhistogram 7.6 | Dev-dependency only; exact percentiles; merges across threads | ✓ |
| Own log2 buckets | About 40 lines, no new crate; percentiles rounded to powers of two | |

**User's choice:** hdrhistogram 7.6

## Decided autonomously

[auto] [Harness] — Q: "Where does the harness run?" → Selected: "In process, against the Filesystem core API, as an integration test" (reversible technical — decided autonomously: test code only)
[auto] [Harness] — Q: "How does the harness pass on today's code?" → Selected: "Serialize mode on in the suite; serialize off is #[ignore] until Phases 19-21" (reversible technical — decided autonomously: a harness flag)
[auto] [Harness] — Q: "Short run and long run?" → Selected: "Short run in cargo test; long run #[ignore] with env vars" (reversible technical — decided autonomously: test settings)
[auto] [Harness] — Q: "How are the data checked?" → Selected: "Per-file and namespace model, seeded" (reversible technical — decided autonomously: test code)
[auto] [Lock stats] — Q: "How are lock times measured?" → Selected: "Timing wrappers behind cargo feature lock-stats, off by default" (reversible technical — decided autonomously: feature-gated)
[auto] [Spikes] — Q: "How are S3, S5, S7 run before a pool exists?" → Selected: "Harness threads call the core directly" (reversible technical — decided autonomously: measurement code)
[auto] [Spikes] — Q: "How are S2, S4, S6 run?" → Selected: "Linux Docker mount scripts" (reversible technical — decided autonomously: scripts)
[auto] [Spikes] — Q: "How is S1 run?" → Selected: "Standalone fuser test server with a 4-worker pool in a manual macOS CI workflow" (reversible technical — decided autonomously: test binary and CI file)
[auto] [Docs] — Q: "Where do results go?" → Selected: "Results section per spike in the research doc" (reversible technical — decided autonomously: docs)
