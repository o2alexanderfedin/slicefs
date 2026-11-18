//! Sorted runs of the store index: immutable files of (ObjectId, entry),
//! sorted by the full 33-byte ID (tag, then digest). A lookup reads one
//! 4 KiB page; opening a run reads only its footer and its tables.
//!
//! Layout of a run file (integers are little-endian):
//!
//! - Pages of [`PAGE`] bytes from offset 0. A page is: entry count u16, two
//!   zero bytes, CRC32C u32 of the entry bytes, then the entries
//!   ([`ENTRY`] bytes each, at most [`PER_PAGE`]), then zeros. A page holds
//!   the entries of one tag only: a new tag starts a new page.
//! - The tag table: for each tag in the run, in tag order: tag u8, first
//!   page u64, page count u64 ([`TAG_ROW`] bytes).
//! - The fence table: for each page, digest bytes 0..8 of its first entry
//!   ([`FENCE`] bytes, in digest byte order).
//! - The filter block (empty when the filter type is [`FILTER_NONE`]).
//! - The footer ([`FOOTER`] bytes): magic `SFRN`, run format version u16,
//!   filter type u8, bits per key u8, entry count u64, tombstone count u64,
//!   page count u64; then offset u64, length u64 and CRC32C u32 of the tag
//!   table, of the fence table and of the filter block; then the smallest
//!   and the largest pack number of the put entries (`u64::MAX` and 0 if
//!   there is no put entry); then a CRC32C u32 of the footer bytes before it.
//!
//! An entry is: the ID, pack u64, offset u64, stored length u32, raw length
//! u32, flags u8. A tombstone has flag bit [`TOMB`] set; its pack field is
//! the pack whose entry it hides, and its other fields are 0.
//!
//! Every count, offset and length that a run file gives is untrusted: it is
//! checked with checked arithmetic against the file length before any use or
//! allocation. Damage is [`Error::Corrupt`], never a panic, a skipped page or
//! a wrong answer.

use crate::pack_io::{PackFile, PackIo, PackRead};
use crate::{Error, Loc, ObjectId, Result, ID_LEN};
use std::io;
use std::sync::Arc;

/// Size of a page of entries.
pub(crate) const PAGE: usize = 4096;
/// Entry count u16, two zero bytes, CRC32C u32.
const PAGE_HEADER: usize = 8;
/// ID, pack u64, offset u64, stored u32, raw u32, flags u8.
pub(crate) const ENTRY: usize = ID_LEN + 8 + 8 + 4 + 4 + 1;
/// Most entries in one page.
pub(crate) const PER_PAGE: usize = (PAGE - PAGE_HEADER) / ENTRY;
/// Flag bit of a tombstone (a `Loc` uses bit 0 only).
const TOMB: u8 = 0x80;
/// Tag u8, first page u64, page count u64.
const TAG_ROW: usize = 1 + 8 + 8;
/// Bytes of one fence: digest bytes 0..8 of the first entry of a page.
const FENCE: usize = 8;
const MAGIC: &[u8; 4] = b"SFRN";
/// Format version of run files (packs and `.idx` files have their own).
pub(crate) const RUN_VERSION: u16 = 1;
/// Filter type: no filter (the filter block is empty).
pub(crate) const FILTER_NONE: u8 = 0;
/// Offset u64, length u64, CRC32C u32 of one block after the pages.
const SECTION: usize = 8 + 8 + 4;
/// Magic, version, filter type, bits per key, entry count, tombstone count,
/// page count, three sections, smallest and largest pack, CRC32C.
pub(crate) const FOOTER: usize = 4 + 2 + 1 + 1 + 8 + 8 + 8 + 3 * SECTION + 8 + 8 + 4;
/// `RunWriter` syncs after this many bytes, so the copy of unsynced bytes
/// that `RetryingIo` keeps stays small.
const SYNC_EVERY: u64 = 64 << 20;

const _: () = assert!(ENTRY == 58 && PER_PAGE == 70 && FOOTER == 112);

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

fn encode_entry(out: &mut [u8], id: &ObjectId, e: &Entry) {
    let (pack, offset, stored, raw, flags) = match *e {
        Entry::Put(l) => (l.pack, l.offset, l.stored, l.raw, l.flags),
        Entry::Tomb(pack) => (pack, 0, 0, 0, TOMB),
    };
    out[..ID_LEN].copy_from_slice(&id.0);
    let o = ID_LEN;
    out[o..o + 8].copy_from_slice(&pack.to_le_bytes());
    out[o + 8..o + 16].copy_from_slice(&offset.to_le_bytes());
    out[o + 16..o + 20].copy_from_slice(&stored.to_le_bytes());
    out[o + 20..o + 24].copy_from_slice(&raw.to_le_bytes());
    out[o + 24] = flags;
}

fn decode_entry(b: &[u8]) -> (ObjectId, Entry) {
    let id = ObjectId(b[..ID_LEN].try_into().unwrap());
    let o = ID_LEN;
    let pack = le_u64(b, o);
    let flags = b[o + 24];
    if flags & TOMB != 0 {
        return (id, Entry::Tomb(pack));
    }
    let loc = Loc { pack, offset: le_u64(b, o + 8), stored: le_u32(b, o + 16), raw: le_u32(b, o + 20), flags };
    (id, Entry::Put(loc))
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
/// stream to disk one page at a time. RAM: one page, the tag table and the
/// fence table (8 bytes per 70 entries).
///
/// The file is durable after `finish` (its name only after a `sync_dir`).
/// A writer dropped before `finish`, or one that returned an error, leaves
/// a partial file that the caller removes.
pub(crate) struct RunWriter {
    file: Box<dyn PackFile>,
    num: u64,
    /// The page being filled; its header is written when it is full.
    page: Vec<u8>,
    /// Entries in `page`.
    in_page: usize,
    /// Bytes written to the file so far.
    pos: u64,
    /// Bytes written since the last sync.
    unsynced: u64,
    /// Tag, first page, page count.
    tags: Vec<(u8, u64, u64)>,
    fences: Vec<u8>,
    last: Option<ObjectId>,
    entries: u64,
    tombstones: u64,
    min_pack: u64,
    max_pack: u64,
}

impl RunWriter {
    /// A writer of a new run file `num`. `expected` is an upper bound of the
    /// entry count (the filter of a later format version is sized from it);
    /// the footer holds the real count.
    pub fn create(io: &dyn PackIo, num: u64, expected: u64) -> Result<Self> {
        let file = io.create_run(num)?;
        let pages = expected.div_ceil(PER_PAGE as u64);
        let fences = Vec::with_capacity(usize::try_from(pages.saturating_mul(FENCE as u64)).unwrap_or(0).min(1 << 24));
        Ok(RunWriter {
            file,
            num,
            page: vec![0; PAGE],
            in_page: 0,
            pos: 0,
            unsynced: 0,
            tags: Vec::new(),
            fences,
            last: None,
            entries: 0,
            tombstones: 0,
            min_pack: u64::MAX,
            max_pack: 0,
        })
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
        if self.in_page == PER_PAGE || (self.in_page > 0 && self.last.is_some_and(|l| l.tag() != tag)) {
            self.write_page()?;
        }
        if self.in_page == 0 {
            let page = (self.fences.len() / FENCE) as u64;
            match self.tags.last_mut() {
                Some((t, _, count)) if *t == tag => *count += 1,
                _ => self.tags.push((tag, page, 1)),
            }
            self.fences.extend_from_slice(&id.0[1..1 + FENCE]);
        }
        let at = PAGE_HEADER + self.in_page * ENTRY;
        encode_entry(&mut self.page[at..at + ENTRY], id, e);
        self.in_page += 1;
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

    /// Write the page being filled (header, entries, zeros) and start a new one.
    fn write_page(&mut self) -> Result<()> {
        let end = PAGE_HEADER + self.in_page * ENTRY;
        let crc = crc32c::crc32c(&self.page[PAGE_HEADER..end]);
        self.page[..2].copy_from_slice(&(self.in_page as u16).to_le_bytes());
        self.page[2..4].fill(0);
        self.page[4..8].copy_from_slice(&crc.to_le_bytes());
        let page = std::mem::take(&mut self.page);
        let r = self.write(&page);
        self.page = page;
        self.page.fill(0);
        self.in_page = 0;
        r
    }

    /// Write the last page, the tables and the footer, and sync the file.
    pub fn finish(mut self) -> Result<RunInfo> {
        if self.in_page > 0 {
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
        let footer = Footer {
            filter_type: FILTER_NONE,
            bits_per_key: 0,
            entries: self.entries,
            tombstones: self.tombstones,
            pages,
            tags: Section { off: tags_at, len: tags.len() as u64, crc: crc32c::crc32c(&tags) },
            fences: Section { off: fences_at, len: self.fences.len() as u64, crc: crc32c::crc32c(&self.fences) },
            filter: Section { off: filter_at, len: 0, crc: crc32c::crc32c(&[]) },
            min_pack: self.min_pack,
            max_pack: self.max_pack,
        };
        let mut tail = tags;
        tail.extend_from_slice(&std::mem::take(&mut self.fences));
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

/// An open run: the footer values, the tag table and the fences in RAM; the
/// pages stay on disk.
#[allow(dead_code)] // `tombstones` and the pack range: for the merges (17-06).
pub(crate) struct Run {
    file: Arc<dyn PackRead>,
    num: u64,
    len: u64,
    entries: u64,
    tombstones: u64,
    min_pack: u64,
    max_pack: u64,
    tags: Vec<TagRange>,
    /// Digest bytes 0..8 of the first entry of each page (see `prefix`).
    fences: Vec<u64>,
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
    /// writer or from the file). Reads the footer, the tag table and the
    /// fence table; reads no page.
    pub fn open(file: Arc<dyn PackRead>, num: u64, len: u64) -> Result<Run> {
        let bad = |what: &str| Self::corrupt(num, what);
        if len < FOOTER as u64 {
            return Err(bad("shorter than a footer"));
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
            t => return Err(Self::corrupt(num, format_args!("unknown filter type {t}"))),
        }
        let max_entries = f.pages.checked_mul(PER_PAGE as u64).ok_or_else(|| bad("page count overflows"))?;
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

        Ok(Run {
            file,
            num,
            len,
            entries: f.entries,
            tombstones: f.tombstones,
            min_pack: f.min_pack,
            max_pack: f.max_pack,
            tags,
            fences,
        })
    }

    #[allow(dead_code)] // For the merges (17-06).
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

    #[allow(dead_code)] // For the merges (17-06).
    pub fn tombstones(&self) -> u64 {
        self.tombstones
    }

    /// The smallest and largest pack of the put entries; `None` if the run
    /// has only tombstones.
    #[allow(dead_code)] // For the merges (17-06).
    pub fn pack_range(&self) -> Option<(u64, u64)> {
        (self.min_pack <= self.max_pack).then_some((self.min_pack, self.max_pack))
    }

    /// Read page `page` (an index below the page count) and check it.
    /// Returns the page bytes and its entry count.
    fn read_page(&self, page: usize, buf: &mut [u8]) -> Result<usize> {
        let at = |what: &str| Self::corrupt(self.num, format_args!("page {page}: {what}"));
        Self::read(&*self.file, self.num, buf, (page * PAGE) as u64)?;
        let count = le_u16(buf, 0) as usize;
        if count == 0 || count > PER_PAGE {
            return Err(at("bad entry count"));
        }
        let end = PAGE_HEADER + count * ENTRY;
        if buf[2..4] != [0, 0] || buf[end..].iter().any(|&b| b != 0) {
            return Err(at("bytes that must be zero are not"));
        }
        if crc32c::crc32c(&buf[PAGE_HEADER..end]) != le_u32(buf, 4) {
            return Err(at("bad CRC"));
        }
        Ok(count)
    }

    /// The entry of `id`, if the run has one. One page read when the run has
    /// pages of the ID's tag and the ID is not below the first of them; a
    /// key whose 8-byte prefix equals a page fence can need the page before
    /// it too. A damaged page is `Corrupt`, never `None`.
    pub fn get(&self, id: &ObjectId) -> Result<Option<Entry>> {
        let Ok(t) = self.tags.binary_search_by_key(&id.tag(), |t| t.tag) else {
            return Ok(None);
        };
        let t = self.tags[t];
        let fences = &self.fences[t.first..t.first + t.count];
        let p = prefix(id);
        let after = fences.partition_point(|&f| f <= p);
        if after == 0 {
            return Ok(None);
        }
        let mut page = t.first + after - 1;
        let mut buf = vec![0u8; PAGE];
        loop {
            let count = self.read_page(page, &mut buf)?;
            let entries = &buf[PAGE_HEADER..PAGE_HEADER + count * ENTRY];
            let (mut lo, mut hi) = (0, count);
            while lo < hi {
                let mid = (lo + hi) / 2;
                let e = &entries[mid * ENTRY..(mid + 1) * ENTRY];
                match e[..ID_LEN].cmp(&id.0[..]) {
                    std::cmp::Ordering::Less => lo = mid + 1,
                    std::cmp::Ordering::Greater => hi = mid,
                    std::cmp::Ordering::Equal => return Ok(Some(decode_entry(e).1)),
                }
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

    /// Every entry in ID order, read one page at a time. A damaged page
    /// yields `Corrupt` and ends the iteration.
    pub fn iter(&self) -> RunIter<'_> {
        RunIter { run: self, page: 0, buf: vec![0u8; PAGE], count: 0, next: 0, seen: 0, last: None, done: false }
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
            self.count = self.run.read_page(self.page, &mut self.buf)?;
            self.next = 0;
            self.page += 1;
        }
        let at = PAGE_HEADER + self.next * ENTRY;
        let (id, e) = decode_entry(&self.buf[at..at + ENTRY]);
        if self.last.is_some_and(|l| id <= l) {
            return Err(Run::corrupt(self.run.num, format_args!("page {}: entries out of order", self.page - 1)));
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
        Entry::Put(Loc { pack: 1 + i % 5, offset: i * 100, stored: i as u32, raw: 2 * i as u32, flags: (i % 2) as u8 })
    }

    fn write_run(io: &dyn PackIo, num: u64, entries: &[(ObjectId, Entry)]) -> RunInfo {
        let mut w = RunWriter::create(io, num, entries.len() as u64).unwrap();
        for (id, e) in entries {
            w.push(id, e).unwrap();
        }
        w.finish().unwrap()
    }

    fn open_counting(io: &dyn PackIo, num: u64, len: u64) -> (Run, Arc<Counting>) {
        let c = Arc::new(Counting { inner: io.open_run(num).unwrap(), reads: Mutex::new(Vec::new()) });
        let run = Run::open(Arc::clone(&c) as Arc<dyn PackRead>, num, len).unwrap();
        (run, c)
    }

    // Tracer: a run written through create_run is found again through
    // open_run with one page read, on each I/O strategy.
    #[test]
    fn a_run_round_trips_through_each_io() {
        let dir = tempfile::tempdir().unwrap();
        let files = FileIo::new(dir.path().join("packs"));
        files.prepare(false).unwrap();
        let retrying = RetryingIo::with_backoff(Arc::new(MemIo::new()), Vec::new());
        let ios: [(&str, &dyn PackIo); 3] = [("mem", &MemIo::new()), ("files", &files), ("retrying", &retrying)];

        let entries: Vec<(ObjectId, Entry)> = (0..1000).map(|i| (key(0x1e, i), put(i))).collect();
        for (name, io) in ios {
            let info = write_run(io, 1, &entries);
            assert_eq!((info.entries, info.tombstones), (1000, 0), "{name}");
            let (run, c) = open_counting(io, 1, info.len);
            let pages = 1000u64.div_ceil(PER_PAGE as u64);
            let opened = c.take();
            assert!(!opened.is_empty(), "{name}");
            assert!(opened.iter().all(|&(o, _)| o >= pages * PAGE as u64), "{name}: open read a page: {opened:?}");
            assert_eq!(run.entries(), 1000, "{name}");
            assert_eq!(run.pack_range(), Some((1, 5)), "{name}");
            for (id, e) in &entries {
                assert_eq!(run.get(id).unwrap(), Some(*e), "{name}");
                assert_eq!(c.take().len(), 1, "{name}: one read for each hit");
            }
            for i in [0, 1, 500, 999, 5000] {
                let mut absent = key(0x1e, i);
                absent.0[8] |= 1; // between i and i + 1
                assert_eq!(run.get(&absent).unwrap(), None, "{name}");
                assert!(c.take().len() <= 1, "{name}: at most one read for a miss");
            }
            assert_eq!(run.get(&key(0x2f, 3)).unwrap(), None, "{name}: another tag");
            assert_eq!(c.take().len(), 0, "{name}: no read for a tag that is not in the run");
            assert_eq!(io.list_runs().unwrap(), vec![1], "{name}");
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
        let run = Run::open(io.open_run(1).unwrap(), 1, info.len).unwrap();
        (io, run, info)
    }

    /// Bytes of the file length a run of `pages` pages and `tags` tags has.
    fn run_len(pages: u64, tags: u64) -> u64 {
        pages * PAGE as u64 + tags * TAG_ROW as u64 + pages * FENCE as u64 + FOOTER as u64
    }

    #[test]
    fn runs_of_each_size_round_trip() {
        for n in [0u64, 1, 70, 71, 10_000] {
            let entries: Vec<(ObjectId, Entry)> = (0..n).map(|i| (key(0x1e, i), put(i))).collect();
            let (_io, run, info) = mem_run(&entries);
            let pages = n.div_ceil(PER_PAGE as u64);
            assert_eq!(info.len, run_len(pages, (n > 0) as u64), "n {n}");
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
        // 70 + 30 entries of tag 0x1e, then a page of 50 of tag 0xf0.
        assert_eq!(info.len, run_len(3, 2));
        let (run, c) = open_counting(&io, 1, info.len);
        c.take();
        check_run(&run, &entries);
        c.take();
        assert_eq!(run.get(&key(0xf0, 7)).unwrap(), Some(put(7)));
        assert_eq!(c.take(), vec![(2 * PAGE as u64, PAGE)], "the 0xf0 key is on page 2");
        assert_eq!(run.get(&key(0x1e, 99)).unwrap(), Some(put(99)));
        assert_eq!(c.take(), vec![(PAGE as u64, PAGE)], "the last 0x1e key is on page 1");
    }

    #[test]
    fn keys_with_the_prefix_of_a_page_fence_are_found() {
        // Keys 60..80 share one 8-byte prefix, across the boundary of pages
        // 0 and 1, so page 1 starts with that prefix.
        let mut entries: Vec<(ObjectId, Entry)> = (0..140)
            .map(|i| {
                let mut id = key(0x1e, if (60..80).contains(&i) { 60 } else { i });
                id.0[9] = i as u8; // digest byte 8: keeps them apart and sorted
                (id, put(i))
            })
            .collect();
        entries.sort_by_key(|(id, _)| *id);
        let (_io, run, _) = mem_run(&entries);
        check_run(&run, &entries);
        assert_eq!(run.fences[1], prefix(&entries[70].0));
        assert_eq!(run.fences[1], prefix(&entries[69].0));
        let mut absent = entries[65].0;
        absent.0[10] = 0xff;
        assert_eq!(run.get(&absent).unwrap(), None);

        // All keys with one prefix: every fence is equal.
        let entries: Vec<(ObjectId, Entry)> = (0..200u64)
            .map(|i| {
                let mut id = key(0x1e, 5);
                id.0[9..17].copy_from_slice(&i.to_be_bytes());
                (id, put(i))
            })
            .collect();
        let (_io, run, _) = mem_run(&entries);
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
        let mut w = RunWriter::create(&io, 1, 3).unwrap();
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
        Run::open(io.open_run(1).unwrap(), 1, bytes.len() as u64)
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

        let max_entries = u64::MAX / ENTRY as u64 + 1;
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("entry count u64::MAX", refooter(&good, |f| set_u64(f, 8, u64::MAX))),
            ("entry count past the pages", refooter(&good, |f| set_u64(f, 8, 3 * PER_PAGE as u64 + 1))),
            ("entry count that overflows bytes", refooter(&good, |f| set_u64(f, 8, max_entries))),
            ("tombstones above entries", refooter(&good, |f| set_u64(f, 16, 201))),
            ("page count 1 << 63", refooter(&good, |f| set_u64(f, 24, 1 << 63))),
            ("page count u64::MAX", refooter(&good, |f| set_u64(f, 24, u64::MAX))),
            ("page count one more", refooter(&good, |f| set_u64(f, 24, 4))),
            ("tag table length past the end", refooter(&good, |f| set_u64(f, 40, good.len() as u64))),
            ("tag table length u64::MAX", refooter(&good, |f| set_u64(f, 40, u64::MAX))),
            ("fence table length past the end", refooter(&good, |f| set_u64(f, 60, u64::MAX - 7))),
            ("fence table offset past the end", refooter(&good, |f| set_u64(f, 52, u64::MAX))),
            ("filter length past the end", refooter(&good, |f| set_u64(f, 80, 64))),
            ("unknown filter type", refooter(&good, |f| f[6] = 9)),
            ("wrong magic", refooter(&good, |f| f[0] = b'X')),
            ("unknown version", refooter(&good, |f| f[4..6].copy_from_slice(&2u16.to_le_bytes()))),
            ("bad pack range", refooter(&good, |f| set_u64(f, 92, u64::MAX))),
            ("bad footer CRC", {
                let mut b = good.clone();
                let at = b.len() - FOOTER + 9;
                b[at] ^= 1;
                b
            }),
            ("bad tag table CRC", {
                let mut b = good.clone();
                b[3 * PAGE] ^= 1;
                b
            }),
            ("bad fence table CRC", {
                let mut b = good.clone();
                b[3 * PAGE + TAG_ROW] ^= 1;
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
        assert!(is_corrupt(&Run::open(io.open_run(1).unwrap(), 1, good.len() as u64 + PAGE as u64)));
    }

    #[test]
    fn a_damaged_page_is_corrupt_for_get_and_iter() {
        let entries: Vec<(ObjectId, Entry)> = (0..200).map(|i| (key(0x1e, i), put(i))).collect();
        let (io, run, _) = mem_run(&entries);
        // An entry byte of page 1 (entries 70..140).
        io.flip(1, Kind::Run, PAGE + PAGE_HEADER + 5 * ENTRY + 20);
        assert!(is_corrupt(&run.get(&key(0x1e, 100))), "a key on the damaged page");
        assert!(is_corrupt(&run.get(&key(0x1e, 75))), "the damaged key itself");
        assert_eq!(run.get(&key(0x1e, 10)).unwrap(), Some(put(10)), "other pages still read");
        assert_eq!(run.get(&key(0x1e, 150)).unwrap(), Some(put(150)));
        let mut it = run.iter();
        for i in 0..PER_PAGE as u64 {
            assert_eq!(it.next().unwrap().unwrap(), (key(0x1e, i), put(i)));
        }
        assert!(is_corrupt(&it.next().unwrap()), "the damaged page");
        assert!(it.next().is_none(), "iter stops after damage");

        // Damage in the page header and in the zero padding.
        for at in [2 * PAGE, 2 * PAGE + 2, 2 * PAGE + 5, 3 * PAGE - 1] {
            let (io, run, _) = mem_run(&entries);
            io.flip(1, Kind::Run, at);
            assert!(is_corrupt(&run.get(&key(0x1e, 150))), "byte {at}");
            assert!(run.iter().any(|r| is_corrupt(&r)), "byte {at}");
        }
    }
}
