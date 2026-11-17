//! Spike C: reuse of free space inside arenas, instead of compaction
//! (plan section 6). `ReuseStore` has the same surface as `SpikeStore`
//! (open, put, get, contains, sync, close, stats, gc_begin / gc_finish /
//! gc_abort) on the same arena files (fixed length, V1 sparse by default),
//! the same record v2 (padded to the granule G) and idx v2 (state
//! `Checkpoint`).
//!
//! Durability rules:
//! - RULE W: before the first write into an arena that has a checkpoint, the
//!   arena goes into the dirty list (`packs/dirty`), durably. An arena with
//!   no checkpoint is scanned at open anyway, so it needs no entry.
//! - RULE C: an arena leaves the dirty list only after its new checkpoint is
//!   durable (the arena file is `full_sync`ed first; `Arena::seal` does it).
//! - `sync()` runs `full_sync` on every arena written since the last sync,
//!   then the directory if it changed. Then it writes the checkpoints of the
//!   dirty arenas the allocator has left since (so a commit pays for them,
//!   not a put), and rewrites the dirty list once.
//! - Checkpoints are also written at close (every arena) and after GC (every
//!   arena that lost records).
//!
//! Recovery (open): an arena in the dirty list, or without a checkpoint, is
//! scanned at every granule; a record is accepted only if its marker,
//! bounds, CRC and BLAKE3(decoded bytes) == id all hold. No tail is cut or
//! zeroed. A missing or damaged dirty list means: scan every arena. Free
//! space is derived from the index, never stored.
//!
//! GC: `gc_begin` seals nothing and returns `Seq(next seq)`. `gc_finish`
//! removes every indexed record with a smaller seq that is not live and was
//! not put or read since `gc_begin`; their extents go to `pending_free`.
//! While a GC runs the allocator gives out bump space only (never-written
//! space; `relax_during_gc` turns this off). An arena left with no live
//! record is deleted (also an own arena of a large object). With
//! `evacuate_below = Some(x)` (V3) an arena under x live is emptied: its
//! records are copied (bump space only) and it is deleted after the copies
//! are durable. Then the checkpoints are written, and last the pending
//! extents are given to the allocator under the write lock of `reuse_gate`.
//!
//! Readers: `get` holds the read guard of `reuse_gate` from the index lookup
//! to the end of the copy-out / decompress, so no extent is reused while a
//! reader that saw the old location is still reading it. BLAKE3 runs after
//! the guard is dropped.

use crate::alloc::{self, Allocator};
use crate::arena::{self, Arena, Recovery, WINDOW};
use crate::codec::{self, Decoded, IdxState, FLAG_ZSTD, HEADER_PAGE, MARKER, MAX_STORED, REC_CRC, REC_HEADER};
use crate::read::{self, ReadPath};
use crate::store::GcToken;
use crate::sys::{self, Fault};
use crate::{Error, GcStats, Loc, ObjectId, Options, Result, ReuseCfg, Stats};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

const MIN_COMPRESS: usize = 64;
const DIRTY_MAGIC: &[u8; 4] = b"SFDL";

/// Bytes written to the store directory, by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoCounters {
    /// Records of new objects (with padding).
    pub records: u64,
    /// Of `records`: padding.
    pub padding: u64,
    /// GC copies (compaction or evacuation).
    pub copies: u64,
    /// idx / checkpoint files.
    pub idx: u64,
    /// Dirty-list rewrites (bytes) and their count.
    pub dirty_list: u64,
    pub dirty_writes: u64,
    pub checkpoints: u64,
}

impl IoCounters {
    pub fn total(&self) -> u64 {
        self.records + self.copies + self.idx + self.dirty_list
    }
}

/// Size of an encoded idx with `n` entries.
pub fn idx_bytes(n: usize) -> u64 {
    (codec::IDX_HEAD + n * codec::IDX_ENTRY + 4) as u64
}

// ── Granule scan ──────────────────────────────────────────────────────────────

/// Decode and check the record at `b[0]`: marker, bounds, CRC, then
/// BLAKE3(decoded) == id.
pub fn accept(b: &[u8], max_stored: u32) -> Option<codec::RecHead> {
    let Decoded::Ok(h) = codec::decode_record(b, max_stored) else { return None };
    let p = codec::payload(b, h.stored);
    let ok = if h.flags & FLAG_ZSTD != 0 {
        zstd::bulk::decompress(p, h.raw as usize).is_ok_and(|d| d.len() == h.raw as usize && ObjectId::of(&d) == h.id)
    } else {
        p.len() == h.raw as usize && ObjectId::of(p) == h.id
    };
    ok.then_some(h)
}

/// Scan the arena at every granule (reuse-mode recovery). Returns the
/// accepted records, the end of the highest one, and the bytes read.
pub fn granule_scan(a: &Arena, seek_data: bool) -> io::Result<(Vec<(ObjectId, Loc)>, u64, u64)> {
    let len = a.len();
    let g = a.granule().max(1) as u64;
    let max_stored = a.max_stored();
    let maxrec = codec::rec_len(max_stored);
    let mut buf = vec![0u8; WINDOW + maxrec];
    let (mut ws, mut wl) = (0u64, 0usize);
    let mut pos = HEADER_PAGE;
    let mut out = Vec::new();
    let mut hwm = HEADER_PAGE;
    let mut read = 0u64;
    let min = (REC_HEADER + REC_CRC) as u64;
    let marker = MARKER.to_le_bytes();
    while pos + min <= len {
        if pos + maxrec as u64 > ws + wl as u64 && ws + (wl as u64) < len || pos < ws {
            if seek_data {
                match sys::seek_data(a.file(), pos) {
                    Ok(Some(d)) if d > pos => {
                        pos = d.div_ceil(g) * g;
                        continue;
                    }
                    Ok(None) => break,
                    _ => {}
                }
            }
            ws = pos;
            wl = ((len - pos) as usize).min(buf.len());
            a.read_at(ws, &mut buf[..wl])?;
            read += wl as u64;
        }
        let rel = (pos - ws) as usize;
        let b = &buf[rel..wl];
        if b.len() >= 4 && b[..4] == marker {
            if let Some(h) = accept(b, max_stored) {
                out.push((h.id, Loc { arena: a.num, off: pos, stored: h.stored, raw: h.raw, flags: h.flags, seq: 0 }));
                pos += codec::padded_len(h.stored, g as u32);
                hwm = pos;
                continue;
            }
        }
        pos += g;
    }
    Ok((out, hwm, read))
}

// ── Dirty list ────────────────────────────────────────────────────────────────

pub fn dirty_path(dir: &Path) -> PathBuf {
    dir.join("dirty")
}

pub fn encode_dirty(nums: &BTreeSet<u64>) -> Vec<u8> {
    let mut b = Vec::with_capacity(12 + nums.len() * 8);
    b.extend_from_slice(DIRTY_MAGIC);
    b.extend_from_slice(&(nums.len() as u32).to_le_bytes());
    for n in nums {
        b.extend_from_slice(&n.to_le_bytes());
    }
    let crc = crc32c::crc32c(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    b
}

/// `None`: missing or damaged.
pub fn load_dirty(dir: &Path) -> Option<BTreeSet<u64>> {
    let b = fs::read(dirty_path(dir)).ok()?;
    if b.len() < 12 || &b[..4] != DIRTY_MAGIC {
        return None;
    }
    let (body, crc) = b.split_at(b.len() - 4);
    if crc32c::crc32c(body) != u32::from_le_bytes(crc.try_into().unwrap()) {
        return None;
    }
    let n = u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;
    if body.len() != 8 + n * 8 {
        return None;
    }
    Some(body[8..].chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect())
}

// ── Store ─────────────────────────────────────────────────────────────────────

struct RArena {
    arena: Arena,
    ids: HashSet<ObjectId>,
    /// Padded bytes of the indexed records.
    live: u64,
    /// An own arena of one large object (sealed idx, not reused).
    own: bool,
    /// Has a checkpoint on disk (RULE W applies).
    ckpt: bool,
}

struct RInner {
    index: HashMap<ObjectId, Loc>,
    arenas: BTreeMap<u64, RArena>,
    alloc: Box<dyn Allocator>,
    /// The durable content of the dirty list.
    dirty: BTreeSet<u64>,
    /// Written since the last sync.
    written: BTreeSet<u64>,
    /// Dirty arenas the allocator left: checkpoint at the next sync.
    left: BTreeSet<u64>,
    target: Option<u64>,
    next_arena: u64,
    stats: Stats,
    touched: Option<HashSet<ObjectId>>,
    seq: u64,
    dir_dirty: bool,
    io: IoCounters,
    /// Arena entries, for the target switch statistics.
    switches: u64,
}

pub struct ReuseStore {
    dir: PathBuf,
    opts: Options,
    rc: ReuseCfg,
    gate: RwLock<()>,
    inner: Mutex<RInner>,
    read: Box<dyn ReadPath>,
}

/// What `open` found (C-B3).
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenReport {
    pub arenas: u64,
    pub scanned: u64,
    pub bytes_read: u64,
}

impl ReuseStore {
    pub fn open(dir: &Path, opts: Options) -> Result<Self> {
        Self::open_report(dir, opts).map(|x| x.0)
    }

    pub fn open_report(dir: &Path, opts: Options) -> Result<(Self, OpenReport)> {
        let rc = opts.reuse.ok_or_else(|| Error::Io(io::Error::other("ReuseStore needs Options::reuse")))?;
        if opts.arena.trim_on_seal {
            return Err(Error::Io(io::Error::other("trim_on_seal is refused with reuse")));
        }
        if !opts.arena.prealloc.fixed() {
            return Err(Error::Io(io::Error::other("reuse needs fixed-size arenas (V1-V3)")));
        }
        let g = opts.arena.granule.max(1);
        if 4096 % g != 0 {
            return Err(Error::Io(io::Error::other("granule must divide 4096")));
        }
        let packs = dir.join("packs");
        let ro = opts.read_only;
        if !ro {
            fs::create_dir_all(&packs)?;
        }
        let mut inner = RInner {
            index: HashMap::new(),
            arenas: BTreeMap::new(),
            alloc: alloc::new(rc.alloc),
            dirty: BTreeSet::new(),
            written: BTreeSet::new(),
            left: BTreeSet::new(),
            target: None,
            next_arena: 1,
            stats: Stats::default(),
            touched: None,
            seq: 0,
            dir_dirty: false,
            io: IoCounters::default(),
            switches: 0,
        };
        let mut rep = OpenReport::default();
        let mut nums = Vec::new();
        if packs.is_dir() {
            for e in fs::read_dir(&packs)? {
                let name = e?.file_name().to_string_lossy().into_owned();
                if name.ends_with(".tmp") {
                    if !ro {
                        let _ = fs::remove_file(packs.join(&name));
                    }
                    continue;
                }
                if let Some(n) = name.strip_suffix(".arena").and_then(|h| u64::from_str_radix(h, 16).ok()) {
                    nums.push(n);
                }
            }
        }
        nums.sort_unstable();
        inner.next_arena = nums.last().map_or(1, |n| n + 1);
        let dirty = load_dirty(&packs);
        let scan_all = dirty.is_none() && !nums.is_empty();
        let dirty = dirty.unwrap_or_default();
        let mut ready = Vec::new();
        for num in nums {
            let idx = arena::load_idx(&packs, num);
            // An idx that open would trust when the arena is not dirty.
            let has_ckpt =
                idx.as_ref().is_some_and(|i| i.num == num && matches!(i.state, IdxState::Checkpoint | IdxState::Sealed));
            let scan = scan_all || dirty.contains(&num) || !has_ckpt;
            let idx = if scan { None } else { idx };
            let rec = Recovery::Reuse { scan, seek_data: rc.seek_data };
            let Some(o) = Arena::open(&packs, num, &opts.arena, idx, rec, ro, false)? else { continue };
            rep.arenas += 1;
            if scan && !o.sealed {
                rep.scanned += 1;
                rep.bytes_read += o.walked;
            }
            let own = o.arena.size() != opts.arena.size;
            ready.push((num, o.arena.len(), !own));
            let mut ra = RArena { arena: o.arena, ids: HashSet::new(), live: 0, own, ckpt: has_ckpt };
            for (id, l) in o.entries {
                if inner.index.contains_key(&id) {
                    continue; // a second copy: its space is free
                }
                ra.ids.insert(id);
                ra.live += codec::padded_len(l.stored, g);
                inner.add(id, l);
            }
            inner.stats.packs += 1;
            if !own {
                let mut used: Vec<(u64, u64)> =
                    ra.ids.iter().map(|id| inner.index[id]).map(|l| (l.off, codec::padded_len(l.stored, g))).collect();
                inner.alloc.add_arena_used(num, HEADER_PAGE, ra.arena.len(), &mut used);
                if scan {
                    // Still dirty on disk: checkpoint it at the next sync.
                    if dirty.contains(&num) {
                        inner.left.insert(num);
                    }
                }
            }
            inner.arenas.insert(num, ra);
        }
        // Only arenas that exist stay in the dirty list.
        inner.dirty = dirty.into_iter().filter(|n| inner.arenas.contains_key(n)).collect();
        if scan_all && !ro {
            // The list was missing or damaged: every scanned arena is dirty.
            inner.dirty = inner.arenas.iter().filter(|(_, a)| !a.own).map(|(&n, _)| n).collect();
            inner.left = inner.dirty.clone();
        }
        let read = read::make(&packs, &opts)?;
        for (num, len, fixed) in ready {
            read.arena_ready(num, len, fixed);
        }
        let s = ReuseStore { dir: packs, opts, rc, gate: RwLock::new(()), inner: Mutex::new(inner), read };
        // A new store starts with an (empty) dirty list; a damaged or
        // missing one is rewritten after the full scan.
        if !ro && (scan_all || !dirty_path(&s.dir).exists()) {
            let mut inner = s.inner.lock().unwrap();
            s.write_dirty(&mut inner)?;
        }
        Ok((s, rep))
    }

    pub fn packs_dir(&self) -> &Path {
        &self.dir
    }

    pub fn options(&self) -> &Options {
        &self.opts
    }

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
        if data.len() > u32::MAX as usize - REC_HEADER {
            return Err(Error::TooLarge(data.len()));
        }
        let compressed = (self.opts.zstd_level > 0 && data.len() >= MIN_COMPRESS)
            .then(|| zstd::bulk::compress(data, self.opts.zstd_level).ok())
            .flatten()
            .filter(|c| c.len() <= data.len() - data.len() / 8);
        let (flags, payload) = match &compressed {
            Some(c) => (FLAG_ZSTD, c.as_slice()),
            None => (0, data),
        };
        let g = self.opts.arena.granule;
        let rec = codec::encode_record(&id, flags, data.len() as u32, payload, g);
        let mut inner = self.inner.lock().unwrap();
        inner.touch(id);
        if inner.index.contains_key(&id) {
            return Ok(id);
        }
        let loc = self.place(&mut inner, &id, &rec, payload.len() as u32, data.len() as u32, flags)?;
        inner.io.records += rec.len() as u64;
        inner.io.padding += rec.len() as u64 - codec::rec_len(payload.len() as u32) as u64;
        inner.add(id, loc);
        let ra = inner.arenas.get_mut(&loc.arena).unwrap();
        ra.ids.insert(id);
        ra.live += rec.len() as u64;
        Ok(id)
    }

    fn new_arena(&self, inner: &mut RInner, size: u64) -> Result<u64> {
        let num = inner.next_arena;
        inner.next_arena += 1;
        let mut cfg = self.opts.arena;
        cfg.size = size;
        let a = Arena::create(&self.dir, num, &cfg)?;
        let own = size != self.opts.arena.size;
        self.read.arena_ready(num, a.len(), !own);
        if !own {
            inner.alloc.add_arena(num, HEADER_PAGE, a.len());
        }
        inner.arenas.insert(num, RArena { arena: a, ids: HashSet::new(), live: 0, own, ckpt: false });
        inner.stats.packs += 1;
        inner.dir_dirty = true;
        Ok(num)
    }

    /// Write an encoded (padded) record where the allocator says, under
    /// RULE W. A record too large for an arena gets an own arena, sealed.
    fn place(&self, inner: &mut RInner, id: &ObjectId, rec: &[u8], stored: u32, raw: u32, flags: u8) -> Result<Loc> {
        let size = self.opts.arena.size;
        let len = rec.len() as u64;
        if len > size - HEADER_PAGE || stored > MAX_STORED {
            let own = (HEADER_PAGE + len).div_ceil(4096) * 4096;
            let own = if own == size { own + 4096 } else { own };
            let n = self.new_arena(inner, own)?;
            let loc = inner.loc(n, HEADER_PAGE, stored, raw, flags);
            let a = &mut inner.arenas.get_mut(&n).unwrap().arena;
            a.write_at(HEADER_PAGE, rec)?;
            a.seal(&[(*id, loc)], IdxState::Sealed)?;
            inner.io.idx += idx_bytes(1);
            inner.dir_dirty = false;
            return Ok(loc);
        }
        let (n, off) = loop {
            if let Some(x) = inner.alloc.alloc(len) {
                break x;
            }
            self.new_arena(inner, size)?;
        };
        if inner.target != Some(n) {
            if let Some(t) = inner.target {
                if inner.dirty.contains(&t) || inner.arenas.get(&t).is_some_and(|a| !a.ckpt) {
                    inner.left.insert(t);
                }
            }
            inner.target = Some(n);
            inner.left.remove(&n);
            inner.switches += 1;
        }
        // RULE W.
        if inner.arenas[&n].ckpt && !inner.dirty.contains(&n) {
            inner.dirty.insert(n);
            if let Err(e) = self.write_dirty(inner) {
                inner.dirty.remove(&n);
                inner.alloc.free(n, off, len);
                return Err(e);
            }
        }
        let a = &mut inner.arenas.get_mut(&n).unwrap().arena;
        if let Err(e) = a.write_at(off, rec) {
            inner.alloc.free(n, off, len);
            return Err(e.into());
        }
        inner.written.insert(n);
        Ok(inner.loc(n, off, stored, raw, flags))
    }

    fn write_dirty(&self, inner: &mut RInner) -> Result<()> {
        let b = encode_dirty(&inner.dirty);
        sys::hit(Fault::DirtyWrite)?;
        sys::write_durable(&dirty_path(&self.dir), &b)?;
        inner.io.dirty_list += b.len() as u64;
        inner.io.dirty_writes += 1;
        inner.dir_dirty = false;
        Ok(())
    }

    /// Sync the arena (if written) and write its checkpoint. Does not touch
    /// the dirty list (the caller rewrites it once).
    fn checkpoint(&self, inner: &mut RInner, n: u64) -> Result<()> {
        let RInner { index, arenas, io, written, .. } = inner;
        let Some(ra) = arenas.get_mut(&n) else { return Ok(()) };
        if ra.own {
            return Ok(());
        }
        ra.arena.sync()?;
        written.remove(&n);
        sys::hit(Fault::CheckpointWrite)?;
        let entries: Vec<(ObjectId, Loc)> = ra.ids.iter().map(|id| (*id, index[id])).collect();
        ra.arena.seal(&entries, IdxState::Checkpoint)?;
        ra.ckpt = true;
        io.idx += idx_bytes(entries.len());
        io.checkpoints += 1;
        Ok(())
    }

    /// `full_sync` every arena written since the last sync, then the
    /// directory if it changed.
    fn sync_written(&self, inner: &mut RInner) -> Result<()> {
        let w = std::mem::take(&mut inner.written);
        for n in w {
            if let Some(ra) = inner.arenas.get_mut(&n) {
                ra.arena.sync()?;
            }
        }
        if inner.dir_dirty {
            sys::sync_dir(&self.dir)?;
            inner.dir_dirty = false;
        }
        Ok(())
    }

    /// Checkpoint `nums`, then take them out of the dirty list (RULE C).
    fn checkpoint_all(&self, inner: &mut RInner, nums: &[u64]) -> Result<()> {
        let mut changed = false;
        for &n in nums {
            self.checkpoint(inner, n)?;
            inner.left.remove(&n);
            changed |= inner.dirty.remove(&n);
        }
        if changed {
            self.write_dirty(inner)?;
        }
        Ok(())
    }

    pub fn sync(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        self.sync_written(&mut inner)?;
        let left: Vec<u64> = inner.left.iter().copied().filter(|&n| Some(n) != inner.target).collect();
        if !left.is_empty() {
            self.checkpoint_all(&mut inner, &left)?;
        }
        Ok(())
    }

    /// Sync, checkpoint every arena that needs one, empty the dirty list.
    pub fn close(&self) -> Result<()> {
        if self.opts.read_only {
            return Ok(());
        }
        let mut inner = self.inner.lock().unwrap();
        self.sync_written(&mut inner)?;
        let nums: Vec<u64> = inner.arenas.iter().filter(|(n, a)| !a.own && (!a.ckpt || inner.dirty.contains(n))).map(|(&n, _)| n).collect();
        self.checkpoint_all(&mut inner, &nums)?;
        if !inner.dirty.is_empty() {
            inner.dirty.clear();
            self.write_dirty(&mut inner)?;
        }
        Ok(())
    }

    pub fn get(&self, id: &ObjectId) -> Option<Vec<u8>> {
        let guard = self.gate.read().unwrap();
        let loc = {
            let mut inner = self.inner.lock().unwrap();
            let loc = *inner.index.get(id)?;
            inner.touch(*id);
            loc
        };
        let data = read::with_record(&*self.read, &loc, |buf| {
            let Decoded::Ok(h) = codec::decode_record(buf, u32::MAX) else { return None };
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
        drop(guard);
        (ObjectId::of(&data) == *id).then_some(data)
    }

    pub fn contains(&self, id: &ObjectId) -> bool {
        self.inner.lock().unwrap().index.contains_key(id)
    }

    pub fn loc(&self, id: &ObjectId) -> Option<Loc> {
        self.inner.lock().unwrap().index.get(id).copied()
    }

    pub fn stats(&self) -> Stats {
        self.inner.lock().unwrap().stats
    }

    pub fn io(&self) -> IoCounters {
        self.inner.lock().unwrap().io
    }

    /// (arenas in the dirty list, target switches so far).
    pub fn dirty_info(&self) -> (usize, u64) {
        let i = self.inner.lock().unwrap();
        (i.dirty.len(), i.switches)
    }

    pub fn alloc_stats(&self, min: u64) -> alloc::AllocStats {
        self.inner.lock().unwrap().alloc.stats(min)
    }

    pub fn gc_begin(&self) -> Result<GcToken> {
        let mut inner = self.inner.lock().unwrap();
        inner.touched = Some(HashSet::new());
        let relax = self.rc.relax_during_gc;
        inner.alloc.set_bump_only(!relax);
        Ok(GcToken::Seq(inner.seq + 1))
    }

    pub fn gc_abort(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.touched = None;
        inner.alloc.set_bump_only(false);
    }

    pub fn gc_finish(&self, live: &HashSet<ObjectId>, token: GcToken) -> Result<GcStats> {
        let GcToken::Seq(t) = token else {
            return Err(Error::Io(io::Error::other("reuse mode takes a Seq token")));
        };
        let g = self.opts.arena.granule;
        let size = self.opts.arena.size;
        let mut st = GcStats::default();
        let mut pending: Vec<(u64, u64, u64)> = Vec::new();
        let mut doomed: Vec<u64> = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            let touched = inner.touched.take().unwrap_or_default();
            let victims: Vec<(ObjectId, Loc)> = inner
                .index
                .iter()
                .filter(|(id, l)| l.seq < t && !live.contains(id) && !touched.contains(id))
                .map(|(id, l)| (*id, *l))
                .collect();
            let mut freed: BTreeSet<u64> = BTreeSet::new();
            for (id, l) in victims {
                inner.index.remove(&id);
                inner.stats.objects -= 1;
                inner.stats.raw_bytes -= l.raw as u64;
                inner.stats.stored_bytes -= l.stored as u64;
                st.removed_objects += 1;
                st.removed_bytes += l.stored as u64;
                let len = codec::padded_len(l.stored, g);
                let ra = inner.arenas.get_mut(&l.arena).unwrap();
                ra.ids.remove(&id);
                ra.live -= len;
                pending.push((l.arena, l.off, len));
                freed.insert(l.arena);
            }
            st.kept_objects = inner.index.len() as u64;
            let target = inner.target;
            // Empty arenas go; V3 evacuates sparse ones.
            let nums: Vec<u64> = inner.arenas.keys().copied().collect();
            for n in nums {
                let ra = &inner.arenas[&n];
                if ra.ids.is_empty() && (ra.own || Some(n) != target) {
                    if !ra.own {
                        inner.alloc.remove_arena(n);
                    }
                    doomed.push(n);
                    st.packs_deleted += 1;
                    continue;
                }
                let Some(x) = self.rc.evacuate_below else { continue };
                if ra.own || Some(n) == target || (ra.live as f64) >= x as f64 * (size - HEADER_PAGE) as f64 {
                    continue;
                }
                inner.alloc.remove_arena(n);
                inner.alloc.set_bump_only(!self.rc.relax_during_gc);
                let ids: Vec<ObjectId> = inner.arenas[&n].ids.iter().copied().collect();
                for id in ids {
                    let l = inner.index[&id];
                    let rec = read::with_record(&*self.read, &l, |b| {
                        let mut r = b.to_vec();
                        r.resize(codec::padded_len(l.stored, g) as usize, 0);
                        r
                    })
                    .ok_or_else(|| Error::Corrupt(format!("record of {id:?} unreadable during evacuation")))?;
                    let nl = self.place(&mut inner, &id, &rec, l.stored, l.raw, l.flags)?;
                    inner.index.insert(id, nl);
                    inner.io.copies += rec.len() as u64;
                    let dst = inner.arenas.get_mut(&nl.arena).unwrap();
                    dst.ids.insert(id);
                    dst.live += rec.len() as u64;
                }
                let ra = inner.arenas.get_mut(&n).unwrap();
                ra.ids.clear();
                ra.live = 0;
                doomed.push(n);
                st.packs_compacted += 1;
            }
            // Copies durable, then checkpoints without the freed records
            // (RULE C), then delete.
            self.sync_written(&mut inner)?;
            let ck: Vec<u64> = freed.iter().copied().filter(|n| !doomed.contains(n) && inner.arenas.contains_key(n)).collect();
            for &n in &ck {
                self.checkpoint(&mut inner, n)?;
                inner.left.remove(&n);
                inner.dirty.remove(&n);
            }
            let mut list_changed = !ck.is_empty();
            for &n in &doomed {
                list_changed |= inner.dirty.remove(&n);
                inner.left.remove(&n);
            }
            if list_changed {
                self.write_dirty(&mut inner)?;
            }
            if !doomed.is_empty() {
                for &n in &doomed {
                    self.read.forget(n);
                    let _ = fs::remove_file(arena::idx_path(&self.dir, n));
                    fs::remove_file(arena::arena_path(&self.dir, n))?;
                    inner.arenas.remove(&n);
                    inner.stats.packs -= 1;
                }
                sys::sync_dir(&self.dir)?;
            }
            inner.alloc.set_bump_only(false);
        }
        // Publish the freed extents once no reader can still use them.
        let _w = self.gate.write().unwrap();
        let mut inner = self.inner.lock().unwrap();
        for (a, off, len) in pending {
            if !doomed.contains(&a) {
                inner.alloc.free(a, off, len);
            }
        }
        Ok(st)
    }
}

impl RInner {
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

impl crate::file_fork::ObjStore for ReuseStore {
    fn put(&self, data: &[u8]) -> Result<ObjectId> {
        ReuseStore::put(self, data)
    }
    fn get(&self, id: &ObjectId) -> Option<Vec<u8>> {
        ReuseStore::get(self, id)
    }
    fn sync(&self) -> Result<()> {
        ReuseStore::sync(self)
    }
    fn close(&self) -> Result<()> {
        ReuseStore::close(self)
    }
    fn stats(&self) -> Stats {
        ReuseStore::stats(self)
    }
}
