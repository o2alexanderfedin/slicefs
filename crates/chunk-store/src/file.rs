//! File content on the chunk store: a streaming writer that chunks with
//! FastCDC, a manifest tree of nodes, and a reader for byte ranges.
//!
//! Node format: magic `SFM1`, level `u8`, entry count `u32 LE`, then entries
//! of `ObjectId` (`ID_LEN` = 33 bytes: tag + digest) + byte length `u64 LE`. Level 0 entries are
//! chunks; a level `k > 0` entry is a level `k - 1` node and the bytes it
//! covers.
//!
//! The nodes are cut by content, as the chunks are: a node ends after an
//! entry whose ID matches a bit pattern (1 in `NODE_CUT_ONE_IN`), with at
//! least `NODE_MIN` and at most `LEAF_ENTRIES` entries. So a chunk added or
//! removed near the start of a file changes only the nodes up to the next
//! cut (and their parents), not every node after it. The tree depends only
//! on the chunk sequence, so the key of a file does not depend on the sizes
//! of the writes that made it.
//!
//! `splice` makes the key of an edited content without a full re-chunk. It
//! copies the old chunk entries up to the largest old cut strictly below the
//! first changed byte (a cut depends on the byte at it), cuts again from
//! there, and stops at a new cut at or past the change that is also a usable
//! old cut (not the old end if the size or the tail changes). Where the
//! writer and an old complete node start together, it copies the whole node
//! as one entry. Known zeros are pushed as the zero chunk without a read or
//! a hash (FastCDC cuts zeros at `CHUNK_MAX`). The key is the key of a full
//! write of the same bytes.

use crate::{chunk_ranges, hashers, ChunkStore, Error, ObjectId, Result, CHUNK_MAX, DIGEST_LEN, ID_LEN};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

/// Most entries in a manifest node (a forced cut).
pub const LEAF_ENTRIES: usize = 4096;

/// Fewest entries in a manifest node before a content cut (the last node of
/// a level can have fewer).
pub const NODE_MIN: usize = 64;

/// A node ends after about one entry in this many (a power of two).
pub const NODE_CUT_ONE_IN: u32 = 512;

/// True if a node may end after an entry with this ID. The digest bytes of
/// an ID are uniform (the tag is not: it is the same for most IDs).
fn is_cut(id: &ObjectId) -> bool {
    let w = u32::from_le_bytes(id.digest()[DIGEST_LEN - 4..].try_into().unwrap());
    w & (NODE_CUT_ONE_IN - 1) == 0
}

const NODE_MAGIC: &[u8; 4] = b"SFM1";
const ENTRY: usize = ID_LEN + 8;

#[derive(Clone, Copy, Debug)]
struct Entry {
    id: ObjectId,
    len: u64,
}

fn encode_node(level: u8, entries: &[Entry]) -> Vec<u8> {
    let mut b = Vec::with_capacity(9 + entries.len() * ENTRY);
    b.extend_from_slice(NODE_MAGIC);
    b.push(level);
    b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        b.extend_from_slice(&e.id.0);
        b.extend_from_slice(&e.len.to_le_bytes());
    }
    b
}

struct Node {
    level: u8,
    entries: Vec<Entry>,
    /// Start offset of each entry within the node.
    starts: Vec<u64>,
    len: u64,
}

fn decode_node(b: &[u8]) -> Option<Node> {
    if b.len() < 9 || &b[..4] != NODE_MAGIC {
        return None;
    }
    let level = b[4];
    let n = u32::from_le_bytes(b[5..9].try_into().unwrap()) as usize;
    if b.len() != 9 + n * ENTRY || n > LEAF_ENTRIES {
        return None;
    }
    let mut entries = Vec::with_capacity(n);
    let mut starts = Vec::with_capacity(n);
    let mut pos = 0u64;
    for e in b[9..].chunks_exact(ENTRY) {
        let id = ObjectId(e[..ID_LEN].try_into().unwrap());
        let len = u64::from_le_bytes(e[ID_LEN..].try_into().unwrap());
        starts.push(pos);
        pos = pos.checked_add(len)?;
        entries.push(Entry { id, len });
    }
    Some(Node { level, entries, starts, len: pos })
}

fn load_node(store: &ChunkStore, id: &ObjectId) -> Result<Node> {
    crate::probe::inc(&crate::probe::NODE_LOADS, 1);
    let bytes = store.read(id)?;
    decode_node(&bytes).ok_or_else(|| Error::Corrupt(format!("manifest node {id:?}")))
}

// ── Writer ────────────────────────────────────────────────────────────────────

/// Streams file bytes into the store. `clone()` + `finish()` gives the key of
/// the content written so far without ending the stream.
#[derive(Clone, Default)]
pub struct FileWriter {
    /// Bytes not yet cut into chunks (less than 2 * CHUNK_MAX after a write).
    buf: Vec<u8>,
    /// `levels[k]`: entries of the level-k node being filled.
    levels: Vec<Vec<Entry>>,
    len: u64,
}

impl FileWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes written so far.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Append `data`.
    pub fn write(&mut self, store: &ChunkStore, data: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(data);
        self.len += data.len() as u64;
        if self.buf.len() >= 2 * CHUNK_MAX as usize {
            self.cut(store, false)?;
        }
        Ok(())
    }

    /// Append `n` zero bytes.
    pub fn write_zeros(&mut self, store: &ChunkStore, mut n: u64) -> Result<()> {
        let zeros = vec![0u8; 1 << 16];
        while n > 0 {
            let k = n.min(zeros.len() as u64) as usize;
            self.write(store, &zeros[..k])?;
            n -= k as u64;
        }
        Ok(())
    }

    /// Store the chunks of `buf`. Without `all`, the last chunk stays in
    /// `buf`: more bytes can move its end.
    fn cut(&mut self, store: &ChunkStore, all: bool) -> Result<()> {
        let ranges = chunk_ranges(&self.buf);
        let keep = if all { ranges.len() } else { ranges.len().saturating_sub(1) };
        let mut used = 0;
        for r in &ranges[..keep] {
            let id = store.put(&self.buf[r.clone()])?;
            self.push(store, 0, Entry { id, len: r.len() as u64 })?;
            used = r.end;
        }
        self.buf.drain(..used);
        Ok(())
    }

    fn push(&mut self, store: &ChunkStore, level: usize, e: Entry) -> Result<()> {
        if self.levels.len() <= level {
            self.levels.resize_with(level + 1, Vec::new);
        }
        self.levels[level].push(e);
        let n = self.levels[level].len();
        if n == LEAF_ENTRIES || (n >= NODE_MIN && is_cut(&e.id)) {
            let entries = std::mem::take(&mut self.levels[level]);
            let node = self.put_node(store, level, &entries)?;
            self.push(store, level + 1, node)?;
        }
        Ok(())
    }

    /// Push an entry that is already in the store: a chunk at level 0, or a
    /// whole node at a higher level (for `splice`; `buf` must be empty).
    fn push_entry(&mut self, store: &ChunkStore, level: usize, e: Entry) -> Result<()> {
        debug_assert!(self.buf.is_empty());
        self.len += e.len;
        self.push(store, level, e)
    }

    fn put_node(&self, store: &ChunkStore, level: usize, entries: &[Entry]) -> Result<Entry> {
        let id = store.put(&encode_node(level as u8, entries))?;
        Ok(Entry { id, len: entries.iter().map(|e| e.len).sum() })
    }

    /// End the stream and return the key of the content (the root node).
    pub fn finish(mut self, store: &ChunkStore) -> Result<ObjectId> {
        self.cut(store, true)?;
        if self.levels.is_empty() {
            self.levels.push(Vec::new());
        }
        let mut k = 0;
        loop {
            let higher = self.levels[k + 1..].iter().any(|l| !l.is_empty());
            if !higher {
                // The top: one node of the level below is the root itself.
                if k > 0 && self.levels[k].len() == 1 {
                    return Ok(self.levels[k][0].id);
                }
                let entries = std::mem::take(&mut self.levels[k]);
                return Ok(self.put_node(store, k, &entries)?.id);
            }
            if !self.levels[k].is_empty() {
                let entries = std::mem::take(&mut self.levels[k]);
                let node = self.put_node(store, k, &entries)?;
                self.levels[k + 1].push(node);
            }
            k += 1;
        }
    }
}

// ── Reader ────────────────────────────────────────────────────────────────────

/// Reads byte ranges of one file content. Keeps a few nodes and the last
/// chunk in memory, so sequential reads do not load them again.
pub struct FileReader {
    root: Arc<Node>,
    key: ObjectId,
    nodes: HashMap<ObjectId, Arc<Node>>,
    chunk: Option<(ObjectId, Vec<u8>)>,
}

const NODE_CACHE: usize = 16;

/// FastCDC cuts a run of zeros into chunks of `CHUNK_MAX` zero bytes, which
/// all have one ID for each hash. A reader serves them from one shared zero
/// buffer: no lookup, no I/O, no decompression (a flyweight for sparse files).
fn zero_chunk() -> &'static (Vec<ObjectId>, Vec<u8>) {
    static ZERO: OnceLock<(Vec<ObjectId>, Vec<u8>)> = OnceLock::new();
    ZERO.get_or_init(|| {
        let z = vec![0u8; CHUNK_MAX as usize];
        (hashers().iter().map(|h| h.id(&z)).collect(), z)
    })
}

impl FileReader {
    /// Open the content `key`.
    pub fn open(store: &ChunkStore, key: ObjectId) -> Result<Self> {
        let root = Arc::new(load_node(store, &key)?);
        Ok(FileReader { root, key, nodes: HashMap::new(), chunk: None })
    }

    /// The key this reader reads.
    pub fn key(&self) -> ObjectId {
        self.key
    }

    /// Content length.
    pub fn len(&self) -> u64 {
        self.root.len
    }

    pub fn is_empty(&self) -> bool {
        self.root.len == 0
    }

    fn node(&mut self, store: &ChunkStore, id: &ObjectId) -> Result<Arc<Node>> {
        if let Some(n) = self.nodes.get(id) {
            return Ok(Arc::clone(n));
        }
        if self.nodes.len() >= NODE_CACHE {
            self.nodes.clear();
        }
        let n = Arc::new(load_node(store, id)?);
        self.nodes.insert(*id, Arc::clone(&n));
        Ok(n)
    }

    /// Up to `size` bytes from `offset` (fewer at the end of the content).
    pub fn read(&mut self, store: &ChunkStore, offset: u64, size: usize) -> Result<Vec<u8>> {
        let end = self.len().min(offset.saturating_add(size as u64));
        let mut out = Vec::with_capacity(end.saturating_sub(offset) as usize);
        let mut pos = offset;
        while pos < end {
            // Find the chunk that holds `pos`.
            let mut node = Arc::clone(&self.root);
            let mut base = 0u64;
            let (id, start, len) = loop {
                let i = node.starts.partition_point(|&s| s <= pos - base) - 1;
                let e = node.entries[i];
                if node.level == 0 {
                    break (e.id, base + node.starts[i], e.len);
                }
                base += node.starts[i];
                let child = self.node(store, &e.id)?;
                // The tree may come from a peer: a child must hold exactly
                // the bytes its parent's entry says, one level lower. Else a
                // read could slice out of range or stop making progress.
                if child.len != e.len || child.level.checked_add(1) != Some(node.level) {
                    return Err(Error::Corrupt(format!(
                        "manifest node {:?}: level {} with {} bytes under a level-{} entry of {} bytes",
                        e.id, child.level, child.len, node.level, e.len
                    )));
                }
                node = child;
            };
            let (zids, zeros) = zero_chunk();
            if len == zeros.len() as u64 && zids.contains(&id) {
                let from = (pos - start) as usize;
                let to = ((end - start).min(len)) as usize;
                out.extend_from_slice(&zeros[from..to]);
                pos = start + to as u64;
                continue;
            }
            if self.chunk.as_ref().map(|(c, _)| *c) != Some(id) {
                let bytes = store.read(&id)?;
                if bytes.len() as u64 != len {
                    return Err(Error::Corrupt(format!("chunk {id:?}: {} bytes, manifest says {len}", bytes.len())));
                }
                self.chunk = Some((id, bytes));
            }
            let bytes = &self.chunk.as_ref().unwrap().1;
            let from = (pos - start) as usize;
            let to = ((end - start).min(len)) as usize;
            out.extend_from_slice(&bytes[from..to]);
            pos = start + to as u64;
        }
        Ok(out)
    }
}

/// Every object of the content `key`: its nodes and its chunks (for GC).
pub fn file_objects(store: &ChunkStore, key: ObjectId) -> Result<Vec<ObjectId>> {
    let mut out = vec![key];
    let mut todo = vec![key];
    while let Some(id) = todo.pop() {
        let node = load_node(store, &id)?;
        for e in &node.entries {
            out.push(e.id);
            if node.level > 0 {
                todo.push(e.id);
            }
        }
    }
    Ok(out)
}

// ── Splice ────────────────────────────────────────────────────────────────────

/// The new content of a `splice`.
pub trait SpliceSource {
    /// Exactly `len` bytes of the new content at `offset`.
    fn read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>>;
}

/// What a `splice` did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpliceStats {
    /// Bytes of the chunks that the re-chunk cut with FastCDC.
    pub rechunked: u64,
    /// Old chunk entries copied one by one.
    pub reused_entries: u64,
    /// Whole old nodes copied as one entry.
    pub reused_nodes: u64,
    /// Zero chunks pushed without a read or a hash.
    pub zero_runs: u64,
}

/// Walks the level-0 entries of a tree in order. Holds one node per level:
/// `path[0]` is the root, the last frame is a leaf. Empty at the end.
#[derive(Clone)]
struct Cursor {
    path: Vec<(Arc<Node>, usize)>,
    /// Start offset of the current entry (the content length at the end).
    off: u64,
}

impl Cursor {
    fn new(store: &ChunkStore, root: Node) -> Result<Self> {
        let mut c = Cursor { path: vec![(Arc::new(root), 0)], off: 0 };
        if c.path[0].0.entries.is_empty() {
            c.path.clear();
        } else {
            c.descend(store)?;
        }
        Ok(c)
    }

    fn at_end(&self) -> bool {
        self.path.is_empty()
    }

    /// The current level-0 entry (not at the end).
    fn entry(&self) -> Entry {
        let (n, i) = self.path.last().unwrap();
        n.entries[*i]
    }

    /// Load the first entries from the last frame down to level 0. The tree
    /// may come from a peer: the same checks as `FileReader::read`.
    fn descend(&mut self, store: &ChunkStore) -> Result<()> {
        loop {
            let (node, i) = self.path.last().unwrap();
            let e = node.entries[*i];
            if e.len == 0 {
                return Err(Error::Corrupt(format!("manifest entry {:?} of 0 bytes", e.id)));
            }
            if node.level == 0 {
                return Ok(());
            }
            let child = load_node(store, &e.id)?;
            if child.len != e.len || child.level.checked_add(1) != Some(node.level) {
                return Err(Error::Corrupt(format!(
                    "manifest node {:?}: level {} with {} bytes under a level-{} entry of {} bytes",
                    e.id, child.level, child.len, node.level, e.len
                )));
            }
            self.path.push((Arc::new(child), 0));
        }
    }

    /// Move past the current entry of frame `j` (with all it covers).
    fn step(&mut self, store: &ChunkStore, j: usize) -> Result<()> {
        let (n, i) = &self.path[j];
        self.off += n.entries[*i].len;
        self.path.truncate(j + 1);
        loop {
            let Some((n, i)) = self.path.last_mut() else { return Ok(()) };
            *i += 1;
            if *i < n.entries.len() {
                break;
            }
            self.path.pop();
        }
        self.descend(store)
    }

    /// Move to the next level-0 entry.
    fn next(&mut self, store: &ChunkStore) -> Result<()> {
        self.step(store, self.path.len() - 1)
    }

    /// The highest old node that starts at the cursor, is complete, and ends
    /// before `limit`, while the writer has no pending entries at its level
    /// or below: `(frame, entry of the node in its parent)`. A complete node
    /// ends where the node cut rule cuts (a partial node of the right spine
    /// does not), so pushing its entries one by one would make this node.
    fn reusable_node(&self, w: &FileWriter, limit: u64) -> Option<(usize, Entry)> {
        let d = self.path.len().checked_sub(1)?;
        let mut first = d + 1;
        while first > 0 && self.path[first - 1].1 == 0 {
            first -= 1;
        }
        for f in first.max(1)..=d {
            let node = &self.path[f].0;
            let (parent, i) = &self.path[f - 1];
            let pe = parent.entries[*i];
            let k = node.level as usize;
            let n = node.entries.len();
            let complete = n == LEAF_ENTRIES || (n >= NODE_MIN && is_cut(&node.entries[n - 1].id));
            if complete && self.off + pe.len < limit && w.levels.iter().take(k + 1).all(|l| l.is_empty()) {
                return Some((f, pe));
            }
        }
        None
    }

    /// Move to the entry that holds offset `x` (forward only; at the end if
    /// `x` is past the content).
    fn advance_to(&mut self, store: &ChunkStore, x: u64) -> Result<()> {
        while !self.at_end() && self.off + self.entry().len <= x {
            self.next(store)?;
        }
        Ok(())
    }
}

/// After a new cut at `e`: take in every region that starts at or before
/// `e`, then true if the re-chunk can stop at `e` (past the regions, at a
/// usable old cut).
fn resync_at(
    store: &ChunkStore,
    cur: &mut Cursor,
    regions: &[Range<u64>],
    ri: &mut usize,
    end: &mut u64,
    e: u64,
    usable: &dyn Fn(u64) -> bool,
) -> Result<bool> {
    cur.advance_to(store, e)?;
    while *ri < regions.len() && regions[*ri].start <= e {
        *end = (*end).max(regions[*ri].end);
        *ri += 1;
    }
    Ok(e >= *end && usable(e) && cur.off == e)
}

/// True if the new bytes `[s, s + CHUNK_MAX)` are known zeros: outside every
/// changed range, and below `keep` covered by old zero chunks (the cursor
/// holds `s`). An old zero chunk may start before `s`.
fn known_zero(store: &ChunkStore, cur: &Cursor, ext: &[Range<u64>], ci: &mut usize, keep: u64, s: u64) -> Result<bool> {
    let e = s + CHUNK_MAX as u64;
    while *ci < ext.len() && ext[*ci].end <= s {
        *ci += 1;
    }
    if ext.get(*ci).is_some_and(|c| c.start < e) {
        return Ok(false);
    }
    let old_end = e.min(keep);
    if s < old_end {
        let (zids, _) = zero_chunk();
        let mut c = cur.clone();
        while c.off < old_end {
            if c.at_end() {
                return Ok(false);
            }
            let x = c.entry();
            if x.len != CHUNK_MAX as u64 || !zids.contains(&x.id) {
                return Ok(false);
            }
            c.next(store)?;
        }
    }
    Ok(true)
}

fn invalid(what: String) -> Error {
    Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, what))
}

/// Copy old entries from the cursor while they end before `limit`: whole
/// old nodes where it can, else one chunk entry at a time.
fn copy_old(store: &ChunkStore, cur: &mut Cursor, w: &mut FileWriter, limit: u64, st: &mut SpliceStats) -> Result<()> {
    while !cur.at_end() {
        if let Some((f, pe)) = cur.reusable_node(w, limit) {
            let level = cur.path[f].0.level as usize + 1;
            w.push_entry(store, level, pe)?;
            st.reused_nodes += 1;
            cur.step(store, f - 1)?;
            continue;
        }
        let e = cur.entry();
        if cur.off + e.len >= limit {
            break;
        }
        w.push_entry(store, 0, e)?;
        st.reused_entries += 1;
        cur.next(store)?;
    }
    Ok(())
}

/// The key of new content made from the content `base` (`None`: empty) by an
/// edit, without a full re-chunk. Byte `x < new_len` of the new content is
/// the byte of `src` if `x` is in a `changed` range, else the base byte if
/// `x < keep`, else zero. `changed` is sorted, without overlaps, inside
/// `[0, new_len)`; `keep` is at most the base length and `new_len`.
///
/// Only the chunks near the changed bytes are cut again: from the largest
/// old cut strictly below the first changed byte (a cut at a byte depends on
/// that byte) to a new cut at or past the change that is also a usable old
/// cut. The old cut at the end of the base is usable only if the size and
/// the tail stay. The key is the key of a `FileWriter` that writes all the
/// new bytes. Memory is a few chunks plus one node per tree level.
pub fn splice(
    store: &ChunkStore,
    base: Option<ObjectId>,
    keep: u64,
    changed: &[Range<u64>],
    new_len: u64,
    src: &mut dyn SpliceSource,
) -> Result<(ObjectId, SpliceStats)> {
    let mut cur = match base {
        Some(id) => Cursor::new(store, load_node(store, &id)?)?,
        None => Cursor { path: Vec::new(), off: 0 },
    };
    let base_len = match &cur.path.first() {
        Some((root, _)) => root.len,
        None => 0,
    };
    if keep > base_len || keep > new_len {
        return Err(invalid(format!("splice: keep {keep} past base {base_len} or new length {new_len}")));
    }
    let mut ext: Vec<Range<u64>> = Vec::new();
    for c in changed {
        if c.start > c.end || c.end > new_len || ext.last().is_some_and(|p| c.start < p.end) {
            return Err(invalid(format!("splice: bad changed range {c:?} (new length {new_len})")));
        }
        if !c.is_empty() {
            ext.push(c.clone());
        }
    }
    // The tail [keep, new_len) changes when the size or the old tail changes:
    // the old last chunk ended at the old end, not at a content cut.
    let tail = new_len != base_len || keep < base_len;
    let mut regions: Vec<Range<u64>> = Vec::new();
    for c in &ext {
        if tail && c.start >= keep {
            break;
        }
        regions.push(c.clone());
    }
    if tail {
        match regions.last_mut() {
            Some(l) if l.end >= keep => l.end = new_len,
            _ => regions.push(keep..new_len),
        }
    }
    let usable = |e: u64| e < keep || (e == base_len && !tail);

    let mut w = FileWriter::new();
    let mut st = SpliceStats::default();
    let mut ri = 0;
    let max = CHUNK_MAX as usize;
    let zeros = &zero_chunk().1;
    let mut zero_id = None;
    // First changed range that ends after the current chunk start.
    let mut ci = 0;
    loop {
        let limit = regions.get(ri).map_or(u64::MAX, |r| r.start);
        copy_old(store, &mut cur, &mut w, limit, &mut st)?;
        if ri == regions.len() {
            break;
        }
        // Re-chunk from the old cut before the region.
        let mut end = regions[ri].end;
        ri += 1;
        let mut start = cur.off;
        let mut buf: Vec<u8> = Vec::new();
        let mut resynced = false;
        while start < new_len {
            // Zero shortcut: FastCDC cuts CHUNK_MAX zeros into one chunk, so
            // known zeros need no read and no hash.
            if start + max as u64 <= new_len && known_zero(store, &cur, &ext, &mut ci, keep, start)? {
                let id = match zero_id {
                    Some(id) => id,
                    // A full write puts the zero chunk: put it once.
                    None => *zero_id.insert(store.put(zeros)?),
                };
                w.push_entry(store, 0, Entry { id, len: max as u64 })?;
                st.zero_runs += 1;
                buf.drain(..buf.len().min(max));
                start += max as u64;
                if resync_at(store, &mut cur, &regions, &mut ri, &mut end, start, &usable)? {
                    resynced = true;
                    break;
                }
                continue;
            }
            let at = start + buf.len() as u64;
            if at < new_len {
                let n = ((2 * max - buf.len()) as u64).min(new_len - at) as usize;
                let t = std::time::Instant::now();
                let piece = src.read(at, n)?;
                crate::probe::add(&crate::probe::SRC_NS, t);
                crate::probe::inc(&crate::probe::SRC_BYTES, n as u64);
                if piece.len() != n {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!("splice source: {} bytes at {at}, asked {n}", piece.len()),
                    )));
                }
                buf.extend_from_slice(&piece);
            }
            let all = start + buf.len() as u64 == new_len;
            let t = std::time::Instant::now();
            let ranges = chunk_ranges(&buf);
            crate::probe::add(&crate::probe::CDC_NS, t);
            let n = if all { ranges.len() } else { ranges.len().saturating_sub(1) };
            let mut used = 0;
            for r in &ranges[..n] {
                let id = store.put(&buf[r.clone()])?;
                w.push_entry(store, 0, Entry { id, len: r.len() as u64 })?;
                st.rechunked += r.len() as u64;
                used = r.end;
                if resync_at(store, &mut cur, &regions, &mut ri, &mut end, start + r.end as u64, &usable)? {
                    resynced = true;
                    break;
                }
            }
            if resynced {
                break;
            }
            buf.drain(..used);
            start += used as u64;
        }
        if !resynced {
            break;
        }
    }
    Ok((w.finish(store)?, st))
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)] // one changed range is a list of one
mod tests {
    use super::*;
    use crate::mem::MemIo;
    use crate::{Blake3_256, ObjectHasher, Options};

    /// A manifest of `entries` (fake chunks: only the nodes are stored).
    fn build(s: &ChunkStore, entries: &[Entry]) -> ObjectId {
        let mut w = FileWriter::default();
        for e in entries {
            w.push(s, 0, *e).unwrap();
        }
        w.finish(s).unwrap()
    }

    // A crafted tree (for example from a peer) must give an error, never a
    // panic: a child whose length differs from the parent's entry, an empty
    // child, or a child at the wrong level.
    #[test]
    fn a_malformed_tree_is_an_error_not_a_panic() {
        let s = ChunkStore::open_with(std::sync::Arc::new(MemIo::new()), Options::default()).unwrap();
        let chunk = s.put(&[7u8; 50]).unwrap();
        let e = |id, len| Entry { id, len };
        let leaf = s.put(&encode_node(0, &[e(chunk, 50)])).unwrap();
        let empty = s.put(&encode_node(0, &[])).unwrap();
        let wrong_level = s.put(&encode_node(2, &[e(chunk, 50)])).unwrap();
        for (what, child, claim) in [("longer claim", leaf, 100), ("empty child", empty, 10), ("wrong level", wrong_level, 50)] {
            let root = s.put(&encode_node(1, &[e(child, claim)])).unwrap();
            let mut r = FileReader::open(&s, root).unwrap();
            let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| r.read(&s, 0, claim as usize)));
            assert!(matches!(got, Ok(Err(Error::Corrupt(_)))), "{what}: {got:?}");
        }
        // A good tree still reads.
        let root = s.put(&encode_node(1, &[e(leaf, 50)])).unwrap();
        assert_eq!(FileReader::open(&s, root).unwrap().read(&s, 10, 100).unwrap(), vec![7u8; 40]);
    }

    fn fake(n: u32) -> Vec<Entry> {
        (0..n).map(|i| Entry { id: Blake3_256.id(&i.to_le_bytes()), len: 8192 }).collect()
    }

    /// A chunk added or removed at the start changes only a few nodes, not
    /// every node after it (about 200,000 chunks: a 1.6 GB file).
    #[test]
    fn a_shift_at_the_start_changes_few_nodes() {
        let s = ChunkStore::open_with(Arc::new(MemIo::new()), Options::default()).unwrap();
        let base = fake(200_000);
        build(&s, &base);
        let nodes = s.stats().objects;
        assert!(nodes > 300, "{nodes} nodes");

        let mut ins = vec![Entry { id: Blake3_256.id(b"new"), len: 100 }];
        ins.extend_from_slice(&base);
        let before = s.stats().objects;
        build(&s, &ins);
        let added = s.stats().objects - before;
        eprintln!("insert: {added} new nodes of {nodes}");
        assert!(added <= 6, "insert: {added} new nodes of {nodes}");

        let before = s.stats().objects;
        build(&s, &base[1..]);
        let added = s.stats().objects - before;
        eprintln!("delete: {added} new nodes of {nodes}");
        assert!(added <= 6, "delete: {added} new nodes of {nodes}");
    }

    #[test]
    fn nodes_stay_within_the_size_limits() {
        let s = ChunkStore::open_with(Arc::new(MemIo::new()), Options::default()).unwrap();
        let root = build(&s, &fake(100_000));
        let mut todo = vec![root];
        let mut sizes = Vec::new();
        while let Some(id) = todo.pop() {
            let n = load_node(&s, &id).unwrap();
            sizes.push(n.entries.len());
            if n.level > 0 {
                todo.extend(n.entries.iter().map(|e| e.id));
            }
        }
        // Every node but the last of its level has NODE_MIN..=LEAF_ENTRIES.
        let small = sizes.iter().filter(|&&n| n < NODE_MIN).count();
        assert!(small <= 3, "{small} small nodes: {sizes:?}");
        assert!(sizes.iter().all(|&n| n <= LEAF_ENTRIES));
        let leaves = sizes.len() - 1;
        let mean = 100_000 / leaves.max(1);
        assert!((300..=1200).contains(&mean), "mean {mean} entries per leaf");
    }

    // ── splice ──

    /// The new content of a splice, from a slice.
    struct VecSource<'a>(&'a [u8]);

    impl SpliceSource for VecSource<'_> {
        fn read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
            Ok(self.0[offset as usize..offset as usize + len].to_vec())
        }
    }

    /// xorshift64: test data and edits without a dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }

        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next() as u8).collect()
        }
    }

    /// No compression: the IDs hash the raw bytes, so keys stay the same.
    fn fast_store() -> ChunkStore {
        ChunkStore::open_with(Arc::new(MemIo::new()), Options { zstd_level: 0, ..Options::default() }).unwrap()
    }

    fn full_write(s: &ChunkStore, data: &[u8]) -> ObjectId {
        let mut w = FileWriter::new();
        for piece in data.chunks(1 << 20) {
            w.write(s, piece).unwrap();
        }
        w.finish(s).unwrap()
    }

    /// An edit of a base: keep the first `keep` base bytes, zeros up to
    /// `new_len`, then new bytes in the `changed` ranges.
    struct Edit {
        keep: u64,
        changed: Vec<Range<u64>>,
        new_len: u64,
    }

    fn new_content(base: &[u8], ed: &Edit, rng: &mut Rng) -> Vec<u8> {
        let mut d = base[..ed.keep as usize].to_vec();
        d.resize(ed.new_len as usize, 0);
        for c in &ed.changed {
            let fresh = rng.bytes((c.end - c.start) as usize);
            d[c.start as usize..c.end as usize].copy_from_slice(&fresh);
        }
        d
    }

    /// Splice `ed` into `base` and check the key against a full write of
    /// the new bytes, and the bytes read back.
    fn check_splice(s: &ChunkStore, base: Option<(ObjectId, &[u8])>, ed: &Edit, rng: &mut Rng, what: &str) -> SpliceStats {
        let old: &[u8] = base.map_or(&[], |b| b.1);
        let data = new_content(old, ed, rng);
        let (key, st) = splice(s, base.map(|b| b.0), ed.keep, &ed.changed, ed.new_len, &mut VecSource(&data)).unwrap();
        assert_eq!(key, full_write(s, &data), "{what}: keep {} changed {:?} new_len {}", ed.keep, ed.changed, ed.new_len);
        let got = FileReader::open(s, key).unwrap().read(s, 0, data.len()).unwrap();
        assert!(got == data, "{what}: read back");
        st
    }

    /// Sorted ranges without overlaps inside [0, len).
    fn extents(rng: &mut Rng, len: u64, count: usize, max_len: u64) -> Vec<Range<u64>> {
        let mut starts: Vec<u64> = (0..count).map(|_| rng.below(len)).collect();
        starts.sort_unstable();
        starts.dedup();
        let mut out: Vec<Range<u64>> = Vec::new();
        for (i, &a) in starts.iter().enumerate() {
            let next = starts.get(i + 1).copied().unwrap_or(len);
            let b = (a + 1 + rng.below(max_len)).min(next);
            out.push(a..b);
        }
        out
    }

    /// Old chunk cuts of `data` (the ends of its chunks).
    fn cuts(data: &[u8]) -> Vec<u64> {
        chunk_ranges(data).iter().map(|r| r.end as u64).collect()
    }

    #[test]
    fn a_splice_gives_the_same_key_as_a_full_write() {
        let s = fast_store();
        let mut rng = Rng(0x5eed_1234_abcd_0001);
        let base = rng.bytes(1 << 20);
        let n = base.len() as u64;
        let key = full_write(&s, &base);
        let b = Some((key, &base[..]));
        let old = cuts(&base);
        for i in 0..320 {
            let kind = i % 10;
            let ed = match kind {
                // Overwrite that starts on an old cut.
                0 => {
                    let c = old[rng.below(old.len() as u64 - 1) as usize];
                    Edit { keep: n, changed: vec![c..(c + 1 + rng.below(20_000)).min(n)], new_len: n }
                }
                // Overwrite that starts anywhere (it may reach the end).
                1 => {
                    let a = rng.below(n);
                    Edit { keep: n, changed: vec![a..(a + 1 + rng.below(20_000)).min(n)], new_len: n }
                }
                // Append.
                2 => {
                    let k = 1 + rng.below(100_000);
                    Edit { keep: n, changed: vec![n..n + k], new_len: n + k }
                }
                // Pure shrink (every other time to an old cut).
                3 => {
                    let m = if i % 20 == 3 { old[rng.below(old.len() as u64 - 1) as usize] } else { rng.below(n) };
                    Edit { keep: m, changed: vec![], new_len: m }
                }
                // Shrink, then extend with zeros (and maybe a write).
                4 => {
                    let keep = rng.below(n);
                    let new_len = keep + 1 + rng.below(2 * n);
                    let changed = if i % 20 == 4 { extents(&mut rng, new_len, 1, 5000) } else { vec![] };
                    Edit { keep, changed, new_len }
                }
                // Extend with zeros.
                5 => Edit { keep: n, changed: vec![], new_len: n + 1 + rng.below(200_000) },
                // 2-5 extents at once, with a size change.
                6 => {
                    let new_len = n - 50_000 + rng.below(100_000);
                    let keep = rng.below(n.min(new_len) + 1).max(n.min(new_len) - 1000);
                    let changed = extents(&mut rng, new_len, 2 + (i / 10) % 4, 20_000);
                    Edit { keep, changed, new_len }
                }
                // No base.
                7 => {
                    let new_len = rng.below(300_000);
                    let changed = if new_len == 0 { vec![] } else { extents(&mut rng, new_len, 3, 100_000) };
                    let ed = Edit { keep: 0, changed, new_len };
                    check_splice(&s, None, &ed, &mut rng, "no base");
                    continue;
                }
                // New length 0.
                8 => Edit { keep: 0, changed: vec![], new_len: 0 },
                // Two extents, the second on the old cut after the first:
                // the re-chunk must take in a region that starts at its
                // resync point.
                _ => {
                    let j = rng.below(old.len() as u64 - 3) as usize;
                    let (c0, c1) = (old[j], old[j + 1]);
                    Edit { keep: n, changed: vec![c0 + 100..c0 + 101, c1..c1 + 3], new_len: n }
                }
            };
            check_splice(&s, b, &ed, &mut rng, &format!("edit {i} kind {kind}"));
        }
    }

    /// Start offsets of the leaves of a tree.
    fn leaf_starts(s: &ChunkStore, key: ObjectId) -> Vec<u64> {
        let mut c = Cursor::new(s, load_node(s, &key).unwrap()).unwrap();
        let mut out = Vec::new();
        while !c.at_end() {
            if c.path.last().unwrap().1 == 0 {
                out.push(c.off);
            }
            c.next(s).unwrap();
        }
        out
    }

    #[test]
    fn a_splice_across_many_leaves_gives_the_same_key() {
        let s = fast_store();
        let mut rng = Rng(0x5eed_1234_abcd_0002);
        let base = rng.bytes(32 << 20);
        let n = base.len() as u64;
        let key = full_write(&s, &base);
        let leaves = leaf_starts(&s, key);
        assert!(leaves.len() >= 3, "{} leaves", leaves.len());
        let lb = leaves[leaves.len() / 2];
        // The start of the last chunk of the leaf before `lb`. A change in
        // its first CHUNK_MIN bytes (not hashed by FastCDC) keeps the cut at
        // `lb` but changes the chunk ID: the re-chunk resyncs at `lb` while
        // the writer still holds entries of the new leaf.
        let old = cuts(&base);
        let c = old[old.partition_point(|&x| x < lb) - 1];
        let b = Some((key, &base[..]));
        let edits = [
            ("near the start", Edit { keep: n, changed: vec![100..4196], new_len: n }),
            ("in the middle", Edit { keep: n, changed: vec![n / 2..n / 2 + 4096], new_len: n }),
            ("at a leaf boundary", Edit { keep: n, changed: vec![lb..lb + 4096], new_len: n }),
            ("before a leaf boundary", Edit { keep: n, changed: vec![lb - 10..lb], new_len: n }),
            ("in the last chunk of a leaf", Edit { keep: n, changed: vec![c + 10..c + 11], new_len: n }),
            ("near the end", Edit { keep: n, changed: vec![n - 5000..n - 1000], new_len: n }),
            ("append", Edit { keep: n, changed: vec![n..n + 50_000], new_len: n + 50_000 }),
            ("shrink", Edit { keep: n - 100_000, changed: vec![], new_len: n - 100_000 }),
            ("shrink to a leaf boundary", Edit { keep: lb, changed: vec![], new_len: lb }),
            ("extend", Edit { keep: n, changed: vec![], new_len: n + 100_000 }),
            ("shrink then extend", Edit { keep: n / 2, changed: vec![n / 2 + 10..n / 2 + 20], new_len: n + 1000 }),
        ];
        for (what, ed) in &edits {
            check_splice(&s, b, ed, &mut rng, what);
        }
    }

    /// The premise of the zero shortcut: FastCDC cuts zeros at CHUNK_MAX.
    #[test]
    fn zero_runs_cut_at_chunk_max() {
        let max = CHUNK_MAX as usize;
        let lens: Vec<usize> = chunk_ranges(&vec![0u8; 4 * max + 100]).iter().map(|r| r.len()).collect();
        assert_eq!(lens, vec![max, max, max, max, 100]);
    }

    #[test]
    fn a_splice_over_zeros_and_repeating_data_gives_the_same_key() {
        let s = fast_store();
        let mut rng = Rng(0x5eed_1234_abcd_0003);
        let n = 8u64 << 20;
        let zeros = vec![0u8; n as usize];
        let pattern = rng.bytes(4096);
        let periodic: Vec<u8> = pattern.iter().copied().cycle().take(n as usize).collect();
        for (what, base) in [("zeros", &zeros), ("periodic", &periodic)] {
            let key = full_write(&s, base);
            let b = Some((key, &base[..]));
            let mut zero_runs = 0;
            for i in 0..20 {
                let ed = match i % 4 {
                    // Small overwrites in the middle of the run.
                    0 | 1 => Edit { keep: n, changed: extents(&mut rng, n, 1 + i % 3, 8192), new_len: n },
                    // A write past the end.
                    2 => {
                        let a = n + rng.below(1 << 20);
                        Edit { keep: n, changed: vec![a..a + 4096], new_len: a + 4096 }
                    }
                    // A shrink and a write.
                    _ => {
                        let keep = n / 2 + rng.below(n / 4);
                        Edit { keep, changed: extents(&mut rng, keep, 1, 4096), new_len: keep }
                    }
                };
                let st = check_splice(&s, b, &ed, &mut rng, &format!("{what} edit {i}"));
                zero_runs += st.zero_runs;
                if what == "zeros" && i % 4 < 2 {
                    // The zero shortcut ends the re-chunk of a small write
                    // soon, also off the old 32 KiB grid.
                    assert!(st.rechunked <= 256 << 10, "{what} edit {i}: {st:?}");
                }
            }
            if what == "zeros" {
                assert!(zero_runs > 0, "{what}: no zero runs");
            }
        }
        // Extend the zero base by 64 MiB of zeros.
        let key = full_write(&s, &zeros);
        let ed = Edit { keep: n, changed: vec![], new_len: n + (64 << 20) };
        let st = check_splice(&s, Some((key, &zeros[..])), &ed, &mut rng, "extend by 64 MiB");
        assert!(st.zero_runs >= 2000 && st.rechunked <= 256 << 10, "extend: {st:?}");
    }

    #[test]
    fn a_small_splice_reuses_old_nodes() {
        let s = fast_store();
        let mut rng = Rng(0x5eed_1234_abcd_0004);
        let base = rng.bytes(32 << 20);
        let n = base.len() as u64;
        let key = full_write(&s, &base);
        let ed = Edit { keep: n, changed: vec![n / 2..n / 2 + 4096], new_len: n };
        let data = new_content(&base, &ed, &mut rng);
        let before = s.stats().objects;
        let (got, st) = splice(&s, Some(key), ed.keep, &ed.changed, ed.new_len, &mut VecSource(&data)).unwrap();
        let added = s.stats().objects - before;
        eprintln!(
            "small splice: rechunked {} B, reused entries {}, reused nodes {}, new objects {added}",
            st.rechunked, st.reused_entries, st.reused_nodes
        );
        assert_eq!(got, full_write(&s, &data));
        assert!(st.rechunked <= 256 << 10, "{st:?}");
        assert!(st.reused_nodes >= 1, "{st:?}");
        assert!(added <= 12, "{added} new objects");

        // A deep tree (fake chunks, 8 GB): whole level-1 nodes are reused
        // and the key is still the writer's key.
        let fk = fake(1_000_000);
        let root = build(&s, &fk);
        assert!(load_node(&s, &root).unwrap().level >= 2);
        let len = fk.len() as u64 * 8192;
        let (same, st) = splice(&s, Some(root), len, &[], len, &mut VecSource(&[])).unwrap();
        assert_eq!(same, root, "no change: {st:?}");
        // Leaf-only reuse would take about one node per leaf.
        let leaves = leaf_starts(&s, root).len() as u64;
        assert!(st.reused_nodes + 60 < leaves, "no change: {st:?}, {leaves} leaves");
        // Shrink to an old cut: the chunk before it is cut again.
        let j = 900_000;
        let last = rng.bytes(8192);
        let mut src = TailSource { at: (j as u64 - 1) * 8192, data: &last };
        let (got, st) = splice(&s, Some(root), j as u64 * 8192, &[], j as u64 * 8192, &mut src).unwrap();
        let mut w = FileWriter::new();
        for e in &fk[..j - 1] {
            w.push_entry(&s, 0, *e).unwrap();
        }
        w.write(&s, &last).unwrap();
        assert_eq!(got, w.finish(&s).unwrap(), "shrink: {st:?}");
        assert!(st.reused_entries < 5000, "shrink: {st:?}");
    }

    /// New bytes that start at `at` (the splice reads nothing before).
    struct TailSource<'a> {
        at: u64,
        data: &'a [u8],
    }

    impl SpliceSource for TailSource<'_> {
        fn read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
            let i = (offset - self.at) as usize;
            Ok(self.data[i..i + len].to_vec())
        }
    }

    // A crafted child at level 255 gives an error on read, never an
    // overflow panic.
    #[test]
    fn a_read_of_a_level_255_child_is_an_error_not_a_panic() {
        let s = fast_store();
        let chunk = s.put(&[7u8; 50]).unwrap();
        let top_level = s.put(&encode_node(255, &[Entry { id: chunk, len: 50 }])).unwrap();
        let root = s.put(&encode_node(1, &[Entry { id: top_level, len: 50 }])).unwrap();
        let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            FileReader::open(&s, root).and_then(|mut r| r.read(&s, 0, 10))
        }));
        assert!(matches!(got, Ok(Err(Error::Corrupt(_)))), "{got:?}");
    }

    // A crafted base tree gives an error, never a panic.
    #[test]
    fn a_splice_of_a_malformed_tree_is_an_error_not_a_panic() {
        let s = fast_store();
        let chunk = s.put(&[7u8; 50]).unwrap();
        let e = |id, len| Entry { id, len };
        let leaf = s.put(&encode_node(0, &[e(chunk, 50)])).unwrap();
        let empty = s.put(&encode_node(0, &[])).unwrap();
        let wrong_level = s.put(&encode_node(2, &[e(chunk, 50)])).unwrap();
        let top_level = s.put(&encode_node(255, &[e(chunk, 50)])).unwrap();
        for (what, child, claim) in [
            ("longer claim", leaf, 100),
            ("empty child", empty, 10),
            ("wrong level", wrong_level, 50),
            ("level 255", top_level, 50),
        ] {
            let root = s.put(&encode_node(1, &[e(child, claim)])).unwrap();
            let data = vec![0u8; claim as usize];
            let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                splice(&s, Some(root), claim, &[0..1], claim, &mut VecSource(&data))
            }));
            assert!(matches!(got, Ok(Err(Error::Corrupt(_)))), "{what}: {got:?}");
        }
        // A broken precondition is an error too.
        let root = s.put(&encode_node(1, &[e(leaf, 50)])).unwrap();
        let data = vec![0u8; 60];
        let bad = |keep, changed: &[Range<u64>], new_len| {
            matches!(splice(&s, Some(root), keep, changed, new_len, &mut VecSource(&data)), Err(Error::Io(_)))
        };
        assert!(bad(51, &[], 60), "keep past the base");
        assert!(bad(50, &[5..10, 0..2], 60), "unsorted ranges");
        assert!(bad(50, &[5..61], 60), "range past the new length");
    }
}
