---
phase: 19-fine-grained-locks
plan: 16
subsystem: concurrency
status: complete
tags: [lock-01, lock-04, lock-05, lock-stats, measurement, gap-closure, g6]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-12 store sync and record write with no store lock; 19-13 WAL record writes with no WAL lock and mkdir stripes; 19-14 step token, unlocked retries, hard-cap gate; 19-15 unlocked finalize, size changes under the token"
provides:
  - "runs/g6-macos-lockstats.log and runs/g6-linux-lockstats.log: 3 serialize-off lock-stats runs each, with lock_wait_rows and lock_stats_report"
  - "Updated ## Phase 19 result in docs/research/multithreaded-request-handling.md (before/after hold table, what still does I/O under a lock)"
  - "deferred-items.md with an Open part and a Closed by 19-12..19-15 part"
  - "runs/issue-21-body.md: the #21 body for the orchestrator to apply"
affects: [20]

estimate:
  tokens: 70000
  tasks: 3
actuals:
  tokens: 280400    # chars/4 over the added lines of the diff; the four logs are about 275000 of it, the text and code about 4700
  tasks: 3
  commits: 5

plan_head_before: 5820dd30ef6c9b51d68a1f297031379985ec386c
plan_head_after: f37913e500207ac7f2a48d7eb9bc36d1b2f22b64
commits: 5

tech-stack:
  added: []
  patterns:
    - "Measurement logs are written to a scratch file and copied into runs/ after the run, so the tree stays clean while the Docker tar stream reads it"

key-files:
  created:
    - .planning/phases/19-fine-grained-locks/runs/g6-macos-lockstats.log
    - .planning/phases/19-fine-grained-locks/runs/g6-linux-lockstats.log
    - .planning/phases/19-fine-grained-locks/runs/g6-macos-probe.log
    - .planning/phases/19-fine-grained-locks/runs/issue-21-body.md
    - .planning/phases/19-fine-grained-locks/19-16-SUMMARY.md
  modified:
    - crates/slicefs-cli/tests/lock_wait_rows.rs
    - docs/research/multithreaded-request-handling.md
    - .planning/phases/19-fine-grained-locks/deferred-items.md
    - .planning/REQUIREMENTS.md

key-decisions:
  - "LOCK-01 is complete: on macOS and Docker Linux, 54 harness rows of the six sites each, 0 rows above the stated bar; no outlier attribution was needed"
  - "LOCK-05 is complete: lock_wait_rows (4 tests, including the 19-15 forced-drop tests) passed on both systems, and the 19-14/19-15 tests passed in the phase gate"
  - "LOCK-04 stays [ ]: its code part is complete; the orchestrator applies the #21 body and then marks it"
  - "Finding for the owner: unlink_logged holds its two namespace stripes about 20 ms on macOS while reclaim_inode reads the manifest from the store (no global lock held); stripes are not a LOCK-01 site, so LOCK-01 stays marked, and the fix is an owner decision"

requirements-completed: [LOCK-01, LOCK-05]

duration: about 35 min (machine load average 15-33)
completed: 2026-11-22
---

# Phase 19 Plan 16: Measure, document and close Summary

**After 19-12..19-15, the store lock, `readers`, `wal_queue`, `wal_writer`, `dir_live` and `inode_live` hold for at most 1776 us on macOS and at most 1967 us on Docker Linux in 3 serialize-off runs of 3 modes. No row is above the bar (p99 500 us; max 5 ms macOS, 2 ms Linux). Before, the store lock held for 21-91 ms on macOS and up to 144 ms on Linux. One finding outside the six sites: on macOS, an unlink holds its two namespace stripes for about 20 ms while `reclaim_inode` reads the manifest from the store (each of the 3 many-files runs; no global lock is held then).**

## Performance

- **Duration:** about 35 min, from about 22:49Z to 23:25Z on 2026-11-22.
- **Load:** the host load average was 15-33 during the plan (15-17 during the measurement runs, up to 32.7 at the end of the phase gate). It is next to each measurement below.
- **Tasks:** 3. **Files:** 4 created in runs/, 4 modified.

## Accomplishments

- **Task 1 (macOS):** 3 harness runs, `lock_wait_rows` and `lock_stats_report` in `runs/g6-macos-lockstats.log`. The threshold check prints no `OVER` line.
- **Task 2 (Docker Linux):** the same in `runs/g6-linux-lockstats.log`. The committed tree was streamed into the container with tar (no bind mount), with the fresh target volume `slicefs-target-g6`. The threshold check prints no `OVER` line.
- **Task 3:** the research doc, `deferred-items.md`, the #21 body copy and REQUIREMENTS.md were brought in line with the code. The phase gate is green.
- **Orchestrator note 3:** the stale doc comment of `held_hook` in `lock_wait_rows.rs` is fixed. Later rebase steps of the write or of the finalize also run with the cell unlocked.

## Task Commits

1. **Orchestrator note 3: held_hook comment** - `74561f6`
2. **Task 1: macOS lock-stats runs** - `050bf7e`
3. **Task 2: Docker Linux lock-stats runs** - `4ff502e`
4. **Task 3: research doc, deferred items, #21 body copy, requirement marks** - `c6d33fb`
5. **Task 3 follow-up: the stripe finding and the probe log** - `f37913e`

## Measurement conditions

| System | Where | Build | Load average (host) | Commit measured |
|---|---|---|---|---|
| macOS 26.2 (25C56), rustc 1.95.0-nightly | in-process, no mount | debug | 15.0-15.9 (before and after each run) | `74561f6` |
| Linux 7.2.12-orbstack x86_64, rustc 1.100.0, `/tmp` on overlay | OrbStack Docker, tar stream | debug | host 16.5-17.4; container 1.0-1.8 | `050bf7e` (only a log differs from `74561f6`) |

Each run is `serialize_off_target`: one-file and many-files 2 s each, multi-handle 1 s, 4 threads, serialize off, PerOp log, commit every 250 ms.

## Threshold check

| Check | macOS | Linux |
|---|---|---|
| `grep -c '^=== run'` | 3 | 3 |
| awk check over the whole log | `rows=78 over=0` | `rows=78 over=0` |
| rows of the six sites in the 3 harness runs only | 54 | 54 |
| `test result: ok` lines | 5 (3 harness, lock_wait_rows 4 passed, lock_stats_report) | 5 |
| `test result: FAILED` | 0 | 0 |
| `lock site=store` lines | 13 | 13 |
| `Linux ... x86_64` line | n/a | 1 |

- The awk check counts 78 rows, not 54, because `lock_wait_rows` also prints `lock site=` rows (with no p99). They go to the last mode in the awk, but none is above the bar, so they do not change the result. A second awk that stops at the `=== lock_wait_rows` line counts 54 rows and 0 over.
- No row was over the bar, so the outlier rule did not start for the six sites. An extra probe ran for the stripe and cell holds (see "Residuals"). `runs/g6-linux-probe.log` was not created.
- The closest value to a bar is the Linux store hold max of 1967.1 us (many-files, run 2) against 2000 us. The many-files store max per run was 980.5, 1967.1 and 1196.0 us. It is under the bar, so the rule does not apply. It is listed here so the verifier sees it.
- The other sites (not in the check): `io` (the metadata `StoreIo` lock) has 6-11 samples per mode and hold max 0.3-0.6 ms (macOS) and 0.2-1.0 ms (Linux); with so few samples its p99 is its max. `wal`, `handles` and `cell_map` hold max 0.04-1.5 ms.
- `git status --porcelain -- crates/` printed nothing after Tasks 1 and 2.

## Lock rows, macOS

Run 1, plus the hold p99 / hold max and the wait max over all 3 runs. Times in microseconds.

| Mode | Site | n | wait p50 / p99 / max | hold p50 / p99 / max | all runs: hold p99 / hold max | all runs: wait max |
|---|---|---|---|---|---|---|
| one-file | store | 3972 | 0.0 / 0.0 / 40.1 | 1.7 / 6.8 / 66.5 | 7.7 / 80.7 | 40.1 |
| one-file | readers | 1250 | 0.0 / 0.0 / 0.0 | 0.7 / 2.2 / 32.4 | 2.6 / 32.4 | 0.0 |
| one-file | wal_queue | 1851 | 0.0 / 0.0 / 0.8 | 0.7 / 10.0 / 32.7 | 10.0 / 35.1 | 0.8 |
| one-file | wal_writer | 3320 | 0.0 / 0.5 / 67.5 | 1.5 / 18.3 / 50.2 | 20.4 / 50.2 | 67.5 |
| one-file | dir_live | 227 | 0.0 / 0.0 / 0.0 | 0.6 / 7.1 / 16.3 | 8.0 / 16.3 | 0.0 |
| one-file | inode_live | 248 | 0.0 / 0.0 / 0.0 | 0.6 / 8.7 / 28.1 | 8.9 / 47.5 | 0.0 |
| one-file | file_cell | 2442 | 0.0 / 1200.1 / 3549.2 | 9.6 / 688.6 / 3567.6 | 907.3 / 7856.1 | 6254.6 |
| one-file | stripe | 12 | 0.0 / 0.0 / 0.0 | 13.2 / 323.6 / 323.6 | 498.2 / 498.2 | 0.0 |
| many-files | store | 11324 | 0.0 / 32.0 / 117.4 | 2.3 / 25.9 / 122.2 | 25.9 / 122.2 | 174.1 |
| many-files | readers | 560 | 0.0 / 0.0 / 11.9 | 1.3 / 11.0 / 48.8 | 14.5 / 60.6 | 11.9 |
| many-files | wal_queue | 3074 | 0.0 / 0.0 / 2.4 | 0.5 / 2.1 / 18.1 | 2.2 / 21.4 | 2.4 |
| many-files | wal_writer | 4942 | 0.0 / 0.9 / 22.6 | 1.2 / 16.8 / 53.2 | 17.2 / 77.3 | 57.4 |
| many-files | dir_live | 2323 | 0.0 / 0.5 / 39.1 | 3.1 / 17.6 / 32.6 | 20.6 / 51.1 | 39.1 |
| many-files | inode_live | 4039 | 0.0 / 0.0 / 3.8 | 1.8 / 9.9 / 99.4 | 10.0 / 99.4 | 20.8 |
| many-files | file_cell | 3889 | 0.0 / 0.0 / 0.0 | 3.7 / 96.3 / 356.1 | 96.3 / 356.1 | 0.0 |
| many-files | stripe | 1638 | 0.0 / 16.4 / 122.1 | 28.6 / 165.8 / 21545.0 | 213.2 / 21545.0 | 122.1 |
| multi-handle | store | 3096 | 0.0 / 0.0 / 0.0 | 1.9 / 5.6 / 62.1 | 8.8 / 262.4 | 4.7 |
| multi-handle | readers | 1292 | 0.0 / 0.0 / 2.8 | 0.7 / 2.6 / 5.3 | 3.1 / 25.6 | 2.8 |
| multi-handle | wal_queue | 1108 | 0.0 / 0.0 / 0.0 | 0.5 / 2.1 / 24.8 | 2.1 / 26.7 | 1.1 |
| multi-handle | wal_writer | 1933 | 0.0 / 2.6 / 45.9 | 1.1 / 19.3 / 55.6 | 20.9 / 1775.6 | 5124.1 |
| multi-handle | dir_live | 131 | 0.0 / 0.0 / 0.0 | 0.6 / 9.0 / 12.3 | 10.2 / 26.5 | 0.0 |
| multi-handle | inode_live | 159 | 0.0 / 0.0 / 0.0 | 0.6 / 8.1 / 13.9 | 14.0 / 16.1 | 0.0 |
| multi-handle | file_cell | 1015 | 0.0 / 88.1 / 134.4 | 6.0 / 75.1 / 398.1 | 94.0 / 1200.1 | 10747.9 |
| multi-handle | stripe | 28 | 0.0 / 1.0 / 1.0 | 5.8 / 254.5 / 254.5 | 254.5 / 254.5 | 3.6 |

**Before (19-11 run and spike 19g, macOS):** store hold p99 22-158 us, hold max 21-91 ms (5 runs); `wal_writer` hold p99 20.6-36.4 ms, hold max 37-43 ms; `file_cell` hold max up to 37 ms; `stripe` hold max up to 37 ms.

## Lock rows, Docker Linux

| Mode | Site | n | wait p50 / p99 / max | hold p50 / p99 / max | all runs: hold p99 / hold max | all runs: wait max |
|---|---|---|---|---|---|---|
| one-file | store | 4051 | 0.0 / 0.0 / 0.0 | 1.8 / 13.3 / 104.8 | 14.1 / 272.4 | 610.3 |
| one-file | readers | 1266 | 0.0 / 0.0 / 0.0 | 1.0 / 3.2 / 43.7 | 3.9 / 43.7 | 0.6 |
| one-file | wal_queue | 9000 | 0.0 / 0.0 / 0.6 | 0.8 / 2.9 / 44.4 | 2.9 / 88.8 | 1.4 |
| one-file | wal_writer | 15100 | 0.0 / 2.6 / 1093.6 | 2.2 / 26.1 / 153.1 | 26.1 / 170.2 | 2463.7 |
| one-file | dir_live | 291 | 0.0 / 0.0 / 0.0 | 0.7 / 10.8 / 58.0 | 13.6 / 58.0 | 0.0 |
| one-file | inode_live | 314 | 0.0 / 0.0 / 0.0 | 0.6 / 15.5 / 66.6 | 15.5 / 66.6 | 0.0 |
| one-file | file_cell | 11958 | 0.0 / 454.1 / 7311.4 | 10.8 / 288.5 / 7295.0 | 296.7 / 13017.1 | 12853.2 |
| one-file | stripe | 12 | 0.0 / 562.7 / 562.7 | 21.8 / 826.4 / 826.4 | 826.4 / 826.4 | 562.7 |
| many-files | store | 54379 | 0.0 / 117.8 / 2273.3 | 3.3 / 46.5 / 980.5 | 47.9 / 1967.1 | 2928.6 |
| many-files | readers | 2958 | 0.0 / 0.0 / 145.5 | 2.0 / 17.7 / 187.3 | 25.1 / 242.0 | 877.1 |
| many-files | wal_queue | 16266 | 0.0 / 0.0 / 238.7 | 0.6 / 2.1 / 216.2 | 2.3 / 216.2 | 238.7 |
| many-files | wal_writer | 25494 | 0.0 / 1.9 / 1474.6 | 1.7 / 24.2 / 255.4 | 25.3 / 1019.4 | 1474.6 |
| many-files | dir_live | 13062 | 0.0 / 0.0 / 513.0 | 4.3 / 35.6 / 873.5 | 35.6 / 873.5 | 1029.6 |
| many-files | inode_live | 21013 | 0.0 / 0.0 / 21.3 | 2.3 / 16.3 / 155.3 | 16.8 / 163.6 | 70.7 |
| many-files | file_cell | 19880 | 0.0 / 0.0 / 0.0 | 5.1 / 120.4 / 416.8 | 125.7 / 534.5 | 0.0 |
| many-files | stripe | 8923 | 0.0 / 0.0 / 1494.0 | 38.0 / 286.0 / 1539.1 | 286.0 / 2560.0 | 1870.8 |
| multi-handle | store | 4204 | 0.0 / 0.0 / 108.2 | 1.9 / 15.3 / 168.1 | 15.3 / 170.6 | 108.2 |
| multi-handle | readers | 5423 | 0.0 / 0.0 / 358.7 | 1.0 / 5.5 / 135.2 | 5.5 / 135.2 | 549.9 |
| multi-handle | wal_queue | 9742 | 0.0 / 0.0 / 1.8 | 0.6 / 2.1 / 48.2 | 2.2 / 203.6 | 1.8 |
| multi-handle | wal_writer | 15631 | 0.0 / 2.8 / 1394.7 | 1.9 / 21.9 / 182.8 | 26.3 / 539.1 | 1974.3 |
| multi-handle | dir_live | 195 | 0.0 / 0.0 / 0.0 | 0.5 / 13.3 / 34.3 | 13.3 / 108.0 | 0.0 |
| multi-handle | inode_live | 224 | 0.0 / 0.0 / 0.0 | 0.6 / 13.2 / 87.2 | 15.4 / 87.2 | 5.9 |
| multi-handle | file_cell | 12299 | 0.0 / 562.2 / 4120.6 | 11.0 / 214.4 / 3508.2 | 217.0 / 3508.2 | 4120.6 |
| multi-handle | stripe | 28 | 0.0 / 3.7 / 3.7 | 8.7 / 273.2 / 273.2 | 273.2 / 273.2 | 6.1 |

**Before (spike 19g, 3 Linux runs):** store hold p99 29-67 us, hold max 3.4-50 ms and one 144 ms; `wal_writer` hold p99 0.26-1.5 ms, hold max 9-49 ms; `file_cell` hold max up to 22 ms; `stripe` hold max up to 47 ms.

OrbStack uses a virtual disk. Its fsync does not flush a drive cache as a real disk does (spike 19g caveat), so the Linux numbers can be lower than on real hardware.

## Residuals

- **Six sites (LOCK-01):** none. No row was above the bar, on either system.
- **Outside the six sites: `stripe` (finding for the owner).**
  - Measured: on macOS, the many-files mode had one stripe hold of about 20 ms in each of the 3 runs (21.5, 20.2 and 20.2 ms). On Linux the stripe max was 1.1-2.6 ms.
  - Extra probe (`runs/g6-macos-probe.log`, 4 local runs, patches reverted):
    - Probe 1 printed a backtrace when a stripe or cell guard with a hold of 3 ms or more was dropped. The stripe holds of 20-63 ms are all the `NsGuard` of `unlink_logged`. They come in pairs (two stripes); the second of a pair includes the print time of the first.
    - Probe 2 timed the steps of `unlink_logged`. Of 7 slow calls, in 6 the first store read of the manifest of the inode (`get_manifest`, `load_manifest` through the store reader, as in `reclaim_inode`) took 20.0-21.6 ms. In 1 call that read was fast and the reclaim took the 20 ms. The other steps took microseconds.
  - So an unlink of the last link holds the parent and child stripes during a store read of the manifest. No global lock is held then: in the same mode the store hold max is 122 us and the `io` hold max 0.5 ms.
  - Not measured: why that read takes about 20 ms. It is the same time as an F_FULLFSYNC on the volume, but this is not shown.
  - This is not a LOCK-01 site (stripes are not a global lock), so LOCK-01 stays marked. It is against the #21 rule "never hold a lock during disk I/O". A fix (reclaim after the stripes are released, or read the manifest before they are taken) is an owner decision (Rule 4). It is in `deferred-items.md`, the research doc and the #21 body copy.
- **Outside the six sites: `file_cell`.**
  - On macOS the one-file mode had holds of 3.6, 6.6 and 7.9 ms (one per run). On Linux one run had a hold of 13.0 ms. The waits were up to 10.7 ms (macOS multi-handle) and 12.9 ms (Linux one-file).
  - The probe caught only holds of 3.2-4.8 ms, released in `write_in_cell`. That names the function, not the span. The 6.6-13 ms holds are not attributed.
  - The `lock_wait_rows` tests show that no writer I/O is in a cell hold on the paths they park (readers wait at most 1.3 us there).
  - It is in `deferred-items.md` ("From 19-16") for Phase 20.

## Paths that still do I/O under a lock

- **Off the read and write paths:** the GC copy loop and its syncs, `gc_begin`, `close`, the offline log `rotate`, destroy (`finalize_locked`), and the commit cut (`flush_cell(FlushIo::Locked)` under the exclusive gate, when no request runs).
- **On a request path:**
  - The size change of a closed file does its splice under its own transient cell. Reads and getattr do not lock that cell. An open or a second size change of the same inode waits for it.
  - An unlink of the last link runs `reclaim_inode` (a store read of the manifest, refcount drops, the inode delete) with the namespace stripes held. See "Residuals".
- **Fallbacks:** a pack whose writer or sync handle cannot be cloned (`try_clone` fails) keeps the locked append (`append_locked`) or the locked sync (`sync_locked_fallback`).
- **A page-cache read:** the re-lookup in `locked_lookup` under the store lock can read a run page (from 19-02). It did not show in the hold maxima above.

## Docs and the #21 body

- **Research doc, `## Phase 19 result`:**
  - "Copy, I/O, publish" now has the step token, unlocked retries, the hard-cap gate, the unlocked finalize and size change, and reads that compose pending writes.
  - "Lock tails" now says that the store and the WAL write records with no lock (one write in flight at the end of the pack or segment), and that the phase-2, index flush and directory fsyncs run with no store lock.
  - The decision "a page-cache record write stays under the store lock" is replaced by "no record write under the store or WAL lock".
  - Criterion 1 names the forced-drop tests. Criterion 3 has the before/after hold table of the six sites on both systems and the bar.
  - "Still open" no longer has the store-lock and locked-fallback findings. It lists the I/O that still runs under a lock (now with the unlink reclaim), and the stripe and cell holds with the probe result. The mkdir window is closed.
- **deferred-items.md:** an "Open" part (closed-file size change against unlink, against a fields setattr, the rename ancestor check, the unlink reclaim under the stripes, the file_cell holds, WAL group commit, the Phase 20 flusher and throttle) and a "Closed by 19-12..19-15" part (the store lock, the WAL writer lock, the locked fallbacks, the mkdir window, each with its plan).
- **#21 body copy (`runs/issue-21-body.md`):** copied from the scratch draft (5600 bytes, not changed). Changes in the copy (7450 bytes):
  1. Design table, "Log segments": a record write runs with no log lock, one write in flight at the end of the segment.
  2. Design table, "Chunk store": written by rebase steps and at commit; a short store lock covers memory only; a record write runs with no lock, one write in flight at the end of the active pack; fsync runs with no lock. (The draft said "no lock".)
  3. Writer step 2: "with no lock held" is added.
  4. The rebase paragraph ("drops the result and tries again later") is replaced by a list: copy, I/O, publish; one step token per file, writers wait for it with the lock released, readers never; a dropped step retries with no lock, stream steps EIO after a bounded number, a dropped rebase step leaves the work to the next write or the release; the hard cap (wait or own unlocked step before the apply, each step frees at most one step size, the writer steps again until it is under the cap); the last release and the size change of an open file use the same unlocked steps.
  5. Reader step 1 copies the stream state and the parts of pending writes in the range; step 2 composes pending writes on top in memory.
  6. Lock order item 3: create, mknod, symlink and mkdir take the new inode number first and lock the stripes of the parent and of the new inode before the name is linked.
  7. The fsync paragraph: "the committer ... holds no inode locks" now says that it locks a file cell only in the commit cut, under the exclusive gate.
  8. A new paragraph after it: GC, the commit cut (exclusive gate, when no request runs) and offline rotate still do I/O under a lock, off the read and write paths. On a request path, a closed-file size change does its I/O under its own transient cell (reads do not wait for it), and an unlink of the last link reclaims the inode with the namespace stripes held.
  - Checks on the copy: `same locks` 2; `the slow part, no inode lock` 0; forbidden words and home paths 0.
  - Issue #21 itself was not edited. No `gh` command that changes GitHub was run.

## Requirements

- **LOCK-01: marked complete.** Both threshold checks passed with no `OVER` line (54 harness rows each), so the outlier rule did not need to start. The other global sites (`io`, `wal`, `handles`, `cell_map`) have no hold near 20 ms either. The 20 ms stripe hold of unlink is not a LOCK-01 site; it is handed to the owner (see "Residuals").
- **LOCK-05: marked complete.** `lock_wait_rows` (4 tests, including `readers_do_not_wait_when_steps_drop_rows` and `release_with_drops_does_not_block_readers_rows`) passed on macOS and in Docker Linux. The 19-14 and 19-15 tests (`lock_free_io.rs` and the others) passed in the phase gate.
- **LOCK-04: not marked (code part complete).** The checkbox stays `[ ]`. The traceability row says "Pending (code part complete; the orchestrator applies the #21 text)". The orchestrator applies the body with `gh issue edit 21 --body-file .planning/phases/19-fine-grained-locks/runs/issue-21-body.md`, then checks the live body (`gh issue view 21 --json body -q .body`: "same locks" at least 1; home paths and forbidden words 0), then marks LOCK-04.
- The plan frontmatter lists LOCK-04, but it was not passed to the requirement tool, as the plan says.

## Phase gate

Run on the tree of `c6d33fb` (its `crates/` is the same as `74561f6`, and the follow-up commit changed only docs and a log). Load average 18.2 at the start, 32.7 at the end. Each step exited 0:

- `cargo test --workspace`
- `cargo check --workspace --all-targets`
- `cargo check --workspace --all-targets --features slicefs-cli/lock-stats`
- `cargo check -p slicefs-cli --all-targets --features deadlock-detect`
- `cargo test -p slicefs-cli --features deadlock-detect --test stress_harness serialize_off_target`
- `cargo test -p slicefs-cli --features lock-stats --test lock_wait_rows --test lock_stats_report`

The log has 70 `test result: ok` lines (1453 tests passed), 0 `test result: FAILED`, and 0 lines with "STRESS deadlock detected" or "lock rank violation".

Task 3 grep gates on the final tree:
- Research doc: one `## Phase 19 result`; home paths and forbidden words 0.
- #21 body copy: not empty; `same locks` 2; forbidden words and home paths 0; `the slow part, no inode lock` 0.
- `Closed by 19-12` in `deferred-items.md`: 1.
- `[ ] **LOCK-04**` in REQUIREMENTS.md: 1.

## Deviations from Plan

### Plan facts

- **Logs via the scratchpad.** Each run wrote its log to a scratch file, which was copied into `runs/` after the run. A log file in the tree during the Docker run would be in the tar stream.
- **A warm build ran first.** The macOS script and the container both ran `cargo test --no-run` for the three test binaries before `=== run 1`. Before the macOS run, a Docker build-only container warmed the volume `slicefs-target-g6`; it ran at the same time as a workspace test and measured nothing.
- **Load lines in the logs.** The logs have `load before` / `load after` lines (host `uptime`; in Docker also `/proc/loadavg`). The checks ignore them.
- **An extra probe that the plan did not ask for.** No row of the six sites was over the bar, so the outlier rule did not start. But the stripe max of about 20 ms was in all 3 macOS many-files runs, so two short local probes ran (4 harness runs, load average 19-25) to name it. Both patches were reverted with `git checkout -- <file>`; `git diff --quiet 74561f6 -- crates/` was empty after them. The filtered log (no file paths) is `runs/g6-macos-probe.log`. `runs/g6-linux-probe.log` was not created.
- **The finding changed the committed docs.** The Task 3 commit `c6d33fb` said "cause not measured" for the stripe hold. The follow-up commit `f37913e` replaced that with the probe result in the research doc, `deferred-items.md` and the #21 body copy.
- **Workspace test for the Task 1 and Task 2 commits.** `cargo test --workspace` ran once on the tree of `74561f6` (exit 0, 67 `test result: ok`, 0 FAILED). The Task 1 and Task 2 commits add only log files, and `crates/` did not change after that run.
- **The held_hook comment (orchestrator note 3)** is in its own commit `74561f6`, before the measurements, so the measured tree has it.

### Auto-fixed Issues

None.

**Total deviations:** 0 auto-fixes, 7 plan facts. **Impact:** none on the six-site results; one finding for the owner (the unlink reclaim under the stripes).

## Known Stubs

None.

## Threat Flags

None. No new endpoint, file kind or trust boundary.
- T-19-SC: no new crate.
- T-19-61: the doc and the body copy have no home paths and no forbidden words (grep). The logs have no home paths (grep for `Users/`: 0 in both).
- T-19-62: the thresholds and the outlier rule were in the plan before the runs. The Docker run streamed the committed tree. The logs are committed.
- T-19-63: two probe patches were applied locally after the measurement runs and reverted. `git status --porcelain -- crates/` was empty after Tasks 1 and 2 and after each probe, and `git diff --quiet 74561f6 -- crates/` was empty before the last commit.

## Self-Check: PASSED

- The three run logs in `runs/`, the #21 body copy and this SUMMARY: FOUND.
- Commits `74561f6`, `050bf7e`, `4ff502e`, `c6d33fb` and `f37913e`: FOUND on HEAD (`git rev-list --count 5820dd3..f37913e` prints 5).
