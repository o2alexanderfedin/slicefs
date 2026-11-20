---
phase: 14-buffered-delta-re-log
plan: 03
subsystem: testing
tags: [docker, libfuse, memory, random-write, "#98"]
status: complete
requires: []
provides:
  - "scripts/docker-linux-test/bigwrite.sh: random-write VmHWM check (measure only while CAP_MIB is empty)"
  - "run.sh bigwrite [size-MiB ...] (release build, by hand, not in CI)"
  - "Measured 'before' numbers at 1 GiB for the #98 comment"
affects: [14-07, 14-08]
tech-stack:
  added: []
  patterns: ["one python3 process writes the same seeded random blocks to the mount and to a reference file", "SLICEFS_TARGET_VOL isolates the cargo target volume per run"]
key-files:
  created:
    - scripts/docker-linux-test/bigwrite.sh
  modified:
    - scripts/docker-linux-test/run.sh
decisions:
  - "Before (old code, 1 GiB): VmHWM after random write 6224 MiB, so the check sees the whole-file buffer"
  - "run.sh gets SLICEFS_TARGET_VOL (default slicefs-target), so runs from parallel worktrees do not share the release binary"
metrics:
  duration: "about 15 min"
  completed: 2026-11-18
actuals:
  tokens: 2015    # chars/4 of the diff 5fce243..b9d9d33 (8059 chars)
  tasks: 2
  commits: 1      # git rev-list --count 5fce243..b9d9d33; the SUMMARY commits are not counted
plan_head_before: 5fce24319860899851a66716622e7e41bc40e817
plan_head_after: b9d9d3347a946c8e535918285ba4caeacfddc3e8
---

# Phase 14 Plan 03: bigwrite Docker memory check (#98) Summary

`run.sh bigwrite [size-MiB ...]` builds a release binary in Docker and runs `bigwrite.sh`. For each size, the script copies a urandom file into a fresh store, then one python3 process writes 512 MiB of seeded random 1 MiB and 4 KiB blocks to that file and to a reference copy. Then it does cmp, and cmp again after a remount. It prints the daemon VmHWM after each step. On the old code (before plan 14-01), the 1 GiB run shows a VmHWM of **6224 MiB**. The check sees the whole-file buffer, so it can fail.

## Script facts (for plan 14-07)

- Mount flags: `--wal-strategy per-op --gc-interval 0`, default commit interval (D-11).
- `CAP_MIB=` is empty: measure only. When it is set, the script adds `bw-rss-bounded-S` (VmHWM after cmp <= CAP_MIB) and `bw-overlay-used-S` (at least one `switches to overlay` line from this daemon).
- Steps per size: `bw-cp-S` (900 s), `bw-random-write-S` (1800 s), `bw-cmp-S` (600 s), `bw-cmp-after-remount-S` (600 s). A seed failure gives `bw-seed-S: FAIL`.
- Info line (printed after the first unmount, so `CAS bytes` reads a closed store; the three VmHWM values are read before the unmount):
  `bigwrite S MiB: VmHWM after cp A, after random write B, after cmp C MiB (cap N MiB|measure only); writes W, max write X ms, max fsync Y ms; overlay switches N; CAS bytes X; times cp/rw/cmp T1/T2/T3 s`
- End lines: `    run time: N s` and `RESULT bigwrite: PASS|FAIL`, with the exit status.

## "Before" run (Task 2, old code, 1 GiB)

Tree: this worktree at b9d9d33 (base 5fce243). `grep -c "WriteMode::Overlay" crates/slicefs-cli/src/filesystem.rs` printed 0, so no scratch worktree was needed. Command: `SLICEFS_TARGET_VOL=slicefs-target-1403 scripts/docker-linux-test/run.sh bigwrite 1024`. Exit status 0.

```
  bw-cp-1024: ok
  bw-random-write-1024: ok
  bw-cmp-1024: ok
    bigwrite 1024 MiB: VmHWM after cp 81, after random write 6224, after cmp 6224 MiB (measure only); writes 65792, max write 17295 ms, max fsync 331 ms; overlay switches 0; CAS bytes 1912157485; times cp/rw/cmp 28/37/33 s
  bw-cmp-after-remount-1024: ok
    run time: 114 s
RESULT bigwrite: PASS
```

- VmHWM: after cp **81 MiB**, after random write **6224 MiB**, after cmp **6224 MiB**. 6224 MiB is about 6x the file. The probable cause is that Buffered mode holds the whole file and the commits make more copies of it. This is not checked.
- Slowest write: **17295 ms**. It is probably the first write after the O_RDWR open, which loads the whole file. This is not checked, because rw.py does not record which write was the slowest. Slowest fsync: **331 ms**.
- Step times: cp 28 s, random write 37 s, cmp 33 s. Script run time 114 s. Whole run time (image check, cached release build, wait until the container is 45 s old, script): **163 s**.
- Overlay switches: 0. This is expected, because the old code has no overlay switch line. mount.log stays inside the container, so the count of `switches to Buffered` lines was not read.
- CAS bytes after the first unmount: 1912157485 (about 1.78 GiB). The probable cause is the 1 GiB copy plus the chunks that the random writes made. This is not checked.
- Release build time: **about 59 s**, derived and not timed. The 45 s wait counts the build time. In the "before" run, the fixed overhead (image check, container start) is 163 - 114 - 45 = about 4 s. The first (cold) build of the private target volume was in the 64 MiB smoke run, which took 86 s with 23 s of script time, so the build took about 86 - 23 - 4 = 59 s (the cargo registry volume was warm). The "before" run used the build that was already in the volume.
- Another agent can use Docker at the same time, so the times can change from run to run.
- Scope: the third VmHWM is read after cmp and before the unmount, so the final commit at unmount is not in it. The 4096 MiB size was not run in this plan (plan 14-07 runs it on the new code).

## Smoke run before the Task 1 commit (64 MiB)

`run.sh bigwrite 64` exited 0: `VmHWM after cp 70, after random write 130, after cmp 200 MiB; writes 65792, max write 86 ms, max fsync 259 ms; overlay switches 0; CAS bytes 135834088; times cp/rw/cmp 1/18/1 s`, all four steps ok. It proved the empty-directory seed, rw.py, the info line and the pass-through of the size argument.

## Deviations from Plan

1. **[Rule 3 - Blocking] `SLICEFS_TARGET_VOL` in run.sh.** All worktrees mount their source at `/src` and share the `slicefs-target` volume. Another agent's release build of the new code could replace the binary of the "before" run, and the measurement would be false. The `RUN` array now uses `-v "${SLICEFS_TARGET_VOL:-slicefs-target}":/target`. The default stays the same, so CI and the other modes do not change. Both runs used `slicefs-target-1403`. I created that volume and removed it after the runs. Commit b9d9d33.
2. **bash 3.2 pass-through.** macOS `/bin/bash` 3.2 with `set -u` fails on an empty `"$@"`, so the bigwrite mode passes `_ ${@+"$@"}`. Commit b9d9d33.
3. **Info line after the first unmount.** The plan prints the info line before the unmount. Here the three VmHWM values are still read before the unmount, but the line is printed after it, so that `slicefs stats` reads `CAS bytes` from a closed store.
4. **Task 2 has no commit of its own.** The run needed no script change. This SUMMARY commit carries its numbers.
5. **Executor process:** the launcher chose the branch `feature/14-03-bigwrite`, so the agent-branch allow-list of the commit protocol was not applied. As the launcher told, STATE.md, ROADMAP.md and REQUIREMENTS.md were not changed, and nothing was pushed.

## Logs (scratchpad, not committed)

- /private/tmp/claude-501/-Users-alexanderfedin-projects-slicefs/d764d3d1-c5db-45fa-99e9-83e307688877/scratchpad/14-03-before.log
- /private/tmp/claude-501/-Users-alexanderfedin-projects-slicefs/d764d3d1-c5db-45fa-99e9-83e307688877/scratchpad/14-03-smoke.log

## Self-Check: PASSED

- FOUND: scripts/docker-linux-test/bigwrite.sh (executable, `bash -n` ok)
- FOUND: scripts/docker-linux-test/run.sh bigwrite mode (`bash -n` ok, `/dk/bigwrite.sh /target/release/slicefs` present)
- FOUND: b9d9d33 (ancestor of HEAD); `git rev-list --count 5fce243..b9d9d33` = 1
- Task 2 verify command: exit 0 (`bw-cmp-after-remount-1024: ok`, after random write 6224 >= 1024)
- verify-phases.sh and .github are unchanged
