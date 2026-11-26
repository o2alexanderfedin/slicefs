---
phase: 23-hashes-and-rehash
verified: 2026-11-26T17:02:52Z
status: passed
resolved: 2026-11-26
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
  - crates/metadata/src/inode_map.rs
  - crates/metadata/src/lib.rs
  - crates/metadata/src/rehash/map.rs
  - crates/metadata/src/rehash/mod.rs
  - crates/metadata/src/segment/mod.rs
  - crates/metadata/src/segment/switch.rs
  - crates/metadata/src/store.rs
  - crates/metadata/src/wal/mod.rs
  - crates/slicefs-cli/Cargo.toml
  - crates/slicefs-cli/src/cli.rs
  - crates/slicefs-cli/src/content.rs
  - crates/slicefs-cli/src/gc.rs
  - crates/slicefs-cli/src/header.rs
  - crates/slicefs-cli/src/lib.rs
  - crates/slicefs-cli/src/main.rs
  - crates/slicefs-cli/src/rehash.rs
  - crates/slicefs-cli/tests/hash_change_tests.rs
  - crates/slicefs-cli/tests/rehash_tests.rs
  - crates/slicefs-cli/tests/snapshot_switch_gc_tests.rs
  - crates/slicefs-cli/tests/store_header_tests.rs
  - docs/design/chunk-store.md
  - docs/design/store-format.md
covered_digest: "v4:sha256:f2ac3d2ee0a32ab638ae1493b1f38127563a26e8a560fa3548df54eafef27b9b"
behavior_unverified: 0
overrides_applied: 0
human_verification:
  - test: "On a CPU with SHA extensions (x86_64 SHA-NI, or any ARMv8 with SHA2, e.g. Apple Silicon), run `cargo test --release -p chunk-store -- --ignored sha256_speed --nocapture`"
    expected: "The printed speed is over 1,000 MB/s on 8 KiB blocks (the hardware path of sha2). About 250 MB/s means the soft path."
    why_human: "The dev machine (Intel Xeon W-3245, Cascade Lake) has no SHA-NI, so only the soft path runs here. sha2 0.11.2 does not expose which backend it picked; the run-time detection is in the crate code (src/sha256.rs, cpufeatures `sha`+`sse4.1` / `sha2` hwcap), and no `sha2_256_backend` cfg is set in .cargo/config.toml or scripts/."
  - test: "IN-04 (deferred by the review): power loss before the chunk-store sync of a rewrite. Engine test: run `rehash_roots` in `Mode::Rewrite` on `MemIo` with no `cs.sync()`, call `mem.crash()`, reopen; the old root must give the same tree, and a second `rehash_roots` must give the same new root as a clean run."
    expected: "The old root loads and reads the same files; a rerun completes with the clean-run root."
    why_human: "The crash tests stop the process between steps (the unsynced pack bytes stay in the page cache). The only power-loss model (`new_objects_survive_a_crash_after_sync`) runs after the sync. A pre-sync power loss is not tested. The owner must accept the deferral or schedule the test; no later roadmap phase covers it."
  - test: "Owner decision: accept the RAM bound of the full GC at rehash step 6. `metadata::gc::mark` (gc/mod.rs:112-123) collects the keys of every metadata node and file content of all roots in one `Vec<ObjectKey>` (33 bytes each), sorted and deduplicated in RAM; only the object IDs of the mark go to the external sort."
    expected: "The owner accepts that rehash RAM is O(file count + node count) at 33 bytes each in the GC step (about 330 MB for 10 million files), as docs/design/chunk-store.md (Rehash, Memory) states, or asks for a bounded mark."
    why_human: "HASH-05 says 'memory is bounded (no in-RAM map of all objects)'. The engine meets this: no old-to-new map in RAM and no list of all chunks. But the GC step that the command always runs keeps a list in RAM that grows with the file count. That GC code is older than this phase. Whether 'bounded' must include it is a scope decision for the owner."
---

# Phase 23: Hashes and rehash Verification Report

**Phase Goal:** A store can use SHA-256 as its write hash, and `slicefs rehash` converts an offline store from one hash algorithm to another without data loss, even after a crash (#62)
**Verified:** 2026-11-26T17:02:52Z
**Status:** human_needed
**Re-verification:** No (initial verification)

The code and the tests at HEAD `9ff6da8` give the phase goal. All five success criteria and all six requirements have code evidence and passing tests that I ran. Three items need a human: the hardware SHA-256 speed check, the IN-04 power-loss-before-sync case that the review deferred, and an owner decision on the RAM bound of the full GC step.

## Goal Achievement

### Observable Truths (ROADMAP success criteria)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | A store can be seeded or mounted with `--hash sha256` (own tag, hardware-accelerated where the CPU has it); BLAKE3-256 stays the default | VERIFIED | `chunk_store::hash::Sha256` (name `sha256`, tag 0x12, `sha2::Sha256::digest`) is in `HASHERS` of all builds (hash.rs). `DEFAULT_HASHER = &Blake3_256` does not change. `--hash` on seed and mount uses `parse_hash` (cli.rs:96, 157). `seed.rs:65` calls `check_write_with` and `mount.rs:133` calls `select_hash`. Both write the hash line and the incompat `hash-sha256` flag in one atomic header write (`add_hash_flag`, `marker_with`). `header::parse` enforces the pairing rule. sha2 0.11.2 `src/sha256.rs` selects SHA-NI or ARMv8 SHA2 at run time through `cpufeatures`, and no backend cfg is forced. Tests I ran: `sha256_golden_vectors` (4 FIPS vectors), `every_known_hasher_round_trips`, the 4 `sha256_*` tests in hash_change_tests (seed, mount, flag stays, ensure_hash_flag), and 4 header pairing tests. All pass. The hardware path is a human item. |
| 2 | `rehash --to` converts BLAKE3-256 to SHA-256 and back, a mixed store ends with one hash, a rehashed file gets the fresh-write key, files and snapshots do not change, scrub passes | VERIFIED | Engine (`metadata::rehash`): every value is streamed `FileReader` -> `FileWriter` (fresh-write keys). All 6 key fields of the root record are translated (INODE_MAP, INODE_DATA, DIR_DATA, XATTR_DATA, MANIFEST_DATA, REFCOUNTS; store.rs root_rec). `InodeMeta` holds no keys. The switch writes the new current root and every snapshot root. Tests: `round_trip_blake3_to_sha256_and_back`, `round_trip_sha256_to_blake3_and_back`, `round_trip_mixed_store_ends_with_one_hash` (live tags {0x1e, 0xf0, 0x12} -> one tag) and `round_trip_large_profile_keys_equal_fresh_writes`. Each one uses `rehash_and_check`, which checks one live tag, no old-tag object left, `dead left: 0`, equal dumps of every root, an equal snapshot list (version, name, created_at), `run_scrub`, and `assert_fresh_write_keys` (`put_all(get_all(k)) == k` for every content key of every root). All pass. |
| 3 | Rehash refuses while the store is mounted (OS file lock) and while the write log has records after the last root | VERIFIED | rehash.rs:144 takes `header::lock_offline`, an exclusive `File::try_lock` on the store directory (`try_store_lock`, header.rs:587). It holds the lock to the end of the run, GC included. A write mount takes `lock_write_mount` (mount.rs:103) and a read-only mount takes `lock_read_mount` (mount.rs:446), on the same OS lock. rehash.rs:156-160 runs `scan_store_with_log` and refuses on a non-empty log. A `mount.lock` file is refused too. Tests: `refuse_while_the_store_lock_is_held_exclusive`, `..._shared`, `refuse_with_a_mount_lock_file`, `refuse_with_write_log_records_after_the_last_root`, `refuse_an_unknown_target`, `refuse_an_unknown_header_hash`, `refuse_a_path_with_no_store`. Each one checks that the store tree does not change. All pass. |
| 4 | A crash at any point leaves a store that mounts with the old or the new root; GC frees old objects after the switch; old objects are verified as they are read | VERIFIED | Run order (rehash.rs): checks, flag, rewrite, `cs.sync()`, switch, header line, full GC. The switch (`write_switch_segment`) writes a `.seg.tmp`, syncs it, renames it, syncs the directory, then removes the older segments. Both loaders keep the last `SnapshotRecord` per version (`last_per_version`). FORMAT writes are atomic (temp file, sync, rename, dir sync; header.rs:691). `gc_on_open_store(&cs, roots, true)` always runs with the new roots. Reads go through `FileReader` (verified). Tests: `crash_after_each_step_then_rerun` (all 7 steps, with old or new roots, same files, scrub, then a rerun that finishes), `crash_inside_the_rewrite_then_rerun`, `crash_with_a_stale_seg_tmp_then_rerun`, `crash_with_old_segments_left_after_the_switch`, `verify_a_damaged_object_stops_before_the_switch` (Corrupt error, no segment change, hash line unchanged). Engine and switch unit tests: `new_objects_survive_a_crash_after_sync`, `old_segments_left_after_the_rename_lose_to_the_switch`, `a_stale_switch_tmp_is_removed_by_the_next_switch`, `a_damaged_object_stops_the_run`. All pass. Limit: these tests model a process crash only. A power loss before the sync (IN-04) is a human item. |
| 5 | Rehash reports progress, bytes rewritten and bytes freed; `--dry-run` changes nothing; memory is bounded, with no in-RAM map of all objects | VERIFIED | The report has lines for read values and bytes, `Rewritten: N values; new objects: N (B bytes stored)`, kept values, stale rows, the GC line, `Bytes freed: N` and the disk usage before and after. Progress goes to stderr at a fixed interval and at each step. The dry-run uses `open_read_only` and puts its map in a `tempfile` directory outside the store. The old-to-new map is `KeyMap`: an on-disk open-addressing table with a fixed direct-mapped RAM cache (`CACHE_SLOTS = 1<<14`). Refcount rows go through the bounded extsort, and each value has one 1 MiB window. Tests: `report_has_the_counts_and_bytes_freed`, `progress_lines_come_at_each_step`, `dry_run_changes_nothing_and_reports_an_estimate` (BLAKE3 tree of every store file is equal), `dry_run_counts_equal_the_real_run`, `dry_run_refuses_like_a_real_run`, `the_cache_stays_fixed_while_entries_grow`. All pass. The memory bound of the engine rests on the code structure and the cache test; no RSS measurement was made. The full GC at step 6 keeps one 33-byte key per metadata node and per file of all roots in RAM (`metadata::gc::mark`, gc/mod.rs:112-123). That list is not a map of all objects, because the chunk IDs of the mark go to an external sort. But it grows with the file count, and docs/design/chunk-store.md says so. The owner decides if this is accepted (human item 3). |

**Score:** 5/5 truths verified (0 present, behavior-unverified)

### Plan-level must-haves: deviations by review fixes (not gaps)

| Plan truth | What the code does now | Why it is not a gap |
|------------|------------------------|---------------------|
| 23-03: "`Mode::DryRun` writes nothing (no map file, no temp dir, no put)" | Since WR-02 (`40b9894`), `Mode::DryRun { map_dir }` writes the on-disk map file in a new `tempfile` dir in the system temp dir (`TMPDIR`, about 134 bytes per distinct key). The CLI removes the dir at the end, also on an error. | Roadmap SC5 says "changes nothing" about the store, and the store tree is proved equal. The change makes the dry-run counts correct (`dry_run_counts_equal_the_real_run`). The cost is temp space outside the store. The review accepted it. If the owner wants the plan text closed formally, add an override for this plan truth. |
| 23-04: "when no root changed it writes no switch segment" | Since IN-02 (`bfc98a7`), the run also writes a same-roots switch when more than one `segment-*.seg` is left, to remove old segments that a crash left. | This refines the plan truth. `a_second_run_finds_nothing_to_rewrite` still proves that a clean store gets no new segment. |

### Required Artifacts

`gsd-tools query verify.artifacts` on each PLAN: exit 0 for every plan. 23-01 gave 5/5, 23-02 4/4, 23-03 4/4, 23-04 4/4 and 23-05 4/4, with no issues.

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `crates/chunk-store/src/hash.rs` | `Sha256` hasher, golden vectors, speed test | VERIFIED | in `HASHERS`; used by `header::hash_flag`, the CLI and the tests |
| `crates/slicefs-cli/src/header.rs` | `HASH_SHA256_FLAG`, `hash_flag`, pairing rule | VERIFIED | used by `content::add_hash_flag` and `marker_with` |
| `crates/metadata/src/segment/switch.rs` | atomic switch segment | VERIFIED | called at rehash.rs:237 |
| `crates/metadata/src/rehash/mod.rs`, `map.rs` | engine, on-disk map | VERIFIED | called at rehash.rs:219 and 302 |
| `crates/slicefs-cli/src/rehash.rs` | the command | VERIFIED | `Cmd::Rehash` is dispatched in main.rs |
| `crates/slicefs-cli/src/gc.rs` | `gc_on_open_store` | VERIFIED | called by rehash.rs:254 and `gc_report` |
| `crates/slicefs-cli/tests/rehash_tests.rs` | 25 end-to-end tests | VERIFIED | 25 pass |

### Key Link Verification

`gsd-tools query verify.key-links` on each PLAN: exit 0 for every plan. 23-01 gave 3/3, 23-02 3/3, 23-03 4/4, 23-04 5/5 and 23-05 2/2. I also checked these links by hand:

| From | To | Via | Status |
|------|----|-----|--------|
| `main.rs` | `rehash::run_rehash` | `Cmd::Rehash` arm | WIRED |
| `rehash.rs` | `content::ensure_hash_flag` | step 2 (before the first object of the target hash) | WIRED |
| `rehash.rs` | `content::open_with_hasher` -> `metadata::rehash::rehash_roots` | step 3 | WIRED |
| `rehash.rs` | `write_switch_segment` | step 4, after `cs.sync()` | WIRED |
| `rehash.rs` | `content::check_write` (header hash line) | step 5 | WIRED |
| `rehash.rs` | `gc::gc_on_open_store(.., true)` with new roots | step 6 | WIRED |
| `mount.rs` / `seed.rs` | `select_hash` / `check_write_with` (flag + hash line) | `--hash` | WIRED |
| both segment loaders | `last_per_version` | return value | WIRED |

### Data-Flow Trace (Level 4)

| Artifact | Data | Source | Real data | Status |
|----------|------|--------|-----------|--------|
| rehash report | values and bytes read, rewritten, stored | `Stats` from the engine walk, plus `cs.stats()` before and after | yes | FLOWING |
| rehash report | `Bytes freed` | `GcStats.removed_bytes` from `gc_on_open_store` | yes | FLOWING |
| dry-run report | other-tag objects and bytes | `for_each_file_object` over the live tree | yes | FLOWING |

### Behavioral Spot-Checks (targeted test runs at HEAD 9ff6da8)

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| Engine, map, switch, loaders | `cargo test -p metadata --lib -- rehash:: segment::` | 27 passed, 0 failed | PASS |
| End-to-end rehash (round trips, refusals, crashes, dry-run, report) | `cargo test -p slicefs-cli --test rehash_tests` | 25 passed, 0 failed (29 s) | PASS |
| SHA-256 hasher | `cargo test -p chunk-store --lib hash::` | 7 passed, 1 ignored (`sha256_speed`) | PASS |
| seed and mount `--hash sha256` | `cargo test -p slicefs-cli --test hash_change_tests sha256` | 4 passed | PASS |
| Header pairing rule | `cargo test -p slicefs-cli --lib header::` | 24 passed | PASS |

I did not rerun the full gate. The orchestrator reports that it passed at this HEAD: 1671 passed, 0 failed, 4 skips; clippy ok; unsafe files=31 tokens=132. `runs/gate-23-05.log` records the full Kani run (`KANI ok harnesses=9`) and `cargo build --workspace --locked: exit 0`. That Kani run is on tree `f808f3d`, before the review fixes, not on HEAD. Kani is not part of a success criterion, so this does not block.

### Probe Execution

Step 7c: not applicable. There is no `scripts/*/tests/probe-*.sh`, and the phase docs declare no probe.

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|-------------|-------------|--------|----------|
| HASH-01 | 23-01 | SHA-256 production write hash with its own tag; seed or mount `--hash sha256`; BLAKE3-256 default | SATISFIED | SC1. The hardware speed check is a human item. |
| HASH-02 | 23-03, 23-04, 23-05 | `rehash --to` rewrites all objects (chunks, manifests, nodes, roots, snapshots); fresh-write keys | SATISFIED | SC2; `assert_fresh_write_keys` for 2 profiles |
| HASH-03 | 23-04 | Offline: refuses while mounted (OS lock) or with write-log records after the last root | SATISFIED | SC3 |
| HASH-04 | 23-02, 23-03, 23-05 | Crash-safe; GC frees old objects after the switch; old objects verified as read | SATISFIED | SC4 (process-crash model). IN-04 is a human item. |
| HASH-05 | 23-03, 23-04, 23-05 | Progress, bytes rewritten, bytes freed, `--dry-run`, bounded memory | SATISFIED | SC5 |
| HASH-06 | 23-05 | Both ways and back; mixed store ends with one hash; files and snapshots equal; scrub passes | SATISFIED | the `round_trip_*` tests |

No orphaned requirements: REQUIREMENTS.md maps only HASH-01..06 to Phase 23, and every one of them is claimed by a plan. The "Pending" marks in REQUIREMENTS.md and the unchecked ROADMAP plan boxes are orchestrator bookkeeping, not gaps.

### Anti-Patterns Found

| File | Line | Pattern | Severity | Impact |
|------|------|---------|----------|--------|
| (all phase-modified files) | - | TBD / FIXME / XXX / TODO / HACK / `todo!` / `unimplemented!` | none found | - |
| `crates/slicefs-cli/src/header.rs` | 605 | `lock_result` returns a lock handle with no real lock when the OS lock call gives `ErrorKind::Unsupported`, so the lock is granted | Info | Pre-existing, from before this phase. On a filesystem with no file-lock support, the offline guarantee of rehash is only the `mount.lock` check. Not a phase 23 regression. |
| `crates/metadata/src/rehash/mod.rs` | 425-432 | the refcount pass drops a row whose object is `Missing` as stale | Info | By design (R-03). Live contents are translated first, through the manifests, so a missing live object fails earlier with an error. The dropped rows are counted in the report. |

### Human Verification Required

### 1. Hardware SHA-256 path

**Test:** On a CPU with SHA extensions (SHA-NI, or ARMv8 SHA2 such as Apple Silicon), run `cargo test --release -p chunk-store -- --ignored sha256_speed --nocapture`.
**Expected:** Over 1,000 MB/s on 8 KiB blocks.
**Why human:** The dev CPU (Xeon W-3245, Cascade Lake) has no SHA-NI. sha2 does not expose which backend it picked.

### 2. IN-04: power loss before the rewrite sync (deferred)

**Test:** Add or run an engine test: `rehash_roots` in `Mode::Rewrite` on `MemIo` with no `cs.sync()`, then `mem.crash()` and a reopen. Check that the old root gives the same tree, then rerun and compare with a clean-run root.
**Expected:** The old root is intact, and the rerun gives the clean-run root.
**Why human:** No test covers this case today. The review skipped it ("needs a crash-aware storage layer"). No later roadmap phase covers it, so the owner must accept the deferral or schedule the test.

### 3. RAM bound of the full GC step (owner decision)

**Test:** Read `metadata::gc::mark` (gc/mod.rs:112-123) and the Rehash memory notes in docs/design/chunk-store.md.
**Expected:** The owner accepts the O(file count + node count) list of 33-byte keys that the GC step keeps in RAM, or asks for a bounded mark.
**Why human:** The rehash engine has bounded RAM. The GC step that the command always runs is older code, and its key list grows with the file count. Whether HASH-05 "bounded" covers that step is a scope decision.

### Gaps Summary

No blocking gaps. Every roadmap success criterion and every requirement has code that exists, is wired, and is exercised by passing tests that I ran at HEAD `9ff6da8`. The review findings WR-01, WR-02, IN-01, IN-02 and IN-03 are fixed in HEAD, and each fix has its test. The status is `human_needed` for three reasons only: the hardware SHA-256 speed check, which cannot run on this CPU; the IN-04 power-loss-before-sync case, which the review deferred; and an owner decision on whether the RAM of the full GC step (it grows with the file count, and the docs say so) is within "bounded memory".

---

_Verified: 2026-11-26T17:02:52Z_
_Verifier: Claude (gsd-verifier)_

## Human items: resolved (2026-11-26)

| Item | Result |
|---|---|
| 1. Hardware SHA-256 speed | Measured on GitHub runners with SHA instructions: ubuntu-latest (Xeon 8573C, `sha_ni`) 1484 MB/s, macos-latest (Apple M1, `FEAT_SHA256`) 1734 MB/s, ubuntu-24.04-arm (`sha2`) 1855 MB/s, on 8 KiB blocks. |
| 2. Power loss before the sync (IN-04) | Tested in Docker with LazyFS and an fsync/rename kill shim (`scripts/power-loss/`, PR #206): 49 crash points, all mount with the old or the new roots. |
| 3. RAM of the full GC step | Fixed in PR #209 (#207): the mark keys go through the external sort into `gc-tmp`. `gc --full` heap 109 MB at 1M files with 1 or 4 snapshots; rehash RSS 832 to 337 MiB. Still open in #207: the whole `.idx` decode and the Bloom filters (about 1.25 B per object). `scrub` has the same key list: #210. |
