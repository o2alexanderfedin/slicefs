# Phase 12: Add SMB/FSKit Backend Support for FUSE-T - Context

**Gathered:** 2026-03-31
**Status:** Ready for planning

<domain>
## Phase Boundary

Configure FUSE-T to use the SMB or FSKit backend instead of the broken NFS backend on macOS. The NFS backend has a confirmed macOS kernel bug (FUSE-T Issue #45) that deadlocks on simultaneous read+write file descriptors (e.g., `cp`). This phase adds backend auto-detection, selection, fallback, and enhanced unmount — all at the mount-time configuration level. No changes to the FUSE filesystem implementation itself.

</domain>

<decisions>
## Implementation Decisions

### Backend selection strategy
- Auto-detect best available backend at mount time: FSKit > SMB > NFS priority
- Add `--backend=nfs|smb|fskit` CLI flag to override auto-detection
- Use FUSE mount option (`-o backend=smb`) as primary mechanism
- Fall back to modifying fuse-t.ini temporarily if FUSE-T version doesn't support mount-level backend option

### Fallback behavior
- Interactive (TTY): prompt user for confirmation when falling back to a lower backend
- Non-interactive (no TTY): auto-fallback silently with warning to stderr
- **NFS is blocked by default** — refuse to mount with NFS backend unless `--backend=nfs` or `--force` is explicitly passed
- When NFS is blocked, error message explains why and suggests `--backend=smb`

### FUSE-T version handling
- Detect version by parsing `/usr/local/lib/libfuse-t-*.dylib` filename
- Require FUSE-T 1.0.35+ minimum (SMB backend availability)
- Refuse to mount on older FUSE-T with clear error message
- FSKit detection: check macOS version (26+) AND `/Applications/fuse-t.app` exists

### Stuck mount protection
- Both signal handler AND watchdog approach — safety and completeness
- Catch SIGTERM/SIGINT for graceful shutdown (proper unmount before exit)
- Watchdog monitors mount health, force-unmounts on daemon crash
- Enhanced `slicefs unmount`: try umount → kill processes → force umount → clean mount.lock — one command fixes everything

### Startup logging
- Always log backend info on mount: `SliceFS mounted at /path (backend: smb, fuse-t: 1.0.54)`
- Integrates with existing exhaustive FUSE callback logging

### Claude's Discretion
- Exact watchdog implementation (thread vs process, health check mechanism)
- How to temporarily modify/restore fuse-t.ini safely (file locking, atomic write)
- Signal handler registration approach (ctrlc crate vs raw libc)
- Exact version parsing strategy for edge cases

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- `mount.rs::build_mount_options()` — already builds FUSE mount config, add backend option here
- `mount.rs::run_mount()` — main mount function, add backend detection before `mount2()` call
- `mount.rs::check_mountpoint_not_in_use()` — pre-flight check pattern, extend for backend detection
- `mount.rs::parse_wal_config()` — CLI flag parsing pattern to follow for `--backend`
- `unmount.rs` — existing unmount implementation to enhance

### Established Patterns
- Mount options via `MountOption::CUSTOM(string)` — same mechanism for `backend=smb`
- CLI argument parsing in `main.rs` — add `--backend` and `--force` flags following existing patterns
- Error handling with `Box<dyn Error>` return — consistent across mount functions
- eprintln! for FUSE trace logging — backend selection should log similarly

### Integration Points
- `build_mount_options()` — add `CUSTOM("backend=smb")` based on detection result
- `run_mount()` — add backend detection + fallback logic before `mount2()`
- CLI arg parsing — add `--backend` and `--force` flags
- `slicefs unmount` command — enhance with process cleanup and force-unmount

</code_context>

<specifics>
## Specific Ideas

- User explicitly wants SMB as default — NFS should never be used silently
- "Reasonably choose towards safety and precision and completeness" for mount protection
- Backend info must be logged on every mount for debugging (already have exhaustive FUSE logging)
- The fuse-t.ini at `/Library/Application Support/fuse-t/cfg/fuse-t.ini` is the system config — modify only as fallback, always restore

</specifics>

<deferred>
## Deferred Ideas

- Linux support for this phase — Linux uses libfuse (kernel FUSE), not FUSE-T. Backend selection is macOS-only.
- Windows WinFSP support — mentioned in PROJECT.md but out of scope for this phase
- FSKit-only mode (dropping FUSE entirely) — would require different filesystem trait, much larger scope

</deferred>

---

*Phase: 12-add-smb-fskit-backend-support-for-fuse-t*
*Context gathered: 2026-03-31*
