---
phase: 12-add-smb-fskit-backend-support-for-fuse-t
plan: "02"
subsystem: slicefs-cli
tags: [fuse-t, backend-integration, mount, signal-handler, watchdog, macos]
dependency_graph:
  requires: [backend.rs, FuseTBackend, select_backend_auto, parse_backend_flag]
  provides: [run_mount-with-backend, build_mount_options-with-backend, watchdog, signal-handler, fuse-t-ini-fallback]
  affects: [crates/slicefs-cli/src/mount.rs, crates/slicefs-cli/src/main.rs, crates/slicefs-cli/src/lib.rs]
tech_stack:
  added: []
  patterns: [RAII Drop guard, Arc<AtomicBool> thread shutdown, cfg(target_os = "macos") gating, signal-safe atomic handler]
key_files:
  created: []
  modified:
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/src/main.rs
    - crates/slicefs-cli/src/lib.rs
decisions:
  - "build_mount_options takes platform-conditional backend param using #[cfg] attribute on parameter; avoids Option<()> on non-macOS"
  - "fuse-t.ini fallback only triggered on backend/option error string match; primary path via CUSTOM mount option"
  - "FuseTIniGuard Drop restores original ini content even on panic (guaranteed restoration)"
  - "inject_backend_into_ini handles all ini cases: replace existing, add to existing section, create new section"
  - "lib.rs exposes backend module as pub mod to allow lib test access"
metrics:
  duration_seconds: 309
  completed_date: "2026-04-01"
  tasks_completed: 1
  files_modified: 3
  files_created: 0
---

# Phase 12 Plan 02: Backend-Aware Mount Lifecycle Summary

**One-liner:** Backend selection (select_backend_auto), CUSTOM mount option injection, SIGTERM/SIGINT signal handler, 5s watchdog thread, and atomic fuse-t.ini fallback all wired into run_mount() with startup logging showing backend name and FUSE-T version.

## Tasks Completed

| # | Task | Commit | Files |
|---|------|--------|-------|
| 1 | Wire backend into mount lifecycle with signal handler, watchdog, and startup logging | a5b070d | mount.rs, main.rs, lib.rs |

## What Was Built

### Task 1: Backend-aware mount lifecycle

**run_mount signature extended:**
- `backend: Option<&str>` — FUSE-T backend name ("smb", "fskit", "nfs")
- `force: bool` — override NFS block

**Backend selection (macOS only):**
- Calls `parse_backend_flag(s)` to convert CLI string to `FuseTBackend`
- Calls `select_backend_auto(requested, force)` returning `(backend, version)` tuple
- Error aborts before `load_store()` is called (no store state touched)

**build_mount_options updated:**
- Accepts `backend: Option<&FuseTBackend>` on macOS via `#[cfg(target_os = "macos")]` parameter attribute
- When backend is `Some`, pushes `MountOption::CUSTOM(backend.as_mount_option().to_string())`
- All callers updated to pass `None` or `Some(&selected_backend)`

**Signal handler (macOS only):**
- `static SHUTDOWN_REQUESTED: AtomicBool` for signal-safe communication
- `register_signal_handlers()` uses `libc::signal` for SIGTERM and SIGINT
- Handler performs single atomic store (POSIX signal-safe)
- Registered BEFORE `mount2()` blocking call

**Watchdog thread (macOS only):**
- `spawn_watchdog(mountpoint, interval=5s, Arc<AtomicBool>)`
- Health check: `std::fs::metadata(&mountpoint)` — Err means mount is dead
- Also checks `SHUTDOWN_REQUESTED` static
- On failure: eprintln warning + `umount -f <mountpoint>`
- Spawned BEFORE `mount2()`, joined AFTER with `watchdog_shutdown.store(true)`

**fuse-t.ini fallback (macOS only):**
- `with_fuse_t_ini_backend(backend, f)` — RAII `FuseTIniGuard` struct with Drop restoring original
- Atomic write: write to `.ini.tmp` then rename to original path
- `inject_backend_into_ini(ini, backend)` handles: replace existing line, add to existing `[Default]`, create new `[Default]` section
- Only triggered when primary CUSTOM mount option fails with backend/option error

**Startup logging:**
- macOS: `SliceFS mounted at <path> (backend: smb, fuse-t: 1.0.54)`
- non-macOS: `SliceFS mounted at <path>` (unchanged)

**main.rs:**
- Removed placeholder `let _ = (&backend, &force);` from Plan 01
- Passes `backend.as_deref()` and `force` to `run_mount()`

**New tests:**
- `test_build_mount_options_with_backend_smb` — CUSTOM("backend=smb") present when backend=Some(Smb)
- `test_build_mount_options_no_backend` — no CUSTOM("backend=*") when backend=None
- `test_inject_backend_replaces_existing` — replaces `backend=nfs` with `backend=smb`
- `test_inject_backend_adds_when_absent` — adds entry to existing [Default] section
- `test_inject_backend_creates_section_when_missing` — creates [Default] section from scratch
- `test_inject_backend_empty_ini` — handles completely empty ini

## Verification Results

```
cargo build -p slicefs-cli           → Finished (no errors, pre-existing warnings only)
cargo test -p slicefs-cli            → all passed, 0 failed
cargo test --workspace               → all passed, 0 failed (zero regressions)
```

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Added backend module to lib.rs**
- **Found during:** Task 1 — `crate::backend` reference in mount.rs failed in lib crate context
- **Issue:** `lib.rs` did not declare `backend` as a module, so `crate::backend` was unresolvable when compiling the lib target
- **Fix:** Added `pub mod backend;` to `lib.rs`
- **Files modified:** `crates/slicefs-cli/src/lib.rs`
- **Commit:** a5b070d

**2. [Rule 1 - Type inference] Explicit String type annotation in map_err closures**
- **Found during:** Task 1 compile — E0282 type annotations needed
- **Issue:** `map_err(|e| -> Box<dyn Error> { e.into() })` — Rust couldn't infer type of `e` without the return type annotation being sufficient
- **Fix:** Changed to `map_err(|e: String| -> Box<dyn Error> { e.into() })`
- **Files modified:** `crates/slicefs-cli/src/mount.rs`
- **Commit:** a5b070d

## Self-Check: PASSED

- crates/slicefs-cli/src/mount.rs: FOUND
- crates/slicefs-cli/src/main.rs: FOUND
- crates/slicefs-cli/src/lib.rs: FOUND
- Commit a5b070d (Task 1): FOUND
