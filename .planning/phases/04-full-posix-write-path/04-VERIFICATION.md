---
phase: 04-full-posix-write-path
verified: 2026-03-27T00:00:00Z
status: gaps_found
score: 9/10 must-haves verified
gaps:
  - truth: "slicefs-cli integration tests (90 tests) pass in CI / on this machine"
    status: failed
    reason: "Cargo.toml workspace fuser dependency lacks `features = [\"macos-no-mount\"]`. Without this, `cargo test --package slicefs-cli` fails with a pkg-config/fuse build error on macOS (no FUSE-T headers). The 90 tests pass when the feature flag is temporarily restored but the current HEAD (395d40c) has it absent."
    artifacts:
      - path: "Cargo.toml"
        issue: "fuser = { version = \"0.17\" } — macos-no-mount feature deliberately removed in commit 395d40c, but no alternative test compilation path exists for macOS without FUSE-T headers installed in the build env"
    missing:
      - "Either restore `features = [\"macos-no-mount\"]` in the workspace fuser dependency, OR document the required host setup (FUSE-T headers + pkg-config) so CI can compile slicefs-cli tests without the feature flag. Currently the 90 integration tests in write_path_tests.rs / dir_link_tests.rs / statfs_tests.rs / posix_compliance_tests.rs cannot be compiled or run."
human_verification:
  - test: "Mount a fresh store with `slicefs mount`, then run: echo hello > mountpoint/test.txt; cat mountpoint/test.txt; mv mountpoint/test.txt mountpoint/test2.txt; ln -s test2.txt mountpoint/link; mkdir mountpoint/dir; rmdir mountpoint/dir; slicefs unmount"
    expected: "All operations complete without EROFS or other errors. File content survives rename. Symlink resolves correctly. Directory removed cleanly."
    why_human: "End-to-end FUSE session through actual kernel VFS; cannot verify FUSE kernel-userspace protocol in unit tests."
  - test: "On Linux: build project, seed a store, mount it, run `prove -r pjdfstest/tests/` against the mount point."
    expected: ">95% of pjdfstest cases pass (requirement POSIX-14)."
    why_human: "pjdfstest requires Linux FUSE kernel module + real mount; not reproducible on macOS dev machine."
---

# Phase 4: Full POSIX Write Path Verification Report

**Phase Goal:** Files can be created, written, modified, renamed, deleted, and linked through the mount point with inline deduplication active; real tools (editors, package managers, build systems) work correctly; pjdfstest passes >95% on Linux.

**Verified:** 2026-03-27
**Status:** gaps_found
**Re-verification:** No — initial verification

---

## Goal Achievement

### Observable Truths

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | Refcount for a Digest224 increments/decrements correctly | VERIFIED | `increment_refcount`, `decrement_refcount`, `get_refcount` exist in store.rs:119-140; 5 refcount tests pass |
| 2 | DictMetadataStore has per-handle write buffer table with AtomicU64 counter | VERIFIED | `OpenFileState`, `open_files: Mutex<HashMap<u64, OpenFileState>>`, `next_fh: AtomicU64` in filesystem.rs:37-55 |
| 3 | Mount starts a read-write session (no MountOption::RO) | VERIFIED | `build_mount_options()` in mount.rs:88-99 contains only FSName + DefaultPermissions + optional NoAtime; no RO flag |
| 4 | destroy() persists dictionary.bin and root.bin to store_path | VERIFIED | filesystem.rs:674-688: commits meta, serializes dictionary, writes both files |
| 5 | File create/write/release pipeline stores content in CAS | VERIFIED | `State::push_all` + `set_manifest` + `increment_refcount` in release path (filesystem.rs); 16 write_path_tests pass with feature flag |
| 6 | Truncate via setattr(size=N) works for open and closed files | VERIFIED | `test_setattr_size` handles both in-flight buffer and closed-file CAS re-push; 3 truncate tests pass |
| 7 | mkdir/rmdir/rename/symlink/link/unlink all implemented | VERIFIED | All 6 FUSE callbacks delegate to simulate_* methods in filesystem.rs:970-1113; 25 dir_link_tests pass |
| 8 | statfs reports logical bytes and physical bytes (dedup ratio visible) | VERIFIED | statfs reads `meta.logical_bytes()` and `dict.len() * 92`; 9 statfs tests pass |
| 9 | POSIX locking (getlk/setlk) handled correctly | VERIFIED | fuser 0.17 trait defaults return ENOSYS; no explicit override needed (confirmed by 04-04-SUMMARY.md decision log) |
| 10 | slicefs-cli 90 integration tests compilable and passing on dev machine | FAILED | `cargo test --package slicefs-cli` fails on macOS without `macos-no-mount` feature; tests pass when flag temporarily restored but Cargo.toml HEAD lacks it |

**Score:** 9/10 truths verified

---

## Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `crates/metadata/src/store.rs` | Refcount BTreeMap with increment/decrement/get; logical_bytes AtomicU64 | VERIFIED | Fields at lines 58-63; methods at lines 119-150 |
| `crates/metadata/tests/refcount_tests.rs` | 5 refcount tests | VERIFIED | 5 tests pass (increment, decrement, zero, shared content, round-trip) |
| `crates/slicefs-cli/src/filesystem.rs` | OpenFileState, open_files, next_fh, store_path; all FUSE write callbacks | VERIFIED | All present; create/write/release/setattr/mkdir/rmdir/rename/symlink/readlink/link/unlink implemented |
| `crates/slicefs-cli/src/mount.rs` | RW mount (no MountOption::RO); store_path passed to SliceFsFilesystem | VERIFIED | RO absent from build_mount_options(); store_path passed at line 116 |
| `crates/slicefs-cli/tests/write_path_tests.rs` | 16 write pipeline tests | VERIFIED (conditional) | 16 tests exist and pass when macos-no-mount feature is available |
| `crates/slicefs-cli/tests/dir_link_tests.rs` | 25 dir/link tests | VERIFIED (conditional) | 25 tests exist and pass when macos-no-mount feature is available |
| `crates/slicefs-cli/tests/statfs_tests.rs` | 9 statfs tests | VERIFIED (conditional) | 9 tests exist and pass when macos-no-mount feature is available |
| `crates/slicefs-cli/tests/posix_compliance_tests.rs` | 40 POSIX compliance tests | VERIFIED (conditional) | 40 tests exist and pass when macos-no-mount feature is available |
| `Cargo.toml` | fuser with macos-no-mount feature for test builds | FAILED | Feature absent in HEAD (395d40c); executor added it twice, reverted twice |

---

## Key Link Verification

| From | To | Via | Status | Details |
|------|----|-----|--------|---------|
| `store.rs` | `DictMetadataStore` | `refcounts: Mutex<BTreeMap<Digest224, u64>>` field | WIRED | Line 58 |
| `filesystem.rs` | `SliceFsFilesystem` | `open_files: Mutex<HashMap<u64, OpenFileState>>` | WIRED | Line 53 |
| `mount.rs` | `filesystem.rs` | `SliceFsFilesystem::new` receives `Some(store_path.to_path_buf())` | WIRED | Line 116 |
| `filesystem.rs` | `blockset::State::push_all` | `release()` flushes buffer through CDC | WIRED | In `flush_buffer_to_cas` helper; called from `test_release` |
| `filesystem.rs` | `meta.set_manifest` | `release()` stores content digest as manifest | WIRED | `self.meta.set_manifest(ino, &[content_digest])` |
| `filesystem.rs` | `meta.increment_refcount` | `release()` increments refcount for new content | WIRED | `self.meta.increment_refcount(&content_digest)` |
| `filesystem.rs` | `meta.create_directory` | `mkdir()` delegates to DictMetadataStore | WIRED | `.create_directory(parent_ino, name, &dir_meta)` |
| `filesystem.rs` | `meta.link + meta.unlink` | `rename()` composes link+unlink for move | WIRED | Lines 577-582 in simulate_rename |
| `filesystem.rs` | `State::push_all` (symlink target) | `symlink()` stores target bytes as CAS content | WIRED | `State::push_all(&mut *dict, target_bytes)` in simulate_symlink |
| `filesystem.rs` | `meta.logical_bytes()` | `statfs()` reads running logical byte total | WIRED | `let logical = self.meta.logical_bytes()` line 856 |

---

## Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|------------|-------------|--------|----------|
| CAS-04 | 04-01 | Reference counting per block | SATISFIED | increment/decrement/get_refcount in store.rs; refcount tests pass; refcounts persist in 184-byte root record |
| CAS-06 | 04-04 | Dedup-aware space reporting (statfs) | SATISFIED | statfs uses logical_bytes() and dict.len()*92; 9 statfs tests verify dedup ratio visibility |
| POSIX-01 | 04-02 | File read/write/create/delete operations | SATISFIED | create/write/release/unlink implemented; 16 write_path_tests + 40 posix_compliance_tests cover the full pipeline |
| POSIX-02 | 04-03 | Directory create/delete/list | SATISFIED | mkdir/rmdir implemented with . and .. entries, nlinks lifecycle; 6 dir tests in posix_compliance_tests |
| POSIX-03 | 04-03 | Atomic rename | SATISFIED | simulate_rename handles same-dir, cross-dir, overwrite, RENAME_NOREPLACE; editor pattern (create-temp-then-rename) tested |
| POSIX-04 | 04-03 | Symbolic links | SATISFIED | simulate_symlink stores target as CAS content; simulate_readlink retrieves it; 5 symlink tests |
| POSIX-05 | 04-03 | Hard links with inode-level refcount | SATISFIED | simulate_link increments nlinks; simulate_unlink decrements, deletes inode at 0; 5 hard link tests |
| POSIX-09 | 04-02 | Truncate with partial block handling | SATISFIED | test_setattr_size handles open-handle buffer truncate and closed-file CAS re-push with refcount updates; 3 truncate tests |
| POSIX-12 | 04-04 | POSIX locking (fcntl/flock) | SATISFIED | fuser 0.17 getlk/setlk defaults return ENOSYS; kernel handles local locking; no explicit override needed |
| POSIX-14 | 04-04 | pjdfstest >95% pass rate | NEEDS HUMAN | 40 custom POSIX compliance tests pass (covering all pjdfstest categories); actual pjdfstest run requires Linux + FUSE mount |

---

## Anti-Patterns Found

| File | Line | Pattern | Severity | Impact |
|------|------|---------|----------|--------|
| `crates/slicefs-cli/src/mount.rs` | 127 | Comment says "Phase 4 will need post-session serialization" but Phase 4 has been completed | Info | Stale comment; destroy() in filesystem.rs handles serialization correctly |
| `crates/slicefs-cli/src/filesystem.rs` | 1193 | `fallocate` returns EROFS | Warning | Should return ENOSYS (not supported) rather than EROFS (read-only filesystem); EROFS is misleading on a writable filesystem |
| `Cargo.toml` | 24 | `fuser = { version = "0.17" }` without macos-no-mount | Blocker | Prevents compilation of slicefs-cli integration tests on macOS dev machine; 90 tests cannot be run |

---

## Human Verification Required

### 1. End-to-End FUSE Write Session

**Test:** Mount a fresh store with `slicefs mount`, then run: `echo hello > mountpoint/test.txt; cat mountpoint/test.txt; mv mountpoint/test.txt mountpoint/test2.txt; ln -s test2.txt mountpoint/link; mkdir mountpoint/dir; rmdir mountpoint/dir; slicefs unmount`

**Expected:** All operations complete without EROFS or other errors. File content survives rename. Symlink resolves correctly. Directory removed cleanly.

**Why human:** End-to-end FUSE session through actual kernel VFS; cannot verify FUSE kernel-userspace protocol correctness in unit tests.

### 2. pjdfstest on Linux (POSIX-14)

**Test:** On Linux: build project, seed a store, mount with `slicefs mount`, run `prove -r pjdfstest/tests/` against the mount point.

**Expected:** Greater than 95% of pjdfstest cases pass.

**Why human:** pjdfstest requires Linux FUSE kernel module and a real mount point; not reproducible on macOS development machine.

---

## Gaps Summary

One gap blocks automated test verification on the current HEAD.

**Root cause:** The `macos-no-mount` feature was intentionally removed from Cargo.toml in Phase 3 (commit c3816a6) when FUSE-T was installed and real FUSE mounts became available on the macOS dev machine. The Phase 4 executor correctly added it back twice for test builds, but both additions were reverted (commits 9f899a1 and 395d40c) — presumably to preserve the real-FUSE-capable state.

**Effect:** `cargo test --package slicefs-cli` currently fails with a pkg-config/fuse build error on this macOS machine. The 90 integration tests in write_path_tests.rs, dir_link_tests.rs, statfs_tests.rs, and posix_compliance_tests.rs exist and are substantive but cannot be compiled without either (a) the macos-no-mount feature, or (b) FUSE development headers available via pkg-config.

**Evidence that tests work:** When the feature was temporarily restored for verification (`git checkout 4c3353d -- Cargo.toml`), all 90 tests passed cleanly.

**Resolution options:**
1. Add `features = ["macos-no-mount"]` back to the workspace fuser dependency and document that FUSE-T integration tests require a separate binary build (not the feature-gated test build).
2. Document the pkg-config FUSE headers requirement so the build environment can be configured to compile tests without the feature flag.

All implementation code (filesystem.rs, store.rs, mount.rs) is fully implemented, wired, and substantive. The gap is purely in the test compilation infrastructure.

---

_Verified: 2026-03-27_
_Verifier: Claude (gsd-verifier)_
