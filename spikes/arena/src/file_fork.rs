//! File content on the chunk store: a streaming writer that chunks with
//! FastCDC, a manifest tree of nodes, and a reader for byte ranges.
//!
//! Node format: magic `SFM1`, level `u8`, entry count `u32 LE`, then entries
//! of `ObjectId` (28 bytes) + byte length `u64 LE`. Level 0 entries are
//! chunks; a level `k > 0` entry is a level `k - 1` node and the bytes it
//! covers. A node holds at most `LEAF_ENTRIES` entries. The tree depends
//! only on the chunk sequence, so the key of a file does not depend on the
//! sizes of the writes that made it.

//!
//! Forked from `chunk-store/src/file.rs` for the arena spike: the code is the
//! same, but it runs on any `ObjStore` (the real `ChunkStore` or the
//! `SpikeStore`), so every workload runs against both.

use chunk_store::{chunk_ranges, ChunkStore, Error, ObjectId, Result, CHUNK_MAX, ID_LEN};
use std::collections::HashMap;
use std::sync::Arc;

/// What the file layer and the benches need from a store.
pub trait ObjStore: Sync {
    fn put(&self, data: &[u8]) -> Result<ObjectId>;
    fn get(&self, id: &ObjectId) -> Option<Vec<u8>>;
    fn sync(&self) -> Result<()>;
    fn close(&self) -> Result<()>;
    fn stats(&self) -> chunk_store::Stats;
}

impl ObjStore for ChunkStore {
    fn put(&self, data: &[u8]) -> Result<ObjectId> {
        ChunkStore::put(self, data)
    }
    fn get(&self, id: &ObjectId) -> Option<Vec<u8>> {
        ChunkStore::get(self, id)
    }
    fn sync(&self) -> Result<()> {
        ChunkStore::sync(self)
    }
    fn close(&self) -> Result<()> {
        ChunkStore::close(self)
    }
    fn stats(&self) -> chunk_store::Stats {
        ChunkStore::stats(self)
    }
}

impl ObjStore for crate::SpikeStore {
    fn put(&self, data: &[u8]) -> Result<ObjectId> {
        crate::SpikeStore::put(self, data)
    }
    fn get(&self, id: &ObjectId) -> Option<Vec<u8>> {
        crate::SpikeStore::get(self, id)
    }
    fn sync(&self) -> Result<()> {
        crate::SpikeStore::sync(self)
    }
    fn close(&self) -> Result<()> {
        crate::SpikeStore::close(self)
    }
    fn stats(&self) -> chunk_store::Stats {
        crate::SpikeStore::stats(self)
    }
}

/// Entries per manifest node.
pub const LEAF_ENTRIES: usize = 1024;

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

fn load_node(store: &dyn ObjStore, id: &ObjectId) -> Result<Node> {
    let bytes = store.get(id).ok_or(Error::Missing(*id))?;
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
    pub fn write(&mut self, store: &dyn ObjStore, data: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(data);
        self.len += data.len() as u64;
        if self.buf.len() >= 2 * CHUNK_MAX as usize {
            self.cut(store, false)?;
        }
        Ok(())
    }

    /// Append `n` zero bytes.
    pub fn write_zeros(&mut self, store: &dyn ObjStore, mut n: u64) -> Result<()> {
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
    fn cut(&mut self, store: &dyn ObjStore, all: bool) -> Result<()> {
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

    fn push(&mut self, store: &dyn ObjStore, level: usize, e: Entry) -> Result<()> {
        if self.levels.len() <= level {
            self.levels.resize_with(level + 1, Vec::new);
        }
        self.levels[level].push(e);
        if self.levels[level].len() == LEAF_ENTRIES {
            let entries = std::mem::take(&mut self.levels[level]);
            let node = self.put_node(store, level, &entries)?;
            self.push(store, level + 1, node)?;
        }
        Ok(())
    }

    fn put_node(&self, store: &dyn ObjStore, level: usize, entries: &[Entry]) -> Result<Entry> {
        let id = store.put(&encode_node(level as u8, entries))?;
        Ok(Entry { id, len: entries.iter().map(|e| e.len).sum() })
    }

    /// End the stream and return the key of the content (the root node).
    pub fn finish(mut self, store: &dyn ObjStore) -> Result<ObjectId> {
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

impl FileReader {
    /// Open the content `key`.
    pub fn open(store: &dyn ObjStore, key: ObjectId) -> Result<Self> {
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

    fn node(&mut self, store: &dyn ObjStore, id: &ObjectId) -> Result<Arc<Node>> {
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
    pub fn read(&mut self, store: &dyn ObjStore, offset: u64, size: usize) -> Result<Vec<u8>> {
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
            if self.chunk.as_ref().map(|(c, _)| *c) != Some(id) {
                let bytes = store.get(&id).ok_or(Error::Missing(id))?;
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
pub fn file_objects(store: &dyn ObjStore, key: ObjectId) -> Result<Vec<ObjectId>> {
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
