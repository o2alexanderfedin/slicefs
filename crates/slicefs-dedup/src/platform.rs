use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

/// Platform-correct full sync per ARCHITECTURE §3 I10.
/// macOS: F_FULLFSYNC (plain fsync is a no-op on Apple SSDs).
/// Linux: fdatasync.
/// Other Unix: best-effort fsync.
#[allow(dead_code)] // Consumed by manifest.rs (D3) and bloom_snapshot.rs (D4).
pub fn durable_sync(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    #[cfg(target_os = "macos")]
    {
        let r = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
        if r == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let r = unsafe { libc::fdatasync(fd) };
        if r == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = fd;
        file.sync_data()
    }
}

/// Open a directory and full-sync it. Used after rename(2) and after
/// creating files to make the directory entry durable.
#[allow(dead_code)] // Consumed by manifest.rs (D3) and bloom_snapshot.rs (D4).
pub fn fsync_parent_dir(dir: &std::path::Path) -> io::Result<()> {
    let f = File::open(dir)?;
    durable_sync(&f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_sync_succeeds_on_fresh_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x");
        let f = File::create(&path).unwrap();
        durable_sync(&f).unwrap();
    }

    #[test]
    fn fsync_parent_dir_on_tempdir_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        fsync_parent_dir(dir.path()).unwrap();
    }
}
