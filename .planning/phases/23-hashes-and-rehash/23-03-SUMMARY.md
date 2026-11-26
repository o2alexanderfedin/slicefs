---
phase: 23-hashes-and-rehash
plan: 03
subsystem: metadata (rehash engine), chunk-store (extsort visibility)
status: complete
tags: [rehash, hash, bounded-memory, extsort, dry-run]
requires: []
provides:
  - "metadata::rehash::rehash_roots, RootSet, RehashOptions, Mode, Stats, RehashError, WINDOW"
  - "metadata::rehash::map::{KeyMap, Kind, SLOT, CACHE_SLOTS, START_SLOTS, MAP_FILE}"
  - "pub chunk_store::extsort (TmpDir, Sorter, SortedFile, RecReader)"
affects: [23-04 (CLI command, switch, GC, report), 23-05 (HASH-06 end to end)]
tech-stack:
  added: []
  patterns:
    - "stream every value FileReader -> FileWriter in 1 MiB windows (fresh-write keys)"
    - "on-disk open-addressing table with a fixed direct-mapped RAM cache"
    - "external sort of refcount rows with a row index (no lost equal rows)"
key-files:
  created:
    - crates/metadata/src/rehash/mod.rs
    - crates/metadata/src/rehash/map.rs
    - .planning/phases/23-hashes-and-rehash/runs/red-23-03.log
    - .planning/phases/23-hashes-and-rehash/runs/gate-23-03.log
  modified:
    - crates/chunk-store/src/lib.rs
    - crates/chunk-store/src/extsort.rs
    - crates/metadata/src/lib.rs
    - crates/metadata/src/store.rs
    - crates/metadata/src/inode_map.rs
decisions:
  - "Stats.values_rewritten counts every rewritten value: contents, byte-copied nodes, key-carrying nodes and root records; the stop hook uses the same count"
  - "Dry-run other_tag_bytes adds the length of a value only when its tree has at least one object of another tag"
  - "The engine leaves <map_dir>/map in place; the caller (23-04) removes the directory. The refcount sort dirs are removed on drop"
  - "The canonical-form check (load_from_root(new) then commit() gives the same root and no new object) holds and is kept as an assertion in the tracer test"
  - "SortedFile::len stays test-only and is pub(crate) (clippy len_without_is_empty once the module is public)"
metrics:
  duration: "about 55 minutes"
  completed: 2026-11-26
estimate:
  tokens: 200000
  tasks: 3
actuals:
  tokens: 18800
  tasks: 3
  commits: 3
plan_head_before: 526609e4de6bba028cb4c1cc049631cffbecb51f
plan_head_after: 14f6a0708d0ca929256c79ba09b89fa404aff395
---

# Phase 23 Plan 03: Rehash engine Summary

The rehash engine in `metadata::rehash`: every value of the current root and of each snapshot root is streamed through `FileReader` (verified reads) into a `FileWriter` of the target store, so each new key is the key of a fresh write. The key-carrying nodes are translated in windows of whole records. An on-disk old-to-new map with a fixed RAM cache does shared values once, and a dry-run mode writes nothing.

BASE = `526609e` (`git merge-base HEAD feature/phase-23-hashes-rehash`), recorded before the first edit.

## What was built

- `rehash_roots(cs, roots, opts) -> (RootSet, Stats)`.
  - **Byte copy of values.** File content and the inode, directory and xattr nodes are copied in 1 MiB windows into a new `FileWriter`. There is no chunk-reuse path (R-04).
  - **Key-carrying nodes** are always translated, also when their own tag is the target. These are the manifest lists, the four `u64 -> key` maps, the inode map, the refcount table and the root record. The lengths are checked with checked arithmetic before any large read, and the `ino` values must be strictly ascending.
  - **Refcount rows.** Each row is 49 bytes (new key, count BE, row index BE). The rows are sorted with `chunk_store::extsort::Sorter` and merged by saturating sum. A row whose read gives `Missing` is dropped and counted in `stale_refcounts`.
  - **Kept as is.** If a value's own key and every object in its tree already have the target tag, the value is kept. The engine reads its nodes to check the tags and reads no chunk.
  - **Progress.** The callback runs at a fixed interval, and also at the start and end of each root and once at the end.
  - **Crash-test hook.** The hidden `stop_after_values` hook stops the run for crash tests.
  - **Errors.** A read error is `RehashError::Read { what, key, source }`. It keeps the `chunk_store::Error` kind, and its text names the node kind.
- `map::KeyMap` (D-16, R-02):
  - The key is (kind, old key) and the value is the new key. Each slot is 67 bytes, and kind 0 means empty.
  - The table uses open addressing in `<map_dir>/map`, with reads and writes at offsets. At half load it grows to twice the size by one sequential scan.
  - A direct-mapped cache of `CACHE_SLOTS` (2^14, about 1.1 MB) sits in front of the file.
  - `cache_only()` is for the dry-run.
- `Mode::DryRun` uses no map file, no sorter, no writer and no put. It verifies what a real run reads, and counts the objects of another tag and the bytes of their values.
- Visibility only:
  - `pub mod extsort`. `TmpDir`, `Sorter`, `SortedFile` and `RecReader` are `pub`.
  - `pub(crate)` for `root_rec`, `MAP_ENTRY` and the inode map `RECORD`.

## Tasks and commits

| Task | Name | Commit |
| ---- | ---- | ------ |
| 1 (tracer) | One root rehashes to the same tree (byte copy, key-carrying nodes, refcount sort, root record) | 6ceac0c |
| 2 | On-disk map with a fixed RAM cache; shared values; kept-as-is rule | 1d1302f |
| 3 | Dry-run, progress, stop hook, error stops, durability after sync; gates | 14f6a07 |

Tracer gate: the full Task 1 `<verify>` (crate tests, greps, clippy, local gate) ran once, before the Task 1 commit. Before expansion, only the three tracer tests ran again, in the first Task 2 test run, and they passed.

## TDD record (runs/red-23-03.log)

- **Task 1:** before the code, all three tests were red (a stub returned an error). A guard mutation with 41-byte rows and no row index turns `two_old_keys_of_one_content_merge_their_counts` red (count 1, not 2).
- **Task 2:**
  - With the Task 1 stand-in, `a_target_tag_value_is_kept` was red.
  - A guard mutation that makes the map ignore the kind turns `a_file_byte_equal_to_a_node_gets_both_translations` red.
  - The map tests did not compile before `KeyMap` existed.
- **Task 3:** `dry_run_writes_nothing_and_counts` was red before the dry-run counts. One run combined four guard mutations, and each turned its test red:
  - no progress calls;
  - no stop hook;
  - read errors turned into strings;
  - no `cs.sync()` in the crash test.

## Canonical-form try (RESEARCH A5)

It holds. `commit()` does not return early: it writes every map again. After a rehash, `load_from_root(new)` then `commit()` returns `new` and adds no object. So the check is strong, and it stays as an assertion in `a_root_rehashes_to_the_same_tree`.

## Gates

- `cargo test -p metadata --lib -- rehash::` gives 17 passed.
- The awk gate (whole-value or whole-map reads in engine code) gives 0.
- There is no change in `segment/`, `wal/` or `hash.rs` since BASE (`diff_exit=0`).
- `RUST_STABLE=stable scripts/safety/clippy.sh` gives `CLIPPY ... result=ok` and `UNSAFE files=31 tokens=132 result=ok`.
- Local gate (`cargo test --workspace` with the 4 standard skips): exit 0, 79 test binaries, 1625 passed, 0 failed, 12 ignored.

## Deviations from Plan

1. **[Rule 3 - Blocking] Branch base.**
   - The plan names `feature/phase-23-hashes-and-rehash`, but the real branch is `feature/phase-23-hashes-rehash`. All merge-base and diff checks use the real name; BASE is `526609e`.
   - The worktree started at `ee2d0ec` (main). `exec/23-03` was made and then reset to the phase branch before any edit or commit.
2. **[Rule 3 - Blocking] Clippy findings from the visibility change.**
   - `SortedFile::len` (test-only) became `pub(crate)`, because `len_without_is_empty` fires once the type is public.
   - Constant-size `chunks_exact` became `as_chunks::<N>()`.
   - Neither change alters behavior.
3. **The Task 1 stand-in** was a fixed in-RAM direct-mapped map keyed by (kind, old key), as the plan allowed. Task 2 replaced it, and `Kind` moved to `map.rs`.
4. **Gate flake under load.** The first Task 2 gate run failed two timing tests in `slicefs-cli/tests/flusher_tests.rs`. The machine load average was about 37, from the parallel executors. These tests passed alone (26 of 26) and in the rerun of the full gate. No code change was made for them; this plan does not touch that code.

## Known Stubs

None.

## Threat Flags

None. The map file is a per-run temp file in a directory that the caller gives, and the engine adds no network or format surface.

## Open questions for the owner

- None are one-way doors. The map slot layout and the sort rows are per-run temp data.
- The stale-refcount policy (RESEARCH A4) is "drop and count". If the owner wants a stop on any missing refcount object instead, it is a one-line change.

## Self-Check: PASSED

- The files exist: `crates/metadata/src/rehash/mod.rs`, `crates/metadata/src/rehash/map.rs`, `runs/red-23-03.log`, `runs/gate-23-03.log`.
- The commits exist on `exec/23-03`: 6ceac0c, 1d1302f, 14f6a07.
