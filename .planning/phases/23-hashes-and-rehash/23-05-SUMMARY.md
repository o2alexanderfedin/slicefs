---
phase: 23-hashes-and-rehash
plan: 05
status: complete
subsystem: slicefs-cli tests (rehash end to end), design docs
tags: [rehash, sha256, crash-safety, HASH-06, HASH-04, HASH-02, HASH-05]
requires:
  - 23-04 (slicefs_cli::rehash, the Step hooks, content::remove_rehash_tmp)
  - 23-03 (metadata::rehash engine)
  - 23-02 (switch segment, last record per snapshot version, gc_on_open_store)
  - 23-01 (Sha256, the hash-sha256 flag)
provides:
  - "HASH-06 tests: round_trip_* (both ways, a three-hash store, two profiles)"
  - "HASH-04 tests at the command level: crash_* and verify_*"
  - "docs/design/chunk-store.md section Rehash (#62); SHA-256 in the hash section"
  - "docs/design/store-format.md: rehash in the lock texts M13 to M15 and the command lists; rehash-tmp"
affects: [phase 23 verification]
tech-stack:
  added: []
  patterns: [crash states by the hidden Step hooks, then a rerun with no hook; a writable open with no commit to check a crash state]
key-files:
  created:
    - .planning/phases/23-hashes-and-rehash/runs/red-23-05.log
    - .planning/phases/23-hashes-and-rehash/runs/gate-23-05.log
  modified:
    - crates/slicefs-cli/tests/rehash_tests.rs
    - docs/design/chunk-store.md
    - docs/design/store-format.md
    - README.md
decisions:
  - "No source change: every test passed at once, and each has a guard mutation with a red run in runs/red-23-05.log"
  - "assert_opens checks a crash state with no commit: the read-only dumps, a writable content::open + close (as a mount, it removes rehash-tmp) and scrub"
  - "The three-hash test also rehashes a copy of the store to SHA-256 first (the write hash of its header), with a SHA-256 file root over chunks of all three tags"
  - "Expect::New asserts that every snapshot root has the target tag, not only the current root"
  - "The local gate of the final tree (gate 2) ran after the docs were edited and counts as the phase gate; no code changed after it"
metrics:
  duration: about 2 h
  completed: 2026-11-26
estimate:
  tokens: 160000
  tasks: 3
actuals:
  tokens: 14900    # chars/4 of git diff c8795f4..f808f3d (59674 chars)
  tasks: 3
  commits: 3
plan_head_before: c8795f4
plan_head_after: f808f3d
---

# Phase 23 Plan 05: Rehash end to end and docs Summary

The tests prove the phase end to end. BLAKE3-256 and SHA-256 stores convert both ways, and a store with three hashes ends with one hash. After each run the files, the snapshots, scrub and the fresh-write keys stay correct, for two chunk profiles. A crash after each step of the run leaves the old roots or the new roots, and a rerun finishes. A damaged object stops the run before the switch. The design docs and README describe SHA-256 and `slicefs rehash`.

BASE = `c8795f4`, the merge of plans 01-04 on `feature/phase-23-hashes-rehash`. It was recorded before the first edit. The branch is `exec/23-05` and was not pushed.

## Commits

| Task | Commit | Subject |
|------|--------|---------|
| 1 | 1fe2a42 | test(23-05): HASH-06 round trips, three-hash store, fresh-write keys for two profiles |
| 2 | 5a4dbb9 | test(23-05): crash after each step and inside the rewrite, torn switch, damaged object |
| 3 | f808f3d | docs(23-05): rehash and SHA-256 in the design docs and README; phase gate |

## Tests (`crates/slicefs-cli/tests/rehash_tests.rs`: 23 tests, about 26 s)

- **Helpers:**
  - `loader_roots`, `live_ids` and `live_tags`. `all_tags` now calls `live_tags`.
  - `rich_store(hash, profile)` makes these files: a 400 KiB random file, a 10-byte file, an empty file, a 3 MiB sparse file with 64 KiB of data at 1 MiB, and a subdirectory with two equal files. Then it adds an xattr and spliced edits, a new file, snapshot `s1`, a new file and an edit, snapshot `s2`, and one more edit.
  - `record` / `same_store` check the snapshot list (version, name, `created_at`), the dumps of each root, and scrub.
  - `assert_fresh_write_keys` uses `content::put_all` in the store opened by `content::open`, so the header hash and the header profile apply.
  - `rehash_and_check`: the run leaves one tag, no old-tag ID of the old roots is in the index, `dead left: 0`, `same_store` holds, the keys are fresh-write keys, the header has the `hash` line, and no `rehash-tmp` is left.
- **Round trips:**
  - `round_trip_blake3_to_sha256_and_back`. The flag stays after the run back to BLAKE3-256.
  - `round_trip_sha256_to_blake3_and_back`.
  - `round_trip_mixed_store_ends_with_one_hash`: tags `{0x1e, 0xf0, 0x12}`. The store goes to blake3-256, then to sha256, and a copy goes to sha256.
  - `round_trip_large_profile_keys_equal_fresh_writes`: `ChunkProfile::new(16384, 65536, 262144)`.
- **Crash:**
  - `crash_after_each_step_then_rerun`: all 7 `Step` values, `Step::Checked` to `Step::GcDone`. `Checked` to `Synced` give the old roots and `Switched` to `GcDone` give the new roots. The test also checks the header (flag, hash line) at each step.
  - `crash_inside_the_rewrite_then_rerun` uses `stop_after_values: Some(3)`.
  - `crash_with_a_stale_seg_tmp_then_rerun`.
  - `crash_with_old_segments_left_after_the_switch`.
  - After each case, a rerun with no hook gives the final state: tag 0x12, the same store, `hash sha256` and the flag, no `rehash-tmp`, no `.seg.tmp`.
- **Verify:** `verify_a_damaged_object_stops_before_the_switch`.
  - Setup: one byte is flipped 100 bytes into the record of the largest chunk, in the sealed pack. The precheck gives `Corrupt` with the text `damaged data: object ...`.
  - The real run fails with `Nothing was switched`, `file content` and that cause. The segment files stay byte for byte the same, and the header keeps `hash blake3-256`.
  - After the run, the object still reads as `Corrupt` with the same cause. The dry-run fails with the same kind and cause.

## TDD record (runs/red-23-05.log)

Every test passed at once, because plan 23-04 built the command, so each test is a guard. Each mutation was undone after its red run, and `git status` then showed no change under `crates/*/src`.

- **Task 1:**
  - A: the switch writes the old snapshot roots. All four round trips go red.
  - B2: a value is kept on its own tag alone. The mixed test goes red.
  - C: the conversion store uses the default profile. The large-profile test goes red.
- **Task 2:**
  - G2: no `remove_switch_tmps`. The stale-tmp test goes red.
  - G3: a switch is written on an engine error. The verify test goes red.
  - G4: read errors become `Missing`. The verify test goes red.
  - G5: the real run drops `stop_after_values`. The inside-the-rewrite test goes red.
  - G6: the loaders keep the first record of each snapshot version. The old-segments test goes red.
  - G7: the flag is written after the rewrite. The each-step test goes red.
- **Mutations that stayed green:**
  - G1 (no sync before the switch): a hook stop loses no written bytes in process. The engine test of 23-03 with `MemIo::crash` covers the sync order.
  - The plan's loader-rule and manifest-list examples: the log gives the reasons.

## Gates (runs/gate-23-05.log)

- Local gate (4 standard skips), exit 0:
  - Task 1 tree: 1663 passed, 0 failed, 13 ignored.
  - Final tree: 1668 passed, 0 failed, 13 ignored, 81 result lines.
- `RUST_STABLE=stable scripts/safety/clippy.sh` passed at each commit: `CLIPPY os=Darwin toolchain=stable result=ok` and `UNSAFE files=31 tokens=132 result=ok`. `docs/design/unsafe-review.md` did not change (`diff_exit=0`).
- `cargo build --workspace --locked` exits 0.
- `scripts/safety/kani.sh` (full run): `KANI ok harnesses=9`.
- The doc greps give these counts:
  - `## Rehash`: 1;
  - `is future work`: 0;
  - `gc, seed, rehash, snapshot create or snapshot switch` in store-format.md: 3;
  - `rehash-tmp`: 3 in chunk-store.md and 1 in store-format.md;
  - `slicefs rehash` in README: 1.
- No home path, user name or license word is in the logs or in the added doc lines.
- Not run here: the hand check of the hardware SHA-256 path. It needs a CPU with SHA extensions: `cargo test --release -p chunk-store --lib -- --ignored sha256_speed --nocapture` (over 1,000 MB/s means a hardware path).

## Deviations from Plan

1. **[Rule 3 - Blocking] Branch name.** The plan names `feature/phase-23-hashes-and-rehash`; the real branch is `feature/phase-23-hashes-rehash`. `exec/23-05` was made from it at `c8795f4`, and the merge-base check uses the real name.
2. **Mixed-store test, wider than the plan (reversible).** The plan's guard for this test (keep a target-tag manifest list) cannot fire. A manifest list has the hash of its own content key, and a run to blake3-256 from a store whose header says sha256 finds no blake3 key-carrying node over old keys. So the test changed in two ways:
   - Under SHA-256 it splices `old.bin` once more, so its file root has tag 0x12 over chunks of all three tags.
   - It also rehashes a copy of the store straight to sha256.
   
   The plan's run order (to blake3-256, then to sha256) is kept.
3. **Guard mutations differ from the plan's examples.** Some of the plan's examples cannot turn a test red here: the loader rule in Task 1, the manifest list, and sync after the switch. Other mutations took their place. The red log gives each one and why.
4. **`assert_opens` does no commit.** It does not call `open_fs`/`close_fs`, because a commit writes a new root under the header hash and removes the old segments that the test put back. In its place it uses the read-only dumps of each root, plus `content::open(..).close()`: that is the writable open of a mount, and it removes `rehash-tmp`.
5. **One local gate for Task 2 and Task 3.** The full gate ran on the final tree: the Task 2 tests plus the Task 3 docs, which were already in the tree. Task 3 changed only docs and the gate log after that, so this run is the phase gate. Clippy, the `--locked` build and Kani also ran on the final tree.
6. **Process.**
   - The plan commit ledger file was not written. `plan_head_before` is BASE, and `commits` comes from `git rev-list --count c8795f4..f808f3d`.
   - STATE.md, ROADMAP.md and REQUIREMENTS.md were not touched, as the orchestrator said.

No source file changed: `rehash.rs` and `rehash/mod.rs` are the same as at BASE.

## Known Stubs

None.

## Threat Flags

None. The plan adds tests and docs only.

## For the owner

- **The `hash-sha256` flag.** It stays after a rehash back to BLAKE3-256 (D-11, tested). Its removal is still deferred.
- **`HASH_SHA256_RELEASE`** is still the placeholder `v2.2`.
- **The `Rewritten:` report count** still counts nodes that keep their key. The chunk-store doc says so.
- **Stale refcount rows** are still dropped and counted (as 23-03 and 23-04 did).
- **No in-process sync test.** A missing sync before the switch cannot be shown by a hook stop at the command level. Only the engine-level `MemIo` crash test covers it. A command-level test would need a crash-aware `PackIo` under `content::open_with_hasher`. That is a test seam that does not exist yet. Reversible; your choice.

## Self-Check: PASSED

- Files: `crates/slicefs-cli/tests/rehash_tests.rs`, `runs/red-23-05.log`, `runs/gate-23-05.log`, `docs/design/chunk-store.md` (with `## Rehash (#62)`), `docs/design/store-format.md` and `README.md` exist.
- Commits 1fe2a42, 5a4dbb9 and f808f3d are ancestors of HEAD on `exec/23-05`. `git rev-list --count c8795f4..f808f3d` is 3.
