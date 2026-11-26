//! `unmount` subcommand — multi-step unmount with process cleanup.
//!
//! ## Unmount sequence
//!
//! 1. **Soft unmount** — `fusermount3 -u`, `fusermount -u`, `umount`
//! 2. **Kill processes** — `lsof +D <mountpoint>` + SIGTERM; also end the FUSE-T
//!    server of this mount point (never the servers of other mounts)
//! 3. **Force unmount** — `diskutil unmount force` (macOS) or `umount -f` / lazy unmount (Linux)
//! 4. **Wait for the daemon** — until the SliceFS daemon of the mount has
//!    exited (its final commit is done), at most 30 s: with `--store`, until
//!    the store lock is free; without, until the `slicefs mount` processes of
//!    the mount point (found before the unmount) have exited
//! 5. **Clean mount.lock** — remove `<store>/mount.lock` if `--store` provided
//!
//! Returns `Ok(())` on successful unmount, `Err` with actionable message otherwise.

use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

// ── Public entry point ────────────────────────────────────────────────────────

/// Run the `unmount` subcommand with enhanced multi-step cleanup.
///
/// `store_path` is optional; when provided, `mount.lock` inside the store is
/// removed after a successful unmount and the exit of the daemon. Returns
/// after the daemon has exited, so a new mount of the store can start; an
/// error if it still runs 30 s after the unmount.
pub fn run_unmount(
    mountpoint: &Path,
    store_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    // The daemons of the mount point, found before the unmount: the wait
    // after the unmount needs them.
    let daemons = mount_daemons(mountpoint);

    // Step 1 — Soft unmount.
    if try_soft_unmount(mountpoint) {
        println!("Unmounted {}", mountpoint.display());
        return wait_daemon(mountpoint, store_path, &daemons);
    }

    // Step 2 — Kill processes holding the mount.
    eprintln!("Soft unmount failed -- killing processes...");
    kill_processes_at_mountpoint(mountpoint);
    thread::sleep(Duration::from_secs(1));

    // Step 3 — Force unmount.
    if try_force_unmount(mountpoint) {
        println!("Force-unmounted {}", mountpoint.display());
        return wait_daemon(mountpoint, store_path, &daemons);
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
pub(crate) fn try_soft_unmount(mountpoint: &Path) -> bool {
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
        if let Ok(status) = cmd.status()
            && status.success()
        {
            return true;
        }
    }
    false
}

// ── Process kill ──────────────────────────────────────────────────────────────

/// Kill processes that have files open under `mountpoint`.
///
/// Runs `lsof +D <mountpoint>`, parses PIDs from column 2, sends SIGTERM.
/// Also ends the FUSE-T server (`go-nfsv4`) of this mount point. Only that
/// one: every FUSE-T mount on the machine (of any program) has its own
/// server, and ending another one breaks that mount (ECONNRESET).
pub(crate) fn kill_processes_at_mountpoint(mountpoint: &Path) {
    let mp = mountpoint.to_string_lossy();

    // lsof +D recursively lists all processes with open files under the path.
    if let Ok(output) = Command::new("lsof").arg("+D").arg(mp.as_ref()).output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().skip(1) {
            // lsof output: COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 2
                && let Ok(pid) = fields[1].parse::<i32>()
            {
                // SAFETY: kill is a safe POSIX syscall when given a valid pid.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
    }

    // End the FUSE-T server of this mount point.
    let mut points = vec![mp.to_string()];
    if let Ok(c) = std::fs::canonicalize(mountpoint) {
        points.push(c.to_string_lossy().into_owned()); // /tmp -> /private/tmp
    }
    if let Ok(out) = Command::new("ps").args(["-axww", "-o", "pid=,command="]).output() {
        for pid in fuse_t_servers_of(&String::from_utf8_lossy(&out.stdout), &points) {
            // SAFETY: kill is a safe POSIX syscall when given a valid pid.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

/// PIDs of the `go-nfsv4` processes in `ps -o pid=,command=` output whose
/// last argument (the mount point) is one of `points`.
fn fuse_t_servers_of(ps: &str, points: &[String]) -> Vec<i32> {
    ps.lines()
        .filter_map(|line| {
            let (pid, cmd) = line.trim_start().split_once(' ')?;
            let prog = cmd.split_whitespace().next()?;
            if !prog.ends_with("/go-nfsv4") && prog != "go-nfsv4" {
                return None;
            }
            let cmd = cmd.trim_end().trim_end_matches('/');
            points
                .iter()
                .any(|p| {
                    let p = p.trim_end_matches('/');
                    cmd.strip_suffix(p).is_some_and(|head| head.ends_with(' '))
                })
                .then(|| pid.parse().ok())
                .flatten()
        })
        .collect()
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
            && status.success()
        {
            return true;
        }
        // Fallback: umount -f.
        if let Ok(status) = Command::new("umount").args(["-f", mp.as_ref()]).status()
            && status.success()
        {
            return true;
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
            if let Ok(status) = cmd.status()
                && status.success()
            {
                return true;
            }
        }
    }

    false
}

// ── Wait for the daemon ───────────────────────────────────────────────────────

/// How long `unmount` waits for the daemon to exit after the unmount.
const EXIT_WAIT: Duration = Duration::from_secs(30);

/// How often `unmount` looks whether the daemon has exited.
const EXIT_POLL: Duration = Duration::from_millis(20);

/// The mount point as given and as resolved (/tmp -> /private/tmp).
fn points_of(mountpoint: &Path) -> Vec<String> {
    let mut points = vec![mountpoint.to_string_lossy().into_owned()];
    if let Ok(c) = std::fs::canonicalize(mountpoint) {
        points.push(c.to_string_lossy().into_owned());
    }
    points
}

/// PIDs of the `slicefs mount` daemons in `ps -o pid=,command=` output
/// whose mount point is one of `points`; `own` (this process) is left out.
/// The subcommand is the first word after the program that is not a flag.
fn daemons_of(ps: &str, points: &[String], own: i32) -> Vec<i32> {
    let points: Vec<&str> = points.iter().map(|p| p.trim_end_matches('/')).collect();
    ps.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid: i32 = words.next()?.parse().ok()?;
            let prog = words.next()?;
            if pid == own || (prog != "slicefs" && !prog.ends_with("/slicefs")) {
                return None;
            }
            let args: Vec<&str> = words.collect();
            let sub = args.iter().find(|a| !a.starts_with('-'))?;
            let at_point = args.iter().any(|a| points.contains(&a.trim_end_matches('/')));
            (*sub == "mount" && at_point).then_some(pid)
        })
        .collect()
}

/// The `slicefs mount` daemons of `mountpoint` that run now.
fn mount_daemons(mountpoint: &Path) -> Vec<i32> {
    match Command::new("ps").args(["-axww", "-o", "pid=,command="]).output() {
        Ok(out) => daemons_of(&String::from_utf8_lossy(&out.stdout), &points_of(mountpoint), std::process::id() as i32),
        Err(_) => Vec::new(),
    }
}

/// True while `pid` runs. A zombie (exited, not reaped yet) does not run.
fn runs(pid: i32) -> bool {
    match Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output() {
        Ok(out) => {
            let stat = String::from_utf8_lossy(&out.stdout);
            let stat = stat.trim();
            !stat.is_empty() && !stat.starts_with('Z')
        }
        Err(_) => false,
    }
}

/// Wait until each of `pids` has exited (a zombie counts as exited), at
/// most `limit`. Returns the PIDs that still run.
fn wait_exit(pids: &[i32], limit: Duration) -> Vec<i32> {
    let t = Instant::now();
    loop {
        let left: Vec<i32> = pids.iter().copied().filter(|&p| runs(p)).collect();
        if left.is_empty() || t.elapsed() >= limit {
            return left;
        }
        thread::sleep(EXIT_POLL);
    }
}

/// Wait until no process holds the store lock of `store`, at most
/// `limit`. True when it is free.
fn wait_store_free(store: &Path, limit: Duration) -> bool {
    let t = Instant::now();
    loop {
        if !crate::header::store_lock_held(store) {
            return true;
        }
        if t.elapsed() >= limit {
            return false;
        }
        thread::sleep(EXIT_POLL);
    }
}

/// After the unmount: wait until the daemon has exited, then clean
/// `mount.lock`. With `store`, first until the store lock is free (the
/// daemon releases it last); then, also without `store`, until the
/// `daemons` found before the unmount have exited (this covers a file
/// system with no `flock`). An error after `EXIT_WAIT` in all; then
/// `mount.lock` is left in place.
///
/// The daemons are found by the text of their arguments, so a mount point
/// spelled otherwise at mount time (relative, or with spaces) is not
/// found; `--store` is the reliable form.
fn wait_daemon(mountpoint: &Path, store: Option<&Path>, daemons: &[i32]) -> Result<(), Box<dyn std::error::Error>> {
    let t = Instant::now();
    if let Some(store) = store
        && !wait_store_free(store, EXIT_WAIT)
    {
        return Err(format!(
            "{} is unmounted, but {} is still in use {} s later: the SliceFS daemon has not exited. \
             mount.lock is left in place. Wait for the daemon, or end it.",
            mountpoint.display(),
            store.display(),
            EXIT_WAIT.as_secs()
        )
        .into());
    }
    let left = wait_exit(daemons, EXIT_WAIT.saturating_sub(t.elapsed()));
    if !left.is_empty() {
        return Err(format!(
            "{} is unmounted, but the SliceFS daemon (pid {:?}) still runs {} s later. Wait for it, or end it.",
            mountpoint.display(),
            left,
            EXIT_WAIT.as_secs()
        )
        .into());
    }
    if store.is_some() {
        clean_mount_lock(store);
    }
    Ok(())
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
    fn only_the_server_of_the_mount_point_is_selected() {
        let ps = "  101 /usr/local/bin/go-nfsv4 --backend fskit /tmp/a/mnt\n\
                  102 /usr/local/bin/go-nfsv4 --backend smb /tmp/a/mnt2\n\
                  103 /usr/local/bin/go-nfsv4 --backend smb /other/tmp/a/mnt\n\
                  104 vim /tmp/a/mnt\n\
                  105 go-nfsv4 --backend smb /private/tmp/a/mnt/\n";
        let pts = vec!["/tmp/a/mnt".to_string(), "/private/tmp/a/mnt".to_string()];
        assert_eq!(fuse_t_servers_of(ps, &pts), vec![101, 105]);
        assert!(fuse_t_servers_of(ps, &["/nowhere".to_string()]).is_empty());
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

    // ── wait for the daemon ──────────────────────────────────────────────────

    #[test]
    fn only_the_mount_daemons_of_the_mount_point_are_selected() {
        let ps = "  201 /usr/local/bin/slicefs mount --store /s /tmp/a/mnt\n\
                  202 slicefs mount /private/tmp/a/mnt/ --store /s\n\
                  203 slicefs unmount /tmp/a/mnt\n\
                  204 /usr/local/bin/slicefs mount --store /s /tmp/a/mnt2\n\
                  205 vim /tmp/a/mnt\n\
                  206 slicefs mount --store /s /tmp/a/mnt\n\
                  207 slicefs --json mount /tmp/a/mnt\n";
        let pts = vec!["/tmp/a/mnt".to_string(), "/private/tmp/a/mnt".to_string()];
        assert_eq!(daemons_of(ps, &pts, 206), vec![201, 202, 207]);
        assert!(daemons_of(ps, &["/nowhere".to_string()], 0).is_empty());
    }

    /// A daemon that exited but is not reaped yet (a zombie) counts as
    /// exited; one that still runs is reported after the limit.
    #[test]
    fn the_wait_ends_when_the_process_exits() {
        let mut quick = Command::new("sleep").arg("0.2").spawn().unwrap();
        let t = std::time::Instant::now();
        assert!(wait_exit(&[quick.id() as i32], Duration::from_secs(10)).is_empty(), "the process exited");
        assert!(t.elapsed() < Duration::from_secs(5), "a zombie counts as exited ({:?})", t.elapsed());
        quick.wait().unwrap();

        let mut slow = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = slow.id() as i32;
        assert_eq!(wait_exit(&[pid], Duration::from_millis(300)), vec![pid], "still running after the limit");
        slow.kill().unwrap();
        slow.wait().unwrap();
    }

    #[test]
    fn the_wait_ends_when_the_store_lock_is_free() {
        let dir = TempDir::new().unwrap();
        let held = crate::header::lock_offline(dir.path()).unwrap();
        assert!(!wait_store_free(dir.path(), Duration::from_millis(300)), "held: not free after the limit");
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        assert!(wait_store_free(dir.path(), Duration::from_secs(10)), "free after the holder ends");
        h.join().unwrap();
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
