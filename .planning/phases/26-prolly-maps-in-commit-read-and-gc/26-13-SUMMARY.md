---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 13
subsystem: bench
status: complete
tags: [pmap07, prolly, bench, docker, verdict]
requires: ["26-01", "26-11", "26-12"]
provides:
  - "PMAP-07 verdict on the product code: pass (A, B and C)"
  - "boost mode of scripts/prolly-bench/pmap07.sh"
  - "research record: Phase 26 product run, insert/delete rows and their acceptance"
affects: []
tech-stack:
  added: []
  patterns: ["quiet-start gate before each decision run", "verdict is unedited script output"]
key-files:
  created:
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/pmap07/verdict.txt
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/pmap07/harness-smoke.log
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/pmap07/run-order.log
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-13.log
  modified:
    - scripts/prolly-bench/pmap07.sh
    - scripts/prolly-bench/README.md
    - docs/research/prolly-trees.md
key-decisions:
  - "Insert/delete rows where prolly is worse get a written acceptance; code mitigations are follow-ups only"
  - "Each decision run starts only after three quiet samples of the Docker VM (load < 3, disk writes < 30 MB/s, host load < 24), at most 45 min"
  - "Cargo target volumes slicefs-target-pmap07-flat and -prolly (one per source tree), warmed by the smoke"
metrics:
  completed: 2026-11-28
actuals:
  tokens: 39100
  tasks: 3
  commits: 3
plan_head_before: ba401563f005e38b3d19121e54c1961ab5e73196
plan_head_after: 7e14ead9ff9d8a48ea8621750a6cd990f2779337
---

# Phase 26 Plan 13: PMAP-07 decision run Summary

PMAP-07 passes on the product code. At 1,000,000 files a one-change commit
takes 6.8 ms in place of 528.6 ms, 1.6 in place of 515.1 CPU ms, and 18 in
place of 14,181 nodes. All nine gated Boost rows are better than flat.

## Commits

- 3a64054 feat(26-13): boost mode of the PMAP-07 runner and the harness smoke
- 92ee5fc docs(26-13): PMAP-07 decision runs and verdict (pass)
- 7e14ead docs(26-13): Phase 26 product run in the research record; gate log

`commits: 3` counts these three. The SUMMARY commit is not in the count.

## The numbers (from `runs/pmap07/verdict.txt`)

- Check A (limit 0.20): CPU ratio 0.003 (515.083 to 1.575 ms), nodes
  ratio 0.001 (14,181 to 18). Pass.
- Check B (lower than flat): wall ratio 0.013 (528.631 to 6.786 ms). Pass.
- Check C (recorded, does not block): all 9 rows better, bytes ok. The CPU
  ratios are 0.052 to 0.731 and the wall ratios are 0.124 to 0.716. The
  highest of both is insdel K = 100. Pass.
- Verdict line: `verdict pmap07=pass`. PMAP-07 decided by checks A and B
  (owner amendment); check C recorded.
- Recorded rows where prolly is worse: insdel K = 1,000 (CPU 1.356, wall
  1.392, bytes 0.549) and scatter K = 1,000 (CPU 1.025, wall 1.074, bytes
  0.377). Both have a written acceptance in `docs/research/prolly-trees.md`.

Runs: flat is `v2.3.0` (582a41d) in `.claude/worktrees/v230-flat`, and
prolly is the branch at 3a64054. Product code is the same as the base
ba40156. The order was flat, prolly, flat, prolly, Boost flat, Boost
prolly, one at a time. All six runs had rc=0, and none was repeated.

## Decisions

- Insert/delete acceptance (the plan's discretion choice): every row where
  prolly is worse gets a written acceptance with its numbers. Code
  mitigations are follow-ups only.
- Quiet-start gate: other projects ran heavy jobs on the host (CI runner
  VMs in OrbStack, test binaries, a training job). I could not stop them.
  So I fixed a start condition before the first run, and each run waited
  for it. Load samples are in `runs/pmap07/run-order.log`. At the run
  starts the host load was 10.8 to 16.8 (32 CPUs) and the VM load was 0.3
  to 2.4. At the run ends the host load was up to 21.8.
- Target volumes: one per source tree (`slicefs-target-pmap07-flat` and
  `-prolly`). Both trees mount at `/src`, so a shared volume could mix the
  builds. The smoke used the same volumes, so the decision builds were warm.
- Smoke of the decision script: `pmap07.py` fixes n = 1,000,000 and gates
  K = 100. So the smoke Boost runs used `KS="1 10 100"`, and the smoke
  commit logs went to the script through scratch copies with `n=2000`
  changed to `n=1000000`. This is written in the header of
  `harness-smoke.log`. The decision logs were never changed.

## Deviations from Plan

1. **[Rule 3] The plan's rule-diff check gives 1, not 0.** The check
   compares against the commit with the subject "docs(26-01): pre-register
   the PMAP-07 rule" (8c6d271). `pmap07.py` changed once after that, in
   5cab585 (26-01, before any measurement; see the 26-01 SUMMARY). The
   rule file is unchanged since 8c6d271, and the script is unchanged since
   5cab585. Both checks are empty (`rule_rc=0`, `script_rc=0`).
2. **[Rule 2] Extra files in `runs/pmap07/`.** These are `run-order.log`
   (order, times, load samples, build heads) and, in each `boost-*` dir,
   `machine.txt`, `build.txt`, `disk.csv`, `edits.csv` and `shape.txt`.
   They show the build identity and that the runs did not overlap.
   `bench.sh` writes `commit=unknown` in the container, so `build.txt`
   records the source commit from the host.
3. **Docker gate volume.** The dispatch says to use the default
   `slicefs-target` volume. So `run.sh all` ran without
   `SLICEFS_TARGET_VOL` (the plan named `slicefs-target-26-13`).
4. **Workspace run before the first commit.** The dispatch says to commit
   only when `cargo test --workspace` passes. So I ran it once before the
   Task 1 commit and before any decision run (rc=0). The plan changes no
   Rust code.
5. **Smoke noise.** The Boost download ran at the same time as the harness
   smoke, so the prolly smoke commits had slow syncs (170 to 1,437 ms).
   The smoke decides nothing. The decision runs started only after the
   quiet gate.

## Gates

`runs/gate-26-13.log`, on 92ee5fc plus the research doc (no code change):

- `cargo test --workspace`: rc=0 (no `retry_tests` failure)
- `RUST_STABLE=stable scripts/safety/clippy.sh`: rc=0 (clippy ok, unsafe
  count ok: 35 files, 153 tokens)
- `scripts/docker-linux-test/run.sh all` (default target volume): rc=0
- `verdict=pass`

Also, before the first commit: `cargo test --workspace` rc=0. The
lock-stats test was not run, because no lock changed.

## Self-Check: PASSED

- Files exist: `runs/pmap07/verdict.txt`, `harness-smoke.log`,
  `run-order.log`, the four `commit-*.log`, `boost-flat/results.csv`,
  `boost-prolly/results.csv`, and `runs/gate-26-13.log`.
- Commits 3a64054, 92ee5fc and 7e14ead are ancestors of HEAD.
- Plan checks: 11 prolly samples in `commit-prolly-1.log` and 11 flat
  samples in `commit-flat-2.log`. `verdict.txt` has 3 check lines and 1
  verdict line. There are 380 `whole` rows in `boost-prolly/results.csv`.
  `prolly-trees.md` has 66 matching lines (`Phase 26: the product run`
  or `insdel`). The gate log has 3 `rc=0` lines and no `rc=[1-9]` line.
  There are no home paths and no legal text in the committed files.

## Cleanup

- The scratch folder (Boost data, raw logs) is removed at the end.
- The `v2.3.0` worktree `.claude/worktrees/v230-flat` is kept. It has the
  uncommitted bench copy that `prepare-v230` made.
- The Docker volumes `slicefs-target-pmap07-flat`, `slicefs-target-pmap07-prolly`
  and `slicefs-pmap07-work` are kept. The work volume has only an empty
  `tmp`.

## Known Stubs

None.

## Threat Flags

None. Boost data was only read (mounted read-only) and was never committed.
