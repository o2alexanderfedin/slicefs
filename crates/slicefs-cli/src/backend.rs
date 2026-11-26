//! FUSE-T backend detection and selection for SliceFS.
//!
//! SliceFS mounts through FUSE-T with the FSKit backend only (macOS 26+,
//! FUSE-T 1.2+, the FUSE-T file system extension enabled in System Settings).
//!
//! The other FUSE-T backends were removed:
//! - NFS: the macOS NFS client sends an fsync on every file close; with the
//!   durable write log (#20) each fsync is a disk flush.
//! - SMB: on macOS 26.7 the SMB client's `close()` waits about 41 ms after a
//!   file was read through the page cache, so reading many small files (diff,
//!   builds, search) is about 5x slower than on FSKit.
//!
//! `--force` is accepted but currently unused.

use std::fs;
use std::path::Path;
use std::process::Command;

// ── Backend enum ──────────────────────────────────────────────────────────────

/// FUSE-T mount backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuseTBackend {
    /// FSKit backend — macOS 26+ only, requires fuse-t.app 1.2.0+.
    Fskit,
}

impl FuseTBackend {
    /// Returns the FUSE-T mount option string for this backend.
    ///
    /// Pass the result as `-o <value>` to FUSE-T at mount time.
    pub fn as_mount_option(&self) -> &'static str {
        match self {
            FuseTBackend::Fskit => "backend=fskit",
        }
    }
}

/// How to enable the FUSE-T file system extension (shown when FSKit fails).
pub const ENABLE_FSKIT_HINT: &str = "enable FUSE-T in System Settings > General > \
    Login Items & Extensions > File System Extensions";

// ── CLI flag parsing ──────────────────────────────────────────────────────────

/// Parse a `--backend` CLI flag value into a [`FuseTBackend`].
///
/// Accepts `"fskit"` (case-sensitive), the only backend.
pub fn parse_backend_flag(s: &str) -> Result<FuseTBackend, String> {
    match s {
        "fskit" => Ok(FuseTBackend::Fskit),
        "nfs" | "smb" => Err(format!(
            "the {s} backend was removed; SliceFS uses fskit (the default and only backend)"
        )),
        other => Err(format!("unknown backend {other:?}. The only backend is fskit")),
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
    if let Some(out) = output
        && out.status.success()
    {
        let version_str = String::from_utf8_lossy(&out.stdout);
        let major_str = version_str.trim().split('.').next().unwrap_or("0");
        if let Ok(major) = major_str.parse::<u32>() {
            return major >= 26;
        }
    }
    false
}

/// Returns `true` if FSKit backend is available.
///
/// Requires macOS 26+ AND `/Applications/fuse-t.app` present. (Whether the
/// extension is enabled cannot be seen here; the mount reports it.)
pub fn is_fskit_available() -> bool {
    check_macos_version_26_or_later() && Path::new("/Applications/fuse-t.app").exists()
}

// ── Backend selection ─────────────────────────────────────────────────────────

/// Minimum FUSE-T version: the first with the FSKit backend.
const MIN_VERSION: (u32, u32, u32) = (1, 2, 0);

/// Select the FUSE-T backend given explicit request, force flag, detected version, and
/// FSKit availability.
///
/// This function is pure (no filesystem access) to allow unit testing.
///
/// # Errors
///
/// - `FUSE-T {version} is too old` — version < 1.2.0
/// - `FSKit backend not available` — not macOS 26+ or no fuse-t.app
pub fn select_backend(
    requested: Option<FuseTBackend>,
    _force: bool,
    version: (u32, u32, u32),
    fskit_available: bool,
) -> Result<FuseTBackend, String> {
    if version < MIN_VERSION {
        return Err(format!(
            "FUSE-T {}.{}.{} is too old. Minimum required: 1.2.0 (for the FSKit backend). \
             Please update FUSE-T from https://github.com/macos-fuse-t/fuse-t/releases",
            version.0, version.1, version.2
        ));
    }
    if !fskit_available {
        return Err(format!(
            "FSKit backend not available. It requires macOS 26+ and fuse-t.app installed at \
             /Applications/fuse-t.app; then {ENABLE_FSKIT_HINT}."
        ));
    }
    Ok(requested.unwrap_or(FuseTBackend::Fskit))
}

// ── Convenience wrapper ───────────────────────────────────────────────────────

/// Detect the FUSE-T version and select the backend.
///
/// This is the production entry point called from `run_mount()`.
///
/// Returns `(backend, version)` tuple.
pub fn select_backend_auto(
    requested: Option<FuseTBackend>,
    force: bool,
) -> Result<(FuseTBackend, (u32, u32, u32)), String> {
    let version = detect_fuse_t_version()
        .ok_or_else(|| "FUSE-T not found. Install from https://github.com/macos-fuse-t/fuse-t/releases".to_string())?;
    let backend = select_backend(requested, force, version, is_fskit_available())?;
    Ok((backend, version))
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn test_detect_version_with_multiple_dylibs_returns_first_found() {
        let dir = TempDir::new().unwrap();
        create_dylib(&dir, "libfuse-t-1.0.40.dylib");
        create_dylib(&dir, "libfuse-t-1.0.54.dylib");
        let (major, minor, _) = detect_fuse_t_version_from_path(dir.path().to_str().unwrap()).unwrap();
        assert_eq!((major, minor), (1, 0));
    }

    #[test]
    fn test_detect_version_ignores_partial_match() {
        let dir = TempDir::new().unwrap();
        create_dylib(&dir, "libfuse-t-1.0.54.so");
        create_dylib(&dir, "libfuse-t-1.0.54");
        assert_eq!(detect_fuse_t_version_from_path(dir.path().to_str().unwrap()), None);
    }

    #[test]
    fn test_detect_version_zero_components() {
        let dir = TempDir::new().unwrap();
        create_dylib(&dir, "libfuse-t-0.0.0.dylib");
        assert_eq!(detect_fuse_t_version_from_path(dir.path().to_str().unwrap()), Some((0, 0, 0)));
    }

    // ── parse_backend_flag ───────────────────────────────────────────────────

    #[test]
    fn test_parse_backend_fskit() {
        assert_eq!(parse_backend_flag("fskit"), Ok(FuseTBackend::Fskit));
    }

    #[test]
    fn test_parse_backend_smb_and_nfs_are_removed() {
        for b in ["smb", "nfs"] {
            let err = parse_backend_flag(b).unwrap_err();
            assert!(err.contains("removed") && err.contains("fskit"), "{err}");
        }
    }

    #[test]
    fn test_parse_backend_invalid() {
        for b in ["invalid", "", "FSKIT", "SMB"] {
            assert!(parse_backend_flag(b).is_err(), "{b:?} must be invalid");
        }
        assert!(parse_backend_flag("x").unwrap_err().contains("fskit"));
    }

    // ── as_mount_option ──────────────────────────────────────────────────────

    #[test]
    fn test_as_mount_option_fskit() {
        assert_eq!(FuseTBackend::Fskit.as_mount_option(), "backend=fskit");
    }

    // ── select_backend ───────────────────────────────────────────────────────

    #[test]
    fn test_select_backend_default_is_fskit() {
        assert_eq!(select_backend(None, false, (1, 2, 9), true), Ok(FuseTBackend::Fskit));
        assert_eq!(select_backend(Some(FuseTBackend::Fskit), false, (1, 2, 0), true), Ok(FuseTBackend::Fskit));
    }

    #[test]
    fn test_select_backend_without_fskit_is_an_error_with_a_hint() {
        let err = select_backend(None, false, (1, 2, 9), false).unwrap_err();
        assert!(err.contains("FSKit backend not available"), "{err}");
        assert!(err.contains("File System Extensions"), "{err}");
    }

    #[test]
    fn test_select_backend_needs_fuse_t_1_2() {
        for v in [(1, 1, 99), (1, 0, 54), (0, 9, 99)] {
            let err = select_backend(None, false, v, true).unwrap_err();
            assert!(err.contains("too old") && err.contains("1.2.0"), "{err}");
        }
        assert!(select_backend(None, false, (2, 0, 0), true).is_ok());
    }
}
