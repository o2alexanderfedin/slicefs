//! The node byte format (private to this crate, not a store format).
//!
//! Header: `NODE_MAGIC` (4 bytes), level `u8`, entry count `u32` LE.
//! Leaf entry (level 0): klen `u16` LE, vlen `u16` LE, key, value.
//! Internal entry (level > 0): klen `u16` LE, key (the last key of the
//! child), child `ObjectId` (`ID_LEN` bytes), subtree entry count `u64` LE.

use crate::{Error, Result, MAX_KEY, NODE_HEADER, NODE_MAGIC};
use chunk_store::{ChunkStore, ObjectId, ID_LEN};

/// One entry of an internal node: a child node and what it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Child {
    /// The last (largest) key under the child.
    pub last_key: Vec<u8>,
    /// The child node.
    pub id: ObjectId,
    /// Leaf entries under the child.
    pub count: u64,
}

/// A decoded node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Node {
    Leaf(Vec<(Vec<u8>, Vec<u8>)>),
    Internal { level: u8, children: Vec<Child> },
}

impl Node {
    pub fn level(&self) -> u8 {
        match self {
            Node::Leaf(_) => 0,
            Node::Internal { level, .. } => *level,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Node::Leaf(e) => e.len(),
            Node::Internal { children, .. } => children.len(),
        }
    }

    fn first_key(&self) -> Option<&[u8]> {
        match self {
            Node::Leaf(e) => e.first().map(|(k, _)| k.as_slice()),
            Node::Internal { children, .. } => children.first().map(|c| c.last_key.as_slice()),
        }
    }

    fn last_key(&self) -> Option<&[u8]> {
        match self {
            Node::Leaf(e) => e.last().map(|(k, _)| k.as_slice()),
            Node::Internal { children, .. } => children.last().map(|c| c.last_key.as_slice()),
        }
    }
}

/// Encoded bytes of a leaf entry.
pub(crate) fn leaf_entry_len(klen: usize, vlen: usize) -> usize {
    4 + klen + vlen
}

/// Encoded bytes of an internal entry.
pub(crate) fn internal_entry_len(klen: usize) -> usize {
    2 + klen + ID_LEN + 8
}

fn header(level: u8, count: usize, cap: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(cap);
    b.extend_from_slice(&NODE_MAGIC);
    b.push(level);
    b.extend_from_slice(&(count as u32).to_le_bytes());
    b
}

/// The bytes of a leaf. The caller keeps keys and values within the
/// limits (the builder checks them).
pub(crate) fn encode_leaf(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let cap = NODE_HEADER + entries.iter().map(|(k, v)| leaf_entry_len(k.len(), v.len())).sum::<usize>();
    let mut b = header(0, entries.len(), cap);
    for (k, v) in entries {
        b.extend_from_slice(&(k.len() as u16).to_le_bytes());
        b.extend_from_slice(&(v.len() as u16).to_le_bytes());
        b.extend_from_slice(k);
        b.extend_from_slice(v);
    }
    b
}

/// The bytes of an internal node at `level` (at least 1).
pub(crate) fn encode_internal(level: u8, children: &[Child]) -> Vec<u8> {
    let cap = NODE_HEADER + children.iter().map(|c| internal_entry_len(c.last_key.len())).sum::<usize>();
    let mut b = header(level, children.len(), cap);
    for c in children {
        b.extend_from_slice(&(c.last_key.len() as u16).to_le_bytes());
        b.extend_from_slice(&c.last_key);
        b.extend_from_slice(&c.id.0);
        b.extend_from_slice(&c.count.to_le_bytes());
    }
    b
}

fn corrupt(what: &str) -> Error {
    Error::Corrupt(format!("prolly node: {what}"))
}

/// A bounds-checked reader over node bytes.
struct Input<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Input<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or_else(|| corrupt("length overflow"))?;
        let s = self.b.get(self.at..end).ok_or_else(|| corrupt("entry runs past the end"))?;
        self.at = end;
        Ok(s)
    }

    fn u16(&mut self) -> Result<usize> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]) as usize)
    }

    fn key(&mut self) -> Result<Vec<u8>> {
        let klen = self.u16()?;
        if klen > MAX_KEY {
            return Err(corrupt("key above MAX_KEY"));
        }
        Ok(self.take(klen)?.to_vec())
    }
}

/// Decodes node bytes. Never panics: every length is checked against the
/// buffer before use, and the count before any allocation.
pub(crate) fn decode(bytes: &[u8]) -> Result<Node> {
    if bytes.len() < NODE_HEADER {
        return Err(corrupt("shorter than the header"));
    }
    if bytes[..4] != NODE_MAGIC {
        return Err(corrupt("wrong magic"));
    }
    let level = bytes[4];
    let count = u32::from_le_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) as usize;
    let remaining = bytes.len() - NODE_HEADER;
    let smallest = if level == 0 { leaf_entry_len(0, 0) } else { internal_entry_len(0) };
    if count > remaining / smallest {
        return Err(corrupt("count does not fit in the bytes"));
    }
    if level > 0 && count == 0 {
        return Err(corrupt("internal node with no entries"));
    }
    let mut r = Input { b: bytes, at: NODE_HEADER };
    let node = if level == 0 {
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(count);
        for _ in 0..count {
            let klen = r.u16()?;
            let vlen = r.u16()?;
            if klen > MAX_KEY {
                return Err(corrupt("key above MAX_KEY"));
            }
            let k = r.take(klen)?.to_vec();
            let v = r.take(vlen)?.to_vec();
            if entries.last().is_some_and(|(p, _)| *p >= k) {
                return Err(corrupt("keys not strictly ascending"));
            }
            entries.push((k, v));
        }
        Node::Leaf(entries)
    } else {
        let mut children: Vec<Child> = Vec::with_capacity(count);
        for _ in 0..count {
            let last_key = r.key()?;
            let id = <[u8; ID_LEN]>::try_from(r.take(ID_LEN)?).map_err(|_| corrupt("bad child id"))?;
            let c = r.take(8)?;
            let n = u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
            if children.last().is_some_and(|p| p.last_key >= last_key) {
                return Err(corrupt("keys not strictly ascending"));
            }
            children.push(Child { last_key, id: ObjectId(id), count: n });
        }
        Node::Internal { level, children }
    };
    if r.at != bytes.len() {
        return Err(corrupt("bytes after the last entry"));
    }
    Ok(node)
}

/// Reads (hash-verified) and decodes node `id`. With `want_level` (a
/// child), the node must be at that level and must not be empty.
pub(crate) fn load(store: &ChunkStore, id: &ObjectId, want_level: Option<u8>) -> Result<(Node, usize)> {
    let bytes = store.read(id)?;
    let node = decode(&bytes)?;
    if let Some(want) = want_level {
        if node.level() != want {
            return Err(Error::Corrupt(format!(
                "prolly node: child at wrong level {} (want {want})",
                node.level()
            )));
        }
        if node.len() == 0 {
            return Err(corrupt("empty child node"));
        }
    }
    Ok((node, bytes.len()))
}

/// Reads and decodes the child `entry` of a node, at `level`, and checks
/// it against the entry: its last key is the entry's key, its first key is
/// above `lo` (the last key of the previous sibling, or the bound of the
/// parent; `None` on the leftmost path), and its leaf entries are the
/// entry's count. With the order inside each node, these keep the keys of
/// a tree in order and its counts true along every loaded path.
pub(crate) fn load_child(store: &ChunkStore, level: u8, entry: &Child, lo: Option<&[u8]>) -> Result<(Node, usize)> {
    let (node, bytes) = load(store, &entry.id, Some(level))?;
    if node.last_key() != Some(entry.last_key.as_slice()) {
        return Err(corrupt("child's last key is not its parent entry's key"));
    }
    if let (Some(lo), Some(first)) = (lo, node.first_key()) {
        if first <= lo {
            return Err(corrupt("child's first key is not above the previous sibling"));
        }
    }
    let count = match &node {
        Node::Leaf(e) => e.len() as u64,
        Node::Internal { children, .. } => children
            .iter()
            .try_fold(0u64, |n, c| n.checked_add(c.count))
            .ok_or_else(|| corrupt("subtree count above u64"))?,
    };
    if count != entry.count {
        return Err(corrupt("child's entry count is not its parent entry's count"));
    }
    Ok((node, bytes))
}
