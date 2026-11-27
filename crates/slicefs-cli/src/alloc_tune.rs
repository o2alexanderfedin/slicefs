//! Allocator settings and numbers (Phase 21 [Allocator]).
//!
//! The only place with calls into the C allocator. With glibc, each writer
//! thread gets its own malloc arena, and freed memory stays in that arena:
//! the RSS grows with the writer count, not with the live data (phase 21
//! `runs/s7-memory-diagnosis-NOTES.md`). A fixed mmap threshold of 128 KiB
//! gives large blocks their own mmap, so a free gives them back at once.

/// The mmap threshold that `tune_at_start` sets on Linux glibc, in bytes.
pub const MMAP_THRESHOLD: usize = 128 * 1024;

/// Allocator numbers of the whole process, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllocStats {
    /// Bytes in live blocks (glibc: `uordblks + hblkhd`; macOS: in use).
    pub live_bytes: u64,
    /// Free bytes that the allocator keeps (glibc: `fordblks`, free in the
    /// arenas; macOS: allocated minus in use). Not the RSS minus the live heap.
    pub free_bytes: u64,
    /// Bytes in blocks with a mmap of their own (glibc: `hblkhd`; 0 on macOS).
    pub mmap_bytes: u64,
}

/// Set the allocator for the daemon. Call it once at start, before other
/// threads start. On Linux glibc: `mallopt(M_MMAP_THRESHOLD, 128 KiB)`. A
/// fixed threshold also stops glibc from raising it (and the trim
/// threshold) after a free of a mmapped block. Returns the threshold set,
/// or `None` when nothing was set (other systems, or `mallopt` refused).
pub fn tune_at_start() -> Option<usize> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: mallopt takes two integers; glibc owns the state.
        let ok = unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, MMAP_THRESHOLD as libc::c_int) };
        (ok == 1).then_some(MMAP_THRESHOLD)
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        None
    }
}

/// The allocator numbers now (`None` where the allocator does not give them).
pub fn stats() -> Option<AllocStats> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: mallinfo2 takes no pointers; glibc owns the state.
        let mi = unsafe { libc::mallinfo2() };
        Some(AllocStats {
            live_bytes: (mi.uordblks + mi.hblkhd) as u64,
            free_bytes: mi.fordblks as u64,
            mmap_bytes: mi.hblkhd as u64,
        })
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: a null zone means all zones; `s` is a valid out pointer.
        let mut s: libc::malloc_statistics_t = unsafe { std::mem::zeroed() };
        unsafe { libc::malloc_zone_statistics(std::ptr::null_mut(), &mut s) };
        Some(AllocStats {
            live_bytes: s.size_in_use as u64,
            free_bytes: s.size_allocated.saturating_sub(s.size_in_use) as u64,
            mmap_bytes: 0,
        })
    }
    #[cfg(not(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")))]
    {
        None
    }
}
