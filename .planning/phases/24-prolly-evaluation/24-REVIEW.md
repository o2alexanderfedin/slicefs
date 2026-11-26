---
phase: 24-prolly-evaluation
reviewed: 2026-11-26T00:00:00Z
depth: standard
files_reviewed: 17
files_reviewed_list:
  - Cargo.toml
  - crates/prolly/Cargo.toml
  - crates/prolly/src/lib.rs
  - crates/prolly/src/rule.rs
  - crates/prolly/src/node.rs
  - crates/prolly/src/tree.rs
  - crates/prolly/src/apply.rs
  - crates/prolly/src/tests.rs
  - benchmarks/prolly-bench/Cargo.toml
  - benchmarks/prolly-bench/build.rs
  - benchmarks/prolly-bench/src/main.rs
  - benchmarks/prolly-bench/src/replay.rs
  - benchmarks/prolly-bench/src/whole.rs
  - benchmarks/prolly-bench/tests/smoke.rs
  - scripts/prolly-bench/bench.sh
  - scripts/prolly-bench/README.md
  - scripts/docker-linux-test/run.sh
findings:
  critical: 0
  warning: 4
  info: 5
  total: 9
  necessity: 0
status: issues_found
---

# Phase 24: Code Review Report

**Reviewed:** 2026-11-26
**Depth:** standard
**Files Reviewed:** 17
**Status:** issues_found

## Summary

Scope: `git diff a882251..HEAD -- crates/ benchmarks/ scripts/ Cargo.toml`. I did not run cargo.

**Product and store format.** No product crate or store format changed. The full diff outside `.planning/` touches only `Cargo.toml` (two new workspace members), `Cargo.lock` (two new packages), `scripts/docker-linux-test/run.sh` (a new `prolly-bench-smoke` target), and the docs (`docs/design/metadata-maps.md`, `docs/research/prolly-trees.md`, two count lines in `docs/design/unsafe-review.md`). Nothing changed in `crates/metadata`, `crates/chunk-store`, `crates/slicefs-cli`, `crates/slicefs-traits` or `docs/design/store-format.md`. No `Cargo.toml` under `crates/` other than `crates/prolly` names `prolly`.

**Prolly crate.** I traced build, apply, cursor, get and walk. I found no defect on valid trees:
- The boundary rule is the same `Splitter` in build and apply.
- The resync rule is sound. A run starts after an unchanged node, which always ends at a cut. It stops only when the writer is empty at an old node end before an untouched node. The cut decision has no look-ahead and no state carries over a cut.
- `parent_ops` merges strictly sorted lists.
- `collapse` plus `Builder::with_level` give the canonical root.
- The forced cut at `node_max - max_entry` bounds every node.
- `exp_neg` uses only basic float operations.
- All `level - 1` and `len() - 1` operations are guarded by decode and load checks, so no panic is reachable from node bytes.

The weak spot is tree-level consistency on crafted input (WR-04).

**Benchmark.** The digest checks are correct: each check compares the final replayed map with the real final `inode_data`. The flat serializer is a faithful copy of `intern_u64_digest_map` plus `put_value`. The root-record offsets match `root_rec` in `crates/metadata/src/store.rs`. But:
- The edit script does much less than it says. Creates and deletes almost cancel inside each commit, so the small commits measure value updates only, which is the best case for prolly (WR-01).
- The on-disk sizes of `flat` and `prolly` are taken before `close()`, so the pack index is not counted. `whole` is taken after `close()` (WR-02).
- Some timed regions are not the same between the modes (WR-03).

These do not reverse the "later" recommendation. WR-01 and WR-02 both make prolly look slightly better than it is, so they make "later" stronger, not weaker. But the report should state them.

**Shell.** `bench.sh` is safe: `set -euo pipefail`, every `rm -rf` is under a `mktemp -d` dir that the script makes, the EXIT trap is set after `S` exists, the signal trap ignores later signals, and `WORK=/` is refused. The script has no time limit of its own; the README and the research doc tell the user to wrap it in `timeout`, and the Docker target does so.

## Warnings

### WR-01: The edit script's creates and deletes cancel inside each commit, so the small commits measure value updates only (prolly's best case)

**File:** `benchmarks/prolly-bench/src/whole.rs:117-131`, `benchmarks/prolly-bench/src/whole.rs:179-197`
**Issue:**
- Edit i of commit seq has the kind `(i + seq) % 4`, so a create (kind 2) is almost always followed by a delete (kind 3) in the same commit.
- The delete pops the oldest file from the `created` FIFO.
- Between commits the FIFO holds only about 1 to 3 files. After those are popped, every delete removes a file that this commit just created. That file never reaches a committed `inode_data`.

The recorded run data proves it:
- `runs/prolly-edits.csv` row `1,scatter,1000,31,250,250,250,250` shows 250 creates and 250 deletes.
- `runs/prolly-batches.csv` row `1,31,scatter,1000,502,2` shows that the real `inode_data` change of that commit was 502 puts and only 2 deletes.

So the 1,000 "changed files" are about 500 value replacements plus 2 appends and 2 tail deletes. With fixed-width values, a value replacement never moves a boundary (D-13), and an append touches only the right edge. The small-commit rows never insert or delete a key in the middle of the map. That case shifts boundaries and forces prolly to rewrite up to the next resync. Only the version step exercises it.

The research doc ("Method", "it is smaller than K") does not say why the entries-changed count is about half of K. A reader takes the create and delete costs as measured.

Failure scenario: the owner reads "K = 1,000: 401 nodes written" and the per-commit gain as covering creates and deletes of real files. A real workload deletes old files in the middle of the ino range, and prolly would write more nodes per commit than shown. Flat stays the same (it rewrites everything).

**Fix:** Make the deletes remove files that exist in the committed map. For example, delete a random seeded file from the scatter or cluster pick, and keep the bench-dir files only for creates. Or pop from the back of a FIFO that is not refilled in the same commit. Then re-run, or at least state in `docs/research/prolly-trees.md` that the small commits are value updates plus edge appends only:
```rust
3 => {
    // A delete of a file that exists in the committed map, not one made in this commit.
    let ino = self.pick(cluster)?;
    let (dir, name) = /* parent and name of ino, via the dir map */;
    self.meta.unlink(dir, &name)?;
    self.meta.delete_inode(ino)?;
    Ok(3)
}
```

### WR-02: The on-disk sizes of `flat` and `prolly` exclude the pack index; `whole` includes it

**File:** `benchmarks/prolly-bench/src/replay.rs:393`, `:415-416`, `:476`, `:502-504`; compare `benchmarks/prolly-bench/src/whole.rs:150-151`
**Issue:**
- `flat` and `prolly` call `print_disk(.., "end", ..)` before `cs.close()`.
- `ChunkStore::close` seals the active pack, which writes its `.idx` (`crates/chunk-store/src/lib.rs:3281-3292`). Before close, the store dir holds only the active pack.
- `whole` prints its end size after `w.cs.close()`.

The recorded data proves it. `runs/prolly-disk.csv` gives `files=1` for flat and prolly at start and end, and `files=12` and `files=19` for whole. The "On-disk size" table in the research doc therefore compares stores without an index (flat, prolly) against a store with one (whole). This breaks D-23 ("index included").

The bias favours prolly. Prolly writes more objects than flat (for example 1,026 against 323 at bulk, and 401 against 244 per scatter K = 1,000 commit), and the index is about 50 bytes per object (`IDX_ENTRY = ID_LEN + 17`). The effect is a few hundred KB against 17.5 and 31.2 MiB of growth, so the 44 % claim stands, but the method is wrong as stated.

**Fix:** Close the store before the end measurement in both replay modes, as `whole` does:
```rust
check("prolly", &plan, &d)?;
print_tree(&cs, &root)?;
cs.close()?;
print_disk("prolly", "end", &store)?;
```
For the "start" point, measure after a `close()` and reopen, or state that "start" has no index.

### WR-03: The timed regions differ between flat and prolly in the bulk and scale build rows

**File:** `benchmarks/prolly-bench/src/replay.rs:384-392` vs `:469-475`; `:570-576` vs `:587-595`
**Issue:**
- **Bulk row.** `flat` reads `base.bin` into its `BTreeMap` outside `timed`, then times only `intern_flat`. `prolly` times the `for_each_base` loop, so the 3.4 MB `BufReader` read of `base.bin` counts in the prolly wall and CPU time.
- **Scale build row.** `flat` makes the synthetic values (`synth_value`, 33 RNG steps per entry) before `measure`. `prolly` makes them inside `measure`. At n = 4,000,000 that is about 132 M xorshift steps that count against prolly only.

Both biases add time to prolly. They are small: about 1 to 2 ms of 64 ms CPU in the bulk row, and an estimated 3 to 6 % of the 3,082 ms CPU in the 4 M build row. They do not change the decision, but the "build" and "bulk" rows in the report compare unequal work.

**Fix:** Make the input outside the timed region in both modes. For example, read `base.bin` into a `Vec<(u64, [u8; 33])>` in prolly too, or stream it in both modes inside the timer. In `scale`, make the values first, or give `flat` the same in-timer generation:
```rust
let values: Vec<[u8; KEY_LEN]> = (0..n).map(|_| synth_value(&mut rng)).collect();
let ((root, ser), m) = measure(&cs, || { /* push pkey(2 + i), &values[i] */ })?;
```
Note that a pre-built `Vec` raises the prolly scale RSS. Report it apart, or accept the generation cost in both modes.

### WR-04: Parent and child consistency is not checked; on a crafted tree, `get` gives wrong answers and `apply` writes unreadable nodes while it returns `Ok`

**File:** `crates/prolly/src/tree.rs:70-103` (no order check in `LevelWriter::push_leaf`/`push_child`), `crates/prolly/src/tree.rs:292-306`, `crates/prolly/src/apply.rs:272-321`; claim at `crates/prolly/src/lib.rs:27-28`
**Issue:** `decode` checks each node alone: magic, counts, bounds, and ascending keys inside the node. Nothing checks that:
- a child's keys are within the range that its parent entry gives (above the previous sibling's `last_key`, at most its own);
- the parent's `last_key` equals the child's real last key;
- the subtree `count` is true.

Take a root with children A (`last_key "m"`, entries `a`, `m`) and B (`last_key "z"`, entries `b`, `z`). Each node is valid on its own.
- `get("b")` goes to A by `partition_point` and returns `Ok(None)`, though B holds `b`.
- `iter` yields `a, m, b, z` out of order.
- `apply(put "c")` merges A to `a, c, m`. If the writer is not empty at A's end, the run continues into B and pushes `b` and `z` through `LevelWriter`, which does not check order. It then writes a leaf `[a, c, m, b, z]` and returns `Ok(new_root)`. Every later read of that root fails with `Error::Corrupt("keys not strictly ascending")`.

The `ChunkStore::read` hash check stops random damage, so only a crafted or buggy writer can build such a tree. The crate doc's contract ("a damaged or crafted node gives `Error::Corrupt`") holds per node, not per tree. If a later phase builds on this code, `apply` silently persists the damage.

**Fix:** Carry the key range down the path and check it when a node is loaded:
- In `get`, `Cursor`, `walk` and `LevelCursor::descend`, pass `(lo_exclusive, hi_inclusive)` from the parent entry. Reject a child whose first key is at or below `lo`, or whose last key is not equal to `hi` (or, at minimum, is above it).
- In `LevelWriter`, keep the last pushed key and return `Error::Corrupt` on a key that does not ascend.

At the least, change the crate doc to say that tree-level consistency is trusted and not verified.

## Info

### IN-01: `new_record_bytes` leaves out the 4-byte record CRC

**File:** `benchmarks/prolly-bench/src/main.rs:61-62`, `:368`; `scripts/prolly-bench/README.md:210`
**Issue:** A pack record is `REC_HEADER (ID_LEN + 9 = 42) + payload + REC_CRC (4)` (`crates/chunk-store/src/lib.rs:374-375`, `:474`). The bench adds 42 bytes per object, so the README claim "the bytes that the packs grow by" is short by 4 bytes per object. The research doc does not use this column.
**Fix:** Change `REC_HEADER_BYTES` to 46, or rename it to `REC_OVERHEAD_BYTES = 42 + 4`, and update the README row.

### IN-02: `decode` limits keys to `MAX_KEY` and not to the config, and it does not limit the entry size; `apply` copies old entries without a size check

**File:** `crates/prolly/src/node.rs:113-118`, `:145-151`; `crates/prolly/src/apply.rs:283-321`
**Issue:**
- `decode` accepts any key up to the crate constant `MAX_KEY` (1,024), even under `Config::SMALL` (`max_key` 128). It accepts any leaf value up to `u16::MAX` bytes, and so any entry size.
- `apply` checks only the batch (`check`) and pushes the old entries back unchecked.

A crafted or foreign-config node with a 60 KB entry is pushed again. The forced cut then cannot keep the new node under `node_max`, so the "no node above `NODE_MAX`" bound fails on such input. There is no panic.
**Fix:** In `decode` (or in `load`, with the config), reject entries above `cfg.max_entry()`, keys above `cfg.max_key()`, and node bytes above `cfg.node_max()`.

### IN-03: The `flat` digest check verifies the in-RAM map, not the stored value

**File:** `benchmarks/prolly-bench/src/replay.rs:410-414`
**Issue:**
- The `prolly` check iterates over the stored tree, so it proves what was written.
- The `flat` check hashes the `BTreeMap` in RAM, so it proves only that the batch replay is correct. If `intern_flat` stored wrong bytes, the check would still pass.

**Fix:** Stream the last `put_value` key back through `MapStream` and digest that. Keep the last key from `intern_flat`.

### IN-04: `batches` opens the measured store read-write

**File:** `benchmarks/prolly-bench/src/replay.rs:142`
**Issue:** `batches` only reads, but it opens the `whole` store with `Options::default()`, so the open may recover, merge or write the store after its end size was measured. `seed` uses `read_only: true` (`main.rs:307`).
**Fix:** `ChunkStore::open(&store, Options { read_only: true, ..Options::default() })`.

### IN-05: A relative `CARGO_TARGET_DIR` resolves against the caller's directory, not the repo

**File:** `scripts/prolly-bench/bench.sh:76-79`
**Issue:** The script runs `cargo build` in `$REPO`, so a relative `CARGO_TARGET_DIR` is relative to `$REPO`. But `BIN=${CARGO_TARGET_DIR:-$REPO/target}/release/prolly-bench` goes through `abs`, which prefixes `$PWD`. When the script is started from another directory, the result is "BIN is not executable" and exit 2.

A related nit: `#[allow(dead_code, reason = "the caller is apply in plan 24-02")]` at `crates/prolly/src/tree.rs:205` is stale, because `apply.rs:437-439` now calls `push_node`.
**Fix:** `case $CARGO_TARGET_DIR in /*) ;; ?*) CARGO_TARGET_DIR=$REPO/$CARGO_TARGET_DIR ;; esac` before building. Remove the stale `allow`.

## Fixes

Fixed on the branch `exec/24-fix` (base `deea9f7`), one commit per finding.
No product crate and no store format changed (`git diff deea9f7` is empty
for `crates/metadata`, `crates/chunk-store`, `crates/slicefs-cli`,
`crates/slicefs-traits`, `docs/design/store-format.md`, `Cargo.toml` and
`Cargo.lock`).

| Finding | Commit | Fix |
|---|---|---|
| WR-01 | 4eab5b8 | A new edit pattern `insdel`, after the version step, so the old commits and their rows do not change. Each edit deletes a random file of the tree, or inserts a file that an earlier commit deleted again at its freed inode number (`create_inode_at`, with a new mtime). So each commit changes exactly K `inode_data` keys in the middle of the map, and no key is both inserted and deleted in one commit. `batches` counts the inserts (a new `inserts` column in `batches.csv`). The `insdel` rows stand next to the `scatter` and `cluster` rows; the old rows stay. The smoke test checks that an `insdel` commit changes exactly K keys, with real inserts and deletes. The report now says why the `scatter` and `cluster` commits change about K / 2 values. |
| WR-02 | 9037d2f | `flat` and `prolly` close the store before each disk line (start: after the bulk step, then the store opens again; end: after the last commit), so the pack index is counted, as for `whole`. The smoke test checks that each replay disk line counts a pack and its index. |
| WR-03 | 69384d7 | The prolly bulk build and the prolly scale build are timed in pieces of 16,384 entries. The input of a piece is read (`base.bin`) or made (the synthetic values) outside the timer, as `flat` does, and wall and CPU time add up over the timed pieces. No whole input is held in RAM, and the values do not change. |
| WR-04 | fd36069 | `node::load_child`: when `get`, the cursor, `walk` or `apply` loads a child, its last key must be the parent entry's key, its first key must be above the last key of the previous sibling (a bound carried down the path), and its leaf entry count must be the entry's count; else `Error::Corrupt`. `LevelWriter` refuses a key that does not ascend. The check is per path, and the crate doc says so. Test `inconsistent_tree_is_an_error`: the A/B tree of this review and three other crafted trees, for `get`, `iter`, `walk` and `apply`. |
| IN-01 | 51fa660 | `REC_OVERHEAD_BYTES = 42 + 4` (record header and CRC); the README row says so. |
| IN-02 | 9b94a66 | `decode` takes a config and refuses a node above `node_max`, a key above `max_key` and an entry above `max_entry`. `apply` loads every node with its own config (it writes the old entries again); `get`, the cursor and `walk` use `Config::DEFAULT`, the largest limits. New tests: an entry above `MAX_ENTRY`, a node above `NODE_MAX`, an internal key above `MAX_KEY`, and a `DEFAULT` tree that an apply with `Config::SMALL` refuses before it writes anything. |
| IN-03 | ad62442 | `intern_flat` returns the key of the stored value. The flat check streams that value back (`MapStream`) and digests it. |
| IN-04 | 9521872 | `batches` opens the store with `read_only: true`. |
| IN-05 | 9a26452 | `bench.sh` makes a relative `CARGO_TARGET_DIR` relative to the repo before the build. The stale `allow(dead_code)` of `push_node` is removed. |

Gate before the last code commit (IN-05), on its tree (`runs/gate-24-fix.log`):

- `cargo test -p prolly -p prolly-bench`: exit 0; prolly 23 passed, smoke 2 passed.
- `RUST_STABLE=stable scripts/safety/clippy.sh`: `CLIPPY os=Darwin toolchain=stable result=ok`, `UNSAFE files=32 tokens=134 result=ok`.
- `timeout 3600 cargo test --workspace` with the four `--skip` names: exit 0; 85 `test result` lines, 1,696 passed, 0 failed, 13 ignored.

Benchmark again (commit e2dc575), as plan 24-05 did it: `bench.sh DATA 3`
with the defaults (now with `PATTERNS="scatter cluster insdel"`), in
process, no mount; on the macOS host and in Linux Docker with GNU `time`.

- macOS: 2026-11-26T22:18:56Z to 22:21:47Z. The script waited 30 minutes for
  a 1-minute load of 8 or less and no `slicefs-gcmem` container; the load
  stayed at 13 to 18 (jobs of other projects), so it started at 13.30
  (after: 14.25). No `slicefs-gcmem` container ran.
- Linux Docker: 22:22:19Z to 22:25:15Z; VM load 0.05 before and 1.06 after,
  host load 13.80 and 15.19.
- 6 digest checks `ok=1` on each system. `REPORT-OK` for both stems.
- In every old row (seq 0 to 81) the entries, serialized bytes, new objects,
  new raw bytes and nodes read and written are equal to the first run on
  both systems, so the fixes WR-02 to WR-04 and IN-02 did not change what
  is written.

Old against new headline numbers (macOS, median, scatter K = 1 unless named):

| | first run | after the fixes |
|---|---|---|
| whole commit ms (CPU ms) | 61.7 (40.8) | 63.9 (42.9) |
| flat `inode_data` CPU ms | 6.1 | 6.6 |
| prolly `inode_data` CPU ms | 0.6 | 0.6 |
| estimated whole commit with prolly ms (of whole) | 56.4 (91.4 %) | 58.4 (91.4 %) |
| gain where prolly helps, ms per commit | 4.7 to 5.6 | 3.7 to 8.3 |
| 4,000,000 entries, one change: flat ms, prolly ms | 229.9, 21.8 | 242.1, 22.0 |
| flat map passes 100 ms per commit at | 1.67 M entries | 1.60 M entries |
| disk growth flat, prolly MiB | 31.2, 17.5 (81 commits, no index) | 64.4, 38.4 (120 commits, index) |
| peak RSS whole, flat, prolly MiB | 390.0, 107.8, 26.6 (82 commits) | 478.2, 175.4, 33.7 (122 commits) |

The new `insdel` rows (macOS; Linux in brackets), CPU ms and wall ms:

| K | entries changed | flat CPU ms | prolly CPU ms | flat ms | prolly ms | prolly nodes written | estimate / whole |
|---|---|---|---|---|---|---|---|
| 1 | 1 | 6.6 (6.1) | 0.6 (1.4) | 28.6 (8.1) | 20.9 (4.2) | 3 | 87.0 % (87.5 %) |
| 10 | 10 | 7.3 (7.0) | 1.9 (3.1) | 30.4 (9.3) | 22.1 (5.9) | 18 | 86.0 % (90.2 %) |
| 100 | 100 | 12.5 (12.7) | 10.1 (13.4) | 34.5 (15.4) | 30.8 (16.7) | 117 | 95.2 % (102.6 %) |
| 1000 | 1,000 | 24.2 (26.8) | 46.3 (61.7) | 49.5 (29.8) | 69.5 (64.5) | 564 | 115.7 % (130.6 %) |

Recommendation: still **later** by the rubric on the main (macOS) run, as
plan 24-05 applied it. The fixes did not change the word, but the `insdel`
rows weaken the case: on Linux, prolly is a little slower than flat at
insdel K = 100 (16.7 against 15.4 ms, 13.4 against 12.7 CPU ms), so a strict
reading of the `drop` rule on the Linux run alone would give `drop` for that
one row. The report (`docs/research/prolly-trees.md`), the design section
(`docs/design/metadata-maps.md`) and the #19 text (`24-ISSUE-19-COMMENT.md`,
not posted) say this.

---

_Reviewed: 2026-11-26_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_
