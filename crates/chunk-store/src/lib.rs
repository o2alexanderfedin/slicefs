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
//!
//! Index: a memtable of recent entries and sorted run files in
//! `<dir>/index`, with a MANIFEST (see `index.rs`). The store keeps no map of
//! every object in RAM. Open replays only the pack records above the
//! high-water mark of the MANIFEST.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::ops::{Deref, DerefMut, Range};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

mod bloom;
mod extsort;
pub mod file;
pub mod hash;
mod index;
mod locality;
pub mod lock_stats;
#[cfg(test)]
mod locality_tests;
#[cfg(test)]
mod lock_tail_tests;
mod mark;
pub mod pack_io;
#[cfg(test)]
mod race_tests;
pub mod retry;
mod run;
pub mod mem;
pub mod sync;

use crate::lock_stats::Site;
use crate::sync::{Condvar, Mutex, MutexGuard, Rank};
use crate::index::{Counts, Hwm, Index, MergeOut, Snapshot};
use crate::run::Run;
use crate::pack_io::{FileIo, Kind, PackFile, PackIo, PackRead, PackSync, PUNCH_BLOCK, SyncWait};
pub use crate::locality::LocalityStats;
use crate::locality::LocalityCache;
pub use crate::mark::{LiveFilter, LiveSet, LIVE_FILTER_BITS};
use crate::mark::Touched;
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
    /// The index writes its memtable to a new run file when it holds this
    /// many entries (a tuning and test knob; at least 1).
    pub index_memtable_limit: usize,
    /// Bloom filter bits per key of new index runs (about 1 % false
    /// positives at 10); 0 writes runs without a filter. A tuning and test
    /// knob.
    pub index_bloom_bits: u8,
    /// Index runs merge when this many adjacent runs have one size tier
    /// (size-tiered merges; at least 2). A tuning and test knob.
    pub index_merge_fan_in: usize,
    /// Merge index runs on a background thread of the store (a writable
    /// store only). Off: runs merge only through
    /// [`ChunkStore::merge_index_now`] and [`ChunkStore::merge_index_all`]
    /// (tests that need a fixed run list).
    pub index_background_merge: bool,
    /// The idle merge (#111): when the index has had no new entry and no
    /// flush, merge or GC for this long, the merge thread merges the newest
    /// runs (see `index::idle_window`), so lookups check fewer runs. `None`:
    /// no idle merge. Needs `index_background_merge`. A tuning and test knob.
    pub index_idle_merge_after: Option<Duration>,
    /// The objects put or read between `gc_begin` and `gc_finish` are kept
    /// as exact IDs up to this many; more move into a Bloom filter (fixed
    /// RAM; a false positive only keeps a dead object). A tuning and test
    /// knob.
    pub gc_touched_exact: usize,
    /// The directory for the temp files of a full GC (the exact mark; see
    /// [`ChunkStore::gc_finish_full`]). A writable open removes it (files
    /// that a crash left). [`ChunkStore::open`] sets `<dir>/gc-tmp` if
    /// none. None: a full GC fails.
    pub gc_tmp_dir: Option<PathBuf>,
    /// IDs in one sorted chunk of the exact mark (RAM: 33 bytes each). A
    /// tuning and test knob.
    pub gc_sort_chunk_ids: usize,
    /// Most sorted chunks that one merge of the exact mark reads at a time
    /// (open files; 64 KiB of buffer each). A tuning and test knob.
    pub gc_merge_fan_in: usize,
    /// The locality cache of `put` (#112; see `locality.rs`): after a few
    /// dedup hits in a row in one pack that this process sealed, the
    /// entries of its `.idx` answer the next dedup checks from RAM (at most
    /// 65,536 entries). Off: every dedup check goes to the index.
    pub locality_cache: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            pack_limit: 64 << 20,
            zstd_level: 3,
            read_only: false,
            hasher: DEFAULT_HASHER,
            index_memtable_limit: 1 << 18,
            index_bloom_bits: index::DEFAULT_BLOOM_BITS,
            index_merge_fan_in: 4,
            index_background_merge: true,
            index_idle_merge_after: Some(index::IDLE_MERGE_AFTER),
            gc_touched_exact: 1 << 16,
            gc_tmp_dir: None,
            gc_sort_chunk_ids: extsort::SORT_CHUNK_BYTES / ID_LEN,
            gc_merge_fan_in: extsort::MERGE_FAN_IN,
            locality_cache: true,
        }
    }
}

/// The shape of the index, for tests and tools.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexInfo {
    /// Live run files.
    pub runs: u64,
    /// Entries in the runs (tombstones included).
    pub run_entries: u64,
    /// Tombstone entries in the runs.
    pub run_tombstones: u64,
    /// Entries in the memtable (not in a run yet).
    pub memtable_entries: u64,
    /// The high-water mark: every pack record below (pack, offset) is in a
    /// durable run.
    pub hwm_pack: u64,
    pub hwm_offset: u64,
    /// Bytes of the Bloom filters of the runs, in RAM.
    pub filter_bytes: u64,
    /// Bytes of the fence tables of the runs, in RAM.
    pub fence_bytes: u64,
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
/// Bit in the version field of a `.idx` header: the `.idx` is exact, every
/// entry is the current index entry of its ID (see `locality.rs`).
const IDX_EXACT: u16 = 0x4000;
/// The `TRUST` file: magic(4) + floor(8) + CRC(4) (see `locality.rs`).
const TRUST_MAGIC: &[u8; 4] = b"SFTR";
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
/// `gc_finish` changes the selected packs in groups of at most this many
/// packs. Each pending GC run keeps its file open until the group is
/// installed and merged into one run. So a GC holds at most this many
/// pending run files open, plus one merged run for each group done (open
/// until the background merges take it into a larger run), not one file
/// for each pack.
pub const GC_GROUP_PACKS: usize = 32;

/// Paces a GC: [`GcPacer::at_safe_point`] runs at points where the GC holds
/// no store lock and no install is open (in the sweep: before each group of
/// [`GC_GROUP_PACKS`] packs; in the metadata mark: between roots and between
/// files). It may sleep to give way to writes. It must return in bounded
/// time; the GC trusts it to cap its own waits. Offline GC uses [`NoPacer`].
pub trait GcPacer {
    fn at_safe_point(&self);
}

/// A pacer that never waits (offline GC, tests).
pub struct NoPacer;

impl GcPacer for NoPacer {
    fn at_safe_point(&self) {}
}
/// GC copies at most this many records of a compacted pack per hold of the
/// store lock (a 64 MiB pack of small objects has some 100,000 records).
const COPY_BATCH: usize = 1024;
/// Objects smaller than this are not compressed.
const MIN_COMPRESS: usize = 64;

/// The longest wait of a store call for a slow job of another thread (a
/// seal, a pack create, a background sync). At the limit the call fails
/// with an I/O error of kind `TimedOut`; the store is not failed, since
/// nothing was lost.
const STORE_WAIT: Duration = Duration::from_secs(10);

/// The error of a wait that reached `STORE_WAIT`.
fn timed_out(what: &str) -> Error {
    Error::Io(io::Error::new(io::ErrorKind::TimedOut, format!("the chunk store waited {} s for {what}", STORE_WAIT.as_secs())))
}

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
    let v = u16::from_le_bytes([b[4], b[5]]) & !(PACK_PUNCHED | IDX_EXACT);
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

/// A `.idx` of `entries`; `exact` sets the [`IDX_EXACT`] bit (only for a
/// list where each entry is the current index entry of its ID).
fn encode_index(entries: &[(ObjectId, Loc)], exact: bool) -> Vec<u8> {
    let mut sorted: Vec<&(ObjectId, Loc)> = entries.iter().collect();
    sorted.sort_by_key(|(id, _)| *id);
    let mut b = Vec::with_capacity(14 + entries.len() * IDX_ENTRY + 4);
    b.extend_from_slice(IDX_MAGIC);
    let v = if exact { FORMAT_VERSION | IDX_EXACT } else { FORMAT_VERSION };
    b.extend_from_slice(&v.to_le_bytes());
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

/// True if `b` is an intact `.idx` with the [`IDX_EXACT`] bit.
fn idx_exact(b: &[u8]) -> bool {
    if b.len() < 18 || &b[..4] != IDX_MAGIC {
        return false;
    }
    let (body, crc) = b.split_at(b.len() - 4);
    crc32c::crc32c(body) == u32::from_le_bytes(crc.try_into().unwrap())
        && u16::from_le_bytes([b[4], b[5]]) & IDX_EXACT != 0
}

/// The intact `.idx` `b` without the [`IDX_EXACT`] bit (a new CRC).
fn idx_without_exact(b: &[u8]) -> Vec<u8> {
    let mut out = b[..b.len() - 4].to_vec();
    let v = u16::from_le_bytes([out[4], out[5]]) & !IDX_EXACT;
    out[4..6].copy_from_slice(&v.to_le_bytes());
    let crc = crc32c::crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// The trust floor of the store (see `locality.rs`): 0 if there is no
/// `TRUST` file, `None` (unknown: trust nothing, and write it at the next
/// raise) if it is damaged or cannot be read.
fn read_trust_floor(io: &dyn PackIo) -> Option<u64> {
    match io.read(0, Kind::Trust) {
        Ok(b) if b.len() == 16 && &b[..4] == TRUST_MAGIC
            && crc32c::crc32c(&b[..12]) == u32::from_le_bytes(b[12..].try_into().unwrap()) =>
        {
            Some(u64::from_le_bytes(b[4..12].try_into().unwrap()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Some(0),
        _ => None,
    }
}

fn encode_trust_floor(floor: u64) -> Vec<u8> {
    let mut b = TRUST_MAGIC.to_vec();
    b.extend_from_slice(&floor.to_le_bytes());
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
    /// A handle that syncs the pack with no store lock held (`None`: the
    /// file has none; then a sync runs under the lock).
    sync: Option<Arc<dyn PackSync>>,
    /// Appends so far: a sync with no lock clears `dirty` only if none came
    /// after its start.
    published: u64,
}

impl Active {
    fn new(num: u64, file: Box<dyn PackFile>, len: u64, entries: Vec<(ObjectId, Loc)>, dirty: bool) -> Self {
        let sync = file.sync_handle();
        Active { num, file, len, entries, dirty, sync, published: 0 }
    }
}

/// A sync that runs with no store lock (see `ChunkStore::sync`): what it
/// syncs, and the marks that say which flags it may clear after.
struct SyncJob {
    /// The active pack: its number, its `published` count, its handle.
    pack: Option<(u64, u64, Arc<dyn PackSync>)>,
    /// The directory: `Inner::created` at the start.
    dir: Option<u64>,
}

struct Inner {
    /// The memtable and the run files: the same index as
    /// `ChunkStore::index`. Changes happen under the store lock; lookups
    /// also run outside it (see `index.rs`).
    index: Arc<Index>,
    /// Open packs for reads, with the tick of their last use (LRU, at most
    /// `MAX_OPEN_PACKS`).
    readers: HashMap<u64, (Arc<dyn PackRead>, u64)>,
    tick: u64,
    active: Option<Active>,
    next_pack: u64,
    /// The durable trust floor (the `TRUST` file, see `locality.rs`) read at
    /// open; `None` if the open could not read it. After the open
    /// `ChunkStore::trust` has it.
    trust_floor: Option<u64>,
    stats: Stats,
    /// A pack file was created since the last directory sync.
    dir_dirty: bool,
    /// Pack files created so far: a directory sync with no lock clears
    /// `dir_dirty` only if none came after its start.
    created: u64,
    /// While a GC runs: every object put or read since `gc_begin`. GC keeps
    /// them even if its mark did not find them.
    touched: Option<Touched>,
    /// A sync failed. After that, a later sync can succeed without the data
    /// (Linux can mark the pages clean), so the store refuses every write and
    /// sync until it is opened again; the open scan finds the durable state.
    failed: bool,
    /// Told of every flush (see `MergeCtl`; the same as `ChunkStore::merge`).
    merge: Arc<MergeCtl>,
    /// The seal that runs in the background, if any (see `ChunkStore::seal`).
    sealing: Option<Sealing>,
    /// A `put` creates the pack file with this number now, with no store
    /// lock held, and signals the end (see `ChunkStore::prepare_append`).
    /// The number is taken from `next_pack`.
    creating: Option<(u64, SyncWait)>,
    /// A flush runs now (see `ChunkStore::flush_if_full`), and signals its
    /// end: one flush at a time, and no GC install meanwhile.
    flushing: Option<SyncWait>,
}

/// A slow step that `put` does without the store lock before its append
/// (see `ChunkStore::prepare_append`).
enum Before {
    /// Wait for this signal, then look again.
    Wait(SyncWait),
    /// Create the pack file with this number, then install it and signal.
    Create(u64, SyncWait),
}

/// A seal in a background thread, and the signal of its end (a wait on it
/// needs no store lock; the join under the lock then takes no time).
struct Sealing {
    thread: JoinHandle<io::Result<()>>,
    done: SyncWait,
}

/// The slow part of a seal: sync the pack, then write its `.idx` (#108).
struct SealJob {
    io: Arc<dyn PackIo>,
    num: u64,
    file: Box<dyn PackFile>,
    idx: Vec<u8>,
    dirty: bool,
}

impl SealJob {
    fn run(mut self) -> io::Result<()> {
        let t = Instant::now();
        if self.dirty {
            self.file.sync()?;
        }
        let synced = t.elapsed();
        self.io.write_durable(self.num, Kind::Idx, &self.idx)?;
        retry::log(&format!("seal num={} sync_s={:.3} idx_s={:.3} bg=1",
            self.num, synced.as_secs_f64(), (t.elapsed() - synced).as_secs_f64()));
        Ok(())
    }
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
    /// A full GC: the dead objects that it did not remove (in packs that an
    /// object put or read during the GC kept, or whose entry list did not
    /// match the index). `None` for a normal GC.
    pub dead_left: Option<u64>,
    /// A full GC: the merge passes of the sort of its mark (0 if one
    /// sorted chunk held it).
    pub mark_merge_passes: u32,
    /// Packs where GC punched holes over dead records.
    pub packs_punched: u64,
    /// Bytes in the punched ranges.
    pub punched_bytes: u64,
}

/// The mark of a full GC (see [`ChunkStore::gc_finish_full`]): the IDs go
/// to sorted temp files, so RAM does not grow with their count.
pub struct ExactMark {
    sorter: extsort::Sorter<ID_LEN>,
    inserted: u64,
}

impl ExactMark {
    /// Add a live object (a duplicate is fine).
    pub fn insert(&mut self, id: &ObjectId) -> Result<()> {
        self.inserted += 1;
        Ok(self.sorter.push(id.0)?)
    }

    /// The inserts so far (duplicates included).
    pub fn inserted(&self) -> u64 {
        self.inserted
    }
}

/// The mark that a GC gets.
enum Mark<'a> {
    Filter(&'a dyn LiveSet),
    Exact(ExactMark),
}

/// A dead entry of a full GC: the pack (big endian, so the records sort by
/// pack) and the ID.
const DEAD_REC: usize = 8 + ID_LEN;

/// The time of a `gc_finish` and its holds of the store lock (puts and
/// reads wait for the store lock; see [`ChunkStore::last_gc_timing`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcTiming {
    /// The longest hold of the store lock.
    pub lock_hold_max: Duration,
    /// All holds of the store lock together.
    pub lock_hold_total: Duration,
    /// The whole call.
    pub wall: Duration,
}

/// A content-addressed object store in pack files.
pub struct ChunkStore {
    io: Arc<dyn PackIo>,
    /// The strategy can punch holes (asked once, at open).
    punches: bool,
    /// zstd level for new objects (0 = none); starts as `opts.zstd_level`.
    zstd_level: AtomicI32,
    opts: Options,
    /// The index, for lookups outside the store lock (also in `Inner`).
    index: Arc<Index>,
    inner: Arc<Mutex<Inner>>,
    /// Shared with the merge thread (see `MergeCtl`).
    merge: Arc<MergeCtl>,
    /// The merge thread (a writable store with background merges), until
    /// `close` or `Drop` joins it.
    merge_thread: Mutex<Option<JoinHandle<()>>>,
    /// Test hook: `put` and `read` take it and call it once, right after
    /// their lookup outside the store lock and before they take the lock
    /// (see `race_tests.rs`).
    #[cfg(test)]
    after_lookup: Mutex<Option<LookupHook>>,
    /// Test hook: `merge_index_now` and `merge_index_all` take it and call
    /// it once, after the merge output is written and before the install,
    /// with no store lock held (see `race_tests.rs`).
    #[cfg(test)]
    after_merge_write: Mutex<Option<LookupHook>>,
    /// One GC at a time: `gc_begin`, `gc_abort` and the whole `gc_finish`
    /// hold it. Lock order: this lock, then the store lock.
    gc: Mutex<()>,
    /// See [`ChunkStore::last_gc_timing`].
    gc_timing: Mutex<GcTiming>,
    /// The locality cache of `put` (see `locality.rs`).
    locality: LocalityCache,
    /// The durable trust floor (`None`: unknown). Held across the write of
    /// the `TRUST` file, with no store lock (LOCK-01): one writer at a time,
    /// so the file never goes back to a lower floor.
    trust: Mutex<Option<u64>>,
    /// Test hook: `gc_finish` calls it at the start of its pass 1 (with 1)
    /// and of its pass 2 (with 2), with no store lock held (see
    /// `race_tests.rs`).
    #[cfg(test)]
    gc_pass_hook: Mutex<Option<PassHook>>,
    /// Test counter: the times a `put` began to wait for a slow operation of
    /// another thread (a seal, a pack create, a background sync, a flush;
    /// see `lock_tail_tests.rs`).
    #[cfg(test)]
    waits: std::sync::atomic::AtomicUsize,
}

/// The merge state that the store shares with its merge thread.
struct MergeCtl {
    /// One merge at a time: held for a whole merge step. Lock order: this
    /// lock, then the store lock. `state` is a leaf lock: nothing else is
    /// taken while it is held.
    step: Mutex<()>,
    /// Set at `close` and `Drop`: the thread ends, and its merge stops
    /// between two input pages.
    stop: AtomicBool,
    state: Mutex<MergeState>,
    /// Signals a change of `state` (and `stop`).
    cv: Condvar,
}

#[derive(Default)]
struct MergeState {
    /// A flush wrote a run since the thread last looked for a merge.
    pending: bool,
    /// The thread is merging.
    busy: bool,
    /// The thread is alive.
    running: bool,
    /// The idle merge check is due: the thread waits until the index is
    /// idle, then runs the idle merge (see `Options::index_idle_merge_after`).
    idle_check: bool,
}

impl Default for MergeCtl {
    fn default() -> Self {
        MergeCtl {
            step: Mutex::new(Rank::Store, ()),
            stop: AtomicBool::new(false),
            state: Mutex::new(Rank::Store, MergeState::default()),
            cv: Condvar::new(),
        }
    }
}

impl MergeCtl {
    fn state(&self) -> MutexGuard<'_, MergeState> {
        self.state.lock()
    }

    /// A flush wrote a run: the thread looks for a merge.
    fn notify(&self) {
        self.state().pending = true;
        self.cv.notify_all();
    }

    /// Ask the thread to end (under the state lock, so no wakeup is lost).
    fn request_stop(&self) {
        let _s = self.state();
        self.stop.store(true, Ordering::SeqCst);
        self.cv.notify_all();
    }
}

/// Marks the merge thread as gone when it ends, also by a panic, so
/// `wait_index_idle` never waits for it.
struct ThreadExit<'a>(&'a MergeCtl);

impl Drop for ThreadExit<'_> {
    fn drop(&mut self) {
        let mut s = self.0.state();
        s.running = false;
        s.busy = false;
        self.0.cv.notify_all();
    }
}

/// The merge knobs of the options.
#[derive(Clone, Copy)]
struct MergeCfg {
    fan_in: usize,
    memtable_limit: usize,
    idle_after: Option<Duration>,
}

impl MergeCfg {
    fn of(o: &Options) -> Self {
        MergeCfg {
            fan_in: o.index_merge_fan_in.max(2),
            memtable_limit: o.index_memtable_limit.max(1),
            idle_after: o.index_idle_merge_after,
        }
    }
}

/// Which merge a merge step picks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Policy {
    /// Size-tiered (see `Index::pick_merge`).
    Tiered,
    /// Every run into one.
    All,
    /// The idle merge (see `Index::pick_idle_merge`).
    Idle,
}

/// What a merge step needs: the parts of the store that a merge thread
/// shares (never the `ChunkStore` itself, so its `Drop` runs).
#[derive(Clone)]
struct Merger {
    io: Arc<dyn PackIo>,
    inner: Arc<Mutex<Inner>>,
    index: Arc<Index>,
    ctl: Arc<MergeCtl>,
    cfg: MergeCfg,
}

impl Merger {
    /// One merge step (see `Index::pick_merge` and `Index::pick_idle_merge`):
    /// pick the window under the store lock, write the output with no lock
    /// held, install it under the store lock, then remove the inputs.
    /// `between` runs after the write, with no lock held (a test hook).
    ///
    /// Returns false if there was nothing to merge, the window changed, the
    /// store is failed, or `stop` was set. An error changes nothing that a
    /// lookup sees: on a read error of an input the partial output is
    /// removed; if the error is `Corrupt` (a damaged page), a rebuild of the
    /// index is requested, so the next writable open rebuilds it from the
    /// packs, and no merge runs until then. A merge never fails the store:
    /// the runs are derived data.
    fn step(&self, policy: Policy, stop: &AtomicBool, between: &mut dyn FnMut()) -> Result<bool> {
        let (io, index) = (&*self.io, &*self.index);
        let _one = self.ctl.step.lock();
        let plan = {
            let inner = self.inner.lock();
            if inner.failed {
                return Ok(false);
            }
            let plan = match policy {
                Policy::Tiered => index.pick_merge(self.cfg.fan_in, self.cfg.memtable_limit, false),
                Policy::All => index.pick_merge(self.cfg.fan_in, self.cfg.memtable_limit, true),
                Policy::Idle => index.pick_idle_merge(),
            };
            match plan {
                Some(p) => p,
                None => return Ok(false),
            }
        };
        let out = match index.write_merge(io, &plan, stop) {
            Ok(Some(out)) => out,
            Ok(None) => return Ok(false),
            Err(e) => {
                if let Error::Corrupt(m) = &e {
                    let _inner = self.inner.lock();
                    if !index.rebuild_requested() {
                        if let Err(e) = index.request_rebuild(io, m) {
                            tracing::warn!("chunk store: cannot ask for a rebuild of the index: {e}");
                        }
                    }
                }
                return Err(e);
            }
        };
        between();
        let gone = {
            let inner = self.inner.lock();
            if inner.failed {
                drop(inner);
                if let MergeOut::Run(r) = out {
                    let num = r.num();
                    drop(r);
                    let _ = io.remove(num, Kind::Run);
                }
                return Ok(false);
            }
            match index.install_merge(io, &plan, out)? {
                Some(gone) => gone,
                None => return Ok(false),
            }
        };
        Index::remove_runs(io, &gone);
        Ok(true)
    }

    /// The merge thread: wait until a flush notifies, then merge by the
    /// size-tiered policy until it finds no window. Then, with an idle delay
    /// (`MergeCfg::idle_after`) and an idle window, wait until the index is
    /// idle: no new memtable entry and no generation change (flush, merge,
    /// GC) for the whole delay; then run the idle merge. A flush during the
    /// wait starts over with the size-tiered merges; an idle merge that
    /// started runs to its end. End when `stop` is set. A failed merge is
    /// logged and tried again after the next flush; it never fails the
    /// store (the foreground keeps working).
    fn run(self) {
        let ctl = Arc::clone(&self.ctl);
        let _exit = ThreadExit(&ctl);
        loop {
            let Some(policy) = self.next_work(&ctl) else { return };
            let mut failed = false;
            while !ctl.stop.load(Ordering::SeqCst) {
                match self.step(policy, &ctl.stop, &mut || {}) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(e) => {
                        tracing::warn!("chunk store: a background merge of the index failed (tried again after the next flush): {e}");
                        failed = true;
                        break;
                    }
                }
            }
            let mut s = ctl.state();
            s.busy = false;
            // After the size-tiered merges, the idle check (not after a
            // failure: the next flush tries again).
            s.idle_check = policy == Policy::Tiered && !failed && self.cfg.idle_after.is_some();
            drop(s);
            ctl.cv.notify_all();
        }
    }

    /// Wait for the next work of the merge thread and mark it busy; `None`:
    /// `stop` is set.
    fn next_work(&self, ctl: &MergeCtl) -> Option<Policy> {
        let mut s = ctl.state();
        loop {
            if ctl.stop.load(Ordering::SeqCst) {
                return None;
            }
            if s.pending {
                s.pending = false;
                s.busy = true;
                return Some(Policy::Tiered);
            }
            let delay = match self.cfg.idle_after {
                Some(d) if s.idle_check => d,
                _ => {
                    ctl.cv.wait(&mut s);
                    continue;
                }
            };
            // `state` is a leaf lock: look at the runs without it.
            drop(s);
            let due = self.index.idle_merge_due();
            s = ctl.state();
            if s.pending || ctl.stop.load(Ordering::SeqCst) {
                continue;
            }
            if !due {
                s.idle_check = false;
                ctl.cv.notify_all();
                continue;
            }
            // Idle: the memtable size and the generation do not change for
            // the whole delay (both are atomics: `state` stays a leaf lock).
            let mark = (self.index.mem_len(), self.index.gen());
            let until = Instant::now() + delay;
            let mut idle = true;
            loop {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() || s.pending || ctl.stop.load(Ordering::SeqCst) {
                    break;
                }
                ctl.cv.wait_for(&mut s, left);
                if (self.index.mem_len(), self.index.gen()) != mark {
                    idle = false;
                    break;
                }
            }
            if idle && !s.pending && !ctl.stop.load(Ordering::SeqCst) {
                s.idle_check = false;
                s.busy = true;
                return Some(Policy::Idle);
            }
        }
    }
}

/// See `ChunkStore::after_lookup`.
#[cfg(test)]
type LookupHook = Box<dyn FnMut(&ChunkStore) + Send>;

/// See `ChunkStore::gc_pass_hook`: the store and the pass number.
#[cfg(test)]
type PassHook = Box<dyn FnMut(&ChunkStore, u32) + Send>;

impl ChunkStore {
    fn new(io: Arc<dyn PackIo>, punches: bool, opts: Options, inner: Inner) -> Self {
        let trust_floor = inner.trust_floor;
        let floor = trust_floor.unwrap_or(u64::MAX);
        // Packs from here on have no `.idx` yet (the active pack).
        let unsealed = inner.active.as_ref().map_or(inner.next_pack, |a| a.num);
        let s = ChunkStore {
            io,
            punches,
            zstd_level: AtomicI32::new(opts.zstd_level),
            index: Arc::clone(&inner.index),
            merge: Arc::clone(&inner.merge),
            inner: Arc::new(Mutex::new(Rank::Store, inner).timed(Site::Store)),
            merge_thread: Mutex::new(Rank::Store, None),
            opts,
            #[cfg(test)]
            after_lookup: Mutex::new(Rank::Store, None),
            #[cfg(test)]
            after_merge_write: Mutex::new(Rank::Store, None),
            gc: Mutex::new(Rank::Store, ()),
            gc_timing: Mutex::new(Rank::Store, GcTiming::default()),
            locality: LocalityCache::default(),
            trust: Mutex::new(Rank::Store, trust_floor),
            #[cfg(test)]
            gc_pass_hook: Mutex::new(Rank::Store, None),
            #[cfg(test)]
            waits: Default::default(),
        };
        s.locality.forget_below(floor);
        s.locality.sealed_below(unsealed);
        if !s.opts.read_only && s.opts.index_background_merge {
            s.start_merges();
        }
        s
    }

    /// Start the merge thread. It first looks for a merge at once (the runs
    /// of earlier sessions and of the replay at open).
    fn start_merges(&self) {
        {
            let mut st = self.merge.state();
            st.running = true;
            st.pending = true;
        }
        let m = self.merger();
        match std::thread::Builder::new().name("chunk-store-merge".into()).spawn(move || m.run()) {
            Ok(h) => *self.merge_thread.lock() = Some(h),
            Err(e) => {
                // The store works without it: the runs only stay more.
                tracing::warn!("chunk store: cannot start the index merge thread: {e}");
                self.merge.state().running = false;
            }
        }
    }

    /// Stop the merge thread and wait for it (its merge stops between two
    /// input pages and removes its partial output). The caller holds no
    /// store lock: the thread may wait for it.
    fn stop_merges(&self) {
        self.merge.request_stop();
        let h = self.merge_thread.lock().take();
        if let Some(h) = h {
            if h.join().is_err() {
                tracing::warn!("chunk store: the index merge thread panicked");
            }
        }
    }

    /// Wait until the merge thread has no merge to do and is not merging.
    /// Returns at once if background merges are off, the store is
    /// read-only, or the thread has stopped. Does not wait for the idle
    /// merge (see [`ChunkStore::wait_index_settled`]). For tests and
    /// benchmarks.
    pub fn wait_index_idle(&self) {
        let mut s = self.merge.state();
        while s.running && (s.pending || s.busy) {
            self.merge.cv.wait(&mut s);
        }
    }

    /// [`ChunkStore::wait_index_idle`], and also the idle merge (see
    /// [`Options::index_idle_merge_after`]): wait until the thread has no
    /// merge to do and no idle check due, at most `timeout`. True if
    /// settled. Puts during the wait delay the idle merge. For tests and
    /// benchmarks.
    pub fn wait_index_settled(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        let mut s = self.merge.state();
        while s.running && (s.pending || s.busy || s.idle_check) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            self.merge.cv.wait_for(&mut s, left);
        }
        true
    }

    /// The merge test hook (see `after_merge_write`); nothing outside tests.
    fn after_merge_write(&self) {
        #[cfg(test)]
        {
            let hook = self.after_merge_write.lock().take();
            if let Some(mut f) = hook {
                f(self);
            }
        }
    }

    /// Count a wait of a `put` for a slow operation (see `waits`); nothing
    /// outside tests.
    fn note_wait(&self) {
        #[cfg(test)]
        self.waits.fetch_add(1, Ordering::SeqCst);
    }

    /// See `waits`.
    #[cfg(test)]
    pub(crate) fn test_waits(&self) -> usize {
        self.waits.load(Ordering::SeqCst)
    }

    /// The test hook (see `after_lookup`); nothing outside tests.
    fn after_lookup(&self) {
        #[cfg(test)]
        {
            let hook = self.after_lookup.lock().take();
            if let Some(mut f) = hook {
                f(self);
            }
        }
    }

    /// True if the answer of a lookup of `id` made when the generation was
    /// `gen` is still the current one (see `Index::gen`). The caller holds
    /// the store lock.
    fn unchanged(&self, gen: u64, id: &ObjectId) -> bool {
        self.index.gen() == gen && !self.index.mem_has(id)
    }
    /// Open (or create) the store in `dir`. Packs without a valid index are
    /// scanned; the newest unsealed pack becomes the active pack, cut after
    /// its last valid record.
    pub fn open(dir: &Path, mut opts: Options) -> Result<Self> {
        if opts.gc_tmp_dir.is_none() {
            opts.gc_tmp_dir = Some(dir.join("gc-tmp"));
        }
        let files = Arc::new(FileIo::new(dir.join("packs")));
        Self::open_with(Arc::new(retry::RetryingIo::new(files)), opts)
    }

    /// Open the store on a storage strategy (files, a mock, ...).
    ///
    /// Reads the index MANIFEST and the run footers and tables, then replays
    /// only the packs at or above the high-water mark (hwm): packs below it
    /// are not opened. A writable open writes the memtable to a run when it
    /// reaches the limit, only between two packs, with the hwm at the start
    /// of the next pack (the entries of an `.idx` are in ID order, so a mark
    /// inside a pack would be wrong). A read-only open writes nothing.
    ///
    /// The index is derived data. If it cannot be used (damage, the rebuild
    /// flag, see `Index::open`), or if it points to pack bytes that are not
    /// there, the open starts again from an empty index and replays every
    /// pack: a writable open writes a new index, a read-only open keeps it
    /// in RAM.
    pub fn open_with(io: Arc<dyn PackIo>, opts: Options) -> Result<Self> {
        if let (Some(d), false) = (&opts.gc_tmp_dir, opts.read_only) {
            // The temp files of a full GC that a crash ended.
            match std::fs::remove_dir_all(d) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        if !io.prepare(opts.read_only)? {
            return Ok(ChunkStore::new(io, false, opts, Inner::empty()));
        }
        let index = Index::open(&*io, opts.read_only)?;
        let inner = match Self::load(&*io, &opts, index, None)? {
            Loaded::Done(inner) => *inner,
            Loaded::Rebuild { reason, resync } => {
                let index = Index::rebuild(&*io, opts.read_only, &reason)?;
                match Self::load(&*io, &opts, index, resync)? {
                    Loaded::Done(inner) => *inner,
                    // An empty index has no run to damage and no hwm.
                    Loaded::Rebuild { reason, .. } => return Err(Error::Corrupt(reason)),
                }
            }
        };
        let punches = !opts.read_only && io.can_punch();
        Ok(ChunkStore::new(io, punches, opts, inner))
    }

    /// The pack part of `open_with` on the index `index`. `resync` is a pack
    /// to treat as sealed (resynced past damage, never cut) although it is
    /// the newest one: a read-only rebuild after damage below the hwm.
    ///
    /// Returns `Rebuild` before it changes any pack if the index points to
    /// pack bytes that are gone: the hwm pack is missing, or it ends below
    /// the hwm. Those bytes were synced before a run pointed to them, so this
    /// is damage, not a torn tail. A cut there would leave run entries that
    /// point past the end of the pack (a stale dedup hit in `put` loses the
    /// object), and later appends would land below the hwm (a replay skips
    /// them, so a crash loses them although they were synced). A writable
    /// open first makes the pack sealed: resynced past the damage, synced,
    /// and with a durable `.idx`, so no later open cuts it. A `Corrupt`
    /// lookup during the replay (a damaged run page) also returns `Rebuild`.
    fn load(io: &dyn PackIo, opts: &Options, mut index: Index, resync: Option<u64>) -> Result<Loaded> {
        index.set_bloom_bits(opts.index_bloom_bits);
        let mut nums = io.list()?;
        nums.sort_unstable();
        let hwm = index.hwm();
        let c = index.counts();
        if hwm.offset > 0 && nums.binary_search(&hwm.pack).is_err() {
            let reason = format!("pack {} of the index high-water mark is missing", hwm.pack);
            return Ok(Loaded::Rebuild { reason, resync: None });
        }

        let mut inner = Inner {
            index: Arc::new(index),
            readers: HashMap::new(),
            tick: 0,
            active: None,
            // A new pack must never get a number that the hwm covers.
            next_pack: nums.last().map_or(1, |n| n + 1).max(hwm.pack).max(1),
            trust_floor: read_trust_floor(io),
            stats: Stats {
                objects: c.objects,
                raw_bytes: c.raw_bytes,
                stored_bytes: c.stored_bytes,
                packs: nums.len() as u64,
            },
            dir_dirty: false,
            created: 0,
            touched: None,
            failed: false,
            merge: Arc::new(MergeCtl::default()),
            sealing: None,
            creating: None,
            flushing: None,
        };
        let limit = opts.index_memtable_limit.max(1);
        let last = nums.last().copied();
        for num in nums.into_iter().filter(|&n| n >= hwm.pack) {
            let loaded = match io.read(num, Kind::Idx) {
                Ok(b) => decode_index(&b, num)?,
                Err(_) => None,
            };
            let gap = |e: &[(ObjectId, Loc)]| num == hwm.pack && !covers(e, hwm.offset);
            let missing = || format!("pack {num} lacks records that the index points to, below its high-water mark at {}", hwm.offset);
            let mut active = None;
            let entries = match loaded {
                // A sealed pack: nothing to write, also after the repair below.
                Some(e) if gap(&e) => return Ok(Loaded::Rebuild { reason: missing(), resync: None }),
                Some(e) => e,
                None => {
                    let buf = io.read(num, Kind::Pack)?;
                    if buf.starts_with(PACK_MAGIC) {
                        check_version(&buf, "pack", num)?;
                    }
                    let is_active = Some(num) == last && !is_punched(&buf) && resync != Some(num);
                    // The active pack is cut at its first bad record. A sealed
                    // pack (its index is lost) may have holes or damage in the
                    // middle, so it is resynced past them.
                    let (entries, end) = if is_active { scan_pack(&buf, num) } else { (resync_scan(&buf, num)?, 0) };
                    if gap(&entries) {
                        let reason = missing();
                        // The pack becomes sealed first, with a durable
                        // `.idx`: no later open cuts it at the damage, and an
                        // open after a failure in the rebuild finds the same
                        // gap through the `.idx` and starts it again.
                        if !opts.read_only {
                            let entries = if is_active { resync_scan(&buf, num)? } else { entries };
                            io.open_rw(num)?.sync()?;
                            io.write_durable(num, Kind::Idx, &encode_index(&entries, false))?;
                        }
                        return Ok(Loaded::Rebuild { reason, resync: Some(num) });
                    }
                    if is_active && !opts.read_only {
                        // The newest pack without an index is the active pack.
                        let mut file = io.open_rw(num)?;
                        let valid = if end < PACK_HEADER as u64 {
                            file.set_len(0)?;
                            file.write_at(&pack_header(num), 0)?;
                            PACK_HEADER as u64
                        } else {
                            file.set_len(end)?;
                            end
                        };
                        file.sync()?;
                        active = Some((file, valid));
                    } else if !opts.read_only {
                        // A run may point into this pack after the next
                        // flush: its bytes must be durable first.
                        io.open_rw(num)?.sync()?;
                        io.write_durable(num, Kind::Idx, &encode_index(&entries, false))?;
                    }
                    entries
                }
            };
            match inner.replay(&entries, hwm) {
                Ok(()) => {}
                Err(Error::Corrupt(reason)) => return Ok(Loaded::Rebuild { reason, resync: None }),
                Err(e) => return Err(e),
            }
            if let Some((file, len)) = active {
                // The pack keeps all its entries (for its `.idx` at seal);
                // only the memtable skips those below the hwm.
                inner.active = Some(Active::new(num, file, len, entries, false));
                continue;
            }
            if !opts.read_only && inner.index.mem_len() >= limit {
                // Between two packs: every record of the packs up to `num`
                // is in the memtable or in a run.
                inner.flush_at(io, Hwm { pack: num + 1, offset: 0 })?;
            }
        }
        Ok(Loaded::Done(Box::new(inner)))
    }

    /// Store `data` and return its ID. Bytes already in the store are not
    /// written again.
    ///
    /// An index lookup that fails (for example a damaged run page) counts as
    /// "not found": the object is written again, and its new entry hides the
    /// damaged page. The stats then count the object as new, so they can
    /// count it twice until the index is rebuilt (see
    /// [`ChunkStore::request_index_rebuild`]).
    ///
    /// The dedup lookup (with its run page read) runs outside the store
    /// lock. A GC can run between that lookup and the lock: a hit can then
    /// be an object that the GC removed, and a write skipped on it would
    /// lose the object. So under the lock the answer is used only if the
    /// index generation is the same and the memtable has no entry of the
    /// ID (see `Index::gen`); else the lookup runs again under the lock.
    ///
    /// The locality cache (see `locality.rs`) can answer the lookup instead
    /// of the index. Its hit takes the same path as an index hit: it is
    /// touched (for a running GC) and checked under the lock in the same
    /// way.
    pub fn put(&self, data: &[u8]) -> Result<ObjectId> {
        if self.opts.read_only {
            return Err(Error::Io(io::Error::other("the chunk store is open read-only")));
        }
        let id = self.opts.hasher.id(data);
        // A failed index read counts as "not found" in every check: the
        // object is written again, and its new memtable entry hides the run
        // that could not be read. Its stats then count it as new.
        let gen = self.index.gen();
        let found = self.dedup_lookup(&id)?;
        self.after_lookup();
        if found {
            let mut inner = self.inner.lock();
            loop {
                inner.touch(id);
                if self.unchanged(gen, &id) {
                    return Ok(id);
                }
                match self.locked_lookup(&inner, &id) {
                    Ok(true) => return Ok(id),
                    Ok(false) => break,
                    Err(floor) => {
                        drop(inner);
                        self.raise_trust_floor(floor)?;
                        inner = self.inner.lock();
                    }
                }
            }
            drop(inner);
            // GC removed the object after the lookup: write it again.
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
                let mut inner = self.inner.lock();
                loop {
                    inner.touch(id);
                    // Another put of the same bytes, or a flush, merge or GC,
                    // since the lookup: look up again.
                    if !self.unchanged(gen, &id) {
                        match self.locked_lookup(&inner, &id) {
                            Ok(true) => return Ok(id),
                            Ok(false) => {}
                            Err(floor) => {
                                drop(inner);
                                self.raise_trust_floor(floor)?;
                                inner = self.inner.lock();
                                continue;
                            }
                        }
                    }
                    inner.check()?;
                    // The append needs a slow operation (a pack create, or a
                    // wait for a seal or a background sync): do it without
                    // the store lock, then start again (LOCK-01).
                    match self.prepare_append(&mut inner, s.record.len()) {
                        None => break,
                        Some(Before::Wait(w)) => {
                            drop(inner);
                            self.note_wait();
                            w.wait();
                            inner = self.inner.lock();
                        }
                        Some(Before::Create(num, done)) => {
                            drop(inner);
                            let made = self.create_pack(num);
                            inner = self.inner.lock();
                            if let Some(spare) = self.install_pack(&mut inner, num, made, &done)? {
                                drop(inner);
                                drop(spare);
                                // Best effort: an empty pack below the active
                                // one is only removed by a later GC.
                                let _ = self.io.remove(num, Kind::Pack);
                                inner = self.inner.lock();
                            }
                        }
                    }
                }
                let loc = self.append(&mut inner, &id, &s.record, stored, data.len() as u32, flags)?;
                inner.add_new(id, loc);
                drop(inner);
                self.flush_if_full()?;
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

    /// The dedup check of `put` outside the store lock: the locality cache,
    /// then the index. An index hit can start a fill of the cache (a read
    /// of one exact `.idx`, outside every lock). A failed index lookup
    /// counts as "not found" after it moved the trust floor (an error if it
    /// could not).
    fn dedup_lookup(&self, id: &ObjectId) -> Result<bool> {
        if self.opts.locality_cache && self.locality.get(id).is_some() {
            return Ok(true);
        }
        let loc = match self.index.get(id) {
            Ok(Some(loc)) => loc,
            Ok(None) => return Ok(false),
            Err(_) => {
                let floor = self.inner.lock().next_pack;
                self.raise_trust_floor(floor)?;
                return Ok(false);
            }
        };
        if !self.opts.locality_cache {
            return Ok(true);
        }
        if let Some(epoch) = self.locality.index_hit(loc.pack) {
            self.fill_locality(loc.pack, epoch);
        }
        Ok(true)
    }

    /// Read the `.idx` of `pack` into the locality cache if it is exact
    /// and not too big (see `locality.rs`).
    fn fill_locality(&self, pack: u64, epoch: u64) {
        match self.io.file_len(pack, Kind::Idx) {
            Ok(n) if n > locality::max_idx_len(14, IDX_ENTRY) => return self.locality.refuse(pack),
            Ok(_) => {}
            Err(_) => return self.locality.fill_failed(),
        }
        let b = match self.io.read(pack, Kind::Idx) {
            Ok(b) => b,
            Err(_) => return self.locality.fill_failed(),
        };
        if !idx_exact(&b) {
            return self.locality.refuse(pack);
        }
        match decode_index(&b, pack) {
            Ok(Some(entries)) => self.locality.fill(pack, epoch, entries),
            _ => self.locality.refuse(pack),
        }
    }

    /// The index lookup of `put` under the store lock: `Ok(found)`. A failed
    /// lookup counts as "not found" after the trust floor is at least
    /// `next_pack` (durable): the object is written again, so a pack may then
    /// hold an older copy that a later GC does not select when it removes
    /// the new copy, and its `.idx` would still list the object (see
    /// `locality.rs`). If the floor is lower, `Err(floor)`: the caller drops
    /// the store lock, calls `raise_trust_floor(floor)` and looks again.
    fn locked_lookup(&self, inner: &Inner, id: &ObjectId) -> std::result::Result<bool, u64> {
        match inner.index.get(id) {
            Ok(l) => Ok(l.is_some()),
            Err(_) => {
                let floor = inner.next_pack;
                // A busy lock is a floor write now: raise it after it.
                let durable = self.trust.try_lock().is_some_and(|t| t.is_some_and(|f| f >= floor));
                if durable { Ok(false) } else { Err(floor) }
            }
        }
    }

    /// Before `put` writes an object again after a failed lookup: no pack
    /// below `floor` (the `next_pack` of the store at the failure: every
    /// pack that exists then) is trusted again, in this process or a later
    /// one. The new floor is durable before the caller writes (see
    /// `locality.rs`). The caller holds no store lock (LOCK-01).
    fn raise_trust_floor(&self, floor: u64) -> Result<()> {
        self.locality.forget_below(floor);
        let mut durable = self.trust.lock();
        if durable.is_none_or(|f| floor > f) {
            self.io.write_durable(0, Kind::Trust, &encode_trust_floor(floor))?;
            *durable = Some(floor);
        }
        Ok(())
    }

    /// The counters of the locality cache of `put` (see
    /// [`Options::locality_cache`]).
    pub fn locality_stats(&self) -> LocalityStats {
        self.locality.stats()
    }

    /// What `append` of a record of `len` bytes would do under the store
    /// lock that is slow, if anything (LOCK-01): create the pack file (no
    /// active pack), wait for the seal of the previous pack (this record
    /// fills the active pack), or wait for a background sync at `KEEP_CAP`
    /// (see `retry.rs`). `put` does it without the lock and asks again; the
    /// GC copies, under the lock, do not ask (they are not on the put and
    /// read paths). For a create, the number is taken here and `creating`
    /// is set: one create at a time, and other puts wait for it.
    fn prepare_append(&self, inner: &mut Inner, len: usize) -> Option<Before> {
        let Some(a) = inner.active.as_ref() else {
            if let Some((_, w)) = &inner.creating {
                return Some(Before::Wait(w.clone()));
            }
            let num = inner.next_pack;
            inner.next_pack += 1;
            let done = SyncWait::default();
            inner.creating = Some((num, done.clone()));
            return Some(Before::Create(num, done));
        };
        if a.len + len as u64 >= self.opts.pack_limit {
            // After its signal the seal thread only returns: the join under
            // the lock then takes no time.
            if let Some(s) = inner.sealing.as_ref().filter(|s| !s.done.is_done()) {
                return Some(Before::Wait(s.done.clone()));
            }
        }
        a.file.write_wait(len).map(Before::Wait)
    }

    /// A new pack file `num` with its header (no store lock held).
    fn create_pack(&self, num: u64) -> Result<Box<dyn PackFile>> {
        let mut file = self.io.create(num)?;
        file.write_at(&pack_header(num), 0)?;
        Ok(file)
    }

    /// Under the store lock after `create_pack`: end the create (wake the
    /// puts that wait for it) and make the file the active pack. A GC copy
    /// may have made an active pack meanwhile (under the lock); then the new
    /// file is returned, for the caller to remove without the lock. Its
    /// number is not used again.
    fn install_pack(
        &self,
        inner: &mut Inner,
        num: u64,
        made: Result<Box<dyn PackFile>>,
        done: &SyncWait,
    ) -> Result<Option<Box<dyn PackFile>>> {
        inner.creating = None;
        done.done();
        let file = made?;
        if inner.active.is_some() {
            return Ok(Some(file));
        }
        inner.active = Some(Active::new(num, file, PACK_HEADER as u64, Vec::new(), true));
        inner.stats.packs += 1;
        inner.dir_dirty = true;
        inner.created += 1;
        Ok(None)
    }

    /// Append an encoded record to the active pack (made if there is none),
    /// and seal the pack when it is full. Returns where the record is.
    ///
    /// The record write stays under the store lock (D-14): it is a pwrite to
    /// the page cache, not a disk wait. The offset of a record is the end of
    /// the pack, given out under this lock; a write outside it could leave a
    /// hole before a later record on a crash, and the pack scans (`covers`,
    /// `scan_pack`, `resync_scan`) assume a pack has no such hole.
    fn append(&self, inner: &mut Inner, id: &ObjectId, rec: &[u8], stored: u32, raw: u32, flags: u8) -> Result<Loc> {
        if inner.active.is_none() {
            let num = inner.next_pack;
            inner.next_pack += 1;
            let mut file = self.io.create(num)?;
            file.write_at(&pack_header(num), 0)?;
            inner.active = Some(Active::new(num, file, PACK_HEADER as u64, Vec::new(), true));
            inner.stats.packs += 1;
            inner.dir_dirty = true;
            inner.created += 1;
        }
        let a = inner.active.as_mut().unwrap();
        a.file.write_at(rec, a.len)?;
        let loc = Loc { pack: a.num, offset: a.len, stored, raw, flags };
        a.len += rec.len() as u64;
        a.entries.push((*id, loc));
        a.dirty = true;
        a.published += 1;
        if a.len >= self.opts.pack_limit {
            self.seal(inner)?;
        }
        Ok(loc)
    }

    /// Start a GC: seal the active pack, so every object so far is in a
    /// sealed pack, and record from now on each object that is put or read.
    /// Returns the last pack number that the GC may change.
    pub fn gc_begin(&self) -> Result<u64> {
        let _one = self.gc.lock();
        let mut inner = self.lock_no_create();
        inner.check()?;
        if inner.active.as_ref().is_some_and(|a| !a.entries.is_empty()) {
            self.seal(&mut inner)?;
        }
        // GC reads the `.idx` of every pack up to the one it returns.
        inner.join_seal()?;
        inner.touched = Some(Touched::spilling(self.opts.gc_touched_exact));
        Ok(inner.next_pack - 1)
    }

    /// The store lock, with no pack create in flight: a pack whose number
    /// is taken but whose file is not installed yet would be at or below the
    /// boundary of `gc_begin` with no entries, so `gc_select` would take it
    /// as unused and remove it. The wait for the create runs without the
    /// store lock.
    fn lock_no_create(&self) -> MutexGuard<'_, Inner> {
        loop {
            let inner = self.inner.lock();
            let Some(w) = inner.creating.as_ref().map(|c| c.1.clone()) else { return inner };
            drop(inner);
            w.wait();
        }
    }

    /// Start a full GC (see [`ChunkStore::gc_finish_full`]): as
    /// [`ChunkStore::gc_begin`], but the objects put or read from now on go
    /// to an exact set in temp files (bounded RAM), not to a Bloom filter.
    pub fn gc_begin_full(&self) -> Result<u64> {
        let dir = self.gc_tmp()?;
        let _one = self.gc.lock();
        let mut inner = self.lock_no_create();
        inner.check()?;
        if inner.active.as_ref().is_some_and(|a| !a.entries.is_empty()) {
            self.seal(&mut inner)?;
        }
        inner.join_seal()?;
        inner.touched = Some(Touched::on_disk(extsort::Sorter::new(dir, "touched", self.sort_cfg())));
        Ok(inner.next_pack - 1)
    }

    /// An empty exact mark for [`ChunkStore::gc_finish_full`]: its IDs go
    /// to sorted temp files in `Options::gc_tmp_dir`.
    pub fn exact_mark(&self) -> Result<ExactMark> {
        Ok(ExactMark { sorter: extsort::Sorter::new(self.gc_tmp()?, "live", self.sort_cfg()), inserted: 0 })
    }

    fn gc_tmp(&self) -> Result<Arc<extsort::TmpDir>> {
        let Some(d) = &self.opts.gc_tmp_dir else {
            return Err(Error::Io(io::Error::new(io::ErrorKind::Unsupported, "a full GC needs Options::gc_tmp_dir")));
        };
        Ok(Arc::new(extsort::TmpDir::new(d)?))
    }

    fn sort_cfg(&self) -> extsort::SortCfg {
        extsort::SortCfg { chunk_records: self.opts.gc_sort_chunk_ids, fan_in: self.opts.gc_merge_fan_in }
    }

    /// End a GC without deleting anything (for example after a failed mark).
    pub fn gc_abort(&self) {
        let _one = self.gc.lock();
        self.inner.lock().touched = None;
    }

    /// End a GC: remove every object of the packs up to `boundary` that is
    /// not in `live` and was not put or read since `gc_begin`. A pack with no
    /// live object is deleted; a pack with less than half of its bytes live,
    /// or a small pack, is compacted (its live records are copied to the
    /// active pack, which is synced before the old pack is deleted); a pack
    /// with at least half of its bytes live gets holes over its dead records
    /// (if the storage can punch). A pack that the index no longer uses at
    /// all is deleted.
    ///
    /// The steps:
    /// 1. Start (store lock): the touched set so far decides with `live`; a
    ///    new touched set records the objects put or read from now on. A
    ///    snapshot of the index: a copy of the memtable and the run list.
    /// 2. Pass 1, no store lock: one merged pass over the snapshot gives
    ///    the totals of each pack (bytes, kept bytes, entries).
    /// 3. The decision for each pack, from its totals.
    ///
    /// Steps 4 to 6 run for one group of at most [`GC_GROUP_PACKS`]
    /// selected packs at a time (in pack order), so that the pending runs
    /// of only one group are open at a time. The packs that the index no
    /// longer uses go with the last group.
    /// 4. Pass 2, no store lock, one selected pack at a time: its entries
    ///    from its `.idx` (or a scan of the pack), each checked against the
    ///    snapshot (current only if the snapshot points to this record). The
    ///    tombstones of the victims go to a pending run on disk that no
    ///    MANIFEST lists yet. The live records of a pack to compact are
    ///    copied: a batch is read with no lock, then appended under the
    ///    store lock.
    /// 5. Install (store lock): the plan of a pack that holds an object put
    ///    or read during the GC (also during an earlier group) is dropped
    ///    (that pack does not change). The copies are synced; one MANIFEST
    ///    write lists the pending runs of the kept plans as the newest runs,
    ///    and the memtable in a run under them. This write is the durable
    ///    point of the group.
    /// 6. Only then, with no store lock, one pack at a time: delete, or
    ///    punch (its remaining entries are read again from the index). Then
    ///    the runs of the install are merged into one run, which closes the
    ///    files of the pending runs.
    ///
    /// No pack of a group changes and no tombstone of it becomes durable
    /// unless every index read of steps 2, 4 and 5 for it succeeded. A read
    /// error (also a damaged run page, which also asks for a rebuild of the
    /// index) ends the call with the error before the install of its group:
    /// the pending runs of that group are removed, and nothing of that group
    /// or of a later group changed except the copies (duplicates with newer,
    /// valid places). The groups before it are done: each one is a complete
    /// GC of its packs. An index read error is never taken as "not current".
    ///
    /// RAM: the totals of each pack, the entries of one pack, the memtable
    /// copy, and the filter and fences of the pending runs; no map of every
    /// object. `live` is the caller's (a [`LiveFilter`]: 1.25 bytes for each
    /// object); the touched set before this call moves into a filter of the
    /// same size after `Options::gc_touched_exact` IDs; the touched set
    /// during this call is exact. Cost: one sequential pass over the runs, and one verified
    /// snapshot lookup per `.idx` entry of each selected pack (two for a
    /// punched pack). Each store-lock hold is bounded by a batch of copies
    /// (`COPY_BATCH` records), by the memtable limit (a flush, the install),
    /// or by the objects touched during the GC; see
    /// [`ChunkStore::last_gc_timing`].
    pub fn gc_finish(&self, live: &impl LiveSet, boundary: u64) -> Result<GcStats> {
        self.gc_finish_mark(Mark::Filter(live), boundary, &NoPacer)
    }

    /// [`ChunkStore::gc_finish`], with `pacer` called before each group of
    /// packs (no store lock held then).
    pub fn gc_finish_paced(&self, live: &impl LiveSet, boundary: u64, pacer: &dyn GcPacer) -> Result<GcStats> {
        self.gc_finish_mark(Mark::Filter(live), boundary, pacer)
    }

    /// End a full GC (started with [`ChunkStore::gc_begin_full`]): as
    /// [`ChunkStore::gc_finish`], but with an exact mark and no holes, so it
    /// removes every dead object of the packs up to `boundary`.
    ///
    /// - The mark `live` and the objects put or read since `gc_begin_full`
    ///   are sorted in temp files (chunks of `Options::gc_sort_chunk_ids`
    ///   IDs, then merges of at most `Options::gc_merge_fan_in` chunks at a
    ///   time) into one sorted file of distinct IDs.
    /// - Pass 1 joins the index (in ID order) with that file: no false
    ///   positive. The dead entries go, as (pack, ID), to a second sorted
    ///   temp file.
    /// - Each pack with a dead record is compacted (or deleted if it has no
    ///   live record), never punched; pass 2 takes the dead IDs of one pack
    ///   at a time from the second file.
    ///
    /// RAM does not grow with the object count: one sort chunk, or the read
    /// buffers of one merge (64 KiB each), plus what `gc_finish` needs. The
    /// temp files are removed at the end (and at a writable open after a
    /// crash). A pack that holds an object put or read during the call does
    /// not change, as in `gc_finish`; `GcStats::dead_left` counts the dead
    /// objects left so.
    pub fn gc_finish_full(&self, live: ExactMark, boundary: u64) -> Result<GcStats> {
        self.gc_finish_mark(Mark::Exact(live), boundary, &NoPacer)
    }

    /// [`ChunkStore::gc_finish_full`], with `pacer` called before each group
    /// of packs (no store lock held then).
    pub fn gc_finish_full_paced(&self, live: ExactMark, boundary: u64, pacer: &dyn GcPacer) -> Result<GcStats> {
        self.gc_finish_mark(Mark::Exact(live), boundary, pacer)
    }

    fn gc_finish_mark(&self, mark: Mark<'_>, boundary: u64, pacer: &dyn GcPacer) -> Result<GcStats> {
        let start = Instant::now();
        let _one = self.gc.lock();
        let holds = Holds::default();
        let r = self.gc_run(mark, boundary, &holds, pacer);
        *self.gc_timing.lock() =
            GcTiming { lock_hold_max: holds.max.get(), lock_hold_total: holds.total.get(), wall: start.elapsed() };
        r
    }

    /// The store-lock holds and the time of the last `gc_finish` (zero
    /// before the first one).
    pub fn last_gc_timing(&self) -> GcTiming {
        *self.gc_timing.lock()
    }

    /// The store lock, with the time it is held added to `holds`.
    fn held<'a>(&'a self, holds: &'a Holds) -> Held<'a> {
        let g = self.inner.lock();
        Held { g, at: Instant::now(), holds }
    }

    /// The GC test hook (see `gc_pass_hook`); nothing outside tests.
    fn gc_pass(&self, pass: u32) {
        #[cfg(test)]
        {
            let hook = self.gc_pass_hook.lock().take();
            if let Some(mut f) = hook {
                f(self, pass);
                let mut slot = self.gc_pass_hook.lock();
                if slot.is_none() {
                    *slot = Some(f);
                }
            }
        }
        #[cfg(not(test))]
        let _ = pass;
    }

    fn gc_run(&self, mark: Mark<'_>, boundary: u64, holds: &Holds, pacer: &dyn GcPacer) -> Result<GcStats> {
        // (1) Start. The touched set ends with this GC, also when it fails
        // (#63): the next GC starts its own set in `gc_begin`, before its
        // gate commit, so its mark sees everything put before that.
        let (touched, snap, active) = {
            let mut inner = self.held(holds);
            let touched = inner.touched.take().unwrap_or_else(Touched::exact);
            inner.check()?;
            inner.touched = Some(Touched::exact());
            (touched, inner.index.snapshot(), inner.active.as_ref().map(|a| a.num))
        };
        let mut st = GcStats::default();
        let live = match mark {
            Mark::Filter(live) => {
                let keep = |id: &ObjectId| live.may_contain(id) || touched.contains(id);
                // (2) and (3).
                let (todo, unused) =
                    match self.gc_select(&snap, &mut |id, _| Ok(keep(id)), false, boundary, active, &mut st) {
                        Ok(d) => d,
                        Err(e) => return Err(self.gc_abandon(e, &[], holds)),
                    };
                return self.gc_groups(&snap, &keep, &mut |_| Ok(None), todo, unused, holds, pacer, st);
            }
            Mark::Exact(live) => live,
        };

        // A full GC. The touched set so far joins the mark, and all of it is
        // sorted.
        let sorted = (|| -> Result<_> {
            let mut sorter = live.sorter;
            match touched.into_exact().map_err(|e| Error::Io(io::Error::other(e)))? {
                Ok(t) => sorter.absorb(t)?,
                Err(ids) => {
                    for id in ids {
                        sorter.push(id.0)?;
                    }
                }
            }
            let live = sorter.finish()?;
            let dead = extsort::Sorter::<DEAD_REC>::new(self.gc_tmp()?, "dead", self.sort_cfg());
            Ok((live, dead))
        })();
        let (live, mut dead) = match sorted {
            Ok(x) => x,
            Err(e) => return Err(self.gc_abandon(e, &[], holds)),
        };
        // (2) and (3): a merge join of the index with the sorted mark; the
        // dead entries go to `dead`, as (pack, ID).
        let mut dead_n = 0u64;
        let selected = (|| -> Result<_> {
            let mut cur = extsort::Cursor::new(live.reader()?)?;
            let mut keep = |id: &ObjectId, l: &Loc| -> Result<bool> {
                let k = cur.seek(&id.0)?;
                if !k {
                    let mut r = [0u8; DEAD_REC];
                    r[..8].copy_from_slice(&l.pack.to_be_bytes());
                    r[8..].copy_from_slice(&id.0);
                    dead.push(r)?;
                    dead_n += 1;
                }
                Ok(k)
            };
            self.gc_select(&snap, &mut keep, true, boundary, active, &mut st)
        })();
        let ((todo, unused), dead) = match selected.and_then(|sel| Ok((sel, dead.finish()?))) {
            Ok(x) => x,
            Err(e) => return Err(self.gc_abandon(e, &[], holds)),
        };
        let mut dc = match dead.reader().and_then(extsort::Cursor::new) {
            Ok(c) => c,
            Err(e) => return Err(self.gc_abandon(e.into(), &[], holds)),
        };
        // The dead IDs of pack `num` (the packs come in ascending order).
        let mut dead_of = |num: u64| -> Result<Option<HashSet<ObjectId>>> {
            let mut set = HashSet::new();
            while let Some(r) = dc.peek() {
                let p = u64::from_be_bytes(r[..8].try_into().unwrap());
                if p > num {
                    break;
                }
                if p == num {
                    set.insert(ObjectId(r[8..].try_into().unwrap()));
                }
                dc.advance()?;
            }
            Ok(Some(set))
        };
        // Only for the objects touched during this call (few): a read error
        // counts as "not live", which keeps their packs.
        let keep = |id: &ObjectId| live.contains(&id.0).unwrap_or(false);
        let mut st = self.gc_groups(&snap, &keep, &mut dead_of, todo, unused, holds, pacer, st)?;
        st.dead_left = Some(dead_n.saturating_sub(st.removed_objects));
        st.mark_merge_passes = live.passes();
        Ok(st)
    }

    /// Steps 4 to 6 of a GC over the selected packs `todo` and the unused
    /// packs. `dead_of(num)` gives the dead IDs of pack `num` (a full GC; the
    /// calls come in pack order), or `None`: `keep` decides.
    #[allow(clippy::too_many_arguments)]
    fn gc_groups(
        &self,
        snap: &Snapshot,
        keep: &dyn Fn(&ObjectId) -> bool,
        dead_of: &mut dyn FnMut(u64) -> Result<Option<HashSet<ObjectId>>>,
        todo: Vec<GcTodo>,
        unused: Vec<u64>,
        holds: &Holds,
        pacer: &dyn GcPacer,
        mut st: GcStats,
    ) -> Result<GcStats> {
        if todo.is_empty() && unused.is_empty() {
            self.held(holds).touched = None;
            return Ok(st);
        }

        // (4) to (6), one group of at most `GC_GROUP_PACKS` packs at a time:
        // the pending runs of a group are open until its install and merge,
        // so a GC over many packs holds few files open. The unused packs go
        // with the last group.
        let groups: Vec<&[GcTodo]> = if todo.is_empty() { vec![&[]] } else { todo.chunks(GC_GROUP_PACKS).collect() };
        let last = groups.len() - 1;
        let mut touched_packs = HashSet::new();
        let r = (|| -> Result<()> {
            for (g, group) in groups.iter().enumerate() {
                // A safe point: no store lock, no install open. (The objects
                // put or read while it waits go to the touched set, so their
                // packs do not change.)
                pacer.at_safe_point();
                let unused: &[u64] = if g == last { &unused } else { &[] };
                self.gc_group(snap, keep, dead_of, group, unused, g == last, &mut touched_packs, holds, &mut st)?;
            }
            Ok(())
        })();
        // The touched set ends with this GC, also after an error.
        self.held(holds).touched = None;
        r.map(|()| st)
    }

    /// Steps 4 to 6 of `gc_finish` for one group of selected packs (and,
    /// with the last group, the unused packs). `touched_packs` collects,
    /// over the groups, the packs of the objects put or read during the GC;
    /// such a pack does not change. The touched set records again after the
    /// install of a group that is not the last one.
    #[allow(clippy::too_many_arguments)]
    fn gc_group(
        &self,
        snap: &Snapshot,
        keep: &dyn Fn(&ObjectId) -> bool,
        dead_of: &mut dyn FnMut(u64) -> Result<Option<HashSet<ObjectId>>>,
        group: &[GcTodo],
        unused: &[u64],
        last: bool,
        touched_packs: &mut HashSet<u64>,
        holds: &Holds,
        st: &mut GcStats,
    ) -> Result<()> {
        let io = &*self.io;
        let mut pending = Vec::new();

        // Before any change of this group (copies, tombstones): its packs
        // lose the exact mark of their `.idx`, durably (see `locality.rs`).
        let mine = group.iter().map(|t| t.0).chain(unused.iter().copied());
        if let Err(e) = self.gc_distrust(mine) {
            return Err(self.gc_abandon(e, &pending, holds));
        }

        // (4)
        let mut plans = match self.gc_prepare(snap, keep, dead_of, group, holds, &mut pending, st) {
            Ok(p) => p,
            Err(e) => return Err(self.gc_abandon(e, &pending, holds)),
        };

        // (5) The install. Not while a flush runs: its frozen run must be
        // older than the tombstones of this install (see `install_gc`), so
        // wait for it, without the store lock.
        let (added, before) = {
            let mut inner = loop {
                let inner = self.held(holds);
                let Some(w) = inner.flushing.clone() else { break inner };
                drop(inner);
                w.wait();
            };
            let during = inner.touched.take().unwrap_or_else(Touched::exact);
            if !last {
                inner.touched = Some(Touched::exact());
            }
            // An object put or read during the GC: its pack does not change
            // (in this group or in a later one).
            let checked = (|| -> Result<()> {
                for id in during.ids().iter().filter(|id| !keep(id)) {
                    if let Some(l) = snap.get_verified(id)? {
                        touched_packs.insert(l.pack);
                    }
                }
                inner.check()
            })();
            for p in plans.iter_mut().filter(|p| touched_packs.contains(&p.num)) {
                p.dropped = true;
            }            if let Err(e) = checked {
                drop(inner);
                drop(plans);
                return Err(self.gc_abandon(e, &pending, holds));
            }
            let mut after = inner.stats;
            for p in plans.iter().filter(|p| !p.dropped) {
                after.objects -= p.victims;
                after.raw_bytes -= p.raw;
                after.stored_bytes -= p.stored;
            }
            // A failure from here on fails the store (the MANIFEST may be
            // durable). The pending runs stay: an open removes them if no
            // MANIFEST lists them.
            inner.sync_active()?;
            inner.sync_dir(io)?;
            let runs: Vec<Arc<Run>> = plans.iter().filter(|p| !p.dropped).filter_map(|p| p.run.clone()).collect();
            let before: HashSet<u64> = inner.index.runs().iter().map(|r| r.num()).collect();
            let mut added = Vec::new();
            // Whenever a pack changes, also with nothing to add: the hwm
            // must move past the packs that change.
            if plans.iter().any(|p| !p.dropped) || !unused.is_empty() {
                // The `.idx` of a pack that changes can list removed objects
                // from now on (see `locality.rs`); before the install, under
                // the store lock, so no `put` that sees the new generation
                // can get them from the cache.
                self.locality.untrust(plans.iter().filter(|p| !p.dropped).map(|p| p.num).chain(unused.iter().copied()));
                let hwm = inner.hwm_end();
                let counts = Counts { objects: after.objects, raw_bytes: after.raw_bytes, stored_bytes: after.stored_bytes };
                match inner.index.install_gc(io, runs, hwm, counts) {
                    Ok(nums) => added = nums,
                    Err(e) => {
                        inner.failed = true;
                        return Err(e);
                    }
                }
                inner.merge.notify();
            }
            inner.stats = after;
            (added, before)
        };
        let dropped: Vec<u64> = plans.iter_mut().filter(|p| p.dropped).filter_map(|p| p.run.take()).map(|r| r.num()).collect();
        self.remove_pending(&dropped);
        for p in &mut plans {
            p.run = None;
            if p.dropped {
                st.kept_objects += p.entries;
                continue;
            }
            st.kept_objects += p.entries - p.victims;
            st.removed_objects += p.victims;
            st.removed_bytes += p.stored;
            match p.act {
                GcAct::Delete => st.packs_deleted += 1,
                GcAct::Compact => st.packs_compacted += 1,
                GcAct::Punch => {}
            }
        }
        // (6) Only now change the packs. An error returns: the tombstones
        // are durable, and a later GC removes the packs that are left.
        let changed = (|| -> Result<()> {
            let kept = || plans.iter().filter(|p| !p.dropped);
            for p in kept().filter(|p| p.act == GcAct::Punch) {
                self.gc_punch(p.num, holds, st)?;
            }
            for num in kept().filter(|p| p.act != GcAct::Punch).map(|p| p.num).chain(unused.iter().copied()) {
                let _ = io.remove(num, Kind::Idx);
                io.remove(num, Kind::Pack)?;
                let mut inner = self.held(holds);
                inner.readers.remove(&num);
                inner.stats.packs -= 1;
            }
            self.held(holds).sync_dir(io)
        })();
        // One run for the runs of this install, so a GC over many packs
        // does not leave one small run per pack, and the files of the
        // pending runs of this group close.
        if !added.is_empty() {
            self.gc_merge_added(&before, holds);
        }
        changed
    }

    /// Steps 2 and 3 of `gc_finish`: the totals and the decisions. Returns
    /// the selected packs (number, action, current entries) in pack order,
    /// and the packs that the index no longer uses.
    fn gc_select(
        &self,
        snap: &Snapshot,
        keep: &mut dyn FnMut(&ObjectId, &Loc) -> Result<bool>,
        full: bool,
        boundary: u64,
        active: Option<u64>,
        st: &mut GcStats,
    ) -> Result<(Vec<GcTodo>, Vec<u64>)> {
        let io = &*self.io;
        // (2) The totals of each pack, from one complete pass.
        self.gc_pass(1);
        let mut totals: BTreeMap<u64, PackTotals> = BTreeMap::new();
        for item in snap.iter_all() {
            let (id, l) = item?;
            if l.pack > boundary || Some(l.pack) == active {
                continue;
            }
            let t = totals.entry(l.pack).or_default();
            let n = rec_end(&l) - l.offset;
            t.bytes += n;
            t.entries += 1;
            if keep(&id, &l)? {
                t.keep_bytes += n;
            }
        }

        // (3) The decisions. A pack where everything is kept and that is
        // not small stays; so does a mostly live pack if holes cannot be
        // punched (its dead records stay in the index, so a later GC still
        // counts their bytes). A full GC never punches: it compacts every
        // pack with a dead record.
        let mut todo = Vec::new();
        for (&num, t) in &totals {
            let small = t.bytes < self.opts.pack_limit / 8;
            let act = if t.keep_bytes == t.bytes && !small {
                None
            } else if !full && t.keep_bytes > 0 && t.keep_bytes * 2 >= t.bytes && !small {
                self.punches.then_some(GcAct::Punch)
            } else if t.keep_bytes > 0 {
                Some(GcAct::Compact)
            } else {
                Some(GcAct::Delete)
            };
            // A full GC also compacts a pack with dead bytes that the index
            // no longer lists: a punched pack (the holes and the parts of
            // its removed records outside whole blocks), or a pack with
            // more records than current entries (an older copy of an
            // object). A read error ends the GC before any change.
            let act = match act {
                None if full && self.has_unlisted_bytes(num, t.entries)? => Some(GcAct::Compact),
                a => a,
            };
            match act {
                Some(a) => todo.push((num, a, t.entries)),
                None => st.kept_objects += t.entries,
            }
        }
        // Packs that the index no longer uses at all (older copies). This
        // needs the complete pass. No pack at or below the boundary gets
        // new entries: new records go to the active pack of the start or
        // to packs above the boundary.
        let mut unused: Vec<u64> =
            io.list()?.into_iter().filter(|&n| n <= boundary && Some(n) != active && !totals.contains_key(&n)).collect();
        unused.sort_unstable();
        Ok((todo, unused))
    }

    /// True if the sealed pack `num` with `entries` current entries holds
    /// bytes that are not in a current record (see `gc_select`).
    fn has_unlisted_bytes(&self, num: u64, entries: u64) -> Result<bool> {
        let mut head = [0u8; PACK_HEADER];
        self.io.open_read(num)?.read_at(&mut head, 0)?;
        Ok(is_punched(&head) || self.pack_entries(num)?.len() as u64 != entries)
    }

    /// Step 4 of `gc_finish` for one group of selected packs: the plans
    /// with their pending runs and copies. Any error ends it (the caller
    /// removes the runs in `pending`).
    #[allow(clippy::too_many_arguments)]
    fn gc_prepare(
        &self,
        snap: &Snapshot,
        keep: &dyn Fn(&ObjectId) -> bool,
        dead_of: &mut dyn FnMut(u64) -> Result<Option<HashSet<ObjectId>>>,
        group: &[GcTodo],
        holds: &Holds,
        pending: &mut Vec<u64>,
        st: &mut GcStats,
    ) -> Result<Vec<GcPlan>> {
        let io = &*self.io;
        // (4) One selected pack at a time.
        self.gc_pass(2);
        let mut plans = Vec::with_capacity(group.len());
        for &(num, act, entries) in group {
            let dead = dead_of(num)?;
            let keep = |id: &ObjectId| match &dead {
                Some(d) => !d.contains(id),
                None => keep(id),
            };
            let mut current = Vec::new();
            for (id, l) in self.pack_entries(num)? {
                if matches!(snap.get_verified(&id)?, Some(c) if c.pack == num && c.offset == l.offset) {
                    current.push((id, l));
                }
            }
            if current.len() as u64 != entries {
                // The entry list of the pack does not have every entry that
                // the index has: the pack must not change.
                tracing::warn!(
                    "chunk store: GC keeps pack {num}: its entry list has {} of the {entries} entries that the index has",
                    current.len()
                );
                st.kept_objects += entries;
                continue;
            }
            let (victims, copies) = if act == GcAct::Punch {
                let v = punch_victims(&current, &keep);
                if v.is_empty() {
                    st.kept_objects += entries;
                    continue;
                }
                (v, Vec::new())
            } else {
                current.into_iter().partition(|(id, _)| !keep(id))
            };
            let run = if victims.is_empty() {
                None
            } else {
                let n = self.held(holds).index.take_run_num();
                pending.push(n);
                let mut tombs: Vec<(ObjectId, u64)> = victims.iter().map(|(id, _)| (*id, num)).collect();
                tombs.sort_unstable_by_key(|(id, _)| *id);
                Some(Arc::new(self.index.write_pending(io, n, &tombs)?))
            };
            if !copies.is_empty() {
                // The copies get newer places in the memtable; a flush at
                // the size limit is fine: no pack changed yet. A batch of
                // records is read with no lock (the pack is sealed, and only
                // GC changes it), then appended under one short hold.
                let file = self.held(holds).reader(io, num)?;
                let mut recs = Vec::new();
                for batch in copies.chunks(COPY_BATCH) {
                    recs.clear();
                    for (_, l) in batch {
                        let mut rec = vec![0u8; (rec_end(l) - l.offset) as usize];
                        file.read_at(&mut rec, l.offset)?;
                        recs.push(rec);
                    }
                    let mut inner = self.held(holds);
                    inner.check()?;
                    for ((id, l), rec) in batch.iter().zip(&recs) {
                        let nl = self.append(&mut inner, id, rec, l.stored, l.raw, l.flags)?;
                        inner.index.insert(*id, nl);
                    }
                    drop(inner);
                    self.flush_if_full()?;
                }
            }
            plans.push(GcPlan {
                num,
                act,
                run,
                victims: victims.len() as u64,
                raw: victims.iter().map(|(_, l)| l.raw as u64).sum(),
                stored: victims.iter().map(|(_, l)| l.stored as u64).sum(),
                entries,
                dropped: false,
            });
        }
        Ok(plans)
    }

    /// The end of a GC group that failed before its install: nothing of
    /// this group changed but the copies (the groups before it are done).
    /// A damaged run page asks for a rebuild of the index.
    fn gc_abandon(&self, e: Error, pending: &[u64], holds: &Holds) -> Error {
        {
            let mut inner = self.held(holds);
            inner.touched = None;
            if let Error::Corrupt(m) = &e {
                if !self.opts.read_only && !inner.index.rebuild_requested() {
                    if let Err(e) = inner.index.request_rebuild(&*self.io, m) {
                        tracing::warn!("chunk store: cannot ask for a rebuild of the index: {e}");
                    }
                }
            }
        }
        self.remove_pending(pending);
        e
    }

    /// Remove pending GC runs that no MANIFEST lists (best effort: a
    /// writable open removes what is left).
    fn remove_pending(&self, nums: &[u64]) {
        for &num in nums {
            match self.io.remove(num, Kind::Run) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    tracing::warn!("chunk store: cannot remove the pending GC run {num:016x}: {e}")
                }
                _ => {}
            }
        }
    }

    /// A GC is about to change `nums` (copies, tombstones, punches,
    /// removals): drop them from the locality cache, and write each exact
    /// `.idx` of them again without the exact bit, durably, before the
    /// first change. A crash then never leaves an exact `.idx` that lists an
    /// object that the GC removed or copied (see `locality.rs`). An error
    /// ends the GC group before it changed anything.
    fn gc_distrust(&self, nums: impl Iterator<Item = u64> + Clone) -> Result<()> {
        self.locality.untrust(nums.clone());
        for num in nums {
            match self.io.read(num, Kind::Idx) {
                Ok(b) if idx_exact(&b) => self.io.write_durable(num, Kind::Idx, &idx_without_exact(&b))?,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// The entries of a sealed pack: its `.idx`, or a scan of the pack if
    /// the `.idx` is missing or damaged. A read error is returned.
    fn pack_entries(&self, num: u64) -> Result<Vec<(ObjectId, Loc)>> {
        match self.io.read(num, Kind::Idx) {
            Ok(b) => {
                if let Some(e) = decode_index(&b, num)? {
                    return Ok(e);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        resync_scan(&self.io.read(num, Kind::Pack)?, num)
    }

    /// Merge the runs that one GC install added into one run (see
    /// `Merger::step`; a failure is only logged: the runs are derived data).
    /// These are the runs at the head of the list that were not in it
    /// before the install (`before`): the runs of the install, and the runs
    /// that a flush or a background merge of them put there since. They are
    /// adjacent, also when a background merge took some of them first.
    /// Takes no merge lock: a merge that runs at the same time is seen at
    /// the install, which checks that the window is still in the list.
    fn gc_merge_added(&self, before: &HashSet<u64>, holds: &Holds) {
        let io = &*self.io;
        let plan = {
            let inner = self.held(holds);
            if inner.failed {
                return;
            }
            let nums: Vec<u64> =
                inner.index.runs().iter().map(|r| r.num()).take_while(|n| !before.contains(n)).collect();
            match inner.index.plan_merge_of(&nums) {
                Some(p) => p,
                None => return,
            }
        };
        let never = AtomicBool::new(false);
        let r = (|| -> Result<()> {
            let Some(out) = self.index.write_merge(io, &plan, &never)? else { return Ok(()) };
            let gone = {
                let inner = self.held(holds);
                if inner.failed {
                    drop(inner);
                    if let MergeOut::Run(r) = out {
                        let num = r.num();
                        drop(r);
                        let _ = io.remove(num, Kind::Run);
                    }
                    return Ok(());
                }
                inner.index.install_merge(io, &plan, out)?
            };
            if let Some(gone) = gone {
                Index::remove_runs(io, &gone);
            }
            Ok(())
        })();
        if let Err(e) = r {
            if let Error::Corrupt(m) = &e {
                let inner = self.held(holds);
                if !inner.index.rebuild_requested() {
                    if let Err(e) = inner.index.request_rebuild(io, m) {
                        tracing::warn!("chunk store: cannot ask for a rebuild of the index: {e}");
                    }
                }
            }
            tracing::warn!("chunk store: the merge of the runs of a GC failed (a later merge does it): {e}");
        }
    }

    /// Step 6 for a mostly live pack, after the tombstones of its victims
    /// are durable. Its remaining entries come from its `.idx` and lookups
    /// in the index (the victims now read as removed); the holes are the
    /// whole blocks in the gaps between them that hold a removed record.
    /// A lookup error skips the punch (the tombstones are durable; the dead
    /// bytes stay until a later GC).
    ///
    /// Order: (1) write the pack's `.idx` durably with the remaining
    /// entries; (2) mark the pack as punched in its header and sync; (3)
    /// punch. A crash at any point leaves an `.idx` that lists only bytes
    /// that are still there, and a punched pack is resynced at open, never
    /// cut.
    fn gc_punch(&self, num: u64, holds: &Holds, st: &mut GcStats) -> Result<()> {
        let io = &*self.io;
        let skip = |e: &Error| {
            tracing::warn!("chunk store: GC does not punch pack {num} ({e}); its dead bytes stay until a later GC");
        };
        let entries = match self.pack_entries(num) {
            Ok(e) => e,
            Err(e) => {
                skip(&e);
                return Ok(());
            }
        };
        let end = entries.iter().map(|(_, l)| rec_end(l)).max().unwrap_or(0);
        let mut rest = Vec::new();
        let mut dead = Vec::new();
        for (id, l) in entries {
            match self.index.get_verified(&id) {
                Ok(Some(c)) if c.pack == num && c.offset == l.offset => rest.push((id, l)),
                Ok(_) => dead.push(l),
                Err(e) => {
                    if let Error::Corrupt(m) = &e {
                        let inner = self.held(holds);
                        if !inner.index.rebuild_requested() {
                            if let Err(e) = inner.index.request_rebuild(io, m) {
                                tracing::warn!("chunk store: cannot ask for a rebuild of the index: {e}");
                            }
                        }
                    }
                    skip(&e);
                    return Ok(());
                }
            }
        }
        let mut ranges = punch_ranges(rest.iter().map(|(_, l)| l).collect(), end);
        // Only the gaps with a removed record (not the holes of earlier GCs).
        ranges.retain(|&(a, n)| dead.iter().any(|l| hits(l, a, n)));
        if ranges.is_empty() {
            return Ok(());
        }
        let marked = (|| -> io::Result<()> {
            io.write_durable(num, Kind::Idx, &encode_index(&rest, true))?;
            let mut f = io.open_rw(num)?;
            f.write_at(&pack_header_with(num, PACK_PUNCHED), 0)?;
            f.sync()
        })();
        if let Err(e) = marked {
            self.held(holds).failed = true;
            return Err(e.into());
        }
        match io.punch(num, &ranges) {
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
    /// Errors: `Missing`, `Corrupt` (a damaged record, or a damaged index
    /// entry), `UnknownHash` (a tag this build does not know: a damaged or
    /// newer store), `Io`.
    ///
    /// The lookup (with its run page read) runs outside the store lock.
    /// Under the lock, the place is used only if the index generation is
    /// the same and the memtable has no entry of the ID; else it is looked
    /// up again (a GC may have moved the object and deleted its old pack).
    /// A cached pack handle is taken under the lock. A pack that is not
    /// cached is opened without the lock (LOCK-01); then, under the lock
    /// again, the place must still be the current one (else the read starts
    /// over at the new place), so the pack was still there.
    pub fn read(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let hasher = hasher_by_tag(id.tag()).ok_or(Error::UnknownHash(*id))?;
        let gen = self.index.gen();
        // An index read error is returned, never taken as "missing".
        let looked = self.index.get(id);
        self.after_lookup();
        let loc = looked?.ok_or(Error::Missing(*id))?;
        let (loc, file) = {
            let mut inner = self.inner.lock();
            let mut loc = if self.unchanged(gen, id) { loc } else { inner.index.get(id)?.ok_or(Error::Missing(*id))? };
            inner.touch(*id);
            loop {
                if let Some(file) = inner.cached_reader(loc.pack) {
                    break (loc, file);
                }
                // Open the pack without the store lock (LOCK-01). Then the
                // place must still be the current one: a GC may have moved
                // the object and removed the pack meanwhile (it changes the
                // index before it removes a pack).
                let seen = self.index.gen();
                drop(inner);
                let opened = self.io.open_read(loc.pack);
                inner = self.inner.lock();
                let now = if self.unchanged(seen, id) { loc } else { inner.index.get(id)?.ok_or(Error::Missing(*id))? };
                if now != loc {
                    loc = now;
                    continue;
                }
                match opened {
                    Ok(file) => break (loc, inner.add_reader(loc.pack, file)),
                    Err(e) => {
                        drop(inner);
                        return Err(self.check_entry(id, e.into()));
                    }
                }
            }
        };
        Self::read_record(id, hasher, loc, &*file).map_err(|e| self.check_entry(id, e))
    }

    /// The record of `id` at `loc` in `file`, checked: its CRC, its ID and
    /// the hash of its bytes.
    fn read_record(id: &ObjectId, hasher: &dyn ObjectHasher, loc: Loc, file: &dyn PackRead) -> Result<Vec<u8>> {
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

    /// `err` came from the record that the index points to (no record
    /// there, a short read, a missing pack, a wrong ID or hash). Lookups do
    /// not check the CRC of the run page (see `run.rs`), so the entry
    /// itself may be damaged: look it up again through the verified page
    /// reads. If the run page is damaged, ask for a rebuild of the index
    /// (the next writable open rebuilds it from the packs, where the bytes
    /// of the object still are) and return that damage.
    fn check_entry(&self, id: &ObjectId, err: Error) -> Error {
        let suspect = match &err {
            Error::Corrupt(_) => true,
            Error::Io(e) => matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::UnexpectedEof),
            _ => false,
        };
        if !suspect {
            return err;
        }
        let inner = self.inner.lock();
        match inner.index.get_verified(id) {
            Err(Error::Corrupt(m)) => {
                if !self.opts.read_only && !inner.index.rebuild_requested() {
                    if let Err(e) = inner.index.request_rebuild(&*self.io, &m) {
                        tracing::warn!("chunk store: cannot ask for a rebuild of the index: {e}");
                    }
                }
                Error::Corrupt(m)
            }
            _ => err,
        }
    }

    /// The hasher of new objects.
    pub fn hasher(&self) -> &'static dyn ObjectHasher {
        self.opts.hasher
    }

    /// True if the store has `id` (false also if the index cannot be read).
    /// A point-in-time answer: the lookup runs outside the store lock.
    pub fn contains(&self, id: &ObjectId) -> bool {
        matches!(self.index.get(id), Ok(Some(_)))
    }

    /// Stored (possibly compressed) payload length of `id` (`None` also if
    /// the index cannot be read). A point-in-time answer, as `contains`.
    pub fn stored_len(&self, id: &ObjectId) -> Option<u32> {
        self.index.get(id).ok().flatten().map(|l| l.stored)
    }

    /// The shape of the index now.
    pub fn index_info(&self) -> IndexInfo {
        let _inner = self.inner.lock();
        let ix = &self.index;
        let hwm = ix.hwm();
        let runs = ix.runs();
        IndexInfo {
            runs: runs.len() as u64,
            run_entries: runs.iter().map(|r| r.entries()).sum(),
            run_tombstones: runs.iter().map(|r| r.tombstones()).sum(),
            memtable_entries: ix.mem_len() as u64,
            hwm_pack: hwm.pack,
            hwm_offset: hwm.offset,
            filter_bytes: runs.iter().map(|r| r.filter_bytes()).sum(),
            fence_bytes: runs.iter().map(|r| r.fence_bytes()).sum(),
        }
    }

    /// Bytes of the index merge outputs installed since the open (the write
    /// cost of the merges). For tests and tools.
    pub fn index_merge_bytes(&self) -> u64 {
        self.index.merge_bytes()
    }

    /// The entry counts of the live runs (tombstones included), from the
    /// newest run to the oldest. For tests and tools.
    pub fn index_run_entries(&self) -> Vec<u64> {
        self.index.runs().iter().map(|r| r.entries()).collect()
    }

    /// One merge step of the index runs by the size-tiered policy (see
    /// [`Options::index_merge_fan_in`]); false if no window is due. For tests
    /// and tools: a writable store also merges in the background (see
    /// [`Options::index_background_merge`]). An error is returned but does
    /// not fail the store (see `Merger::step`). Refused on a read-only store.
    pub fn merge_index_now(&self) -> Result<bool> {
        self.merge_index(Policy::Tiered)
    }

    /// One step of the idle merge (#111; see
    /// [`Options::index_idle_merge_after`]) now, with no wait for idle;
    /// false if it has no window. For tests and tools; refused on a
    /// read-only store.
    pub fn merge_index_idle(&self) -> Result<bool> {
        self.merge_index(Policy::Idle)
    }

    /// Merge every index run into one (also one run alone, if it has
    /// tombstones), so the result has no tombstone; false if there is
    /// nothing to do. It rewrites the whole index. For tests and tools;
    /// refused on a read-only store.
    pub fn merge_index_all(&self) -> Result<bool> {
        self.merge_index(Policy::All)
    }

    fn merge_index(&self, policy: Policy) -> Result<bool> {
        if self.opts.read_only {
            return Err(Error::Io(io::Error::other("the chunk store is open read-only")));
        }
        let never = AtomicBool::new(false);
        self.merger().step(policy, &never, &mut || self.after_merge_write())
    }

    fn merger(&self) -> Merger {
        Merger {
            io: Arc::clone(&self.io),
            inner: Arc::clone(&self.inner),
            index: Arc::clone(&self.index),
            ctl: Arc::clone(&self.merge),
            cfg: MergeCfg::of(&self.opts),
        }
    }

    /// Ask for a rebuild of the index from the packs: the next writable open
    /// removes the run files and the MANIFEST and replays every pack; a
    /// read-only open replays every pack into RAM. For damage that is found
    /// in a run after the open (a lookup that gives `Corrupt`). Writes the
    /// rebuild flag into a durable MANIFEST; the index in use does not
    /// change.
    pub fn request_index_rebuild(&self) -> Result<()> {
        if self.opts.read_only {
            return Err(Error::Io(io::Error::other("the chunk store is open read-only")));
        }
        let inner = self.inner.lock();
        inner.index.request_rebuild(&*self.io, "a caller asked for a rebuild")
    }

    /// Write the memtable to a run when it has reached the limit. The
    /// caller holds no store lock (LOCK-01).
    ///
    /// One flush at a time (`Inner::flushing`; a second one waits for the
    /// first without the lock). The steps:
    /// 1. Without the lock, wait for the background seal and a background
    ///    sync of the active pack (started here), as `sync` does.
    /// 2. Under the lock: join the ended seal, sync the rest of the active
    ///    pack (the bytes put since step 1) and the directory, take the hwm
    ///    and the counts, and freeze the memtable (`Index::freeze`): lookups
    ///    still see it, new entries go to an empty memtable.
    /// 3. Without the lock: write the run, then the MANIFEST
    ///    (`Index::install_frozen`, under the MANIFEST lock).
    ///
    /// A GC install waits for a flush in step 1 to 3, so the frozen run is
    /// installed before the tombstones of a GC. A failure is final.
    fn flush_if_full(&self) -> Result<()> {
        let limit = self.opts.index_memtable_limit.max(1);
        let (done, waits) = loop {
            let mut inner = self.inner.lock();
            if inner.index.mem_len() < limit {
                return Ok(());
            }
            inner.check()?;
            if let Some(w) = inner.flushing.clone() {
                drop(inner);
                self.note_wait();
                w.wait();
                continue;
            }
            let done = SyncWait::default();
            inner.flushing = Some(done.clone());
            let seal = inner.sealing.as_ref().map(|s| s.done.clone());
            let soon = inner.active.as_mut().filter(|a| a.dirty).and_then(|a| a.file.sync_soon());
            break (done, [seal, soon]);
        };
        for w in waits.iter().flatten() {
            w.wait();
        }
        let r = self.flush_frozen();
        let mut inner = self.inner.lock();
        inner.flushing = None;
        done.done();
        if r.is_err() {
            inner.failed = true;
        }
        r
    }

    /// Steps 2 and 3 of `flush_if_full`.
    fn flush_frozen(&self) -> Result<()> {
        let (entries, num, hwm, counts) = {
            // A seal that started after step 1 is waited for without the lock.
            let mut inner = loop {
                let inner = self.inner.lock();
                let Some(w) = inner.sealing.as_ref().filter(|s| !s.done.is_done()).map(|s| s.done.clone()) else {
                    break inner;
                };
                drop(inner);
                w.wait();
            };
            inner.check()?;
            inner.sync_active()?;
            if inner.dir_dirty {
                inner.sync_dir(&*self.io)?;
            }
            let hwm = inner.hwm_end();
            let s = inner.stats;
            let counts = Counts { objects: s.objects, raw_bytes: s.raw_bytes, stored_bytes: s.stored_bytes };
            let (entries, num) = inner.index.freeze();
            (entries, num, hwm, counts)
        };
        let run = self.index.write_frozen(&*self.io, num, &entries)?;
        self.index.install_frozen(&*self.io, run, hwm, counts)?;
        self.merge.notify();
        Ok(())
    }

    /// Make every object put so far durable. Without new objects this does
    /// not touch the disk.
    ///
    /// No fsync runs under the store lock (LOCK-01), so puts and reads go
    /// on meanwhile (#108):
    /// 1. Start the sync of the active pack in the background, and wait for
    ///    it and for a background seal with no lock.
    /// 2. Wait (with no lock) for a seal that started since, and under the
    ///    lock join it and take the handle of the active pack and the marks
    ///    (see `SyncJob`). With no lock, sync the pack through the handle,
    ///    then the directory. Under the lock again, set `failed` on an
    ///    error, else clear the flags that nothing changed since the marks.
    ///
    /// A pack with no sync handle is synced under the lock (a rare
    /// fallback, `Inner::sync_locked_fallback`). GC, `gc_begin` and `close`
    /// still sync under the lock; they are not on the put and read paths.
    pub fn sync(&self) -> Result<()> {
        let (seal, wait) = {
            let mut inner = self.inner.lock();
            inner.check()?;
            let seal = inner.sealing.as_ref().map(|s| s.done.clone());
            (seal, inner.active.as_mut().filter(|a| a.dirty).and_then(|a| a.file.sync_soon()))
        };
        for w in seal.iter().chain(&wait) {
            w.wait();
        }
        let job = {
            let mut inner = self.lock_no_seal()?;
            inner.check()?;
            inner.join_seal()?;
            match inner.sync_job() {
                Some(job) => job,
                None => return inner.sync_locked_fallback(&*self.io),
            }
        };
        self.run_sync(job)
    }

    /// The store lock, with no background seal in flight: a seal that runs
    /// is waited for with no lock (at most `STORE_WAIT`), so the join under
    /// the lock then takes no time.
    fn lock_no_seal(&self) -> Result<MutexGuard<'_, Inner>> {
        loop {
            let inner = self.inner.lock();
            let Some(w) = inner.sealing.as_ref().filter(|s| !s.done.is_done()).map(|s| s.done.clone()) else {
                return Ok(inner);
            };
            drop(inner);
            if !w.wait_timeout(STORE_WAIT) {
                return Err(timed_out("a pack seal"));
            }
        }
    }

    /// Run a `SyncJob` with no store lock; then, under the lock, set
    /// `failed` on an error, or clear the flags by its marks.
    fn run_sync(&self, job: SyncJob) -> Result<()> {
        if job.pack.is_none() && job.dir.is_none() {
            return Ok(());
        }
        let r = (|| -> io::Result<()> {
            if let Some((_, _, h)) = &job.pack {
                h.sync()?;
            }
            if job.dir.is_some() {
                self.io.sync_dir()?;
            }
            Ok(())
        })();
        let mut inner = self.inner.lock();
        if let Err(e) = r {
            inner.failed = true;
            return Err(e.into());
        }
        if let (Some((num, published, _)), Some(a)) = (&job.pack, inner.active.as_mut()) {
            if a.num == *num && a.published == *published {
                a.dirty = false;
            }
        }
        if job.dir.is_some_and(|c| c == inner.created) {
            inner.dir_dirty = false;
        }
        Ok(())
    }

    /// Seal the active pack (sync, write its index). Call at unmount. Stops
    /// the background merges first (`merge_index_now` still works). Does
    /// not flush the memtable: an open replays the records above the hwm.
    pub fn close(&self) -> Result<()> {
        self.stop_merges();
        let mut inner = self.inner.lock();
        inner.check()?;
        if inner.active.is_some() {
            self.seal(&mut inner)?;
        }
        inner.join_seal()?;
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
        self.inner.lock().stats
    }

    /// An empty GC mark for this store: a [`LiveFilter`] for as many keys
    /// as the store has objects (the live objects are not more), at
    /// [`LIVE_FILTER_BITS`] bits each. Fill it and give it to
    /// [`ChunkStore::gc_finish`].
    pub fn live_filter(&self) -> LiveFilter {
        LiveFilter::new(self.stats().objects)
    }

    ///
    /// The pack sync and the `.idx` write run in a background thread, so a
    /// put that fills a pack does not wait for them (#108); `sync_active`
    /// (so `sync`, a flush and GC) and the next seal wait for it. A crash
    /// before it ends leaves a pack with no `.idx` below the newest pack:
    /// the open scans it (`resync_scan`) and seals it then. Nothing durable
    /// refers to its objects yet, since every sync waits for the seal.
    fn seal(&self, inner: &mut Inner) -> Result<()> {
        inner.join_seal()?;
        let Some(a) = inner.active.take() else { return Ok(()) };
        // The directory is not marked clean: a pack made after this starts
        // may not be in the directory sync of the `.idx` write.
        let num = a.num;
        let job = SealJob { io: Arc::clone(&self.io), num: a.num, file: a.file, idx: encode_index(&a.entries, true), dirty: a.dirty };
        let slot = Arc::new(Mutex::new(Rank::Store, Some(job)));
        let theirs = Arc::clone(&slot);
        let done = SyncWait::default();
        let end = done.clone();
        let spawned = std::thread::Builder::new().name("pack-seal".into()).spawn(move || {
            let job = theirs.lock().take();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.map_or(Ok(()), SealJob::run)))
                .unwrap_or_else(|_| Err(io::Error::other("the pack seal panicked")));
            end.done();
            r
        });
        match spawned {
            Ok(thread) => inner.sealing = Some(Sealing { thread, done }),
            Err(_) => {
                // No thread: seal here.
                let job = slot.lock().take();
                if let Err(e) = job.map_or(Ok(()), SealJob::run) {
                    inner.failed = true;
                    return Err(e.into());
                }
            }
        }
        // Its `.idx` is exact (see `locality.rs`): it lists the records of
        // the pack, each one the current entry of its ID. A fill before the
        // seal wrote it finds no file and counts as a dropped fill.
        self.locality.sealed_below(num + 1);
        Ok(())
    }
}

impl Drop for ChunkStore {
    /// Stop and join the merge thread (it holds no `ChunkStore`, so this
    /// runs). A merge in progress stops between two input pages; its
    /// partial output is removed, and an open removes what is left.
    fn drop(&mut self) {
        self.stop_merges();
        // Let a background seal end; its error is lost (nothing was promised).
        let _ = self.inner.lock().join_seal();
    }
}

/// Outcome of the pack part of an open (see `ChunkStore::load`).
enum Loaded {
    Done(Box<Inner>),
    /// The index cannot be used: start again from an empty index.
    /// `resync`: a pack to resync, never cut, in that second pass.
    Rebuild { reason: String, resync: Option<u64> },
}

/// The end of the record at `l`.
fn rec_end(l: &Loc) -> u64 {
    l.offset + (REC_HEADER + l.stored as usize + REC_CRC) as u64
}

/// True if the records of a pack below `hwm_offset` fill it from its header
/// to `hwm_offset` with no gap: every record that a run can point to is
/// there. The hwm pack is the active pack of the flush that set the hwm, so
/// its records below the hwm are contiguous (GC punches only packs below
/// the hwm pack). True if `hwm_offset` is 0 (no record is covered).
fn covers(entries: &[(ObjectId, Loc)], hwm_offset: u64) -> bool {
    if hwm_offset == 0 {
        return true;
    }
    let mut below: Vec<&Loc> = entries.iter().map(|(_, l)| l).filter(|l| l.offset < hwm_offset).collect();
    below.sort_unstable_by_key(|l| l.offset);
    let mut pos = PACK_HEADER as u64;
    for l in below {
        if l.offset != pos {
            return false;
        }
        pos = rec_end(l);
    }
    pos == hwm_offset
}

/// What a GC does with a pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GcAct {
    Delete,
    Compact,
    Punch,
}

/// The totals of one pack in pass 1 of a GC (record bytes, with the
/// header and the CRC).
#[derive(Clone, Copy, Default)]
struct PackTotals {
    bytes: u64,
    keep_bytes: u64,
    entries: u64,
}

/// A pack that the GC selected: its number, the action, and its current
/// entries (from the totals pass).
type GcTodo = (u64, GcAct, u64);

/// The plan of a GC for one pack (small: no entries of the pack).
struct GcPlan {
    num: u64,
    act: GcAct,
    /// The pending run with the tombstones of the victims (none if no
    /// victim).
    run: Option<Arc<Run>>,
    victims: u64,
    /// Raw and stored bytes of the victims.
    raw: u64,
    stored: u64,
    /// The current entries of the pack.
    entries: u64,
    /// An object of the pack was put or read during the GC: the pack does
    /// not change.
    dropped: bool,
}

/// Store-lock holds of one `gc_finish`.
#[derive(Default)]
struct Holds {
    max: Cell<Duration>,
    total: Cell<Duration>,
}

/// The store lock, held for a GC: its hold time goes into `Holds`.
struct Held<'a> {
    g: MutexGuard<'a, Inner>,
    at: Instant,
    holds: &'a Holds,
}

impl Deref for Held<'_> {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.g
    }
}

impl DerefMut for Held<'_> {
    fn deref_mut(&mut self) -> &mut Inner {
        &mut self.g
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let d = self.at.elapsed();
        self.holds.total.set(self.holds.total.get() + d);
        self.holds.max.set(self.holds.max.get().max(d));
    }
}

/// Is the record at `l` in the range of `n` bytes at `a`?
fn hits(l: &Loc, a: u64, n: u64) -> bool {
    l.offset < a + n && rec_end(l) > a
}

/// The whole blocks in the gaps between the records `stay` of a pack whose
/// records end at `end` (also the gap after the last one).
fn punch_ranges(mut stay: Vec<&Loc>, end: u64) -> Vec<(u64, u64)> {
    stay.sort_unstable_by_key(|l| l.offset);
    let mut ranges = Vec::new();
    let mut from = PACK_HEADER as u64;
    for (s, e) in stay.iter().map(|l| (l.offset, rec_end(l))).chain(std::iter::once((end, end))) {
        let a = from.div_ceil(PUNCH_BLOCK) * PUNCH_BLOCK;
        let b = s / PUNCH_BLOCK * PUNCH_BLOCK;
        if b > a {
            ranges.push((a, b - a));
        }
        from = from.max(e);
    }
    ranges
}

/// The victims of the GC of a mostly live pack: the records that are not
/// kept and lie in a whole block of a gap between kept records. A dead
/// record that holds no whole block of such a gap stays (a later GC
/// punches it when a neighbour dies too).
fn punch_victims(entries: &[(ObjectId, Loc)], keep: &dyn Fn(&ObjectId) -> bool) -> Vec<(ObjectId, Loc)> {
    let end = entries.iter().map(|(_, l)| rec_end(l)).max().unwrap_or(0);
    let ranges = punch_ranges(entries.iter().filter(|(id, _)| keep(id)).map(|(_, l)| l).collect(), end);
    entries.iter().filter(|(id, l)| !keep(id) && ranges.iter().any(|&(a, n)| hits(l, a, n))).copied().collect()
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
            index: Arc::new(Index::empty()),
            readers: HashMap::new(),
            tick: 0,
            active: None,
            next_pack: 1,
            trust_floor: Some(0),
            stats: Stats::default(),
            dir_dirty: false,
            created: 0,
            touched: None,
            failed: false,
            merge: Arc::new(MergeCtl::default()),
            sealing: None,
            creating: None,
            flushing: None,
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

    /// Wait for the background seal, if one runs. A failure is final.
    fn join_seal(&mut self) -> Result<()> {
        if let Some(t) = self.sealing.take() {
            if let Err(e) = t.thread.join().unwrap_or_else(|_| Err(io::Error::other("the pack seal thread panicked"))) {
                self.failed = true;
                return Err(e.into());
            }
        }
        Ok(())
    }

    /// Sync the active pack if it has new bytes, after the background seal
    /// of the pack before it: then every record so far is durable. A failure
    /// is final. It syncs under the store lock: only GC, `gc_begin`, `close`
    /// and the fallback below call it (`ChunkStore::sync` and the index
    /// flush sync through a handle with no lock).
    fn sync_active(&mut self) -> Result<()> {
        self.join_seal()?;
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

    /// What a sync with no store lock must do now: the active pack through
    /// its handle if it is dirty, and the directory if it is dirty. `None`
    /// if the dirty active pack has no handle (then
    /// `sync_locked_fallback`).
    fn sync_job(&self) -> Option<SyncJob> {
        let pack = match self.active.as_ref().filter(|a| a.dirty) {
            Some(a) => Some((a.num, a.published, Arc::clone(a.sync.as_ref()?))),
            None => None,
        };
        Some(SyncJob { pack, dir: self.dir_dirty.then_some(self.created) })
    }

    /// The rare fallback of a sync with no handle (`try_clone` failed): sync
    /// the active pack and the directory under the store lock. A failure
    /// is final.
    fn sync_locked_fallback(&mut self, io: &dyn PackIo) -> Result<()> {
        self.sync_active()?;
        if self.dir_dirty {
            self.sync_dir(io)?;
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
            t.insert(id, self.stats.objects);
        }
    }

    /// Index a new object (the caller found no current entry of `id`).
    fn add_new(&mut self, id: ObjectId, l: Loc) {
        self.index.insert(id, l);
        self.stats.objects += 1;
        self.stats.raw_bytes += l.raw as u64;
        self.stats.stored_bytes += l.stored as u64;
    }

    /// Put the entries of a pack, read at open, in the memtable: only those
    /// at or above `hwm` (the runs have the others). An entry counts in the
    /// stats only if the index has no current entry of its ID (a GC copy,
    /// or an entry that a run has, is not counted twice). A lookup error
    /// fails the open.
    fn replay(&mut self, entries: &[(ObjectId, Loc)], hwm: Hwm) -> Result<()> {
        for (id, l) in entries {
            if (Hwm { pack: l.pack, offset: l.offset }) < hwm {
                continue;
            }
            if self.index.get(id)?.is_some() {
                self.index.insert(*id, *l);
            } else {
                self.add_new(*id, *l);
            }
        }
        Ok(())
    }

    /// The hwm of a flush now: the end of the active pack, else the start
    /// of the pack that a `put` creates now, else the start of the next
    /// pack. Every pack record so far is below it.
    fn hwm_end(&self) -> Hwm {
        match (&self.active, &self.creating) {
            (Some(a), _) => Hwm { pack: a.num, offset: a.len },
            (None, Some((num, _))) => Hwm { pack: *num, offset: 0 },
            (None, None) => Hwm { pack: self.next_pack, offset: 0 },
        }
    }

    /// Write the memtable to a run with this hwm and the current counts. The
    /// caller has made every record below `hwm` durable. A failure is final.
    fn flush_at(&mut self, io: &dyn PackIo, hwm: Hwm) -> Result<()> {
        let s = self.stats;
        let counts = Counts { objects: s.objects, raw_bytes: s.raw_bytes, stored_bytes: s.stored_bytes };
        if let Err(e) = self.index.flush(io, hwm, counts) {
            self.failed = true;
            return Err(e);
        }
        self.merge.notify();
        Ok(())
    }

    /// A pack for reads: the cached handle, or a new one (opened under the
    /// store lock; for GC, which is not on the read path).
    fn reader(&mut self, io: &dyn PackIo, num: u64) -> io::Result<Arc<dyn PackRead>> {
        if let Some(f) = self.cached_reader(num) {
            return Ok(f);
        }
        Ok(self.add_reader(num, io.open_read(num)?))
    }

    /// The cached handle of pack `num`, if any (it counts as a use).
    fn cached_reader(&mut self, num: u64) -> Option<Arc<dyn PackRead>> {
        self.tick += 1;
        let (f, used) = self.readers.get_mut(&num)?;
        *used = self.tick;
        Some(Arc::clone(f))
    }

    /// Cache the new handle `f` of pack `num`, or keep the one that another
    /// read cached meanwhile. Returns the cached handle.
    fn add_reader(&mut self, num: u64, f: Arc<dyn PackRead>) -> Arc<dyn PackRead> {
        if let Some(cached) = self.cached_reader(num) {
            return cached;
        }
        if self.readers.len() >= MAX_OPEN_PACKS {
            // Close the least recently used pack. A read that holds its
            // handle keeps it open until the read ends.
            let oldest = self.readers.iter().min_by_key(|(_, (_, used))| *used).map(|(n, _)| *n);
            if let Some(n) = oldest {
                self.readers.remove(&n);
            }
        }
        self.readers.insert(num, (Arc::clone(&f), self.tick));
        f
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
