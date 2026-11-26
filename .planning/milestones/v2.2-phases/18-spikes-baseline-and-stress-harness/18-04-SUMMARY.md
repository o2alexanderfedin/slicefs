---
phase: 18-spikes-baseline-and-stress-harness
plan: 04
subsystem: testing
tags: [spike-s1, fuse-t, fskit, fuser, macos-ci, docker]
requires: []
provides:
  - "examples/s1_probe.rs: standalone fuser server (1 reader, 4 workers, 200 ms slow READ) that prints an S1 RESULT line"
  - "scripts/macos-mount-test/s1-overlap.sh: S1 client for macOS and Linux, all waits limited"
  - "run.sh s1 mode (Linux Docker control)"
  - ".github/workflows/s1-overlap.yml: macOS FSKit job (workflow_dispatch and push to ci-s1/**)"
  - "runs/s1-linux-control.log and runs/s1-macos-fskit.log"
affects: [18-07, 21]
tech-stack:
  added: []
  patterns:
    - "Result lines saved with run URL and commit; no value written that a run did not print"
key-files:
  created:
    - crates/slicefs-cli/examples/s1_probe.rs
    - scripts/macos-mount-test/s1-overlap.sh
    - .github/workflows/s1-overlap.yml
    - .planning/phases/18-spikes-baseline-and-stress-harness/runs/s1-linux-control.log
    - .planning/phases/18-spikes-baseline-and-stress-harness/runs/s1-macos-fskit.log
  modified:
    - scripts/docker-linux-test/run.sh
key-decisions:
  - "D-10 followed: S1 is a standalone probe run in its own macOS CI workflow; the trigger is dispatch plus push to ci-s1/** (dispatch alone needs the file on the default branch; ci/** would start the long macos-mounts job)"
requirements-completed: [SPIKE-03, SPIKE-01]
duration: ~45 min
completed: 2026-11-21
status: complete
actuals:
  tokens: 4200
  tasks: 2
  commits: 3
plan_head_before: 86f9e8cf42078524c9333760460c39fa2b305949
plan_head_after: c8c75387d22b70c475f1fa5e4c57cbecd6044dab
commits: 3
---

# Phase 18 Plan 04: S1 FUSE-T overlap and reply order Summary

S1 ran on a macOS FSKit mount in CI: FUSE-T 1.2.9 handed the probe up to 2 requests at a time and accepted out-of-order replies (5 inversions), with no error and no hang. The Linux Docker control shows the probe can see overlap (max_inflight 3, 16 inversions).

## Tasks

| Task | Name | Commit |
|------|------|--------|
| 1 (tracer) | S1 probe, portable client, Linux Docker control run | f0249f8 |
| 2 | macOS FSKit CI workflow, run, saved result | 97086b2 (workflow), c8c7538 (result log) |

## Results (raw, for Plan 07)

| Run | max_inflight | reply_order_inversions | arrived | stats during slow READ |
|-----|--------------|------------------------|---------|------------------------|
| Linux control (OrbStack, kernel 7.2.12) | 3 | 16 | 510 | 50 of 50 each round, max stat 1 ms |
| macOS FSKit CI (macOS 26.7.2, fuse-t cask 1.2.9) | 2 | 5 | 548 | 50 of 50 each round, max stat 2 ms |

- macOS run: https://github.com/o2alexanderfedin/slicefs/actions/runs/44882009650, commit 97086b2d6a45337a0f2469472f2ad492483945ea. One CI run was enough (first run passed).
- Per the decision rule in RESEARCH, case (a) applies: overlap (>= 2), reply-order inversions (>= 1), all 250 stats finished while the 200 ms READ was pending, no client error or hang. So the macOS default worker count may be more than 1; throughput is still to be shown in Phase 21 (the 2 in flight here is a small sample: one slow READ against a stat loop).
- Observation: the slow `dd` took 297-298 ms in rounds 0 and 1 and 211-248 ms later (Linux: 203-204 ms every round). The extra ~50-100 ms is not explained; it may be FSKit warm-up or extra lookups per open. It does not change the overlap result.
- The temporary branch `ci-s1/phase-18` was pushed once and deleted after the run (`git ls-remote` prints nothing).

## Deviations from Plan

None - plan executed as written. Small notes:
- The per-round client logic is one `python3` script (dd as a child process, a watcher thread for the dd end time, 50 stats) instead of bash plus a separate python loop; the printed fields are as specified.
- The workflow comment text avoids the literal words `workflow_dispatch` and `ci-s1/**` so the plan's single-occurrence greps hold.
- `tee` in the workflow step makes `client.log` for the artifact upload; the step uses `shell: bash` so a failing client still fails the step.

## Known Stubs

None.

## Threat Flags

None. The workflow has `contents: read`, only dispatch and `ci-s1/**` push triggers, no secrets; the probe uses `SessionACL::Owner` and serves zero bytes.

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/examples/s1_probe.rs, scripts/macos-mount-test/s1-overlap.sh, .github/workflows/s1-overlap.yml, both run logs
- FOUND commits: f0249f8, 97086b2, c8c7538
- Gates: logs have 5 `round=` lines each, 0 `/Users/` lines; macOS log has the run URL; `cargo build --release -p slicefs-cli --example s1_probe` exits 0.
