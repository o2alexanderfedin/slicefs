# Phase 12: Add SMB/FSKit Backend Support for FUSE-T - Research

**Researched:** 2026-03-31
**Domain:** FUSE-T backend configuration, macOS mount lifecycle, Rust signal handling, process cleanup
**Confidence:** HIGH

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions
- Auto-detect best available backend at mount time: FSKit > SMB > NFS priority
- Add `--backend=nfs|smb|fskit` CLI flag to override auto-detection
- Use FUSE mount option (`-o backend=smb`) as primary mechanism
- Fall back to modifying fuse-t.ini temporarily if FUSE-T version doesn't support mount-level backend option
- Interactive (TTY): prompt user for confirmation when falling back to a lower backend
- Non-interactive (no TTY): auto-fallback silently with warning to stderr
- NFS is blocked by default — refuse to mount with NFS backend unless `--backend=nfs` or `--force` is explicitly passed
- When NFS is blocked, error message explains why and suggests `--backend=smb`
- Detect version by parsing `/usr/local/lib/libfuse-t-*.dylib` filename
- Require FUSE-T 1.0.35+ minimum (SMB backend availability)
- Refuse to mount on older FUSE-T with clear error message
- FSKit detection: check macOS version (26+) AND `/Applications/fuse-t.app` exists
- Both signal handler AND watchdog approach — safety and completeness
- Catch SIGTERM/SIGINT for graceful shutdown (proper unmount before exit)
- Watchdog monitors mount health, force-unmounts on daemon crash
- Enhanced `slicefs unmount`: try umount → kill processes → force umount → clean mount.lock — one command fixes everything
- Always log backend info on mount: `SliceFS mounted at /path (backend: smb, fuse-t: 1.0.54)`
- Integrates with existing exhaustive FUSE callback logging

### Claude's Discretion
- Exact watchdog implementation (thread vs process, health check mechanism)
- How to temporarily modify/restore fuse-t.ini safely (file locking, atomic write)
- Signal handler registration approach (ctrlc crate vs raw libc)
- Exact version parsing strategy for edge cases

### Deferred Ideas (OUT OF SCOPE)
- Linux support for this phase — Linux uses libfuse (kernel FUSE), not FUSE-T. Backend selection is macOS-only.
- Windows WinFSP support — mentioned in PROJECT.md but out of scope for this phase
- FSKit-only mode (dropping FUSE entirely) — would require different filesystem trait, much larger scope
</user_constraints>

---

## Summary

This phase adds FUSE-T backend selection (SMB/FSKit preferred, NFS blocked by default) to the SliceFS mount process. The motivation is FUSE-T Issue #45: the macOS kernel NFS client has a confirmed data corruption bug on Sonoma 14.1.1+ with Apple Silicon when files are simultaneously read and written. The SMB backend (added in FUSE-T 1.0.35, now production-stable as of 1.0.54) and FSKit backend (added in FUSE-T 1.1.0, requires macOS 26+) bypass the broken NFS code path.

The implementation is entirely in `slicefs-cli` — no changes to the filesystem implementation itself. It touches three files: `cli.rs` (add `--backend` and `--force` flags), `mount.rs` (backend detection, selection, fuse-t.ini fallback, signal handling, watchdog, startup logging), and `unmount.rs` (enhanced cleanup: process kill, force umount, mount.lock removal). The installed FUSE-T version is 1.0.54, which fully supports both the `-o backend=smb` mount option and FSKit (though FSKit requires fuse-t.app in /Applications which is not present on this machine — FSKit will be detected as unavailable).

The primary implementation risk is fuse-t.ini modification: the file is system-level and must be restored atomically. Signal handling in Rust without adding new crates is done with `libc` (already a dependency). The watchdog is best implemented as a background `std::thread` using `Arc<AtomicBool>` for health heartbeat, consistent with the existing background GC thread pattern.

**Primary recommendation:** Use `MountOption::CUSTOM("backend=smb")` as the primary mechanism. Fall back to fuse-t.ini only when FUSE-T is detected as older than the mount-option support (pre-1.0.35 in practice will never have SMB anyway — the real fallback path is for versions between 1.0.35 and the version that added mount-level option support, which appears to be 1.0.35 itself). The fuse-t.ini fallback is low-risk since 1.0.54 is installed and supports the mount option directly.

---

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| fuser | 0.17.0 | FUSE mount lifecycle | Already the project's FUSE adapter |
| libc | 0.2.183 | SIGTERM/SIGINT signal registration via `libc::signal` | Already a dependency; no new crate needed |
| clap | 4.6.0 | `--backend` and `--force` CLI flags | Already the project's CLI parser |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| std::thread | std | Watchdog background thread | Mount health monitoring |
| std::sync::atomic::AtomicBool | std | Shutdown/health signal between threads | Watchdog heartbeat pattern (already used in GC thread) |
| std::fs::read_dir | std | Dylib glob to detect FUSE-T version | Version detection at mount time |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| Raw `libc::signal` | `ctrlc` crate (not yet a dep) | ctrlc is simpler but adds a dependency; libc is already present and sufficient |
| Raw `libc::signal` | `signal-hook` crate | signal-hook is safer (async-signal-safe) but overkill for our single-action use case |
| Inline watchdog thread | Separate watchdog process | Thread is simpler, consistent with GC pattern, sufficient for mount health |

**Installation:** No new dependencies required. All needed crates (`libc`, `clap`, `fuser`, `std`) are already in `slicefs-cli/Cargo.toml`.

---

## Architecture Patterns

### Recommended Project Structure

No new files needed. Changes are to existing files only:

```
crates/slicefs-cli/src/
├── cli.rs          # Add --backend=<nfs|smb|fskit> and --force flags to Mount
├── mount.rs        # Backend detection, selection, fuse-t.ini fallback,
│                   # signal handler registration, watchdog thread, startup logging
└── unmount.rs      # Enhanced cleanup: kill procs → force umount → clean mount.lock
```

### Pattern 1: Backend Detection via Dylib Glob

**What:** Parse FUSE-T version from `/usr/local/lib/libfuse-t-*.dylib` filename. Format is `libfuse-t-{major}.{minor}.{patch}.dylib`. The installed version on this machine is `1.0.54`.

**When to use:** Always called at mount time before selecting backend.

**Example:**
```rust
// Source: Derived from confirmed file at /usr/local/lib/libfuse-t-1.0.54.dylib
fn detect_fuse_t_version() -> Option<(u32, u32, u32)> {
    let lib_dir = std::path::Path::new("/usr/local/lib");
    for entry in std::fs::read_dir(lib_dir).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name().to_string_lossy().to_string();
        // Match: libfuse-t-1.0.54.dylib
        if name.starts_with("libfuse-t-") && name.ends_with(".dylib") {
            let version_str = &name[10..name.len() - 6]; // strip prefix/suffix
            let parts: Vec<&str> = version_str.split('.').collect();
            if parts.len() == 3 {
                if let (Ok(maj), Ok(min), Ok(patch)) =
                    (parts[0].parse(), parts[1].parse(), parts[2].parse())
                {
                    return Some((maj, min, patch));
                }
            }
        }
    }
    None
}
```

### Pattern 2: Backend Selection Logic

**What:** FSKit > SMB > NFS priority with NFS blocked by default.

**When to use:** In `run_mount()` before calling `build_mount_options()`.

**Example:**
```rust
#[derive(Debug, Clone, PartialEq)]
pub enum FuseTBackend {
    Fskit,
    Smb,
    Nfs,
}

pub fn select_backend(
    requested: Option<FuseTBackend>,
    force: bool,
) -> Result<FuseTBackend, Box<dyn std::error::Error>> {
    let version = detect_fuse_t_version()
        .ok_or("FUSE-T not found at /usr/local/lib/libfuse-t-*.dylib")?;

    // Enforce minimum version for SMB (1.0.35+)
    if version < (1, 0, 35) {
        return Err(format!(
            "FUSE-T {}.{}.{} is too old. Minimum required: 1.0.35 (SMB backend). \
             Update FUSE-T: https://github.com/macos-fuse-t/fuse-t/releases",
            version.0, version.1, version.2
        ).into());
    }

    if let Some(req) = requested {
        // Explicit --backend=nfs requires --force
        if req == FuseTBackend::Nfs && !force {
            return Err(
                "NFS backend is blocked by default due to macOS kernel bug (FUSE-T Issue #45) \
                 causing data corruption. Use --backend=smb instead, or pass --force to \
                 override this safety check."
                    .into(),
            );
        }
        return Ok(req);
    }

    // Auto-detect: FSKit > SMB > NFS
    if is_fskit_available() {
        Ok(FuseTBackend::Fskit)
    } else {
        Ok(FuseTBackend::Smb) // SMB is the safe default
    }
}

fn is_fskit_available() -> bool {
    // FSKit requires macOS 26+ AND fuse-t.app in /Applications
    // macOS version check via sysctl kern.osproductversion or std::process::Command
    let macos_ok = check_macos_version_26_or_later();
    let app_exists = std::path::Path::new("/Applications/fuse-t.app").exists();
    macos_ok && app_exists
}
```

**Note on macOS 26+:** Current machine runs macOS 26.3. `sw_vers -productVersion` returns "26.3". Parse major version from this output. FSKit also requires fuse-t.app which is NOT currently installed (`/Applications/fuse-t.app` does not exist) — so FSKit will not be selected on this machine.

### Pattern 3: Backend as FUSE Mount Option

**What:** Pass `backend=smb` or `backend=fskit` as a FUSE CUSTOM option. This is the primary mechanism supported since FUSE-T 1.0.35.

**When to use:** After selecting backend, pass to `build_mount_options()`.

**Example:**
```rust
// In build_mount_options(), add backend option for macOS:
#[cfg(target_os = "macos")]
if let Some(backend) = backend {
    let backend_str = match backend {
        FuseTBackend::Smb => "backend=smb",
        FuseTBackend::Fskit => "backend=fskit",
        FuseTBackend::Nfs => "backend=nfs",
    };
    mount_options.push(MountOption::CUSTOM(backend_str.to_string()));
}
```

The exact FUSE-T option name confirmed from release notes: `backend=smb`, `backend=nfs`, `backend=fskit`.

### Pattern 4: fuse-t.ini Fallback (Atomic Write + Restore)

**What:** For FUSE-T versions that support backend selection only via config file (edge case — 1.0.35 already supports mount-level option), temporarily modify fuse-t.ini, mount, then restore. Since installed version is 1.0.54 this path may never trigger, but must be safe.

**When to use:** Only when mount-option approach fails AND version >= 1.0.35.

**Approach:**
```rust
const FUSE_T_INI_PATH: &str = "/Library/Application Support/fuse-t/cfg/fuse-t.ini";

fn with_fuse_t_ini_backend<F, R>(backend: &str, f: F) -> Result<R, Box<dyn std::error::Error>>
where
    F: FnOnce() -> Result<R, Box<dyn std::error::Error>>,
{
    let ini_path = std::path::Path::new(FUSE_T_INI_PATH);
    let original = std::fs::read_to_string(ini_path)?;

    // Write modified ini with backend= set (or appended under [Default])
    let modified = set_ini_backend(&original, backend);
    // Atomic write: write to .tmp, then rename
    let tmp_path = ini_path.with_extension("tmp");
    std::fs::write(&tmp_path, &modified)?;
    std::fs::rename(&tmp_path, ini_path)?;

    let result = f();

    // Always restore original
    let tmp_path = ini_path.with_extension("tmp");
    std::fs::write(&tmp_path, &original)?;
    let _ = std::fs::rename(&tmp_path, ini_path);

    result
}
```

**Key concern:** fuse-t.ini is at system path `/Library/Application Support/fuse-t/cfg/fuse-t.ini`. The file is confirmed to exist. Current content has `backend=nfs` commented out (`;backend=nfs`). Modification requires write permission to `/Library/Application Support/fuse-t/cfg/` which may require sudo. Since the primary path (mount option) works on 1.0.54, this fallback is for completeness only.

### Pattern 5: Signal Handler (SIGTERM/SIGINT)

**What:** Register OS-level signal handler that triggers graceful FUSE unmount. fuser's `mount2` already handles SIGTERM/Ctrl+C by ending the session and calling `destroy()`. The signal handler adds explicit pre-unmount flush guarantee.

**When to use:** After `load_store()`, before `mount2()` in `run_mount()`.

**Note:** fuser 0.17 on macOS already handles SIGINT/SIGTERM by ending the session. The `destroy()` callback is already fully implemented with WAL flush and commit. The signal handler here is belt-and-suspenders to ensure watchdog shutdown and any additional cleanup happens.

```rust
// Using libc (already a dependency):
#[cfg(target_os = "macos")]
unsafe fn register_signal_handlers(shutdown_flag: Arc<AtomicBool>) {
    // Store in a global that the handler can access.
    // Use libc::signal or sigaction for SIGTERM/SIGINT.
    // Simple approach: set AtomicBool which the watchdog checks.
    extern "C" fn handle_signal(_: libc::c_int) {
        // Signal-safe: just set atomic flag
        // Access via a global static AtomicBool
        SHUTDOWN_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    libc::signal(libc::SIGTERM, handle_signal as libc::sighandler_t);
    libc::signal(libc::SIGINT,  handle_signal as libc::sighandler_t);
}
```

**Alternative (cleaner):** Use `std::sync::atomic::AtomicBool` with a global static and `libc::signal`. The `libc` crate is already a project dependency so no new crate needed.

### Pattern 6: Watchdog Thread

**What:** Background thread that periodically checks if the mount is still alive. If the FUSE daemon crashes or becomes unresponsive, the watchdog calls `umount -f` on the mountpoint.

**When to use:** Spawned after `load_store()`, shut down after `mount2()` returns.

**Consistency with existing pattern:** The existing background GC thread (`spawn_background_gc`) uses `Arc<AtomicBool>` + `Duration::from_secs(60)` + a shutdown handle. The watchdog should follow the same pattern.

```rust
fn spawn_watchdog(
    mountpoint: PathBuf,
    interval: Duration,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(interval);
            if shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            // Health check: can we stat the mountpoint?
            // If not, the mount is dead — force unmount.
            if !is_mount_alive(&mountpoint) {
                eprintln!("[watchdog] mount health check failed — forcing unmount");
                let _ = std::process::Command::new("umount")
                    .arg("-f")
                    .arg(&mountpoint)
                    .status();
                break;
            }
        }
    })
}

fn is_mount_alive(mountpoint: &std::path::Path) -> bool {
    // Check mount table: if mountpoint disappears, daemon has crashed
    let output = std::process::Command::new("mount").output();
    match output {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            let mp = mountpoint.to_string_lossy();
            text.lines().any(|l| l.contains(mp.as_ref()))
        }
        Err(_) => true, // Assume alive if we can't check
    }
}
```

**Discretion recommendation:** Use a 5-second health-check interval (not 60s like GC). Mount liveness is more urgent than GC. Use `mount` table check (same as `check_mountpoint_not_in_use`) as the health probe — it's cheap and already proven in the codebase.

### Pattern 7: Enhanced Unmount

**What:** `slicefs unmount` becomes a nuclear option that reliably cleans up stuck mounts. Sequence: soft umount → kill FUSE processes → force umount → remove mount.lock.

**When to use:** Replace the current `unmount.rs` implementation.

```rust
pub fn run_unmount(mountpoint: &Path, store_path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    // Step 1: Soft unmount (existing logic)
    let soft_ok = try_soft_unmount(mountpoint);

    if soft_ok {
        println!("Unmounted {}", mountpoint.display());
        if let Some(store) = store_path {
            let _ = std::fs::remove_file(store.join("mount.lock"));
        }
        return Ok(());
    }

    // Step 2: Kill processes holding the mount open
    eprintln!("Soft unmount failed — killing processes using mountpoint...");
    kill_processes_at_mountpoint(mountpoint);

    // Step 3: Force unmount
    let force_ok = try_force_unmount(mountpoint);
    if !force_ok {
        return Err(format!(
            "Force unmount failed for {}. Try: sudo diskutil unmount force {}",
            mountpoint.display(),
            mountpoint.display()
        ).into());
    }

    // Step 4: Clean mount.lock if store path known
    if let Some(store) = store_path {
        let _ = std::fs::remove_file(store.join("mount.lock"));
    }

    println!("Force-unmounted {}", mountpoint.display());
    Ok(())
}

fn kill_processes_at_mountpoint(mountpoint: &Path) {
    // Use lsof to find processes with open files at mountpoint
    let mp = mountpoint.to_string_lossy();
    let output = std::process::Command::new("lsof")
        .arg("+D")
        .arg(mp.as_ref())
        .output();
    // Parse PIDs and send SIGTERM, then SIGKILL after 2s
    // Also kill go-nfsv4 processes (FUSE-T backend daemon)
    if let Ok(o) = output {
        for line in String::from_utf8_lossy(&o.stdout).lines().skip(1) {
            if let Some(pid_str) = line.split_whitespace().nth(1) {
                if let Ok(pid) = pid_str.parse::<i32>() {
                    unsafe { libc::kill(pid, libc::SIGTERM); }
                }
            }
        }
    }
    // Also kill orphaned go-nfsv4 processes
    let _ = std::process::Command::new("pkill").arg("-f").arg("go-nfsv4").status();
}

fn try_force_unmount(mountpoint: &Path) -> bool {
    // macOS: diskutil unmount force; fallback: umount -f
    let mp = mountpoint.to_string_lossy();
    std::process::Command::new("diskutil")
        .args(["unmount", "force", mp.as_ref()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        || std::process::Command::new("umount")
            .arg("-f")
            .arg(mp.as_ref())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}
```

**Note on Unmount CLI signature change:** The current `Cmd::Unmount` only takes `mountpoint`. To support `mount.lock` cleanup, we need to optionally accept `--store`. This is backward-compatible as an optional flag.

### Pattern 8: Startup Logging

**What:** Replace the current `println!("SliceFS mounted at {}", mountpoint.display())` with backend-aware logging.

**Example:**
```rust
// In run_mount(), after backend selection:
println!(
    "SliceFS mounted at {} (backend: {}, fuse-t: {}.{}.{})",
    mountpoint.display(),
    backend_name,
    version.0, version.1, version.2
);
```

### Anti-Patterns to Avoid

- **Do NOT hardcode `/usr/local/lib`:** Use it as the primary search path but log clearly if FUSE-T is not found. Do not silently proceed without version detection on macOS.
- **Do NOT hold fuse-t.ini lock across the entire mount lifetime:** The fuse-t.ini fallback modifies the file only during the brief window between config write and FUSE-T reading it at mount initialization — not for the entire mount session.
- **Do NOT use `umount -f` as the first attempt:** Always try soft unmount first. Force unmount without prior process termination can leave FUSE-T's go-nfsv4 backend process as a zombie, causing future mounts at the same path to fail.
- **Do NOT register signal handlers after `mount2()`:** fuser 0.17 on macOS uses a single thread; `mount2()` is blocking. Signal handlers must be registered before calling `mount2()`.

---

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| macOS version parsing | Custom string parser | `sw_vers -productVersion` + simple `split('.')` | sw_vers is stable, always present on macOS |
| Process killing | Custom `/proc` scanner | `lsof +D <mountpoint>` + `libc::kill` | lsof handles all open file types including FUSE mounts; built into macOS |
| Atomic file write | Custom temp-file logic | Write to `.tmp` then `rename` | POSIX rename is atomic on same filesystem; correct pattern already used in the codebase |
| FUSE-T version API | Version query protocol | Dylib filename parsing | No FUSE-T version API exists; filename is the canonical source |

**Key insight:** FUSE-T exposes no programmatic version API. The dylib filename at `/usr/local/lib/libfuse-t-{VERSION}.dylib` is the only reliable version indicator. The glob over `/usr/local/lib` for files matching `libfuse-t-*.dylib` is the correct approach (confirmed by inspecting the installed file).

---

## Common Pitfalls

### Pitfall 1: mount2() Is Blocking — Signal Handlers Must Be Pre-Registered
**What goes wrong:** Registering signal handlers after `mount2()` is never reached because `mount2()` blocks the thread until the session ends.
**Why it happens:** Developers forget that `mount2()` is a blocking call on all platforms.
**How to avoid:** Register signal handlers (and spawn watchdog thread) BEFORE calling `mount2()`.
**Warning signs:** Signal handler code placed after `mount2()` call in the source.

### Pitfall 2: fuse-t.ini Is System-Wide — Race with Other Mounts
**What goes wrong:** If two `slicefs mount` processes run concurrently, both may modify fuse-t.ini simultaneously, corrupting the config.
**Why it happens:** fuse-t.ini is a single system-wide file.
**How to avoid:** Use file locking (`flock` via libc) around fuse-t.ini read-modify-write. Or: since the installed version (1.0.54) supports mount-level options, the fuse-t.ini path only triggers as a last resort and can include a lock file.
**Warning signs:** Not needed on 1.0.54; only a concern if the code is ever run on 1.0.35-era installations.

### Pitfall 3: FSKit Detection — fuse-t.app Must Exist
**What goes wrong:** Detecting macOS 26+ and assuming FSKit is available without checking for fuse-t.app.
**Why it happens:** FSKit requires both a new macOS AND the fuse-t.app bundle to provide the FSKit extensions.
**How to avoid:** BOTH conditions required: `macOS_version >= 26` AND `/Applications/fuse-t.app` exists. Current machine has macOS 26.3 but `/Applications/fuse-t.app` does NOT exist — FSKit would be incorrectly selected without the app check.
**Warning signs:** Tests on macOS 26 machine where fuse-t.app was not installed via the .app bundle.

### Pitfall 4: Version Comparison — Parse All Three Components
**What goes wrong:** Comparing version strings lexicographically ("1.0.9" > "1.0.35" in lexicographic order).
**Why it happens:** String comparison doesn't handle numeric version semantics.
**How to avoid:** Parse into `(u32, u32, u32)` and compare as tuple. `(1, 0, 35) < (1, 0, 54)` is correct.
**Warning signs:** Using string comparison `>`, `<` on version strings.

### Pitfall 5: Unmount Cleanup Order — Kill Before Force-Unmount
**What goes wrong:** Calling `umount -f` before killing processes leaves go-nfsv4 as a zombie that blocks future mounts.
**Why it happens:** Force unmount disconnects the kernel's view but leaves the FUSE-T NFS backend process running.
**How to avoid:** Sequence: soft umount → kill lsof processes → pkill go-nfsv4 → force umount → remove mount.lock.
**Warning signs:** Repeated mount failures after a force-unmount, or `ps aux | grep go-nfsv4` showing zombie processes.

### Pitfall 6: NFS Fallback Prompt in Non-Interactive Mode
**What goes wrong:** Blocking forever waiting for user input when run in a script/daemon (no TTY).
**Why it happens:** Using `std::io::stdin().read_line()` without checking `isatty(0)`.
**How to avoid:** Check `unsafe { libc::isatty(0) } == 1` before prompting. If non-TTY, auto-fallback with stderr warning.
**Warning signs:** Mount command hangs in CI or automation environments.

---

## Code Examples

Verified patterns from project source and official sources:

### Adding CUSTOM Mount Option (existing pattern)
```rust
// Source: /Volumes/Unitek-B/Projects/file-systems/crates/slicefs-cli/src/mount.rs
// Existing pattern for CUSTOM options:
mount_options.push(MountOption::CUSTOM("direct_io".to_string()));
// Backend selection follows the same pattern:
mount_options.push(MountOption::CUSTOM("backend=smb".to_string()));
```

### Background Thread with AtomicBool Shutdown (existing GC pattern)
```rust
// Source: /Volumes/Unitek-B/Projects/file-systems/crates/slicefs-cli/src/mount.rs
// Existing background GC thread pattern to follow for watchdog:
let gc_shutdown = Arc::new(AtomicBool::new(false));
let gc_handle = spawn_background_gc(
    weak_meta,
    segments_dir,
    Duration::from_secs(60),
    1000,
    Arc::clone(&gc_shutdown),
);
// ... mount2 blocks ...
gc_shutdown.store(true, Ordering::SeqCst);
gc_handle.shutdown();
```

### Clap Optional Enum Flag (add to cli.rs)
```rust
// Source: clap 4 derive API — consistent with existing --wal-strategy pattern
/// FUSE-T backend: nfs, smb, or fskit (default: auto-detect, prefers fskit > smb > nfs).
/// NFS is blocked by default; use --force to override.
#[arg(long, value_name = "BACKEND")]
backend: Option<String>,  // Parse to FuseTBackend in mount.rs

/// Force use of a blocked backend (e.g., --backend=nfs --force).
#[arg(long, default_value_t = false)]
force: bool,
```

### TTY Detection for Interactive Prompt
```rust
// Source: libc crate (already a dependency)
fn is_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) != 0 }
}
```

### macOS Version Detection
```rust
fn check_macos_version_26_or_later() -> bool {
    let output = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output();
    match output {
        Ok(o) => {
            let ver = String::from_utf8_lossy(&o.stdout);
            let major: u32 = ver.trim().split('.').next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            major >= 26
        }
        Err(_) => false,
    }
}
```

---

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| NFS backend (default FUSE-T) | SMB backend (stable since 1.0.35) | FUSE-T 1.0.35, Jan 2024 | Bypasses macOS kernel NFS bug (Issue #45) |
| No backend selection | `-o backend=X` mount option | FUSE-T 1.0.35 | Per-mount backend choice without global config change |
| NFS/SMB backends only | FSKit backend added | FUSE-T 1.1.0, March 2025 | Native macOS FSKit bypass of NFS/SMB layer entirely |
| fuse-t.ini global config | Mount-level `-o backend=X` | FUSE-T 1.0.35 | No system-wide config change needed |

**Deprecated/outdated:**
- NFS backend as default: SMB is stable, NFS has confirmed kernel bug. Use SMB.
- fuse-t.ini for per-mount config: The `-o backend=X` mount option is available since 1.0.35 and is the correct approach.
- `direct_io` FUSE option: Previously used to bypass NFS page cache, removed from SliceFS because it broke FUSE-T's write forwarding (documented in mount.rs comments).

---

## Open Questions

1. **fuse-t.ini write permission**
   - What we know: The file is at `/Library/Application Support/fuse-t/cfg/fuse-t.ini`. On macOS, system Library paths require elevated permissions for non-root users.
   - What's unclear: Whether the fuse-t installer grants write permission to the standard user, or if sudo is required.
   - Recommendation: Since the fuse-t.ini path is a last-resort fallback and the installed 1.0.54 supports mount-level options, the planner should treat fuse-t.ini modification as optional/best-effort. If permission denied, fail gracefully with a message explaining the manual workaround.

2. **go-nfsv4 process name for pkill**
   - What we know: FUSE-T spawns a go-nfsv4 backend process per mount. The debug notes reference "14 stale process pairs (slicefs + go-nfsv4)".
   - What's unclear: The exact process name as it appears in `ps aux` on 1.0.54.
   - Recommendation: Use `pkill -f "go-nfsv4"` as the pattern (matches anywhere in the command line) rather than an exact process name.

3. **Watchdog health check mechanism**
   - What we know: The mount table check (via `mount` command) is already used in `check_mountpoint_not_in_use()`.
   - What's unclear: Whether stat'ing a path inside the mount point is better (faster, no shell exec) vs parsing mount table.
   - Recommendation: Use `std::fs::metadata(mountpoint)` as the primary health check — no child process needed. If metadata() returns an error suggesting the mount is gone (ENOENT, ENOTCONN), trigger cleanup.

---

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in `#[test]` (no external framework) |
| Config file | Cargo.toml test configuration (default) |
| Quick run command | `cargo test -p slicefs-cli 2>&1 \| tail -30` |
| Full suite command | `cargo test --workspace 2>&1 \| tail -30` |

### Phase Requirements to Test Map

This phase has no formal requirement IDs, but the behaviors map to tests:

| Behavior | Test Type | Automated Command |
|----------|-----------|-------------------|
| Backend auto-detect: FSKit > SMB > NFS | unit | `cargo test -p slicefs-cli detect_backend` |
| Version detection from dylib filename | unit | `cargo test -p slicefs-cli fuse_t_version` |
| NFS blocked without --force | unit | `cargo test -p slicefs-cli nfs_blocked` |
| NFS allowed with --force | unit | `cargo test -p slicefs-cli nfs_force` |
| FUSE-T too old error (< 1.0.35) | unit | `cargo test -p slicefs-cli version_too_old` |
| FSKit detection (macOS 26 + app) | unit | `cargo test -p slicefs-cli fskit_detection` |
| Backend option in mount config | unit | `cargo test -p slicefs-cli backend_in_mount_options` |
| Startup log includes backend+version | unit | `cargo test -p slicefs-cli startup_logging` |
| --backend CLI flag parsing | unit | `cargo test -p slicefs-cli backend_flag` |
| TTY detection for interactive prompt | unit | `cargo test -p slicefs-cli tty_detection` |
| Enhanced unmount sequence | manual-only | Cannot automate FUSE mount in CI |

### Sampling Rate
- **Per task commit:** `cargo test -p slicefs-cli 2>&1 | tail -30`
- **Per wave merge:** `cargo test --workspace 2>&1 | tail -30`
- **Phase gate:** Full workspace green before `/gsd:verify-work`

### Wave 0 Gaps
- [ ] `crates/slicefs-cli/src/backend.rs` — new module for backend detection/selection logic (unit-testable in isolation)

All backend detection, version parsing, and selection logic should live in a new `backend.rs` module within `slicefs-cli/src/`. This makes it independently testable without requiring a live FUSE-T installation. Tests can mock the dylib detection by accepting a path parameter.

---

## Sources

### Primary (HIGH confidence)
- Confirmed: `/usr/local/lib/libfuse-t-1.0.54.dylib` — FUSE-T 1.0.54 is installed
- Confirmed: `/Library/Application Support/fuse-t/cfg/fuse-t.ini` — system config file exists, `backend=nfs` commented out
- Confirmed: `/Applications/fuse-t.app` does NOT exist on this machine — FSKit unavailable
- Confirmed: macOS 26.3 (ProductVersion from sw_vers)
- Project source: `mount.rs`, `unmount.rs`, `cli.rs`, `filesystem.rs` — integration points fully understood
- FUSE-T Release 1.0.35: SMB backend added with `-o backend=smb` mount option ([release](https://github.com/macos-fuse-t/fuse-t/releases/tag/1.0.35))
- FUSE-T Release 1.1.0: FSKit backend added, requires macOS 26+ and fuse-t.app

### Secondary (MEDIUM confidence)
- FUSE-T Issue #45: NFS data corruption on Sonoma 14.1.1 with Apple Silicon — confirmed macOS kernel bug ([issue](https://github.com/macos-fuse-t/fuse-t/issues/45))
- FUSE-T wiki: backend option syntax `-backend=[smb|nfs|fskit]` (note: wiki shows `-backend` not `-o backend`; release notes show `-o backend=smb`; use `-o backend=smb` as FUSE option format)
- rclone forum: SMB backend usage with fuse-t confirmed working ([forum](https://forum.rclone.org/t/rclone-smb-mount-using-fuse-t/53057/1))

### Tertiary (LOW confidence)
- Watchdog thread pattern: standard Rust `AtomicBool`-based watchdog — consistent with existing GC pattern in codebase

---

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — all dependencies already present, confirmed from source
- Architecture: HIGH — integration points confirmed from code read, FUSE-T behavior confirmed from release notes and installed version
- Pitfalls: HIGH — pitfalls 1, 3, 4 confirmed from code; pitfalls 2, 5, 6 confirmed from research and debug history
- FSKit availability: HIGH — fuse-t.app not installed, macOS 26.3 present; FSKit will not be selected on this machine

**Research date:** 2026-03-31
**Valid until:** 2026-05-01 (FUSE-T is actively developed; check releases before planning if >30 days pass)
