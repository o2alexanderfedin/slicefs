# Phase 20 deferred items

## Open

- **Harness time near the 10 s limit (found in 20-05).** Five `stress_harness` runs took 9.38-10.45 s at a load average of about 19; the runs alone were under 10 s. In 20-07 two warm runs alone took 11.27 s and 10.71 s at a load average of about 75. The harness starts no flusher; the new work per write is one clock read. Decision (orchestrator): accept; the load caused the spread. Check again on a quiet machine. In the 20-08 workspace runs it took 12.36 s (first empty-pass fix run) , 9.83 s (first gate run) and 12.01 s (gate run on e44d57d) at a high host load.
- **`--dirty-limit` and the stream reorder window (found in 20-06, older than Phase 20).** A stream can hold out-of-order pending writes up to `REORDER_WINDOW` (fixed 64 MiB), whatever `--dirty-limit` is. 20-08 documents it in the help text and the README. Tying the window to the limit is a later item.
- **Finalize retry of a kept cell.** After a failed last release the cell is kept (Phase 19 WR-04 fix). The flusher steps such a cell, but it does not retire it. The cell is stored only by a later open and release, or by destroy. Since c0d10d0 a failed flusher step of such a cell puts it in a back-off (at most 30 s), so a lasting store error costs one try and one log line per back-off.
- **A delay curve instead of one wait threshold** (research Alternatives; reversible). Today a writer waits only at or above the budget. A delay that grows between half the budget and the budget is an option.
- **Phase 19 open items.** See `.planning/phases/19-fine-grained-locks/deferred-items.md` (Open). Phase 20 did not change them. Three of them name Phase 20 as owner and stay open: the span probe of the long `file_cell` holds, the performance effect of the wait of other writers behind a writer at the hard cap, and the retry of a kept cell (the item above).
- **Flaky test `the_idle_merge_leaves_at_most_two_runs_with_the_same_answers` (chunk-store, found in the empty-pass fix).** In one workspace run under host load, the assert `run_tombstones > 0` at `index_crash_tests.rs:516` failed: the idle merge (50 ms) had already merged the runs. Three runs alone and all later workspace runs passed. Not related to Phase 20 (no chunk-store change). Owner: a later fix of the test (it depends on time).

## Decided (closed for Phase 20)

- **Commit timer during unmount (found in 20-03, older than Phase 20).** The code review (IN-01) found no data loss, but one extra commit could run after `shutdown_wal` and `content.close`. Fixed in b912aed: `shutdown()` marks the file system as shut down first, and the timer's commit checks the mark with the exclusive gate held and then does no commit.

- **No upper bound on `--dirty-budget` / `--dirty-limit` (found in 20-06).** Decision (orchestrator, reversible while there are no deployments): no upper bound. Overflow is safe (`saturating_add`). The help text and the README say that the budget is memory the daemon may use, and that there is no upper bound.
- **Empty flusher passes from the kick (found in 20-05, measured in 20-07).** Fixed in 4d8ac3d and e44d57d: after a pass that found no candidate with the total at half the budget or more, a write below the budget does not wake the flusher. See `20-FIX-empty-pass.md`.
