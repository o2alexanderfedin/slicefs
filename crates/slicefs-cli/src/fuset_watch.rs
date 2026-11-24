//! End the FUSE-T session when the mount goes away (macOS).
//!
//! FUSE-T serves the mount from a `go-nfsv4` child process. With the NFS
//! backend (now removed), that child exited when the volume was unmounted,
//! the FUSE channel reached EOF, and the session ended (destroy, final
//! commit). With the SMB backend (now removed) the child kept running after
//! the unmount: the session never ended and the daemon never exited. The
//! FSKit server may also not exit, so the watcher stays.
//!
//! This watcher polls the kernel mount table (`getfsstat`, which never
//! touches the FUSE mount, so it cannot hang on it). When the mount it saw is
//! gone, it ends its own `go-nfsv4` children with SIGKILL (they ignore
//! SIGTERM). The channel then reaches EOF and the session ends normally.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// True if a file system is mounted at exactly `path` (from the kernel mount
/// table; the mount itself is not accessed).
pub fn is_mounted(path: &Path) -> bool {
    let want = path.as_os_str().as_encoded_bytes();
    // SAFETY: getfsstat with a null buffer returns the count; then it fills
    // at most `n` entries of the buffer we give it.
    unsafe {
        let n = libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT);
        if n <= 0 {
            return false;
        }
        let mut buf: Vec<libc::statfs> = vec![std::mem::zeroed(); n as usize + 8];
        let size = (buf.len() * std::mem::size_of::<libc::statfs>()) as libc::c_int;
        let got = libc::getfsstat(buf.as_mut_ptr(), size, libc::MNT_NOWAIT);
        if got <= 0 {
            return false;
        }
        buf.truncate(got as usize);
        buf.iter().any(|s| {
            let name = std::ffi::CStr::from_ptr(s.f_mntonname.as_ptr());
            name.to_bytes() == want
        })
    }
}

/// PIDs of this process's `go-nfsv4` children.
fn go_nfsv4_children() -> Vec<i32> {
    let out = std::process::Command::new("pgrep")
        .args(["-P", &std::process::id().to_string(), "go-nfsv4"])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .filter_map(|p| p.parse().ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Watch `mountpoint`; see the module docs. Prints `on_mounted` when the mount
/// first appears. Ends when `shutdown` is set or after it ended the children
/// once. Returns true if the mount appeared at all: a FUSE-T session can start
/// and end without a volume (for example a backend that cannot mount).
pub fn spawn_unmount_watcher(
    mountpoint: &Path,
    on_mounted: String,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<bool> {
    // The kernel mount table holds the resolved path (/tmp -> /private/tmp).
    let path: PathBuf = std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_path_buf());
    std::thread::Builder::new()
        .name("slicefs-unmount-watch".into())
        .spawn(move || {
            let mut seen = false;
            loop {
                let stop = shutdown.load(Ordering::SeqCst);
                let mounted = is_mounted(&path);
                if mounted && !seen {
                    println!("{on_mounted}");
                }
                seen |= mounted;
                if stop {
                    return seen;
                }
                if mounted {
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
                if !seen {
                    std::thread::sleep(Duration::from_millis(100));
                    continue; // not mounted yet
                }
                for pid in go_nfsv4_children() {
                    eprintln!("[unmount-watch] {} is unmounted; ending FUSE-T server pid {pid}", path.display());
                    // SAFETY: plain kill(2) on our own child.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
                return true;
            }
        })
        .expect("spawn unmount watcher")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_is_mounted_and_a_plain_directory_is_not() {
        assert!(is_mounted(Path::new("/")));
        let dir = tempfile::tempdir().unwrap();
        let p = std::fs::canonicalize(dir.path()).unwrap();
        assert!(!is_mounted(&p));
    }
}
