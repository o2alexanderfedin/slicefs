---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 02
subsystem: prolly
tags: [prolly, node-cache, range, visit, pmap-03, pmap-06]
requires: ["26-01"]
provides:
  - "NodeCache (fixed byte cap, LRU) and Ctx"
  - "get_in, iter_in, apply_in; ApplyStats::cache_hits"
  - "range, count, height (root only), visit with node IDs"
affects: [26-03, 26-04, 26-06]
tech-stack:
  added: []
  patterns: ["every node load through Ctx; checks run on hits too", "insert into the cache only after all checks pass"]
key-files:
  created:
    - crates/prolly/src/cache.rs
    - crates/prolly/src/range.rs
    - crates/prolly/src/visit.rs
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-02.log
  modified:
    - crates/prolly/src/node.rs
    - crates/prolly/src/tree.rs
    - crates/prolly/src/apply.rs
    - crates/prolly/src/lib.rs
    - crates/prolly/src/tests.rs
    - crates/metadata/src/store.rs
key-decisions:
  - "LRU by a tick (BTreeMap tick to ObjectId); charge = encoded length + 64 per entry + 128; one Mutex at Rank::Store, held for map operations only"
  - "A cache hit is checked against the asking config (node bytes, max key, max entry stored with the node), so an apply of a small config still refuses a node a reader cached"
  - "The parent-entry and level checks run before the insert, so a node that fails them is never cached"
  - "range is a Cursor with an end bound and a seek start; Range is a thin wrapper that adds max_nodes_held"
status: complete
metrics:
  duration: "about 1 h"
  completed: 2026-11-27
actuals:
  tokens: 9500
  tasks: 2
  commits: 2
plan_head_before: 9b269853778bee00461ee427e52246620bdddb65
plan_head_after: 7cd73cbb7761815144622c1a50a628d1ba2d58b6
---

# Phase 26 Plan 02: Node cache, range, count, height and visit Summary

The prolly crate now reads nodes through a fixed-cap node cache (`Ctx`), and offers a seek/range cursor, root-only `count` and `height`, and a node visitor with IDs, with no change to the node bytes or the goldens.

`commits: 2` counts the code commit and the gate-log commit; the SUMMARY commit that follows is not in the count.

## What was built

- `cache.rs`: `NodeCache::new(cap)`, `stats()` (`hits`, `misses`, `entries`, `bytes`, `cap`), `clear()`; `Ctx::new(store, Option<&NodeCache>)`, `Ctx::uncached(store)`. Eviction by oldest use; a node larger than the cap is not kept; cap 0 keeps nothing.
- `node.rs`: `load` and `load_child` take `&Ctx` and return `Loaded { node: Arc<Node>, bytes, hit }`. Order: cache hit (or `store.read` + decode), config fit, level and non-empty, parent-entry checks, then insert on a miss.
- `tree.rs`: `get_in`, `iter_in`; `Cursor` holds the `Ctx` and `Arc<Node>` and clones the entry it yields; `seek_in` (start and end) for `range`. `get`, `iter`, `walk`, `build`, `Builder` keep their signatures.
- `apply.rs`: `apply_in(&Ctx, ...)`; `apply` wraps it. `nodes_read` counts every load, `cache_hits` the hits. Module doc updated.
- `range.rs`: `range`, `Range` (with `max_nodes_held`), `count` (checked sum from the root node), `height`.
- `visit.rs`: `visit(&Ctx, root, &mut dyn FnMut(&NodeVisit) -> bool) -> Result<bool>`; parent before children, left to right, one path; `Ok(false)` when `f` stops it.
- `lib.rs`: exports; "# Memory" and "# Concurrency" text for the bounded cache.

## Decisions

Discretion choices from the plan are in the frontmatter (LRU by tick, charge formula, lock). Also: new nodes written by `apply_in` are not put into the cache (not asked for; a later plan can warm it if the bench shows a need). A cold apply can have cache hits (each level pass reads the upper path again), so the test compares a warm apply to a cold one (more hits, same `nodes_read`) and does not expect zero cold hits.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 2 - Missing critical functionality] A cache hit must obey the config of the caller**
- **Found during:** Task 1 design
- **Issue:** the cache is keyed by ID only, but `apply` decodes with its own (tighter) config; a node cached by a reader (`Config::READ` limits) would pass an apply of a smaller config, which the existing test `foreign_config_node_is_an_error_in_apply` forbids.
- **Fix:** each cached node stores its largest key and entry; a hit is checked against the asking config. Test `a_hit_is_checked_against_the_config_of_the_apply`.
- **Files modified:** `cache.rs`, `node.rs`

**2. [Rule 3 - Blocking, rule 12] Lock-order comment**
- **Issue:** rule 12 says every new lock goes into the lock-order comment at the top of `crates/metadata/src/store.rs`, a file outside `files_modified`.
- **Fix:** two comment lines (the `NodeCache` lock is a leaf at `Rank::Store`). No code change; `cargo test -p metadata` re-run after it and passed. The workspace gate ran just before this comment edit. The lock-stats test was not run: no timed lock or `Site` was added and no existing lock changed.
- **Files modified:** `crates/metadata/src/store.rs`
- **Commit:** 7aa5658

**3. [Plan structure] One code commit for both tasks**
- Task 1 and Task 2 share `lib.rs` (modules and exports) and `tree.rs` (`seek_in` serves `range`), so the tree compiles only with both. One code commit (7aa5658) and one gate-log commit (7cd73cb).

All other edited files are in `files_modified`. `benchmarks/prolly-bench` is unchanged.

## Verification

- `runs/gate-26-02.log`: `cargo test -p prolly` (48 tests, golden_root unchanged), `cargo test -p prolly-bench`, clippy `-D warnings` for `prolly`, `unsafe-count.sh`, `cargo test --workspace`: all five rc=0. `retry_tests` did not fail.
- New tests: cache bounds, hits, cap 0, clear, cached equals uncached (get, iter, 200-edit apply), warm apply counters, a hit checked against the apply config, a bad node not cached; proptest `range_equals_filtered_iter` (40 cases, also through a small cache), range edges, range holds one path of a depth 3 tree, count and height read one node (1 cache miss), visit equals a reference recursion (IDs, order, levels, leaf entries), visit stops, crafted trees give `Error::Corrupt` from `range`, `count` (overflow) and `visit`, cached and uncached.

## Known Stubs

None.

## Threat Flags

None. The only new shared state is the cache: ID-keyed hash-verified nodes, fixed cap, one leaf lock (T-26-04, T-26-05, T-26-06 as planned).

## Self-Check: PASSED

Files found: `cache.rs`, `range.rs`, `visit.rs`, `gate-26-02.log`. Commits 7aa5658 and 7cd73cb are ancestors of HEAD.
