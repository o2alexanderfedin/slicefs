---
phase: 25-prolly-format-and-layout
plan: 06
subsystem: prolly-bench
tags: [synth, layout, matrix, decision-rules, docker, pre-registration]
requires: [25-05]
provides:
  - "prolly-bench synth: five synthetic maps of N inodes from streams, the D-02 commit mix, full state check"
  - "scripts/prolly-bench/layout.sh: the whole Boost and synthetic matrix (3 layouts x 3 configs), resumable"
  - "scripts/prolly-bench/decide.py: the pre-registered rules as constants, the 1B extrapolation, a self-test"
  - "scripts/docker-linux-test/run.sh prolly-layout: the runner in Docker on disk-backed storage"
affects: [25-07]
requirements-completed: [PMAP-01, PMAP-04]
status: complete
commits: 2
plan_head_before: 84fd9e55ce3f812196981bedddbd3c1e194c005d
plan_head_after: 75c15edd185efc8d08277852ffcd9d79acae6720
actuals:
  tokens: 21700
  tasks: 3
  commits: 2
tech-stack:
  added: []
  patterns: ["sorted-vector overlay with binary search", "affine slot bijection mod 2N with u128 modular inverse", "one process per row with a time limit"]
key-files:
  created:
    - benchmarks/prolly-bench/src/synth.rs
    - scripts/prolly-bench/layout.sh
    - scripts/prolly-bench/decide.py
    - .planning/phases/25-prolly-format-and-layout/runs/gate-25-06.log
  modified:
    - benchmarks/prolly-bench/src/layout.rs
    - benchmarks/prolly-bench/src/main.rs
    - benchmarks/prolly-bench/tests/smoke.rs
    - scripts/prolly-bench/README.md
    - scripts/docker-linux-test/run.sh
key-decisions:
  - "A new runner layout.sh; the Phase 24 bench.sh is unchanged"
  - "Synthetic refcount keys 0x1e || BE u64 slot || 24 zero bytes; the slot of a file's key is an affine bijection mod 2N, so keys land at random leaves while the stream stays in key order"
  - "An inserted refcount key copies a random base slot and sets the last 8 filler bytes to a counter that starts at 1"
  - "decide.py takes --expect-ks (default 100,1000) next to --expect-ns, so a smoke run can use other K values without changing a rule"
---

# Phase 25 Plan 06: Synthetic source, matrix runner and pre-registered decision rules

The synthetic source, the runner of the whole Q-2 and constants matrix, the decision script with a passing self-test, and the Docker case exist and run end to end on smoke data on the host and in Docker. `decide.py` is committed unchanged-from-now (commit `75c15ed`) before any real run.

## What was built

- **Tasks 1 and 2 (commit `cd3d393`).** `synth.rs`: `synth --n N --layout a|b|c --store P [--seed] [--l] [--node-max] [--commits] [--ks] [--patterns]`. Five base maps are sorted streams made on the fly (no collection of entries): inode (all), dir (about 10%), manifest (files), xattr (about 5%), refcounts (2 keys per file, slot space 2N, `perm(x) = (A x + B) mod 2N`, the stream walks the slots and asks the inverse permutation for the owner). Commit patterns `one` (k=1), `mixed` (each k) and `insdel` (each k: k/2 creates at the right edge with 2 refcount inserts and a parent dir update, k-k/2 deletes with xattr and base refcount slots). The overlay (changed versions, dir versions, dead files, dead slots, inserted refcount keys, the next inode and counter) is kept in sorted vectors with binary search; no `BTreeMap` or `HashMap` appears in `synth.rs` (the grep gate is 0). The final check compares the entry count of each map and every overlay key through `Engine::get`. A negative probe (a delete of the wrong key) gave `ok=0` and exit 1, so the check is not vacuous. `layout.rs`: the `build` line inside `Engine::build` (so `layout` mode prints it too), `Engine::get`, a one-pass `Engine::map_counts` (the combined tree is read once, not five times), `Engine::print_check`, and a shared `layout_opts` for `--layout/--l/--node-max`.
- **Task 3 (commit `75c15ed`).** `layout.sh` (bash 3.2 safe): Boost rows (`seed`, `whole` with patterns `scatter insdel` and the next tree, `maps`, then `layout` per layout and config) and synthetic rows (`synth` per size, layout, config); one process per row under `timeout`, own store dir, output in `rows/<source>-<n>-<layout>-<L>-<M>.txt` ending `rc=<code>`; `RESUME=1` skips rows and Boost steps that ended `rc=0`; without `RESUME` the work dirs are cleared first (the Docker volume persists). It writes `layout.csv`, `shape.csv`, `build.csv`, `checks.txt`, `machine.txt` and exits 1 on a failed row or a check that is not `ok=1`. `decide.py`: the constants of the plan, the rows, the layout rule, the constants rule, the 1B extrapolation, `--expect-ns`, `--expect-ks`, exit 2 on `missing row:` or `check failed:`, and `--self-test` with all seven cases of the plan plus extras (CPU guard, two arms far apart, a failed check, two qualifying configs, the extrapolation of a known tree, degenerate trees). `run.sh prolly-layout`: DATA read only, OUT writable, stores on `slicefs-prolly-work`, env passed with `-e`, `/work/run` kept only with `RESUME=1`. README: the section "Layout and constants benchmark (Phase 25)"; the lines about the duplicate inode map now say it was removed in store format 8.

## Verification

`runs/gate-25-06.log` has eight sections, all `rc=0`: `cargo test -p prolly-bench` (6 unit tests, 3 smoke tests including `synth_smoke`), clippy with `-D warnings -D clippy::allow_attributes_without_reason`, `decide.py --self-test` (`self-test ok`), the host smoke of the runner (297 `layout.csv` rows, 27 checks, no failed row), `decide.py` on that smoke output (a `verdict` line), the Docker case `prolly-layout` with `LAYOUT_SMOKE=1` (same counts; run on the disk-backed volume), `bash -n` of both scripts, and `cargo test --workspace` (86 `test result: ok` lines, none failed). The verdict on smoke data means nothing; only that the chain runs.

## Deviations from Plan

**1. [Plan conflict] The smoke K values did not match the required rows.** The plan sets `LAYOUT_SMOKE` to `KS="1 10"` and `SYNTH_KS="10 40"`, but the rules need three Boost K roles (1, medium, large). Fix: smoke `KS="1 10 40"`, and `decide.py` gets `--expect-ks K1,K2` (default `100,1000`, the pre-registered values, stays literal). The smoke call is `decide.py --expect-ns 5000,20000 --expect-ks 10,40`. No rule or threshold changed. **Files:** `layout.sh`, `decide.py`.

**2. [Rule 2 - guard] `synth` refuses a K above N/4** (usage error, exit 2) when a pattern other than `one` runs, and the choice of a live file or slot stops after 4,096 tries with a clean failure. Without it, a small N with the default K could loop for ever. **Commit:** `cd3d393`.

**3. Tasks 1 and 2 are one commit.** The tracer and the pattern task were written and tested together; the tracer gate (the whole `cargo test -p prolly-bench`) passed on the combined state. **Commit:** `cd3d393`.

**4. [Rule 1 - lint] clippy `manual_is_multiple_of`** in `synth.rs`: fixed with `is_multiple_of`. **Commit:** `cd3d393`.

**5. Gate 7 of the plan** (`bash -n scripts/prolly-bench/layout.sh scripts/docker-linux-test/run.sh`) would check only the first file (the second is an argument). The gate runs `bash -n` on each file. The gate log shows filtered output (test and result lines, the runner progress, the verdict) instead of the full cargo output.

**6. Median.** `decide.py` uses the plain median (the mean of the two middle values for an even count), not `median_low` of `bench.sh`. The rules say "median over its commits"; this is recorded in the docstring.

No auth gates. No file under `crates/` changed. No new package.

## Discretion choices (recorded)

- Runner: new `layout.sh`, not more modes in `bench.sh`.
- Row file names use `n=0` for Boost rows (the Boost inode count is not a row key); `decide.py` matches Boost rows by source only.
- `decide.py` reads only `layout.csv`, `shape.csv` and `checks.txt`; F(C) and the 1B extrapolation come from the `shape.csv` rows of phase `base`, summing `levels` and `forced_cuts` over the trees of the 10M base build.
- Degenerate cases (documented in the docstring, not part of the rules): a one-leaf tree has fanout `e`; a fanout of 1 or less is raised to 2; a ratio with a zero denominator is 1 for 0/0 and infinite otherwise.
- A required row for the constants rule is checked only for the chosen layout; the layout rule needs all three layouts at the Dolt config. Every check line present must be `ok=1`, and each used (source, n, layout, config) needs one.
- The time of a synthetic `build` line includes making its streams (the generator is the same in every layout and config); the commit rows measure only the engine.

## Boost refcounts are empty (for 25-07 and the decision)

From 25-05: the product never writes refcounts (`whole` and `seed` never call `increment_refcount`), so the refcount map of every Boost store is empty. In the Boost rows the refcount tree of layouts a and c is empty and layout b has no `0x02` keys, so the Boost rows say nothing about refcounts in a layout. `decide.py` keeps the Boost rows exactly as the pre-registered rules name them (they do not draw a refcount conclusion by themselves), prints a `note` line before the verdict, and says the same in its docstring and in the README. Any statement about layout b (refcounts inside) against layout c (refcounts apart) must rest on the synthetic rows, which carry 2 refcount keys per file. Plan 25-07 must say this in the decision text.

## For plan 25-07

Run, from the phase branch tree, after confirming that `decide.py` is unchanged against commit `75c15ed`:

```
export PATH="$HOME/.cargo/bin:$PATH"
DATA=<Boost data dir with x/boost_1_84_0 and x/boost_1_85_0> OUT=<result dir> \
  SLICEFS_TARGET_VOL=slicefs-target-25-07 \
  scripts/docker-linux-test/run.sh prolly-layout </dev/null     # background; no outer timeout shorter than the rows (ROW_TIMEOUT 5400 s each)
python3 scripts/prolly-bench/decide.py "$OUT" </dev/null
```

Defaults give the pre-registered matrix: Boost KS `1 100 1000` with patterns `scatter insdel`; synthetic sizes `1000000 10000000`, `SYNTH_KS="100 1000"`, 10 commits, seed 25, layouts `a b c`, configs `4096:16384 8192:16384 8192:32768`. `RESUME=1` continues after a failed row (the volume `slicefs-prolly-work` then keeps `/work/run`). Row sizes were not measured at 1M or 10M here (smoke sizes only), so the time per 10M row is unknown; the `build` and `commit` lines of the first rows give it, and `ROW_TIMEOUT` can be raised by env without touching `decide.py`.

## Known Stubs

None.

## Threat Flags

None. T-25-19: `decide.py` is code, committed in `75c15ed` before any real run. T-25-20: `DATA` is mounted `:ro`; only `OUT` and the volume are writable. T-25-21: the runner writes no path, host or user (`machine.txt` holds OS, CPU, memory, file system, rustc, short commit, parameters); the gate log passed the sed of execution rule 8 and the three acceptance greps give 0. T-25-22: one process per row with `timeout`, `RESUME=1` continues.

## Self-Check: PASSED

- Files exist: `synth.rs`, `layout.sh`, `decide.py`, `gate-25-06.log`, this file.
- Commits `cd3d393` and `75c15ed` are ancestors of HEAD on `exec/25-06`.
- Acceptance counts: `NODES_RATIO_MAX = 0.80`, `CONST_NODES_1B_RATIO_MAX = 0.65`, `FORCED_SHARE_MAX = 0.005` in `decide.py`; `prolly-layout)` and `slicefs-prolly-work` in `run.sh`; "Layout and constants benchmark" in the README; no user or home paths in `layout.sh`, `decide.py` or the gate log; gate log has 8 `rc=0`, 0 failed, 1 verdict line.
