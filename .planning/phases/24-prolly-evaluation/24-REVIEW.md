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

---

_Reviewed: 2026-11-26_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_
