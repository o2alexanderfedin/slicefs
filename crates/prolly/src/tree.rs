//! Build and read a tree: the streaming builder, `get`.

use crate::node::{self, encode_internal, encode_leaf, internal_entry_len, leaf_entry_len, Child, Node};
use crate::{Config, Error, Result, Splitter};
use chunk_store::{ChunkStore, ObjectId};

/// A node just written: what its parent entry needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NodeRef {
    /// The last key under the node.
    pub last_key: Vec<u8>,
    /// The node.
    pub id: ObjectId,
    /// Leaf entries under the node.
    pub count: u64,
    /// Encoded bytes of the node.
    pub bytes: usize,
}

/// Counts of a build.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BuildStats {
    /// Nodes passed to `ChunkStore::put` (dedup may store fewer).
    pub nodes_written: u64,
    /// Bytes passed to `ChunkStore::put`.
    pub bytes_written: u64,
    /// Largest sum of the bytes of all nodes in progress (the memory bound).
    pub high_water_bytes: usize,
    /// Levels of the tree: 1 for one leaf.
    pub depth: u8,
    /// Leaf nodes written.
    pub leaves: u64,
    /// Leaf entries in the tree.
    pub entries: u64,
}

/// The node in progress of one level. Every entry goes through the one
/// `Splitter`.
pub(crate) struct LevelWriter {
    level: u8,
    splitter: Splitter,
    leaf: Vec<(Vec<u8>, Vec<u8>)>,
    children: Vec<Child>,
    /// Leaf entries under the node in progress.
    count: u64,
    /// Nodes this level has written.
    emitted: u64,
    /// The first node this level wrote.
    first: Option<ObjectId>,
    /// The last key of the last node this level wrote: every key pushed
    /// after it must be above it.
    prev: Option<Vec<u8>>,
}

impl LevelWriter {
    pub fn new(level: u8) -> Self {
        Self {
            level,
            splitter: Splitter::new(),
            leaf: Vec::new(),
            children: Vec::new(),
            count: 0,
            emitted: 0,
            first: None,
            prev: None,
        }
    }

    /// A key that is not above the last key pushed is an error: the input
    /// came from a damaged or crafted tree (the builder checks its own
    /// input first).
    fn check_ascending(&self, key: &[u8]) -> Result<()> {
        let last = if self.level == 0 {
            self.leaf.last().map(|(k, _)| k.as_slice())
        } else {
            self.children.last().map(|c| c.last_key.as_slice())
        };
        match last.or(self.prev.as_deref()) {
            Some(last) if key <= last => Err(Error::Corrupt("prolly: keys not ascending in a rewrite".into())),
            _ => Ok(()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.leaf.is_empty() && self.children.is_empty()
    }

    /// Encoded bytes of the node in progress (0 when empty).
    pub fn buffered_bytes(&self) -> usize {
        if self.is_empty() {
            0
        } else {
            self.splitter.size()
        }
    }

    pub fn push_leaf(
        &mut self,
        store: &ChunkStore,
        cfg: &Config,
        key: &[u8],
        value: &[u8],
        stats: &mut BuildStats,
    ) -> Result<Option<NodeRef>> {
        self.check_ascending(key)?;
        let cut = self.splitter.push(cfg, self.level, key, leaf_entry_len(key.len(), value.len()));
        self.leaf.push((key.to_vec(), value.to_vec()));
        self.count += 1;
        if cut {
            self.flush(store, stats)
        } else {
            Ok(None)
        }
    }

    pub fn push_child(
        &mut self,
        store: &ChunkStore,
        cfg: &Config,
        child: Child,
        stats: &mut BuildStats,
    ) -> Result<Option<NodeRef>> {
        self.check_ascending(&child.last_key)?;
        let cut = self.splitter.push(cfg, self.level, &child.last_key, internal_entry_len(child.last_key.len()));
        self.count += child.count;
        self.children.push(child);
        if cut {
            self.flush(store, stats)
        } else {
            Ok(None)
        }
    }

    /// Writes the node in progress, if any.
    pub fn flush(&mut self, store: &ChunkStore, stats: &mut BuildStats) -> Result<Option<NodeRef>> {
        if self.is_empty() {
            return Ok(None);
        }
        let (bytes, last_key) = if self.level == 0 {
            let b = encode_leaf(&self.leaf);
            (b, self.leaf.pop().map(|(k, _)| k).unwrap_or_default())
        } else {
            let b = encode_internal(self.level, &self.children);
            (b, self.children.pop().map(|c| c.last_key).unwrap_or_default())
        };
        let id = store.put(&bytes)?;
        stats.nodes_written += 1;
        stats.bytes_written += bytes.len() as u64;
        if self.level == 0 {
            stats.leaves += 1;
        }
        self.emitted += 1;
        self.first.get_or_insert(id);
        self.prev = Some(last_key.clone());
        let r = NodeRef { last_key, id, count: self.count, bytes: bytes.len() };
        self.leaf.clear();
        self.children.clear();
        self.count = 0;
        self.splitter.reset();
        Ok(Some(r))
    }
}

/// Builds a tree from entries in strictly ascending key order, with one
/// node in progress per level: memory is O(depth x node_max).
pub struct Builder<'s> {
    store: &'s ChunkStore,
    cfg: Config,
    /// The level of `levels[0]`.
    base: u8,
    levels: Vec<LevelWriter>,
    last_key: Option<Vec<u8>>,
    /// Nodes given with `push_node` (a builder made with `with_level`).
    pushed: u64,
    last_pushed: Option<ObjectId>,
    stats: BuildStats,
}

impl<'s> Builder<'s> {
    /// A builder of a new tree from leaf entries.
    pub fn new(store: &'s ChunkStore, cfg: Config) -> Self {
        Self::with_level(store, cfg, 0)
    }

    /// A builder whose lowest level is `level`; above 0 it takes finished
    /// nodes of level `level - 1` with `push_node`.
    pub(crate) fn with_level(store: &'s ChunkStore, cfg: Config, level: u8) -> Self {
        Self {
            store,
            cfg,
            base: level,
            levels: vec![LevelWriter::new(level)],
            last_key: None,
            pushed: 0,
            last_pushed: None,
            stats: BuildStats::default(),
        }
    }

    fn check_order(&mut self, key: &[u8]) -> Result<()> {
        match &mut self.last_key {
            Some(last) if key <= last.as_slice() => Err(Error::Unsorted),
            Some(last) => {
                last.clear();
                last.extend_from_slice(key);
                Ok(())
            }
            None => {
                self.last_key = Some(key.to_vec());
                Ok(())
            }
        }
    }

    /// Adds the next entry; its key must be above the last one.
    pub fn push(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        debug_assert_eq!(self.base, 0, "leaf entries go to a builder of level 0");
        if key.len() > self.cfg.max_key() {
            return Err(Error::KeyTooLarge { len: key.len(), max: self.cfg.max_key() });
        }
        let len = leaf_entry_len(key.len(), value.len());
        if len > self.cfg.max_entry() {
            return Err(Error::EntryTooLarge { len, max: self.cfg.max_entry() });
        }
        self.check_order(key)?;
        let r = self.levels[0].push_leaf(self.store, &self.cfg, key, value, &mut self.stats)?;
        self.stats.entries += 1;
        self.cascade(0, r)?;
        self.note_high_water();
        Ok(())
    }

    /// Adds a finished node of level `base - 1`; its last key must be above
    /// the last one.
    pub(crate) fn push_node(&mut self, r: NodeRef) -> Result<()> {
        debug_assert!(self.base > 0, "nodes go to a builder above level 0");
        self.check_order(&r.last_key)?;
        self.pushed += 1;
        self.last_pushed = Some(r.id);
        self.stats.entries += r.count;
        let child = Child { last_key: r.last_key, id: r.id, count: r.count };
        let out = self.levels[0].push_child(self.store, &self.cfg, child, &mut self.stats)?;
        self.cascade(0, out)?;
        self.note_high_water();
        Ok(())
    }

    fn note_high_water(&mut self) {
        let sum = self.levels.iter().map(LevelWriter::buffered_bytes).sum::<usize>();
        self.stats.high_water_bytes = self.stats.high_water_bytes.max(sum);
    }

    /// Pushes a node written by `levels[i]` up, level by level.
    fn cascade(&mut self, mut i: usize, mut r: Option<NodeRef>) -> Result<()> {
        while let Some(n) = r {
            i += 1;
            if self.levels.len() == i {
                let level = self.base.checked_add(i as u8).ok_or_else(|| Error::Corrupt("tree too deep".into()))?;
                self.levels.push(LevelWriter::new(level));
            }
            let child = Child { last_key: n.last_key, id: n.id, count: n.count };
            r = self.levels[i].push_child(self.store, &self.cfg, child, &mut self.stats)?;
        }
        Ok(())
    }

    /// Writes the nodes in progress and returns the root. The root is
    /// canonical: the only node of the lowest level that wrote exactly one
    /// node, never an internal node with one child; no entries give
    /// `empty_root`.
    pub fn finish(mut self) -> Result<(ObjectId, BuildStats)> {
        if self.base > 0 && self.pushed <= 1 {
            // One node in: it is the root (its level holds no cut).
            let root = match self.last_pushed {
                Some(id) => id,
                None => empty_root(self.store)?,
            };
            self.stats.depth = if self.pushed == 1 { self.base } else { 1 };
            return Ok((root, self.stats));
        }
        let mut i = 0;
        loop {
            let r = self.levels[i].flush(self.store, &mut self.stats)?;
            self.cascade(i, r)?;
            let lw = &self.levels[i];
            match (lw.emitted, lw.first) {
                (1, Some(id)) => {
                    self.stats.depth = lw.level + 1;
                    return Ok((id, self.stats));
                }
                (0, _) => {
                    // No entries at all (only level 0 can write nothing).
                    self.stats.depth = 1;
                    return Ok((empty_root(self.store)?, self.stats));
                }
                _ => i += 1,
            }
        }
    }
}

/// The limits that `get`, the cursor and `walk` check nodes against: the
/// largest config (the crate constants), so they read a tree of any config.
/// `apply` checks against its own config, because it writes the old entries
/// again.
const READ_CFG: Config = Config::DEFAULT;

/// The root of the empty map: the empty leaf object.
pub fn empty_root(store: &ChunkStore) -> Result<ObjectId> {
    Ok(store.put(&encode_leaf(&[]))?)
}

/// Builds a tree from entries in strictly ascending key order.
pub fn build<K, V>(store: &ChunkStore, cfg: Config, entries: impl IntoIterator<Item = (K, V)>) -> Result<(ObjectId, BuildStats)>
where
    K: AsRef<[u8]>,
    V: AsRef<[u8]>,
{
    let mut b = Builder::new(store, cfg);
    for (k, v) in entries {
        b.push(k.as_ref(), v.as_ref())?;
    }
    b.finish()
}

/// The value of `key` in the tree `root`, reading one path of nodes. Each
/// node of the path is checked against its parent entry; a bad node that
/// is not on the path is not seen.
pub fn get(store: &ChunkStore, root: &ObjectId, key: &[u8]) -> Result<Option<Vec<u8>>> {
    let (mut n, _) = node::load(store, &READ_CFG, root, None)?;
    let mut lo: Option<Vec<u8>> = None;
    loop {
        match n {
            Node::Internal { level, mut children } => {
                let i = children.partition_point(|c| c.last_key.as_slice() < key);
                if i >= children.len() {
                    return Ok(None);
                }
                if i > 0 {
                    lo = Some(std::mem::take(&mut children[i - 1].last_key));
                }
                n = node::load_child(store, &READ_CFG, level - 1, &children[i], lo.as_deref())?.0;
            }
            Node::Leaf(entries) => {
                return Ok(entries.binary_search_by(|(k, _)| k.as_slice().cmp(key)).ok().map(|i| entries[i].1.clone()));
            }
        }
    }
}

/// An ordered iterator over the entries of a tree. It holds one path of
/// nodes (one per level), never the map. Each node is checked against its
/// parent entry when it is loaded.
pub struct Cursor<'s> {
    store: &'s ChunkStore,
    /// Per node of the path: the node, the next entry, and the bound that
    /// its first key must be above (`None` on the leftmost path).
    stack: Vec<(Node, usize, Option<Vec<u8>>)>,
    max_held: usize,
    done: bool,
}

impl Cursor<'_> {
    /// The most nodes held at one time so far.
    pub fn max_nodes_held(&self) -> usize {
        self.max_held
    }
}

/// A cursor at the first entry of the tree `root`.
pub fn iter<'s>(store: &'s ChunkStore, root: &ObjectId) -> Result<Cursor<'s>> {
    let (n, _) = node::load(store, &READ_CFG, root, None)?;
    Ok(Cursor { store, stack: vec![(n, 0, None)], max_held: 1, done: false })
}

impl Iterator for Cursor<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    /// The next entry in key order; after an error, `None`.
    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            let Some((n, i, lo)) = self.stack.last_mut() else {
                self.done = true;
                break;
            };
            match n {
                Node::Leaf(entries) => {
                    if let Some(e) = entries.get_mut(*i) {
                        *i += 1;
                        return Some(Ok(std::mem::take(e)));
                    }
                    self.stack.pop();
                }
                Node::Internal { level, children } => {
                    let Some(c) = children.get(*i) else {
                        self.stack.pop();
                        continue;
                    };
                    let child_lo = if *i > 0 { Some(children[*i - 1].last_key.clone()) } else { lo.clone() };
                    *i += 1;
                    match node::load_child(self.store, &READ_CFG, *level - 1, c, child_lo.as_deref()) {
                        Ok((child, _)) => {
                            self.stack.push((child, 0, child_lo));
                            self.max_held = self.max_held.max(self.stack.len());
                        }
                        Err(e) => {
                            self.done = true;
                            return Some(Err(e));
                        }
                    }
                }
            }
        }
        None
    }
}

/// One node seen by `walk`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    /// Level of the node (0 is a leaf).
    pub level: u8,
    /// Entries in the node.
    pub entries: usize,
    /// Encoded bytes of the node.
    pub bytes: usize,
    /// The last key of the node (empty for the empty leaf).
    pub last_key: Vec<u8>,
    /// True on the rightmost path: the last node of its level.
    pub last_of_level: bool,
}

/// Calls `f` for every node of the tree `root`, depth first and left to
/// right, holding one path of nodes. Each node is checked against its
/// parent entry when it is loaded.
pub fn walk(store: &ChunkStore, root: &ObjectId, mut f: impl FnMut(&NodeInfo)) -> Result<()> {
    let (n, bytes) = node::load(store, &READ_CFG, root, None)?;
    walk_node(store, n, bytes, None, true, &mut f)
}

/// Visits the loaded node `n` and its subtree; `lo` is the bound that the
/// first key of `n` was checked against.
fn walk_node<F: FnMut(&NodeInfo)>(
    store: &ChunkStore,
    n: Node,
    bytes: usize,
    lo: Option<&[u8]>,
    last_of_level: bool,
    f: &mut F,
) -> Result<()> {
    let (level, entries, last_key) = match &n {
        Node::Leaf(e) => (0, e.len(), e.last().map(|(k, _)| k.clone()).unwrap_or_default()),
        Node::Internal { level, children } => {
            (*level, children.len(), children.last().map(|c| c.last_key.clone()).unwrap_or_default())
        }
    };
    f(&NodeInfo { level, entries, bytes, last_key, last_of_level });
    if let Node::Internal { level, children } = &n {
        for (i, c) in children.iter().enumerate() {
            let child_lo = if i > 0 { Some(children[i - 1].last_key.as_slice()) } else { lo };
            let (child, child_bytes) = node::load_child(store, &READ_CFG, level - 1, c, child_lo)?;
            walk_node(store, child, child_bytes, child_lo, last_of_level && i + 1 == children.len(), f)?;
        }
    }
    Ok(())
}
