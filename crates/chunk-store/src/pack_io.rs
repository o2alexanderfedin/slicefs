//! Storage strategies: every file operation of the store goes through
//! [`PackIo`], so the store can run on real files ([`FileIo`]), on an
//! in-memory mock with faults ([`crate::mem::MemIo`]), or later on other
//! read and write paths (for example mmap) without changes in the store.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// The two kinds of file of one pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// The records.
    Pack,
    /// The index of a sealed pack.
    Idx,
}

impl Kind {
    fn ext(self) -> &'static str {
        match self {
            Kind::Pack => "pack",
            Kind::Idx => "idx",
        }
    }
}

/// File name of a pack file.
pub fn pack_name(num: u64, kind: Kind) -> String {
    format!("{num:016x}.{}", kind.ext())
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
}

/// The directory of pack files.
pub trait PackIo: Send + Sync {
    /// Make the directory if needed. With `read_only`, make nothing and
    /// return whether the directory exists.
    fn prepare(&self, read_only: bool) -> io::Result<bool>;
    /// Numbers of every `.pack` file.
    fn list(&self) -> io::Result<Vec<u64>>;
    fn exists(&self, num: u64, kind: Kind) -> bool;
    /// The whole file.
    fn read(&self, num: u64, kind: Kind) -> io::Result<Vec<u8>>;
    /// Replace the file atomically and durably (also syncs the directory).
    fn write_durable(&self, num: u64, kind: Kind, data: &[u8]) -> io::Result<()>;
    fn remove(&self, num: u64, kind: Kind) -> io::Result<()>;
    /// Make creations and removals durable.
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
}

// ── Files ─────────────────────────────────────────────────────────────────────

/// Packs as files in a directory, with pread and pwrite.
pub struct FileIo {
    dir: PathBuf,
    /// Result of the hole probe (see `can_punch`).
    punches: OnceLock<bool>,
}

impl FileIo {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        FileIo { dir: dir.into(), punches: OnceLock::new() }
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
        self.dir.join(pack_name(num, kind))
    }
}

struct FilePack(File);

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
        full_sync(&self.0)
    }
}

impl PackIo for FileIo {
    fn prepare(&self, read_only: bool) -> io::Result<bool> {
        if read_only {
            return Ok(self.dir.is_dir());
        }
        fs::create_dir_all(&self.dir)?;
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

    fn write_durable(&self, num: u64, kind: Kind, data: &[u8]) -> io::Result<()> {
        write_durable(&self.path(num, kind), data)
    }

    fn remove(&self, num: u64, kind: Kind) -> io::Result<()> {
        fs::remove_file(self.path(num, kind))
    }

    fn sync_dir(&self) -> io::Result<()> {
        sync_dir(&self.dir)
    }

    fn create(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        let f = OpenOptions::new().read(true).write(true).create_new(true).open(self.path(num, Kind::Pack))?;
        Ok(Box::new(FilePack(f)))
    }

    fn open_rw(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        let f = OpenOptions::new().read(true).write(true).open(self.path(num, Kind::Pack))?;
        Ok(Box::new(FilePack(f)))
    }

    fn open_read(&self, num: u64) -> io::Result<Arc<dyn PackRead>> {
        Ok(Arc::new(FilePack(File::open(self.path(num, Kind::Pack))?)))
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
}

fn unsupported_if(e: io::Error) -> io::Error {
    match e.raw_os_error() {
        Some(libc::EOPNOTSUPP) | Some(libc::ENOTSUP) => io::Error::new(io::ErrorKind::Unsupported, e),
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
fn write_durable(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(data)?;
    full_sync(&f)?;
    fs::rename(&tmp, path)?;
    sync_dir(path.parent().unwrap())
}
