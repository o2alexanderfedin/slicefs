---
phase: 12-add-smb-fskit-backend-support-for-fuse-t
plan: "01"
subsystem: slicefs-cli
tags: [fuse-t, backend-detection, cli, unmount, macOS]
dependency_graph:
  requires: []
  provides: [backend.rs, FuseTBackend, detect_fuse_t_version, select_backend, confirm_fallback, enhanced-unmount]
  affects: [crates/slicefs-cli/src/backend.rs, crates/slicefs-cli/src/cli.rs, crates/slicefs-cli/src/main.rs, crates/slicefs-cli/src/unmount.rs]
tech_stack:
  added: [libc (isatty/kill), tempfile (tests)]
  patterns: [TDD red-green, TTY-aware prompting, platform-conditional force-unmount]
key_files:
  created: [crates/slicefs-cli/src/backend.rs]
  modified: [crates/slicefs-cli/src/cli.rs, crates/slicefs-cli/src/main.rs, crates/slicefs-cli/src/unmount.rs]
decisions:
  - "FuseTBackend selection is pure (takes version + fskit_available as params) for full unit testability"
  - "confirm_fallback_with_tty takes is_tty bool for testability; production confirm_fallback calls is_interactive()"
  - "NFS blocked by default; force=true overrides (explicit user intent required)"
  - "Minimum FUSE-T version 1.0.35 (SMB backend support threshold)"
  - "Mount.lock cleanup is separated into clean_mount_lock() helper for direct test coverage"
metrics:
  duration_seconds: 301
  completed_date: "2026-04-01"
  tasks_completed: 2
  files_modified: 4
  files_created: 1
---

# Phase 12 Plan 01: FUSE-T Backend Detection, Selection, and Enhanced Unmount Summary

**One-liner:** FUSE-T backend auto-detection (FSKit>SMB>NFS priority) with NFS blocking, TTY-aware fallback confirmation, and 4-step enhanced unmount using libfuse-t-*.dylib version parsing.

## Tasks Completed

| # | Task | Commit | Files |
|---|------|--------|-------|
| 1 | Create backend.rs module with detection, selection, and CLI flags | 24e2db6 | backend.rs (new), cli.rs, main.rs |
| 2 | Enhanced unmount with multi-step cleanup | 932176b | unmount.rs, cli.rs, main.rs |

## What Was Built

### Task 1: backend.rs module

New `crates/slicefs-cli/src/backend.rs` provides:

- `FuseTBackend` enum — `Fskit`, `Smb`, `Nfs` variants with `as_mount_option()` returning FUSE-T mount option strings
- `detect_fuse_t_version_from_path(lib_dir)` — scans directory for `libfuse-t-<major>.<minor>.<patch>.dylib`, returns `Option<(u32, u32, u32)>`
- `detect_fuse_t_version()` — production wrapper calling `/usr/local/lib`
- `select_backend(requested, force, version, fskit_available)` — pure selection logic:
  - Rejects versions < 1.0.35 with "too old" error
  - Blocks NFS unless `force=true` (FUSE-T Issue #45 kernel bug)
  - Rejects FSKit when unavailable
  - Auto-detects: FSKit > SMB priority
- `confirm_fallback_with_tty(backend, reason, reader, is_tty)` — TTY-aware prompting with injectable reader for testability
- `select_backend_auto()` — production convenience wrapper with automatic fallback confirmation
- `is_fskit_available()` — checks macOS 26+ AND `/Applications/fuse-t.app` exists
- `is_interactive()` — libc::isatty on STDIN_FILENO

CLI additions: `--backend=nfs|smb|fskit` and `--force` on `slicefs mount`.

27 unit tests covering all behaviors listed in the plan spec.

### Task 2: Enhanced unmount

Rewrote `unmount.rs` with 4-step sequence:

1. **Soft unmount** — `fusermount3 -u`, `fusermount -u`, `umount`
2. **Kill processes** — `lsof +D <mountpoint>` + SIGTERM on each PID; `pkill -f go-nfsv4` for orphaned FUSE-T daemons
3. **Force unmount** — `diskutil unmount force` (macOS) or `umount -f`; Linux lazy: `fusermount3 -uz`, `umount -l`
4. **Clean mount.lock** — `rm <store>/mount.lock` when `--store` provided

CLI addition: optional `--store` flag on `slicefs unmount`.

4 unit tests: non-existent path no-panic, mount.lock cleanup success, no-store no-op, nonexistent lock no-error.

## Verification Results

```
cargo test -p slicefs-cli -- backend    → 27 passed, 0 failed
cargo test -p slicefs-cli -- unmount   → 7 passed, 0 failed
cargo test --workspace                  → all passed, 0 failed
cargo build -p slicefs-cli              → Finished (no errors)
```

## Deviations from Plan

None — plan executed exactly as written.

## Self-Check: PASSED

- backend.rs: FOUND
- unmount.rs: FOUND
- SUMMARY.md: FOUND
- Commit 24e2db6 (Task 1): FOUND
- Commit 932176b (Task 2): FOUND
