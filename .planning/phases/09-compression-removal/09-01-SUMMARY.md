---
phase: 09-compression-removal
plan: 01
subsystem: filesystem
tags: [rust, fuse, blockset, compression-removal, write-path, read-path]

# Dependency graph
requires:
  - phase: 08-correctness-fixes
    provides: fixed refcounts, statfs, and write-path correctness

provides:
  - SliceFsFilesystem without compressor/store_version fields
  - run_mount with 8 params (no compressor args)
  - CLI Cmd::Mount without --compressor/--compressor-level flags
  - Stats reporting "none (v3 raw)" for compressor field
  - slicefs-cli Cargo.toml without slicefs-compression dependency
  - Raw bytes flowing directly through State::push_all and file_storage_get

affects:
  - 09-02 (test file updates will use 3-param constructor)
  - 10-streaming-writes (flush_buffer_to_cas is now clean for push_bytes refactor)

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "v3 raw store format: raw bytes → State::push_all, no compression header, Digest224 on raw bytes"
    - "Direct slice passing: &buf to State::push_all instead of allocating wire_bytes"

key-files:
  created: []
  modified:
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/src/cli.rs
    - crates/slicefs-cli/src/main.rs
    - crates/slicefs-cli/src/stats.rs
    - crates/slicefs-cli/Cargo.toml

key-decisions:
  - "Clean break v3 format: raw bytes pushed directly to CAS with no compression header; no v1/v2 backward compatibility path"
  - "Tests left with old constructor calls in Plan 01 (slicefs_compression still in dev-dependencies via workspace); Plan 02 updates all test call sites"
  - "Stats compressor field kept in StoreStats struct for JSON API stability; value changed to none (v3 raw)"

patterns-established:
  - "v3 write path: State::push_all(&buf) — no to_wire_bytes allocation"
  - "v3 read path: file_storage_get bytes used directly — no from_wire_bytes decompress call"

requirements-completed: [DECOMP-01, DECOMP-02, DECOMP-03, DECOMP-04]

# Metrics
duration: 14min
completed: 2026-03-30
---

# Phase 9 Plan 01: Compression Removal Summary

**Compression layer stripped from SliceFsFilesystem: raw bytes flow directly through State::push_all and file_storage_get with no compression header (v3 store format)**

## Performance

- **Duration:** 14 min
- **Started:** 2026-03-30T04:16:48Z
- **Completed:** 2026-03-30T04:30:19Z
- **Tasks:** 2
- **Files modified:** 6

## Accomplishments

- Removed all `to_wire_bytes`/`from_wire_bytes` call sites from filesystem.rs (5 sites: test_setattr_size read+write, simulate_symlink_create, simulate_readlink, FUSE read callback)
- Removed `compressor_name`/`compressor_level` params from `run_mount` (10 → 8 params) and `Cmd::Mount` CLI struct
- Removed `slicefs-compression` from `slicefs-cli` Cargo.toml dependencies; library compiles cleanly
- Stats now reports `"none (v3 raw)"` for the compressor field

## Task Commits

Each task was committed atomically:

1. **Task 1: Remove compression from filesystem.rs core** - `f924383` (feat)
2. **Task 2: Remove compression from CLI layer and Cargo.toml** - `141f4d3` (feat)

## Files Created/Modified

- `crates/slicefs-cli/src/filesystem.rs` - Removed 5 to_wire_bytes/from_wire_bytes call sites; raw bytes passed directly to/from CAS
- `crates/slicefs-cli/src/mount.rs` - Removed parse_compressor import, 2 params from run_mount, updated println
- `crates/slicefs-cli/src/cli.rs` - Removed --compressor and --compressor-level CLI flags from Cmd::Mount
- `crates/slicefs-cli/src/main.rs` - Removed compressor/compressor_level from match destructure and run_mount call
- `crates/slicefs-cli/src/stats.rs` - Changed compressor string from "zstd (default)" to "none (v3 raw)"
- `crates/slicefs-cli/Cargo.toml` - Removed slicefs-compression dependency

## Decisions Made

- Clean break v3 format — raw bytes pushed directly to CAS with no compression header; no v1/v2 backward compat path maintained in production code
- Tests left with old constructor calls for Plan 02 to fix — the test `#[cfg(test)]` blocks still reference `NoneCompressor` and the old 5-param `SliceFsFilesystem::new`. This is intentional: the plan objective is production code only.
- `StoreStats.compressor` field kept in struct for JSON API stability — value updated to reflect v3 raw format

## Deviations from Plan

None - plan executed exactly as written. The constructor was already at 3 params and `flush_buffer_to_cas`/`test_read` were already clean; the remaining 5 call sites were fixed as specified.

## Issues Encountered

None.

## Next Phase Readiness

- Plan 02 (test updates) is unblocked: update `fresh_fs()` helper and all test `SliceFsFilesystem::new(...)` calls to the 3-param signature, remove `slicefs_compression::NoneCompressor` imports from test modules
- Phase 10 streaming writes: `flush_buffer_to_cas` is now clean raw-byte → State::push_all; ready for `push_bytes` refactor

---
*Phase: 09-compression-removal*
*Completed: 2026-03-30*
