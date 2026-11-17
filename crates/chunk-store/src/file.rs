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

use crate::{chunk_ranges, hashers, ChunkStore, Error, ObjectId, Result, CHUNK_MAX, DIGEST_LEN, ID_LEN};
use std::collections::HashMap;
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
                node = self.node(store, &e.id)?;
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

#[cfg(test)]
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
}
