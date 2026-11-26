---
phase: 24-prolly-evaluation
plan: 02
subsystem: prolly prototype
status: complete
tags: [prolly, prototype, apply, chunk-store]
requires:
  - "24-01: node codec, Splitter, LevelWriter, Builder::with_level and push_node"
provides:
  - "prolly::apply(store, cfg, root, batch) -> (ObjectId, ApplyStats), with Edit and ApplyStats"
  - "the Apply section of the crate doc"
affects: [24-03]
tech-stack:
  added: []
  patterns:
    - "one LevelWriter (and so one Splitter) for build and apply at every level"
    - "level-generic rewrite: an internal entry is a keyed entry, so one routine runs at every level"
key-files:
  created:
    - crates/prolly/src/apply.rs
    - .planning/phases/24-prolly-evaluation/runs/gate-24-02.log
  modified:
    - crates/prolly/src/lib.rs
    - crates/prolly/src/tests.rs
decisions:
  - "Edit is an enum with owned bytes; a batch is a slice sorted by strictly ascending key; an empty batch returns the same root with no reads"
  - "A shrink of the height may write up to depth nodes that the canonical collapse skips; they stay unreferenced in the store"
  - "No node cache; the decoded old root is kept for the whole apply, so the top level pass does not read it again"
  - "The collapse reads the single new top node (one read) to see if it has one child"
  - "The empty root in the result counts as one written node of 9 bytes in ApplyStats"
  - "apply sums the subtree counts it pushes in each level pass with checked_add and gives Error::Corrupt on overflow (crafted counts); tree.rs is not changed"
  - "high_water_bytes is noted after each node load and after each push"
metrics:
  duration: "about 40 min"
  completed: 2026-11-26
requirements: [PROLLY-01]
actuals:
  tokens: 8900    # chars/4 over the code diff
  tasks: 3
  commits: 4       # git rev-list --count 3aa47c8..fe0b32c
plan_head_before: 3aa47c8
plan_head_after: fe0b32c
---

# Phase 24 Plan 02: Prolly apply Summary

`prolly::apply` applies one sorted batch of puts and deletes (one commit) to a prolly tree. It rewrites only the touched nodes, the nodes up to the next resync point and their paths, through the `LevelWriter` of the bulk builder. The root is canonical: it equals the `build` root of the final set, whatever the edit order or the batch cuts.

## Commits

| Task | Commit | Subject |
|------|--------|---------|
| 1 (tracer) | c45f629 | feat(24-02): apply a sorted batch to a prolly tree |
| 2 | 4e499db | test(24-02): canonical roots, order independence and idempotent apply |
| 3 | 5fcb71b | test(24-02): model test, apply memory bound, crafted nodes in apply |
| 3 (fix) | fe0b32c | fix(24-02): keep the count check in apply; high water after each load |

The SUMMARY has its own docs commits.

## How apply works

- The batch is checked first (order, key size, entry size), before any read or write.
- Level by level from the leaves: a forward-only path cursor finds the old node of each edit (the first node whose last key is at or above the key, else the last node). The cursor reads each internal node at most once per level pass.
- At a touched node, a fresh `LevelWriter` takes the old entries with the edits merged in. At each old node end, the run stops if the writer is empty and the next node has no edit (resync). The last node of a level ends by flush, so it takes all remaining edits.
- Each replaced node gives "delete child (last key)" and each new node gives "put child (last key, id, count)" for the level above. A put that equals the old child entry is dropped. When no edit is left, the old root is returned.
- Above the old top: 0 new nodes give `empty_root`, 1 node is collapsed through one-child nodes, and more nodes go to `Builder::with_level(top + 1)`.

## Results

- `cargo test -p prolly`: 21 passed (11 from 24-01, 10 new), 0 failed, about 1.1 s. `golden_root` is still green, so the rule and the node format did not change.
- The tracer gate: the four tests of the Task 1 verify passed, and the full `cargo test -p prolly` passed, on the same tree just before the Task 1 commit. Task 2 started only after that.
- `cargo +stable clippy --locked -p prolly --all-targets -- -D warnings -D clippy::allow_attributes_without_reason`: `clippy=0`.
- `RUST_STABLE=stable scripts/safety/clippy.sh`: `CLIPPY os=Darwin toolchain=stable result=ok`, `UNSAFE files=31 tokens=132 result=ok`. The count did not change. It ran before each code commit.
- `unsafe` tokens in `crates/prolly`: 0. `HashMap` in `crates/prolly/src`: 0.
- `git diff --quiet a882251 -- crates/metadata crates/slicefs-cli crates/chunk-store docs/design/store-format.md`: `diff_rc=0`.
- File-list gate (`git diff --name-only 3aa47c8 HEAD`, 3aa47c8 is the merge base): `24-02-SUMMARY.md`, `runs/gate-24-02.log`, `apply.rs`, `lib.rs`, `tests.rs`. Only the files of the plan. No Cargo file changed.
- `one_change_writes_one_path`: a same-width value change writes exactly depth nodes (DEFAULT 20,000 entries and SMALL 5,000 entries), and the store gets depth new objects.
- Line count (D-18): 2,095 lines in `crates/prolly/src/*.rs` (apply 463, lib 232, node 195, rule 123, tests 668, tree 414). This is above 2,000. Why: the tests are 668 lines (21 tests, with the proptest model and the crafted-node cases), and about a third of `apply.rs` and `lib.rs` is doc. The code of `apply.rs` without doc is about 300 lines.
- Full log: `runs/gate-24-02.log`. Its first line shows head `c3f4828`, because the gate ran on the work tree of the fix before the fix commit `fe0b32c`.

## Deviations from Plan

1. **[Rule 2 - Correctness] A check of crafted subtree counts in apply.** The new crafted-node test found a panic: two old children with a subtree count of `u64::MAX` give "attempt to add with overflow" in `LevelWriter::push_child` (debug builds). Only apply can reach this, because it feeds old child counts from the store into the writer. The test was red first. A first fix in `tree.rs` (commit 5fcb71b) broke the file-list gate of the plan, so commit fe0b32c moved the check into `apply.rs` and put `tree.rs` back to the 24-01 version. Each level pass now sums the counts it pushes with `checked_add` and gives `Error::Corrupt` on overflow, before the push. Each writer sum, and each sum of the bulk builder over the new top nodes, is a part of that total, so none can overflow. In a valid tree, the total is at most the entry count. The stale `#[allow(dead_code, reason = "the caller is apply in plan 24-02")]` on `Builder::push_node` stays in `tree.rs` (it gives no warning). It can go in a later plan that may edit `tree.rs`.
1b. **[Rule 1 - Bug] `high_water_bytes` was too low for delete-only work.** In 5fcb71b the high water was noted only after a push, so a batch that only empties nodes reported 0. Commit fe0b32c also notes it after each node load. `canonical_root` now checks a lower bound: the delete-all apply reports at least the batch bytes.
2. **[Test addition] `malformed_node_is_an_error_in_apply` has a fifth case** ("subtree counts above u64") next to the four cases of the plan. Each case is tested as the root and as the child of a good node. The batch key is `zzzz`, so the edit goes to the last node of each level and reaches the bad node.
3. **[Discretion] The proof that the batch errors come before any read** in `apply_rejects_unsorted_batch`: the same bad batches also go to a root that is not in the store. A read would give `Error::Store`, but the result is still `Unsorted`, `EntryTooLarge` or `KeyTooLarge`. `KeyTooLarge` of a delete key is checked too.
4. **[Discretion] Scratch folder.** The launch message gave a fixed private scratch folder, so the plan's `mktemp -d` was not used. The gate log went through sed for the repo path, the scratch path and the home path.
5. **[Process] The GSD (Get Shit Done workflow) state steps were not done.** The launch message did not permit changes to `STATE.md` and `ROADMAP.md`. The commits are on `exec/24-02`. The plan-commit ledger file was not written, because the worktree tool guard refused the compound git command. The base `3aa47c8` is known from the start step.
6. **[Gate] The plan's "full suite" with `prolly-bench`** is not run, because that package comes in 24-03 (as in 24-01). The gate is `cargo test -p prolly` and clippy.

## Notes for plan 24-03

- `ApplyStats { nodes_read, nodes_written, bytes_written, high_water_bytes }` is ready for the bench. `bytes_written` counts the bytes passed to `put`. Dedup can store fewer bytes, so use the store stats delta for the on-disk growth.
- `nodes_read` of a one-change apply is (depth - 1) x depth / 2 + 2, plus the reads of a run past a resync miss: each level pass below the top reads the path under the root down to that level, the root is read once, and the collapse reads the new top node once.
- One `apply` per root at a time (single writer). Readers of old roots are not affected.

## Known Stubs

None.

## Threat Flags

None. No new network, auth or file path. T-24-04 (crafted tree) and T-24-05 (bad batch) are covered by `malformed_node_is_an_error_in_apply` and `apply_rejects_unsorted_batch`. T-24-SC: no dependency change.

## Open owner questions

None from this plan. Q-1 to Q-3 of 24-CONTEXT.md stay open.

## Self-Check: PASSED

- `apply.rs` and `runs/gate-24-02.log` exist. The commits c45f629, 4e499db, 5fcb71b and fe0b32c are on `exec/24-02` (`git log 3aa47c8..HEAD`). c3f4828 is the first docs commit of this SUMMARY.
