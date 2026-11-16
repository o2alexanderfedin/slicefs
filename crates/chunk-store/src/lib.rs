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

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ── Object IDs and chunking ───────────────────────────────────────────────────

/// BLAKE3-256 of the uncompressed bytes of an object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
    /// The ID of `data`.
    pub fn of(data: &[u8]) -> Self {
        ObjectId(*blake3::hash(data).as_bytes())
    }

    /// Lowercase hex.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ObjectId({})", &self.to_hex()[..16])
    }
}

/// FastCDC v2020 chunk sizes (bytes).
pub const CHUNK_MIN: u32 = 2 * 1024;
pub const CHUNK_AVG: u32 = 8 * 1024;
pub const CHUNK_MAX: u32 = 32 * 1024;

/// Content-defined chunk boundaries of `data` (FastCDC v2020, 2/8/32 KiB).
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
}

impl Default for Options {
    fn default() -> Self {
        Options { pack_limit: 64 << 20, zstd_level: 3 }
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
}

pub type Result<T> = std::result::Result<T, Error>;

// ── On-disk format ────────────────────────────────────────────────────────────

const PACK_MAGIC: &[u8; 4] = b"SFPK";
const IDX_MAGIC: &[u8; 4] = b"SFIX";
const FORMAT_VERSION: u16 = 1;
/// magic(4) + version(2) + pack number(8)
const PACK_HEADER: usize = 14;
/// id(32) + flags(1) + raw length(4) + stored length(4)
const REC_HEADER: usize = 41;
const REC_CRC: usize = 4;
/// id(32) + offset(8) + stored(4) + raw(4) + flags(1)
const IDX_ENTRY: usize = 49;
const FLAG_ZSTD: u8 = 1;
/// Objects smaller than this are not compressed.
const MIN_COMPRESS: usize = 64;

#[derive(Clone, Copy, Debug)]
struct Loc {
    pack: u64,
    offset: u64,
    stored: u32,
    raw: u32,
    flags: u8,
}

fn pack_name(num: u64, ext: &str) -> String {
    format!("{num:016x}.{ext}")
}

fn encode_record(id: &ObjectId, flags: u8, raw: u32, payload: &[u8]) -> Vec<u8> {
    let mut r = Vec::with_capacity(REC_HEADER + payload.len() + REC_CRC);
    r.extend_from_slice(&id.0);
    r.push(flags);
    r.extend_from_slice(&raw.to_le_bytes());
    r.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    r.extend_from_slice(payload);
    let crc = crc32c::crc32c(&r);
    r.extend_from_slice(&crc.to_le_bytes());
    r
}

/// Parse the record that starts at `buf[0]`. Returns its location fields and
/// its total length, or `None` for a short or damaged record.
fn decode_record(buf: &[u8]) -> Option<(ObjectId, u8, u32, u32, usize)> {
    if buf.len() < REC_HEADER + REC_CRC {
        return None;
    }
    let id = ObjectId(buf[..32].try_into().unwrap());
    let flags = buf[32];
    let raw = u32::from_le_bytes(buf[33..37].try_into().unwrap());
    let stored = u32::from_le_bytes(buf[37..41].try_into().unwrap());
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
fn scan_pack(path: &Path, num: u64) -> io::Result<(Vec<(ObjectId, Loc)>, u64)> {
    let buf = fs::read(path)?;
    let mut out = Vec::new();
    if buf.len() < PACK_HEADER || &buf[..4] != PACK_MAGIC {
        return Ok((out, 0));
    }
    let mut pos = PACK_HEADER;
    while let Some((id, flags, raw, stored, total)) = decode_record(&buf[pos..]) {
        out.push((id, Loc { pack: num, offset: pos as u64, stored, raw, flags }));
        pos += total;
    }
    Ok((out, pos as u64))
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

fn decode_index(b: &[u8], num: u64) -> Option<Vec<(ObjectId, Loc)>> {
    if b.len() < 18 || &b[..4] != IDX_MAGIC {
        return None;
    }
    let (body, crc) = b.split_at(b.len() - 4);
    if crc32c::crc32c(body) != u32::from_le_bytes(crc.try_into().unwrap()) {
        return None;
    }
    let n = u64::from_le_bytes(body[6..14].try_into().unwrap()) as usize;
    if body.len() != 14 + n * IDX_ENTRY {
        return None;
    }
    Some(
        body[14..]
            .chunks_exact(IDX_ENTRY)
            .map(|e| {
                let id = ObjectId(e[..32].try_into().unwrap());
                let offset = u64::from_le_bytes(e[32..40].try_into().unwrap());
                let stored = u32::from_le_bytes(e[40..44].try_into().unwrap());
                let raw = u32::from_le_bytes(e[44..48].try_into().unwrap());
                (id, Loc { pack: num, offset, stored, raw, flags: e[48] })
            })
            .collect(),
    )
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

// ── Store ─────────────────────────────────────────────────────────────────────

struct Active {
    num: u64,
    file: File,
    len: u64,
    entries: Vec<(ObjectId, Loc)>,
    /// Bytes appended since the last sync.
    dirty: bool,
}

struct Inner {
    index: HashMap<ObjectId, Loc>,
    readers: HashMap<u64, Arc<File>>,
    active: Option<Active>,
    next_pack: u64,
    stats: Stats,
    /// A pack file was created since the last directory sync.
    dir_dirty: bool,
}

/// A content-addressed object store in pack files.
pub struct ChunkStore {
    packs: PathBuf,
    opts: Options,
    inner: Mutex<Inner>,
}

impl ChunkStore {
    /// Open (or create) the store in `dir`. Packs without a valid index are
    /// scanned; the newest unsealed pack becomes the active pack, cut after
    /// its last valid record.
    pub fn open(dir: &Path, opts: Options) -> Result<Self> {
        let packs = dir.join("packs");
        fs::create_dir_all(&packs)?;
        let mut nums: Vec<u64> = fs::read_dir(&packs)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                let hex = n.strip_suffix(".pack")?;
                u64::from_str_radix(hex, 16).ok()
            })
            .collect();
        nums.sort_unstable();

        let mut inner = Inner {
            index: HashMap::new(),
            readers: HashMap::new(),
            active: None,
            next_pack: nums.last().map_or(1, |n| n + 1),
            stats: Stats::default(),
            dir_dirty: false,
        };
        let last = nums.last().copied();
        for num in nums {
            let pack = packs.join(pack_name(num, "pack"));
            let idx = packs.join(pack_name(num, "idx"));
            let loaded = fs::read(&idx).ok().and_then(|b| decode_index(&b, num));
            let entries = match loaded {
                Some(e) => e,
                None => {
                    let (entries, valid) = scan_pack(&pack, num)?;
                    if Some(num) == last {
                        // The newest pack without an index is the active pack.
                        let file = OpenOptions::new().read(true).write(true).open(&pack)?;
                        let valid = if valid < PACK_HEADER as u64 {
                            file.set_len(0)?;
                            file.write_all_at(&pack_header(num), 0)?;
                            PACK_HEADER as u64
                        } else {
                            file.set_len(valid)?;
                            valid
                        };
                        full_sync(&file)?;
                        inner.next_pack = num + 1;
                        for (id, l) in &entries {
                            inner.add(*id, *l);
                        }
                        inner.stats.packs += 1;
                        inner.active = Some(Active { num, file, len: valid, entries, dirty: false });
                        continue;
                    }
                    write_durable(&idx, &encode_index(&entries))?;
                    entries
                }
            };
            for (id, l) in entries {
                inner.add(id, l);
            }
            inner.stats.packs += 1;
        }
        Ok(ChunkStore { packs, opts, inner: Mutex::new(inner) })
    }

    /// Store `data` and return its ID. Bytes already in the store are not
    /// written again.
    pub fn put(&self, data: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::of(data);
        if self.contains(&id) {
            return Ok(id);
        }
        if data.len() > u32::MAX as usize - REC_HEADER {
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
        let rec = encode_record(&id, flags, data.len() as u32, payload);

        let mut inner = self.inner.lock().unwrap();
        if inner.index.contains_key(&id) {
            return Ok(id);
        }
        if inner.active.is_none() {
            let num = inner.next_pack;
            inner.next_pack += 1;
            let path = self.packs.join(pack_name(num, "pack"));
            let file = OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
            file.write_all_at(&pack_header(num), 0)?;
            inner.active = Some(Active { num, file, len: PACK_HEADER as u64, entries: Vec::new(), dirty: true });
            inner.stats.packs += 1;
            inner.dir_dirty = true;
        }
        let a = inner.active.as_mut().unwrap();
        a.file.write_all_at(&rec, a.len)?;
        let loc = Loc { pack: a.num, offset: a.len, stored: payload.len() as u32, raw: data.len() as u32, flags };
        a.len += rec.len() as u64;
        a.entries.push((id, loc));
        a.dirty = true;
        let full = a.len >= self.opts.pack_limit;
        inner.add(id, loc);
        if full {
            self.seal(&mut inner)?;
        }
        Ok(id)
    }

    /// The bytes of `id`, or `None` if the store does not have the object or
    /// its record is damaged.
    pub fn get(&self, id: &ObjectId) -> Option<Vec<u8>> {
        let (loc, file) = {
            let mut inner = self.inner.lock().unwrap();
            let loc = *inner.index.get(id)?;
            let file = inner.reader(&self.packs, loc.pack).ok()?;
            (loc, file)
        };
        let mut buf = vec![0u8; REC_HEADER + loc.stored as usize + REC_CRC];
        file.read_exact_at(&mut buf, loc.offset).ok()?;
        let (rid, flags, raw, _, _) = decode_record(&buf)?;
        if rid != *id {
            return None;
        }
        let payload = &buf[REC_HEADER..REC_HEADER + loc.stored as usize];
        let data = if flags & FLAG_ZSTD != 0 {
            zstd::bulk::decompress(payload, raw as usize).ok()?
        } else {
            payload.to_vec()
        };
        (ObjectId::of(&data) == *id).then_some(data)
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
        if let Some(a) = inner.active.as_mut() {
            if a.dirty {
                full_sync(&a.file)?;
                a.dirty = false;
            }
        }
        if inner.dir_dirty {
            sync_dir(&self.packs)?;
            inner.dir_dirty = false;
        }
        Ok(())
    }

    /// Seal the active pack (sync, write its index). Call at unmount.
    pub fn close(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.active.is_some() {
            self.seal(&mut inner)?;
        }
        if inner.dir_dirty {
            sync_dir(&self.packs)?;
            inner.dir_dirty = false;
        }
        Ok(())
    }

    /// Current counts.
    pub fn stats(&self) -> Stats {
        self.inner.lock().unwrap().stats
    }

    fn seal(&self, inner: &mut Inner) -> Result<()> {
        let Some(a) = inner.active.take() else { return Ok(()) };
        full_sync(&a.file)?;
        write_durable(&self.packs.join(pack_name(a.num, "idx")), &encode_index(&a.entries))?;
        inner.dir_dirty = false; // write_durable synced the directory
        Ok(())
    }
}

fn pack_header(num: u64) -> Vec<u8> {
    let mut h = Vec::with_capacity(PACK_HEADER);
    h.extend_from_slice(PACK_MAGIC);
    h.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    h.extend_from_slice(&num.to_le_bytes());
    h
}

impl Inner {
    fn add(&mut self, id: ObjectId, l: Loc) {
        if self.index.insert(id, l).is_none() {
            self.stats.objects += 1;
            self.stats.raw_bytes += l.raw as u64;
            self.stats.stored_bytes += l.stored as u64;
        }
    }

    fn reader(&mut self, packs: &Path, num: u64) -> io::Result<Arc<File>> {
        if let Some(f) = self.readers.get(&num) {
            return Ok(Arc::clone(f));
        }
        let f = Arc::new(File::open(packs.join(pack_name(num, "pack")))?);
        self.readers.insert(num, Arc::clone(&f));
        Ok(f)
    }
}
