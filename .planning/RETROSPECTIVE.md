# Retrospective

## Milestone: v2.0 — Streaming Writes & Hardening

**Shipped:** 2026-11-17
**Phases:** 6 (07.1, 8-12) | **Plans:** 13

### What Was Built
- Streaming writes for sequential writes, Buffered fallback for out-of-order writes.
- statfs and snapshot lookup fixes; signal handling with a soft unmount and a final commit.
- After the phases: FastCDC + BLAKE3 chunk store, FSKit-only macOS backend, reachability and online GC, write log with replay (#20).

### What Worked
- Audits that read the code across phases found real bugs the phase verifications missed (FUSET-05 dead signal code; the unsynced re-log at a commit).
- Tests that simulate a crash in-process (`mem::forget`) caught replay bugs fast and with no mount.

### What Was Inefficient
- 12 of 21 requirements were superseded by later design changes (blockset removal, FSKit only, per-object zstd). The phase plans aged fast.
- The VERIFICATION files predate most of the later PRs, so they say little about the code that shipped.
- Mounts on macOS in tests can hang the kernel; live checks need Docker on Linux or a hand run.

### Patterns Established
- Merge on CI green; the Boost mount run is by hand or on main.
- A crash test must cover each write-log mode: the default `per-op` mode hid a bug that only `flush-on-fsync` shows.

### Key Lessons
- When a commit deletes log segments and logs records again, sync them before the commit returns.
- In-process crash tests that drop a writer flush its buffers; use `mem::forget` and a buffered log mode to see lost records.

---

## Milestone: v2.1 — Store Index & Measurements

**Shipped:** 2026-11-20
**Phases:** 5 (13-17) | **Plans:** 27

### What Was Built
- On-disk chunk index (#85): sorted runs, MANIFEST, Bloom filters, mapped lookups, background merge, GC by tombstones. RAM per object 106-196 B -> 2.2-3.3 B; 100M-object open 141 s -> 0.49 s.
- Bounded-memory random writes (#98, #93): per-file dirty limit and shared budget; 4 GiB file at VmHWM 165 MiB.
- Measurements: pack files (#8) and index baseline (#66); write log close-out (#20) with crash tests per window and per WAL mode.

### What Worked
- A benchmark before the build (Phase 16) gave fixed targets for Phase 17 and settled custom runs against fjall with numbers.
- Mutation checks on new crash tests (break the Cut-link check, see every case fail) proved the tests see the bug.
- Docker bigwrite with a VmHWM cap turned the "no whole file in RAM" rule into a pass/fail check.

### What Was Inefficient
- Phase verifications went stale as later PRs touched covered files; all 5 had to be re-run at close.
- One slow-write debug session spent many runs on host stalls (page-cache drops, fsync noise) that single runs cannot rank.
- Plan wording (column lists, "manual run" notes) aged as later work improved the scripts.

### Patterns Established
- macOS FSKit mount checks run in CI (workflow_dispatch), never on the local Mac.
- Close-out: re-run verifiers and the open-artifact audit before the milestone close.

### Key Lessons
- Keep a deferred item's status in its file current when a later commit fixes it.
- Single timing runs on a shared VM host are noise for maxima; compare medians or counters.

---

## Milestone: v2.2 — Concurrency & Store Evolution

**Shipped:** 2026-11-26 (release `v2.3.0`)
**Phases:** 9 (18-21, 21.1, 21.2, 22-24) | **Plans:** 69

### What Was Built
- Multithreaded requests: fine-grained locks with a rank checker (#21), a dirty budget with a background flusher, and a worker pool (`--workers`, #22). pjdfstest passes with 4 workers.
- Store header format 7 with feature flags and a format policy (#154).
- Safety gates in the release gate: clippy, Miri, TSan, loom, Kani, deadlock stress, coverage. The two Phase 19 races are fixed.
- Per-store FastCDC profile with a benchmark (#67); SHA-256 and an offline, crash-safe `slicefs rehash` (#62); prolly-tree prototype and decision "later" (#19).

### What Worked
- Spikes first (Phase 18): S1-S7 decided each lock step with numbers, and the stress harness was the "done" test for Phases 19-21.
- Lock-hold bars set before the runs (p99 and max per site) showed exactly which locks still did I/O; four follow-up plans (19-12..19-15) then met the bars.
- The concurrency track (18-21) and the store track (22-24) ran in parallel with no conflicts, because they edit different files.
- Red-first tests for each race and each refusal proved that the tests see the bug.

### What Was Inefficient
- Phase 19 grew from 11 to 16 plans: the first measurements showed locked I/O that the plans did not expect.
- Tests that start a child process or depend on time were flaky on a loaded Mac (60 s child start, idle-merge timing). Many gate runs were repeated.
- Two phases (21.1, 21.2) were inserted during the milestone; the requirement count rose to 37.
- The GSD tool reads Phases 18-23 as stale at close, because later phases changed covered source files.
- The deferred-items scanner read the rows of a table as items; the table had to become plain text before the acknowledge.

### Patterns Established
- A new store setting line always comes with a feature flag; old releases refuse or mount read-only.
- Heavy safety checks run in the release gate (release.yml) and by hand, not on each PR.
- I/O runs in unlocked steps under a per-file step token; a step publishes only if the generation did not change.
- Benchmarks of format options (profiles, prolly) run by hand on Boost trees; the default changes only by an owner decision.

### Key Lessons
- Measure lock holds against a stated bar before closing a lock phase; code review alone missed the locked fsync and pwrite.
- Give each test that starts a child process a limit that covers a slow start on a loaded host, or make the child a small binary.
- Write scaling needs its own measurement (the single pack appender); read scaling does not show it.
- Keep deferred-item text in bullets, not tables, so the audit tools can read it.

---

## Cross-Milestone Trends

| Milestone | Phases | Plans | Requirements superseded |
|-----------|--------|-------|-------------------------|
| v1.0 | 7 | 24 | - |
| v2.0 | 6 | 13 | 12/21 |
| v2.1 | 5 | 27 | 0/19 |
| v2.2 | 9 | 69 | 0/37 |
