---
phase: 25-prolly-format-and-layout
plan: 05
subsystem: prolly-bench
tags: [layout, maps, prolly, bench, refcounts, combined-key]
requires: [25-01, 25-02]
provides:
  - "prolly-bench maps: five-map change of each real commit (base.bin, batch files, plan.txt)"
  - "prolly-bench layout a|b|c: replay into five trees, one combined tree, or combined plus refcounts apart"
  - "layout.rs engine contract for plan 25-06 (Layout, MapKind, MapEdit, Ctx, Engine)"
  - "fixed output schema: maps, final, commit, tree, check lines"
affects: [25-06, 25-07]
requirements-completed: [PMAP-01]
status: complete
actuals:
  tokens: 9800
  tasks: 2
  commits: 2
plan_head_before: 59d85ae3df24e1a08cf0b18ac4595acffc39b418
plan_head_after: 940223d83d4a561cbdccc5e654d3153ba4b92b46
tech-stack:
  added: []
  patterns: ["streamed base build, edits of one commit in a Vec", "four-way merge for the combined base"]
key-files:
  created:
    - benchmarks/prolly-bench/src/layout.rs
    - .planning/phases/25-prolly-format-and-layout/runs/gate-25-05.log
  modified:
    - benchmarks/prolly-bench/src/replay.rs
    - benchmarks/prolly-bench/src/main.rs
    - benchmarks/prolly-bench/tests/smoke.rs
key-decisions:
  - "Layout c (b with refcounts apart) is a third arm: refcounts have another key space and lifetime (#165)"
  - "Combined key is ino first: 0x01 ino(8 BE) kind(1); refcounts 0x02 content key(33); room for a later name suffix"
  - "Directories stay one value per directory in the bench (by-name entries are a Phase 26 decision)"
  - "check compares each final tree with the count and digest of the real last map, taken by maps"
---

# Phase 25 Plan 05: Layout engine for the Q-2 test

`prolly-bench` now extracts the change of all five metadata maps of each real commit (`maps`) and replays it into prolly trees of layout a, b or c (`layout`), with the fixed output schema and a final check against the real maps. No crate outside the bench changed.

## What was built

- **Task 1 (tracer), `748ae7d`.** `layout.rs` with `Layout`, `MapKind`, `MapEdit`, `Ctx`, `Engine` (`build`, `commit`, `print_trees`, `iter_map`), `maps` and `layout`. `maps` writes `DIR/<map>/base.bin`, `DIR/<map>/batch-<seq>.bin` (op byte and the raw 41-byte record, from the merge diff of two streams) and `DIR/plan.txt` (`maps` lines and one `final map=.. entries=.. digest=..` per map). Per-inode maps are compared by the ino as a number; refcounts by the 33-byte key. `layout --layout a` builds five trees, makes one `apply` per non-empty tree and one `sync` per commit inside one `measure`, then checks every final tree against `plan.txt`. `replay.rs`: `MapStream::next_raw` (the raw record), `Digest::add_raw`, and visibility for shared helpers; Phase 24 output is unchanged (the smoke asserts of the old modes pass untouched).
- **Task 2, `940223d`.** Layouts b and c. Combined key `0x01 || ino (8 BE) || kind (1)` (kind 0 inode, 1 dir, 2 manifest, 3 xattr) and `0x02 || content key (33)`. The combined base is a four-way merge of the per-inode streams by (ino, kind), then the refcount stream (layout b); layout c builds the refcount tree apart. `iter_map` filters the combined tree by prefix and kind. `--l` and `--node-max` go through `Config::new`; a bad `--layout`, `--l` or `--node-max` exits 2 before any file access. Doc comment at the top of `layout.rs`: layouts, key grammar, name-suffix room, schema, memory rule.

## Verification

`runs/gate-25-05.log` has four sections, all `rc=0`: `cargo test -p prolly-bench` (3 unit tests, 2 smoke tests), clippy (`cargo +stable clippy --locked -p prolly-bench --all-targets -- -D warnings -D clippy::allow_attributes_without_reason`), `unsafe-count.sh` (no new `unsafe`), `cargo test --workspace`. The retry timing test did not flake.

Smoke (200-file tree, 7 commits after the base): `maps` gives 35 `maps` lines and 5 `final` lines; layouts a, b, c and a with `--l 8192 --node-max 32768` each give one `check ... ok=1` and 7 `commit` lines; `tree` lines are 10 (a), 2 (b), 4 (c). Unit test `every_layout_replay_matches_the_model` replays two commits of puts, inserts and deletes in all five maps through each layout against a `BTreeMap` model (300 entries per map, same ino in all four per-inode maps, so the merge ties are exercised).

## Deviations from Plan

**1. [Plan assumption wrong] The refcount map is empty in the smoke store (and in every Boost run).**
- **Found during:** Task 1 smoke run.
- **Issue:** the plan asks the smoke test to assert that some `map=refcounts` line has `puts+dels>=1`. `whole` and `seed` never call `increment_refcount`, so the refcount map of every `whole` store has 0 entries in all commits (the final refcounts line is the empty-map digest). The assert cannot hold. Same for `xattr` (0 entries).
- **Fix:** the smoke test does not assert a refcount change. The refcount code path (record order, diff, `0x02` keys, layout c's separate tree) is covered by two unit tests in `layout.rs` (`a_refcount_diff_has_puts_deletes_and_inserts`, `every_layout_replay_matches_the_model`). **Consequence for 25-06/25-07:** on the Boost source the refcount arm measures nothing real (an empty tree in layouts a and c, no `0x02` keys in b); only the synthetic source can say anything about refcounts in a layout. The decision data must say so.
- **Files:** `benchmarks/prolly-bench/tests/smoke.rs`. **Commit:** `748ae7d`.

**2. [Rule 3 - Blocking] `Fail` derives `Debug`.** The unit tests use `expect` on `Res`, which needs it. One line in `main.rs`, inside `files_modified`. **Commit:** `748ae7d`.

**3. Added unit tests (not in the plan).** Three tests in `layout.rs` (refcount diff, replay against a model for all layouts, combined key order). They cover what the smoke store cannot (deviation 1).

No auth gates. No architectural changes. No file under `crates/` changed.

## Discretion choices (recorded)

- Layout c is a third arm (RESEARCH A1).
- The combined key puts the ino first so the records of one file share a leaf.
- The key grammar leaves room for `0x01 || ino || kind || name` (at most 8 + 255 + 2 bytes, under `MAX_KEY`); the bench keeps one value per directory (Phase 26 decides by-name entries).
- The edits of one commit are sorted in a `Vec`; the base is streamed.
- `check` is per map by `iter_map`; in layouts b and c that is one pass over the combined tree per map (five passes). Plan 25-06 can use a one-pass check if the 10M run needs it.
- `n` in every line is the inode entry count of the base; `source=boost`.
- No `commit` line for the base build (the base is reported by the `tree` lines of phase base).

## Known Stubs

None.

## Threat Flags

None. T-25-16: `maps` opens the real store with `read_only: true`; `layout` writes only its own store. T-25-17: lines carry counts and sizes only; the gate log passed the sed of rule 7 and has no home path or user name. T-25-18: `check` compares all five final trees with the real final maps; `ok=0` exits 1.

## Self-Check: PASSED

- layout.rs, the gate log and this file exist.
- Commits 748ae7d and 940223d are on `exec/25-05`.
- `git diff --name-only feature/phase-25-prolly-format-and-layout...HEAD` lists only files of `files_modified` (plus this SUMMARY).
- The three acceptance counts on the gate log hold: four `rc=0`, no `rc=` failure, no home path or license text.
