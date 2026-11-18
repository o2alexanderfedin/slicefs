---
phase: 13-write-log-close-out
plan: 02
subsystem: testing
tags: [write-log, docker, libfuse, kill-9, memory, "#20"]
status: complete
requires: []
provides:
  - "verify-phases.sh sections Phase 05c, Phase 05d, Large copy (#20)"
  - "Measured daemon VmHWM, CAP_MIB, step times and docker stats for the #20 comment"
affects: [13-03]
tech-stack:
  added: []
  patterns: ["one python3 handle writes reversed 1 MiB blocks inside 16 MiB groups", "unsynced control rename proves the directory fsync mattered"]
key-files:
  created: []
  modified:
    - scripts/docker-linux-test/verify-phases.sh
decisions:
  - "CAP_MIB=512: measured peak P=130 MiB <= 341 MiB, so the first branch of the cap rule applies"
  - "Step timeouts set to 3x the measured maximum: big-copy-in-order 180 s, all other big steps 120 s"
  - "Out-of-order pattern is reversed 1 MiB blocks inside 16 MiB groups on one handle, not dd per block (D-04 deviation, see below)"
metrics:
  duration: "about 15 min"
  completed: 2026-11-18
actuals:
  tokens: 3243
  tasks: 3
  commits: 3
plan_head_before: 1c22b19370e261722b0dbeee613a1bc6657bd90b
plan_head_after: b06127913a8adec50ae2ad1b52af927e26cd310d
---

# Phase 13 Plan 02: Docker checks for the write log close-out Summary

verify-phases.sh now has three new real-mount checks on Linux libfuse in Docker. Phase 05c: a rename whose only sync is `sync <dir>` survives `kill -9`, and an unsynced control rename is lost. Phase 05d: a cut of 3 bytes in the newest segment does not stop the mount. Large copy: 1 GiB in order and out of order, with cmp after the copy and after a remount. The daemon VmHWM peaked at 130 MiB against a 512 MiB cap.

## Sections and step names (exact, for the #20 comment)

- `== Phase 05c: a rename is durable after an fsync of the directory only (write log, #20)`
  - steps: `rename-survives-dir-fsync`, `old-name-gone-after-dir-fsync`
  - info line: `rename with no sync after it: lost` (other values: `kept`, `neither p.txt nor q.txt exists`)
  - also `dir-fsync` (a bad line only if `sync <dir>` fails)
- `== Phase 05d: a torn last record does not stop the mount (write log, #20)`
  - info line: `cut 3 bytes of segment-NNNNNN.seg`
  - steps: `mount-with-torn-last-record`, `records-before-the-tear-kept`, `torn-record-not-applied`, `write-after-torn-tail`, `scrub-after-torn-tail`
- `== Large copy (#20): 1 GiB in order and out of order, bounded daemon memory`
  - steps: `big-copy-in-order`, `big-write-out-of-order`, `big-in-order-cmp`, `big-out-of-order-cmp`, `daemon-rss-bounded`, `no-switch-to-buffered`, `big-in-order-cmp-after-remount`, `big-out-of-order-cmp-after-remount`
  - info lines: `timing: ...`, `daemon VmHWM: ... (cap 512 MiB)`, `section time: N s`

## Measurement run 1 (Task 1, Large copy only, no cap yet)

- `timing: cp 47 s, out-of-order write 16 s, cmp 25 s`
- `daemon VmHWM: after cp 84 MiB, after out-of-order write 111 MiB, after cmp 111 MiB` (H1=84, H2=111, H3=111)
- B1-B0 = 0 (no switch to Buffered)
- Section time: 124 s. Whole verify run (container, build included): 196 s.
- `docker stats` MemUsage peak during the section: 118.1 MiB. An earlier sample of 171.7 MiB came during the in-container cargo build, before the first mount.
- No `daemon-exit` failure.

## Measurement run 2 (Task 2, all sections)

- `timing: cp 47 s, out-of-order write 14 s, cmp 30 s`
- `daemon VmHWM: after cp 85 MiB, after out-of-order write 130 MiB, after cmp 130 MiB` (H1=85, H2=130, H3=130)
- B1-B0 = 0
- `rename with no sync after it: lost`
- `cut 3 bytes of segment-000002.seg`
- Replay lines: 05c `Replayed 43 request(s) from the write log (0 skipped).`, 05d `Replayed 21 request(s) from the write log (0 skipped).`
- Section time: 126 s. Whole run: 199 s. `docker stats` peak: 167.6 MiB (one sample near the end of the section; the other samples were at most 136.4 MiB).

## Cap rule result

- P = max(H3 run 1, H3 run 2) = max(111, 130) = **130 MiB**
- Branch: P <= 341 (2/3 of 512), so **CAP_MIB=512**, the value D-04 proposed. No 1.5x branch and no stop.
- Timeout rule (3x the larger measured time, rounded up to a multiple of 60 s, minimum 120 s). The script prints one combined cmp time, so that time is used for all four cmp steps:
  - big-copy-in-order: max 47 s -> 180 s
  - big-write-out-of-order: max 16 s -> 120 s
  - big-in-order-cmp, big-out-of-order-cmp, both `-after-remount`: max cmp 30 s -> 120 s

## Final run (Task 3)

- Exit status 0, `RESULT verify-phases: PASS`, no FAIL line.
- `timing: cp 47 s, out-of-order write 16 s, cmp 28 s`
- `daemon VmHWM: after cp 84 MiB, after out-of-order write 112 MiB, after cmp 112 MiB (cap 512 MiB)`
- `daemon-rss-bounded: ok`, `no-switch-to-buffered: ok`
- `rename with no sync after it: lost`
- `cut 3 bytes of segment-000002.seg`, all 05d steps ok
- Section time: **126 s**. Whole verify run: 200 s.
- `docker stats` MemUsage peak of the final run: **114.8 MiB** (out of 15.68 GiB). On OrbStack this number does not include the page cache of the 1 GiB files. It is close to the daemon VmHWM.
- All 15 new results are `ok`: big-copy-in-order, big-write-out-of-order, big-in-order-cmp, big-out-of-order-cmp, no-switch-to-buffered, daemon-rss-bounded, big-in-order-cmp-after-remount, big-out-of-order-cmp-after-remount, rename-survives-dir-fsync, old-name-gone-after-dir-fsync, mount-with-torn-last-record, records-before-the-tear-kept, torn-record-not-applied, write-after-torn-tail, scrub-after-torn-tail.

Build: debug (`cargo build -q -p slicefs-cli`, as `run.sh verify` builds it). Host: OrbStack on macOS.

## Deviation note from D-04 (out-of-order pattern)

D-04 said "reverse 1 MiB blocks with `dd seek=N conv=notrunc`". That pattern cannot meet any cap below the file size. One `dd` per block opens the file again, and an open of a file that has content goes to Buffered mode at once. A full reverse on one handle jumps past REORDER_WINDOW (64 MiB) and then writes behind the stream, which also switches to Buffered (crates/slicefs-cli/src/filesystem.rs, Streaming branch of `test_write`). The check uses one python3 process with one handle. It writes 1 MiB blocks last to first inside each 16 MiB group, with an fsync every 64 MiB (as the NFS COMMITs did). Every jump stays inside the reorder window. `no-switch-to-buffered` fails the run if any handle leaves Streaming mode. **Scope of the bound for the #20 comment:** daemon memory is bounded for in-order writes and for out-of-order writes that stay inside the 64 MiB reorder window. A random write pattern outside the window still holds the whole file in RAM (Buffered mode). This check does not cover that case.

Also checked before the runs: the daemon does not turn on the FUSE writeback cache (no `writeback` flag in crates/slicefs-cli/src or vendor/fuser/src). So the kernel sends the reversed blocks in the order that the writer uses, and `no-switch-to-buffered` is real evidence. `fsyncdir` goes to `handle_fsyncdir` (fuse_callbacks.rs l.282), which calls `flush_wal`. Segment files are not preallocated, so `truncate -s -3` cuts the last record.

## Deviations from Plan

1. **[Rule 2 - Missing output] Added a `    section time: N s` info line.** The SUMMARY must give the total Large copy section time, and the plan's two info lines did not carry it. Commit 198c08c.
2. **[Rule 2 - Clarity] Added a third value to the 05c control line.** If neither p.txt nor q.txt exists, the line prints `neither p.txt nor q.txt exists`, so this case cannot be read as "lost". Commit 8e53663.
3. **Executor process:** the role's per-commit `agent-*` branch allow-list was not applied, because the launcher told the agent to work on `feature/13-docker-checks`. The detached-HEAD and protected-branch checks were applied. As the launcher told, STATE.md, ROADMAP.md and REQUIREMENTS.md were not updated and nothing was pushed.

Each task's verify run is the gate for its commit: runs 1, 2 and 3 all exited 0 before their commits.

## Notes

- The shell prints `line N: PID Killed ... $B mount ...` after each `kill -9`. These lines come from bash job control. Phase 05b already printed them before this plan.
- No `daemon-exit` failure in any run. The unmount after the 1 GiB session is fast.
- The section takes about 2 minutes, which is less than the 10-minute limit. The CI time grows by about 2 minutes.
- The timeouts are 3x the times measured on OrbStack. A much slower CI runner (more than 3x slower) can kill a step. If that happens, raise the timeouts from CI times; do not change the cap.

## Self-Check: PASSED

- FOUND: scripts/docker-linux-test/verify-phases.sh (sections Phase 05c, Phase 05d, Large copy (#20))
- FOUND: 198c08c, 8e53663, b061279 (ancestors of HEAD)
- Final run log: `RESULT verify-phases: PASS`, `daemon-rss-bounded: ok`, `rename with no sync after it: lost`
