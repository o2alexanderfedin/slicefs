---
phase: 16-chunk-index-benchmark
plan: 01
subsystem: benchmarks
tags: [chunk-store, index, benchmark, rss, recovery]
status: complete

requires: []
provides:
  - benchmarks/index-bench bin crate with create, open, lookup, recover, run subcommands
  - Backend and Store traits (backend.rs) as the seam for Phase 17 index backends
  - Stable key=value output contract per step, and markdown summary tables from run
affects: [16-02, 16-03, 17-on-disk-chunk-index]

actuals:
  tokens: 13505
  tasks: 3
  commits: 3
plan_head_before: 07e43346337c2b4a70774530e406022c0058cbf5
plan_head_after: 554e5a40141adf6fd40faac6d919fc55ebdc2f03

tech-stack:
  added: []
  patterns:
    - one fresh process per measured step (run starts current_exe for each step)
    - steps exit with process::exit after printing, without a drop of the store
    - marker file guards every step except create

key-files:
  created:
    - benchmarks/index-bench/Cargo.toml
    - benchmarks/index-bench/src/main.rs
    - benchmarks/index-bench/src/backend.rs
    - benchmarks/index-bench/tests/cli_tests.rs
  modified:
    - Cargo.toml
    - Cargo.lock

key-decisions:
  - "recover defaults to --packs 1 when the flag is absent"
  - "run --drop-caches also drops caches before each recover run, so Extra s compares cold to cold"
  - "linux_rss_bytes is compiled on every OS (allow(dead_code) on macOS) so a macOS build type-checks the Linux path"

duration: 10min
completed: 2026-11-18
---

# Phase 16 Plan 01: index-bench tool Summary

**`benchmarks/index-bench` is a bin crate that runs each step in its own process. It makes 8-byte-object chunk stores and measures open time and peak/steady RSS per object, contains() hit/miss and put() dedup-hit throughput, and recovery open time and scan rate with one or all .idx files removed. All store calls go through a Backend trait, so Phase 17 can plug in a new index.**

## Performance

- Duration: about 10 min of execution (plus orientation)
- Tasks: 3 of 3
- Files: 4 created, 2 modified

## Accomplishments

- `create` builds N unique objects through the public ChunkStore API, closes the store (every pack sealed with an .idx), and writes `index-bench.txt`. It refuses a directory that is not empty.
- `open` reports wall and CPU time, `ru_maxrss` before and after the open, current RSS (mach `task_info` on macOS, `/proc/self/statm` on Linux), and peak and steady bytes per object. Then it exits without a drop.
- `lookup` makes a fixed-seed splitmix64 sample of hits (below N) and misses (N and above) before any timing. It fails if a hit is missing, a miss is found, or the object count changes.
- `recover` moves the target .idx files to `D/bench-saved`, times the scan open, and labels each pack sealed or active. Afterwards it keeps each rebuilt idx only when it is byte-identical to the saved copy, and otherwise moves the copy back. It restores on the error path too.
- `run` starts create (when needed), one warm-up open, R opens, lookup, then R recovers for each scope (one, all) and mode (rw, ro), then an open check. It prints Store, Open, Lookups and Recovery tables with an OS column and never prints the D path.

## Task Commits

1. **Task 1: tracer run -> create -> open:** `51ab6e3` (feat)
2. **Task 2: lookup step:** `4715e94` (feat)
3. **Task 3: recover step:** `554e5a4` (feat)

## Verification

- `cargo test -q --locked -p index-bench`: 11 passed, about 2.5 s test time (12 s with the build).
- `cargo clippy -q --locked -p index-bench --all-targets --no-deps -- -D warnings`: clean.
- Plan verify commands: `run --objects 20000 --reps 1 | grep -c '^step=open'` gives 2. `create --objects 5000 --pack-limit 65536` then `recover --packs all | grep -c '^step=recover'` gives 1.
- Acceptance greps: workspace entry 1, Cargo.lock entry 1, `cargo tree -e normal --depth 1` 3 lines, `impl Backend for ChunkStoreBackend` 1, `publish = false` 1, `fn splitmix64` 1, `bench-saved` in backend.rs 1 (the SAVED_DIR constant), `fn drop_index` 2 (trait default and impl).
- `cargo build -q --locked --workspace`: passes. The Cargo.lock diff is only the 9-line index-bench block.
- After every recover, the recover tests check that the pack and idx files are the same as before in name and size, that there is one idx per pack, that `bench-saved` is gone, and that `open` reports N objects. In every case the rebuilt idx files were byte-identical (`idx_identical=yes`).

## TDD Gate Compliance

Tasks 2 and 3 are `tdd="true"`. The tests were written first and run, and they failed for the planned reason:
- Task 2 RED: `cargo test -p index-bench --test cli_tests` gave 3 failures. Two failed with `index-bench: unknown subcommand lookup`. `run_creates_and_opens_a_small_store` failed on the new `step=lookup` assertion (line 50). The other 4 passed.
- Task 3 RED: 5 failures. The recover tests failed with `unknown subcommand recover`, and the run test failed on the new `step=recover` count assertion (line 53).
- GREEN: in both tasks the tests and the implementation went into one commit (see Deviations).

## Deviations from Plan

1. **[Coordinator rule] No separate RED commits.** The coordinator said to gate each commit on the test exit status, so tasks 2 and 3 each have one green commit. The RED runs are recorded above. The `tdd-red-evidence` classifier was not run: tdd.md lists no adapter for cargo test output.
2. **[Rule 3 - Blocking] Clippy with `--no-deps`.** `cargo clippy -p index-bench -- -D warnings` fails on a `type_complexity` lint that was already in `crates/chunk-store/src/lib.rs:286`, which is out of scope. The index-bench crate is clean with `--no-deps`.
3. **[Rule 1 - Lint] `n.is_multiple_of(1_000_000)`** in place of `n % 1_000_000 == 0`, as clippy `manual_is_multiple_of` asks. This needs Rust 1.87 or later. CI uses `rust:1-bookworm` (latest stable) on Linux and the runner's stable on macOS.
4. **[Small addition] More tests than the plan lists:** bad arguments exit with code 2, lookup and recover refuse a directory without the marker, recover refuses a store with saved files left in `bench-saved`, and run does not print the store path. With `--drop-caches`, run also drops caches before each recover run. This is not in the plan, and the Recovery table has no cache column, so the coordinator should confirm it or remove it.
5. **[Environment] New worktree.** The original agent worktree was removed during the session. The work was done in a new worktree at `.../scratchpad/wt-16` on branch `feature/16-index-bench`, created from 07e4334 as the coordinator asked. The branch is not in the `agent-*` namespace. The coordinator named it, so the commit checks only made sure that HEAD was this branch and not a protected or detached HEAD.

## Known Limits

- The Linux-only code was not compiled or run on this machine (only the x86_64-apple-darwin target is installed). This is the `#[cfg(not(target_os = "macos"))]` `current_rss_bytes` wrapper and the `drop_caches` write. The Linux RSS helper (`linux_rss_bytes`) and `drop_caches` are compiled on macOS too, so they are type-checked. CI on Linux will be the first real run of the tests there.
- `create` sums only `D/packs` for `disk_bytes`, as the plan says. A Phase 17 backend that keeps its index outside `packs/` will report a Disk B/object that is too low.
- The sample numbers from the debug build (20k objects) are only smoke checks. The 1M, 10M and 100M measurements are plan 16-02.

## Next Phase Readiness

Plan 16-02 can use `cargo run --release -p index-bench -- run --dir <scratch> --objects 1000000` and document the key=value contract that this plan defines.

## Self-Check: PASSED

- FOUND: benchmarks/index-bench/Cargo.toml, src/main.rs (820 lines), src/backend.rs, tests/cli_tests.rs (250 lines)
- FOUND commits: 51ab6e3, 4715e94, 554e5a4 (ancestors of HEAD)
