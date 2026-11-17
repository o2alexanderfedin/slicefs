//! Spike E: three write paths for the active arena, at the same level (one
//! writer thread, same hash, same dedup index, same record v2, granule 1).
//!
//! - `Alloc`: today's path of `chunk-store` and `SpikeStore::put`:
//!   `zstd::bulk::compress` (a new zstd context and a new output `Vec` per
//!   put), `codec::encode_record` (a second `Vec`), one `pwrite`.
//! - `PwBuf`: the best `pwrite` path. One `zstd::bulk::Compressor` and one
//!   record buffer per writer. zstd writes into the buffer at `REC_HEADER`;
//!   the header and the CRC go in place; one `pwrite`. No allocation per put.
//! - `Mmap`: the same `Compressor`, but its destination is the shared map of
//!   the arena at the record offset. Header and CRC are written in place.
//!   `sync` = `msync(MS_SYNC)` of the dirty pages, then `F_FULLFSYNC`.
//!
//! The zstd destination is capped at `raw - raw/8`. zstd returns an error if
//! the output does not fit, so "kept only if 1/8 smaller" needs no bound
//! check; on that error the raw bytes are copied over the partial output.
//! The record offset must be known before compression, so a writer reserves
//! the worst case (`rec_len(raw)`) before it compresses.

use crate::arena::Arena;
use crate::codec::{self, Decoded, FLAG_ZSTD, MAX_STORED, REC_CRC, REC_HEADER};
use crate::arena::Recovery;
use crate::{sys, ArenaCfg, Loc, ObjectId, Walk};
use std::collections::HashMap;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Objects smaller than this are not compressed (as `chunk-store`).
const MIN_COMPRESS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Alloc,
    PwBuf,
    Mmap,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        Some(match s {
            "alloc" => Mode::Alloc,
            "pwbuf" => Mode::PwBuf,
            "mmap" => Mode::Mmap,
            _ => return None,
        })
    }
}

/// A shared, writable map of a whole arena file.
pub struct Map {
    ptr: *mut u8,
    len: usize,
}

impl Map {
    pub fn new(f: &std::fs::File, len: u64) -> io::Result<Map> {
        // SAFETY: a new mapping of a valid fd; the kernel picks the address.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                f.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Map { ptr: p as *mut u8, len: len as usize })
    }

    /// The bytes `[off, off+n)` of the map.
    ///
    /// # Panics
    /// If the range is outside the map.
    #[allow(clippy::mut_from_ref)]
    pub fn slice(&self, off: u64, n: usize) -> &mut [u8] {
        assert!(off as usize + n <= self.len, "range outside the map");
        // SAFETY: inside the mapping; one writer owns the range past hwm.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.add(off as usize), n) }
    }

    /// `msync(MS_SYNC)` of `[lo, hi)`, with `lo` floored to a page.
    pub fn msync(&self, lo: u64, hi: u64) -> io::Result<()> {
        let pg = page_size();
        let lo = lo / pg * pg;
        let hi = hi.min(self.len as u64);
        if hi <= lo {
            return Ok(());
        }
        // SAFETY: a page-aligned range inside the mapping.
        if unsafe { libc::msync(self.ptr.add(lo as usize) as *mut _, (hi - lo) as usize, libc::MS_SYNC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: our own mapping.
        unsafe { libc::munmap(self.ptr as *mut _, self.len) };
    }
}

pub fn page_size() -> u64 {
    // SAFETY: plain sysconf.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 }
}

/// Time spent in the parts of `sync` and in arena changes (seconds).
#[derive(Clone, Debug, Default)]
pub struct Times {
    pub msync: Vec<f64>,
    pub fullsync: Vec<f64>,
    /// `Arena::create` + map, per new arena.
    pub new_arena: Vec<f64>,
    /// `munmap` of a full arena.
    pub unmap: Vec<f64>,
}

/// Encode a record into `dst` (at least `rec_len(raw)` bytes): header,
/// payload (zstd into `dst` if 1/8 smaller, else the raw bytes), CRC.
/// Returns the record length.
fn encode_into(cctx: Option<&mut zstd::bulk::Compressor<'static>>, id: &ObjectId, data: &[u8], dst: &mut [u8]) -> usize {
    let raw = data.len();
    let cap = raw - raw / 8;
    let comp = match cctx {
        Some(c) if raw >= MIN_COMPRESS => c.compress_to_buffer(data, &mut dst[REC_HEADER..REC_HEADER + cap]).ok(),
        _ => None,
    };
    let (flags, stored) = match comp {
        Some(n) => (FLAG_ZSTD, n),
        None => {
            dst[REC_HEADER..REC_HEADER + raw].copy_from_slice(data);
            (0u8, raw)
        }
    };
    dst[..4].copy_from_slice(&codec::MARKER.to_le_bytes());
    dst[4..32].copy_from_slice(&id.0);
    dst[32] = flags;
    dst[33..37].copy_from_slice(&(raw as u32).to_le_bytes());
    dst[37..41].copy_from_slice(&(stored as u32).to_le_bytes());
    let body = REC_HEADER + stored;
    let crc = crc32c::crc32c(&dst[..body]);
    dst[body..body + REC_CRC].copy_from_slice(&crc.to_le_bytes());
    body + REC_CRC
}

pub struct Writer {
    pub mode: Mode,
    dir: PathBuf,
    cfg: ArenaCfg,
    level: i32,
    arena: Arena,
    map: Option<Map>,
    /// Dirty range of the map since the last sync.
    dirty: Option<(u64, u64)>,
    cctx: Option<zstd::bulk::Compressor<'static>>,
    buf: Vec<u8>,
    pub index: HashMap<ObjectId, Loc>,
    pub times: Times,
    pub stored_bytes: u64,
    pub arenas: u64,
}

impl Writer {
    /// A writer on a new arena `num` in `dir`.
    pub fn create(dir: &Path, num: u64, cfg: ArenaCfg, mode: Mode, level: i32) -> io::Result<Writer> {
        assert_eq!(cfg.granule, 1, "spike E: granule 1 only");
        let t = Instant::now();
        let arena = Arena::create(dir, num, &cfg)?;
        let mut w = Self::with(dir, cfg, mode, level, arena)?;
        w.times.new_arena.push(t.elapsed().as_secs_f64());
        Ok(w)
    }

    /// A writer that resumes arena `num` (recovery: walk and cut a torn
    /// tail) with its records in the index.
    pub fn open(dir: &Path, num: u64, cfg: ArenaCfg, mode: Mode, level: i32) -> io::Result<Writer> {
        let idx = crate::arena::load_idx(dir, num);
        let o = Arena::open(dir, num, &cfg, idx, Recovery::Append(Walk::Window), false, true)?
            .ok_or_else(|| io::Error::other("arena header bad"))?;
        let mut w = Self::with(dir, cfg, mode, level, o.arena)?;
        w.index.extend(o.entries);
        Ok(w)
    }

    fn with(dir: &Path, cfg: ArenaCfg, mode: Mode, level: i32, arena: Arena) -> io::Result<Writer> {
        let map = match mode {
            Mode::Mmap => Some(Map::new(arena.file(), arena.len())?),
            _ => None,
        };
        let cctx = match mode {
            Mode::Alloc => None,
            _ if level > 0 => Some(zstd::bulk::Compressor::new(level)?),
            _ => None,
        };
        let buf = match mode {
            Mode::PwBuf => vec![0u8; codec::rec_len(MAX_STORED)],
            _ => Vec::new(),
        };
        Ok(Writer {
            mode,
            dir: dir.to_owned(),
            cfg,
            level,
            arena,
            map,
            dirty: None,
            cctx,
            buf,
            index: HashMap::new(),
            times: Times::default(),
            stored_bytes: 0,
            arenas: 1,
        })
    }

    pub fn arena(&self) -> &Arena {
        &self.arena
    }

    /// Store `data`; `None` if it was already there.
    pub fn put(&mut self, data: &[u8]) -> io::Result<Option<Loc>> {
        assert!(data.len() <= MAX_STORED as usize, "spike E: objects up to MAX_STORED");
        let id = ObjectId::of(data);
        if self.index.contains_key(&id) {
            return Ok(None);
        }
        loop {
            if let Some(loc) = self.try_put(&id, data)? {
                self.index.insert(id, loc);
                self.stored_bytes += loc.stored as u64;
                return Ok(Some(loc));
            }
            self.rotate()?;
        }
    }

    /// Put into the active arena; `None` = it does not fit.
    fn try_put(&mut self, id: &ObjectId, data: &[u8]) -> io::Result<Option<Loc>> {
        let raw = data.len() as u32;
        let loc = |off: u64, flags: u8, stored: u32, num: u64| Loc { arena: num, off, stored, raw, flags, seq: 0 };
        match self.mode {
            Mode::Alloc => {
                // Today's code (store.rs / chunk-store put).
                let compressed = (self.level > 0 && data.len() >= MIN_COMPRESS)
                    .then(|| zstd::bulk::compress(data, self.level).ok())
                    .flatten()
                    .filter(|c| c.len() <= data.len() - data.len() / 8);
                let (flags, payload) = match &compressed {
                    Some(c) => (FLAG_ZSTD, c.as_slice()),
                    None => (0, data),
                };
                let rec = codec::encode_record(id, flags, raw, payload, 1);
                Ok(self.arena.append(&rec)?.map(|off| loc(off, flags, payload.len() as u32, self.arena.num)))
            }
            Mode::PwBuf => {
                if self.arena.hwm + codec::rec_len(raw) as u64 > self.arena.len() {
                    return Ok(None);
                }
                let n = encode_into(self.cctx.as_mut(), id, data, &mut self.buf);
                let h = head(&self.buf[..n]);
                let off = self.arena.append(&self.buf[..n])?.expect("fit checked");
                Ok(Some(loc(off, h.0, h.1, self.arena.num)))
            }
            Mode::Mmap => {
                let worst = codec::rec_len(raw);
                let off = self.arena.hwm;
                if off + worst as u64 > self.arena.len() {
                    return Ok(None);
                }
                let map = self.map.as_ref().unwrap();
                let dst = map.slice(off, worst);
                let n = encode_into(self.cctx.as_mut(), id, data, dst);
                let h = head(&dst[..n]);
                let end = off + n as u64;
                self.arena.hwm = end;
                self.arena.dirty = true;
                self.dirty = Some(match self.dirty {
                    Some((lo, hi)) => (lo.min(off), hi.max(end)),
                    None => (off, end),
                });
                Ok(Some(loc(off, h.0, h.1, self.arena.num)))
            }
        }
    }

    /// Make every record so far durable.
    pub fn sync(&mut self) -> io::Result<()> {
        if !self.arena.dirty {
            return Ok(());
        }
        if let (Some(m), Some((lo, hi))) = (&self.map, self.dirty) {
            let t = Instant::now();
            m.msync(lo, hi)?;
            self.times.msync.push(t.elapsed().as_secs_f64());
            self.dirty = None;
        }
        let t = Instant::now();
        sys::full_sync(self.arena.file())?;
        self.arena.dirty = false;
        self.times.fullsync.push(t.elapsed().as_secs_f64());
        Ok(())
    }

    /// The active arena is full: sync it, drop its map, start the next one.
    fn rotate(&mut self) -> io::Result<()> {
        self.sync()?;
        if let Some(m) = self.map.take() {
            let t = Instant::now();
            drop(m);
            self.times.unmap.push(t.elapsed().as_secs_f64());
        }
        let t = Instant::now();
        let num = self.arena.num + 1;
        self.arena = Arena::create(&self.dir, num, &self.cfg)?;
        if self.mode == Mode::Mmap {
            self.map = Some(Map::new(self.arena.file(), self.arena.len())?);
        }
        self.times.new_arena.push(t.elapsed().as_secs_f64());
        self.arenas += 1;
        Ok(())
    }

    /// Read `id` back with `pread` (the record is checked by CRC and hash).
    pub fn get(&self, id: &ObjectId) -> Option<Vec<u8>> {
        let l = self.index.get(id)?;
        if l.arena != self.arena.num {
            let f = std::fs::File::open(crate::arena::arena_path(&self.dir, l.arena)).ok()?;
            return read_loc(&f, l, id);
        }
        read_loc(self.arena.file(), l, id)
    }
}

fn head(rec: &[u8]) -> (u8, u32) {
    (rec[32], u32::from_le_bytes(rec[37..41].try_into().unwrap()))
}

/// `pread` the record at `l` and return its bytes if CRC and hash match.
pub fn read_loc(f: &std::fs::File, l: &Loc, id: &ObjectId) -> Option<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut buf = vec![0u8; codec::rec_len(l.stored)];
    f.read_exact_at(&mut buf, l.off).ok()?;
    let Decoded::Ok(h) = codec::decode_record(&buf, MAX_STORED) else { return None };
    if h.id != *id {
        return None;
    }
    let p = codec::payload(&buf, h.stored);
    let data = if h.flags & FLAG_ZSTD != 0 { zstd::bulk::decompress(p, h.raw as usize).ok()? } else { p.to_vec() };
    (ObjectId::of(&data) == *id).then_some(data)
}
