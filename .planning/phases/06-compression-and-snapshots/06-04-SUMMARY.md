---
phase: 06-compression-and-snapshots
plan: "04"
subsystem: cli
tags: [snapshots, cli, gc, fuse, compression]
dependency_graph:
  requires:
    - "06-03 (DictMetadataStore snapshot methods: create_snapshot, list_snapshots, find_snapshot, snapshot_roots)"
    - "06-01 (Compressor trait and AlgorithmId wire format)"
    - "05-04 (background GC thread, GcHandle)"
  provides:
    - "slicefs snapshot create/list/switch subcommands"
    - "--snapshot and --auto-snapshot mount flags"
    - "--compressor and --compressor-level mount flags"
    - "Snapshot-aware background GC (snapshot_roots())"
    - "commit_root() on DictMetadataStore for snapshot switch"
    - "to_wire_bytes/from_wire_bytes helpers in SliceFsFilesystem"
  affects:
    - "07-windows (if/when implemented): CLI arg surface is now stable)"
tech-stack:
  added: []
  patterns:
    - "Snapshot CLI dispatches through run_snapshot(SnapshotAction) helper"
    - "Snapshot switch writes RootUpdate via commit_root() without re-serializing full store"
    - "to_wire_bytes/from_wire_bytes helpers centralize store_version gating in filesystem.rs"
key-files:
  created:
    - crates/slicefs-cli/src/snapshot.rs
  modified:
    - crates/slicefs-cli/src/cli.rs
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/src/main.rs
    - crates/slicefs-cli/src/lib.rs
    - crates/slicefs-cli/src/filesystem.rs
    - crates/metadata/src/gc/background.rs
    - crates/metadata/src/store.rs
    - crates/metadata/tests/gc_tests.rs
key-decisions:
  - "commit_root() added to DictMetadataStore: writes RootUpdate WAL entry without full re-commit, enables snapshot switch to redirect live root cheaply"
  - "Snapshot --snapshot mount: resolves snapshot, calls load_from_root on snapshot root dict, adds MountOption::RO"
  - "to_wire_bytes/from_wire_bytes helpers: centralise store_version gating; fix v1 write path that incorrectly added compression header"
  - "Background GC uses snapshot_roots() replacing single current_root(): single-line change; snapshot_roots() already returns current root + all snapshot roots"
requirements-completed:
  - SNAP-01
  - SNAP-02
  - SNAP-03
duration: "14min"
completed: "2026-03-29"
---

# Phase 6 Plan 04: Snapshot CLI and GC Integration Summary

**`slicefs snapshot create/list/switch` CLI commands, read-only snapshot mounts via `--snapshot`, compression flags, and snapshot-aware GC that never reclaims blocks reachable from any snapshot.**

## Performance

- **Duration:** ~14 min
- **Started:** 2026-03-29T18:56:06Z
- **Completed:** 2026-03-29T19:09:30Z
- **Tasks:** 2
- **Files modified:** 9

## Accomplishments

- Full `slicefs snapshot` subcommand group: `create`, `list`, `switch` — all require store to be unmounted
- `--snapshot <ref>` mount flag mounts any snapshot read-only by version number or name
- `--compressor` and `--compressor-level` mount flags wire up Phase-6 compression to the CLI
- Background GC updated to use `snapshot_roots()` — all snapshot-reachable blocks survive GC
- Integration test proves snapshot blocks survive GC after the live tree deletes the file

## Task Commits

Each task was committed atomically:

1. **Task 1: Snapshot CLI commands and mount flags** - `409b3de` (feat)
2. **Task 2: Snapshot-aware GC** - `658782a` (feat)

**Plan metadata:** (this commit, docs)

## Files Created/Modified

- `crates/slicefs-cli/src/snapshot.rs` — run_snapshot_create, run_snapshot_list, run_snapshot_switch + dispatcher run_snapshot
- `crates/slicefs-cli/src/cli.rs` — SnapshotAction subcommand enum, --snapshot/--auto-snapshot/--compressor/--compressor-level mount flags
- `crates/slicefs-cli/src/mount.rs` — run_mount with compressor_name, compressor_level, snapshot_ref, auto_snapshot params; snapshot read-only mount path; next_segment_id pub(crate)
- `crates/slicefs-cli/src/main.rs` — routes Cmd::Snapshot to snapshot::run_snapshot; passes all new params to run_mount
- `crates/slicefs-cli/src/lib.rs` — pub mod snapshot added
- `crates/slicefs-cli/src/filesystem.rs` — to_wire_bytes/from_wire_bytes helpers; fixed v1 write path; fixed 3-arg test calls
- `crates/metadata/src/store.rs` — commit_root() method added
- `crates/metadata/src/gc/background.rs` — snapshot_roots() replaces current_root()
- `crates/metadata/tests/gc_tests.rs` — 2 new snapshot GC tests

## Decisions Made

- `commit_root()` on `DictMetadataStore` writes only a `RootUpdate` WAL entry with the provided root digest, without re-serializing the full inode/dir/manifest state. This is correct because on the next mount, `load_store_from_segments` replays segments and uses the last `RootUpdate` as the live root — the snapshot root was already written by `create_snapshot` via `commit()`, so all data is already in the dictionary.
- `to_wire_bytes`/`from_wire_bytes` centralise the `store_version` gate, fixing a pre-existing bug where `store_version=1` writes incorrectly prepended a compression header byte (AlgorithmId::None = 0x00) to raw blocks.
- `--snapshot` mounts are fully read-only: `MountOption::RO` added and store is reconstructed from the snapshot root via `load_from_root`, so the live WAL is not written to.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] SliceFsFilesystem::new signature mismatch (Plan 06-02 artefact)**
- **Found during:** Task 1 (building after adding snapshot.rs)
- **Issue:** `SliceFsFilesystem::new` gained `compressor` and `store_version` params in Phase 6 (06-02), but `mount.rs` and internal test helpers were never updated to match.
- **Fix:** Updated `run_mount` to accept `compressor_name`, `compressor_level`, `snapshot_ref`, `auto_snapshot`; updated 3 test call sites in `filesystem.rs` to pass `Arc::new(NoneCompressor::new()), 1`
- **Files modified:** `crates/slicefs-cli/src/mount.rs`, `crates/slicefs-cli/src/filesystem.rs`
- **Verification:** `cargo build -p slicefs-cli` succeeds, all tests pass
- **Committed in:** `409b3de`

**2. [Rule 1 - Bug] store_version=1 write path incorrectly added compression header**
- **Found during:** Task 1 (compression_tests pre-existing failures surfaced)
- **Issue:** `flush_buffer_to_cas` always called `compress_block` regardless of `store_version`. For v1 stores, this prepended an `AlgorithmId::None` byte (0x00) making read-back off by 1.
- **Fix:** Added `to_wire_bytes()` helper that gates on `store_version >= 2`; refactored all write sites and `from_wire_bytes()` on all read sites.
- **Files modified:** `crates/slicefs-cli/src/filesystem.rs`
- **Verification:** `test_pre_phase6_blocks_readable_with_store_version_1` and `test_backward_compat_none_compressor_v1` now pass
- **Committed in:** `409b3de`

---

**Total deviations:** 2 auto-fixed (1 blocking from prior plan gap, 1 bug in write path)
**Impact on plan:** Both fixes required for correctness. No scope creep.

## Issues Encountered

- Plan 06-02 (store integration with compression) was executed but its effects on `SliceFsFilesystem::new` and `mount.rs` were not fully committed, leaving a build-breaking mismatch. Resolved as Rule 3 deviation above.

## Self-Check

- `crates/slicefs-cli/src/snapshot.rs` — exists: YES
- `crates/metadata/src/gc/background.rs` — uses snapshot_roots(): YES (line 88)
- commit `409b3de` — exists: YES
- commit `658782a` — exists: YES

## Self-Check: PASSED

## Next Phase Readiness

- Snapshot feature is complete end-to-end: create, list, switch, read-only mount
- GC correctly preserves all snapshot-reachable blocks in both background and offline modes
- Phase 6 (compression + snapshots) is now complete
- Ready for Phase 7 (Windows / portability) planning
