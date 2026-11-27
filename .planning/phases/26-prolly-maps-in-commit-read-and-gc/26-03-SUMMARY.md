---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 03
subsystem: bench
tags: [prolly-bench, pmap07, cleanup, in-05]
requires: ["26-01"]
provides:
  - "prolly-bench without the readers of flat metadata maps"
  - "bench.sh ready for PMAP-07 check C (whole and scale only; BIN may be the v2.3.0 binary)"
  - "IN-05: --l default 8192"
affects: [26-04, 26-13]
tech-stack:
  added: []
  patterns: ["retire, do not port, the comparison modes of closed phases; the v2.3.0 tag keeps them"]
key-files:
  created:
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-03.log
  modified:
    - benchmarks/prolly-bench/src/main.rs
    - benchmarks/prolly-bench/src/replay.rs
    - benchmarks/prolly-bench/src/layout.rs
    - benchmarks/prolly-bench/tests/smoke.rs
    - scripts/prolly-bench/bench.sh
    - scripts/prolly-bench/layout.sh
    - scripts/prolly-bench/README.md
    - scripts/docker-linux-test/run.sh
key-decisions:
  - "Retire, not port: batches, flat, prolly, maps and layout are removed; a port would need new record formats for modes whose job (the Phase 24/25 one-map comparisons) is done and recorded"
  - "Test-only code in the layout engine (iter_map, ns_native) is cfg(test), not deleted, so its model test stays"
status: complete
metrics:
  duration: "about 1 h 15 min"
  completed: 2026-11-27
actuals:
  tokens: 30000
  tasks: 2
  commits: 2
plan_head_before: 9b269853778bee00461ee427e52246620bdddb65
plan_head_after: 58b4d57da3b064a55b9407125fb0e97695f276d9
---

# Phase 26 Plan 03: Retire the flat-store readers of prolly-bench Summary

No prolly-bench mode reads the flat metadata maps of a product store any more, so the first tree conversion (plan 26-04) cannot break `smoke_end_to_end`; `--l` defaults to 8192 (IN-05); `bench.sh` runs `whole` and the synthetic `scale` rows only.

`commits: 2` counts the two code commits (c32937b, 58b4d57); the SUMMARY commit that follows is not in the count.

## What was built

- `main.rs`: modes are `gen`, `seed`, `whole`, `scale`, `synth`. `batches`, `flat`, `prolly`, `maps` and `layout` fall to the unknown-mode branch (exit 2). The dead helper `key_of_hex` is removed.
- `replay.rs`: only `scale` and what it needs (`Measured`, `measure`, `Pieces`, `intern_flat`, `pkey`, `synth_value`). `MapStream`, `Digest`, the roots and batch readers and the `batches`, `flat` and `prolly` modes are gone.
- `layout.rs`: the engine for `synth` stays (`Ctx`, `Engine`, `Kv`, `KvStream`, `MapEdit`, `MapKind`, `layout_opts`). `maps`, `layout`, the base-file readers, `write_batch` and `raw_of`/`native_of`/`rec_cmp` are gone. `--l` default is 8192 (`a.num("l", Some(8192))`; no `Some(4096)` left).
- `smoke.rs`: `smoke_end_to_end` runs gen, seed, whole and the `scale` loop. `bad_arguments_exit_2` checks that each retired mode name exits 2, and adds `whole` bad-flag cases. `synth_smoke` also asserts the default `l=8192 node_max=16384`.
- `bench.sh`: per run the copy of the seeded store, `whole`, and the `scale` rows. The `results.csv` header is unchanged; `entries_changed` is `NA` (the `whole` line has none). `batches.csv` and the batch argument of `csv` are gone. `BIN` may be any build with `seed`, `whole` and `scale`.
- `layout.sh`: synthetic rows only, no argument; the Boost steps and the data-dir checks are gone.
- `run.sh`: `prolly-layout` no longer needs `DATA` (mounted read only only when set).
- `README.md`: retired-modes note, mode table, column tables and smoke counts follow the new output.

## Decisions

- Retire, not port (plan discretion). The retired modes stay in tag `v2.3.0`.
- `iter_map` and `ns_native` of the layout engine are used only by the unit test model check, so they are `#[cfg(test)]` instead of being deleted (clippy `-D warnings` clean).
- `prolly-bench-smoke` of `run.sh` is unchanged as the plan says. With no `flat` or `prolly` rows its line "checks ok" prints 0.

## Verification

All in `runs/gate-26-03.log`, all `rc=0`:

- `cargo test -p prolly-bench`: 5 unit tests and 3 smoke tests pass; `cargo +stable clippy --locked -p prolly-bench --all-targets -- -D warnings` is clean for this crate.
- `bench.sh` smoke on the host (`SMOKE=1 RUNS=1`): rc 0; 8 `whole` median rows; 14 run rows and 8 median rows in `results.csv`, 16 rows in `scale.csv`.
- `layout.sh` smoke on the host (`LAYOUT_SMOKE=1`): rc 0; 18 rows, all 18 checks `ok=1`.
- `cargo test --workspace`: rc 0.
- Acceptance greps: `Some(8192)` 1, `Some(4096)` 0, retired `Some("...")` arms in `main.rs` 0, `"batches"`/`"maps"` in `smoke.rs` 1, `step batches`/`BT=`/`batches.csv` in `bench.sh` 0, `step whole` 1, ` maps --store`/`layout --maps` in `layout.sh` 0.

The first `cargo test --workspace` run (before two comment and blank-line fixes in `replay.rs`) was also rc 0; the gate log holds the second run on the final tree. The 200 ms `retry_tests` timing check did not fail in either run.

## Deviations from Plan

None. The plan's file list was enough (no file outside `files_modified` was edited).

## Notes for later plans

- `decide.py` (Phase 25) still asks for the Boost rows, so it cannot give a verdict on a fresh `layout.sh` run; it reads the Phase 25 data only. The README says so. The plan kept `decide.py` as it is.
- Plan 26-13 can run `bench.sh` with `BIN` set to the `v2.3.0` binary and to the branch binary; the `whole` commit lines of the branch binary must keep the `commit mode=whole` shape.

## Self-Check: PASSED

- Commits c32937b and 58b4d57 are on `exec/26-03`.
- `runs/gate-26-03.log` exists, has four `rc=0` lines and no `rc=[1-9]` line.
