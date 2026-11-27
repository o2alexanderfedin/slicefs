---
phase: 25-prolly-format-and-layout
plan: 08
subsystem: prolly-format
tags: [constants, golden, store-format-8, docs, phase-gate]
requires: [25-04, 25-07]
provides:
  - "Config::DEFAULT = L 8192, NODE_MAX 16384 (the verdict of 25-07); golden_root re-pinned"
  - "golden_root passes on macOS and on Linux (Docker): same cut points and roots"
  - "docs/design/store-format.md section 'Metadata maps (store format 8)'"
  - "Phase gate with all Phase 25 plans merged: workspace tests, clippy with unsafe count, Docker all: all rc=0"
affects: [26-prolly-maps]
requirements-completed: [PMAP-01, PMAP-02, PMAP-04, PMAP-05]
status: complete
commits: 4
plan_head_before: cd930b0e8c113cb86257057ae4de37b7b9da1890
plan_head_after: ca10e736151e1eaa6ea1ae4f355bc532a9ac4365
actuals:
  tokens: 40000
  tasks: 3
  commits: 4
tech-stack:
  added: []
  patterns: ["asserting run and print run in the golden log", "doc numbers quoted from decision.txt"]
key-files:
  created:
    - .planning/phases/25-prolly-format-and-layout/runs/golden-macos.log
    - .planning/phases/25-prolly-format-and-layout/runs/golden-linux.log
    - .planning/phases/25-prolly-format-and-layout/runs/gate-25-08.log
  modified:
    - crates/prolly/src/lib.rs
    - crates/prolly/src/node.rs
    - crates/prolly/src/tests.rs
    - crates/prolly/Cargo.toml
    - docs/design/store-format.md
    - docs/research/prolly-trees.md
    - docs/design/metadata-maps.md
    - docs/design/chunk-store.md
key-decisions:
  - "WEIBULL_L 4,096 -> 8,192 by the verdict line; NODE_MAX stays 16,384; RULE_VERSION stays 1"
  - "The doc says rule version 1 is the integer threshold with the constants of Config::DEFAULT"
---

# Phase 25 Plan 08: Pin the constants, prove the goldens, write the format section, phase gate

`Config::DEFAULT` now has L = 8,192 and NODE_MAX = 16,384 (the verdict `layout=a l=8192 node_max=16384` of plan 25-07). `golden_root` is re-pinned and passes on macOS and on Linux. The metadata maps section of `docs/design/store-format.md` is written. The phase gate passes.

## Commits

| Commit | What |
|--------|------|
| `4a4e14d` | Task 1: constants, goldens, crate and node docs, Cargo description |
| `0dc39fa` | Task 1: `golden-macos.log` and `golden-linux.log` |
| `f22e3b5` | Task 2: store-format section, research, metadata-maps and chunk-store docs |
| `ca10e73` | Task 3: `gate-25-08.log` |

## Pinned constants and goldens

Constants: `WEIBULL_L` 8,192; `NODE_MAX` 16,384; `NODE_MIN` 512; `MAX_ENTRY` 2,048; `MAX_KEY` 1,024; `NODE_HEADER` 10; `MAX_NODE_MAX` 32,768; `RULE_VERSION` 1.

`golden_root` values (printed with `PROLLY_PRINT_GOLDENS=1`, not copied from the plan):

- Table hash (unchanged): `6a72017ef6d3d7daf2ea9385afe01ac20fd4bf85610a5179c9bc7384bcc369e4`
- Thresholds: `[7, 20166, 4737341, 11975333, 87726437, 448166577, 803046, 1045739182]`
- Leaf entry counts of `u64_entries(5_000, 7)`: 30 leaves, depth 2: `[258, 103, 205, 132, 161, 210, 123, 185, 103, 235, 232, 177, 155, 154, 156, 251, 155, 155, 105, 162, 287, 209, 170, 139, 165, 45, 200, 215, 128, 25]`
- Unsalted root: `1e74a7eb632708b048b3d53a40d134148232c48802088f47a84250e92c192ded4f`
- Salted root: `1e018bcf4bdb6d818cb17bdf929bab475b6ce59a9f64fc565b3bdb85645c485e89`
- The wide config (8192/32768) thresholds and the boundary hashes did not change.

## Results

- **macOS** (`golden-macos.log`): the asserting run of `golden_root` is `ok`, then the print run shows the same values. The log has both because the print run asserts nothing.
- **Linux** (`golden-linux.log`): Docker case `prolly-bench-smoke` (rustc 1.100.0, release): `cargo test -p prolly` 33 passed, `golden_root` `ok`, bench smoke `checks ok: 2`, `rc=0`. The cut points and roots are the same as on macOS.
- **Fixed-point rule with the new default**: max error 1.19e-7 against the float formula (limit 2^-22); 0 of 1,000,000 cut decisions differ.
- **Phase gate** (`gate-25-08.log`): `cargo test --workspace` rc=0; `RUST_STABLE=stable scripts/safety/clippy.sh` rc=0 with `UNSAFE files=32 tokens=134 result=ok`; `scripts/docker-linux-test/run.sh all` (Linux workspace tests, the vendored fuser tests, the FUSE smoke on stores of this build) rc=0; `paths_ok=1`; `subjects_ok=1`.

## Doc section

`## Metadata maps (store format 8)` has the nine subsections of the plan (design target, layout, root record, node format, boundary rule, constants, boundary salt, scale at 1B inodes, this build and the prolly maps). The numbers come from `decision.txt`. It says in plain words that the refcount signal and the b-versus-c comparison come from synthetic rows only (Boost refcounts are empty). The key grammar of layout a is a table; the name suffix `0x01 || ino || kind || name` is described, and directory entries by name are left to the commit path (issue #215).

## Deviations from Plan

**1. [Rule 1 - Plan error] `threshold_edges` and `levels_use_their_own_salt` held old defaults.** With L = 8,192 the threshold of a node that grows from 0 to NODE_MAX bytes is `2^32 - 483`, not `2^32` (the same arithmetic as the 25-01 note). The test now asserts `2^32 - 483` for 16,384 and `2^32` for 32,768. `levels_use_their_own_salt` needs at least 20 level-1 nodes; at the larger L, 200,000 entries gave 9, so the test builds 500,000 entries (it passes, with a narrow margin over 20). Commit `4a4e14d`.

**2. [Rule 3] Golden macOS log has two runs.** The plan asked for the print run only; that run asserts nothing, so its `ok` line proves nothing. The log holds the asserting run first, then the print run.

**3. [Plan text] The plan names a "Floats" section of the crate doc.** It no longer exists (25-01 removed it). The remaining Status and Integer rule sections were rewritten. The first line of `lib.rs` also said "prototype ... for issue #19"; it now names store format 8 and issue #215.

**4. [Check] The text check of the gate fails on three earlier summaries.** The plan's check (home-path patterns and a legal-terms word over `git diff --name-only develop...HEAD`) lists `25-05-SUMMARY.md` (line 91), `25-06-SUMMARY.md` (line 112) and `25-07-SUMMARY.md` (line 138). Those lines are sentences that say a check found no home path or legal text, so the searched word appears in them. They are not my files and I did not edit them (a failure in another plan's file is reported, not fixed). The gate log therefore shows two lines: `paths_ok=1` (no home path in any changed file) and `text_ok=0` with the three files. The orchestrator may want to reword those three sentences before the merge to a public branch.

**5. [Order] Commit order.** The code commit came after a full `cargo test --workspace` (rc=0) on the same tree; the gate logs are separate commits so the log names a commit that holds the pins.

**6. [Trailer] Commit trailer** `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` per the owner rule and the plan; the branch is `exec/25-08`. `plan_head_before` is the phase branch tip `cd930b0` (no ledger file was written in this worktree run); `commits: 4` is the measured `git rev-list --count cd930b0..HEAD` before this SUMMARY commit.

## Notes for later plans

- `benchmarks/prolly-bench/src/layout.rs` still has `--l` default 4,096 (an explicit sweep default, outside this plan's files). `replay.rs` uses `Config::DEFAULT`, so the Phase 24 replay now runs at L = 8,192; its smoke test passes in the workspace run.
- Owner questions from 25-07 stay open: the 5% tolerance idea for the 1B node rule, and the 1.68x bytes and 1.64x CPU cost of L = 8,192 for a small commit.

## Known Stubs

None.

## Threat Flags

None. T-25-26: the constants come from the verdict line; `config_new_limits` compares `DEFAULT` with `Config::new(8192, 16384)`. T-25-27: the same pinned `golden_root` passes on macOS and Linux. T-25-28: the doc quotes `decision.txt` and the table hash comes from `golden_root`. No dependency was added.

## Self-Check: PASSED

- Files exist: `golden-macos.log`, `golden-linux.log`, `gate-25-08.log`, the four docs, this file.
- Commits `4a4e14d`, `0dc39fa`, `f22e3b5`, `ca10e73` are ancestors of HEAD.
- Acceptance greps: `store-format.md` in `lib.rs` (1 or more); `private to this crate` in `node.rs` (0); `store format 8` in `Cargo.toml` (1); `golden_root ... ok` in both golden logs; last line of `golden-linux.log` is `rc=0`; gate log has 3 `rc=0` and no `rc=[1-9]`; `UNSAFE ... result=ok`; `subjects_ok=1`; `Answered in Phase 25` twice in `prolly-trees.md`; no home path or legal-terms word in the four docs and the three logs.
