//! Sorted runs of the store index: immutable files of (ObjectId, entry),
//! sorted by the full 33-byte ID (tag, then digest). A lookup reads one
//! 4 KiB page; opening a run reads only its footer and its tables.
//!
//! Layout of a run file (integers are little-endian):
//!
//! - Pages of [`PAGE`] bytes from offset 0. A page holds the entries of one
//!   tag only: a new tag starts a new page. A page is a header of
//!   [`PAGE_HEADER`] bytes, then the entries, then zeros. The header: entry
//!   count u16, tag u8, shared u8, CRC32C u32 of the page header without
//!   the CRC and of the entry bytes, then the byte widths of the pack, the
//!   offset, the stored length and the raw length (u8 each; a raw width of
//!   [`RAW_SAME`] says that the raw length of each entry is its stored
//!   length).
//! - An entry of a page is: digest bytes `shared..32` of the ID, then pack,
//!   offset, stored length and raw length as little-endian numbers of the
//!   widths of the page (a width can be 0: the value is 0), then flags u8.
//!   All entries of a page have the same size, so entry `i` is at a fixed
//!   place. `shared` (at most [`SHARE_MAX`]) is the number of leading
//!   digest bytes that all keys of the page have in common; they are not
//!   stored in the page, but are the first bytes of the fence of the page
//!   (in RAM, checked at open). The writer chooses the widths of a page
//!   from its entries (the smallest that hold each value): there is no
//!   limit on a pack number, an offset or a length, and small values take
//!   few bytes (#85 follow-up: the index used 58 bytes for each entry).
//! - The tag table: for each tag in the run, in tag order: tag u8, first
//!   page u64, page count u64 ([`TAG_ROW`] bytes).
//! - The fence table: for each page, digest bytes 0..8 of its first entry
//!   ([`FENCE`] bytes, in digest byte order).
//! - The filter block: empty when the filter type is [`FILTER_NONE`]; a
//!   blocked Bloom filter over the digests of every entry (tombstones too)
//!   when it is [`FILTER_BLOOM`] (see `bloom.rs`).
//! - The footer ([`FOOTER`] bytes): magic `SFRN`, run format version u16,
//!   filter type u8, bits per key u8, entry count u64, tombstone count u64,
//!   page count u64; then offset u64, length u64 and CRC32C u32 of the tag
//!   table, of the fence table and of the filter block; then the smallest
//!   and the largest pack number of the put entries (`u64::MAX` and 0 if
//!   there is no put entry); then a CRC32C u32 of the footer bytes before it.
//!
//! A tombstone has flag bit [`TOMB`] set; its pack field is the pack whose
//! entry it hides, and its other fields are 0.
//!
//! Every count, offset and length that a run file gives is untrusted: it is
//! checked with checked arithmetic against the file length before any use or
//! allocation. Damage is never a panic.
//!
//! Two read paths:
//!
//! - Lookups ([`Run::get`]): the filter in RAM, then the fence table in RAM
//!   (interpolation search: the digests are uniform), then one page, read
//!   in place from the read-only mapping of the file (FileIo; see
//!   `pack_io::RunMap`) or with `read_at` where the strategy has no mapping
//!   (the test mock). An interpolation search inside the page finds the
//!   entry. A hit does not check the page CRC: on a 4 KiB page that costs
//!   300 to 490 ns, more than half of a mapped hit (17-05 mmap spike). A
//!   damaged page can then give a wrong entry; see [`Run::get`] for why
//!   that loses no data.
//! - Verified reads ([`Run::get_verified`], [`Run::iter`]): `read_at` and a
//!   check of the page CRC, of the zero bytes, of the tag and of the first
//!   key against its fence, so damage is always [`Error::Corrupt`]. Merge,
//!   GC and the check of an entry whose record a read rejected use these.

use crate::bloom::{self, Bloom};
use crate::pack_io::{PackFile, PackIo, PackRead, RunMap};
use crate::{Error, Loc, ObjectId, Result, DIGEST_LEN};
use std::io;
use std::sync::Arc;

/// Size of a page of entries.
pub(crate) const PAGE: usize = 4096;
/// Entry count u16, tag u8, shared u8, CRC32C u32, four widths u8.
const PAGE_HEADER: usize = 12;
/// The most leading digest bytes that a page leaves out of its keys: they
/// come from the fence of the page (which has [`FENCE`] bytes).
const SHARE_MAX: usize = FENCE;
/// The raw width that says "raw length = stored length" for every entry.
const RAW_SAME: u8 = 0xff;
/// The smallest entry: the key without [`SHARE_MAX`] bytes, widths 0, flags.
const MIN_ENTRY: usize = DIGEST_LEN - SHARE_MAX + 1;
/// Most entries in one page (all of the smallest size).
pub(crate) const MAX_PER_PAGE: usize = (PAGE - PAGE_HEADER) / MIN_ENTRY;
/// Flag bit of a tombstone (a `Loc` uses bit 0 only).
const TOMB: u8 = 0x80;
/// The flag bits that a put entry can have.
const PUT_FLAGS: u8 = 0x01;
/// Tag u8, first page u64, page count u64.
const TAG_ROW: usize = 1 + 8 + 8;
/// Bytes of one fence: digest bytes 0..8 of the first entry of a page.
const FENCE: usize = 8;
const MAGIC: &[u8; 4] = b"SFRN";
/// Format version of run files (packs and `.idx` files have their own).
/// Version 2: pages with per-page field widths and shared key bytes.
pub(crate) const RUN_VERSION: u16 = 2;
/// Filter type: no filter (the filter block is empty).
pub(crate) const FILTER_NONE: u8 = 0;
/// Filter type: a blocked Bloom filter (see `bloom.rs`).
pub(crate) const FILTER_BLOOM: u8 = 1;
/// Offset u64, length u64, CRC32C u32 of one block after the pages.
const SECTION: usize = 8 + 8 + 4;
/// Magic, version, filter type, bits per key, entry count, tombstone count,
/// page count, three sections, smallest and largest pack, CRC32C.
pub(crate) const FOOTER: usize = 4 + 2 + 1 + 1 + 8 + 8 + 8 + 3 * SECTION + 8 + 8 + 4;
/// `RunWriter` syncs after this many bytes, so the copy of unsynced bytes
/// that `RetryingIo` keeps stays small.
const SYNC_EVERY: u64 = 64 << 20;

const _: () = assert!(MIN_ENTRY == 25 && MAX_PER_PAGE == 163 && FOOTER == 112);

/// The value of a run entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    /// The object is at this place.
    Put(Loc),
    /// The entry of the object in this pack is removed.
    Tomb(u64),
}

/// Digest bytes 0..8 as a number whose order is the byte order.
fn prefix(id: &ObjectId) -> u64 {
    u64::from_be_bytes(id.0[1..1 + FENCE].try_into().unwrap())
}

fn le_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}

fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn le_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// The bytes that `v` needs as a little-endian number (0 for 0).
fn width(v: u64) -> u8 {
    (8 - v.leading_zeros() / 8) as u8
}

/// A little-endian number of `b.len()` bytes (at most 8).
#[inline]
fn le_var(b: &[u8]) -> u64 {
    let mut x = [0u8; 8];
    x[..b.len()].copy_from_slice(b);
    u64::from_le_bytes(x)
}

/// The four place fields of an entry: pack, offset, stored, raw.
fn fields(e: &Entry) -> (u64, u64, u64, u64, u8) {
    match *e {
        Entry::Put(l) => (l.pack, l.offset, l.stored as u64, l.raw as u64, l.flags),
        Entry::Tomb(pack) => (pack, 0, 0, 0, TOMB),
    }
}

/// The shape of the entries of one page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape {
    /// Leading digest bytes that the page leaves out (at most `SHARE_MAX`).
    shared: u8,
    pack: u8,
    offset: u8,
    stored: u8,
    /// `RAW_SAME`, or the width of the raw length.
    raw: u8,
}

impl Shape {
    /// Bytes of the key part of an entry.
    fn key_len(&self) -> usize {
        DIGEST_LEN - self.shared as usize
    }

    /// Bytes of one entry.
    fn entry_len(&self) -> usize {
        let raw = if self.raw == RAW_SAME { 0 } else { self.raw as usize };
        self.key_len() + self.pack as usize + self.offset as usize + self.stored as usize + raw + 1
    }
}

/// A page whose header passed the checks of [`Page::parse`]: every entry
/// lies inside the page, so no access below can panic.
#[derive(Clone, Copy)]
struct Page<'a> {
    b: &'a [u8],
    count: usize,
    tag: u8,
    shape: Shape,
    /// Bytes of one entry.
    w: usize,
}

impl<'a> Page<'a> {
    /// The page in `b` (`PAGE` bytes), if its header is possible: a count of
    /// 1 or more, `shared` and the widths in range, and the entries inside
    /// the page. Nothing else is checked (no CRC).
    #[inline]
    fn parse(b: &'a [u8]) -> Option<Page<'a>> {
        let count = le_u16(b, 0) as usize;
        let shape = Shape { shared: b[3], pack: b[8], offset: b[9], stored: b[10], raw: b[11] };
        let ok_width = |w: u8| w <= 8;
        if count == 0
            || shape.shared as usize > SHARE_MAX
            || !ok_width(shape.pack)
            || !ok_width(shape.offset)
            || !ok_width(shape.stored)
            || !(ok_width(shape.raw) || shape.raw == RAW_SAME)
        {
            return None;
        }
        let w = shape.entry_len();
        if PAGE_HEADER + count * w > b.len() {
            return None;
        }
        Some(Page { b, count, tag: b[2], shape, w })
    }

    fn crc_ok(&self) -> bool {
        let crc = crc32c::crc32c_append(crc32c::crc32c(&self.b[..4]), &self.b[8..PAGE_HEADER + self.count * self.w]);
        crc == le_u32(self.b, 4)
    }

    /// The key part of entry `i` (digest bytes `shared..32`).
    #[inline]
    fn key(&self, i: usize) -> &'a [u8] {
        let at = PAGE_HEADER + i * self.w;
        &self.b[at..at + self.shape.key_len()]
    }

    /// Entry `i`, or `None` if its flags are not those of a put or a
    /// tombstone, or a put points to offset 0 (no record starts there): a
    /// cheap check of a page whose CRC is not checked.
    fn entry(&self, i: usize) -> Option<Entry> {
        let s = self.shape;
        let mut at = PAGE_HEADER + i * self.w + s.key_len();
        let mut next = |n: u8| {
            let v = le_var(&self.b[at..at + n as usize]);
            at += n as usize;
            v
        };
        let pack = next(s.pack);
        let offset = next(s.offset);
        let stored = next(s.stored);
        let raw = if s.raw == RAW_SAME { stored } else { next(s.raw) };
        let flags = self.b[at];
        if flags == TOMB {
            return Some(Entry::Tomb(pack));
        }
        if flags & !PUT_FLAGS != 0 || offset == 0 {
            return None;
        }
        let stored = u32::try_from(stored).ok()?;
        let raw = u32::try_from(raw).ok()?;
        Some(Entry::Put(Loc { pack, offset, stored, raw, flags }))
    }

    /// The index of the entry of `id` (its digest bytes `0..shared` are
    /// those of the page), whose prefixes lie in `lo..=hi` (the fence of the
    /// page and the fence of the next one). An interpolation guess from the
    /// prefix, then a walk to the entry: the walk is at most `count` steps,
    /// also on keys that are not uniform.
    #[inline]
    fn search(&self, id: &ObjectId, lo: u64, hi: u64) -> Option<usize> {
        use std::cmp::Ordering::*;
        let want = &id.digest()[self.shape.shared as usize..];
        let count = self.count;
        let frac = prefix(id).saturating_sub(lo) as f64 / (hi.saturating_sub(lo) as f64 + 1.0);
        let mut i = ((frac * count as f64) as usize).min(count - 1);
        match self.key(i).cmp(want) {
            Equal => Some(i),
            Less => {
                i += 1;
                while i < count {
                    match self.key(i).cmp(want) {
                        Equal => return Some(i),
                        Greater => return None,
                        Less => i += 1,
                    }
                }
                None
            }
            Greater => {
                while i > 0 {
                    i -= 1;
                    match self.key(i).cmp(want) {
                        Equal => return Some(i),
                        Less => return None,
                        Greater => {}
                    }
                }
                None
            }
        }
    }
}

/// Offset, length and CRC32C of one block after the pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Section {
    off: u64,
    len: u64,
    crc: u32,
}

/// The footer of a run file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Footer {
    filter_type: u8,
    bits_per_key: u8,
    entries: u64,
    tombstones: u64,
    pages: u64,
    tags: Section,
    fences: Section,
    filter: Section,
    min_pack: u64,
    max_pack: u64,
}

impl Footer {
    fn encode(&self) -> [u8; FOOTER] {
        let mut b = Vec::with_capacity(FOOTER);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&RUN_VERSION.to_le_bytes());
        b.push(self.filter_type);
        b.push(self.bits_per_key);
        b.extend_from_slice(&self.entries.to_le_bytes());
        b.extend_from_slice(&self.tombstones.to_le_bytes());
        b.extend_from_slice(&self.pages.to_le_bytes());
        for s in [self.tags, self.fences, self.filter] {
            b.extend_from_slice(&s.off.to_le_bytes());
            b.extend_from_slice(&s.len.to_le_bytes());
            b.extend_from_slice(&s.crc.to_le_bytes());
        }
        b.extend_from_slice(&self.min_pack.to_le_bytes());
        b.extend_from_slice(&self.max_pack.to_le_bytes());
        let crc = crc32c::crc32c(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b.try_into().unwrap()
    }

    /// The footer, if its magic, CRC and version are right (the values are
    /// not checked here).
    fn decode(b: &[u8; FOOTER]) -> std::result::Result<Footer, String> {
        if &b[..4] != MAGIC {
            return Err("no run magic".into());
        }
        if crc32c::crc32c(&b[..FOOTER - 4]) != le_u32(b, FOOTER - 4) {
            return Err("bad footer CRC".into());
        }
        let version = le_u16(b, 4);
        if version != RUN_VERSION {
            return Err(format!("run format version {version}, this SliceFS reads version {RUN_VERSION}"));
        }
        let section = |at: usize| Section { off: le_u64(b, at), len: le_u64(b, at + 8), crc: le_u32(b, at + 16) };
        let s = 32;
        Ok(Footer {
            filter_type: b[6],
            bits_per_key: b[7],
            entries: le_u64(b, 8),
            tombstones: le_u64(b, 16),
            pages: le_u64(b, 24),
            tags: section(s),
            fences: section(s + SECTION),
            filter: section(s + 2 * SECTION),
            min_pack: le_u64(b, s + 3 * SECTION),
            max_pack: le_u64(b, s + 3 * SECTION + 8),
        })
    }
}

/// What `RunWriter::finish` wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RunInfo {
    /// Length of the run file.
    pub len: u64,
    /// Entries, tombstones included.
    pub entries: u64,
    pub tombstones: u64,
}

/// Writes one run file: entries go in, in strictly ascending ID order, and
/// stream to disk one page at a time. RAM: the entries of one page, the tag
/// table and the fence table (8 bytes per page).
///
/// The file is durable after `finish` (its name only after a `sync_dir`).
/// A writer dropped before `finish`, or one that returned an error, leaves
/// a partial file that the caller removes.
pub(crate) struct RunWriter {
    file: Box<dyn PackFile>,
    num: u64,
    /// The entries of the page being filled.
    pending: Vec<(ObjectId, Entry)>,
    /// The shape of `pending` (valid when it is not empty).
    shape: Shape,
    /// The largest width of a raw length in `pending`, and whether every
    /// raw length there is its stored length.
    raw_width: u8,
    raw_same: bool,
    /// A page buffer.
    page: Vec<u8>,
    /// Bytes written to the file so far.
    pos: u64,
    /// Bytes written since the last sync.
    unsynced: u64,
    /// Tag, first page, page count.
    tags: Vec<(u8, u64, u64)>,
    fences: Vec<u8>,
    /// The filter, sized from the expected entry count; `None` for filter
    /// type none. The writer keeps no digests: only the filter bits.
    filter: Option<Bloom>,
    bits_per_key: u8,
    last: Option<ObjectId>,
    entries: u64,
    tombstones: u64,
    min_pack: u64,
    max_pack: u64,
}

/// The number of leading bytes that `a` and `b` have in common.
fn common(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

impl RunWriter {
    /// A writer of a new run file `num`. `expected` is an upper bound of the
    /// entry count: the filter is sized from it (a merge passes the sum of
    /// its input counts); the footer holds the real count. `bits_per_key` 0
    /// writes no filter (filter type none).
    pub fn create(io: &dyn PackIo, num: u64, expected: u64, bits_per_key: u8) -> Result<Self> {
        if bits_per_key > 0 && bloom::blocks_for(expected, bits_per_key).is_none() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "index run filter size overflows").into());
        }
        let file = io.create_run(num)?;
        // A guess of the page count, only for the capacity of the fences.
        let pages = expected.div_ceil(64);
        let fences = Vec::with_capacity(usize::try_from(pages.saturating_mul(FENCE as u64)).unwrap_or(0).min(1 << 24));
        Ok(RunWriter {
            file,
            num,
            pending: Vec::with_capacity(MAX_PER_PAGE),
            shape: Shape { shared: SHARE_MAX as u8, pack: 0, offset: 0, stored: 0, raw: RAW_SAME },
            raw_width: 0,
            raw_same: true,
            page: vec![0; PAGE],
            pos: 0,
            unsynced: 0,
            tags: Vec::new(),
            fences,
            filter: (bits_per_key > 0).then(|| Bloom::new(expected, bits_per_key)),
            bits_per_key,
            last: None,
            entries: 0,
            tombstones: 0,
            min_pack: u64::MAX,
            max_pack: 0,
        })
    }

    /// The shape of the pending page with `id`, `e` added to it, and the
    /// raw-length state that goes with it.
    fn shape_with(&self, id: &ObjectId, e: &Entry) -> (Shape, u8, bool) {
        let (pack, offset, stored, raw, _) = fields(e);
        let (mut s, mut raw_width, mut raw_same) = (self.shape, self.raw_width, self.raw_same);
        if self.pending.is_empty() {
            s = Shape { shared: SHARE_MAX as u8, pack: 0, offset: 0, stored: 0, raw: RAW_SAME };
            (raw_width, raw_same) = (0, true);
        } else {
            let first = self.pending[0].0.digest();
            s.shared = s.shared.min(common(first, id.digest()).min(SHARE_MAX) as u8);
        }
        s.pack = s.pack.max(width(pack));
        s.offset = s.offset.max(width(offset));
        s.stored = s.stored.max(width(stored));
        raw_width = raw_width.max(width(raw));
        raw_same &= raw == stored;
        s.raw = if raw_same { RAW_SAME } else { raw_width };
        (s, raw_width, raw_same)
    }

    /// Add the next entry. IDs must be strictly ascending (one entry per ID).
    pub fn push(&mut self, id: &ObjectId, e: &Entry) -> Result<()> {
        if self.last.is_some_and(|last| *id <= last) {
            let msg = format!("index run {:016x}: entries out of order at {id:?}", self.num);
            return Err(io::Error::new(io::ErrorKind::InvalidInput, msg).into());
        }
        if matches!(e, Entry::Put(l) if l.flags & TOMB != 0) {
            let msg = format!("index run {:016x}: a put entry with the tombstone flag", self.num);
            return Err(io::Error::new(io::ErrorKind::InvalidInput, msg).into());
        }
        let tag = id.tag();
        if self.pending.first().is_some_and(|(f, _)| f.tag() != tag) {
            self.write_page()?;
        }
        let mut next = self.shape_with(id, e);
        if !self.pending.is_empty() && PAGE_HEADER + (self.pending.len() + 1) * next.0.entry_len() > PAGE {
            self.write_page()?;
            next = self.shape_with(id, e);
        }
        if self.pending.is_empty() {
            let page = (self.fences.len() / FENCE) as u64;
            match self.tags.last_mut() {
                Some((t, _, count)) if *t == tag => *count += 1,
                _ => self.tags.push((tag, page, 1)),
            }
            self.fences.extend_from_slice(&id.0[1..1 + FENCE]);
        }
        (self.shape, self.raw_width, self.raw_same) = next;
        self.pending.push((*id, *e));
        if let Some(f) = self.filter.as_mut() {
            f.insert(id.digest());
        }
        self.entries += 1;
        match *e {
            Entry::Tomb(_) => self.tombstones += 1,
            Entry::Put(l) => {
                self.min_pack = self.min_pack.min(l.pack);
                self.max_pack = self.max_pack.max(l.pack);
            }
        }
        self.last = Some(*id);
        Ok(())
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.file.write_at(data, self.pos)?;
        self.pos += data.len() as u64;
        self.unsynced += data.len() as u64;
        if self.unsynced >= SYNC_EVERY {
            self.file.sync()?;
            self.unsynced = 0;
        }
        Ok(())
    }

    /// Encode the pending entries into a page, write it and start a new one.
    fn write_page(&mut self) -> Result<()> {
        let s = self.shape;
        let w = s.entry_len();
        let mut page = std::mem::take(&mut self.page);
        page.fill(0);
        page[..2].copy_from_slice(&(self.pending.len() as u16).to_le_bytes());
        page[2] = self.pending[0].0.tag();
        page[3] = s.shared;
        page[8..12].copy_from_slice(&[s.pack, s.offset, s.stored, s.raw]);
        for (i, (id, e)) in self.pending.iter().enumerate() {
            let out = &mut page[PAGE_HEADER + i * w..PAGE_HEADER + (i + 1) * w];
            out[..s.key_len()].copy_from_slice(&id.digest()[s.shared as usize..]);
            let (pack, offset, stored, raw, flags) = fields(e);
            let mut at = s.key_len();
            let mut put = |v: u64, n: u8| {
                out[at..at + n as usize].copy_from_slice(&v.to_le_bytes()[..n as usize]);
                at += n as usize;
            };
            put(pack, s.pack);
            put(offset, s.offset);
            put(stored, s.stored);
            if s.raw != RAW_SAME {
                put(raw, s.raw);
            }
            out[at] = flags;
        }
        let end = PAGE_HEADER + self.pending.len() * w;
        let crc = crc32c::crc32c_append(crc32c::crc32c(&page[..4]), &page[8..end]);
        page[4..8].copy_from_slice(&crc.to_le_bytes());
        let r = self.write(&page);
        self.page = page;
        self.pending.clear();
        r
    }

    /// Write the last page, the tables and the footer, and sync the file.
    pub fn finish(mut self) -> Result<RunInfo> {
        if !self.pending.is_empty() {
            self.write_page()?;
        }
        let pages = (self.fences.len() / FENCE) as u64;
        let mut tags = Vec::with_capacity(self.tags.len() * TAG_ROW);
        for &(tag, first, count) in &self.tags {
            tags.push(tag);
            tags.extend_from_slice(&first.to_le_bytes());
            tags.extend_from_slice(&count.to_le_bytes());
        }
        let tags_at = self.pos;
        let fences_at = tags_at + tags.len() as u64;
        let filter_at = fences_at + self.fences.len() as u64;
        let filter = self.filter.take().map_or_else(Vec::new, |f| f.to_bytes());
        let footer = Footer {
            filter_type: if self.bits_per_key > 0 { FILTER_BLOOM } else { FILTER_NONE },
            bits_per_key: self.bits_per_key,
            entries: self.entries,
            tombstones: self.tombstones,
            pages,
            tags: Section { off: tags_at, len: tags.len() as u64, crc: crc32c::crc32c(&tags) },
            fences: Section { off: fences_at, len: self.fences.len() as u64, crc: crc32c::crc32c(&self.fences) },
            filter: Section { off: filter_at, len: filter.len() as u64, crc: crc32c::crc32c(&filter) },
            min_pack: self.min_pack,
            max_pack: self.max_pack,
        };
        let mut tail = tags;
        tail.extend_from_slice(&std::mem::take(&mut self.fences));
        tail.extend_from_slice(&filter);
        tail.extend_from_slice(&footer.encode());
        self.write(&tail)?;
        self.file.sync()?;
        Ok(RunInfo { len: self.pos, entries: self.entries, tombstones: self.tombstones })
    }
}

/// The pages of one tag.
#[derive(Clone, Copy, Debug)]
struct TagRange {
    tag: u8,
    first: usize,
    count: usize,
}

/// An open run: the footer values, the tag table, the fences and the filter
/// in RAM; the pages stay on disk (mapped read-only where the strategy maps
/// runs). The run owns its mapping: it is unmapped when the last `Arc` of
/// the run drops, so a reader keeps valid bytes after a merge or GC removes
/// the file.
#[allow(dead_code)] // The pack range: for GC (17-07).
pub(crate) struct Run {
    file: Arc<dyn PackRead>,
    /// The whole file, mapped; `None`: lookups read pages with `read_at`.
    map: Option<RunMap>,
    num: u64,
    len: u64,
    entries: u64,
    tombstones: u64,
    min_pack: u64,
    max_pack: u64,
    tags: Vec<TagRange>,
    /// Digest bytes 0..8 of the first entry of each page (see `prefix`).
    fences: Vec<u64>,
    /// `None` for filter type none (every ID may be in the run).
    filter: Option<Bloom>,
}

impl Run {
    fn corrupt(num: u64, what: impl std::fmt::Display) -> Error {
        Error::Corrupt(format!("index run {num:016x}: {what}"))
    }

    /// Read `buf` at `offset`. A file shorter than its footer says is
    /// damage (`Corrupt`); any other read error stays an I/O error.
    fn read(file: &dyn PackRead, num: u64, buf: &mut [u8], offset: u64) -> Result<()> {
        file.read_at(buf, offset).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                Self::corrupt(num, format_args!("short read of {} bytes at {offset}", buf.len()))
            } else {
                Error::Io(e)
            }
        })
    }

    /// Open run `num` of length `len` (the caller knows the length from the
    /// writer or from the file), with the mapping of the file if the
    /// strategy has one. Reads the footer, the tag table, the fence table
    /// and the filter; reads no page.
    pub fn open(file: Arc<dyn PackRead>, map: Option<RunMap>, num: u64, len: u64) -> Result<Run> {
        let bad = |what: &str| Self::corrupt(num, what);
        if len < FOOTER as u64 {
            return Err(bad("shorter than a footer"));
        }
        // A page past the end of a mapped file is SIGBUS, not a short read.
        if map.as_ref().is_some_and(|m| (m.len() as u64) < len) {
            return Err(bad("the file is shorter than its length in the MANIFEST"));
        }
        let mut fb = [0u8; FOOTER];
        Self::read(&*file, num, &mut fb, len - FOOTER as u64)?;
        let f = Footer::decode(&fb).map_err(|e| Self::corrupt(num, e))?;

        // The layout is fixed, so each block must sit exactly where the
        // one before it ends.
        let pages_len = f.pages.checked_mul(PAGE as u64).ok_or_else(|| bad("page count overflows"))?;
        if f.tags.off != pages_len {
            return Err(bad("the tag table does not follow the pages"));
        }
        if f.tags.len % TAG_ROW as u64 != 0 {
            return Err(bad("tag table length is not a whole number of rows"));
        }
        let fences_at = f.tags.off.checked_add(f.tags.len).ok_or_else(|| bad("tag table length overflows"))?;
        if f.fences.off != fences_at {
            return Err(bad("the fence table does not follow the tag table"));
        }
        if Some(f.fences.len) != f.pages.checked_mul(FENCE as u64) {
            return Err(bad("fence table length does not match the page count"));
        }
        let filter_at = f.fences.off.checked_add(f.fences.len).ok_or_else(|| bad("fence table length overflows"))?;
        if f.filter.off != filter_at {
            return Err(bad("the filter does not follow the fence table"));
        }
        let end = f.filter.off.checked_add(f.filter.len).and_then(|e| e.checked_add(FOOTER as u64));
        if end != Some(len) {
            return Err(bad("the blocks do not end at the footer"));
        }
        match f.filter_type {
            FILTER_NONE if f.filter.len == 0 && f.bits_per_key == 0 => {}
            FILTER_NONE => return Err(bad("filter type none with a filter")),
            FILTER_BLOOM => {
                // The writer sizes the filter from an upper bound of the
                // entry count, so the filter is at least the size for the
                // real count, in whole blocks.
                let min = bloom::blocks_for(f.entries, f.bits_per_key).and_then(|b| b.checked_mul(bloom::BLOCK_BYTES as u64));
                if f.bits_per_key == 0 || f.filter.len % bloom::BLOCK_BYTES as u64 != 0 || min.is_none_or(|m| f.filter.len < m) {
                    return Err(bad("the filter length does not fit its parameters"));
                }
            }
            t => return Err(Self::corrupt(num, format_args!("unknown filter type {t}"))),
        }
        let max_entries = f.pages.checked_mul(MAX_PER_PAGE as u64).ok_or_else(|| bad("page count overflows"))?;
        if f.entries < f.pages || f.entries > max_entries {
            return Err(bad("entry count does not fit the page count"));
        }
        if f.tombstones > f.entries {
            return Err(bad("more tombstones than entries"));
        }
        let no_puts = f.tombstones == f.entries;
        if no_puts != (f.min_pack == u64::MAX && f.max_pack == 0) || (!no_puts && f.min_pack > f.max_pack) {
            return Err(bad("bad pack range"));
        }
        // Every length is now below `len`, so the sizes below fit in RAM as
        // much as the file does.
        let pages = usize::try_from(f.pages).map_err(|_| bad("page count overflows"))?;

        let mut tb = vec![0u8; usize::try_from(f.tags.len).map_err(|_| bad("tag table too long"))?];
        Self::read(&*file, num, &mut tb, f.tags.off)?;
        if crc32c::crc32c(&tb) != f.tags.crc {
            return Err(bad("bad tag table CRC"));
        }
        let mut tags = Vec::with_capacity(tb.len() / TAG_ROW);
        let mut next = 0u64;
        for row in tb.chunks_exact(TAG_ROW) {
            let (tag, first, count) = (row[0], le_u64(row, 1), le_u64(row, 9));
            if tags.last().is_some_and(|t: &TagRange| t.tag >= tag) {
                return Err(bad("tags out of order"));
            }
            if first != next || count == 0 {
                return Err(bad("tag page ranges are not contiguous"));
            }
            next = first.checked_add(count).filter(|&n| n <= f.pages).ok_or_else(|| bad("tag page range past the pages"))?;
            tags.push(TagRange { tag, first: first as usize, count: count as usize });
        }
        if next != f.pages {
            return Err(bad("tag table does not cover the pages"));
        }

        let mut fb = vec![0u8; usize::try_from(f.fences.len).map_err(|_| bad("fence table too long"))?];
        Self::read(&*file, num, &mut fb, f.fences.off)?;
        if crc32c::crc32c(&fb) != f.fences.crc {
            return Err(bad("bad fence table CRC"));
        }
        let fences: Vec<u64> = fb.chunks_exact(FENCE).map(|c| u64::from_be_bytes(c.try_into().unwrap())).collect();
        debug_assert_eq!(fences.len(), pages);
        for t in &tags {
            if fences[t.first..t.first + t.count].windows(2).any(|w| w[0] > w[1]) {
                return Err(bad("fences out of order"));
            }
        }

        let filter = if f.filter_type == FILTER_BLOOM {
            let mut b = vec![0u8; usize::try_from(f.filter.len).map_err(|_| bad("filter too long"))?];
            Self::read(&*file, num, &mut b, f.filter.off)?;
            if crc32c::crc32c(&b) != f.filter.crc {
                return Err(bad("bad filter CRC"));
            }
            Some(Bloom::from_bytes(&b, f.bits_per_key).map_err(|e| Self::corrupt(num, e))?)
        } else {
            None
        };

        Ok(Run {
            file,
            map,
            num,
            len,
            entries: f.entries,
            tombstones: f.tombstones,
            min_pack: f.min_pack,
            max_pack: f.max_pack,
            tags,
            fences,
            filter,
        })
    }

    /// Bytes of the filter in RAM.
    pub fn filter_bytes(&self) -> u64 {
        self.filter.as_ref().map_or(0, |f| f.len() as u64)
    }

    /// Bytes of the fence table in RAM.
    pub fn fence_bytes(&self) -> u64 {
        (self.fences.len() * FENCE) as u64
    }

    pub fn num(&self) -> u64 {
        self.num
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    /// Entries, tombstones included.
    pub fn entries(&self) -> u64 {
        self.entries
    }

    /// Tombstone entries.
    pub fn tombstones(&self) -> u64 {
        self.tombstones
    }

    /// False if the filter (in RAM, checked at open) says that the run has
    /// no entry of `id`; true if it may have one, or has no filter.
    pub fn may_contain(&self, id: &ObjectId) -> bool {
        self.filter.as_ref().is_none_or(|f| f.may_contain(id.digest()))
    }

    /// The smallest and largest pack of the put entries; `None` if the run
    /// has only tombstones.
    #[allow(dead_code)] // Not used: a GC must not skip a run by its pack range.
    pub fn pack_range(&self) -> Option<(u64, u64)> {
        (self.min_pack <= self.max_pack).then_some((self.min_pack, self.max_pack))
    }

    /// The tag of page `page` from the tag table.
    fn tag_of(&self, page: usize) -> u8 {
        let i = self.tags.partition_point(|t| t.first + t.count <= page);
        self.tags[i].tag
    }

    /// Read page `page` (an index below the page count) into `buf` and
    /// check it all: the header, the zero bytes, the CRC, the tag, and the
    /// first key against the fence of the page.
    fn read_page<'b>(&self, page: usize, buf: &'b mut [u8]) -> Result<Page<'b>> {
        let at = |what: &str| Self::corrupt(self.num, format_args!("page {page}: {what}"));
        Self::read(&*self.file, self.num, buf, (page * PAGE) as u64)?;
        let p = Page::parse(buf).ok_or_else(|| at("bad page header"))?;
        if p.b[PAGE_HEADER + p.count * p.w..].iter().any(|&b| b != 0) {
            return Err(at("bytes that must be zero are not"));
        }
        if !p.crc_ok() {
            return Err(at("bad CRC"));
        }
        if p.tag != self.tag_of(page) {
            return Err(at("the tag is not that of the tag table"));
        }
        let fence = self.fences[page].to_be_bytes();
        let shared = p.shape.shared as usize;
        if p.key(0)[..FENCE - shared] != fence[shared..] {
            return Err(at("the first key is not the fence"));
        }
        Ok(p)
    }

    /// The tag range of `id` and the page whose fence is the last one at
    /// or below its prefix; `None` if the run has no page that can hold it.
    #[inline]
    fn locate(&self, id: &ObjectId) -> Option<(TagRange, usize)> {
        let t = self.tags[self.tags.binary_search_by_key(&id.tag(), |t| t.tag).ok()?];
        let after = upper_bound(&self.fences[t.first..t.first + t.count], prefix(id));
        (after > 0).then(|| (t, t.first + after - 1))
    }

    /// True if the leading `shared` digest bytes of `id` are those of page
    /// `page` (the first bytes of its fence): else the page cannot hold it.
    #[inline]
    fn same_shared(&self, id: &ObjectId, page: usize, shared: usize) -> bool {
        id.digest()[..shared] == self.fences[page].to_be_bytes()[..shared]
    }

    /// The entry of `id`, if the run has one. The filter answers most
    /// misses with no page access. Else one page, read in place from the
    /// mapping (no `read_at`, no copy) or with one `read_at`; a key whose
    /// 8-byte prefix equals a page fence can need the page before it too.
    ///
    /// No page CRC on a hit (see the module comment). What a damaged page
    /// can do, and why no data is lost:
    /// - A wrong entry for `id` (a damaged pack, offset or length, or a
    ///   damaged width that shifts the fields): `read` checks the record
    ///   CRC, the ID and the hash, so it never returns wrong bytes; it then
    ///   checks the entry with [`Run::get_verified`] and asks for a rebuild
    ///   of the index. A `put` takes the wrong entry as a dedup hit; the
    ///   object's bytes are still in their pack, and the rebuild finds them.
    /// - A miss of an ID that the filter let through: the page CRC is
    ///   checked before `None` is returned (about 1 % of the misses per
    ///   run), and damage is `Corrupt`. So a damaged tombstone never lets
    ///   the search go on to an older run whose put entry it hides, and a
    ///   damaged ID is not taken as "not in this run".
    /// - A damaged flag byte, entry count, tag, `shared` or width that the
    ///   header checks catch: `Corrupt` (the flags must be those of a put
    ///   or a tombstone, and a put cannot point to offset 0, which is what a
    ///   tombstone with its flag bit lost looks like).
    pub fn get(&self, id: &ObjectId) -> Result<Option<Entry>> {
        if self.filter.as_ref().is_some_and(|f| !f.may_contain(id.digest())) {
            return Ok(None);
        }
        let Some((t, page)) = self.locate(id) else { return Ok(None) };
        match &self.map {
            Some(m) => self.search(id, t, page, &mut Mapped(m.bytes())),
            None => self.search(id, t, page, &mut Buffered { run: self, buf: [0; PAGE] }),
        }
    }

    /// The search of [`Run::get`] from `page`, over pages from `pages`.
    #[inline]
    fn search<P: Pages>(&self, id: &ObjectId, t: TagRange, mut page: usize, pages: &mut P) -> Result<Option<Entry>> {
        let p = prefix(id);
        let end = t.first + t.count;
        loop {
            let at = |what: &str| Self::corrupt(self.num, format_args!("page {page}: {what}"));
            let pg = Page::parse(pages.page(page)?).ok_or_else(|| at("bad page header"))?;
            if pg.tag != t.tag {
                return Err(at("the tag is not that of the tag table"));
            }
            let hi = if page + 1 < end { self.fences[page + 1] } else { u64::MAX };
            if self.same_shared(id, page, pg.shape.shared as usize) {
                if let Some(i) = pg.search(id, self.fences[page], hi) {
                    return pg.entry(i).map(Some).ok_or_else(|| at("bad entry"));
                }
            }
            // A miss after the filter said "maybe": check the page, so that
            // damage is never taken as "not in this run".
            if !pg.crc_ok() {
                return Err(at("bad CRC"));
            }
            // Keys with the same 8-byte prefix can sit on both sides of a
            // page boundary: look in the page before.
            if page > t.first && self.fences[page] == p {
                page -= 1;
            } else {
                return Ok(None);
            }
        }
    }

    /// The entry of `id` like [`Run::get`], but every page is read with
    /// `read_at` and fully checked (see [`Run::read_page`]): damage is
    /// always `Corrupt`. For the check of an entry whose record a read
    /// rejected.
    pub fn get_verified(&self, id: &ObjectId) -> Result<Option<Entry>> {
        let Some((t, mut page)) = self.locate(id) else { return Ok(None) };
        let mut buf = vec![0u8; PAGE];
        loop {
            let p = self.read_page(page, &mut buf)?;
            let shared = p.shape.shared as usize;
            if self.same_shared(id, page, shared) {
                let want = &id.digest()[shared..];
                if let Some(i) = (0..p.count).find(|&i| p.key(i) == want) {
                    let at = || Self::corrupt(self.num, format_args!("page {page}: bad entry"));
                    return p.entry(i).map(Some).ok_or_else(at);
                }
            }
            if page > t.first && self.fences[page] == prefix(id) {
                page -= 1;
            } else {
                return Ok(None);
            }
        }
    }

    /// Every entry in ID order, read one page at a time. A damaged page
    /// yields `Corrupt` and ends the iteration.
    pub fn iter(&self) -> RunIter<'_> {
        RunIter { run: self, page: 0, buf: vec![0u8; PAGE], count: 0, next: 0, seen: 0, last: None, done: false }
    }
}

/// The number of values in sorted `f` that are at or below `p` (the same
/// as `f.partition_point(|&x| x <= p)`). Interpolation search, because the
/// fences are prefixes of uniform digests: about 3 to 4 probes in place of
/// 14 to 17 for a binary search (17-05 mmap spike). The guess uses f64
/// arithmetic: a 128-bit division is a slow library call that costs more
/// than the probes it saves. After a few steps (keys that are not uniform)
/// a binary search ends the work, so the cost stays near that of a binary
/// search.
#[inline]
fn upper_bound(f: &[u64], p: u64) -> usize {
    // f[..lo] <= p < f[hi..]; vlo <= p < vhi bound the values in between.
    let (mut lo, mut hi) = (0usize, f.len());
    let (mut vlo, mut vhi) = (0u64, u64::MAX);
    let mut steps = 0;
    while hi - lo > 8 && steps < 6 {
        steps += 1;
        let frac = (p - vlo) as f64 / ((vhi - vlo) as f64 + 1.0);
        let g = (lo + (frac * (hi - lo) as f64) as usize).min(hi - 1);
        if f[g] <= p {
            lo = g + 1;
            vlo = f[g];
        } else {
            hi = g;
            vhi = f[g];
        }
    }
    lo + f[lo..hi].partition_point(|&x| x <= p)
}

/// Where [`Run::get`] takes its pages from.
trait Pages {
    /// The `PAGE` bytes of page `page` (an index below the page count).
    fn page(&mut self, page: usize) -> Result<&[u8]>;
}

/// Pages in place in the mapping of the run file.
struct Mapped<'a>(&'a [u8]);

impl Pages for Mapped<'_> {
    #[inline]
    fn page(&mut self, page: usize) -> Result<&[u8]> {
        Ok(&self.0[page * PAGE..(page + 1) * PAGE])
    }
}

/// Pages read with `read_at` into a buffer (a strategy without a mapping).
struct Buffered<'a> {
    run: &'a Run,
    buf: [u8; PAGE],
}

impl Pages for Buffered<'_> {
    fn page(&mut self, page: usize) -> Result<&[u8]> {
        Run::read(&*self.run.file, self.run.num, &mut self.buf, (page * PAGE) as u64)?;
        Ok(&self.buf)
    }
}

/// See [`Run::iter`].
pub(crate) struct RunIter<'a> {
    run: &'a Run,
    /// The next page to read.
    page: usize,
    buf: Vec<u8>,
    /// Entries in `buf`, and the next one to yield.
    count: usize,
    next: usize,
    /// Entries yielded so far.
    seen: u64,
    last: Option<ObjectId>,
    done: bool,
}

impl RunIter<'_> {
    fn step(&mut self) -> Result<Option<(ObjectId, Entry)>> {
        if self.next == self.count {
            if self.page == self.run.fences.len() {
                if self.seen != self.run.entries {
                    return Err(Run::corrupt(self.run.num, "the pages hold another entry count than the footer"));
                }
                return Ok(None);
            }
            self.count = self.run.read_page(self.page, &mut self.buf)?.count;
            self.next = 0;
            self.page += 1;
        }
        let page = self.page - 1;
        // The page was checked by `read_page`; parse it again (cheap).
        let p = Page::parse(&self.buf).expect("a checked page");
        let shared = p.shape.shared as usize;
        let mut digest = [0u8; DIGEST_LEN];
        digest[..shared].copy_from_slice(&self.run.fences[page].to_be_bytes()[..shared]);
        digest[shared..].copy_from_slice(p.key(self.next));
        let id = ObjectId::new(p.tag, digest);
        let at = |what: &str| Run::corrupt(self.run.num, format_args!("page {page}: {what}"));
        let e = p.entry(self.next).ok_or_else(|| at("bad entry"))?;
        if self.last.is_some_and(|l| id <= l) {
            return Err(at("entries out of order"));
        }
        self.last = Some(id);
        self.next += 1;
        self.seen += 1;
        Ok(Some((id, e)))
    }
}

impl Iterator for RunIter<'_> {
    type Item = Result<(ObjectId, Entry)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let r = self.step();
        if !matches!(r, Ok(Some(_))) {
            self.done = true;
        }
        r.transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::MemIo;
    use crate::pack_io::{FileIo, Kind};
    use crate::retry::RetryingIo;
    use crate::DIGEST_LEN;
    use std::sync::Mutex;

    /// A read handle that records the offset and length of every read.
    struct Counting {
        inner: Arc<dyn PackRead>,
        reads: Mutex<Vec<(u64, usize)>>,
    }

    impl PackRead for Counting {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
            self.reads.lock().unwrap().push((offset, buf.len()));
            self.inner.read_at(buf, offset)
        }
    }

    impl Counting {
        fn take(&self) -> Vec<(u64, usize)> {
            std::mem::take(&mut *self.reads.lock().unwrap())
        }
    }

    /// An ID whose digest bytes 0..8 are `i << 20` (so every key has its own
    /// 8-byte prefix and keys in between are free).
    fn key(tag: u8, i: u64) -> ObjectId {
        let mut d = [0u8; DIGEST_LEN];
        d[..8].copy_from_slice(&(i << 20).to_be_bytes());
        d[8..16].copy_from_slice(&i.to_le_bytes());
        ObjectId::new(tag, d)
    }

    fn put(i: u64) -> Entry {
        Entry::Put(Loc { pack: 1 + i % 5, offset: 14 + i * 100, stored: i as u32, raw: 2 * i as u32, flags: (i % 2) as u8 })
    }

    fn write_run(io: &dyn PackIo, num: u64, entries: &[(ObjectId, Entry)]) -> RunInfo {
        write_run_bits(io, num, entries, 10)
    }

    fn write_run_bits(io: &dyn PackIo, num: u64, entries: &[(ObjectId, Entry)], bits: u8) -> RunInfo {
        let mut w = RunWriter::create(io, num, entries.len() as u64, bits).unwrap();
        for (id, e) in entries {
            w.push(id, e).unwrap();
        }
        w.finish().unwrap()
    }

    fn open_counting(io: &dyn PackIo, num: u64, len: u64) -> (Run, Arc<Counting>) {
        let c = Arc::new(Counting { inner: io.open_run(num).unwrap(), reads: Mutex::new(Vec::new()) });
        let run = Run::open(Arc::clone(&c) as Arc<dyn PackRead>, io.map_run(num).unwrap(), num, len).unwrap();
        (run, c)
    }

    // Tracer: a run written through create_run is found again through
    // open_run, on each I/O strategy: FileIo maps the run, and a hit reads
    // its page in place (no `read_at`); the others read one page per hit.
    #[test]
    fn a_run_round_trips_through_each_io() {
        let dir = tempfile::tempdir().unwrap();
        let files = FileIo::new(dir.path().join("packs"));
        files.prepare(false).unwrap();
        let retrying = RetryingIo::with_backoff(Arc::new(MemIo::new()), Vec::new());
        let ios: [(&str, &dyn PackIo); 3] = [("mem", &MemIo::new()), ("files", &files), ("retrying", &retrying)];

        let entries: Vec<(ObjectId, Entry)> = (0..1000).map(|i| (key(0x1e, i), put(i))).collect();
        for (name, io) in ios {
            let mapped = name == "files";
            let info = write_run(io, 1, &entries);
            assert_eq!(io.map_run(1).unwrap().is_some(), mapped, "{name}");
            assert_eq!((info.entries, info.tombstones), (1000, 0), "{name}");
            let (run, c) = open_counting(io, 1, info.len);
            let pages = run.fences.len() as u64;
            let opened = c.take();
            assert!(!opened.is_empty(), "{name}");
            assert!(opened.iter().all(|&(o, _)| o >= pages * PAGE as u64), "{name}: open read a page: {opened:?}");
            assert_eq!(run.entries(), 1000, "{name}");
            assert_eq!(run.pack_range(), Some((1, 5)), "{name}");
            for (id, e) in &entries {
                assert_eq!(run.get(id).unwrap(), Some(*e), "{name}");
                assert_eq!(c.take().len(), usize::from(!mapped), "{name}: reads for one hit");
            }
            for i in [0, 1, 500, 999, 5000] {
                let mut absent = key(0x1e, i);
                absent.0[8] |= 1; // between i and i + 1
                assert_eq!(run.get(&absent).unwrap(), None, "{name}");
                assert!(c.take().len() <= usize::from(!mapped), "{name}: reads for one miss");
            }
            assert_eq!(run.get(&key(0x2f, 3)).unwrap(), None, "{name}: another tag");
            assert_eq!(c.take().len(), 0, "{name}: no read for a tag that is not in the run");
            assert_eq!(io.list_runs().unwrap(), vec![1], "{name}");
            // The verified path reads (and checks) one page with `read_at`.
            assert_eq!(run.get_verified(&entries[500].0).unwrap(), Some(entries[500].1), "{name}");
            assert_eq!(c.take().len(), 1, "{name}: get_verified reads its page");
            if mapped {
                // The run owns its mapping: a removed file (a merge or GC
                // deleted it) still reads through an open run.
                io.remove(1, Kind::Run).unwrap();
                assert!(!io.exists(1, Kind::Run));
                for (id, e) in entries.iter().step_by(37) {
                    assert_eq!(run.get(id).unwrap(), Some(*e), "{name}: after the file was removed");
                }
                write_run(io, 1, &entries);
            }
        }
        assert!(dir.path().join("index").join("0000000000000001.run").is_file());
        assert!(!dir.path().join("packs").join("0000000000000001.run").exists());
        assert_eq!(files.list().unwrap(), Vec::<u64>::new(), "a run is not a pack");
    }

    fn is_corrupt<T>(r: &Result<T>) -> bool {
        matches!(r, Err(Error::Corrupt(_)))
    }

    /// Check a whole run: `iter` gives `entries` in order, `get` finds each.
    fn check_run(run: &Run, entries: &[(ObjectId, Entry)]) {
        let read: Vec<(ObjectId, Entry)> = run.iter().map(|r| r.unwrap()).collect();
        assert_eq!(read, entries);
        for (id, e) in entries {
            assert_eq!(run.get(id).unwrap(), Some(*e), "{id:?}");
        }
    }

    fn mem_run(entries: &[(ObjectId, Entry)]) -> (MemIo, Run, RunInfo) {
        let io = MemIo::new();
        let info = write_run(&io, 1, entries);
        let run = Run::open(io.open_run(1).unwrap(), io.map_run(1).unwrap(), 1, info.len).unwrap();
        (io, run, info)
    }

    /// Bytes of the file length a run of `pages` pages, `tags` tags and `n`
    /// entries has (10 bits per key).
    fn run_len(pages: u64, tags: u64, n: u64) -> u64 {
        let filter = (n * 10).div_ceil(512).max(1) * 64;
        pages * PAGE as u64 + tags * TAG_ROW as u64 + pages * FENCE as u64 + filter + FOOTER as u64
    }

    /// The layout of page `page` of the run bytes `b`: entry count, bytes
    /// of one entry, bytes of its key part, and the shape.
    fn layout(b: &[u8], page: usize) -> (usize, usize, usize, Shape) {
        let p = Page::parse(&b[page * PAGE..(page + 1) * PAGE]).unwrap();
        (p.count, p.w, p.shape.key_len(), p.shape)
    }

    /// The entry counts of the pages of run 1 in `io`.
    fn page_counts(io: &MemIo, run: &Run) -> Vec<usize> {
        let b = io.read(1, Kind::Run).unwrap();
        (0..run.fences.len()).map(|p| layout(&b, p).0).collect()
    }

    #[test]
    fn runs_of_each_size_round_trip() {
        for n in [0u64, 1, 70, 71, 120, 121, 10_000] {
            let entries: Vec<(ObjectId, Entry)> = (0..n).map(|i| (key(0x1e, i), put(i))).collect();
            let (io, run, info) = mem_run(&entries);
            let pages = run.fences.len() as u64;
            assert_eq!(info.len, run_len(pages, (n > 0) as u64, n), "n {n}");
            assert_eq!(page_counts(&io, &run).iter().sum::<usize>() as u64, n);
            assert!(pages <= n.div_ceil(70), "n {n}: {pages} pages; the 58-byte entries needed more");
            assert_eq!(run.entries(), n);
            check_run(&run, &entries);
        }
    }

    #[test]
    fn each_tag_starts_a_new_page() {
        let mut entries: Vec<(ObjectId, Entry)> = (0..100).map(|i| (key(0x1e, i), put(i))).collect();
        entries.extend((0..50).map(|i| (key(0xf0, i), put(i))));
        let io = MemIo::new();
        let info = write_run(&io, 1, &entries);
        let (run, c) = open_counting(&io, 1, info.len);
        c.take();
        // The 100 entries of tag 0x1e fit in one page; the 50 of tag 0xf0
        // start a new page.
        assert_eq!(page_counts(&io, &run), vec![100, 50]);
        assert_eq!(info.len, run_len(2, 2, 150));
        check_run(&run, &entries);
        c.take();
        assert_eq!(run.get(&key(0xf0, 7)).unwrap(), Some(put(7)));
        assert_eq!(c.take(), vec![(PAGE as u64, PAGE)], "the 0xf0 key is on page 1");
        assert_eq!(run.get(&key(0x1e, 99)).unwrap(), Some(put(99)));
        assert_eq!(c.take(), vec![(0, PAGE)], "the last 0x1e key is on page 0");
    }

    #[test]
    fn keys_with_the_prefix_of_a_page_fence_are_found() {
        // Keys 60..360 share one 8-byte prefix: more than a page, so a page
        // boundary falls inside them, and a page starts with that prefix.
        let mut entries: Vec<(ObjectId, Entry)> = (0..500)
            .map(|i| {
                let mut id = key(0x1e, if (60..360).contains(&i) { 60 } else { i });
                id.0[9..11].copy_from_slice(&(i as u16).to_be_bytes()); // digest bytes 8..10: apart and sorted
                (id, put(i))
            })
            .collect();
        entries.sort_by_key(|(id, _)| *id);
        let (io, run, _) = mem_run(&entries);
        check_run(&run, &entries);
        let counts = page_counts(&io, &run);
        let first = counts[0];
        assert!(first > 60 && first < 360, "{counts:?}");
        assert_eq!(run.fences[1], prefix(&entries[first].0));
        assert_eq!(run.fences[1], prefix(&entries[first - 1].0));
        let mut absent = entries[first - 5].0;
        absent.0[12] = 0xff;
        assert_eq!(run.get(&absent).unwrap(), None);
        assert_eq!(run.get_verified(&absent).unwrap(), None);
        for (id, e) in &entries {
            assert_eq!(run.get_verified(id).unwrap(), Some(*e));
        }

        // All keys with one prefix: every fence is equal.
        let entries: Vec<(ObjectId, Entry)> = (0..400u64)
            .map(|i| {
                let mut id = key(0x1e, 5);
                id.0[9..17].copy_from_slice(&i.to_be_bytes());
                (id, put(i))
            })
            .collect();
        let (_io, run, _) = mem_run(&entries);
        assert!(run.fences.len() > 2);
        check_run(&run, &entries);
    }


    #[test]
    fn tombstones_round_trip_and_are_counted() {
        let entries: Vec<(ObjectId, Entry)> =
            (0..300).map(|i| (key(0x1e, i), if i % 3 == 0 { Entry::Tomb(7 + i) } else { put(i) })).collect();
        let (_io, run, info) = mem_run(&entries);
        assert_eq!((info.entries, info.tombstones), (300, 100));
        assert_eq!((run.entries(), run.tombstones()), (300, 100));
        check_run(&run, &entries);
        assert_eq!(run.get(&key(0x1e, 3)).unwrap(), Some(Entry::Tomb(10)));

        let only: Vec<(ObjectId, Entry)> = (0..10).map(|i| (key(0x1e, i), Entry::Tomb(i))).collect();
        let (_io, run, _) = mem_run(&only);
        assert_eq!(run.pack_range(), None);
        check_run(&run, &only);
    }

    #[test]
    fn the_writer_refuses_keys_out_of_order() {
        let io = MemIo::new();
        let mut w = RunWriter::create(&io, 1, 3, 10).unwrap();
        w.push(&key(0x1e, 2), &put(2)).unwrap();
        assert!(w.push(&key(0x1e, 2), &put(2)).is_err(), "a duplicate");
        assert!(w.push(&key(0x1e, 1), &put(1)).is_err(), "a smaller key");
        let tomb_flag = Entry::Put(Loc { pack: 1, offset: 0, stored: 0, raw: 0, flags: TOMB });
        assert!(w.push(&key(0x1e, 3), &tomb_flag).is_err(), "a put with the tombstone flag");
    }

    /// Open a run made of `bytes`.
    fn open_bytes(bytes: &[u8]) -> Result<Run> {
        let io = MemIo::new();
        io.write_durable(1, Kind::Run, bytes).unwrap();
        Run::open(io.open_run(1).unwrap(), io.map_run(1).unwrap(), 1, bytes.len() as u64)
    }

    /// `bytes` with the footer changed by `f`, and its CRC made right again.
    fn refooter(bytes: &[u8], f: impl FnOnce(&mut [u8])) -> Vec<u8> {
        let mut b = bytes.to_vec();
        let start = b.len() - FOOTER;
        let foot = &mut b[start..];
        f(foot);
        let crc = crc32c::crc32c(&foot[..FOOTER - 4]);
        foot[FOOTER - 4..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    fn set_u64(b: &mut [u8], at: usize, v: u64) {
        b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    // Footer fields: entries 8, tombstones 16, pages 24; sections at 32
    // (tags), 52 (fences), 72 (filter): offset, length, CRC.
    #[test]
    fn a_damaged_footer_or_table_is_refused() {
        let entries: Vec<(ObjectId, Entry)> = (0..200).map(|i| (key(0x1e, i), put(i))).collect();
        let io = MemIo::new();
        write_run(&io, 1, &entries);
        let good = io.read(1, Kind::Run).unwrap();
        assert!(open_bytes(&good).is_ok());
        assert!(open_bytes(&refooter(&good, |_| {})).is_ok(), "refooter alone changes nothing");

        let np = le_u64(&good, good.len() - FOOTER + 24) as usize;
        assert!(np >= 2, "{np} pages");
        let max_entries = u64::MAX / MIN_ENTRY as u64 + 1;
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("entry count u64::MAX", refooter(&good, |f| set_u64(f, 8, u64::MAX))),
            ("entry count past the pages", refooter(&good, |f| set_u64(f, 8, (np * MAX_PER_PAGE) as u64 + 1))),
            ("entry count that overflows bytes", refooter(&good, |f| set_u64(f, 8, max_entries))),
            ("tombstones above entries", refooter(&good, |f| set_u64(f, 16, 201))),
            ("page count 1 << 63", refooter(&good, |f| set_u64(f, 24, 1 << 63))),
            ("page count u64::MAX", refooter(&good, |f| set_u64(f, 24, u64::MAX))),
            ("page count one more", refooter(&good, |f| set_u64(f, 24, np as u64 + 1))),
            ("tag table length past the end", refooter(&good, |f| set_u64(f, 40, good.len() as u64))),
            ("tag table length u64::MAX", refooter(&good, |f| set_u64(f, 40, u64::MAX))),
            ("fence table length past the end", refooter(&good, |f| set_u64(f, 60, u64::MAX - 7))),
            ("fence table offset past the end", refooter(&good, |f| set_u64(f, 52, u64::MAX))),
            ("filter length past the end", refooter(&good, |f| set_u64(f, 80, 128))),
            ("filter length shorter than its parameters", {
                // 200 keys at 10 bits need 4 blocks: a footer that says 2
                // blocks, with the file cut to match.
                let mut b = good[..good.len() - FOOTER - 128].to_vec();
                b.extend_from_slice(&good[good.len() - FOOTER..]);
                refooter(&b, |f| set_u64(f, 80, 128))
            }),
            ("filter length not whole blocks", {
                let mut b = good[..good.len() - FOOTER - 8].to_vec();
                b.extend_from_slice(&good[good.len() - FOOTER..]);
                refooter(&b, |f| set_u64(f, 80, 256 - 8))
            }),
            ("filter type none with a filter", refooter(&good, |f| f[6] = FILTER_NONE)),
            ("bloom with 0 bits per key", refooter(&good, |f| f[7] = 0)),
            ("unknown filter type", refooter(&good, |f| f[6] = 9)),
            ("wrong magic", refooter(&good, |f| f[0] = b'X')),
            ("unknown version", refooter(&good, |f| f[4..6].copy_from_slice(&3u16.to_le_bytes()))),
            ("bad pack range", refooter(&good, |f| set_u64(f, 92, u64::MAX))),
            ("bad footer CRC", {
                let mut b = good.clone();
                let at = b.len() - FOOTER + 9;
                b[at] ^= 1;
                b
            }),
            ("bad tag table CRC", {
                let mut b = good.clone();
                b[np * PAGE] ^= 1;
                b
            }),
            ("bad fence table CRC", {
                let mut b = good.clone();
                b[np * PAGE + TAG_ROW] ^= 1;
                b
            }),
            ("bad filter CRC", {
                let mut b = good.clone();
                b[np * PAGE + TAG_ROW + np * FENCE + 5] ^= 1;
                b
            }),
            ("shorter than a footer", good[..FOOTER - 1].to_vec()),
            ("cut at the front", good[PAGE..].to_vec()),
        ];
        for (what, bytes) in cases {
            assert!(is_corrupt(&open_bytes(&bytes)), "{what}");
        }

        // A length past the end of the file: a short read is damage too.
        let io = MemIo::new();
        io.write_durable(1, Kind::Run, &good).unwrap();
        assert!(is_corrupt(&Run::open(io.open_run(1).unwrap(), io.map_run(1).unwrap(), 1, good.len() as u64 + PAGE as u64)));
    }

    /// The contract of a damaged page (the lookups check no page CRC on a
    /// hit; see `Run::get`): `get` never gives the entry of another ID and
    /// never a false miss; it gives the right entry, `Corrupt`, or (only for
    /// the damaged entry itself) that entry with damaged place fields. The
    /// verified reads (`get_verified`, `iter`) are always `Corrupt`.
    #[test]
    fn a_damaged_page_gives_no_false_miss_and_the_verified_reads_are_corrupt() {
        let entries: Vec<(ObjectId, Entry)> = (0..400).map(|i| (key(0x1e, i), put(i))).collect();
        let (io, run, _) = mem_run(&entries);
        let b = io.read(1, Kind::Run).unwrap();
        let counts = page_counts(&io, &run);
        assert!(counts.len() >= 3, "{counts:?}");
        let (c0, c1) = (counts[0] as u64, counts[1] as u64);
        let (_, w, klen, shape) = layout(&b, 1);
        // A key byte of entry c0 + 5 (page 1 holds entries c0..c0 + c1).
        let bad = c0 + 5;
        io.flip(1, Kind::Run, PAGE + PAGE_HEADER + 5 * w + klen - 3);
        assert!(is_corrupt(&run.get(&key(0x1e, bad))), "the damaged key is not a miss");
        assert!(is_corrupt(&run.get_verified(&key(0x1e, bad))));
        assert!(is_corrupt(&run.get_verified(&key(0x1e, c0 + 30))), "a key on the damaged page");
        for i in c0..c0 + c1 {
            match run.get(&key(0x1e, i)) {
                Ok(e) => assert_eq!(e, Some(put(i)), "key {i}"),
                Err(e) => assert!(matches!(e, Error::Corrupt(_)), "key {i}"),
            }
        }
        assert_eq!(run.get(&key(0x1e, c0 + 30)).unwrap(), Some(put(c0 + 30)), "an intact entry on the page: a hit");
        assert_eq!(run.get(&key(0x1e, 10)).unwrap(), Some(put(10)), "other pages still read");
        assert_eq!(run.get(&key(0x1e, 399)).unwrap(), Some(put(399)));
        let mut it = run.iter();
        for i in 0..c0 {
            assert_eq!(it.next().unwrap().unwrap(), (key(0x1e, i), put(i)));
        }
        assert!(is_corrupt(&it.next().unwrap()), "the damaged page");
        assert!(it.next().is_none(), "iter stops after damage");

        // Damage in each byte of the page header, in the CRC and in the
        // zero padding of the last page: a hit gives the entry of its own
        // key (the right one, or with damaged place fields when a width
        // changed) or Corrupt, never a miss; the verified reads are Corrupt.
        let last = counts.len() - 1;
        let target = 399;
        for at in (0..PAGE_HEADER).map(|i| last * PAGE + i).chain([(last + 1) * PAGE - 1]) {
            let (io, run, _) = mem_run(&entries);
            io.flip(1, Kind::Run, at);
            match run.get(&key(0x1e, target)) {
                Ok(e) => assert!(e.is_some(), "byte {at}: never a miss"),
                Err(e) => assert!(matches!(e, Error::Corrupt(_)), "byte {at}"),
            }
            assert!(is_corrupt(&run.get_verified(&key(0x1e, target))), "byte {at}");
            assert!(run.iter().any(|r| is_corrupt(&r)), "byte {at}");
            let mut absent = key(0x1e, target);
            absent.0[8] |= 1;
            if at % PAGE < PAGE_HEADER {
                assert!(is_corrupt(&run.get(&absent)), "byte {at}: a miss on the damaged page");
            }
        }
        // The count, tag, shared and CRC bytes: never the entry itself
        // with other fields (only the widths can shift the fields).
        for i in [0, 2, 3, 5] {
            let (io, run, _) = mem_run(&entries);
            io.flip(1, Kind::Run, last * PAGE + i);
            match run.get(&key(0x1e, target)) {
                Ok(e) => assert_eq!(e, Some(put(target)), "header byte {i}"),
                Err(e) => assert!(matches!(e, Error::Corrupt(_)), "header byte {i}"),
            }
        }

        // A damaged place field of an entry: that entry, with the damage
        // (the pack read rejects it; see `ChunkStore::read`).
        let (io, run, _) = mem_run(&entries);
        let (_, w, klen, shape0) = layout(&b, 0);
        io.flip(1, Kind::Run, PAGE_HEADER + 3 * w + klen + shape0.pack as usize);
        assert_ne!(run.get(&key(0x1e, 3)).unwrap(), Some(put(3)));
        assert!(is_corrupt(&run.get_verified(&key(0x1e, 3))));
        let _ = shape;
    }

    #[test]
    fn a_damaged_tombstone_never_reads_as_a_miss_or_a_put() {
        let entries: Vec<(ObjectId, Entry)> = hashed_entries(100).into_iter().map(|(id, _)| (id, Entry::Tomb(3))).collect();
        let io = MemIo::new();
        write_run(&io, 1, &entries);
        let good = io.read(1, Kind::Run).unwrap();
        let (count, w, klen, _) = layout(&good, 0);
        assert_eq!(count, 100);
        let at = |i: usize| PAGE_HEADER + i * w;
        let flags = at(40) + w - 1;
        // A digest byte, all flag bits, and only the tombstone bit.
        let cases: [(usize, u8); 3] = [(at(40) + klen - 12, 0x04), (flags, 0xff), (flags, TOMB)];
        for (byte, mask) in cases {
            let mut b = good.clone();
            b[byte] ^= mask;
            let run = open_bytes(&b).unwrap();
            let got = run.get(&entries[40].0);
            assert!(is_corrupt(&got), "byte {byte} mask {mask:#x}: {got:?}");
            assert_eq!(run.get(&entries[41].0).unwrap(), Some(Entry::Tomb(3)));
        }
    }

    /// The widths of the place fields follow the values: from 0 bytes up to
    /// 8 (a pack or offset of `u64::MAX`), and the raw length takes no byte
    /// in a page where each raw length is the stored length.
    #[test]
    fn place_fields_of_each_width_round_trip() {
        let values: Vec<u64> = vec![0, 1, 0xff, 0x100, 0xffff, 0x1_0000, 0xff_ffff, u32::MAX as u64, 1 << 32, 1 << 40, u64::MAX];
        let mut entries = Vec::new();
        let mut i = 0u64;
        for &a in &values {
            for &b in &values {
                let small = |v: u64| (v & 0xffff_ffff) as u32;
                let loc = Loc { pack: a, offset: b.max(1), stored: small(b), raw: small(a), flags: (i % 2) as u8 };
                let e = if i.is_multiple_of(7) { Entry::Tomb(a) } else { Entry::Put(loc) };
                entries.push((hashed(3, i), e));
                i += 1;
            }
        }
        entries.sort_by_key(|(id, _)| *id);
        let (_io, run, _) = mem_run(&entries);
        check_run(&run, &entries);
        for (id, e) in &entries {
            assert_eq!(run.get_verified(id).unwrap(), Some(*e));
        }

        // One page of each kind: widths 0 (tombstones of pack 0), raw same
        // as stored, and raw different.
        let only = |f: &dyn Fn(u64) -> Entry| -> (MemIo, Run, Vec<(ObjectId, Entry)>) {
            let e: Vec<(ObjectId, Entry)> = hashed_entries(50).into_iter().map(|(id, _)| id).zip(0..).map(|(id, i)| (id, f(i))).collect();
            let (io, run, _) = mem_run(&e);
            (io, run, e)
        };
        let (io, run, e) = only(&|_| Entry::Tomb(0));
        check_run(&run, &e);
        let (_, w, _, s) = layout(&io.read(1, Kind::Run).unwrap(), 0);
        assert_eq!((s.pack, s.offset, s.stored, s.raw, w), (0, 0, 0, RAW_SAME, DIGEST_LEN - s.shared as usize + 1));
        let same = |i: u64| Entry::Put(Loc { pack: 7, offset: 1000 + i, stored: 300, raw: 300, flags: 0 });
        let (io, run, e) = only(&same);
        check_run(&run, &e);
        let (_, _, _, s) = layout(&io.read(1, Kind::Run).unwrap(), 0);
        assert_eq!((s.pack, s.offset, s.stored, s.raw), (1, 2, 2, RAW_SAME));
        let diff = |i: u64| Entry::Put(Loc { pack: 7, offset: 1000 + i, stored: 300, raw: 70_000, flags: 1 });
        let (io, run, e) = only(&diff);
        check_run(&run, &e);
        let (_, _, _, s) = layout(&io.read(1, Kind::Run).unwrap(), 0);
        assert_eq!((s.pack, s.offset, s.stored, s.raw), (1, 2, 2, 3));
    }

    /// IDs whose digests differ only in their last byte, or share the first
    /// 8 bytes with the fence of the next page: each one is found, and IDs
    /// near them are misses.
    #[test]
    fn ids_that_share_most_of_their_bytes_are_kept_apart() {
        let mut entries = Vec::new();
        for g in 0..4u8 {
            for last in 0..=255u8 {
                let mut d = [0x5au8; DIGEST_LEN];
                d[0] = g;
                d[DIGEST_LEN - 1] = last;
                entries.push((ObjectId::new(0x1e, d), put(last as u64 + 1)));
            }
        }
        entries.sort_by_key(|(id, _)| *id);
        let (_io, run, _) = mem_run(&entries);
        check_run(&run, &entries);
        for (id, _) in entries.iter().step_by(17) {
            let mut near = *id;
            near.0[DIGEST_LEN - 1] ^= 0x5a; // another key of the same group: may exist
            let want = entries.binary_search_by_key(&near, |(k, _)| *k).ok().map(|i| entries[i].1);
            assert_eq!(run.get(&near).unwrap(), want);
            let mut absent = *id;
            absent.0[DIGEST_LEN - 2] ^= 1;
            assert_eq!(run.get(&absent).unwrap(), None);
            assert_eq!(run.get_verified(&absent).unwrap(), None);
        }
    }

    /// Bytes for each entry of a run with real places: about 41 bytes
    /// (58 with the fixed fields of run version 1).
    #[test]
    fn a_run_of_real_places_needs_about_41_bytes_for_each_entry() {
        let n = 200_000u64;
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut entries: Vec<(ObjectId, Entry)> = (0..n)
            .map(|i| {
                // 1,000 packs of 64 MiB, chunks of 2 to 32 KiB, half of them
                // compressed.
                let raw = 2048 + (next() % 30_720) as u32;
                let zstd = next() % 2 == 0;
                let stored = if zstd { raw / 2 } else { raw };
                let loc = Loc { pack: 1 + next() % 1000, offset: 14 + next() % (64 << 20), stored, raw, flags: zstd as u8 };
                (hashed(4, i), Entry::Put(loc))
            })
            .collect();
        entries.sort_by_key(|(id, _)| *id);
        let (_io, run, info) = mem_run(&entries);
        let pages = run.fences.len() as f64;
        let per_entry = pages * PAGE as f64 / n as f64;
        let per_object = info.len as f64 / n as f64;
        eprintln!("real places: {per_entry:.2} B of pages and {per_object:.2} B of run file for each entry");
        assert!(per_entry < 43.0, "{per_entry}");
        assert!(per_object < 44.5, "{per_object}");
        for (id, e) in entries.iter().step_by(97) {
            assert_eq!(run.get(id).unwrap(), Some(*e));
        }
    }

    #[test]
    fn the_interpolation_search_finds_what_a_binary_search_finds() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut sets: Vec<Vec<u64>> = vec![
            vec![],
            vec![5],
            vec![7; 40],
            (0..1000).map(|i| i << 20).collect(),
            (0..1000).map(|i| i * i * i).collect(),
            (0..500).map(|i| if i < 499 { i } else { u64::MAX }).collect(),
            vec![0, 0, 0, u64::MAX, u64::MAX],
        ];
        let mut uniform: Vec<u64> = (0..5000).map(|_| next()).collect();
        uniform.sort_unstable();
        sets.push(uniform);
        for f in &sets {
            let mut probes: Vec<u64> = vec![0, 1, u64::MAX, u64::MAX - 1];
            probes.extend(f.iter().flat_map(|&v| [v, v.wrapping_sub(1), v.wrapping_add(1)]));
            probes.extend((0..200).map(|_| next()));
            for p in probes {
                assert_eq!(upper_bound(f, p), f.partition_point(|&v| v <= p), "{p} in {} values", f.len());
            }
        }
    }

    /// An ID with a uniform digest (as BLAKE3 gives).
    fn hashed(seed: u64, i: u64) -> ObjectId {
        let mut h = blake3::Hasher::new();
        h.update(&seed.to_le_bytes());
        h.update(&i.to_le_bytes());
        ObjectId::new(0x1e, *h.finalize().as_bytes())
    }

    fn hashed_entries(n: u64) -> Vec<(ObjectId, Entry)> {
        let mut e: Vec<(ObjectId, Entry)> = (0..n).map(|i| (hashed(1, i), put(i))).collect();
        e.sort_by_key(|(id, _)| *id);
        e
    }

    #[test]
    fn a_run_with_a_filter_answers_misses_without_a_page_read() {
        let entries = hashed_entries(5000);
        let io = MemIo::new();
        let info = write_run(&io, 1, &entries);
        let (run, c) = open_counting(&io, 1, info.len);
        c.take();
        assert_eq!(run.filter_bytes(), (5000u64 * 10).div_ceil(512) * 64);
        assert_eq!(run.fence_bytes(), run.fences.len() as u64 * FENCE as u64);
        for i in 0..1000 {
            assert_eq!(run.get(&hashed(2, i)).unwrap(), None);
        }
        let reads = c.take().len();
        assert!(reads <= 30, "{reads} page reads for 1,000 absent IDs");
        for (id, e) in &entries {
            assert_eq!(run.get(id).unwrap(), Some(*e));
        }
        assert_eq!(c.take().len(), entries.len(), "one page read for each hit");
    }

    #[test]
    fn a_run_without_a_filter_round_trips() {
        let entries = hashed_entries(500);
        let io = MemIo::new();
        let info = write_run_bits(&io, 1, &entries, 0);
        let (run, c) = open_counting(&io, 1, info.len);
        assert_eq!(run.filter_bytes(), 0);
        check_run(&run, &entries);
        c.take();
        for i in 0..100 {
            assert_eq!(run.get(&hashed(2, i)).unwrap(), None);
        }
        let reads = c.take().len();
        assert!(reads >= 90, "without a filter almost every miss reads a page: {reads}");
    }

    #[test]
    fn tombstones_are_in_the_filter() {
        let entries: Vec<(ObjectId, Entry)> = hashed_entries(300).into_iter().map(|(id, _)| (id, Entry::Tomb(3))).collect();
        let (_io, run, _) = mem_run(&entries);
        for (id, _) in &entries {
            assert_eq!(run.get(id).unwrap(), Some(Entry::Tomb(3)));
        }
    }
}
