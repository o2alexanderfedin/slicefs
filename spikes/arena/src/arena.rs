//! One arena file: create (header + reserve), append, read, sync, seal, and
//! open with recovery (append mode). See plan sections 3 and 4.

use crate::codec::{self, ArenaHeader, Decoded, Idx, IdxEntry, IdxState, HEADER_PAGE, MAX_STORED};
use crate::sys::{self, Fault};
use crate::{ArenaCfg, Loc, ObjectId, Prealloc, Walk};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub const WINDOW: usize = 1 << 20;

pub fn arena_path(dir: &Path, num: u64) -> PathBuf {
    dir.join(format!("{num:016x}.arena"))
}

pub fn idx_path(dir: &Path, num: u64) -> PathBuf {
    dir.join(format!("{num:016x}.idx"))
}

/// How `open` finds the valid records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// Spikes A and B: one chain of records from the header page, cut at the
    /// first record that fails.
    Append(Walk),
}

pub struct Arena {
    pub num: u64,
    path: PathBuf,
    file: File,
    prealloc: Prealloc,
    granule: u32,
    trim_on_seal: bool,
    /// Logical file length. For `Grow` it is `hwm`.
    len: u64,
    /// Arena length from the header (the fixed length; for `Grow` the seal
    /// limit).
    size: u64,
    /// End of the last record.
    pub hwm: u64,
    /// Written since the last `sync`.
    pub dirty: bool,
    max_stored: u32,
}

/// Result of `Arena::open`.
pub struct Opened {
    pub arena: Arena,
    pub entries: Vec<(ObjectId, Loc)>,
    /// The arena has a sealed idx (it takes no more appends).
    pub sealed: bool,
    /// Bytes the recovery walk read, and whether a cut ran (for A-B4).
    pub walked: u64,
}

impl Arena {
    /// A new arena: header page, then the reserve of the variant, then
    /// `full_sync` (V1-V3; V0 syncs at the first `sync`, as today).
    pub fn create(dir: &Path, num: u64, cfg: &ArenaCfg) -> io::Result<Arena> {
        let path = arena_path(dir, num);
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        let size = cfg.size;
        file.write_all_at(&codec::encode_header(&ArenaHeader { num, size, granule: cfg.granule }), 0)?;
        let len = match cfg.prealloc {
            Prealloc::Grow => HEADER_PAGE,
            Prealloc::Sparse => {
                file.set_len(size)?;
                size
            }
            Prealloc::Reserve => {
                sys::preallocate(&file, size)?;
                file.set_len(size)?;
                size
            }
            Prealloc::Zeroed => {
                write_zeros(&file, HEADER_PAGE, size)?;
                size
            }
        };
        let mut a = Arena {
            num,
            path,
            file,
            prealloc: cfg.prealloc,
            granule: cfg.granule,
            trim_on_seal: cfg.trim_on_seal,
            len,
            size,
            hwm: HEADER_PAGE,
            dirty: true,
            max_stored: max_stored(cfg.size, size),
        };
        if cfg.prealloc.fixed() {
            sys::full_sync(&a.file)?;
            a.dirty = false;
        }
        Ok(a)
    }

    /// Open arena `num`. `idx` is its decoded idx file, if valid. `active`:
    /// this is the arena that takes appends; a torn tail is cut here, before
    /// any reader maps it. A non-active arena without a sealed idx is walked
    /// and sealed. Returns `None` if the header is bad and there is no idx
    /// (the file is deleted, unless read-only).
    pub fn open(
        dir: &Path,
        num: u64,
        cfg: &ArenaCfg,
        idx: Option<Idx>,
        rec: Recovery,
        ro: bool,
        active: bool,
    ) -> io::Result<Option<Opened>> {
        let path = arena_path(dir, num);
        let file = OpenOptions::new().read(true).write(!ro).open(&path)?;
        let flen = file.metadata()?.len();
        let mut hb = vec![0u8; HEADER_PAGE.min(flen) as usize];
        file.read_exact_at(&mut hb, 0)?;
        let header = codec::decode_header(&hb).filter(|h| h.num == num);
        let idx = idx.filter(|i| i.num == num);
        let sealed_idx = idx.as_ref().is_some_and(|i| i.state == IdxState::Sealed);
        let Some(header) = header.or_else(|| {
            // A damaged header with a sealed idx: the records are still
            // checked by CRC on read; keep the arena.
            sealed_idx.then_some(ArenaHeader { num, size: flen, granule: cfg.granule })
        }) else {
            if !ro {
                drop(file);
                let _ = fs::remove_file(idx_path(dir, num));
                fs::remove_file(&path)?;
            }
            return Ok(None);
        };
        let mut a = Arena {
            num,
            path,
            file,
            prealloc: if header.size != cfg.size && cfg.prealloc.fixed() { Prealloc::Sparse } else { cfg.prealloc },
            granule: header.granule.max(1),
            trim_on_seal: cfg.trim_on_seal,
            len: flen,
            size: header.size,
            hwm: HEADER_PAGE,
            dirty: false,
            max_stored: max_stored(cfg.size, header.size),
        };
        if a.prealloc == Prealloc::Grow {
            a.size = cfg.size;
        }
        let to_loc = |e: &IdxEntry| (e.id, Loc { arena: num, off: e.off, stored: e.stored, raw: e.raw, flags: e.flags, seq: 0 });
        if let Some(i) = idx.as_ref().filter(|i| i.state == IdxState::Sealed) {
            a.hwm = i.hwm;
            let entries = i.entries.iter().map(to_loc).collect();
            return Ok(Some(Opened { arena: a, entries, sealed: true, walked: 0 }));
        }
        let Recovery::Append(walk) = rec;
        // Open idx: trust it up to `hwm`, then walk on from there.
        let (mut entries, from) = match &idx {
            Some(i) if i.state == IdxState::Open && i.hwm >= HEADER_PAGE && i.hwm <= flen => {
                (i.entries.iter().map(to_loc).collect::<Vec<_>>(), i.hwm)
            }
            _ => (Vec::new(), HEADER_PAGE),
        };
        let (found, valid, walked) = a.walk(from, walk)?;
        entries.extend(found);
        a.hwm = valid;
        if !ro {
            if active {
                a.cut(valid)?;
            } else {
                a.write_idx(&entries, IdxState::Sealed)?;
                return Ok(Some(Opened { arena: a, entries, sealed: true, walked }));
            }
        }
        Ok(Some(Opened { arena: a, entries, sealed: false, walked }))
    }

    /// The records from `from` up to the first one that fails, and the end
    /// of the last good one. Also returns the bytes read.
    pub fn walk(&self, from: u64, mode: Walk) -> io::Result<(Vec<(ObjectId, Loc)>, u64, u64)> {
        let end = match mode {
            Walk::HoleBounded => sys::seek_hole(&self.file, from).unwrap_or(self.len).min(self.len),
            _ => self.len,
        };
        let mut out = Vec::new();
        let mut push = |h: codec::RecHead, off: u64| {
            out.push((h.id, Loc { arena: self.num, off, stored: h.stored, raw: h.raw, flags: h.flags, seq: 0 }))
        };
        if mode == Walk::FullRead {
            let buf = fs::read(&self.path)?;
            let mut pos = from as usize;
            while pos < buf.len() {
                match codec::decode_record(&buf[pos..], self.max_stored) {
                    Decoded::Ok(h) => {
                        push(h, pos as u64);
                        pos += codec::padded_len(h.stored, self.granule) as usize;
                    }
                    _ => break,
                }
            }
            return Ok((out, (pos as u64).min(buf.len() as u64).max(from), buf.len() as u64));
        }
        let mut buf = vec![0u8; WINDOW.max(self.max_stored as usize + codec::rec_len(0))];
        let (mut wstart, mut wlen) = (from, 0usize);
        let mut pos = from;
        let mut read = 0u64;
        while pos < end {
            let rel = (pos - wstart) as usize;
            let avail = if rel <= wlen { &buf[rel..wlen] } else { &buf[..0] };
            match codec::decode_record(avail, self.max_stored) {
                Decoded::Ok(h) => {
                    push(h, pos);
                    pos += codec::padded_len(h.stored, self.granule);
                }
                Decoded::Bad => break,
                Decoded::Short => {
                    if wlen > 0 && wstart + wlen as u64 >= end {
                        break; // the window reaches the end: a torn record
                    }
                    // Refill from `pos`; the window holds the largest record.
                    let n = ((end - pos) as usize).min(buf.len());
                    self.file.read_exact_at(&mut buf[..n], pos)?;
                    read += n as u64;
                    wstart = pos;
                    wlen = n;
                }
            }
        }
        Ok((out, pos.min(end.max(from)), read))
    }

    /// Recovery cut of the active arena at `valid` (plan section 3): the
    /// bytes after `valid` read as zeros afterwards, and the file has its
    /// fixed length again.
    fn cut(&mut self, valid: u64) -> io::Result<()> {
        match self.prealloc {
            Prealloc::Grow => {
                if self.len != valid {
                    self.file.set_len(valid)?;
                }
                self.len = valid;
            }
            Prealloc::Sparse | Prealloc::Reserve => {
                self.file.set_len(valid)?;
                self.len = valid;
                sys::hit(Fault::CutAfterTruncDown)?;
                if self.prealloc == Prealloc::Reserve {
                    sys::preallocate(&self.file, self.size)?;
                }
                self.file.set_len(self.size)?;
                self.len = self.size;
            }
            Prealloc::Zeroed => {
                if self.len < self.size {
                    write_zeros(&self.file, self.len, self.size)?;
                    self.len = self.size;
                }
                // Zero up to the end of the last window with a nonzero byte.
                let mut last = valid;
                let mut buf = vec![0u8; WINDOW];
                let mut pos = valid;
                while pos < self.len {
                    let n = ((self.len - pos) as usize).min(WINDOW);
                    self.file.read_exact_at(&mut buf[..n], pos)?;
                    if buf[..n].iter().any(|&b| b != 0) {
                        last = pos + n as u64;
                    }
                    pos += n as u64;
                }
                write_zeros(&self.file, valid, last)?;
            }
        }
        sys::full_sync(&self.file)?;
        self.dirty = false;
        Ok(())
    }

    /// Append a record at the high-water mark. `None`: it does not fit (the
    /// check runs before the write). `Grow` arenas always take the record;
    /// the store seals them once they reach their size.
    pub fn append(&mut self, rec: &[u8]) -> io::Result<Option<u64>> {
        let n = rec.len() as u64;
        if self.prealloc.fixed() && self.hwm + n > self.len {
            return Ok(None);
        }
        self.file.write_all_at(rec, self.hwm)?;
        let off = self.hwm;
        self.hwm += n;
        if self.hwm > self.len {
            self.len = self.hwm;
        }
        self.dirty = true;
        Ok(Some(off))
    }

    /// Write a record into a hole (spike C). Never changes the length.
    pub fn write_at(&mut self, off: u64, rec: &[u8]) -> io::Result<()> {
        if off < HEADER_PAGE || off + rec.len() as u64 > self.len {
            return Err(io::Error::other("write_at outside the arena"));
        }
        self.file.write_all_at(rec, off)?;
        self.hwm = self.hwm.max(off + rec.len() as u64);
        self.dirty = true;
        Ok(())
    }

    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(buf, off)
    }

    pub fn sync(&mut self) -> io::Result<()> {
        if self.dirty {
            sys::full_sync(&self.file)?;
            self.dirty = false;
        }
        Ok(())
    }

    /// Sync, then write the idx with `state`. With `trim_on_seal` and
    /// `Sealed`, the file is first cut to `hwm` (A-B3 only).
    pub fn seal(&mut self, entries: &[(ObjectId, Loc)], state: IdxState) -> io::Result<()> {
        self.sync()?;
        if state == IdxState::Sealed && self.trim_on_seal && self.prealloc.fixed() && self.len > self.hwm {
            self.file.set_len(self.hwm)?;
            self.len = self.hwm;
            sys::hit(Fault::TrimBeforeIdx)?;
            sys::full_sync(&self.file)?;
        }
        self.write_idx(entries, state)
    }

    fn write_idx(&self, entries: &[(ObjectId, Loc)], state: IdxState) -> io::Result<()> {
        let dir = self.path.parent().unwrap();
        let idx = Idx {
            num: self.num,
            state,
            hwm: self.hwm,
            entries: entries
                .iter()
                .map(|(id, l)| IdxEntry { id: *id, off: l.off, stored: l.stored, raw: l.raw, flags: l.flags })
                .collect(),
        };
        sys::write_durable(&idx_path(dir, self.num), &codec::encode_idx(&idx))
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Logical file length.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.hwm == HEADER_PAGE
    }

    /// Arena size from the header (the seal limit for `Grow`).
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn granule(&self) -> u32 {
        self.granule
    }

    pub fn prealloc(&self) -> Prealloc {
        self.prealloc
    }

    /// Payload bound of the decoder for this arena.
    pub fn max_stored(&self) -> u32 {
        self.max_stored
    }
}

/// Load and decode the idx of arena `num`, if there is a valid one.
pub fn load_idx(dir: &Path, num: u64) -> Option<Idx> {
    fs::read(idx_path(dir, num)).ok().and_then(|b| codec::decode_idx(&b))
}

/// An arena whose size differs from the configured size holds one large
/// object: its decoder bound is its length.
fn max_stored(cfg_size: u64, size: u64) -> u32 {
    if size != cfg_size {
        size.min(u32::MAX as u64) as u32
    } else {
        MAX_STORED
    }
}

pub fn write_zeros(f: &File, from: u64, to: u64) -> io::Result<()> {
    let zeros = vec![0u8; WINDOW];
    let mut pos = from;
    while pos < to {
        let n = ((to - pos) as usize).min(WINDOW);
        f.write_all_at(&zeros[..n], pos)?;
        pos += n as u64;
    }
    Ok(())
}
