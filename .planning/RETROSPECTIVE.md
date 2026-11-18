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

## Cross-Milestone Trends

| Milestone | Phases | Plans | Requirements superseded |
|-----------|--------|-------|-------------------------|
| v1.0 | 7 | 24 | - |
| v2.0 | 6 | 13 | 12/21 |
