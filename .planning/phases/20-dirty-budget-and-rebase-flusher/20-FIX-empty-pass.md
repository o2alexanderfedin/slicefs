# Phase 20 fix: no flusher kick after a pass with no candidate

**Commit:** 4d8ac3d (`fix(20): no flusher kick after a pass with no candidate`)
**Decision:** orchestrator, after 20-07 (reversible).

## What

- `FlushCtl` has a new atomic `empty_pass`. At the normal end of each pass, `flush_pass` stores `steps == 0` in it (the pass tried no step, so it found no candidate).
- `throttle_write` kicks the flusher when the total is at or above the budget, or when the total is at or above half the budget and `empty_pass` is false.
- No new lock. The kick check is one more atomic load. The throttle wait and its stop exit did not change.

## Why

In the three 20-07 S7 runs, 98.6 % to 99.7 % of the flusher passes found no candidate (up to about 900 passes per second). Each write that found the total at or above half the budget kicked the flusher, also when the only dirty file was hot. Below the budget a hot file is not a candidate, so the pass did nothing.

What can make a candidate again:
- Time: a hot file becomes quiet after the hot window, or a file becomes idle or old. The timed pass (every `FLUSH_PASS`, 100 ms) sees this and sets `empty_pass` again.
- The total reaches the budget: then hot files are candidates. A write at the budget always kicks.
- A pass that tries a step clears `empty_pass`.

A write to a hot file cannot make another file quiet, so a kick below the budget after an empty pass finds no candidate.

## Test

`a_hot_file_above_half_the_budget_does_not_kick_after_an_empty_pass` (`crates/slicefs-cli/tests/flusher_tests.rs`). One hot overlay file (16 dirty pieces of 32 KiB) keeps the total between half the budget and the budget. The pass interval is 60 s and the hot window is one hour, so the count does not depend on the load. 200 writes; each one is checked to be in that band. Bound: at most 2 passes (a write that kicks during the first pass can cause one more). Also: no step and no throttle wait. Then the budget is set to the total, and one throttled write must go in and raise the pass count (a kick at the full budget).

- Red run (tree 9659188 plus the test): `200 flusher passes for 200 writes of one hot file (at most 2)`, `FlusherStats { passes: 200, steps: 0, ... }`, failed in 0.11 s.
- Green run (with the fix): `passes=1`, test ok in 0.20 s.
- `flusher_tests`: 23 passed (debug), and 23 passed with `--features deadlock-detect`.
- `cargo test --workspace`: exit 0 before the commit (69 `test result: ok` lines; `flusher_tests` 23 passed in 6.84 s). The first workspace run failed in a chunk-store test that this fix does not touch (`the_idle_merge_leaves_at_most_two_runs_with_the_same_answers`, `index_crash_tests.rs:516`, an idle merge that depends on time). Three runs of it alone and the second workspace run passed. It is in `deferred-items.md`.

The Docker S7 measurement was not run again for this fix. The 20-08 Docker smoke mount runs on the final tree.
