---
phase: 23-hashes-and-rehash
plan: 01
status: complete
subsystem: chunk-store hashers, store header
tags: [sha256, hash, header, flag, HASH-01]
requires: []
provides:
  - chunk_store::hash::Sha256 (name sha256, tag 0x12) in HASHERS of all builds
  - header::HASH_SHA256_FLAG, HASH_SHA256_RELEASE, hash_flag(), pairing rule in parse
  - content::ensure_hash_flag (flag-only header write, for rehash step 2)
affects: [23-04 rehash CLI (calls ensure_hash_flag and check_write_with)]
tech-stack:
  added: [sha2 0.11.2 (default-features = false)]
  removed: [sha2-compress (unused since Phase 02)]
  patterns: [one hash_flag() helper decides which hasher needs a flag; flag written in the same header write as the hash line]
key-files:
  created:
    - .planning/phases/23-hashes-and-rehash/runs/red-23-01.log
    - .planning/phases/23-hashes-and-rehash/runs/gate-23-01.log
  modified:
    - Cargo.toml
    - Cargo.lock
    - crates/chunk-store/Cargo.toml
    - crates/chunk-store/src/hash.rs
    - crates/slicefs-cli/Cargo.toml
    - crates/slicefs-cli/src/header.rs
    - crates/slicefs-cli/src/content.rs
    - crates/slicefs-cli/tests/hash_change_tests.rs
    - docs/design/store-format.md
decisions:
  - "Flag name hash-sha256, release word from the constant HASH_SHA256_RELEASE = v2.2 (placeholder, as CHUNK_PROFILE_RELEASE)"
  - "Flag line goes right after the hash line in a new header, at the end of the lines in a rewrite"
  - "A small private helper content::add_hash_flag is shared by check_write_with and ensure_hash_flag"
  - "Sha256 is not re-exported from the chunk-store crate root (lib.rs belongs to plan 23-03 in this wave)"
metrics:
  duration: 36 min
  started: 2026-11-26T13:47:48Z
  completed: 2026-11-26T14:23:13Z
actuals:
  tokens: 9700
  tasks: 3
  commits: 3
plan_head_before: 526609e4de6bba028cb4c1cc049631cffbecb51f
plan_head_after: 790d1f4
---

# Phase 23 Plan 01: SHA-256 write hash and the hash-sha256 flag Summary

SHA-256 (RustCrypto `sha2` 0.11, run-time SHA-NI / ARMv8 SHA2 detection, tag 0x12) is now a production write hash for `seed --hash sha256` and `mount --hash sha256`. An incompat `hash-sha256` header flag is on disk before the first SHA-256 object, and `parse` enforces the pairing rule. BLAKE3-256 stays the default.

BASE = `526609e` (`feature/phase-23-hashes-rehash`).

## Commits

| Task | Commit | Subject |
|------|--------|---------|
| 1 (tracer) | c8e83ae | feat(23-01): SHA-256 write hash with the hash-sha256 flag (tracer) |
| 2 | a01bd57 | feat(23-01): hash-sha256 pairing rule, flag on mount --hash, ensure_hash_flag |
| 3 | 790d1f4 | chore(23-01): drop the unused Phase 02 hash crate; document hash-sha256 |

## What was built

- `chunk_store::hash::Sha256`: name `sha256`, tag 0x12, `digest()` from `sha2::Sha256`. It is in `HASHERS` right after `Blake3_256`, with no cfg. `DEFAULT_HASHER` does not change.
- Tests in `hash.rs`:
  - `sha256_golden_vectors` (empty, `abc`, the 56-byte NIST text, one million `a`);
  - `lookup_by_name_and_tag` now also checks sha256 / 0x12;
  - `every_known_hasher_round_trips` (the invariant test for the assumption delta);
  - `sha256_speed`, ignored, run by hand only.
- Changes in `header.rs`:
  - new constants `HASH_SHA256_FLAG` and `HASH_SHA256_RELEASE`;
  - `KNOWN_FLAGS = &[CHUNK_PROFILE_FLAG, HASH_SHA256_FLAG]`;
  - `hash_flag(hasher)`, the one place that says which hasher needs a flag;
  - `marker_with` writes the flag line right after `hash sha256`;
  - `parse` has two new M9 reasons: `hash sha256 with no hash-sha256 flag` and `the feature hash-sha256 must be incompat`. A flag with no `hash sha256` line is valid.
- Changes in `content.rs`:
  - `check_write_with` adds the flag in the same atomic header write as the new hash line;
  - `ensure_hash_flag(store, hasher) -> io::Result<bool>` writes only the flag (for rehash step 2).
- Tests in `hash_change_tests.rs`:
  - the tracer `sha256_seed_records_the_flag_and_tags_every_object`: FORMAT has the flag, every object of the root has tag 0x12, the files read back, scrub passes, and stats lists the known flag;
  - `sha256_mount_hash_change_writes_the_flag_with_the_hash_line`;
  - `sha256_flag_stays_after_the_hash_goes_back`;
  - `sha256_ensure_hash_flag_writes_only_the_flag`.
- Header unit tests: `hash_sha256_needs_the_incompat_flag`, `the_hash_sha256_flag_must_be_incompat`, `the_hash_sha256_flag_without_the_line_is_valid` and `marker_of_sha256_has_the_flag`.
- `sha2-compress` is removed from the workspace and from slicefs-cli. `Cargo.lock` drops `sha2-compress`, `fixed-array` and `uints`. It adds only the audited set: `sha2`, `digest`, `crypto-common`, `block-buffer`, `hybrid-array`, `typenum` and `cpufeatures` 0.3.1.
- Changes in `docs/design/store-format.md`:
  - two flags in the flag list, with the `hash-sha256` row;
  - the parse rule in step 2;
  - the two M9 reasons;
  - an example header of a SHA-256 store;
  - text on the first write and the rewrite.

`cargo tree -p chunk-store -e normal -i sha2`:
```
sha2 v0.11.2
└── chunk-store v0.1.0 (<repo>/crates/chunk-store)
```

## Gates

- Local gate (4 standard skips) at each commit, exit 0 every time:
  - Task 1: 1611 passed, 0 failed;
  - Task 2: 1618 passed, 0 failed;
  - Task 3: 1618 passed, 0 failed, 13 ignored, 79 result lines.
- `KANI_HARNESS=name_bytes_total scripts/safety/kani.sh`: `KANI ok harnesses=1` (Kani 0.68.0, the pinned version, so nothing was installed). There are 9 `#[kani::proof]` attributes, the same as at BASE.
- `RUST_STABLE=stable scripts/safety/clippy.sh`: `CLIPPY os=Darwin toolchain=stable result=ok` and `UNSAFE files=31 tokens=132 result=ok`. `docs/design/unsafe-review.md` did not change.
- `crates/chunk-store/src/lib.rs` did not change (`git diff --quiet 526609e`: exit 0).
- `cargo build --workspace --locked`: ok.
- Red runs are in `runs/red-23-01.log`. The gate summary is in `runs/gate-23-01.log`.

## Deviations from Plan

1. **[Rule 3 - Blocking] Branch name.** The plan names the base branch `feature/phase-23-hashes-and-rehash`, but the real branch is `feature/phase-23-hashes-rehash`. All `merge-base` checks used the real name, so BASE = 526609e.
2. **[Rule 3 - Blocking] Start state.** The worktree started at `ee2d0ec` (main), not at the phase branch. `exec/23-01` was made there with no commits, so it was moved to `feature/phase-23-hashes-rehash` (526609e) before any edit.
3. **Red order in Task 2.** The first red run of `hash_change_tests` showed only the compile error for `ensure_hash_flag`, and that error hid the run-time red of the mount tests. So `ensure_hash_flag` was added first. A second red run then showed both mount tests failing at the FORMAT check, before `check_write_with` changed. Both runs are logged.
4. **Guards, not red.** Two Task 2 header tests passed before the Task 2 code: `marker_of_sha256_has_the_flag` (its code came in Task 1) and `the_hash_sha256_flag_without_the_line_is_valid`. The mutations that turn them red are written in `red-23-01.log`.
5. **Shared helper.** `content::add_hash_flag` (private) is used by both `check_write_with` and `ensure_hash_flag`. Both still go through `header::hash_flag`.
6. **Extra check in the round-trip test.** `every_known_hasher_round_trips` also checks that every object of the file (not only the key) has the tag of the hasher.
7. **GSD state.** The orchestrator did not want STATE.md, ROADMAP.md, REQUIREMENTS.md or WINDOWS.md updated, so they were not touched. HASH-01 is still to be marked complete after the merge.

## Known Stubs

None. `HASH_SHA256_RELEASE = "v2.2"` is a placeholder release word, as `CHUNK_PROFILE_RELEASE` is. It is documented, and the owner must name the release.

## Threat Flags

None beyond the plan threat model. T-23-SC: the lock audit passed. T-23-01 and T-23-03: the flag is written first and `parse` enforces the pairing rule. T-23-02: the golden vectors pass and tag 0x12 is unique.

## Open question for the owner

`HASH_SHA256_RELEASE` (and `CHUNK_PROFILE_RELEASE`) is `v2.2`, but `release/v2.2.0` is already merged to main without these flags. Which release word should the flag show (for example the release that ships Phase 23)? To change it, edit one constant and the example in `store-format.md`.

## Self-Check: PASSED

- All modified and created files exist.
- Commits c8e83ae, a01bd57 and 790d1f4 are ancestors of HEAD on `exec/23-01`.
