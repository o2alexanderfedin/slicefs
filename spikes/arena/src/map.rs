//! Spike B: the mmap read path. All `unsafe` of spike B is in this file.
//!
//! An `ArenaMap` maps one arena file read-only and shared (`PROT_READ`,
//! `MAP_SHARED`) at its full fixed length; the fd is closed right after
//! `mmap`. Writes still go through `pwrite` on the store's own fd, and on a
//! unified buffer cache (macOS, Linux) they are visible through the map.
//! No writes through the map, no `msync`, no `MAP_PRIVATE`, no SIGBUS
//! handler: the store never shortens a file that may be mapped (the recovery
//! cut runs in `open` before any map exists; `trim_on_seal` is refused with
//! mmap; `read_only` stores use pread).
//!
//! `MmapPath` maps an arena lazily on its first read, and only arenas the
//! store announced as `fixed` (full fixed length). Every other arena (V0
//! Grow, an arena cut shorter, an arena whose `fstat` length is not the
//! announced one, or a failed `mmap`) is read with the inner `PreadPath`.

use crate::arena::arena_path;
use crate::codec::rec_len;
use crate::read::{PreadPath, ReadPath};
use crate::{Advice, Loc};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct ArenaMap {
    ptr: *const u8,
    len: usize,
}

// SAFETY: the mapping is read-only and owned by this value; it is unmapped
// only in Drop, when no reference to its bytes can remain.
unsafe impl Send for ArenaMap {}
unsafe impl Sync for ArenaMap {}

impl ArenaMap {
    /// Map `path` read-only. Refused if its length is not `expected_len`.
    pub fn open(path: &Path, expected_len: u64, advice: Advice) -> io::Result<ArenaMap> {
        let f = File::open(path)?;
        let st = f.metadata()?;
        if st.len() != expected_len {
            return Err(io::Error::other(format!("arena length {} is not {expected_len}", st.len())));
        }
        if expected_len == 0 || expected_len > isize::MAX as u64 {
            return Err(io::Error::other("bad arena length for mmap"));
        }
        let len = expected_len as usize;
        // SAFETY: valid fd; a fresh mapping chosen by the kernel.
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, f.as_raw_fd(), 0)
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let m = ArenaMap { ptr: p as *const u8, len };
        m.advise(advice);
        Ok(m) // `f` is closed here; the mapping stays.
    }

    pub fn advise(&self, advice: Advice) {
        let a = match advice {
            Advice::Default => libc::MADV_NORMAL,
            Advice::Random => libc::MADV_RANDOM,
        };
        // SAFETY: the range is our mapping. A failed hint is harmless.
        unsafe { libc::madvise(self.ptr as *mut libc::c_void, self.len, a) };
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `n` bytes at `off`, or `None` if the range is not inside the map.
    pub fn bytes(&self, off: u64, n: usize) -> Option<&[u8]> {
        let end = off.checked_add(n as u64)?;
        if end > self.len as u64 {
            return None;
        }
        // SAFETY: in bounds of a live read-only mapping that outlives &self.
        Some(unsafe { std::slice::from_raw_parts(self.ptr.add(off as usize), n) })
    }

    /// One byte, read with a volatile load (the SIGBUS test).
    pub fn read_byte(&self, off: u64) -> Option<u8> {
        let b = self.bytes(off, 1)?;
        // SAFETY: in bounds of the mapping.
        Some(unsafe { std::ptr::read_volatile(b.as_ptr()) })
    }

    /// Fraction of the mapped pages that are resident (`mincore`).
    pub fn resident(&self) -> f64 {
        let page = page_size();
        let pages = self.len.div_ceil(page);
        let mut v = vec![0 as libc::c_char; pages];
        // SAFETY: our mapping; `v` has one byte per page.
        let r = unsafe { libc::mincore(self.ptr as *mut libc::c_void, self.len, v.as_mut_ptr() as *mut _) };
        if r != 0 {
            return f64::NAN;
        }
        v.iter().filter(|&&b| b & 1 != 0).count() as f64 / pages as f64
    }

    /// Read one byte of every page (prefault), returns a checksum so the
    /// reads are not optimised away.
    pub fn touch_all(&self) -> u64 {
        let page = page_size();
        let mut s = 0u64;
        let mut o = 0;
        while o < self.len {
            // SAFETY: in bounds.
            s = s.wrapping_add(unsafe { std::ptr::read_volatile(self.ptr.add(o)) } as u64);
            o += page;
        }
        s
    }
}

impl Drop for ArenaMap {
    fn drop(&mut self) {
        // SAFETY: our mapping, unmapped once.
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

pub fn page_size() -> usize {
    // SAFETY: sysconf has no preconditions.
    (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }) as usize
}

/// Fraction of the pages of `path` that are resident, through a temporary
/// map (for the cold-cache check).
pub fn resident_fraction(path: &Path) -> io::Result<f64> {
    let len = std::fs::metadata(path)?.len();
    Ok(ArenaMap::open(path, len, Advice::Default)?.resident())
}

/// (resident size, phys_footprint) of this process, bytes (B-B6).
pub fn footprint() -> (u64, u64) {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: zeroed rusage_info_v2 is a valid out buffer of the flavor.
        let mut ri: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::proc_pid_rusage(
                libc::getpid(),
                libc::RUSAGE_INFO_V2,
                &mut ri as *mut libc::rusage_info_v2 as *mut libc::rusage_info_t,
            )
        };
        if r == 0 {
            return (ri.ri_resident_size, ri.ri_phys_footprint);
        }
    }
    (0, 0)
}

/// Set RLIMIT_NOFILE (soft) to `n` (B-B7). True on success.
pub fn set_nofile(n: u64) -> bool {
    let mut rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: plain syscalls with valid structs.
    unsafe {
        libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl);
        rl.rlim_cur = n;
        libc::setrlimit(libc::RLIMIT_NOFILE, &rl) == 0
    }
}

/// Number of open fds below `max` (`fcntl(F_GETFD)` on each; works at
/// EMFILE, unlike listing /dev/fd).
pub fn open_fds(max: i32) -> usize {
    // SAFETY: F_GETFD on any number is harmless.
    (0..max).filter(|&fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1).count()
}

/// The mmap read path (spike B).
pub struct MmapPath {
    dir: PathBuf,
    advice: Advice,
    /// Arena -> (announced length, fixed).
    lens: Mutex<HashMap<u64, (u64, bool)>>,
    maps: Mutex<HashMap<u64, Arc<ArenaMap>>>,
    pread: PreadPath,
}

impl MmapPath {
    pub fn new(dir: &Path, advice: Advice) -> Self {
        MmapPath {
            dir: dir.to_owned(),
            advice,
            lens: Mutex::new(HashMap::new()),
            maps: Mutex::new(HashMap::new()),
            pread: PreadPath::new(dir),
        }
    }

    /// The map of `arena`, made on first use. `None`: read it with pread.
    pub fn map(&self, arena: u64) -> Option<Arc<ArenaMap>> {
        if let Some(m) = self.maps.lock().unwrap().get(&arena) {
            return Some(Arc::clone(m));
        }
        let (len, fixed) = *self.lens.lock().unwrap().get(&arena)?;
        if !fixed {
            return None;
        }
        let m = Arc::new(ArenaMap::open(&arena_path(&self.dir, arena), len, self.advice).ok()?);
        let mut maps = self.maps.lock().unwrap();
        // Another reader may have mapped it meanwhile; keep the first one.
        Some(Arc::clone(maps.entry(arena).or_insert(m)))
    }

    /// Number of arenas mapped now.
    pub fn mapped(&self) -> usize {
        self.maps.lock().unwrap().len()
    }

    /// Drop every map (B-B6).
    pub fn unmap_all(&self) {
        self.maps.lock().unwrap().clear();
    }
}

impl ReadPath for MmapPath {
    fn read_record(&self, loc: &Loc, f: &mut dyn FnMut(&[u8])) -> bool {
        match self.map(loc.arena) {
            Some(m) => match m.bytes(loc.off, rec_len(loc.stored)) {
                Some(b) => {
                    f(b);
                    true
                }
                None => false,
            },
            None => self.pread.read_record(loc, f),
        }
    }

    fn arena_ready(&self, arena: u64, len: u64, fixed: bool) {
        let old = self.lens.lock().unwrap().insert(arena, (len, fixed));
        if old != Some((len, fixed)) {
            // A map of another length must not be used again.
            self.maps.lock().unwrap().remove(&arena);
        }
        self.pread.arena_ready(arena, len, fixed);
    }

    fn forget(&self, arena: u64) {
        self.maps.lock().unwrap().remove(&arena);
        self.lens.lock().unwrap().remove(&arena);
        self.pread.forget(arena);
    }

    fn name(&self) -> &'static str {
        "mmap"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
