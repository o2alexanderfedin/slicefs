---
phase: 23-hashes-and-rehash
verified: 2026-11-26T23:01:29Z
status: passed
score: 5/5 roadmap success criteria verified (6/6 requirements satisfied)
covered_files:
  - .planning/phases/23-hashes-and-rehash/23-01-PLAN.md
  - .planning/phases/23-hashes-and-rehash/23-01-SUMMARY.md
  - .planning/phases/23-hashes-and-rehash/23-02-PLAN.md
  - .planning/phases/23-hashes-and-rehash/23-02-SUMMARY.md
  - .planning/phases/23-hashes-and-rehash/23-03-PLAN.md
  - .planning/phases/23-hashes-and-rehash/23-03-SUMMARY.md
  - .planning/phases/23-hashes-and-rehash/23-04-PLAN.md
  - .planning/phases/23-hashes-and-rehash/23-04-SUMMARY.md
  - .planning/phases/23-hashes-and-rehash/23-05-PLAN.md
  - .planning/phases/23-hashes-and-rehash/23-05-SUMMARY.md
  - Cargo.toml
  - README.md
  - crates/chunk-store/Cargo.toml
  - crates/chunk-store/src/extsort.rs
  - crates/chunk-store/src/hash.rs
  - crates/chunk-store/src/lib.rs
  - crates/chunk-store/tests/gc_full_tests.rs
  - crates/metadata/src/gc/mod.rs
  - crates/metadata/src/inode_map.rs
  - crates/metadata/src/lib.rs
  - crates/metadata/src/rehash/map.rs
  - crates/metadata/src/rehash/mod.rs
  - crates/metadata/src/segment/mod.rs
  - crates/metadata/src/segment/switch.rs
  - crates/metadata/src/store.rs
  - crates/metadata/src/store_io.rs
  - crates/metadata/src/wal/mod.rs
  - crates/slicefs-cli/Cargo.toml
  - crates/slicefs-cli/src/cli.rs
  - crates/slicefs-cli/src/content.rs
  - crates/slicefs-cli/src/gc.rs
  - crates/slicefs-cli/src/gc_pacing.rs
  - crates/slicefs-cli/src/header.rs
  - crates/slicefs-cli/src/lib.rs
  - crates/slicefs-cli/src/main.rs
  - crates/slicefs-cli/src/online_gc.rs
  - crates/slicefs-cli/src/rehash.rs
  - crates/slicefs-cli/tests/hash_change_tests.rs
  - crates/slicefs-cli/tests/rehash_tests.rs
  - crates/slicefs-cli/tests/snapshot_switch_gc_tests.rs
  - crates/slicefs-cli/tests/store_header_tests.rs
  - docs/design/chunk-store.md
  - docs/design/store-format.md
  - docs/research/gc-memory.md
  - scripts/gc-memory/measure.sh
  - scripts/gc-memory/run.sh
  - scripts/power-loss/Dockerfile
  - scripts/power-loss/README.md
  - scripts/power-loss/plshim.c
  - scripts/power-loss/powerloss.sh
  - scripts/power-loss/run.sh
covered_digest: "v4:sha256:8f17afc1fe64d5c4ad8fba844f5e664d38f9b56fbf30c0871656596263252da8"
behavior_unverified: 0
overrides_applied: 0
---

# Phase 23: Hashes and rehash Verification Report

**Phase Goal:** A store can use SHA-256 as its write hash, and `slicefs rehash` converts an offline store from one hash algorithm to another without data loss, even after a crash (#62)
**Verified:** 2026-11-26T23:01:29Z, at HEAD `be29afb` (develop `02c9d55` plus planning docs)
**Status:** passed
**Re-verification:** Yes. The report of `9ff6da8` went stale because PRs #206, #208 and #209 changed covered files. That report had no `gaps:`, so this is a full check of all criteria against the new code.

## What changed since 9ff6da8 (in the rehash path)

- `metadata::gc::mark` (gc/mod.rs:160) no longer keeps a `Vec<ObjectKey>` of all keys. `for_each_referenced` (store.rs:1608) streams each key into `ChunkStore::gc_sorter` (4 MiB in RAM, then sorted temp files in `<store>/gc-tmp`). The sorted keys are then read back one at a time.
- `check_temp_space` (gc/mod.rs:110) runs before the walk. It calls `gc_temp_space_check`, which returns Ok when `need == 0` (the sort fits in RAM) or when the free space is unknown. `content::open_inner` sets `gc_free_space` for every writable open, rehash included.
- rehash.rs:254 now removes `rehash-tmp` before the step 6 GC, not after it. The engine map is dropped when `rehash_roots` returns, so nothing uses those files at that point.
- `NodeRead::chunk_store()` was added (store_io.rs). `main` calls `alloc_tune::tune_at_start()` before any command runs.

## Observable Truths (ROADMAP success criteria)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | `--hash sha256` on seed and mount (own tag, hardware path where the CPU has it); BLAKE3-256 stays the default | VERIFIED | `chunk_store::hash::Sha256` (tag 0x12) is in `HASHERS`, and `DEFAULT_HASHER` is BLAKE3-256. Neither changed since 9ff6da8. Tests: chunk-store lib, 134 passed and 1 ignored (`sha256_speed`, golden vectors included). hash_change_tests: 7 passed. CLI `header::`: pairing tests passed. The hardware speed was measured on CI and is recorded below. |
| 2 | `rehash --to` converts BLAKE3 to SHA-256 and back; a mixed store ends with one hash; fresh-write keys; files and snapshots do not change; scrub passes | VERIFIED | rehash_tests 25/25 passed, including `round_trip_*` (both directions, mixed store, large profile) through `rehash_and_check` and `assert_fresh_write_keys`. The rewrite engine did not change since 9ff6da8. |
| 3 | Refuses while mounted (OS lock) and with write-log records after the last root | VERIFIED | `header::lock_offline` (rehash.rs:144) is held to the end of the run. `scan_store_with_log` (rehash.rs:157) refuses a non-empty log. The `refuse_*` tests pass in rehash_tests. |
| 4 | A crash at any point leaves the old or the new root; GC frees old objects after the switch; old objects are verified as they are read | VERIFIED | The run order is unchanged: flag, rewrite, `cs.sync()`, switch (rehash.rs:237), header line, then the full GC (rehash.rs:257) with the new roots. The `crash_*` tests and `verify_a_damaged_object_stops_before_the_switch` pass. The new GC code fails closed: `a_sorter_io_error_in_the_mark_fails_the_gc_and_deletes_nothing` (gc.rs) and `a_mark_without_space_for_its_temp_files_does_not_start` (gc/mod.rs) pass. If the GC is refused after the switch, the new roots stay, and a rerun does the GC again (the same state as `crash_after_each_step_then_rerun` at the GC step). Power loss: the owner records PR #206 (see below). |
| 5 | Progress, bytes rewritten and bytes freed are reported; `--dry-run` changes nothing; memory is bounded, with no in-RAM map of all objects | VERIFIED | The report, progress and dry-run tests pass in rehash_tests. The engine map is the on-disk `KeyMap` with a fixed cache (`CACHE_SLOTS = 1 << 14`). The step 6 GC mark is now bounded too: its keys go through the extsort (see above). Tests: `the_streamed_mark_gives_the_live_set_of_the_key_lists`, `a_mark_that_fits_in_ram_makes_no_temp_file`, `a_stop_in_the_key_merge_ends_the_mark_and_deletes_nothing`, `an_error_or_a_stop_removes_the_temp_files`, and chunk-store gc_full_tests 7/7. docs/design/chunk-store.md (Rehash, Memory) states the disk use of the temp files. What is left for rehash is per-object state in the full GC (the whole `.idx` decode). It is not a map of all keys, and it is tracked in #207. |

**Score:** 5/5 truths verified (0 present, behavior-unverified)

The plan-level deviations of the earlier report did not change. The 23-03 dry-run writes its map in the system temp dir (WR-02), and the 23-04 same-roots switch removes old segments that a crash left (IN-02). Both refine the plan text. Neither is a gap.

## Artifacts, key links, requirements

- `verify.artifacts`: exit 0 on all 5 plans (5/5, 4/4, 4/4, 4/4, 4/4).
- `verify.key-links`: exit 0 on all 5 plans (3/3, 3/3, 4/4, 5/5, 2/2).
- HASH-01..06 are all SATISFIED (HASH-01 by SC1, HASH-02 by SC2, HASH-03 by SC3, HASH-04 by SC4, HASH-05 by SC5, HASH-06 by the `round_trip_*` tests). REQUIREMENTS.md maps only these six IDs to Phase 23, so no requirement is orphaned. The "Pending" marks are bookkeeping.

## Plan prohibitions

All plan prohibitions have `status: enforced` and a deterministic check. The ones about same-wave file ownership and plan-base diffs (23-01, 23-02, 23-03, 23-05) apply to the plan runs and were checked then. I checked the ones that still apply at HEAD:

- 23-03 D-04 (no in-RAM list of all objects in the engine): `crates/metadata/src/rehash/` has no change since 9ff6da8.
- 23-04 (rehash.rs does not call `gc_report`, `run_gc_mode`, `GarbageCollector` or `content::store_io`): the negative grep gives 0.
- 23-04 (the dry-run writes no file in the store): `dry_run_changes_nothing_and_reports_an_estimate` passes.
- 23-01 D-07: `scripts/safety/unsafe-count.sh` gives `result=ok` (files=32 tokens=134; the count grew with later PRs, and the gate accepts it).

## Behavioral Spot-Checks (run at HEAD be29afb)

| Command | Result |
|---------|--------|
| `cargo test -p slicefs-cli --test rehash_tests --test hash_change_tests --test gc_cli_tests --test snapshot_switch_gc_tests` | 25 + 7 + 5 + 1 passed, 0 failed |
| `cargo test -p chunk-store --test gc_full_tests --lib` | 7 passed; lib 134 passed, 1 ignored (`sha256_speed`) |
| `cargo test -p metadata --lib -- gc:: rehash:: segment::` | 32 passed |
| `cargo test -p slicefs-cli --lib -- header:: gc::` | 32 passed |

I did not run the full workspace gate. The orchestrator reports 1,696 passed on the develop content. Probe step: not applicable (there is no `scripts/*/tests/probe-*.sh`). The power-loss script runs in Docker with LazyFS, so I did not run it here.

## Anti-Patterns

The lines added since 9ff6da8 in the covered files have no TBD, FIXME, XXX, TODO, HACK, `todo!` or `unimplemented!`. One Info item from before this phase is still there: header.rs `lock_result` grants the lock when the OS returns `Unsupported`.

## Gaps Summary

None. All five criteria and all six requirements hold at HEAD with the code of #206, #208 and #209. The #209 change closes the open question of the earlier report about the GC RAM.

---

_Verified: 2026-11-26T23:01:29Z_
_Verifier: Claude (gsd-verifier)_

## Human items: resolved (2026-11-26)

These results are recorded by the owner. I did not run them in this verification.

| Item | Result |
|---|---|
| 1. Hardware SHA-256 speed | Measured on GitHub runners with SHA instructions, on 8 KiB blocks: ubuntu-latest (Xeon 8573C, `sha_ni`) 1484 MB/s, macos-latest (Apple M1, `FEAT_SHA256`) 1734 MB/s, ubuntu-24.04-arm (`sha2`) 1855 MB/s. |
| 2. Power loss before the sync (IN-04) | Tested in Docker with LazyFS and an fsync/rename kill shim (`scripts/power-loss/`, PR #206). All 49 crash points mount with the old or the new roots (49/49 ok). |
| 3. RAM of the full GC step | Fixed in PR #209 (#207): the mark keys go through the external sort into `gc-tmp`. `gc --full` heap is 109 MB at 1M files with 1 or 4 snapshots; rehash RSS went from 832 to 337 MiB. Still open in #207, as follow-ups and not phase gaps: the whole `.idx` decode and the Bloom filters (about 1.25 B per object). `scrub` has the same key list: #210. |
