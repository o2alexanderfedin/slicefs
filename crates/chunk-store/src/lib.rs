//! Content-addressed chunk store: objects by BLAKE3-256 in append-only pack
//! files, with zstd per object. See `docs/design/chunk-store.md`.
//!
//! Layout: `<dir>/packs/NNNNNNNNNNNNNNNN.pack` (records) and `.idx` (index of a
//! sealed pack). One pack is active (append-only); it is sealed when it
//! reaches `Options::pack_limit`, and at `close`.
//!
//! Crash safety: `sync` makes every object put before it durable. At open, a
//! pack without a valid index is scanned; the scan stops at the first record
//! with a bad CRC or a short read, and the pack is cut there.

use std::collections::{HashMap, HashSet};
use std::io;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

pub mod file;
pub mod hash;
pub mod pack_io;
pub mod retry;
#[allow(dead_code)] // The store uses the runs from plan 17-03 on.
mod run;
pub mod mem;

use crate::pack_io::{FileIo, Kind, PackFile, PackIo, PackRead, PUNCH_BLOCK};
pub use crate::hash::{hasher_by_name, hasher_by_tag, hasher_names, hashers, Blake3_256, ObjectHasher, DEFAULT_HASHER};

// ── Object IDs and chunking ───────────────────────────────────────────────────

/// The ID of an object: a tag byte that names the hash (see
/// [`ObjectHasher`]), then the 32-byte digest of the uncompressed bytes. The
/// same size as the `ObjectKey` that the metadata uses for content keys. A
/// global, decentralized store needs the full 2^128 collision bound, and the
/// tag lets the hash change without a new ID format.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; ID_LEN]);

/// Length of a digest in bytes (every hash gives this many).
pub const DIGEST_LEN: usize = 32;

/// Length of an object ID in bytes: the tag, then the digest.
pub const ID_LEN: usize = 1 + DIGEST_LEN;

impl ObjectId {
    /// The ID with this tag and digest.
    pub fn new(tag: u8, digest: [u8; DIGEST_LEN]) -> Self {
        let mut b = [0u8; ID_LEN];
        b[0] = tag;
        b[1..].copy_from_slice(&digest);
        ObjectId(b)
    }

    /// The tag of the hash that made this ID.
    pub fn tag(&self) -> u8 {
        self.0[0]
    }

    /// The digest (without the tag).
    pub fn digest(&self) -> &[u8; DIGEST_LEN] {
        self.0[1..].try_into().unwrap()
    }

    /// Lowercase hex of all bytes (tag first).
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ObjectId({:02x}:{})", self.tag(), &self.to_hex()[2..18])
    }
}

/// FastCDC v2020 chunk sizes (bytes). The minimum is one 4 KiB disk block:
/// no cut inside a file makes a chunk smaller than a block (min 2 KiB stored
/// less than 1 % less data on source trees and tars, with 5-9 % more chunks).
pub const CHUNK_MIN: u32 = 4 * 1024;
pub const CHUNK_AVG: u32 = 8 * 1024;
pub const CHUNK_MAX: u32 = 32 * 1024;

/// Content-defined chunk boundaries of `data` (FastCDC v2020, 4/8/32 KiB).
/// The ranges cover `data` without gaps; empty input gives no chunks.
pub fn chunk_ranges(data: &[u8]) -> Vec<Range<usize>> {
    fastcdc::v2020::FastCDC::new(data, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX)
        .map(|c| c.offset..c.offset + c.length)
        .collect()
}

// ── Options and stats ─────────────────────────────────────────────────────────

/// Store options.
#[derive(Clone, Debug)]
pub struct Options {
    /// Seal the active pack when it reaches this size (bytes).
    pub pack_limit: u64,
    /// zstd level; 0 = no compression.
    pub zstd_level: i32,
    /// Open without changes on disk: no cut of the active pack, no index
    /// writes, and `put` fails. For tools that read a store that a mount may
    /// write at the same time.
    pub read_only: bool,
    /// The hash of the IDs of new objects. Objects of other known hashes
    /// stay readable: each is verified with the hasher of its own tag.
    pub hasher: &'static dyn ObjectHasher,
}

impl Default for Options {
    fn default() -> Self {
        Options { pack_limit: 64 << 20, zstd_level: 3, read_only: false, hasher: DEFAULT_HASHER }
    }
}

/// Counts for `slicefs stats` and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Distinct objects in the store.
    pub objects: u64,
    /// Bytes of the objects before compression.
    pub raw_bytes: u64,
    /// Bytes of the object payloads in the packs (after compression).
    pub stored_bytes: u64,
    /// Pack files.
    pub packs: u64,
}

/// Errors of the chunk store.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("chunk store I/O: {0}")]
    Io(#[from] io::Error),
    #[error("object too large for one record: {0} bytes")]
    TooLarge(usize),
    #[error("object not in the store: {0:?}")]
    Missing(ObjectId),
    #[error("damaged data: {0}")]
    Corrupt(String),
    #[error("object {0:?} uses a hash tag that this SliceFS does not know (a damaged or newer store)")]
    UnknownHash(ObjectId),
}

pub type Result<T> = std::result::Result<T, Error>;

// ── On-disk format ────────────────────────────────────────────────────────────

const PACK_MAGIC: &[u8; 4] = b"SFPK";
const IDX_MAGIC: &[u8; 4] = b"SFIX";
/// Version 2: 33-byte tagged object IDs.
const FORMAT_VERSION: u16 = 2;
/// Bit in the version field of a pack header: GC punched holes in the
/// pack. Such a pack is never the active pack and never cut at open.
const PACK_PUNCHED: u16 = 0x8000;
/// magic(4) + version(2) + pack number(8)
const PACK_HEADER: usize = 14;
/// id(ID_LEN) + flags(1) + raw length(4) + stored length(4)
const REC_HEADER: usize = ID_LEN + 9;
const REC_CRC: usize = 4;
/// id(ID_LEN) + offset(8) + stored(4) + raw(4) + flags(1)
const IDX_ENTRY: usize = ID_LEN + 17;
const FLAG_ZSTD: u8 = 1;
/// Pack files that the store keeps open for reads at most (plus the active
/// pack). More packs than this are opened again when needed, so a large
/// store does not reach the limit of open files.
pub const MAX_OPEN_PACKS: usize = 64;
/// Objects smaller than this are not compressed.
const MIN_COMPRESS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Loc {
    pack: u64,
    offset: u64,
    stored: u32,
    raw: u32,
    flags: u8,
}

/// Encode a record into `r` (cleared first).
fn encode_record_into(r: &mut Vec<u8>, id: &ObjectId, flags: u8, raw: u32, payload: &[u8]) {
    r.clear();
    r.reserve(REC_HEADER + payload.len() + REC_CRC);
    r.extend_from_slice(&id.0);
    r.push(flags);
    r.extend_from_slice(&raw.to_le_bytes());
    r.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    r.extend_from_slice(payload);
    let crc = crc32c::crc32c(r);
    r.extend_from_slice(&crc.to_le_bytes());
}

/// Largest buffer that a thread keeps after a put.
const SCRATCH_KEEP: usize = 1 << 20;

/// Per-thread buffers of `put`: a zstd context and two buffers, used again
/// for each object (spike E: 95 -> 81 us of CPU for each put).
struct Scratch {
    zstd: Option<(i32, zstd::bulk::Compressor<'static>)>,
    compressed: Vec<u8>,
    record: Vec<u8>,
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> =
        const { std::cell::RefCell::new(Scratch { zstd: None, compressed: Vec::new(), record: Vec::new() }) };
}

impl Scratch {
    /// Compress `data` into `self.compressed`; false if it does not get
    /// smaller by 1/8 or the compressor fails.
    fn compress(&mut self, data: &[u8], level: i32) -> bool {
        if self.zstd.as_ref().is_none_or(|(l, _)| *l != level) {
            match zstd::bulk::Compressor::new(level) {
                Ok(c) => self.zstd = Some((level, c)),
                Err(_) => return false,
            }
        }
        let c = &mut self.zstd.as_mut().unwrap().1;
        self.compressed.clear();
        self.compressed.reserve(zstd::zstd_safe::compress_bound(data.len()));
        match c.compress_to_buffer(data, &mut self.compressed) {
            Ok(n) => n <= data.len() - data.len() / 8,
            Err(_) => false,
        }
    }
}

/// Parse the record that starts at `buf[0]`. Returns its location fields and
/// its total length, or `None` for a short or damaged record.
fn decode_record(buf: &[u8]) -> Option<(ObjectId, u8, u32, u32, usize)> {
    if buf.len() < REC_HEADER + REC_CRC {
        return None;
    }
    let id = ObjectId(buf[..ID_LEN].try_into().unwrap());
    let flags = buf[ID_LEN];
    let raw = u32::from_le_bytes(buf[ID_LEN + 1..ID_LEN + 5].try_into().unwrap());
    let stored = u32::from_le_bytes(buf[ID_LEN + 5..ID_LEN + 9].try_into().unwrap());
    let total = REC_HEADER + stored as usize + REC_CRC;
    if buf.len() < total {
        return None;
    }
    let body = total - REC_CRC;
    let crc = u32::from_le_bytes(buf[body..total].try_into().unwrap());
    (crc32c::crc32c(&buf[..body]) == crc).then_some((id, flags, raw, stored, total))
}

/// Scan a pack: the records up to the first bad one, and the length of the
/// valid part.
fn scan_pack(buf: &[u8], num: u64) -> (Vec<(ObjectId, Loc)>, u64) {
    let mut out = Vec::new();
    if buf.len() < PACK_HEADER || &buf[..4] != PACK_MAGIC {
        return (out, 0);
    }
    let mut pos = PACK_HEADER;
    while let Some((id, flags, raw, stored, total)) = decode_record(&buf[pos..]) {
        out.push((id, Loc { pack: num, offset: pos as u64, stored, raw, flags }));
        pos += total;
    }
    (out, pos as u64)
}

/// Error unless a pack or index header (from byte 4) has this format version.
fn check_version(b: &[u8], what: &str, num: u64) -> Result<()> {
    if b.len() < 6 {
        return Ok(()); // short: the scan treats it as damaged
    }
    let v = u16::from_le_bytes([b[4], b[5]]) & !PACK_PUNCHED;
    if v != FORMAT_VERSION {
        return Err(Error::Corrupt(format!(
            "{what} {num} has format version {v}; this SliceFS reads version {FORMAT_VERSION}"
        )));
    }
    Ok(())
}

fn is_punched(pack: &[u8]) -> bool {
    pack.len() >= PACK_HEADER && &pack[..4] == PACK_MAGIC && u16::from_le_bytes([pack[4], pack[5]]) & PACK_PUNCHED != 0
}

/// True if `data` hashes to `id` with the hasher of the ID's tag.
fn verify(id: &ObjectId, data: &[u8]) -> Result<bool> {
    let h = hasher_by_tag(id.tag()).ok_or(Error::UnknownHash(*id))?;
    Ok(h.id(data) == *id)
}

/// The record at `buf[0]` if it is valid and its bytes hash to its ID. A
/// record with a good CRC and an unknown hash tag is an error.
fn verified_record(buf: &[u8]) -> Result<Option<(ObjectId, u8, u32, u32, usize)>> {
    let Some((id, flags, raw, stored, total)) = decode_record(buf) else { return Ok(None) };
    let payload = &buf[REC_HEADER..REC_HEADER + stored as usize];
    let ok = if flags & FLAG_ZSTD != 0 {
        match zstd::bulk::decompress(payload, raw as usize) {
            Ok(d) => verify(&id, &d)?,
            Err(_) => false,
        }
    } else {
        raw == stored && verify(&id, payload)?
    };
    Ok(ok.then_some((id, flags, raw, stored, total)))
}

/// Scan a sealed pack that may have holes or damage: at each position, take
/// a record only if its CRC is good and its bytes hash to its ID; else move
/// on by one byte (zero runs are skipped). A punched gap or a damaged record
/// costs only itself, not the records after it.
fn resync_scan(buf: &[u8], num: u64) -> Result<Vec<(ObjectId, Loc)>> {
    let mut out = Vec::new();
    if buf.len() < PACK_HEADER || &buf[..4] != PACK_MAGIC {
        return Ok(out);
    }
    let mut pos = PACK_HEADER;
    // True where a record must start (the first one, or right after a good
    // one). There a record with an unknown hash tag is real (a newer store):
    // an error, so its objects are not dropped without a word. After skipped
    // damage, a CRC match can be chance (1 in 2^32 per position), so there an
    // unknown tag only means "no record here".
    let mut aligned = true;
    while pos + REC_HEADER + REC_CRC <= buf.len() {
        let rec = match verified_record(&buf[pos..]) {
            Err(Error::UnknownHash(_)) if !aligned => None,
            r => r?,
        };
        if let Some((id, flags, raw, stored, total)) = rec {
            out.push((id, Loc { pack: num, offset: pos as u64, stored, raw, flags }));
            pos += total;
            aligned = true;
            continue;
        }
        aligned = false;
        // A record never starts with 8 zero bytes (its ID is a hash): skip zeros.
        match buf[pos..].iter().position(|&b| b != 0) {
            Some(nz) => pos += nz.saturating_sub(7).max(1),
            None => break,
        }
    }
    Ok(out)
}

fn encode_index(entries: &[(ObjectId, Loc)]) -> Vec<u8> {
    let mut sorted: Vec<&(ObjectId, Loc)> = entries.iter().collect();
    sorted.sort_by_key(|(id, _)| *id);
    let mut b = Vec::with_capacity(14 + entries.len() * IDX_ENTRY + 4);
    b.extend_from_slice(IDX_MAGIC);
    b.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    b.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for (id, l) in sorted {
        b.extend_from_slice(&id.0);
        b.extend_from_slice(&l.offset.to_le_bytes());
        b.extend_from_slice(&l.stored.to_le_bytes());
        b.extend_from_slice(&l.raw.to_le_bytes());
        b.push(l.flags);
    }
    let crc = crc32c::crc32c(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    b
}

/// The entries of an index, `None` if it is damaged (the pack is scanned
/// instead), an error if it is intact but of another format version.
fn decode_index(b: &[u8], num: u64) -> Result<Option<Vec<(ObjectId, Loc)>>> {
    if b.len() < 18 || &b[..4] != IDX_MAGIC {
        return Ok(None);
    }
    let (body, crc) = b.split_at(b.len() - 4);
    if crc32c::crc32c(body) != u32::from_le_bytes(crc.try_into().unwrap()) {
        return Ok(None);
    }
    check_version(body, "index", num)?;
    // The count is untrusted: a size that does not fit (or does not match
    // the body) makes the index unusable, and the pack scan rebuilds it.
    let n = u64::from_le_bytes(body[6..14].try_into().unwrap());
    let size = usize::try_from(n).ok().and_then(|n| n.checked_mul(IDX_ENTRY)).and_then(|s| s.checked_add(14));
    if size != Some(body.len()) {
        return Ok(None);
    }
    Ok(Some(
        body[14..]
            .chunks_exact(IDX_ENTRY)
            .map(|e| {
                let id = ObjectId(e[..ID_LEN].try_into().unwrap());
                let o = ID_LEN;
                let offset = u64::from_le_bytes(e[o..o + 8].try_into().unwrap());
                let stored = u32::from_le_bytes(e[o + 8..o + 12].try_into().unwrap());
                let raw = u32::from_le_bytes(e[o + 12..o + 16].try_into().unwrap());
                (id, Loc { pack: num, offset, stored, raw, flags: e[o + 16] })
            })
            .collect(),
    ))
}

// ── Store ─────────────────────────────────────────────────────────────────────

struct Active {
    num: u64,
    file: Box<dyn PackFile>,
    len: u64,
    entries: Vec<(ObjectId, Loc)>,
    /// Bytes appended since the last sync.
    dirty: bool,
}

struct Inner {
    index: HashMap<ObjectId, Loc>,
    /// Open packs for reads, with the tick of their last use (LRU, at most
    /// `MAX_OPEN_PACKS`).
    readers: HashMap<u64, (Arc<dyn PackRead>, u64)>,
    tick: u64,
    active: Option<Active>,
    next_pack: u64,
    stats: Stats,
    /// A pack file was created since the last directory sync.
    dir_dirty: bool,
    /// While a GC runs: every object put or read since `gc_begin`. GC keeps
    /// them even if its mark did not find them.
    touched: Option<HashSet<ObjectId>>,
    /// A sync failed. After that, a later sync can succeed without the data
    /// (Linux can mark the pages clean), so the store refuses every write and
    /// sync until it is opened again; the open scan finds the durable state.
    failed: bool,
}

/// Outcome of a GC.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcStats {
    pub kept_objects: u64,
    pub removed_objects: u64,
    /// Stored bytes of the removed objects.
    pub removed_bytes: u64,
    pub packs_deleted: u64,
    pub packs_compacted: u64,
    /// Packs where GC punched holes over dead records.
    pub packs_punched: u64,
    /// Bytes in the punched ranges.
    pub punched_bytes: u64,
}

/// A content-addressed object store in pack files.
pub struct ChunkStore {
    io: Arc<dyn PackIo>,
    /// The strategy can punch holes (asked once, at open).
    punches: bool,
    /// zstd level for new objects (0 = none); starts as `opts.zstd_level`.
    zstd_level: AtomicI32,
    opts: Options,
    inner: Mutex<Inner>,
}

impl ChunkStore {
    /// Open (or create) the store in `dir`. Packs without a valid index are
    /// scanned; the newest unsealed pack becomes the active pack, cut after
    /// its last valid record.
    pub fn open(dir: &Path, opts: Options) -> Result<Self> {
        let files = Arc::new(FileIo::new(dir.join("packs")));
        Self::open_with(Arc::new(retry::RetryingIo::new(files)), opts)
    }

    /// Open the store on a storage strategy (files, a mock, ...).
    pub fn open_with(io: Arc<dyn PackIo>, opts: Options) -> Result<Self> {
        if !io.prepare(opts.read_only)? {
            let zstd_level = AtomicI32::new(opts.zstd_level);
            return Ok(ChunkStore { io, punches: false, zstd_level, opts, inner: Mutex::new(Inner::empty()) });
        }
        let mut nums = io.list()?;
        nums.sort_unstable();

        let mut inner = Inner {
            index: HashMap::new(),
            readers: HashMap::new(),
            tick: 0,
            active: None,
            next_pack: nums.last().map_or(1, |n| n + 1),
            stats: Stats::default(),
            dir_dirty: false,
            touched: None,
            failed: false,
        };
        let last = nums.last().copied();
        for num in nums {
            let loaded = match io.read(num, Kind::Idx) {
                Ok(b) => decode_index(&b, num)?,
                Err(_) => None,
            };
            let entries = match loaded {
                Some(e) => e,
                None => {
                    let buf = io.read(num, Kind::Pack)?;
                    if buf.starts_with(PACK_MAGIC) {
                        check_version(&buf, "pack", num)?;
                    }
                    let active = Some(num) == last && !is_punched(&buf);
                    // The active pack is cut at its first bad record. A sealed
                    // pack (its index is lost) may have holes or damage in the
                    // middle, so it is resynced past them.
                    let (entries, valid) = if active { scan_pack(&buf, num) } else { (resync_scan(&buf, num)?, 0) };
                    if opts.read_only {
                        for (id, l) in entries {
                            inner.add(id, l);
                        }
                        inner.stats.packs += 1;
                        continue;
                    }
                    if active {
                        // The newest pack without an index is the active pack.
                        let mut file = io.open_rw(num)?;
                        let valid = if valid < PACK_HEADER as u64 {
                            file.set_len(0)?;
                            file.write_at(&pack_header(num), 0)?;
                            PACK_HEADER as u64
                        } else {
                            file.set_len(valid)?;
                            valid
                        };
                        file.sync()?;
                        inner.next_pack = num + 1;
                        for (id, l) in &entries {
                            inner.add(*id, *l);
                        }
                        inner.stats.packs += 1;
                        inner.active = Some(Active { num, file, len: valid, entries, dirty: false });
                        continue;
                    }
                    io.write_durable(num, Kind::Idx, &encode_index(&entries))?;
                    entries
                }
            };
            for (id, l) in entries {
                inner.add(id, l);
            }
            inner.stats.packs += 1;
        }
        let punches = !opts.read_only && io.can_punch();
        let zstd_level = AtomicI32::new(opts.zstd_level);
        Ok(ChunkStore { io, punches, zstd_level, opts, inner: Mutex::new(inner) })
    }

    /// Store `data` and return its ID. Bytes already in the store are not
    /// written again.
    pub fn put(&self, data: &[u8]) -> Result<ObjectId> {
        if self.opts.read_only {
            return Err(Error::Io(io::Error::other("the chunk store is open read-only")));
        }
        let id = self.opts.hasher.id(data);
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
        SCRATCH.with(|s| {
            let s = &mut *s.borrow_mut();
            // Compress outside the lock.
            let level = self.zstd_level.load(Ordering::Relaxed);
            let zstd = level > 0 && data.len() >= MIN_COMPRESS && s.compress(data, level);
            let (flags, payload) = if zstd { (FLAG_ZSTD, s.compressed.as_slice()) } else { (0, data) };
            encode_record_into(&mut s.record, &id, flags, data.len() as u32, payload);
            let stored = payload.len() as u32;

            let r = (|| {
                let mut inner = self.inner.lock().unwrap();
                inner.touch(id);
                if inner.index.contains_key(&id) {
                    return Ok(id);
                }
                inner.check()?;
                let loc = self.append(&mut inner, &id, &s.record, stored, data.len() as u32, flags)?;
                inner.add(id, loc);
                Ok(id)
            })();
            // Chunks and nodes are small; do not keep the buffers of a big object.
            for b in [&mut s.compressed, &mut s.record] {
                if b.capacity() > SCRATCH_KEEP {
                    *b = Vec::new();
                }
            }
            r
        })
    }

    /// Append an encoded record to the active pack (made if there is none),
    /// and seal the pack when it is full. Returns where the record is.
    fn append(&self, inner: &mut Inner, id: &ObjectId, rec: &[u8], stored: u32, raw: u32, flags: u8) -> Result<Loc> {
        if inner.active.is_none() {
            let num = inner.next_pack;
            inner.next_pack += 1;
            let mut file = self.io.create(num)?;
            file.write_at(&pack_header(num), 0)?;
            inner.active = Some(Active { num, file, len: PACK_HEADER as u64, entries: Vec::new(), dirty: true });
            inner.stats.packs += 1;
            inner.dir_dirty = true;
        }
        let a = inner.active.as_mut().unwrap();
        a.file.write_at(rec, a.len)?;
        let loc = Loc { pack: a.num, offset: a.len, stored, raw, flags };
        a.len += rec.len() as u64;
        a.entries.push((*id, loc));
        a.dirty = true;
        if a.len >= self.opts.pack_limit {
            self.seal(inner)?;
        }
        Ok(loc)
    }

    /// Start a GC: seal the active pack, so every object so far is in a
    /// sealed pack, and record from now on each object that is put or read.
    /// Returns the last pack number that the GC may change.
    pub fn gc_begin(&self) -> Result<u64> {
        let mut inner = self.inner.lock().unwrap();
        inner.check()?;
        if inner.active.as_ref().is_some_and(|a| !a.entries.is_empty()) {
            self.seal(&mut inner)?;
        }
        inner.touched = Some(HashSet::new());
        Ok(inner.next_pack - 1)
    }

    /// End a GC without deleting anything (for example after a failed mark).
    pub fn gc_abort(&self) {
        self.inner.lock().unwrap().touched = None;
    }

    /// End a GC: remove every object of the packs up to `boundary` that is
    /// not in `live` and was not put or read since `gc_begin`. A pack with no
    /// live object is deleted; a pack with less than half of its bytes live,
    /// or a small pack, is compacted (its live records are copied to the
    /// active pack, which is synced before the old pack is deleted).
    pub fn gc_finish(&self, live: &HashSet<ObjectId>, boundary: u64) -> Result<GcStats> {
        let mut inner = self.inner.lock().unwrap();
        // The touched set ends with this GC, also when it fails (#63). This
        // call used it to choose what to remove. A failure can stop the work
        // at any point, but loses no kept object: old packs are deleted only
        // after their live copies are synced, and an open rebuilds the index
        // from the packs. The next GC starts its own set in `gc_begin`, before
        // its gate commit, so its mark sees everything put before that.
        // Tests: tests/gc_fault_tests.rs.
        let touched = inner.touched.take().unwrap_or_default();
        inner.check()?;
        let keep = |id: &ObjectId| live.contains(id) || touched.contains(id);
        let active = inner.active.as_ref().map(|a| a.num);
        let mut by_pack: HashMap<u64, Vec<(ObjectId, Loc)>> = HashMap::new();
        for (id, l) in &inner.index {
            if l.pack <= boundary && Some(l.pack) != active {
                by_pack.entry(l.pack).or_default().push((*id, *l));
            }
        }
        let mut st = GcStats::default();
        let mut doomed = Vec::new();
        let mut nums: Vec<u64> = by_pack.keys().copied().collect();
        nums.sort_unstable();
        for num in nums {
            let entries = &by_pack[&num];
            let rec_len = |l: &Loc| (REC_HEADER + l.stored as usize + REC_CRC) as u64;
            let total: u64 = entries.iter().map(|(_, l)| rec_len(l)).sum();
            let live_bytes: u64 = entries.iter().filter(|(id, _)| keep(id)).map(|(_, l)| rec_len(l)).sum();
            let small = total < self.opts.pack_limit / 8;
            if live_bytes == total && !small {
                st.kept_objects += entries.len() as u64;
                continue;
            }
            if live_bytes > 0 && live_bytes * 2 >= total && !small {
                self.punch_pack(&mut inner, num, entries, &keep, &mut st)?;
                continue;
            }
            // Delete, or compact: copy the live records first.
            let file = inner.reader(&*self.io, num)?;
            for (id, l) in entries {
                if keep(id) {
                    let mut rec = vec![0u8; rec_len(l) as usize];
                    file.read_at(&mut rec, l.offset)?;
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
        // Packs that the index no longer uses at all (older copies).
        for num in 1..=boundary {
            if Some(num) != active && !by_pack.contains_key(&num) && self.io.exists(num, Kind::Pack) {
                doomed.push(num);
            }
        }
        if doomed.is_empty() {
            return Ok(st);
        }
        // The copies must be durable before the originals go.
        inner.sync_active()?;
        inner.sync_dir(&*self.io)?;
        for num in doomed {
            let _ = self.io.remove(num, Kind::Idx);
            self.io.remove(num, Kind::Pack)?;
            inner.readers.remove(&num);
            inner.stats.packs -= 1;
        }
        inner.sync_dir(&*self.io)?;
        Ok(st)
    }

    /// GC of a mostly live pack: free the disk blocks of the dead gaps.
    ///
    /// Order: (1) drop the dead records in the punched ranges from the index
    /// and write the pack's index durably without them; (2) mark the pack as
    /// punched in its header and sync; (3) punch. A crash at any point leaves
    /// an index that lists only bytes that are still there, and a punched pack
    /// is resynced at open, never cut.
    fn punch_pack(
        &self,
        inner: &mut Inner,
        num: u64,
        entries: &[(ObjectId, Loc)],
        keep: &dyn Fn(&ObjectId) -> bool,
        st: &mut GcStats,
    ) -> Result<()> {
        if !self.punches {
            // No holes here: keep the dead records in the index, so a later
            // GC still sees their bytes and compacts the pack when it is
            // mostly dead.
            st.kept_objects += entries.len() as u64;
            return Ok(());
        }
        let rec_end = |l: &Loc| l.offset + (REC_HEADER + l.stored as usize + REC_CRC) as u64;
        let mut sorted: Vec<&(ObjectId, Loc)> = entries.iter().collect();
        sorted.sort_by_key(|(_, l)| l.offset);
        let end = sorted.iter().map(|(_, l)| rec_end(l)).max().unwrap_or(0);
        // Gaps between live records, cut to whole blocks inside them.
        let mut ranges = Vec::new();
        let mut from = PACK_HEADER as u64;
        let lives = sorted.iter().filter(|(id, _)| keep(id)).map(|(_, l)| (l.offset, rec_end(l)));
        for (s, e) in lives.chain(std::iter::once((end, end))) {
            let a = from.div_ceil(PUNCH_BLOCK) * PUNCH_BLOCK;
            let b = s / PUNCH_BLOCK * PUNCH_BLOCK;
            if b > a {
                ranges.push((a, b - a));
            }
            from = from.max(e);
        }
        let hits = |l: &Loc, a: u64, n: u64| l.offset < a + n && rec_end(l) > a;
        let victims: Vec<&(ObjectId, Loc)> = sorted
            .iter()
            .copied()
            .filter(|(id, l)| !keep(id) && ranges.iter().any(|&(a, n)| hits(l, a, n)))
            .collect();
        let gone: HashSet<ObjectId> = victims.iter().map(|(id, _)| *id).collect();
        // Keep only the ranges with a dead record (skip holes of earlier GCs).
        ranges.retain(|&(a, n)| victims.iter().any(|(_, l)| hits(l, a, n)));
        if victims.is_empty() {
            st.kept_objects += entries.len() as u64;
            return Ok(());
        }
        for (id, l) in &victims {
            inner.index.remove(id);
            inner.stats.objects -= 1;
            inner.stats.raw_bytes -= l.raw as u64;
            inner.stats.stored_bytes -= l.stored as u64;
            st.removed_objects += 1;
            st.removed_bytes += l.stored as u64;
        }
        let rest: Vec<(ObjectId, Loc)> = entries.iter().filter(|(id, _)| !gone.contains(id)).copied().collect();
        st.kept_objects += rest.len() as u64;
        let marked = (|| -> io::Result<()> {
            self.io.write_durable(num, Kind::Idx, &encode_index(&rest))?;
            let mut f = self.io.open_rw(num)?;
            f.write_at(&pack_header_with(num, PACK_PUNCHED), 0)?;
            f.sync()
        })();
        if let Err(e) = marked {
            inner.failed = true;
            return Err(e.into());
        }
        match self.io.punch(num, &ranges) {
            Ok(()) => {
                st.packs_punched += 1;
                st.punched_bytes += ranges.iter().map(|&(_, n)| n).sum::<u64>();
            }
            // The probe said yes, but this punch could not: the records are
            // gone from the index, their bytes stay until the pack is
            // compacted or deleted. Rare (for example a file system that
            // changed under the store).
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    /// The bytes of `id`, or `None` if the store does not have the object,
    /// its record is damaged, or its hash is unknown (see [`ChunkStore::read`]).
    pub fn get(&self, id: &ObjectId) -> Option<Vec<u8>> {
        self.read(id).ok()
    }

    /// The bytes of `id`, verified with the hasher of the ID's tag.
    /// Errors: `Missing`, `Corrupt` (a damaged record), `UnknownHash` (a tag
    /// this build does not know: a damaged or newer store), `Io`.
    pub fn read(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let hasher = hasher_by_tag(id.tag()).ok_or(Error::UnknownHash(*id))?;
        let (loc, file) = {
            let mut inner = self.inner.lock().unwrap();
            let loc = *inner.index.get(id).ok_or(Error::Missing(*id))?;
            inner.touch(*id);
            let file = inner.reader(&*self.io, loc.pack)?;
            (loc, file)
        };
        let damaged = || Error::Corrupt(format!("object {id:?}"));
        let mut buf = vec![0u8; REC_HEADER + loc.stored as usize + REC_CRC];
        file.read_at(&mut buf, loc.offset)?;
        let (rid, flags, raw, _, _) = decode_record(&buf).ok_or_else(damaged)?;
        if rid != *id {
            return Err(damaged());
        }
        let payload = &buf[REC_HEADER..REC_HEADER + loc.stored as usize];
        let data = if flags & FLAG_ZSTD != 0 {
            zstd::bulk::decompress(payload, raw as usize).map_err(|_| damaged())?
        } else {
            payload.to_vec()
        };
        if hasher.id(&data) != *id {
            return Err(damaged());
        }
        Ok(data)
    }

    /// The hasher of new objects.
    pub fn hasher(&self) -> &'static dyn ObjectHasher {
        self.opts.hasher
    }

    /// True if the store has `id`.
    pub fn contains(&self, id: &ObjectId) -> bool {
        self.inner.lock().unwrap().index.contains_key(id)
    }

    /// Stored (possibly compressed) payload length of `id`.
    pub fn stored_len(&self, id: &ObjectId) -> Option<u32> {
        self.inner.lock().unwrap().index.get(id).map(|l| l.stored)
    }

    /// Make every object put so far durable. Without new objects this does
    /// not touch the disk.
    pub fn sync(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.check()?;
        inner.sync_active()?;
        if inner.dir_dirty {
            inner.sync_dir(&*self.io)?;
        }
        Ok(())
    }

    /// Seal the active pack (sync, write its index). Call at unmount.
    pub fn close(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.check()?;
        if inner.active.is_some() {
            self.seal(&mut inner)?;
        }
        if inner.dir_dirty {
            inner.sync_dir(&*self.io)?;
        }
        Ok(())
    }

    /// True if GC frees dead space inside packs with holes (the storage can
    /// punch holes). Else GC frees space only by compaction and deletion.
    pub fn punches_holes(&self) -> bool {
        self.punches
    }

    /// Set the zstd level of new objects (0 = no compression). Objects
    /// already stored keep their form: each record says if it is compressed,
    /// so the level needs no record on disk and can change at any time.
    pub fn set_zstd_level(&self, level: i32) {
        self.zstd_level.store(level, Ordering::Relaxed);
    }

    /// The zstd level of new objects (0 = no compression).
    pub fn zstd_level(&self) -> i32 {
        self.zstd_level.load(Ordering::Relaxed)
    }

    /// Current counts.
    pub fn stats(&self) -> Stats {
        self.inner.lock().unwrap().stats
    }

    fn seal(&self, inner: &mut Inner) -> Result<()> {
        inner.sync_active()?;
        let Some(a) = inner.active.take() else { return Ok(()) };
        if let Err(e) = self.io.write_durable(a.num, Kind::Idx, &encode_index(&a.entries)) {
            // The pack is synced; without its index it is scanned at open.
            inner.failed = true;
            return Err(e.into());
        }
        inner.dir_dirty = false; // write_durable synced the directory
        Ok(())
    }
}

fn pack_header(num: u64) -> Vec<u8> {
    pack_header_with(num, 0)
}

fn pack_header_with(num: u64, flags: u16) -> Vec<u8> {
    let mut h = Vec::with_capacity(PACK_HEADER);
    h.extend_from_slice(PACK_MAGIC);
    h.extend_from_slice(&(FORMAT_VERSION | flags).to_le_bytes());
    h.extend_from_slice(&num.to_le_bytes());
    h
}

impl Inner {
    fn empty() -> Self {
        Inner {
            index: HashMap::new(),
            readers: HashMap::new(),
            tick: 0,
            active: None,
            next_pack: 1,
            stats: Stats::default(),
            dir_dirty: false,
            touched: None,
            failed: false,
        }
    }

    fn check(&self) -> Result<()> {
        if self.failed {
            return Err(Error::Io(io::Error::other(
                "an earlier sync of the chunk store failed after retries, so writes are stopped; \
                 data since the last good sync may be lost; unmount and mount the store again",
            )));
        }
        Ok(())
    }

    /// Sync the active pack if it has new bytes. A failure is final.
    fn sync_active(&mut self) -> Result<()> {
        if let Some(a) = self.active.as_mut() {
            if a.dirty {
                if let Err(e) = a.file.sync() {
                    self.failed = true;
                    return Err(e.into());
                }
                a.dirty = false;
            }
        }
        Ok(())
    }

    fn sync_dir(&mut self, io: &dyn PackIo) -> Result<()> {
        if let Err(e) = io.sync_dir() {
            self.failed = true;
            return Err(e.into());
        }
        self.dir_dirty = false;
        Ok(())
    }

    fn touch(&mut self, id: ObjectId) {
        if let Some(t) = self.touched.as_mut() {
            t.insert(id);
        }
    }

    fn add(&mut self, id: ObjectId, l: Loc) {
        if self.index.insert(id, l).is_none() {
            self.stats.objects += 1;
            self.stats.raw_bytes += l.raw as u64;
            self.stats.stored_bytes += l.stored as u64;
        }
    }

    fn reader(&mut self, io: &dyn PackIo, num: u64) -> io::Result<Arc<dyn PackRead>> {
        self.tick += 1;
        if let Some((f, used)) = self.readers.get_mut(&num) {
            *used = self.tick;
            return Ok(Arc::clone(f));
        }
        if self.readers.len() >= MAX_OPEN_PACKS {
            // Close the least recently used pack. A read that holds its
            // handle keeps it open until the read ends.
            let oldest = self.readers.iter().min_by_key(|(_, (_, used))| *used).map(|(n, _)| *n);
            if let Some(n) = oldest {
                self.readers.remove(&n);
            }
        }
        let f = io.open_read(num)?;
        self.readers.insert(num, (Arc::clone(&f), self.tick));
        Ok(f)
    }
}

#[cfg(test)]
mod index_tests {
    use super::*;
    use crate::hash::{Blake3_256, ObjectHasher};

    /// An index with a valid CRC whose entry count is `n` and whose body has
    /// no entries.
    fn index_with_count(n: u64) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(IDX_MAGIC);
        b.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        b.extend_from_slice(&n.to_le_bytes());
        let crc = crc32c::crc32c(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }

    fn record(id: &ObjectId, payload: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        encode_record_into(&mut r, id, 0, payload.len() as u32, payload);
        r
    }

    // A CRC-valid record with an unknown tag: real where a record must start
    // (an error), chance after skipped damage (no record there).
    #[test]
    fn the_resync_scan_treats_an_unknown_tag_by_position() {
        let good_data = b"good record".to_vec();
        let good = Blake3_256.id(&good_data);
        let mut unknown = ObjectId([0u8; ID_LEN]);
        unknown.0[0] = 0xEE; // no hasher has this tag
        unknown.0[1] = 1;
        let odd = record(&unknown, b"looks like a record");

        // Aligned: right after a good record.
        let mut pack = pack_header(1);
        pack.extend(record(&good, &good_data));
        pack.extend(&odd);
        assert!(matches!(resync_scan(&pack, 1), Err(Error::UnknownHash(_))));

        // After damage: skipped, and the good record after it is found.
        let mut pack = pack_header(1);
        pack.extend([0xAB; 5]);
        pack.extend(&odd);
        pack.extend(record(&good, &good_data));
        let found = resync_scan(&pack, 1).expect("an unknown tag after damage is not an error");
        assert_eq!(found.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![good]);
    }

    // #64: an entry count whose byte size overflows is refused (the scan of
    // the pack then rebuilds the index), never a panic or a wrapped length.
    #[test]
    fn an_index_with_an_overflowing_count_is_refused() {
        for n in [1u64 << 63, u64::MAX, u64::MAX / IDX_ENTRY as u64 + 1] {
            assert!(matches!(decode_index(&index_with_count(n), 1), Ok(None)), "count {n}");
        }
        assert!(matches!(decode_index(&index_with_count(0), 1), Ok(Some(v)) if v.is_empty()));
        assert!(matches!(decode_index(&index_with_count(1), 1), Ok(None)), "count 1 with no entry");
    }
}
