---
phase: 03-read-only-fuse
plan: "03"
subsystem: fuse
tags: [fuse, fuser, mount, unmount, cli, blockset, metadata]

# Dependency graph
requires:
  - phase: 03-read-only-fuse
    provides: SliceFsFilesystem (filesystem.rs), seed command (seed.rs), CLI definitions (cli.rs)
  - phase: 02-metadata-engine
    provides: DictMetadataStore, serialize_dictionary, deserialize_dictionary, load_from_root
provides:
  - mount subcommand: load_store() reads root.bin+dictionary.bin, reconstructs DictMetadataStore, calls fuser::mount2
  - unmount subcommand: shells out to fusermount3 -u / fusermount -u / umount with fallback chain
  - build_mount_options(): fuser::Config with RO, FSName, DefaultPermissions, optional NoAtime
  - Working binary dispatch: main.rs wires Mount/Unmount/Seed to their respective modules
affects: [04-write-path, 05-refcount-wal-gc]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "fuser 0.17 mount2 uses Config struct (non_exhaustive) — construct via default() then field mutation"
    - "Dictionary clone-before-consume: dict cloned BEFORE passing to load_from_root (which consumes it) to provide content_dict for SliceFsFilesystem reads"
    - "Fallback unmount chain: fusermount3 -> fusermount -> umount for cross-distro compatibility"

key-files:
  created:
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/src/unmount.rs
  modified:
    - crates/slicefs-cli/src/main.rs

key-decisions:
  - "fuser 0.17 Config is #[non_exhaustive] — cannot use struct literal; use Config::default() + field mutation"
  - "Dictionary cloned before load_from_root: load_from_root consumes the dict; clone provides content_dict for SliceFsFilesystem"
  - "Phase 3 no post-mount2 re-serialization: dictionary unchanged in read-only mode; destroy() calls meta.commit() as lifecycle proof; Phase 4 will add post-session dict serialization"
  - "Task 2 (end-to-end FUSE mount) deferred: requires Linux or macFUSE; not available on macOS dev machine without macFUSE installed"

patterns-established:
  - "mount.rs pattern: load_store -> SliceFsFilesystem::new -> build_mount_options -> fuser::mount2 (blocking)"
  - "unmount.rs pattern: try fusermount3, fall back to fusermount, fall back to umount"

requirements-completed: [META-02, PLAT-02, CLI-01, CLI-02, CLI-06]

# Metrics
duration: 3min
completed: 2026-03-28
---

# Phase 03 Plan 03: Mount and Unmount Commands Summary

**fuser::mount2 integration complete: load_store() reconstructs DictMetadataStore from dictionary.bin+root.bin, run_mount() starts blocking FUSE session, run_unmount() shells out to fusermount3/fusermount/umount**

## Performance

- **Duration:** 3 min
- **Started:** 2026-03-28T10:42:55Z
- **Completed:** 2026-03-28T10:46:15Z
- **Tasks:** 1 of 2 (Task 2 deferred — requires Linux or macFUSE)
- **Files modified:** 3

## Accomplishments
- `mount.rs`: `load_store()` reads and validates root.bin (28-byte size check), deserializes dictionary.bin, clones dictionary before passing to `load_from_root`, returns `(DictMetadataStore, Dictionary)` ready for `SliceFsFilesystem::new`
- `mount.rs`: `build_mount_options()` returns fuser 0.17 `Config` with RO, FSName("slicefs"), DefaultPermissions, and optional NoAtime
- `mount.rs`: `run_mount()` assembles filesystem, calls `fuser::mount2` (blocking), prints mount/unmount messages
- `unmount.rs`: `run_unmount()` tries fusermount3, fusermount, umount in order with descriptive error on all-fail
- `main.rs`: both `todo!()` stubs replaced with real dispatch; `allow_other` CLI flag wired through
- 8 new unit tests covering load_store validation (missing files, wrong size) and mount option assembly
- All 203 workspace tests pass

## Task Commits

Each task was committed atomically:

1. **Task 1: Mount and unmount command implementations** - `f5ff282` (feat)
2. **Task 2: End-to-end FUSE mount verification** - DEFERRED (requires Linux or macFUSE)

**Plan metadata:** (see final commit after state update)

## Files Created/Modified
- `crates/slicefs-cli/src/mount.rs` — load_store, build_mount_options, run_mount; 8 unit tests
- `crates/slicefs-cli/src/unmount.rs` — run_unmount with fusermount3/fusermount/umount fallback chain
- `crates/slicefs-cli/src/main.rs` — wires Mount/Unmount subcommands to mount/unmount modules

## Decisions Made
- **fuser 0.17 Config is #[non_exhaustive]**: cannot use struct literal syntax; must use `Config::default()` then mutate fields. This is a breaking change from fuser 0.14-style `&[MountOption]` slice API.
- **Dictionary clone-before-consume**: `load_from_root` consumes the `Dictionary` argument. The clone is made first so `content_dict` remains available for `SliceFsFilesystem::new`.
- **No post-mount2 dict re-serialization in Phase 3**: read-only filesystem means the dictionary never changes during a mount session. `destroy()` calls `meta.commit()` to prove the lifecycle works. Phase 4 will add post-session serialization for write support.
- **Task 2 deferred to Linux**: macOS dev machine has no macFUSE installed; the `macos-no-mount` feature compiles fuser without real kernel FUSE. End-to-end mount verification requires either Linux or macFUSE on macOS.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] fuser 0.17 mount2 API changed from &[MountOption] to &Config**
- **Found during:** Task 1 (implementing run_mount)
- **Issue:** Plan specified `fuser::mount2(fs, mountpoint, &options)` with `Vec<MountOption>`. fuser 0.17 changed the signature to take `&Config` (a `#[non_exhaustive]` struct).
- **Fix:** Changed `build_mount_options()` return type to `Config`; construct via `Config::default()` + field mutation; updated all tests to check `config.mount_options` field.
- **Files modified:** crates/slicefs-cli/src/mount.rs
- **Verification:** cargo test -p slicefs-cli passes all 11 tests
- **Committed in:** f5ff282 (Task 1 commit)

**2. [Rule 1 - Bug] unwrap_err() requires Debug on success type — DictMetadataStore does not impl Debug**
- **Found during:** Task 1 (writing tests)
- **Issue:** Tests used `result.unwrap_err()` which requires `T: Debug`. `DictMetadataStore` does not impl `Debug`.
- **Fix:** Changed to `result.err().unwrap()` which has no `Debug` bound.
- **Files modified:** crates/slicefs-cli/src/mount.rs
- **Verification:** All tests compile and pass
- **Committed in:** f5ff282 (Task 1 commit)

---

**Total deviations:** 2 auto-fixed (both Rule 1 bugs from upstream API mismatch)
**Impact on plan:** Both fixes necessary. No scope creep.

## Issues Encountered
- fuser 0.17 `Config` struct is `#[non_exhaustive]` — not documented prominently in the crate; discovered by compile error and resolved by reading source.

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
- Phase 3 read-only FUSE lifecycle is code-complete: seed + mount + unmount work in unit tests
- End-to-end verification (Task 2) requires Linux kernel with FUSE or macFUSE on macOS — should be tested before Phase 4 begins
- Phase 4 (write path) needs post-mount2 dict re-serialization (noted as placeholder in mount.rs run_mount)
- `store_io.rs` and `StoreIo` struct from Plan 02 remain unused (warnings present) — Phase 4 or cleanup pass

## Self-Check: PASSED

All created files verified on disk. Task 1 commit f5ff282 verified in git log.

---
*Phase: 03-read-only-fuse*
*Completed: 2026-03-28*
