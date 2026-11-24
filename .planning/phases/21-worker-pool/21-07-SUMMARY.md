---
phase: 21-worker-pool
plan: 07
subsystem: ci
status: complete
tags: [pool-04, ci, workers-option, docker, pjdfstest]

requires:
  - phase: 21-worker-pool
    provides: "--workers and the `Workers: N (set|default)` line (21-01), -e SLICEFS_WORKERS in run.sh (21-01), the mt-linux.sh Workers check (21-01), panic-replay.sh with explicit --workers (21-03), alloc_tune (21-05), the #170 busy-store fix (merge f15a834)"
provides:
  - "SLICEFS_WORKERS in every mount of the Linux CI path (mt-linux.sh, dedup-probe.sh, verify-phases.sh, pjdfstest.sh), with a printed Workers line and a FAIL on a wrong line"
  - "ci.yml Linux job: env SLICEFS_WORKERS=4, each step with pipefail, tee to $RUNNER_TEMP and a required 'Workers: 4 (set)'"
  - "large-copy.sh and boost-test.sh print the Workers line, so the macOS FSKit log shows the default"
  - "Local Docker logs with 4 workers: all, verify, pjdfstest, and the red run of the Workers check"
affects: [21-08, 21-09]

estimate:
  tokens: 70000
  tasks: 3
actuals:
  tokens: 17000    # chars/4 over the added lines (scripts, workflow, 4 logs, this SUMMARY)
  tasks: 3
  commits: 3

plan_head_before: 35a0b4f00326d8923e6dd1cd924bfb0156d6bf94
plan_head_after: bc0d9279991d62e97e8f1b57bd3fbfe440d27c70
commits: 3

tech-stack:
  added: []
  patterns:
    - "A shared mount log: count its lines before the mount, read only the new lines after the mount is up"
    - "The Workers line is read with `grep -o 'Workers: [0-9]* ([a-z]*)'`, not `^Workers:`, so a cut last line of a killed daemon cannot hide the line of the next mount"
    - "A CI step that must prove a setting reached the container: pipefail, tee to a file, grep the file"

key-files:
  created:
    - .planning/phases/21-worker-pool/runs/docker-all-w4.log
    - .planning/phases/21-worker-pool/runs/docker-verify-w4.log
    - .planning/phases/21-worker-pool/runs/docker-pjdfstest-w4.log
    - .planning/phases/21-worker-pool/runs/workers-check-red.log
  modified:
    - scripts/docker-linux-test/verify-phases.sh
    - scripts/docker-linux-test/pjdfstest.sh
    - scripts/docker-linux-test/dedup-probe.sh
    - .github/workflows/ci.yml
    - scripts/macos-mount-test/large-copy.sh
    - scripts/macos-mount-test/boost-test.sh

decisions:
  - "verify-phases.sh mnt adds --workers only when the caller gives no --workers (a case on the argument list); panic-replay.sh is not changed"
  - "The macOS scripts print the last Workers line of the appended mount log (the line of the mount that just came up)"

metrics:
  duration: "about 75 min"
  completed: 2026-11-24
---

# Phase 21 Plan 07: CI with 4 workers on Linux Summary

The Linux CI path now mounts with `--workers $SLICEFS_WORKERS` in every script, prints the Workers line of each mount and fails on a wrong line. ci.yml sets `SLICEFS_WORKERS: "4"` on the Linux job, and each of its three steps fails when the output has no `Workers: 4 (set)`. Local Docker runs of `all`, `verify` and `pjdfstest` pass with 4 workers. The red run shows that a wrong value fails.

## Tasks

| Task | Name | Commit | Files |
| ---- | ---- | ------ | ----- |
| 1 | Worker count in every mount of the Linux CI path, with a loud check | dd95455 | verify-phases.sh, pjdfstest.sh, dedup-probe.sh |
| 2 | ci.yml Linux job with 4 workers and a Workers check per step; macOS scripts print the line | 1ca0f2c | ci.yml, large-copy.sh, boost-test.sh |
| 3 | Local Docker runs of all, verify and pjdfstest with 4 workers | bc0d927 | runs/docker-*-w4.log, runs/workers-check-red.log |

## Verification

- Task 1: `bash -n` passes for the 3 scripts, and each has `SLICEFS_WORKERS` 3 times and `Workers:` 3 times.
- Task 2: Ruby YAML gives `env=4 checks=3 pipefail=3`. The public-text gate (added lines since 9157444) gives 0. `bash -n` passes for both macOS scripts. `git diff --stat 9157444 -- .github/workflows/macos-mounts.yml` is empty.
- Task 3: all plan greps pass. In `all`: 4 times `Workers: 4 (set)`, 1 `daemon-exit: ok`, 1 `content ok`, 0 FAIL. In `verify`: 25 of 25 mounts `Workers: 4 (set)`, 0 `: FAIL`, 0 `NOT MOUNTED`. In `pjdfstest`: `Result: PASS`, `Workers: 4 (set)`, and `daemon alive after run: yes`. All 4 logs have the same toolchain as the 20-01 notes. The red log has 2 `red-exit=1` and 4 FAIL lines.
- `cargo test --workspace` on macOS exited 0 before the Task 1 and 2 commits (1009 s). It also exited 0 a second time (686 s) on the same code before the log and docs commits.

## Local Docker results (4 workers)

All runs used the mount form, a tar stream of the tree at 1ca0f2c (the code of HEAD: `git diff --stat 1ca0f2c HEAD -- . ':!.planning'` is empty; see the tree note under Deviations), the volume `slicefs-target-p21`, `-e SLICEFS_WORKERS=4` and the 45 s wait. The toolchain was `rustc 1.100.0 (0ef1bc386 2026-11-09)`, the same as the 20-01 notes. The host had 32 CPUs. The logs are raw (the cargo warning blocks are kept) and have no home paths.

| Mode | Result | Time |
| ---- | ------ | ---- |
| red run (debug) | both copies FAIL on the Workers line, `red-exit=1` twice | 50 s |
| all | workspace tests 1515 passed, 0 failed, 12 ignored (74 `test result` lines, doc tests included); vendored fuser 54 passed, 0 failed; mt-linux.sh all steps ok, `daemon-exit: ok`, pool line `jobs=163` over 4 workers (39,41,41,42); dedup-probe.sh `content ok` | 166 s (mounts start at 161 s) |
| verify | `RESULT verify-phases: PASS`; `RESULT panic-replay: PASS` (w4 and w1); large copy VmHWM 78 MiB at unmount (cap 512) | 165 s |
| pjdfstest (release) | `Files=237, Tests=8792, 190 wallclock secs`, `Result: PASS`; `open/25.t` rc=0 `Result: PASS`; no failing test files; `daemon alive after run: yes` | 245 s |

The pool did not show any Phase 19 deferred window in these runs. No fix was needed.

## Red run

`runs/workers-check-red.log`. One container, debug build. In the container, a `sed` on copies under /tmp (not on the committed scripts) changed `"Workers: $SLICEFS_WORKERS (set)"` to `"Workers: 5 (set)"`. The log shows 1 change in each copy and 0 in the committed scripts. With `SLICEFS_WORKERS=4`:
- mt-linux.sh copy: all file steps ok, then `workers-line: Workers: 4 (set)`, `workers-line: FAIL`, `red-exit=1`.
- dedup-probe.sh copy: `workers-line: FAIL` after each of its 3 mounts, `content ok`, then `red-exit=1`.

So the only failure is the Workers check. verify-phases.sh and pjdfstest.sh use the same compare. The green runs show their lines.

## Orchestrator after this plan

These steps are an early check of the CI wiring. POOL-04 is marked only on the CI evidence for the final code commit. The orchestrator collects that evidence after plan 21-08 (see the 21-08 SUMMARY). This plan did not push and did not run CI.

1. Push `feature/phase-21-worker-pool`, then open or update its pull request to `develop`, so ci.yml runs (ci.yml runs only on a push to main or develop, or on a pull request to them). Check that the Linux job passed. Check that the log of each of its three steps (all, verify, pjdfstest) has `Workers: 4 (set)`: each step fails without the line, so a green step is the proof. Check that the macOS job (`cargo test --workspace`) passed.
2. Push the same commit to a branch named `ci/<name>` (for example `ci/21-07-workers`), so macos-mounts.yml runs on FSKit (push to main or `ci/**`). Check that it passed and that its log has `workers: Workers: 1 (default)` (large-copy.sh and boost-test.sh print it after each mount). macos-mounts.yml has `paths-ignore: [".planning/**", "**/*.md"]`. If GitHub skips the push run for that reason, start the run by dispatch on that branch.
3. Report any failure before plan 21-08 starts. The evidence file `.planning/phases/21-worker-pool/runs/ci-evidence.md` is written later, for the final code commit.

## Deviations from Plan

- **One workspace test run for Tasks 1 and 2.** I made both edits first. One macOS `cargo test --workspace` run (exit 0, 1009 s) came before both commits. The code did not change after it. A second run on the same code came before the log and docs commits.
- **Workers line pattern.** The scripts read the line with `grep -o 'Workers: [0-9]* ([a-z]*)'`, not `grep '^Workers:'`. In verify-phases.sh, a daemon killed with kill -9 can leave a cut last line. The next mount then appends its first line to that cut line, and `^Workers:` would not find it. The compare with `Workers: $SLICEFS_WORKERS (set)` is as the plan says.
- **macOS scripts print the last line, not the first.** The plan says "the first `Workers:` line of the mount log". Both macOS scripts append to one log across several mounts. So `tail -1` gives the line of the mount that just came up. With the same default, the text is the same.
- **Tree note for three runs.** The tar takes `git ls-files -co`, and I copied each log into `runs/` before the next container. So the `all` tree also had 1 untracked log file, `verify` had 2 and `pjdfstest` had 3. Only the red run had a clean tree. These files are logs under `.planning/`, and the code was that of 1ca0f2c in every run, so the results stand (the same case as the header of `verify-21-03.log`). The committed log headers say "committed tree at 1ca0f2c"; this note makes that exact. The 686 s green macOS run was on the same code, so the SUMMARY fix commit did not need a third run.
- **dedup-probe.sh exit.** The exit status is that of the last unmount (as before), and 1 when a Workers line was wrong.

## Threat model

- T-21-SC: no new crate.
- T-21-19: each script fails on a wrong Workers line (the red run shows this), and each ci.yml step greps `Workers: 4 (set)`.
- T-21-20: pjdfstest and the live checks passed with 4 workers. No Phase 19 deferred window showed.
- T-21-21: the added-lines gate gives 0. The 4 logs have 0 home paths and 0 legal-terms words.

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: runs/docker-all-w4.log, runs/docker-verify-w4.log, runs/docker-pjdfstest-w4.log, runs/workers-check-red.log
- FOUND: commits dd95455, 1ca0f2c, bc0d927 in HEAD
- commits: 3 = `git rev-list --count 35a0b4f..bc0d927` (the docs commit of this SUMMARY comes after)
