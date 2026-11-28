---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 14
subsystem: metadata
tags: [prolly, commit-counters, pmap-03, pmap-07]
requires: ["26-04"]
provides:
  - "DictMetadataStore::last_commit_stats() -> CommitStats { nodes_read, nodes_written, bytes_written, cache_hits, presync_ms, trees: Vec<TreeStats> }"
  - "TreeStats { name, height, entries, apply: prolly::ApplyStats } per map tree (manifest_data now)"
  - "commit_counter_tests: criterion 2 on the manifest tree at 2,000 and 40,000 files; no-change commit keeps the root"
  - "PMAP-07 report nodes() and the bench whole commit line read the counters"
affects: [26-07, 26-08, 26-09, 26-11]
key-files:
  created:
    - crates/metadata/tests/commit_counter_tests.rs
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-14.log
  modified:
    - crates/metadata/src/store.rs
    - crates/metadata/tests/support/pmap07_report.rs
    - benchmarks/prolly-bench/src/whole.rs
    - benchmarks/prolly-bench/src/main.rs
key-decisions:
  - "Counter test sizes 2,000 and 40,000 files; bounds written <= 2h+1, read <= 3h, work growth <= height ratio + 2 (not loosened; passed first time)"
  - "nodes_written and bytes_written include the 197-byte root record"
  - "The stats lock last_stats is a Rank::Store leaf, untimed, set once at the end of a successful commit; it is in the lock-order comment"
  - "presync_ms = wall time of begin_commit + finish_inner up to the object sync; the gap between the two calls (caller time) is not in it"
status: complete
metrics:
  duration: "about 1 h"
  completed: 2026-11-28
actuals:
  tokens: 14000
  tasks: 2
  commits: 1
plan_head_before: 4d63e864942143cacf23d6b213b4cf57f2fc1db8
plan_head_after: fdf809287d321c70dee9039bf8595d8079346791
---

# Phase 26 Plan 14: Commit counters Summary

Each commit now reports its tree work (`last_commit_stats`), and the manifest tree meets ROADMAP criterion 2: a one-change commit reads and writes nodes bounded by the tree height at two store sizes, and a commit with no change keeps the root and writes no tree node.

## What was done

- `store.rs`: `TreeStats`, `CommitStats`, the `last_stats` leaf lock, `last_commit_stats()`. `finish_inner` keeps the `ApplyStats` that `apply_batch` returns, reads `prolly::height` and `prolly::count` of the new root, and sets the stats at the end of a successful commit (a failed commit leaves the last stats unchanged).
- `commit_counter_tests.rs`: stores built with `create_inode`, `link`, `set_manifest`; `one_change_manifest_commit_grows_with_height` (needs h_small < h_large) and `a_commit_without_change_keeps_the_root`. About 10 s in a debug build.
- `pmap07_report.rs`: `nodes` returns the counters. The `PMAP07_N=2000 PMAP07_SAMPLES=11` smoke prints 11 `sample` lines with numbers, no `NA`.
- Bench `whole`: the commit line has `nodes_read=` and `nodes_written=`; `serialized_bytes` = flat map fields + `bytes_written`.

## Deviations from Plan

None - plan executed as written. One note: the first gate run was stopped by process ID and restarted after review fixes (presync span, product-API test setup), so the gate log describes the committed tree.

## Notes for later plans (26-07 to 26-11)

- `CommitStats` counts only the applies of the commit. Edits that a flush moved to a tree before the commit (above `TREE_FLUSH`) are not in it.
- `height` and `entries` cost two root loads per tree per commit; they are not in `nodes_read`.
- Each converted tree pushes one `TreeStats` in `finish_inner` and its apply stats go into the sums (the sums are built from `trees`).
- Bench `serialized_bytes()` no longer includes the root record; it comes from `bytes_written`.
- The PMAP-07 bench one-change commit shows `nodes_read=0 nodes_written=1` today: the edit is not a manifest change, and the other four maps are still flat until plans 26-07 to 26-09. The numbers become meaningful as the trees land.
- Unit tests in `store.rs` still pass with the stats field added to both constructors (`new` and `load_from_root`).

## Known Stubs

None.

## Self-Check: PASSED

Files exist; commit `fdf8092` is in the branch; gate log has 7 sections with rc=0 and no rc=[1-9].
