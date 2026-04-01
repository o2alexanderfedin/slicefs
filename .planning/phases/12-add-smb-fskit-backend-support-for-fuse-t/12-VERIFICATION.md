---
phase: 12-add-smb-fskit-backend-support-for-fuse-t
verified: 2026-03-31T00:00:00Z
status: passed
score: 13/13 must-haves verified
---

# Phase 12: FUSE-T Backend Selection Verification Report

**Phase Goal:** Auto-detect and use SMB/FSKit backend for FUSE-T, blocking NFS by default. Add --backend CLI flag, enhanced unmount, signal handler + watchdog.
**Verified:** 2026-03-31
**Status:** passed
**Re-verification:** No — initial verification

## Goal Achievement

### Observable Truths

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | Backend selection returns FSKit when macOS 26+ and fuse-t.app present, SMB otherwise | VERIFIED | `select_backend()` in `backend.rs:150–187`: auto-detect branch returns `Fskit` when `fskit_available=true`, `Smb` otherwise |
| 2 | FUSE-T version correctly parsed from dylib filename as (u32, u32, u32) | VERIFIED | `parse_libfuse_t_filename()` in `backend.rs:88–98`, 5 unit tests covering found/not found/multi-digit/unrelated files |
| 3 | Requesting NFS without --force returns an error explaining the macOS kernel bug | VERIFIED | `backend.rs:166–170`: Err "NFS backend is blocked by default due to macOS kernel bug (FUSE-T Issue #45)". Test `test_select_backend_nfs_without_force_returns_err` passes |
| 4 | FUSE-T versions older than 1.0.35 are rejected with a clear error | VERIFIED | `backend.rs:157–163`: `version < MIN_VERSION` guard returns Err "is too old". Tests `test_select_backend_version_too_old_returns_err` and `test_select_backend_version_below_minimum_err` pass |
| 5 | Enhanced unmount performs soft -> kill -> force -> mount.lock cleanup sequence | VERIFIED | `unmount.rs:27–53`: explicit 4-step sequence with labeled comments. `kill_processes_at_mountpoint()` uses lsof+SIGTERM |
| 6 | CLI accepts --backend and --force flags on mount subcommand | VERIFIED | `cli.rs:63–67`: both fields present with correct types. `main.rs:25,35–36` passes them as `backend.as_deref()` and `force` |
| 7 | When auto-detection falls back to lower backend, interactive TTY prompts, non-interactive emits stderr warning | VERIFIED | `backend.rs:192–216`: `confirm_fallback_with_tty()` prompts on TTY, auto-accepts non-interactively. Tests `test_confirm_fallback_interactive_no_lowercase`, `test_confirm_fallback_non_interactive` pass |
| 8 | Mount startup log includes backend name and FUSE-T version | VERIFIED | `mount.rs:542–549`: `println!("SliceFS mounted at {} (backend: {}, fuse-t: {}.{}.{})", ...)` on macOS |
| 9 | Backend selection result is passed as FUSE CUSTOM mount option | VERIFIED | `mount.rs:336–363`: `build_mount_options()` pushes `MountOption::CUSTOM(backend.as_mount_option())`. Test `test_build_mount_options_with_backend_smb` passes |
| 10 | Signal handler is registered before mount2() blocking call | VERIFIED | `mount.rs:527–531`: `unsafe { register_signal_handlers(); }` in macOS-only block before `mount2()` at line 552 |
| 11 | Watchdog thread monitors mount health and force-unmounts on crash | VERIFIED | `mount.rs:75–101`: `spawn_watchdog()` checks `fs::metadata()` every 5s, runs `umount -f` on failure. Spawned at line 535, joined at line 556 |
| 12 | fuse-t.ini fallback modifies config atomically and restores after mount init | VERIFIED | `mount.rs:141–169`: `with_fuse_t_ini_backend()` with `FuseTIniGuard` RAII struct. `Drop` restores original. 4 inject tests pass |
| 13 | --backend and --force CLI flags are wired through to run_mount | VERIFIED | `main.rs:35–36`: `backend.as_deref()` and `force` passed to `run_mount()`. `run_mount` signature at `mount.rs:427–438` accepts both |

**Score:** 13/13 truths verified

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `crates/slicefs-cli/src/backend.rs` | FuseTBackend enum, detect_fuse_t_version(), select_backend(), is_fskit_available(), confirm_fallback() | VERIFIED | 494 lines. All exports present and substantive. 27 unit tests |
| `crates/slicefs-cli/src/cli.rs` | --backend and --force CLI flags on Mount subcommand | VERIFIED | Lines 63–67: both flags present with correct clap attributes |
| `crates/slicefs-cli/src/unmount.rs` | Enhanced multi-step unmount with process kill and force unmount | VERIFIED | Lines 89–111: `kill_processes_at_mountpoint()` with lsof+SIGTERM. 4 unit tests |
| `crates/slicefs-cli/src/mount.rs` | Backend-aware mount with signal handler, watchdog, fuse-t.ini fallback, startup logging | VERIFIED | Signal handler line 55–61, watchdog line 75–101, fuse-t.ini lines 142–169, startup log lines 542–549 |
| `crates/slicefs-cli/src/main.rs` | CLI flag passthrough for backend and force | VERIFIED | Lines 25,35–36: destructures both fields, passes them through |

### Key Link Verification

| From | To | Via | Status | Details |
|------|----|-----|--------|---------|
| `cli.rs` | `main.rs` | Mount variant fields backend + force | VERIFIED | `main.rs:25` destructs `backend, force`; lines 35–36 pass them to `run_mount` |
| `backend.rs` | `mount.rs` | `select_backend_auto()` called before mount2() | VERIFIED | `mount.rs:453`: `select_backend_auto(requested, force)` called in macOS block before `load_store()` |
| `mount.rs` | `fuser::mount2` | CUSTOM backend mount option passed in Config | VERIFIED | `mount.rs:352–354`: `MountOption::CUSTOM(b.as_mount_option())` pushed when backend is Some |
| `mount.rs` | watchdog thread | `spawn_watchdog()` before mount2(), shutdown after | VERIFIED | `mount.rs:535–556`: spawn at 535, `mount2` at 552, `watchdog_shutdown.store(true)` + join at 555–556 |

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|------------|-------------|--------|----------|
| FUSET-01 | 12-01 | Auto-detect best FUSE-T backend: FSKit > SMB > NFS | SATISFIED | `select_backend()` auto-detect branch; `is_fskit_available()` |
| FUSET-02 | 12-01 | `--backend=nfs|smb|fskit` CLI flag overrides auto-detection | SATISFIED | `cli.rs:63`, `parse_backend_flag()` in `backend.rs:49` |
| FUSET-03 | 12-01 | NFS blocked by default unless `--backend=nfs` or `--force` | SATISFIED | `backend.rs:166–170`; `select_backend` Err on NFS without force |
| FUSET-04 | 12-01 | Version detected from dylib; minimum 1.0.35 required | SATISFIED | `detect_fuse_t_version_from_path()` + `MIN_VERSION = (1,0,35)` |
| FUSET-05 | 12-02 | Signal handler (SIGTERM/SIGINT) + watchdog thread | SATISFIED | `register_signal_handlers()` + `spawn_watchdog()` in `mount.rs` |
| FUSET-06 | 12-01 | Enhanced unmount: soft -> kill -> force -> mount.lock | SATISFIED | `unmount.rs:27–53`: 4-step sequence |
| FUSET-07 | 12-02 | Startup log includes backend name and FUSE-T version | SATISFIED | `mount.rs:542–549`: `println!` with backend and version tuple |
| FUSET-08 | 12-02 | fuse-t.ini fallback for older FUSE-T versions | SATISFIED | `with_fuse_t_ini_backend()` + `FuseTIniGuard` Drop in `mount.rs` |

All 8 FUSET requirements satisfied. REQUIREMENTS.md traceability table shows all as "Planned" (status not yet updated to Complete — this is a documentation-only gap, not a code gap).

### Anti-Patterns Found

None. Zero TODO/FIXME/PLACEHOLDER comments in any phase-modified file.

### Build and Test Results

```
cargo build -p slicefs-cli   -> Finished (0 errors, pre-existing warnings only)
cargo test --bin slicefs backend  -> 33 passed, 0 failed
cargo test --workspace           -> all test suites passed, 0 failures
```

Specific test counts for phase-delivered functionality:
- `backend::tests` — 27 tests: version parsing, selection priority, NFS blocking, force override, FSKit unavailability, confirm_fallback TTY/non-TTY
- `mount::tests` (new in phase) — 6 tests: `test_build_mount_options_with_backend_smb`, `test_build_mount_options_no_backend`, 4 `inject_backend_into_ini` tests
- `unmount::tests` — 4 tests: non-existent path, mount.lock cleanup, no-store no-op, nonexistent lock

### Human Verification Required

The following behaviors cannot be fully verified programmatically and require a real FUSE-T installation on macOS:

1. **Backend CUSTOM option accepted by FUSE-T**
   - Test: Run `slicefs mount <mp> --store <store>` on macOS with FUSE-T 1.0.54 installed
   - Expected: Mount succeeds using SMB backend; no "invalid option" error from FUSE-T
   - Why human: Requires live FUSE-T kernel extension; cannot mock in unit tests

2. **Watchdog triggers on unexpected unmount**
   - Test: Mount, then run `diskutil unmount force <mp>` from another terminal
   - Expected: Watchdog detects `fs::metadata` failure within 5s, logs "[watchdog] mount health check failed"
   - Why human: Requires live FUSE mount session

3. **Interactive fallback prompt on TTY**
   - Test: Run `slicefs mount` on macOS 24 or 25 (FSKit unavailable) in an interactive terminal
   - Expected: "warning: FSKit backend not available... Continue with Smb backend? [Y/n]" appears on stderr
   - Why human: `is_interactive()` uses `libc::isatty` which is always false in test harness

4. **fuse-t.ini fallback trigger**
   - Test: Use a FUSE-T version that rejects CUSTOM mount options
   - Expected: Fallback to fuse-t.ini injection; `[Default]\nbackend=smb` written atomically
   - Why human: Requires specific older FUSE-T version

---

## Summary

Phase 12 goal is fully achieved. All 13 observable truths are verified against the actual codebase:

- `backend.rs` (494 lines, 27 tests) delivers complete FUSE-T backend detection and selection with TTY-aware fallback prompting
- Enhanced `unmount.rs` implements the documented 4-step sequence: soft -> kill (lsof+SIGTERM) -> force (diskutil/umount -f) -> mount.lock cleanup
- `mount.rs` wires backend selection before `load_store()`, registers signal handlers, spawns a 5-second watchdog, injects the CUSTOM backend mount option, logs backend+version on startup, and provides a RAII fuse-t.ini fallback with guaranteed restoration via `Drop`
- `cli.rs` exposes `--backend` and `--force` on mount and `--store` on unmount
- `main.rs` passes all new flags through to their respective handlers
- Zero workspace test regressions (all test suites pass)
- 4 human-verification items identified for behaviors requiring a live FUSE-T installation

---

_Verified: 2026-03-31_
_Verifier: Claude (gsd-verifier)_
