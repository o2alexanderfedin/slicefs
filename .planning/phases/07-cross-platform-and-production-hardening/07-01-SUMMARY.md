---
phase: 07-cross-platform-and-production-hardening
plan: 01
subsystem: infra
tags: [fuse-t, macos, cargo, build, mount, direct_io, rpath]

requires:
  - phase: 06-compression-and-snapshots
    provides: SliceFsFilesystem with compression + snapshot support that run_mount wraps

provides:
  - macOS cargo build works without manual PKG_CONFIG_PATH or install_name_tool
  - FUSE-T write visibility fix via direct_io mount option on macOS
  - allow_other wired from CLI through run_mount to SessionACL

affects: [08-anything-requiring-macos-mount, packaging, CI-macOS]

tech-stack:
  added: []
  patterns:
    - "build.rs for platform-specific linker flags: emit cargo:rustc-link-arg only on CARGO_CFG_TARGET_OS == macos"
    - "[env] in .cargo/config.toml with relative = true for workspace-relative env vars without force override"
    - "cfg!(target_os) at runtime for mount option selection; #[cfg(target_os)] for platform-specific tests"

key-files:
  created:
    - crates/slicefs-cli/build.rs
  modified:
    - .cargo/config.toml
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/src/main.rs

key-decisions:
  - "SessionACL::All used for allow_other (not MountOption::AllowOther which does not exist in fuser 0.17)"
  - "direct_io added via MountOption::CUSTOM on macOS only to fix FUSE-T NFS page cache staleness (issue #45)"
  - "PKG_CONFIG_PATH set with force=false so externally-set env vars take precedence"
  - "rpath emitted from build.rs not [target.*] rustflags in config.toml for cleaner separation"

patterns-established:
  - "Platform guards in mount.rs: cfg!(target_os) at runtime + #[cfg(target_os)] for unit tests"
  - "build.rs as the canonical place for macOS linker flags in slicefs-cli"

requirements-completed: [PLAT-01]

duration: 3min
completed: 2026-03-29
---

# Phase 7 Plan 01: macOS Build Ergonomics and FUSE-T Write Fix Summary

**macOS `cargo build` now works without manual env setup; FUSE-T write visibility fixed by adding `direct_io` mount option to bypass the NFS page cache**

## Performance

- **Duration:** ~3 min
- **Started:** 2026-03-29T20:38:56Z
- **Completed:** 2026-03-29T20:41:25Z
- **Tasks:** 2
- **Files modified:** 4

## Accomplishments

- Added `[env] PKG_CONFIG_PATH` to `.cargo/config.toml` with `relative = true` so `.pkgconfig/fuse.pc` is found automatically during `cargo build`
- Created `crates/slicefs-cli/build.rs` that emits `-Wl,-rpath,/usr/local/lib` on macOS only, eliminating the need for manual `install_name_tool` after every build
- Fixed FUSE-T write-then-read correctness bug (issue #45) by adding `MountOption::CUSTOM("direct_io")` on macOS to bypass FUSE-T's NFS page cache
- Wired `allow_other` CLI flag through `run_mount` to `SessionACL::All`/`SessionACL::Owner` using fuser 0.17's actual ACL mechanism

## Task Commits

Each task was committed atomically:

1. **Task 1: macOS build ergonomics - cargo config and build.rs** - `2d94d34` (chore)
2. **Task 2: Fix FUSE-T write path with direct_io mount option** - `dcce3e4` (feat)

**Plan metadata:** (docs commit follows)

## Files Created/Modified

- `.cargo/config.toml` - Added `[env]` section with PKG_CONFIG_PATH pointing to `.pkgconfig/` relative to workspace root
- `crates/slicefs-cli/build.rs` - New build script emitting macOS rpath linker flag
- `crates/slicefs-cli/src/mount.rs` - `build_mount_options` gains `allow_other` param, adds `direct_io` on macOS; new tests added
- `crates/slicefs-cli/src/main.rs` - `allow_other` extracted from `Cmd::Mount` and passed to `run_mount`

## Decisions Made

- **SessionACL::All for allow_other:** fuser 0.17 does not have a `MountOption::AllowOther` variant. The `allow_other` kernel option is expressed via `SessionACL::All` (or `SessionACL::RootAndOwner`). Used `SessionACL::All` for the `--allow-other` CLI flag.
- **direct_io via CUSTOM not AllowRoot:** FUSE-T translates FUSE to NFSv4; the NFS client-side page cache retains stale data. `direct_io` as a CUSTOM mount option bypasses this cache. This is a FUSE-T-specific workaround.
- **PKG_CONFIG_PATH with force=false:** Users who have set their own `PKG_CONFIG_PATH` retain override capability. The bundled `.pkgconfig/fuse.pc` is a convenience default.
- **rpath from build.rs not config.toml:** build.rs allows conditional emission per target_os cleanly; `[target.x86_64-apple-darwin]` in config.toml would need per-arch entries and doesn't compose as well.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] MountOption::AllowOther does not exist in fuser 0.17**
- **Found during:** Task 2 (build failure)
- **Issue:** Plan instructed adding `MountOption::AllowOther` but fuser 0.17 has no such variant; `allow_other` is expressed via `SessionACL`
- **Fix:** Used `SessionACL::All` (allow_other=true) vs `SessionACL::Owner` (allow_other=false); updated tests to check `cfg.acl` instead
- **Files modified:** `crates/slicefs-cli/src/mount.rs`
- **Verification:** All 65 tests pass including new `test_build_mount_options_allow_other` and `test_build_mount_options_no_allow_other`
- **Committed in:** `dcce3e4` (Task 2 commit)

---

**Total deviations:** 1 auto-fixed (Rule 1 - bug: non-existent enum variant)
**Impact on plan:** Fix was necessary for compilation. Semantically equivalent to plan intent — `allow_other` is still wired end-to-end; the implementation uses the correct fuser 0.17 API.

## Issues Encountered

None beyond the fuser API mismatch corrected above.

## User Setup Required

None - no external service configuration required.

## Next Phase Readiness

- macOS build and FUSE-T write correctness are unblocked for Phase 7 remaining plans
- `allow_other` is now fully wired for multi-user mount scenarios
- 65 tests all pass; no regressions

---
*Phase: 07-cross-platform-and-production-hardening*
*Completed: 2026-03-29*

## Self-Check: PASSED

All created files verified present. Both task commits confirmed in git log.
