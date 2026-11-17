//! `SpikeStore`: the chunk store on arena files, with the same surface as
//! `chunk_store::ChunkStore` (open, put, get, contains, sync, close, stats,
//! gc_begin / gc_finish / gc_abort). Its structure mirrors `ChunkStore`
//! (compression outside the lock, two lock acquisitions per put, one
//! `pwrite` per record, BLAKE3 check in `get`), so spike V0 can be
//! calibrated against the real store.

use crate::arena::{self, Arena, Recovery};
use crate::codec::{self, IdxState, FLAG_ZSTD, HEADER_PAGE, MAX_STORED};
use crate::read::{self, ReadPath};
use crate::sys;
use crate::{Error, GcStats, Loc, ObjectId, Options, Prealloc, ReadMode, Result, SealRule, Stats};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Objects smaller than this are not compressed (as `chunk-store`).
const MIN_COMPRESS: usize = 64;

/// What `gc_finish` may change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcToken {
    /// Append mode: arenas up to this number (not the active one).
    Boundary(u64),
    /// Reuse mode (spike C): records with a smaller seq.
    Seq(u64),
}

struct Active {
    arena: Arena,
    entries: Vec<(ObjectId, Loc)>,
}

struct Inner {
    index: HashMap<ObjectId, Loc>,
    active: Option<Active>,
    next_arena: u64,
    stats: Stats,
    /// An arena file was made since the last directory sync.
    dir_dirty: bool,
    /// While a GC runs: every object put or read since `gc_begin`.
    touched: Option<HashSet<ObjectId>>,
    seq: u64,
    /// Arena numbers that exist on disk.
    arenas: HashSet<u64>,
}

pub struct SpikeStore {
    dir: PathBuf,
    opts: Options,
    inner: Mutex<Inner>,
    read: Box<dyn ReadPath>,
}

impl SpikeStore {
    /// Open (or create) the store in `dir` (arenas in `dir/packs`).
    pub fn open(dir: &Path, opts: Options) -> Result<Self> {
        if opts.arena.trim_on_seal && (opts.read != ReadMode::Pread || opts.reuse.is_some()) {
            return Err(Error::Io(io::Error::other("trim_on_seal is refused with mmap reads or reuse")));
        }
        if opts.reuse.is_some() {
            return Err(Error::Io(io::Error::other("reuse mode is spike C; not built in spike A")));
        }
        let g = opts.arena.granule.max(1);
        if 4096 % g != 0 {
            return Err(Error::Io(io::Error::other("granule must divide 4096")));
        }
        let packs = dir.join("packs");
        let mut inner = Inner {
            index: HashMap::new(),
            active: None,
            next_arena: 1,
            stats: Stats::default(),
            dir_dirty: false,
            touched: None,
            seq: 0,
            arenas: HashSet::new(),
        };
        if opts.read_only && !packs.is_dir() {
            let read = read::make(&packs, &opts)?;
            return Ok(SpikeStore { dir: packs, opts, inner: Mutex::new(inner), read });
        }
        if !opts.read_only {
            fs::create_dir_all(&packs)?;
        }
        let mut nums = Vec::new();
        for e in fs::read_dir(&packs)? {
            let name = e?.file_name().to_string_lossy().into_owned();
            if name.ends_with(".tmp") {
                if !opts.read_only {
                    let _ = fs::remove_file(packs.join(&name));
                }
                continue;
            }
            if let Some(n) = name.strip_suffix(".arena").and_then(|h| u64::from_str_radix(h, 16).ok()) {
                nums.push(n);
            }
        }
        nums.sort_unstable();
        inner.next_arena = nums.last().map_or(1, |n| n + 1);
        let idxs: Vec<_> = nums.iter().map(|&n| arena::load_idx(&packs, n)).collect();
        // The active arena: the newest without a sealed idx.
        let active_num = nums
            .iter()
            .zip(&idxs)
            .rev()
            .find(|(_, i)| i.as_ref().is_none_or(|i| i.state != IdxState::Sealed))
            .map(|(n, _)| *n);
        let rec = Recovery::Append(opts.walk);
        // (arena, logical length, has the full fixed length) for the read path.
        let mut ready = Vec::new();
        for (num, idx) in nums.into_iter().zip(idxs) {
            let active = Some(num) == active_num;
            let Some(o) = Arena::open(&packs, num, &opts.arena, idx, rec, opts.read_only, active)? else {
                continue;
            };
            inner.arenas.insert(num);
            inner.stats.packs += 1;
            ready.push((num, o.arena.len(), o.arena.prealloc().fixed() && o.arena.len() == o.arena.size()));
            for (id, l) in &o.entries {
                inner.add(*id, *l);
            }
            if active && !o.sealed && !opts.read_only {
                inner.active = Some(Active { arena: o.arena, entries: o.entries });
            }
        }
        let read = read::make(&packs, &opts)?;
        for (num, len, fixed) in ready {
            read.arena_ready(num, len, fixed);
        }
        Ok(SpikeStore { dir: packs, opts, inner: Mutex::new(inner), read })
    }

    pub fn options(&self) -> &Options {
        &self.opts
    }

    /// The directory that holds the arena files.
    pub fn packs_dir(&self) -> &Path {
        &self.dir
    }

    /// Store `data` and return its ID. Bytes already in the store are not
    /// written again.
    pub fn put(&self, data: &[u8]) -> Result<ObjectId> {
        if self.opts.read_only {
            return Err(Error::Io(io::Error::other("the chunk store is open read-only")));
        }
        let id = ObjectId::of(data);
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.index.contains_key(&id) {
                inner.touch(id);
                return Ok(id);
            }
        }
        if data.len() > u32::MAX as usize - codec::REC_HEADER {
            return Err(Error::TooLarge(data.len()));
        }
        // Compress outside the lock.
        let compressed = (self.opts.zstd_level > 0 && data.len() >= MIN_COMPRESS)
            .then(|| zstd::bulk::compress(data, self.opts.zstd_level).ok())
            .flatten()
            .filter(|c| c.len() <= data.len() - data.len() / 8);
        let (flags, payload) = match &compressed {
            Some(c) => (FLAG_ZSTD, c.as_slice()),
            None => (0, data),
        };
        let rec = codec::encode_record(&id, flags, data.len() as u32, payload, self.opts.arena.granule);

        let mut inner = self.inner.lock().unwrap();
        inner.touch(id);
        if inner.index.contains_key(&id) {
            return Ok(id);
        }
        let loc = self.append(&mut inner, &id, &rec, payload.len() as u32, data.len() as u32, flags)?;
        inner.add(id, loc);
        Ok(id)
    }

    fn new_arena(&self, inner: &mut Inner, size: u64) -> Result<Arena> {
        let num = inner.next_arena;
        inner.next_arena += 1;
        let mut cfg = self.opts.arena;
        cfg.size = size;
        let a = Arena::create(&self.dir, num, &cfg)?;
        inner.arenas.insert(num);
        inner.stats.packs += 1;
        inner.dir_dirty = true;
        self.read.arena_ready(num, a.len(), a.prealloc().fixed());
        Ok(a)
    }

    /// Append an encoded record to the active arena (made if there is none).
    /// Fixed arenas: the fit check runs before the write; a record that does
    /// not fit seals the arena. `Grow`: seal after the arena reaches its
    /// size, as `chunk-store`. A record too large for an arena, or with a
    /// payload over `MAX_STORED`, gets an arena of its own, sealed at once.
    fn append(&self, inner: &mut Inner, id: &ObjectId, rec: &[u8], stored: u32, raw: u32, flags: u8) -> Result<Loc> {
        let size = self.opts.arena.size;
        if rec.len() as u64 > size - HEADER_PAGE || stored > MAX_STORED {
            let own = (HEADER_PAGE + rec.len() as u64).div_ceil(4096) * 4096;
            // An own arena must not have the configured size (its decoder
            // bound is its length).
            let own = if own == size { own + 4096 } else { own };
            let mut a = self.new_arena(inner, own)?;
            let off = a.append(rec)?.ok_or_else(|| Error::Io(io::Error::other("own arena too small")))?;
            let loc = inner.loc(a.num, off, stored, raw, flags);
            a.seal(&[(*id, loc)], IdxState::Sealed)?;
            inner.dir_dirty = false; // write_durable synced the directory
            return Ok(loc);
        }
        loop {
            if inner.active.is_none() {
                let arena = self.new_arena(inner, size)?;
                inner.active = Some(Active { arena, entries: Vec::new() });
            }
            let a = inner.active.as_mut().unwrap();
            if let Some(off) = a.arena.append(rec)? {
                let num = a.arena.num;
                let loc = inner.loc(num, off, stored, raw, flags);
                let a = inner.active.as_mut().unwrap();
                a.entries.push((*id, loc));
                if a.arena.prealloc() == Prealloc::Grow && a.arena.hwm >= a.arena.size() {
                    self.seal(inner)?;
                }
                return Ok(loc);
            }
            self.seal(inner)?;
        }
    }

    /// Start a GC. Seals the active arena if `gc_seal` says so (default: it
    /// holds at least size/8), and records from now on each object put or
    /// read. The active arena is never swept.
    pub fn gc_begin(&self) -> Result<GcToken> {
        let mut inner = self.inner.lock().unwrap();
        let seal = inner.active.as_ref().is_some_and(|a| match self.opts.gc_seal {
            SealRule::Always => !a.entries.is_empty(),
            SealRule::Eighth => a.arena.hwm - HEADER_PAGE >= self.opts.arena.size / 8,
        });
        if seal {
            self.seal(&mut inner)?;
        }
        inner.touched = Some(HashSet::new());
        Ok(GcToken::Boundary(inner.next_arena - 1))
    }

    pub fn gc_abort(&self) {
        self.inner.lock().unwrap().touched = None;
    }

    /// End a GC: remove every object of the arenas up to the boundary (not
    /// the active one) that is not in `live` and was not put or read since
    /// `gc_begin`. Rules as `chunk-store`: an arena with no live object is
    /// deleted; one with less than half of its bytes live, or a small one
    /// (under size/8), is compacted into the active arena, which is synced
    /// before the old arena is deleted.
    pub fn gc_finish(&self, live: &HashSet<ObjectId>, token: GcToken) -> Result<GcStats> {
        let GcToken::Boundary(boundary) = token else {
            return Err(Error::Io(io::Error::other("seq tokens are spike C")));
        };
        let mut inner = self.inner.lock().unwrap();
        let touched = inner.touched.take().unwrap_or_default();
        let keep = |id: &ObjectId| live.contains(id) || touched.contains(id);
        let active = inner.active.as_ref().map(|a| a.arena.num);
        let g = self.opts.arena.granule;
        let mut by_arena: HashMap<u64, Vec<(ObjectId, Loc)>> = HashMap::new();
        for (id, l) in &inner.index {
            if l.arena <= boundary && Some(l.arena) != active {
                by_arena.entry(l.arena).or_default().push((*id, *l));
            }
        }
        let mut st = GcStats::default();
        let mut doomed = Vec::new();
        let mut nums: Vec<u64> = by_arena.keys().copied().collect();
        nums.sort_unstable();
        for num in nums {
            let entries = &by_arena[&num];
            let rl = |l: &Loc| codec::padded_len(l.stored, g);
            let total: u64 = entries.iter().map(|(_, l)| rl(l)).sum();
            let live_bytes: u64 = entries.iter().filter(|(id, _)| keep(id)).map(|(_, l)| rl(l)).sum();
            let small = total < self.opts.arena.size / 8;
            if live_bytes == total && !small {
                st.kept_objects += entries.len() as u64;
                continue;
            }
            if live_bytes > 0 && live_bytes * 2 >= total && !small {
                st.kept_objects += entries.len() as u64;
                continue;
            }
            for (id, l) in entries {
                if keep(id) {
                    let rec = read::with_record(&*self.read, l, |b| {
                        let mut r = b.to_vec();
                        r.resize(codec::padded_len(l.stored, g) as usize, 0);
                        r
                    })
                    .ok_or_else(|| Error::Corrupt(format!("record of {id:?} unreadable during compaction")))?;
                    let nl = self.append(&mut inner, id, &rec, l.stored, l.raw, l.flags)?;
                    inner.index.insert(*id, nl);
                    st.kept_objects += 1;
                } else {
                    inner.index.remove(id);
                    inner.stats.objects -= 1;
                    inner.stats.raw_bytes -= l.raw as u64;
                    inner.stats.stored_bytes -= l.stored as u64;
                    st.removed_objects += 1;
                    st.removed_bytes += l.stored as u64;
                }
            }
            if live_bytes > 0 {
                st.packs_compacted += 1;
            } else {
                st.packs_deleted += 1;
            }
            doomed.push(num);
        }
        // Arenas that the index no longer uses at all. Neither the arena that
        // was active when the GC started (with `SealRule::Eighth` it may be
        // <= boundary, and compaction may have filled and sealed it) nor the
        // arena that is active now is a candidate.
        let active_now = inner.active.as_ref().map(|a| a.arena.num);
        let in_use: HashSet<u64> = inner.index.values().map(|l| l.arena).collect();
        for &num in &inner.arenas {
            if num <= boundary
                && Some(num) != active
                && Some(num) != active_now
                && !in_use.contains(&num)
                && !by_arena.contains_key(&num)
                && !doomed.contains(&num)
            {
                doomed.push(num);
            }
        }
        if doomed.is_empty() {
            return Ok(st);
        }
        // The copies must be durable before the originals go. (A compacted
        // record may have sealed the active arena; seal syncs it.)
        if let Some(a) = inner.active.as_mut() {
            a.arena.sync()?;
        }
        sys::sync_dir(&self.dir)?;
        for num in doomed {
            self.read.forget(num);
            let _ = fs::remove_file(arena::idx_path(&self.dir, num));
            fs::remove_file(arena::arena_path(&self.dir, num))?;
            inner.arenas.remove(&num);
            inner.stats.packs -= 1;
        }
        sys::sync_dir(&self.dir)?;
        inner.dir_dirty = false;
        Ok(st)
    }

    /// The bytes of `id`, or `None` if the store does not have the object or
    /// its record is damaged.
    pub fn get(&self, id: &ObjectId) -> Option<Vec<u8>> {
        let loc = {
            let mut inner = self.inner.lock().unwrap();
            let loc = *inner.index.get(id)?;
            inner.touch(*id);
            loc
        };
        let data = read::with_record(&*self.read, &loc, |buf| {
            let codec::Decoded::Ok(h) = codec::decode_record(buf, loc.stored) else { return None };
            if h.id != *id || h.stored != loc.stored {
                return None;
            }
            let payload = codec::payload(buf, h.stored);
            if h.flags & FLAG_ZSTD != 0 {
                zstd::bulk::decompress(payload, h.raw as usize).ok()
            } else {
                Some(payload.to_vec())
            }
        })??;
        (ObjectId::of(&data) == *id).then_some(data)
    }

    pub fn contains(&self, id: &ObjectId) -> bool {
        self.inner.lock().unwrap().index.contains_key(id)
    }

    pub fn stored_len(&self, id: &ObjectId) -> Option<u32> {
        self.inner.lock().unwrap().index.get(id).map(|l| l.stored)
    }

    pub fn loc(&self, id: &ObjectId) -> Option<Loc> {
        self.inner.lock().unwrap().index.get(id).copied()
    }

    /// Make every object put so far durable. Without new objects this does
    /// not touch the disk.
    pub fn sync(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(a) = inner.active.as_mut() {
            a.arena.sync()?;
        }
        if inner.dir_dirty {
            sys::sync_dir(&self.dir)?;
            inner.dir_dirty = false;
        }
        Ok(())
    }

    /// Sync, and write a resumable (`open`) idx for the active arena; it is
    /// not sealed (with `close_seals`, it is, as today).
    pub fn close(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if self.opts.close_seals {
            self.seal(&mut inner)?;
        } else if let Some(a) = inner.active.as_mut() {
            a.arena.seal(&a.entries, IdxState::Open)?;
            inner.dir_dirty = false;
        }
        if inner.dir_dirty {
            sys::sync_dir(&self.dir)?;
            inner.dir_dirty = false;
        }
        Ok(())
    }

    /// Seal the active arena now (tests and benches).
    pub fn seal_active(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        self.seal(&mut inner)
    }

    /// Number and high-water mark of the active arena.
    pub fn active(&self) -> Option<(u64, u64)> {
        self.inner.lock().unwrap().active.as_ref().map(|a| (a.arena.num, a.arena.hwm))
    }

    pub fn stats(&self) -> Stats {
        self.inner.lock().unwrap().stats
    }

    fn seal(&self, inner: &mut Inner) -> Result<()> {
        let Some(mut a) = inner.active.take() else { return Ok(()) };
        a.arena.seal(&a.entries, IdxState::Sealed)?;
        inner.dir_dirty = false; // write_durable synced the directory
        if a.arena.len() != self.opts.arena.size {
            self.read.arena_ready(a.arena.num, a.arena.len(), false);
        }
        Ok(())
    }
}

impl Inner {
    fn touch(&mut self, id: ObjectId) {
        if let Some(t) = self.touched.as_mut() {
            t.insert(id);
        }
    }

    fn loc(&mut self, arena: u64, off: u64, stored: u32, raw: u32, flags: u8) -> Loc {
        self.seq += 1;
        Loc { arena, off, stored, raw, flags, seq: self.seq }
    }

    fn add(&mut self, id: ObjectId, l: Loc) {
        if self.index.insert(id, l).is_none() {
            self.stats.objects += 1;
            self.stats.raw_bytes += l.raw as u64;
            self.stats.stored_bytes += l.stored as u64;
        }
    }
}
