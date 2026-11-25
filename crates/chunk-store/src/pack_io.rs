//! Storage strategies: every file operation of the store goes through
//! [`PackIo`], so the store can run on real files ([`FileIo`]), on an
//! in-memory mock with faults ([`crate::mem::MemIo`]), or later on other
//! read and write paths without changes in the store.
//!
//! Index lookups read sealed run files through a read-only mapping
//! ([`PackIo::map_run`], [`RunMap`]) where the strategy has one (files);
//! other strategies give no mapping, and the lookups read the run pages
//! with `read_at`, so the mock still counts them and can fail them.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::sync::{Condvar, Mutex, Rank};

/// The kinds of file of the store: the two files of one pack, and the
/// files of the store index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// The records.
    Pack,
    /// The index of a sealed pack.
    Idx,
    /// A sorted run of the store index (see `run.rs`).
    Run,
    /// The list of live runs of the store index. There is one; callers
    /// pass the number 0.
    Manifest,
    /// The trust floor of the `.idx` marks (see `locality.rs`): no pack
    /// below it has an exact `.idx`. There is one, in the pack directory;
    /// callers pass the number 0.
    Trust,
}

impl Kind {
    /// True for the files of the store index. They live in their own
    /// directory, next to the pack directory.
    pub fn is_index(self) -> bool {
        matches!(self, Kind::Run | Kind::Manifest)
    }
}

/// File name of a store file.
pub fn pack_name(num: u64, kind: Kind) -> String {
    match kind {
        Kind::Pack => format!("{num:016x}.pack"),
        Kind::Idx => format!("{num:016x}.idx"),
        Kind::Run => format!("{num:016x}.run"),
        Kind::Manifest => "MANIFEST".to_string(),
        Kind::Trust => "TRUST".to_string(),
    }
}

/// Punched ranges must start and end on a multiple of this.
pub const PUNCH_BLOCK: u64 = 4096;

/// Reads of an open pack.
pub trait PackRead: Send + Sync {
    /// Fill `buf` from `offset`; a short file is an error.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
}

/// The active pack: reads and writes.
pub trait PackFile: PackRead {
    fn write_at(&mut self, data: &[u8], offset: u64) -> io::Result<()>;
    fn set_len(&mut self, len: u64) -> io::Result<()>;
    /// Make every write so far durable (F_FULLFSYNC on macOS).
    fn sync(&mut self) -> io::Result<()>;
    /// A handle that syncs this file from another thread while writes go
    /// on (#108). A sync through it makes durable at least every write that
    /// returned before the sync started. `None`: no such handle; the caller
    /// then syncs with `sync`.
    fn sync_handle(&self) -> Option<Arc<dyn PackSync>> {
        None
    }
    /// A handle that writes to this file from another thread, with no
    /// `&mut` access to it (a positional write). `None`: no such handle.
    fn writer(&self) -> Option<Arc<dyn PackWrite>> {
        None
    }
    /// The store published a record that it wrote through `writer` at
    /// `offset` (under its lock, after the write returned).
    fn note_written(&mut self, _offset: u64, _data: &[u8]) {}
    /// Start a sync of every write so far in the background, if this file
    /// can, and return a wait for its end. It only shortens a later `sync`
    /// (which the caller still calls: only that one reports the outcome).
    fn sync_soon(&mut self) -> Option<SyncWait> {
        None
    }
    /// The wait that a `write_at` of `len` more bytes would do now (the end
    /// of a background sync, see `retry.rs`), if any. A caller under a lock
    /// waits for it without the lock first (LOCK-01).
    fn write_wait(&self, _len: usize) -> Option<SyncWait> {
        None
    }
}

/// Syncs one file from any thread (see [`PackFile::sync_handle`]).
pub trait PackSync: Send + Sync {
    fn sync(&self) -> io::Result<()>;
}

/// Writes to one file from any thread (see [`PackFile::writer`]).
pub trait PackWrite: Send + Sync {
    fn write_at(&self, data: &[u8], offset: u64) -> io::Result<()>;
}

/// The end of a background sync (see [`PackFile::sync_soon`]).
#[derive(Clone)]
pub struct SyncWait(Arc<(Mutex<bool>, Condvar)>);

impl Default for SyncWait {
    fn default() -> Self {
        SyncWait(Arc::new((Mutex::new(Rank::Store, false), Condvar::new())))
    }
}

impl SyncWait {
    /// Mark the sync as ended.
    pub fn done(&self) {
        *self.0 .0.lock() = true;
        self.0 .1.notify_all();
    }

    /// True if the sync has ended.
    pub fn is_done(&self) -> bool {
        *self.0 .0.lock()
    }

    /// Wait until the sync has ended.
    pub fn wait(&self) {
        let mut d = self.0 .0.lock();
        while !*d {
            self.0 .1.wait(&mut d);
        }
    }

    /// Wait until the sync has ended, at most `limit`. True if it ended.
    pub fn wait_timeout(&self, limit: Duration) -> bool {
        let until = Instant::now() + limit;
        let mut d = self.0 .0.lock();
        while !*d {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            self.0 .1.wait_for(&mut d, left);
        }
        true
    }
}

/// The directory of pack files, and the files of the store index.
pub trait PackIo: Send + Sync {
    /// Make the directories if needed. With `read_only`, make nothing and
    /// return whether the pack directory exists.
    fn prepare(&self, read_only: bool) -> io::Result<bool>;
    /// Numbers of every `.pack` file.
    fn list(&self) -> io::Result<Vec<u64>>;
    fn exists(&self, num: u64, kind: Kind) -> bool;
    /// The whole file.
    fn read(&self, num: u64, kind: Kind) -> io::Result<Vec<u8>>;
    /// The length of the file, without a read of it (a strategy that has no
    /// cheaper way reads it).
    fn file_len(&self, num: u64, kind: Kind) -> io::Result<u64> {
        self.read(num, kind).map(|b| b.len() as u64)
    }
    /// Replace the file atomically and durably (also syncs the directory).
    fn write_durable(&self, num: u64, kind: Kind, data: &[u8]) -> io::Result<()>;
    fn remove(&self, num: u64, kind: Kind) -> io::Result<()>;
    /// Make creations and removals durable (of packs and of index files).
    fn sync_dir(&self) -> io::Result<()>;
    /// Make a new, empty pack; it must not exist.
    fn create(&self, num: u64) -> io::Result<Box<dyn PackFile>>;
    /// Open a pack for reads and writes.
    fn open_rw(&self, num: u64) -> io::Result<Box<dyn PackFile>>;
    /// Open a pack for reads.
    fn open_read(&self, num: u64) -> io::Result<Arc<dyn PackRead>>;
    /// True if `punch` frees disk blocks here. GC punches only then; else
    /// it keeps dead records in the index, so that compaction still counts
    /// their bytes.
    fn can_punch(&self) -> bool;
    /// Free the disk blocks of `(offset, length)` ranges of a pack; the
    /// ranges read as zeros after this, and the file length does not change.
    /// Each range is aligned to [`PUNCH_BLOCK`]. An error of kind
    /// `Unsupported` means the file system cannot punch holes.
    fn punch(&self, num: u64, ranges: &[(u64, u64)]) -> io::Result<()>;
    /// Numbers of every `.run` file (empty if there is no index directory).
    fn list_runs(&self) -> io::Result<Vec<u64>>;
    /// Make a new, empty run file; it must not exist. It is written like a
    /// pack: `write_at`, then `sync`, then `sync_dir` for its name.
    fn create_run(&self, num: u64) -> io::Result<Box<dyn PackFile>>;
    /// Open a run file for reads.
    fn open_run(&self, num: u64) -> io::Result<Arc<dyn PackRead>>;
    /// Map a sealed run file read-only for lookups, or `None` if this
    /// strategy has no mapping (then lookups use `open_run` and `read_at`).
    /// Required, with no default: a wrapper that forgot to pass it on would
    /// silently put lookups back on `read_at`.
    fn map_run(&self, num: u64) -> io::Result<Option<RunMap>>;
}

/// Runs of at least this many bytes are mapped with `MADV_RANDOM`: a lookup
/// touches one random page, and readahead around a cold page fault of a
/// large run reads far more than that page (the 17-05 mmap spike, Linux,
/// cold page cache: 7.6 times less data read and 1.7 times faster per hit
/// at 585 MB; at 58 MB the default readahead was 2.1 times faster). The
/// limit lies between those two sizes; it is a first value that needs its
/// own measurement.
pub const MADV_RANDOM_MIN: u64 = 256 << 20;

/// A read-only shared mapping (`PROT_READ`, `MAP_SHARED`) of a whole run
/// file. It is unmapped when it is dropped. A run owns its mapping, and
/// readers hold the run through an `Arc`, so a merge or GC that removes the
/// run file never takes the bytes away from a reader: the file system keeps
/// a removed file while a mapping of it exists.
///
/// Accepted risk: a read error of the disk inside the mapping raises
/// SIGBUS, which stops the process, instead of an error value. Run files
/// are sealed (never written or cut after their sync), so only a disk error
/// or a change from outside SliceFS can cause it; scrub, merge and rebuild
/// read runs with `read_at`, where a read error is a normal error.
pub struct RunMap {
    ptr: *const u8,
    len: usize,
}

// SAFETY: the mapping is read-only and is never changed through this
// pointer; reads of shared read-only memory are safe from any thread.
unsafe impl Send for RunMap {}
unsafe impl Sync for RunMap {}

impl RunMap {
    /// The bytes of the file as they were mapped.
    pub fn bytes(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `ptr` is a live mapping of `len` readable bytes until drop.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Map all of `file` read-only.
    fn new(file: &File) -> io::Result<RunMap> {
        use std::os::fd::AsRawFd;
        let len = usize::try_from(file.metadata()?.len()).map_err(|_| io::Error::other("run file too large to map"))?;
        if len == 0 {
            return Ok(RunMap { ptr: std::ptr::null(), len: 0 });
        }
        // SAFETY: a new read-only shared mapping of a valid fd; the result
        // is checked before use.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0) };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        if len as u64 >= MADV_RANDOM_MIN {
            // Only advice: a failure changes nothing that is correct.
            // SAFETY: `p` is the mapping of `len` bytes made above.
            unsafe { libc::madvise(p, len, libc::MADV_RANDOM) };
        }
        Ok(RunMap { ptr: p as *const u8, len })
    }
}

impl Drop for RunMap {
    fn drop(&mut self) {
        if self.len != 0 {
            // SAFETY: the mapping made in `new`, unmapped once.
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
        }
    }
}

// ── Files ─────────────────────────────────────────────────────────────────────

/// Packs as files in a directory, with pread and pwrite. The files of the
/// store index go in the directory `index` next to it (`STORE/packs` and
/// `STORE/index`).
pub struct FileIo {
    dir: PathBuf,
    index: PathBuf,
    /// Result of the hole probe (see `can_punch`).
    punches: OnceLock<bool>,
    /// False: no sync at all (see [`FileIo::unsynced`]).
    sync: bool,
}

impl FileIo {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let index = dir.with_file_name("index");
        FileIo { dir, index, punches: OnceLock::new(), sync: true }
    }

    /// As [`FileIo::new`], but no sync of files or directories: nothing is
    /// durable at a power loss. Only for tests and benchmarks that measure
    /// something else than the disk syncs.
    pub fn unsynced(dir: impl Into<PathBuf>) -> Self {
        FileIo { sync: false, ..Self::new(dir) }
    }

    /// Punch a hole in a small probe file in the pack directory.
    fn probe_punch(&self) -> bool {
        let path = self.dir.join("punch-probe.tmp");
        let ok = (|| -> io::Result<()> {
            let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path)?;
            f.write_all_at(&[1u8; 3 * PUNCH_BLOCK as usize], 0)?;
            punch_hole(&f, PUNCH_BLOCK, PUNCH_BLOCK)
        })()
        .is_ok();
        let _ = fs::remove_file(&path);
        ok
    }

    fn path(&self, num: u64, kind: Kind) -> PathBuf {
        let dir = if kind.is_index() { &self.index } else { &self.dir };
        dir.join(pack_name(num, kind))
    }
}

/// A file, and whether `sync` syncs it.
struct FilePack(File, bool);

impl PackRead for FilePack {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.0.read_exact_at(buf, offset)
    }
}

impl PackFile for FilePack {
    fn write_at(&mut self, data: &[u8], offset: u64) -> io::Result<()> {
        self.0.write_all_at(data, offset)
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }
    fn sync(&mut self) -> io::Result<()> {
        if self.1 {
            full_sync(&self.0)?;
        }
        Ok(())
    }
    fn sync_handle(&self) -> Option<Arc<dyn PackSync>> {
        // A second descriptor of the same open file: its fsync flushes the
        // dirty pages of the file, whoever wrote them.
        let f = self.0.try_clone().ok()?;
        Some(Arc::new(FilePack(f, self.1)))
    }
    fn writer(&self) -> Option<Arc<dyn PackWrite>> {
        let f = self.0.try_clone().ok()?;
        Some(Arc::new(FilePack(f, self.1)))
    }
}

impl PackWrite for FilePack {
    fn write_at(&self, data: &[u8], offset: u64) -> io::Result<()> {
        self.0.write_all_at(data, offset)
    }
}

impl PackSync for FilePack {
    fn sync(&self) -> io::Result<()> {
        if self.1 {
            full_sync(&self.0)?;
        }
        Ok(())
    }
}

impl PackIo for FileIo {
    fn prepare(&self, read_only: bool) -> io::Result<bool> {
        if read_only {
            return Ok(self.dir.is_dir());
        }
        fs::create_dir_all(&self.dir)?;
        fs::create_dir_all(&self.index)?;
        Ok(true)
    }

    fn list(&self) -> io::Result<Vec<u64>> {
        Ok(fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                let hex = n.strip_suffix(".pack")?;
                u64::from_str_radix(hex, 16).ok()
            })
            .collect())
    }

    fn exists(&self, num: u64, kind: Kind) -> bool {
        self.path(num, kind).exists()
    }

    fn read(&self, num: u64, kind: Kind) -> io::Result<Vec<u8>> {
        fs::read(self.path(num, kind))
    }

    fn file_len(&self, num: u64, kind: Kind) -> io::Result<u64> {
        Ok(fs::metadata(self.path(num, kind))?.len())
    }

    fn write_durable(&self, num: u64, kind: Kind, data: &[u8]) -> io::Result<()> {
        write_durable(&self.path(num, kind), data, self.sync)
    }

    fn remove(&self, num: u64, kind: Kind) -> io::Result<()> {
        fs::remove_file(self.path(num, kind))
    }

    fn sync_dir(&self) -> io::Result<()> {
        if !self.sync {
            return Ok(());
        }
        sync_dir(&self.dir)?;
        if self.index.is_dir() {
            sync_dir(&self.index)?;
        }
        Ok(())
    }

    fn create(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        let f = OpenOptions::new().read(true).write(true).create_new(true).open(self.path(num, Kind::Pack))?;
        Ok(Box::new(FilePack(f, self.sync)))
    }

    fn open_rw(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        let f = OpenOptions::new().read(true).write(true).open(self.path(num, Kind::Pack))?;
        Ok(Box::new(FilePack(f, self.sync)))
    }

    fn open_read(&self, num: u64) -> io::Result<Arc<dyn PackRead>> {
        Ok(Arc::new(FilePack(File::open(self.path(num, Kind::Pack))?, self.sync)))
    }

    fn can_punch(&self) -> bool {
        *self.punches.get_or_init(|| self.probe_punch())
    }

    fn punch(&self, num: u64, ranges: &[(u64, u64)]) -> io::Result<()> {
        let f = OpenOptions::new().write(true).open(self.path(num, Kind::Pack))?;
        for &(offset, len) in ranges {
            punch_hole(&f, offset, len)?;
        }
        Ok(())
    }

    fn list_runs(&self) -> io::Result<Vec<u64>> {
        let dir = match fs::read_dir(&self.index) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(dir
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                let hex = n.strip_suffix(".run")?;
                u64::from_str_radix(hex, 16).ok()
            })
            .collect())
    }

    fn create_run(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        let f = OpenOptions::new().read(true).write(true).create_new(true).open(self.path(num, Kind::Run))?;
        Ok(Box::new(FilePack(f, self.sync)))
    }

    fn open_run(&self, num: u64) -> io::Result<Arc<dyn PackRead>> {
        Ok(Arc::new(FilePack(File::open(self.path(num, Kind::Run))?, self.sync)))
    }

    fn map_run(&self, num: u64) -> io::Result<Option<RunMap>> {
        RunMap::new(&File::open(self.path(num, Kind::Run))?).map(Some)
    }
}

fn unsupported_if(e: io::Error) -> io::Error {
    match e.raw_os_error() {
        // A guard, not two patterns: on Linux the two values are equal.
        Some(c) if c == libc::EOPNOTSUPP || c == libc::ENOTSUP => io::Error::new(io::ErrorKind::Unsupported, e),
        _ => e,
    }
}

#[cfg(target_os = "macos")]
fn punch_hole(f: &File, offset: u64, len: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let arg = libc::fpunchhole_t { fp_flags: 0, reserved: 0, fp_offset: offset as libc::off_t, fp_length: len as libc::off_t };
    // SAFETY: fcntl F_PUNCHHOLE on a valid fd with a valid argument.
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_PUNCHHOLE, &arg) } == -1 {
        return Err(unsupported_if(io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn punch_hole(f: &File, offset: u64, len: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
    // SAFETY: fallocate on a valid fd.
    if unsafe { libc::fallocate(f.as_raw_fd(), mode, offset as libc::off_t, len as libc::off_t) } == -1 {
        return Err(unsupported_if(io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn punch_hole(_: &File, _: u64, _: u64) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// `fsync`, and on macOS `F_FULLFSYNC` (a plain fsync there does not flush
/// the drive cache).
fn full_sync(f: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: fcntl F_FULLFSYNC on a valid fd we own.
        if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
            return Ok(());
        }
    }
    f.sync_all()
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    full_sync(&File::open(dir)?)
}

/// Write `data` to `path` atomically and durably.
fn write_durable(path: &Path, data: &[u8], sync: bool) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(data)?;
    if sync {
        full_sync(&f)?;
    }
    fs::rename(&tmp, path)?;
    if sync {
        sync_dir(path.parent().unwrap())?;
    }
    Ok(())
}
