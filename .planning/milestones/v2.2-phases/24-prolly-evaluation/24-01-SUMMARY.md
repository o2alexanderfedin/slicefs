---
phase: 24-prolly-evaluation
plan: 01
subsystem: prolly prototype
status: complete
tags: [prolly, prototype, chunk-store, boundary-rule]
requires: []
provides:
  - "crates/prolly: node codec, size-aware splitter, streaming Builder, get, Cursor, walk"
  - "Builder::with_level and Builder::push_node (pub(crate)) for apply in plan 24-02"
affects: [24-02, 24-03]
tech-stack:
  added: []
  patterns:
    - "one Splitter for every level and every caller"
    - "exp with the four basic float operations only (same node IDs on every platform)"
key-files:
  created:
    - crates/prolly/Cargo.toml
    - crates/prolly/src/lib.rs
    - crates/prolly/src/rule.rs
    - crates/prolly/src/node.rs
    - crates/prolly/src/tree.rs
    - crates/prolly/src/tests.rs
    - .planning/phases/24-prolly-evaluation/runs/gate-24-01.log
  modified:
    - Cargo.toml
    - Cargo.lock
decisions:
  - "node::load returns (Node, encoded length), so that walk can report the node bytes without a second read"
  - "load of a child (want_level set) also rejects an empty node: only the root can be the empty leaf"
  - "Builder::finish in with_level mode: 0 nodes in gives empty_root, 1 node in is the root itself (canonical)"
  - "cut_probability uses saturating u64 operations, so a call with out-of-range sizes cannot overflow; in the rule the sizes are at most 2^14, so the values are exact"
  - "Builder::push_node has #[allow(dead_code, reason = ...)] until plan 24-02 calls it"
metrics:
  duration: "about 45 min"
  completed: 2026-11-26
requirements: [PROLLY-01]
actuals:
  tokens: 11900
  tasks: 3
  commits: 3
plan_head_before: cb56861
plan_head_after: 9782ce1
---

# Phase 24 Plan 01: Prolly crate core Summary

A new crate `crates/prolly` keeps a sorted byte map as a prolly tree. Each node of the tree is one chunk-store object. The crate has Dolt's size-aware key splitter (Weibull K=4, L=4096, a salt per level, a forced cut at `NODE_MAX - MAX_ENTRY`) and a `e^-x` that uses only `+ - * /`. It also has a streaming builder, `get`, an ordered cursor and a node walk. The golden test pins the root ID and the cut-probability bits.

## Commits

| Task | Commit | Subject |
|------|--------|---------|
| 1 (tracer) | 39c3e87 | feat(24-01): prolly crate with the boundary rule, builder and get |
| 2 | 2534227 | feat(24-01): ordered cursor and node walk for the prolly map |
| 3 | 9782ce1 | test(24-01): errors on bad input, prolly crate doc, plan gate log |

## Results

- `cargo test -p prolly`: 11 passed, 0 failed, the test time is about 0.9 s.
- `cargo +stable clippy --locked -p prolly --all-targets -- -D warnings -D clippy::allow_attributes_without_reason`: exit 0.
- `RUST_STABLE=stable scripts/safety/clippy.sh`: `CLIPPY ... result=ok`, `UNSAFE files=31 tokens=132 result=ok`. The count does not change, because `crates/prolly` has 0 tokens.
- `cargo tree -i prolly -e normal` gives only `prolly`. `cargo tree -p prolly --depth 1` gives `blake3 chunk-store prolly`.
- `git diff --quiet a882251 -- crates/metadata crates/slicefs-cli crates/chunk-store docs/design/store-format.md`: `diff_rc=0`.
- In `Cargo.lock`, the `prolly` block has no `source` line. The lock diff is only that block.
- Golden values (`Config::DEFAULT`, `u64_entries(5_000, 7)`): root `1ee1cf56844ec96d06f8dce27d4e0de09b87e7ce5926e397ff4e4c387e271f81d6`, depth 3, 58 leaves. `boundary_hash(0, b"") = 527_629_384`, `boundary_hash(1, b"key") = 94_351_618`.
- Line count (D-18): 1,252 lines in `crates/prolly/src/*.rs` (lib 209, node 195, rule 123, tests 311, tree 414). This is before the `apply` code of plan 24-02, so the crate will probably be above the 1,000 to 1,500 line target after 24-02. Most of the lines are tests and docs.
- The full log is in `runs/gate-24-01.log`. The head in its first line is `2534227`, because the gate ran on the Task 3 work tree before the Task 3 commit.

## Deviations from Plan

1. **[Rule 3 - Blocking] The gate command of 24-VALIDATION.md.** The "full suite" `cargo test -p prolly -p prolly-bench` exits 101 at this time, because the `prolly-bench` package does not exist until plan 24-03 (`package ID specification prolly-bench did not match any packages`). For this plan, the gate is `cargo test -p prolly`, as plan rule 3 says. No stub bench crate was made.
2. **[Discretion] `node::load` returns `(Node, usize)`.** The plan signature is `load(store, id, want_level) -> Result<Node>`. The second value is the encoded length. `walk` uses it for `NodeInfo::bytes`.
3. **[Rule 2 - Correctness] An empty node as a child is `Error::Corrupt`.** The plan does not list this case. An empty leaf is valid only as the root of the empty map.
4. **[Discretion] Scratch folder.** The launch message gave a fixed private scratch folder. Thus the plan's `mktemp -d "${TMPDIR:-/tmp}/p24-01.XXXXXX"` was not used. The gate log is put through sed for the scratch path, the repo path and the home path.
5. **[Process] The GSD (Get Shit Done workflow) state steps were not done.** The launch message did not permit changes to `STATE.md` and `ROADMAP.md`. The commits are on `exec/24-01`, as the launch message said. The plan-commit ledger file was not written, because the worktree tool guard refused the compound git command. The base `cb56861` is known from the start step.

## Notes for plan 24-02

- `Builder::with_level(store, cfg, level)` and `Builder::push_node(NodeRef)` are `pub(crate)`. `push_node` checks the key order. `finish` makes the root canonical in this mode too: when 1 node goes in, that node is the root and no one-child node is written.
- `LevelWriter` is the only caller of `Splitter::push`. It writes no empty node.
- The golden root of `golden_root` must not change in 24-02. If it changes, the rule changed.

## Known Stubs

None.

## Threat Flags

None. The crate adds no network, auth or file path. Node bytes from the store are checked (T-24-01).

## Open owner questions

None from this plan. Q-1 to Q-3 of 24-CONTEXT.md stay open. The crate doc names Q-3 (if the rule and its constants become part of a frozen store format).

## Self-Check: PASSED

- All seven created files exist. The three commits 39c3e87, 2534227 and 9782ce1 are on `exec/24-01`.
