# PMAP-07 decision rule (pre-registered)

Plan 26-01. The decision script `scripts/prolly-bench/pmap07.py` implements this
file word for word. The limits below (0.20, strictly lower, 2 x) are the owner
target of 26-CONTEXT.md, section "PMAP-07 target", and the reading of the Boost
rubric of its section "Claude's Discretion".

## Status

This file and the script are committed before any PMAP-07 measurement. The
decision runs are the runs of plan 26-13. Earlier runs (the early flat baseline
of plan 26-01, harness smoke runs) are feasibility checks and decide nothing.
Neither this file nor the script changes after the first decision run.

## Builds and platform

- flat: tag `v2.3.0` in a detached worktree, with the branch copy of
  `crates/metadata/tests/commit_bench.rs` and
  `scripts/prolly-bench/pmap07/report_flat.rs` (as its report module).
- prolly: the phase branch head at plan 26-13.
- Release builds. Linux in Docker (OrbStack) on this host. Stores and temp files
  are on the Docker volume `slicefs-pmap07-work` (disk-backed, not tmpfs).
- One run at a time, with no other heavy job (`pmap07.sh` refuses a second
  run). macOS numbers are not part of the decision.

## Commit runs

- Test `one_change_commit_at_scale`. `PMAP07_N=1000000` files (plus N/100
  directories and the root: 1,010,001 inodes). `PMAP07_SAMPLES=11` one-change
  commits after the first commit.
- Two rounds, in the order flat, prolly, flat, prolly. Each tree's numbers are
  the median over its 22 samples (Python `statistics.median`).
- Per sample:
  - `wall_ms`: the `commit()` call.
  - `cpu_ms`: user plus system CPU of the calling thread over the call
    (`CLOCK_THREAD_CPUTIME_ID`).
  - `nodes_read` and `nodes_written`.
- Flat nodes: read 0; written = every object of the six flat map values and of
  the root record of the new root, counted after the call.
- Prolly nodes: read and written by the commit (the commit counters of the
  store, plan 26-04).
- The CPU of the sync calls is inside `cpu_ms` in both builds. That bias is
  against prolly, so a pass is conservative.

## Check A (pre-sync work)

check A passes iff both hold:

- median `cpu_ms` prolly <= 0.20 x median `cpu_ms` flat;
- median (`nodes_read` + `nodes_written`) prolly <= 0.20 x the same median of
  flat.

The prolly `presync_ms` column is recorded for information only.

## Check B (total)

check B passes iff median `wall_ms` prolly < median `wall_ms` flat (strictly
lower). The ratio is recorded. The owner accepts a sync-bound total such as
0.7x.

## Check C (Boost, not drop)

- Run `scripts/prolly-bench/bench.sh` of the branch with `BIN` = the
  `prolly-bench` of each build, Boost `1_84_0` then `1_85_0`, `RUNS=3`,
  `KS="1 10 100 1000"`, `COMMITS=10`, `PATTERNS="scatter cluster insdel"`,
  `SEED=24`, `SCALE_NS=""`.
- Rows: `mode=whole run=median`.
- For each pattern in scatter, cluster, insdel and K in 1, 10, 100 (9 rows):
  - better iff prolly `cpu_user_s + cpu_sys_s` < flat AND prolly `total_ms` <
    flat;
  - bytes ok iff prolly `new_stored_bytes` <= 2 x flat.
- check C passes iff all 9 rows are better and bytes ok.
- The Phase 24 scale rows are replaced by checks A and B (product code at
  1,000,000 inodes).

## Recorded, not gated

Every other `whole` median row (K = 1,000, the version step), and every insdel
row with its CPU, wall and bytes ratio. They are the input of the insert/delete
record of PMAP-07 (mitigation or written acceptance, plan 26-13).

## Verdict

PMAP-07 passes iff A, B and C pass. The numbers of all three checks are
recorded whatever the verdict.

## Re-runs

A run that crashes or measures the wrong build is repeated, and the reason is
recorded. A finished run is never replaced to change a verdict. A fail goes to
the owner with its rows; nothing is tuned and re-run to pass.
