//! FUSE-T backend detection and selection for SliceFS.
//!
//! Detects the installed FUSE-T version, determines which backend (SMB, FSKit, or NFS)
//! to use, and handles CLI flag parsing.
//!
//! ## Backend priority (auto-detect)
//!
//! FSKit > SMB > NFS
//!
//! NFS is blocked by default due to a macOS kernel bug (FUSE-T Issue #45) that deadlocks
//! on simultaneous read+write file descriptors. Pass `--backend=nfs --force` to override.

use std::fs;
use std::io::BufRead;
use std::path::Path;
use std::process::Command;

// ── Backend enum ──────────────────────────────────────────────────────────────

/// FUSE-T mount backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuseTBackend {
    /// FSKit backend — macOS 26+ only, requires fuse-t.app 1.2.0+.
    Fskit,
    /// SMB backend — available since FUSE-T 1.0.35. Recommended default.
    Smb,
    /// NFS backend — blocked by default due to macOS kernel bug (FUSE-T Issue #45).
    Nfs,
}

impl FuseTBackend {
    /// Returns the FUSE-T mount option string for this backend.
    ///
    /// Pass the result as `-o <value>` to FUSE-T at mount time.
    pub fn as_mount_option(&self) -> &'static str {
        match self {
            FuseTBackend::Fskit => "backend=fskit",
            FuseTBackend::Smb => "backend=smb",
            FuseTBackend::Nfs => "backend=nfs",
        }
    }
}

// ── CLI flag parsing ──────────────────────────────────────────────────────────

/// Parse a `--backend` CLI flag value into a [`FuseTBackend`].
///
/// Accepts `"smb"`, `"nfs"`, or `"fskit"` (case-sensitive).
pub fn parse_backend_flag(s: &str) -> Result<FuseTBackend, String> {
    match s {
        "smb" => Ok(FuseTBackend::Smb),
        "nfs" => Ok(FuseTBackend::Nfs),
        "fskit" => Ok(FuseTBackend::Fskit),
        other => Err(format!(
            "unknown backend {:?}. Valid values: smb, nfs, fskit",
            other
        )),
    }
}

// ── Version detection ─────────────────────────────────────────────────────────

/// Detect the installed FUSE-T version from `/usr/local/lib/libfuse-t-*.dylib`.
///
/// Returns `None` if FUSE-T is not installed or the version cannot be parsed.
pub fn detect_fuse_t_version() -> Option<(u32, u32, u32)> {
    detect_fuse_t_version_from_path("/usr/local/lib")
}

/// Detect the FUSE-T version from a dylib in the given directory.
///
/// Looks for filenames matching `libfuse-t-<major>.<minor>.<patch>.dylib` and
/// returns the first successfully parsed version tuple.  The path parameter
/// enables unit testing with a temp directory.
pub(crate) fn detect_fuse_t_version_from_path(lib_dir: &str) -> Option<(u32, u32, u32)> {
    let dir = fs::read_dir(lib_dir).ok()?;
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Some(version) = parse_libfuse_t_filename(&name_str) {
            return Some(version);
        }
    }
    None
}

/// Parse `libfuse-t-<major>.<minor>.<patch>.dylib` into a version tuple.
fn parse_libfuse_t_filename(name: &str) -> Option<(u32, u32, u32)> {
    let stem = name.strip_prefix("libfuse-t-")?.strip_suffix(".dylib")?;
    let parts: Vec<&str> = stem.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let major = parts[0].parse::<u32>().ok()?;
    let minor = parts[1].parse::<u32>().ok()?;
    let patch = parts[2].parse::<u32>().ok()?;
    Some((major, minor, patch))
}

// ── macOS / FSKit availability ────────────────────────────────────────────────

/// Returns `true` if the running macOS version is 26 or later.
pub(crate) fn check_macos_version_26_or_later() -> bool {
    let output = Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok();
    if let Some(out) = output {
        if out.status.success() {
            let version_str = String::from_utf8_lossy(&out.stdout);
            let major_str = version_str.trim().split('.').next().unwrap_or("0");
            if let Ok(major) = major_str.parse::<u32>() {
                return major >= 26;
            }
        }
    }
    false
}

/// Returns `true` if FSKit backend is available.
///
/// Requires macOS 26+ AND `/Applications/fuse-t.app` present.
pub fn is_fskit_available() -> bool {
    check_macos_version_26_or_later() && Path::new("/Applications/fuse-t.app").exists()
}

// ── TTY detection ─────────────────────────────────────────────────────────────

/// Returns `true` if stdin is an interactive terminal (TTY).
pub fn is_interactive() -> bool {
    // SAFETY: isatty is a simple syscall with no memory-safety implications.
    unsafe { libc::isatty(libc::STDIN_FILENO) != 0 }
}

// ── Backend selection ─────────────────────────────────────────────────────────

/// Minimum FUSE-T version required for SMB backend support.
const MIN_VERSION: (u32, u32, u32) = (1, 0, 35);

/// Select the FUSE-T backend given explicit request, force flag, detected version, and
/// FSKit availability.
///
/// This function is pure (no filesystem access) to allow unit testing.
///
/// # Errors
///
/// - `FUSE-T {version} is too old` — version < 1.0.35
/// - `NFS backend is blocked` — NFS requested without `--force`
/// - `FSKit backend not available` — FSKit requested but unavailable
pub fn select_backend(
    requested: Option<FuseTBackend>,
    force: bool,
    version: (u32, u32, u32),
    fskit_available: bool,
) -> Result<FuseTBackend, String> {
    // Minimum version gate.
    if version < MIN_VERSION {
        return Err(format!(
            "FUSE-T {}.{}.{} is too old. Minimum required: 1.0.35 (for SMB backend support). \
             Please update FUSE-T from https://github.com/macos-fuse-t/fuse-t/releases",
            version.0, version.1, version.2
        ));
    }

    match requested {
        Some(FuseTBackend::Nfs) if !force => {
            Err("NFS backend is blocked by default due to macOS kernel bug (FUSE-T Issue #45) \
                 that deadlocks on simultaneous read+write file descriptors (e.g., cp). \
                 Use --backend=smb (recommended) or --backend=nfs --force to override."
                .to_string())
        }
        Some(FuseTBackend::Fskit) if !fskit_available => Err(
            "FSKit backend not available. FSKit requires macOS 26+ and fuse-t.app installed at \
             /Applications/fuse-t.app."
                .to_string(),
        ),
        Some(backend) => Ok(backend),
        None => {
            // Auto-detect: prefer FSKit > SMB.
            if fskit_available {
                Ok(FuseTBackend::Fskit)
            } else {
                Ok(FuseTBackend::Smb)
            }
        }
    }
}

// ── Fallback confirmation ─────────────────────────────────────────────────────

/// Inner testable version of confirm_fallback that accepts an explicit `is_tty` flag.
pub fn confirm_fallback_with_tty(
    fallback_backend: FuseTBackend,
    reason: &str,
    reader: &mut dyn BufRead,
    is_tty: bool,
) -> Result<FuseTBackend, String> {
    eprintln!(
        "warning: {}. Falling back to {:?} backend.",
        reason, fallback_backend
    );

    if is_tty {
        eprint!("Continue with {:?} backend? [Y/n] ", fallback_backend);

        let mut line = String::new();
        reader.read_line(&mut line).map_err(|e| e.to_string())?;
        let trimmed = line.trim();

        if trimmed == "n" || trimmed == "N" {
            return Err("Mount cancelled by user.".to_string());
        }
    }

    Ok(fallback_backend)
}

/// Prompt the user for confirmation when falling back to a lower-priority backend.
///
/// - Interactive TTY: prompts on stderr, reads from `reader`. "n"/"N" cancels.
/// - Non-interactive: auto-accepts with a stderr warning (already printed above).
///
/// The `reader` parameter allows unit tests to inject mock stdin.
/// Production callers should pass `&mut std::io::BufReader::new(std::io::stdin())`.
pub fn confirm_fallback(
    fallback_backend: FuseTBackend,
    reason: &str,
    reader: &mut dyn BufRead,
) -> Result<FuseTBackend, String> {
    confirm_fallback_with_tty(fallback_backend, reason, reader, is_interactive())
}

// ── Convenience wrapper ───────────────────────────────────────────────────────

/// Auto-detect backend and version, applying fallback confirmation when needed.
///
/// This is the production entry point called from `run_mount()` (wired in Plan 02).
///
/// Returns `(backend, version)` tuple.
pub fn select_backend_auto(
    requested: Option<FuseTBackend>,
    force: bool,
) -> Result<(FuseTBackend, (u32, u32, u32)), String> {
    let version = detect_fuse_t_version()
        .ok_or_else(|| "FUSE-T not found. Install from https://github.com/macos-fuse-t/fuse-t/releases".to_string())?;

    let fskit_available = is_fskit_available();

    let backend = select_backend(requested, force, version, fskit_available)?;

    // If auto-detecting and FSKit was unavailable (we fell back to SMB), prompt.
    if requested.is_none() && !fskit_available {
        let mut reader = std::io::BufReader::new(std::io::stdin());
        let backend = confirm_fallback(
            backend,
            "FSKit backend not available (requires macOS 26+ and fuse-t.app)",
            &mut reader,
        )?;
        return Ok((backend, version));
    }

    Ok((backend, version))
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    // ── detect_fuse_t_version_from_path ──────────────────────────────────────

    fn create_dylib(dir: &TempDir, name: &str) {
        let path = dir.path().join(name);
        std::fs::write(&path, b"").unwrap();
    }

    #[test]
    fn test_detect_version_found() {
        let dir = TempDir::new().unwrap();
        create_dylib(&dir, "libfuse-t-1.0.54.dylib");
        let result = detect_fuse_t_version_from_path(dir.path().to_str().unwrap());
        assert_eq!(result, Some((1, 0, 54)));
    }

    #[test]
    fn test_detect_version_no_dylib_returns_none() {
        let dir = TempDir::new().unwrap();
        let result = detect_fuse_t_version_from_path(dir.path().to_str().unwrap());
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_version_multi_digit() {
        let dir = TempDir::new().unwrap();
        create_dylib(&dir, "libfuse-t-1.12.3.dylib");
        let result = detect_fuse_t_version_from_path(dir.path().to_str().unwrap());
        assert_eq!(result, Some((1, 12, 3)));
    }

    #[test]
    fn test_detect_version_nonexistent_dir_returns_none() {
        let result = detect_fuse_t_version_from_path("/nonexistent/path/that/does/not/exist");
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_version_unrelated_files_ignored() {
        let dir = TempDir::new().unwrap();
        create_dylib(&dir, "libsomethingelse.dylib");
        create_dylib(&dir, "libfuse-t-bad.dylib");
        let result = detect_fuse_t_version_from_path(dir.path().to_str().unwrap());
        assert_eq!(result, None);
    }

    // ── parse_backend_flag ───────────────────────────────────────────────────

    #[test]
    fn test_parse_backend_smb() {
        assert_eq!(parse_backend_flag("smb"), Ok(FuseTBackend::Smb));
    }

    #[test]
    fn test_parse_backend_nfs() {
        assert_eq!(parse_backend_flag("nfs"), Ok(FuseTBackend::Nfs));
    }

    #[test]
    fn test_parse_backend_fskit() {
        assert_eq!(parse_backend_flag("fskit"), Ok(FuseTBackend::Fskit));
    }

    #[test]
    fn test_parse_backend_invalid() {
        assert!(parse_backend_flag("invalid").is_err());
    }

    // ── FuseTBackend::as_mount_option ────────────────────────────────────────

    #[test]
    fn test_as_mount_option_smb() {
        assert_eq!(FuseTBackend::Smb.as_mount_option(), "backend=smb");
    }

    #[test]
    fn test_as_mount_option_fskit() {
        assert_eq!(FuseTBackend::Fskit.as_mount_option(), "backend=fskit");
    }

    #[test]
    fn test_as_mount_option_nfs() {
        assert_eq!(FuseTBackend::Nfs.as_mount_option(), "backend=nfs");
    }

    // ── select_backend ───────────────────────────────────────────────────────

    #[test]
    fn test_select_backend_auto_fskit_unavailable_returns_smb() {
        let result = select_backend(None, false, (1, 0, 54), false);
        assert_eq!(result, Ok(FuseTBackend::Smb));
    }

    #[test]
    fn test_select_backend_auto_fskit_available_returns_fskit() {
        let result = select_backend(None, false, (1, 0, 54), true);
        assert_eq!(result, Ok(FuseTBackend::Fskit));
    }

    #[test]
    fn test_select_backend_nfs_without_force_returns_err() {
        let result = select_backend(Some(FuseTBackend::Nfs), false, (1, 0, 54), false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("NFS backend is blocked"));
    }

    #[test]
    fn test_select_backend_nfs_with_force_returns_ok() {
        let result = select_backend(Some(FuseTBackend::Nfs), true, (1, 0, 54), false);
        assert_eq!(result, Ok(FuseTBackend::Nfs));
    }

    #[test]
    fn test_select_backend_version_too_old_returns_err() {
        let result = select_backend(None, false, (1, 0, 30), false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too old"));
    }

    #[test]
    fn test_select_backend_fskit_requested_unavailable_returns_err() {
        let result = select_backend(Some(FuseTBackend::Fskit), false, (1, 0, 54), false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("FSKit backend not available"));
    }

    #[test]
    fn test_select_backend_fskit_requested_available_returns_ok() {
        let result = select_backend(Some(FuseTBackend::Fskit), false, (1, 0, 54), true);
        assert_eq!(result, Ok(FuseTBackend::Fskit));
    }

    #[test]
    fn test_select_backend_smb_explicit_returns_ok() {
        let result = select_backend(Some(FuseTBackend::Smb), false, (1, 0, 54), false);
        assert_eq!(result, Ok(FuseTBackend::Smb));
    }

    #[test]
    fn test_select_backend_version_exact_minimum_ok() {
        let result = select_backend(None, false, (1, 0, 35), false);
        assert_eq!(result, Ok(FuseTBackend::Smb));
    }

    #[test]
    fn test_select_backend_version_below_minimum_err() {
        let result = select_backend(None, false, (1, 0, 34), false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too old"));
    }

    // ── confirm_fallback_with_tty ────────────────────────────────────────────

    #[test]
    fn test_confirm_fallback_interactive_yes() {
        let input = b"y\n";
        let mut reader = Cursor::new(&input[..]);
        let result = confirm_fallback_with_tty(
            FuseTBackend::Smb,
            "FSKit not available",
            &mut reader,
            true,
        );
        assert_eq!(result, Ok(FuseTBackend::Smb));
    }

    #[test]
    fn test_confirm_fallback_interactive_yes_default_empty() {
        // Empty input (pressing Enter) should accept (default Yes).
        let input = b"\n";
        let mut reader = Cursor::new(&input[..]);
        let result = confirm_fallback_with_tty(
            FuseTBackend::Smb,
            "FSKit not available",
            &mut reader,
            true,
        );
        assert_eq!(result, Ok(FuseTBackend::Smb));
    }

    #[test]
    fn test_confirm_fallback_interactive_no_lowercase() {
        let input = b"n\n";
        let mut reader = Cursor::new(&input[..]);
        let result = confirm_fallback_with_tty(
            FuseTBackend::Smb,
            "FSKit not available",
            &mut reader,
            true,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("cancelled"), "expected 'cancelled' in: {}", err);
    }

    #[test]
    fn test_confirm_fallback_interactive_no_uppercase() {
        let input = b"N\n";
        let mut reader = Cursor::new(&input[..]);
        let result = confirm_fallback_with_tty(
            FuseTBackend::Smb,
            "FSKit not available",
            &mut reader,
            true,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_confirm_fallback_non_interactive() {
        // Non-interactive: should not read from reader, just return Ok.
        let input = b"n\n"; // would cancel if interactive
        let mut reader = Cursor::new(&input[..]);
        let result = confirm_fallback_with_tty(
            FuseTBackend::Smb,
            "FSKit not available",
            &mut reader,
            false, // not a TTY
        );
        assert_eq!(result, Ok(FuseTBackend::Smb));
    }
}
