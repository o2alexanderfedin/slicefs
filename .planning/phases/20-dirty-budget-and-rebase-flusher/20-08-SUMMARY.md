---
phase: 20-dirty-budget-and-rebase-flusher
plan: 08
subsystem: concurrency
status: complete
tags: [lock-02, lock-06, docs, readme, phase-gate, docker-smoke]

requires:
  - phase: 20-dirty-budget-and-rebase-flusher
    plan: 07
    provides: "P1 runs (runs/s7-flusher.log), dirty_total_stays_bounded_with_parallel_writers"
  - phase: 20-dirty-budget-and-rebase-flusher
    plan: 06
    provides: "the four dirty-data mount options"
provides:
  - "## Phase 20 result in docs/research/multithreaded-request-handling.md"
  - "README part on the dirty-data mount options; matching mount help text"
  - "deferred-items.md with an Open part and a Decided part"
  - "runs/smoke-20-08.log: Docker smoke mount on the final tree"
  - "LOCK-02 and LOCK-06 marked complete on evidence"
affects: [21]

estimate:
  tokens: 60000
  tasks: 2
actuals:
  tokens: 3200      # chars/4 over the added lines of 192ebc0 (11620) and the smoke log (1002); this file is extra
  tasks: 2
  commits: 1

plan_head_before: 2e36e9c2ac8ea1cbb4383ad54a10a8db1a5b4d28
plan_head_after: 192ebc0
commits: 1

tech-stack:
  added: []
  patterns: []

key-files:
  created:
    - .planning/phases/20-dirty-budget-and-rebase-flusher/runs/smoke-20-08.log
    - .planning/phases/20-dirty-budget-and-rebase-flusher/20-08-SUMMARY.md
  modified:
    - docs/research/multithreaded-request-handling.md
    - README.md
    - crates/slicefs-cli/src/cli.rs
    - .planning/research/ARCHITECTURE.md
    - .planning/phases/20-dirty-budget-and-rebase-flusher/deferred-items.md
    - .planning/REQUIREMENTS.md

decisions:
  - "The mount help text says that the budget is memory the daemon may use, that the size options have no upper bound, and that --dirty-limit is for overlays (a stream can hold up to a fixed 64 MiB reorder window)"
  - "LOCK-02 and LOCK-06 are marked complete: every listed check passed (orchestrator rule for this plan)"

metrics:
  duration: "about 75 min for 20-08 (one gate run of 1244 s for the workspace tests, one Docker smoke)"
  completed: 2026-11-23
---

# Phase 20 Plan 08: Phase close (docs, marks, gate) Summary

The research doc has a `## Phase 20 result` section with the S7 numbers of 20-07 and a note that the empty-pass fix came after them. The README and the mount help text document the four dirty-data options. The stale take-out rows of the architecture text now match the code. The deferred list has an Open part. The phase gate is green, a Docker smoke mount on the final tree passed, and LOCK-02 and LOCK-06 are marked complete.

## Before this plan: the empty-pass fix (orchestrator job)

| Commit | Subject |
|---|---|
| 4d8ac3d | fix(20): no flusher kick after a pass with no candidate |
| 2e36e9c | docs(20): note of the empty-pass fix; deferred item fixed in 4d8ac3d |

- `FlushCtl::empty_pass` (atomic). Each pass stores `steps == 0` at its normal end. `throttle_write` kicks when the total is at or above the budget, or at half the budget or more and `empty_pass` is false. No new lock. The throttle wait and its stop exit did not change.
- Test `a_hot_file_above_half_the_budget_does_not_kick_after_an_empty_pass`: one hot file, 200 writes with the total between half the budget and the budget, pass interval 60 s, hot window 1 h. Bound: at most 2 passes. Red run (on 9659188 plus the test): `passes=200`, failed at the bound assert in 0.11 s. Green: `passes=1`. A write at the full budget still kicks (checked at the end of the test).
- Details: `20-FIX-empty-pass.md`.

## Tasks

| Task | Name | Commit | Files |
|---|---|---|---|
| 1 | Phase 20 result in the research doc; README mount options; architecture text; deferred list | 192ebc0 | multithreaded-request-handling.md, README.md, cli.rs, ARCHITECTURE.md, deferred-items.md |
| 2 | Requirement marks on evidence; phase gate with a Docker smoke mount | docs commit | REQUIREMENTS.md, runs/smoke-20-08.log, this file |

## Task 1

- Research doc: `## Phase 20 result` between `## Phase 19 result` and `## Conclusion`. It has the design in short, the mount options, the test names of criteria 1-3, a table for criterion 4 (P_old 260, P0 267, the cap rule, CAP_MIB 320, P1 257/278/287 with a margin of 33 MiB, the flusher steps, drops 0, throttle waits and 0 timeouts, the slowest write, the largest throttle wait, the largest commit gate wait), the machine and system line, the empty-pass note, the limits (one dispatch thread until Phase 21; the 64 MiB reorder window) and a pointer to the deferred list.
- `### S7 result`: one sentence after the first paragraph. The recorded numbers did not change.
- README: `### Dirty data (mount options)` after the Quick start. A table of the four options with defaults, then the units and checks, the triggers, what a writer over the budget does, the hot rule, and the overlay / stream window note.
- Help text (`cli.rs`, doc comments only): `--dirty-budget` now says "This is memory that the daemon may use" and "no upper bound". `--dirty-limit` now says that it is for a file changed in place (an overlay), and that a stream can also hold out-of-order writes up to a fixed 64 MiB window. `slicefs mount --help` was compared with the README text.
- ARCHITECTURE.md: the row "2-7 Open" became "2-5 Landed (Phase 19, Phase 20)" and "6-7 Open"; the `keep_budget` row now says "Done" and names `pay_budget`.
- deferred-items.md: it existed already (the plan said "new"). It now has `## Open` (commit timer at unmount, harness time, reorder window, kept-cell finalize retry, delay curve, the Phase 19 pointer, and a flaky chunk-store test) and `## Decided (closed for Phase 20)` (no upper bound; the empty pass, fixed in 4d8ac3d).

Verify: `## Phase 20 result` count 1; licensing or home path words in the section 0; `--dirty-*` lines in README 5; licensing or home path words in the added lines (README, research doc, ARCHITECTURE, cli.rs, deferred list) 0; `pay_budget` in ARCHITECTURE 2; `## Open` 1.

## Task 2: the phase gate

Tree: 192ebc0 (the gate ran on the working tree, and that tree was committed after the workspace tests ended, with no other change). Machine: macOS host, debug.

| Command | Exit | Result |
|---|---|---|
| `cargo test --workspace` | 0 | 69 `test result: ok` lines, 0 failed, 1244 s; `flusher_tests` 23 passed; `stress_harness` 6 passed in 9.83 s |
| `cargo check --workspace --all-targets` | 0 | |
| `cargo check --workspace --all-targets --features slicefs-cli/lock-stats` | 0 | |
| `cargo check -p slicefs-cli --all-targets --features deadlock-detect` | 0 | |
| `cargo test -p slicefs-cli --features deadlock-detect --test stress_harness serialize_off_target` | 0 | 1 passed |
| `cargo test -p slicefs-cli --features deadlock-detect --test flusher_tests` | 0 | 23 passed |
| `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report` | 0 | 1 passed (no new `Site` label in Phase 20 code after 20-07) |
| `cargo test -p chunk-store two_cells_panic_even_in_key_order` | 0 | 1 passed |
| `cargo test -p slicefs-cli --test no_take_out` | 0 | 4 passed |

No "deadlock detected", "lock rank violation" or "test result: FAILED" in the gate log.

## Task 2: Docker smoke mount

- `docker build` from the same Dockerfile (orchestrator correction; the image ID was not compared). `rustc -V` in the image: `rustc 1.100.0 (0ef1bc386 2026-11-09)`. This is the same as the `RUSTC =` line of `runs/s7-wave0-NOTES.md`. Image ID, for reference only: `sha256:0e6232ea9854206cb698661e86a045f4e3f093a743e0abbb75043da25db6dc35`.
- Tree: `git archive HEAD` of 192ebc0 (porcelain 0 lines) as a tar stream, target volume `slicefs-target-p20`, debug build, wait until the container is 45 s old, then `scripts/docker-linux-test/mt-linux.sh /target/debug/slicefs`.
- `Linux 89a9dd633256 7.2.12-orbstack-00394-gbecdd6130068 #1 SMP PREEMPT Thu Oct 22 22:17:17 UTC 2026 x86_64 GNU/Linux`
- read, echo-write, cp-3MB, cp-again, readback, mkdir-mv, chmod, truncate, xattr, rm: ok; `unmount: ok`; `daemon-exit: ok`; script exit 0. Host load 13.96 / 21.49 / 39.92 at the start.
- Log: `runs/smoke-20-08.log`.

## Task 2: the cap did not move

The plan's awk on `runs/s7-wave0.log` prints `runs=6 modes=2 pm=267 spread=20 r=40 cap=320`. `stress_harness.rs` has `const CAP_MIB: u64 = 320`. `runs/s7-wave0.log` and the notes did not change since `e4bc8d3b9ac1a85ec2d9455d1f7eefc0bdf0c034`, which is the 20-01 Task 3 sha in the 20-01 SUMMARY.

## Requirement marks

**LOCK-02: marked `[x]`, traceability Complete.** Evidence:
- `no_take_out` green in the gate (4 passed: `only_four_functions_remove_a_cell`, `the_budget_step_is_own_file_only`, `the_flusher_takes_no_cell_out`, `header_parser_finds_names`).
- It was red before: in 20-01, `the_budget_step_is_own_file_only` failed on the old tree (`the old cross-file budget function is back at lines [2704]`), and its guard mutation (a fifth cell removal) failed `only_four_functions_remove_a_cell`. In 20-02, a planted cell removal in `flusher.rs` failed `the_flusher_takes_no_cell_out`.
- The rank test `two_cells_panic_even_in_key_order` green.
- The flusher tests ran green in debug (23 passed): the flusher thread runs with `require_gate(true)`, so a cell taken outside a shared gate hold panics.

**LOCK-06: marked `[x]`, traceability Complete.** Evidence:
- Every `flusher_tests` test green: 23 passed in debug and 23 passed with `deadlock-detect` (criteria 2 and 3, including `all_writers_throttled_while_a_commit_waits_flusher_paused` and `..._flusher_in_a_step`).
- The 20-03 smoke mount (aa853f2) and the 20-08 smoke mount (192ebc0) passed (`unmount: ok`, `daemon-exit: ok`).
- The 20-07 P1 runs passed under CAP_MIB: VmHWM 257, 278, 287 MiB, cap 320, `test result: ok` 3 times, with rustc 1.100.0 equal to the 20-01 `RUSTC =` line (the image was rebuilt, as the orchestrator correction allows).
- The cap check above passed.
- Note: the P1 runs are on efc1e9b, before the empty-pass fix (4d8ac3d). They were not run again (orchestrator rule). The fix only removes flusher kicks below the budget; the doc and this file say so.

## Deviations from Plan

### Auto-fixed Issues

None.

### Other deviations

**1. Docker image rebuilt; image ID not compared.** The plan says not to run `docker build` and to compare the image ID. The orchestrator correction replaces this: rustc in the image equals the `RUSTC =` line. The image ID is in the log for reference only.

**2. `cli.rs` help text changed (a docs-only change in a source file).** The orchestrator asked the help text to name the overlay limit, the 64 MiB window, "no upper bound" and "memory the daemon may use". The old text had none of these. The workspace tests passed on the tree with this change before the commit.

**3. deferred-items.md was restructured, not created.** The file existed with five items and no `## Open` heading. The items were kept, moved into `## Open` or `## Decided (closed for Phase 20)`, and the plan's items were added.

**4. Extra gate command.** `cargo test -p slicefs-cli --test no_take_out` was added to the gate, because the LOCK-02 mark depends on it.

**5. One extra Open item: a flaky chunk-store test.** In the first workspace run of the empty-pass fix, `the_idle_merge_leaves_at_most_two_runs_with_the_same_answers` (`index_crash_tests.rs:516`) failed: the idle merge (50 ms) ran before the tombstone assert. Three runs alone and the next two workspace runs passed. No chunk-store code changed in Phase 20. It is in the deferred list.

**6. The research doc Phase 20 section has more test names than the plan lists** (the shutdown test under criterion 3, and the empty-pass test). Both are in `flusher_tests.rs`.

## Open points (for a decision)

- **Harness time.** `stress_harness` took 12.36 s in the empty-pass workspace run and 9.83 s in the gate run (limit 10 s). The host load was high. No quiet machine was available to check it again. It stays open.
- **P1 margin.** P1 is 20 MiB above P0 and the margin to the cap is 33 MiB (about one spread). One `MALLOC_ARENA_MAX=2` flusher run would show if the extra memory is arenas. Not run.
- **S7 after the fix.** The S7 numbers are from before the empty-pass fix. The fix should lower the flusher CPU, and it does not change a step or a wait at the budget. A new S7 run is needed only if the owner wants numbers on the final code.
- **Commit timer at unmount, reorder window, kept-cell finalize retry, delay curve, Phase 19 items:** open in `deferred-items.md`.

## Threat model check

- T-20-SC: no new crate.
- T-20-24: the grep gates on the new doc section and on all added lines gave 0.
- T-20-25: each mark has its named tests and runs above; the gate ran on the final code tree.
- T-20-26: the README text was compared with `slicefs mount --help` output.

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: docs/research/multithreaded-request-handling.md (`## Phase 20 result`), README.md (`--dirty-budget`), deferred-items.md (`## Open`), runs/smoke-20-08.log, 20-FIX-empty-pass.md
- FOUND: 4d8ac3d, 2e36e9c and 192ebc0 in HEAD
- `commits: 1` is measured with `git rev-list --count 2e36e9c..HEAD` before the docs commit of this file.
