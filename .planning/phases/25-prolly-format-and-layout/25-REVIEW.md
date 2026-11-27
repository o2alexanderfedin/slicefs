---
phase: 25-prolly-format-and-layout
reviewed: 2026-11-27T00:00:00Z
depth: standard
files_reviewed: 31
files_reviewed_list:
  - benchmarks/prolly-bench/src/layout.rs
  - benchmarks/prolly-bench/src/main.rs
  - benchmarks/prolly-bench/src/replay.rs
  - benchmarks/prolly-bench/src/synth.rs
  - benchmarks/prolly-bench/tests/smoke.rs
  - crates/chunk-store/src/lock_stats.rs
  - crates/metadata/src/inode_alloc.rs
  - crates/metadata/src/lib.rs
  - crates/metadata/src/rehash/map.rs
  - crates/metadata/src/rehash/mod.rs
  - crates/metadata/src/store.rs
  - crates/prolly/Cargo.toml
  - crates/prolly/src/apply.rs
  - crates/prolly/src/lib.rs
  - crates/prolly/src/node.rs
  - crates/prolly/src/rule.rs
  - crates/prolly/src/tests.rs
  - crates/prolly/src/tree.rs
  - crates/slicefs-cli/src/cli.rs
  - crates/slicefs-cli/src/content.rs
  - crates/slicefs-cli/src/header.rs
  - crates/slicefs-cli/src/main.rs
  - crates/slicefs-cli/src/seed.rs
  - crates/slicefs-cli/src/stats.rs
  - crates/slicefs-cli/tests/boundary_salt_tests.rs
  - crates/slicefs-cli/tests/lock_stats_report.rs
  - crates/slicefs-cli/tests/store_header_tests.rs
  - scripts/docker-linux-test/run.sh
  - scripts/prolly-bench/decide.py
  - scripts/prolly-bench/layout.sh
  - scripts/prolly-table/gen_table.py
findings:
  critical: 0
  warning: 0
  info: 6
  total: 6
status: issues_found
---

# Phase 25: Code Review Report

**Reviewed:** 2026-11-27
**Depth:** standard
**Files Reviewed:** 31 (`rule_table.rs` is generated and excluded; `inode_map.rs` is deleted)
**Status:** issues_found (6 Info items; no BLOCKER and no WARNING)

## Summary

I read the diff against `develop` for every listed file and traced the three areas of special interest. I found no defect that makes behavior wrong, loses data or opens a security hole. The items below are quality and robustness notes only. I did not read `necessity-review.md`, so no `necessity:` count is given.

**`store.rs`: lock removal (checked, no race found).**
- The removed `inode_map` was a pure mirror of `inode_data`. Every site that wrote one wrote the other: `new()`, load, flush settle, commit settle and `delete_inode`. The only reads of the mirror were the `AlreadyExists` check in `insert_inode(at=Some)` and a test. That check now reads `live.map` and `inode_data`, which hold the same set.
- Each removed hold of the allocator lock sat beside an `inode_live` shard lock. Both settle paths and `delete_inode` still take the shard lock, then `inode_data`, in the documented order. `Live::settle(ino, ticket)` still refuses a late install after a delete or a newer change.
- Inode-number reuse is impossible. `allocate_ino` is monotonic under `inode_alloc`, and explicit numbers (`at`) are serialized by that lock plus the shard lock.
- Lost `next_ino`: `begin_commit` reads `next_ino` before `inode_live.take_all()`. This is safe only because the FS holds the commit gate exclusively. It is the same order as the old `inode_map.clone()`. See IN-01.
- Root record offsets (0, 8, 16, 49, 82, 115, 148, 181, 189, 197) agree with the bench copy in `main.rs`. All other users (`rehash/mod.rs`, `for_each_referenced`, `referenced_estimate`) were updated. A grep finds no remaining code use of `inode_map`.

**`prolly` rule and node.**
- Integer math: `p4` is at most (2^20)^4 = 2^80. The shift by 32 gives at most 2^112, and `L^4` is at most 2^56. Both fit in u128.
- `exp_neg_fp`: `j + 1` is at most 1024, which is inside the 1,025-entry table. `lo - hi` is non-negative because the table is strictly decreasing, and a test asserts that. `(lo - hi) * r` is below 2^54, so it fits in u64.
- `gen_table.py --check crates/prolly/src/rule_table.rs` exits 0.
- `decode` checks the length against the header, checks magic and rule version, and checks `count` against `remaining / smallest` before allocating. Every key and entry is bounds-checked, and it rejects trailing bytes. I found no panic path.
- `forced_cuts` is read before `reset` in both `push_leaf` and `push_child`, so the count is right.
- The salted boundary hash is keyed BLAKE3 over the same input bytes, and reads need no salt.

**`header.rs`, `content.rs`, `seed.rs`.**
- A format 7 header gets the M17 text. The `const _: () = assert!(FORMAT_NUMBER == 8)` guard is good.
- The salt line grammar is strict: exactly 64 lowercase hex digits, and duplicates are refused. The line and the ro_compat flag are paired, and duplicate flag names are already refused earlier.
- `check_write_with_salt` re-checks the salt after `check_with`, which closes the window of a parallel create. The seed store lock also serializes it.

## Info

### IN-01: `begin_commit` reads `next_ino` before it takes the dirty inodes

**File:** `crates/metadata/src/store.rs:1056-1058`
**Issue:** `next_ino` is read at line 1056, and `inode_live.take_all()` runs at line 1058. An inode that is allocated and cached between the two lines would be written by this commit, while the root record's `next_ino` would not be above it. After a reload, `allocate_ino` would hand out that number again, because it does not check for existence. Today this cannot happen: the only mid-mount caller (`begin_commit` via `filesystem.rs:4009`) holds the commit gate exclusively. The other callers (`destroy`, `seed`, offline tools) run with no concurrent requests. The old code had the same order (`inode_map.clone()` before `take_all`). The safety depends on a caller rule that is not checked in this function.
**Fix:** Read the allocator after the take, so the recorded value is always above every taken inode:
```rust
let inodes = self.inode_live.take_all();
let next_ino = self.inode_alloc.lock().next_ino();
```
Do this only if you want the function to be safe without the gate. Otherwise state the gate requirement in the `begin_commit` doc comment, which it already half does.

### IN-02: The format 8 header says "prolly metadata maps", but this build writes flat maps

**File:** `crates/slicefs-cli/src/header.rs:28`
**Issue:** `FORMAT` is `slicefs-store 8 (store header with feature flags, prolly metadata maps)`. The `prolly` crate docs (`lib.rs`, "Status") say that the commit, read and GC paths do not use prolly yet. A store made by this build is format 8 with flat maps and a 197-byte root record. CONTEXT accepts that format 8 can still change before a release. If Phase 26 changes the root record without a new bump, a store made now will fail with `Corrupted: expected N bytes, got 197` (the style of the new `a_230_byte_root_record_is_refused` test), not with a clean refusal. The same text appears in the `store_header_tests` expectations.
**Fix:** Make sure Phase 26 either checks the root record size and gives a clear "store made by a pre-release format 8 build" message, or does not leave pre-release stores around. Optionally drop "prolly metadata maps" from the first line until the maps are really prolly. The number is parsed, and the free text is not compared.

### IN-03: `--boundary-salt random` on an existing store gives a misleading message

**File:** `crates/slicefs-cli/src/content.rs:134-141`
**Issue:** `check_salt` refuses `SaltOpt::Random` on every existing store, including one that was made with `random`. The message is built by `msg_salt_differs` and reads `... has a boundary salt; --boundary-salt random is another salt`. "Random" is not "another salt"; it is a request that can never hold for an existing store. A script that runs `seed --boundary-salt random` twice on the same store fails the second time with this text. The refusal is correct and documented in the flag help, but the message does not explain it. The check also runs before the incompat and ro_compat access decision, so for a store with an unknown incompat flag the user sees the salt message first.
**Fix:** Use a dedicated sentence for `Random`, for example `... --boundary-salt random only works for a new store; pass the recorded salt as 64 hex digits, or leave the option out`. Optionally call `decide` before `check_salt`.

### IN-04: `Config` derives `Debug`, which prints the salt

**File:** `crates/prolly/src/lib.rs:176-184`
**Issue:** `#[derive(Clone, Copy, Debug, PartialEq, Eq)]` on `Config` includes `salt: Option<[u8; 32]>`. The bench and the tests print `{cfg:?}` (for example in `decide.py` fields and test assertion messages), and a product error or log line that formats a `Config` would print the salt. The salt is stored in plain text in `FORMAT`, so this is not a secret in the store. Still, the salt exists to resist grinding, and a hand-written `Debug` that prints `salt: Some(..)` costs little.
**Fix:**
```rust
impl std::fmt::Debug for Config { /* print salt as `Some(<32 bytes>)` / `None` */ }
```

### IN-05: The bench defaults to the old Dolt constants, not the product's

**File:** `benchmarks/prolly-bench/src/layout.rs:731`
**Issue:** `layout_opts` defaults `--l` to 4096. `Config::DEFAULT` and `WEIBULL_L` are now 8192. A `layout` or `synth` run with no `--l` measures a config that the store does not use. The matrix script always passes both values, so the pre-registered run is not affected. Also, `replay.rs` and `main.rs` modes (`prolly`, `scale`) now use `Config::DEFAULT` (L = 8192, 10-byte header), so their output no longer reproduces the Phase 24 numbers in `docs/research/prolly-trees.md`.
**Fix:** Default `--l` to `prolly::WEIBULL_L` (and `--node-max` to `prolly::NODE_MAX`) so a bare run matches the product. Note in the README that the Phase 24 `prolly` and `scale` rows are from the older constants.

### IN-06: `InodeAlloc` still derives `Clone`

**File:** `crates/metadata/src/inode_alloc.rs:10`
**Issue:** The `#[derive(Clone)]` was needed when `begin_commit` cloned the whole `InodeMap`. The type is now only locked and read (`next_ino()`), and no code clones it.
**Fix:** Remove the derive, or keep it if you want the type to stay a plain value type. No behavior depends on it.

---

_Reviewed: 2026-11-27_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_
