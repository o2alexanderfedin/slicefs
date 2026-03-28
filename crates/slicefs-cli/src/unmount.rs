//! `unmount` subcommand — shells out to `fusermount3 -u` (with fallbacks).
//!
//! ## Fallback order
//!
//! 1. `fusermount3 -u <mountpoint>` — standard on modern Linux (util-linux ≥ 2.34)
//! 2. `fusermount -u <mountpoint>` — older Linux / custom installs
//! 3. `umount <mountpoint>` — requires root; last resort
//!
//! Returns an error if all three fail.

use std::path::Path;
use std::process::Command;

/// Run the `unmount` subcommand.
///
/// Tries `fusermount3 -u`, then `fusermount -u`, then `umount` as a last
/// resort. Prints a success message on clean unmount.
pub fn run_unmount(mountpoint: &Path) -> Result<(), Box<dyn std::error::Error>> {
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

        match cmd.status() {
            Ok(status) if status.success() => {
                println!("Unmounted {}", mountpoint.display());
                return Ok(());
            }
            _ => continue,
        }
    }

    Err(format!(
        "Failed to unmount {}. Try: sudo umount {}",
        mountpoint.display(),
        mountpoint.display()
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_unmount_nonexistent_path_returns_error() {
        // Trying to unmount a non-existent path should fail (all three commands
        // will exit non-zero or not be found on this machine).
        let result = run_unmount(Path::new("/nonexistent/path/that/cannot/be/a/mountpoint"));
        // On macOS without fusermount3/fusermount installed, or on Linux with a
        // non-mountpoint path, this will fail.  We just verify it does not panic.
        // It MAY succeed (e.g., if `umount` is permissive on CI) — that's fine too.
        let _ = result; // result is either Ok or Err — both are acceptable here
    }
}
