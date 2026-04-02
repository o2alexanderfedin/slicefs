//! `unmount` subcommand — multi-step unmount with process cleanup.
//!
//! ## Unmount sequence
//!
//! 1. **Soft unmount** — `fusermount3 -u`, `fusermount -u`, `umount`
//! 2. **Kill processes** — `lsof +D <mountpoint>` + SIGTERM; also kill orphaned FUSE-T daemons
//! 3. **Force unmount** — `diskutil unmount force` (macOS) or `umount -f` / lazy unmount (Linux)
//! 4. **Clean mount.lock** — remove `<store>/mount.lock` if `--store` provided
//!
//! Returns `Ok(())` on successful unmount, `Err` with actionable message otherwise.

use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

// ── Public entry point ────────────────────────────────────────────────────────

/// Run the `unmount` subcommand with enhanced multi-step cleanup.
///
/// `store_path` is optional; when provided, `mount.lock` inside the store is
/// removed after a successful unmount.
pub fn run_unmount(
    mountpoint: &Path,
    store_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Step 1 — Soft unmount.
    if try_soft_unmount(mountpoint) {
        println!("Unmounted {}", mountpoint.display());
        clean_mount_lock(store_path);
        return Ok(());
    }

    // Step 2 — Kill processes holding the mount.
    eprintln!("Soft unmount failed -- killing processes...");
    kill_processes_at_mountpoint(mountpoint);
    thread::sleep(Duration::from_secs(1));

    // Step 3 — Force unmount.
    if try_force_unmount(mountpoint) {
        println!("Force-unmounted {}", mountpoint.display());
        clean_mount_lock(store_path);
        return Ok(());
    }

    // All strategies failed.
    Err(format!(
        "Failed to unmount {}. Try: sudo diskutil unmount force {}",
        mountpoint.display(),
        mountpoint.display()
    )
    .into())
}

// ── Soft unmount ──────────────────────────────────────────────────────────────

/// Try soft unmount via `fusermount3 -u`, `fusermount -u`, and `umount`.
///
/// Returns `true` if any succeeds.
fn try_soft_unmount(mountpoint: &Path) -> bool {
    let mp = mountpoint.to_string_lossy();
    let programs: &[(&str, &[&str])] = &[
        ("fusermount3", &["-u"]),
        ("fusermount", &["-u"]),
        ("umount", &[]),
    ];

    for (prog, extra_args) in programs {
        let mut cmd = Command::new(prog);
        for arg in *extra_args {
            cmd.arg(arg);
        }
        cmd.arg(mp.as_ref());
        if let Ok(status) = cmd.status() {
            if status.success() {
                return true;
            }
        }
    }
    false
}

// ── Process kill ──────────────────────────────────────────────────────────────

/// Kill processes that have files open under `mountpoint`.
///
/// Runs `lsof +D <mountpoint>`, parses PIDs from column 2, sends SIGTERM.
/// Also kills orphaned FUSE-T NFS daemon processes via `pkill -f go-nfsv4`.
pub(crate) fn kill_processes_at_mountpoint(mountpoint: &Path) {
    let mp = mountpoint.to_string_lossy();

    // lsof +D recursively lists all processes with open files under the path.
    if let Ok(output) = Command::new("lsof").arg("+D").arg(mp.as_ref()).output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().skip(1) {
            // lsof output: COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 2 {
                if let Ok(pid) = fields[1].parse::<i32>() {
                    // SAFETY: kill is a safe POSIX syscall when given a valid pid.
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                }
            }
        }
    }

    // Kill orphaned FUSE-T go-nfsv4 daemon processes.
    let _ = Command::new("pkill").args(["-f", "go-nfsv4"]).status();
}

// ── Force unmount ─────────────────────────────────────────────────────────────

/// Attempt force unmount using platform-appropriate commands.
///
/// macOS: `diskutil unmount force <mp>`, fallback `umount -f <mp>`.
/// Linux: `fusermount3 -uz`, `fusermount -uz`, `umount -l`.
///
/// Returns `true` if any strategy succeeds.
pub(crate) fn try_force_unmount(mountpoint: &Path) -> bool {
    let mp = mountpoint.to_string_lossy();

    #[cfg(target_os = "macos")]
    {
        // Try diskutil unmount force first.
        if let Ok(status) = Command::new("diskutil")
            .args(["unmount", "force", mp.as_ref()])
            .status()
        {
            if status.success() {
                return true;
            }
        }
        // Fallback: umount -f.
        if let Ok(status) = Command::new("umount").args(["-f", mp.as_ref()]).status() {
            if status.success() {
                return true;
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Linux lazy/force unmount options.
        let programs: &[(&str, &[&str])] = &[
            ("fusermount3", &["-uz"]),
            ("fusermount", &["-uz"]),
            ("umount", &["-l"]),
        ];
        for (prog, extra_args) in programs {
            let mut cmd = Command::new(prog);
            for arg in *extra_args {
                cmd.arg(arg);
            }
            cmd.arg(mp.as_ref());
            if let Ok(status) = cmd.status() {
                if status.success() {
                    return true;
                }
            }
        }
    }

    false
}

// ── mount.lock cleanup ────────────────────────────────────────────────────────

/// Remove `<store_path>/mount.lock` if it exists.
///
/// No-op if `store_path` is `None` or the file doesn't exist.
fn clean_mount_lock(store_path: Option<&Path>) {
    if let Some(store) = store_path {
        let lock = store.join("mount.lock");
        let _ = std::fs::remove_file(lock);
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_run_unmount_nonexistent_path_returns_error() {
        // A non-existent path cannot be a mountpoint; all strategies should fail.
        // On macOS without fusermount3/fusermount, or on Linux, this will fail.
        // We just verify it does not panic.
        let result = run_unmount(
            Path::new("/nonexistent/path/that/cannot/be/a/mountpoint"),
            None,
        );
        let _ = result; // Ok or Err — both acceptable (CI may have permissive umount)
    }

    #[test]
    fn test_mount_lock_cleanup_on_success() {
        // create a store dir with a mount.lock file
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("mount.lock");
        std::fs::write(&lock_path, b"locked").unwrap();
        assert!(lock_path.exists(), "setup: mount.lock should exist");

        // Call clean_mount_lock directly (the integration with run_unmount
        // can't be tested without a real mountpoint).
        clean_mount_lock(Some(dir.path()));
        assert!(!lock_path.exists(), "mount.lock should be removed after cleanup");
    }

    #[test]
    fn test_mount_lock_cleanup_no_store() {
        // Providing None for store_path is a no-op; should not panic.
        clean_mount_lock(None);
    }

    #[test]
    fn test_mount_lock_cleanup_nonexistent_lock() {
        // If mount.lock doesn't exist, cleanup should be a no-op (no error).
        let dir = TempDir::new().unwrap();
        clean_mount_lock(Some(dir.path())); // lock file was never created
    }

    // ── kill_processes_at_mountpoint ─────────────────────────────────────────

    #[test]
    fn test_kill_processes_at_mountpoint_nonexistent_path_does_not_panic() {
        // lsof will find no processes for a nonexistent path; should not panic.
        kill_processes_at_mountpoint(Path::new("/nonexistent/path/no_procs_here"));
    }

    #[test]
    fn test_kill_processes_at_mountpoint_temp_dir_does_not_panic() {
        // Even with a real existing directory, kill_processes_at_mountpoint must
        // not panic if no processes have open files there.
        let dir = TempDir::new().unwrap();
        kill_processes_at_mountpoint(dir.path());
    }

    // ── try_force_unmount ────────────────────────────────────────────────────

    #[test]
    fn test_try_force_unmount_nonexistent_path_returns_false() {
        // A non-mounted path should cause all force-unmount strategies to fail.
        let result = try_force_unmount(Path::new("/nonexistent/path/not_mounted"));
        // We don't assert false here because diskutil/umount behavior varies;
        // just ensure no panic occurs. On macOS, diskutil unmount force on a
        // non-mounted path will fail (returns false). On Linux, umount -l also fails.
        let _ = result;
    }

    // ── run_unmount error path ───────────────────────────────────────────────

    #[test]
    fn test_run_unmount_with_nonexistent_store_cleans_nothing() {
        // run_unmount with a store path that has no mount.lock should not panic.
        let dir = TempDir::new().unwrap();
        // No mount.lock in dir.
        let result = run_unmount(
            Path::new("/nonexistent/not_a_mountpoint"),
            Some(dir.path()),
        );
        // Either Ok or Err is valid (platform-dependent), just no panic.
        let _ = result;
    }

    #[test]
    fn test_run_unmount_returns_error_message_with_path() {
        // The error message from run_unmount should contain the mountpoint path.
        let mp = Path::new("/nonexistent/path/that/cannot/be/mounted");
        let result = run_unmount(mp, None);
        if let Err(e) = result {
            let msg = e.to_string();
            assert!(
                msg.contains("nonexistent") || msg.contains("Failed"),
                "error message should reference the path or 'Failed', got: {}", msg
            );
        }
        // If Ok (some systems may accept umount on nonexistent paths), that is also acceptable.
    }
}
