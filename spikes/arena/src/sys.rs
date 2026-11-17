//! System calls the spike needs, and fault-injection points for crash tests.

use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

/// `fsync`, and on macOS `F_FULLFSYNC` (a plain fsync there does not flush
/// the drive cache). Same as `chunk-store`.
pub fn full_sync(f: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: fcntl F_FULLFSYNC on a valid fd we own.
        if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
            return Ok(());
        }
    }
    f.sync_all()
}

/// `fsync` only (no F_FULLFSYNC). Measured, never relied on.
pub fn plain_fsync(f: &File) -> io::Result<()> {
    // SAFETY: fsync on a valid fd.
    if unsafe { libc::fsync(f.as_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `fdatasync` where it exists (Linux); `fsync` elsewhere.
pub fn data_sync(f: &File) -> io::Result<()> {
    f.sync_data()
}

pub fn sync_dir(dir: &Path) -> io::Result<()> {
    full_sync(&File::open(dir)?)
}

/// Write `data` to `path` atomically and durably.
pub fn write_durable(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let mut f = File::create(&tmp)?;
    f.write_all(data)?;
    full_sync(&f)?;
    fs::rename(&tmp, path)?;
    sync_dir(path.parent().unwrap())
}

/// Reserve disk blocks for `len` bytes of `f` without changing its length:
/// `F_PREALLOCATE` (contiguous first, then any) on macOS, `fallocate` with
/// `KEEP_SIZE` on Linux. Returns the bytes the file system says it allocated.
pub fn preallocate(f: &File, len: u64) -> io::Result<u64> {
    #[cfg(target_os = "macos")]
    {
        let cur = f.metadata()?.blocks() * 512;
        let want = len.saturating_sub(cur);
        if want == 0 {
            return Ok(0);
        }
        for flags in [libc::F_ALLOCATECONTIG | libc::F_ALLOCATEALL, libc::F_ALLOCATEALL] {
            let mut st = libc::fstore_t {
                fst_flags: flags,
                fst_posmode: libc::F_PEOFPOSMODE,
                fst_offset: 0,
                fst_length: want as libc::off_t,
                fst_bytesalloc: 0,
            };
            // SAFETY: valid fd and a valid fstore_t.
            if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_PREALLOCATE, &mut st) } != -1 {
                return Ok(st.fst_bytesalloc as u64);
            }
        }
        Err(io::Error::last_os_error())
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: valid fd.
        let r = unsafe { libc::fallocate(f.as_raw_fd(), libc::FALLOC_FL_KEEP_SIZE, 0, len as libc::off_t) };
        if r == 0 {
            Ok(len)
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (f, len);
        Err(io::Error::other("preallocate not supported"))
    }
}

/// `lseek(fd, from, SEEK_HOLE)`: the start of the first hole at or after
/// `from` (the file length if there is none).
pub fn seek_hole(f: &File, from: u64) -> io::Result<u64> {
    // SAFETY: valid fd.
    let r = unsafe { libc::lseek(f.as_raw_fd(), from as libc::off_t, libc::SEEK_HOLE) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r as u64)
    }
}

/// `lseek(fd, from, SEEK_DATA)`: the start of the first data at or after
/// `from`; `None` past the last data (ENXIO).
pub fn seek_data(f: &File, from: u64) -> io::Result<Option<u64>> {
    // SAFETY: valid fd.
    let r = unsafe { libc::lseek(f.as_raw_fd(), from as libc::off_t, libc::SEEK_DATA) };
    if r < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ENXIO) {
            return Ok(None);
        }
        Err(e)
    } else {
        Ok(Some(r as u64))
    }
}

/// `F_NOCACHE` on macOS (for cold-cache benches).
pub fn set_nocache(f: &File, on: bool) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: valid fd.
        if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_NOCACHE, on as libc::c_int) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    let _ = (f, on);
    Ok(())
}

/// Allocated bytes of a file (`st_blocks * 512`).
pub fn allocated(path: &Path) -> io::Result<u64> {
    Ok(fs::metadata(path)?.blocks() * 512)
}

/// Free bytes for unprivileged users on the volume of `path` (`f_bavail`).
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: zeroed statfs is a valid out buffer; c is NUL-terminated.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.f_bavail as u64 * st.f_bsize as u64)
}

// ── Fault injection ───────────────────────────────────────────────────────────

/// Points where a test can make the next operation fail, to simulate a crash
/// in the middle of a multi-step change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Fault {
    None = 0,
    /// Recovery cut: after `ftruncate(valid)`, before the extend.
    CutAfterTruncDown = 1,
    /// `trim_on_seal`: after `ftruncate(hwm)`, before the idx is written.
    TrimBeforeIdx = 2,
    /// Spike C: during the checkpoint write.
    CheckpointWrite = 3,
    /// Spike C: during the dirty-list rewrite.
    DirtyWrite = 4,
    /// Spike D: after the idx without the dead entries is durable, before
    /// the first punch.
    PunchAfterIdx = 5,
    /// Spike D: after the first punch of an arena, before the rest.
    PunchMidway = 6,
}

thread_local! {
    // Per thread, so parallel tests do not consume each other's faults.
    static FAULT: AtomicU32 = const { AtomicU32::new(0) };
}

/// Arm a fault for this thread: the next `hit(f)` returns an error, once.
pub fn arm(f: Fault) {
    FAULT.with(|x| x.store(f as u32, Ordering::SeqCst));
}

/// At a fault point: fail if this point is armed.
pub fn hit(f: Fault) -> io::Result<()> {
    if FAULT.with(|x| x.compare_exchange(f as u32, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok()) {
        return Err(io::Error::other(format!("injected fault {f:?}")));
    }
    Ok(())
}

// ── Hole punching (spike D) ───────────────────────────────────────────────────

/// Free the disk blocks of `[off, off + len)` without changing the file
/// length; the range then reads as zeros. `off` and `len` must be multiples
/// of the file system block size (`fs_block_size`). macOS: `F_PUNCHHOLE`
/// (APFS); Linux: `fallocate(PUNCH_HOLE | KEEP_SIZE)`. Needs a write fd.
pub fn punch_hole(f: &File, off: u64, len: u64) -> io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: zeroed fpunchhole_t is valid (flags and reserved must be 0).
        let mut a: libc::fpunchhole_t = unsafe { std::mem::zeroed() };
        a.fp_offset = off as libc::off_t;
        a.fp_length = len as libc::off_t;
        // SAFETY: valid fd and a valid fpunchhole_t.
        if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_PUNCHHOLE, &a) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
        // SAFETY: valid fd.
        if unsafe { libc::fallocate(f.as_raw_fd(), mode, off as libc::off_t, len as libc::off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (f, off, len);
        Err(io::Error::other("punch_hole not supported"))
    }
}

/// The file system block size of the volume of `path` (`statfs f_bsize`):
/// the unit of `punch_hole`.
pub fn fs_block_size(path: &Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: zeroed statfs is a valid out buffer; c is NUL-terminated.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.f_bsize as u64)
}

/// The block-aligned part of `[start, end)`: `start` rounded up, `end`
/// rounded down to `block`. `None` if no whole block is inside.
pub fn aligned_inside(start: u64, end: u64, block: u64) -> Option<(u64, u64)> {
    let s = start.div_ceil(block) * block;
    let e = end / block * block;
    (e > s).then_some((s, e - s))
}
