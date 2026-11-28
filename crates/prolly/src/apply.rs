//! Batched edit of a tree: `apply`.
//!
//! One batch (one commit) is a slice of puts and deletes sorted by key.
//! Each level is rewritten with the edits of that level, from the leaves
//! up. At a touched node a fresh `LevelWriter` (the writer of the bulk
//! builder, so build and apply cannot cut differently) takes the old
//! entries of the node with the edits merged in. At each old node end the
//! run stops when the writer is empty (a new cut fell on the old node
//! end) and the next old node has no edits: from there the old chunking
//! is the new chunking (resync). Each replaced node and each new node
//! becomes an edit of the level above: "delete the child with this last
//! key" and "put the child (last key, id, count)". A put equal to the old
//! child entry is dropped, so a level with no change stops the walk and
//! the old root stays.
//!
//! Above the old top level, new levels are made with the bulk builder.
//! The root is canonical: while it is an internal node with one child,
//! the child is the root, and no entries give `empty_root`. A shrink of
//! the height may write up to depth nodes that this collapse skips; they
//! stay unreferenced in the store (a later GC would drop them).
//!
//! Memory is O(changes x depth) plus the batch: one path of decoded nodes
//! per level pass, one old node and one writer buffer, and the sorted
//! edits of the next level in a `Vec`. Node reads go through the
//! optional fixed-cap cache of the `Ctx` (D-17 forbids a cache that grows
//! with the map, not a fixed one); there is no keyed collection of nodes
//! besides it.

use crate::cache::Ctx;
use crate::node::{self, leaf_entry_len, Child, Node};
use crate::tree::{empty_root, BuildStats, Builder, LevelWriter, NodeRef};
use crate::{Config, Error, Result};
use chunk_store::{ChunkStore, ObjectId, ID_LEN};
use std::cmp::Ordering;
use std::sync::Arc;

/// One edit of a batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// Set `key` to `value` (insert or replace).
    Put { key: Vec<u8>, value: Vec<u8> },
    /// Remove `key`; nothing happens when it is absent.
    Delete { key: Vec<u8> },
}

impl Edit {
    /// The key of the edit.
    pub fn key(&self) -> &[u8] {
        match self {
            Edit::Put { key, .. } | Edit::Delete { key } => key,
        }
    }
}

/// Counts of one `apply`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApplyStats {
    /// Nodes loaded, from the store or from the cache (so the count
    /// measures work, not cache luck).
    pub nodes_read: u64,
    /// Of `nodes_read`, the nodes the cache gave.
    pub cache_hits: u64,
    /// Nodes passed to `ChunkStore::put` (dedup may store fewer).
    pub nodes_written: u64,
    /// Bytes passed to `ChunkStore::put`.
    pub bytes_written: u64,
    /// Largest sum seen of the decoded old nodes held, the writer buffer
    /// and the pending edits (the memory bound).
    pub high_water_bytes: usize,
    /// Cuts made by the forced rule (size above `force_at`) in the new nodes.
    pub forced_cuts: u64,
}

/// The payload of an entry at any level.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Payload {
    Value(Vec<u8>),
    Child { id: ObjectId, count: u64 },
}

impl Payload {
    fn bytes(&self) -> usize {
        match self {
            Payload::Value(v) => v.len(),
            Payload::Child { .. } => ID_LEN + 8,
        }
    }
}

/// An edit of one level: a put (`Some`) or a delete (`None`) of the key.
type Op = (Vec<u8>, Option<Payload>);

fn op_bytes(ops: &[Op]) -> usize {
    ops.iter().map(|(k, p)| k.len() + p.as_ref().map_or(0, Payload::bytes)).sum()
}

/// The store, the counts and the memory high water of one `apply`.
struct Work<'s> {
    ctx: Ctx<'s>,
    cfg: Config,
    writes: BuildStats,
    reads: u64,
    hits: u64,
    high: usize,
}

impl Work<'_> {
    /// Loads node `id` with the limits of the apply's config: the old
    /// entries are written again, so each must fit the config (no key above
    /// `max_key`, no entry above `max_entry`), else the size bound of the
    /// new nodes would not hold.
    fn load(&mut self, id: &ObjectId, want: Option<u8>) -> Result<(Arc<Node>, usize)> {
        let l = node::load(&self.ctx, &self.cfg, id, want)?;
        Ok(self.count(l))
    }

    /// Loads the child `entry` at `level` and checks it against the entry
    /// (`node::load_child`), with the limits of the apply's config.
    fn load_child(&mut self, level: u8, entry: &Child, lo: Option<&[u8]>) -> Result<(Arc<Node>, usize)> {
        let l = node::load_child(&self.ctx, &self.cfg, level, entry, lo)?;
        Ok(self.count(l))
    }

    fn count(&mut self, l: node::Loaded) -> (Arc<Node>, usize) {
        self.reads += 1;
        self.hits += l.hit as u64;
        (l.node, l.bytes)
    }

    fn note(&mut self, held: usize) {
        self.high = self.high.max(held);
    }

    fn stats(&self) -> ApplyStats {
        ApplyStats {
            nodes_read: self.reads,
            cache_hits: self.hits,
            nodes_written: self.writes.nodes_written,
            bytes_written: self.writes.bytes_written,
            high_water_bytes: self.high,
            forced_cuts: self.writes.forced_cuts,
        }
    }
}

/// The decoded old root.
struct Root {
    id: ObjectId,
    node: Arc<Node>,
    bytes: usize,
}

/// The children of one internal node on the cursor path.
struct Frame {
    /// Level of the children.
    level: u8,
    children: Vec<Child>,
    at: usize,
    bytes: usize,
    /// The bound that the first key of `children[0]` must be above (`None`
    /// on the leftmost path).
    lo: Option<Vec<u8>>,
}

impl Frame {
    /// The bound of the first key of child `i`: the last key of the
    /// previous child, or the bound of the frame.
    fn lo_of(&self, i: usize) -> Option<&[u8]> {
        if i > 0 {
            Some(&self.children[i - 1].last_key)
        } else {
            self.lo.as_deref()
        }
    }
}

/// A forward-only position on the nodes of one level: the path of decoded
/// internal nodes from the root down to the parent of the current node
/// (the node itself is not read). Each internal node is read at most once
/// per level pass.
struct LevelCursor {
    level: u8,
    path: Vec<Frame>,
}

fn children_of(n: Arc<Node>) -> Result<Vec<Child>> {
    match Arc::try_unwrap(n).unwrap_or_else(|a| (*a).clone()) {
        Node::Internal { children, .. } => Ok(children),
        Node::Leaf(_) => Err(Error::Corrupt("prolly node: leaf where an internal node must be".into())),
    }
}

impl LevelCursor {
    /// A cursor at the first node of `level`.
    fn new(w: &mut Work, root: &Root, level: u8) -> Result<Self> {
        let top = root.node.level();
        let first = if level == top {
            // A virtual parent of the root; the root is the last node of its level.
            // It is never loaded as a child, so its entry is not checked.
            Frame { level, children: vec![Child { last_key: Vec::new(), id: root.id, count: 0 }], at: 0, bytes: 0, lo: None }
        } else {
            let children = children_of(root.node.clone())?;
            Frame { level: top - 1, children, at: 0, bytes: root.bytes, lo: None }
        };
        let mut c = LevelCursor { level, path: vec![first] };
        c.descend(w, None)?;
        Ok(c)
    }

    fn deepest(&self) -> &Frame {
        &self.path[self.path.len() - 1]
    }

    /// Loads nodes down to the parents of `level`: at each node the first
    /// child whose last key is at or above `key` (or the last child), or
    /// the first child without a key.
    fn descend(&mut self, w: &mut Work, key: Option<&[u8]>) -> Result<()> {
        while self.deepest().level > self.level {
            let f = self.deepest();
            let want = f.level;
            let lo = f.lo_of(f.at).map(<[u8]>::to_vec);
            let (n, bytes) = w.load_child(want, &f.children[f.at], lo.as_deref())?;
            let children = children_of(n)?;
            let at = key.map_or(0, |k| children.partition_point(|c| c.last_key.as_slice() < k).min(children.len() - 1));
            self.path.push(Frame { level: want - 1, children, at, bytes, lo });
        }
        Ok(())
    }

    /// True when every frame above depth `d` is at its last child.
    fn right_edge(&self, d: usize) -> bool {
        self.path[..d].iter().all(|f| f.at + 1 == f.children.len())
    }

    /// Moves forward to the first node whose last key is at or above
    /// `key`, or to the last node of the level.
    fn seek(&mut self, w: &mut Work, key: &[u8]) -> Result<()> {
        let mut d = self.path.len() - 1;
        while d > 0 {
            let f = &self.path[d];
            let ends_before = f.children.last().is_none_or(|c| c.last_key.as_slice() < key);
            if !ends_before || self.right_edge(d) {
                break;
            }
            d -= 1;
        }
        self.path.truncate(d + 1);
        let f = &mut self.path[d];
        let p = f.children.partition_point(|c| c.last_key.as_slice() < key).min(f.children.len() - 1);
        f.at = f.at.max(p);
        self.descend(w, Some(key))
    }

    /// Moves to the next node of the level; false (and no move) at the end.
    fn advance(&mut self, w: &mut Work) -> Result<bool> {
        let Some(d) = (0..self.path.len()).rev().find(|&d| self.path[d].at + 1 < self.path[d].children.len()) else {
            return Ok(false);
        };
        self.path.truncate(d + 1);
        self.path[d].at += 1;
        self.descend(w, None)?;
        Ok(true)
    }

    /// The parent entry of the current node.
    fn current(&self) -> &Child {
        let f = self.deepest();
        &f.children[f.at]
    }

    /// The bound that the first key of the current node must be above.
    fn current_lo(&self) -> Option<&[u8]> {
        let f = self.deepest();
        f.lo_of(f.at)
    }

    fn is_last(&self) -> bool {
        self.right_edge(self.path.len())
    }

    fn held(&self) -> usize {
        self.path.iter().map(|f| f.bytes).sum()
    }
}

/// The entries of a node as (key, payload).
fn entries(n: Arc<Node>) -> Vec<(Vec<u8>, Payload)> {
    match Arc::try_unwrap(n).unwrap_or_else(|a| (*a).clone()) {
        Node::Leaf(es) => es.into_iter().map(|(k, v)| (k, Payload::Value(v))).collect(),
        Node::Internal { children, .. } => {
            children.into_iter().map(|c| (c.last_key, Payload::Child { id: c.id, count: c.count })).collect()
        }
    }
}

fn push(w: &mut Work, lw: &mut LevelWriter, key: Vec<u8>, p: Payload) -> Result<Option<NodeRef>> {
    match p {
        Payload::Value(v) => lw.push_leaf(w.ctx.store, &w.cfg, &key, &v, &mut w.writes),
        Payload::Child { id, count } => lw.push_child(w.ctx.store, &w.cfg, Child { last_key: key, id, count }, &mut w.writes),
    }
}

/// What one level pass gives: the new nodes of the touched runs and the
/// old nodes they replace, both in key order.
struct Rewrite {
    new: Vec<NodeRef>,
    old: Vec<Child>,
}

/// Rewrites the nodes of `level` that `ops` touch, up to each resync point.
fn rewrite_level(w: &mut Work, root: &Root, level: u8, ops: &[Op]) -> Result<Rewrite> {
    let top = level == root.node.level();
    let pending = op_bytes(ops) + root.bytes;
    let mut cur = LevelCursor::new(w, root, level)?;
    let mut out = Rewrite { new: Vec::new(), old: Vec::new() };
    let mut out_bytes = 0usize;
    // The subtree counts pushed in this pass. In a valid tree they sum to at
    // most the entries of the new tree; a crafted count that passes u64 is
    // an error here, before any writer adds it.
    let mut counts = 0u64;
    let mut i = 0;
    while i < ops.len() {
        cur.seek(w, &ops[i].0)?;
        let mut lw = LevelWriter::new(level);
        loop {
            let c = cur.current().clone();
            let (n, nb) = if top { (root.node.clone(), root.bytes) } else { w.load_child(level, &c, cur.current_lo())? };
            let j = if cur.is_last() {
                ops.len()
            } else {
                i + ops[i..].partition_point(|(k, _)| k.as_slice() <= c.last_key.as_slice())
            };
            let held = cur.held() + nb + pending;
            w.note(held + lw.buffered_bytes() + out_bytes);
            let mut old = entries(n).into_iter().peekable();
            let mut ed = ops[i..j].iter().peekable();
            loop {
                let take_old = match (old.peek(), ed.peek()) {
                    (None, None) => break,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (Some((k, _)), Some((ek, _))) => match k.cmp(ek) {
                        Ordering::Less => true,
                        Ordering::Equal => {
                            old.next();
                            false
                        }
                        Ordering::Greater => false,
                    },
                };
                let (k, p) = if take_old {
                    match old.next() {
                        Some(e) => e,
                        None => break,
                    }
                } else {
                    match ed.next() {
                        Some((k, Some(p))) => (k.clone(), p.clone()),
                        Some((_, None)) => continue,
                        None => break,
                    }
                };
                if let Payload::Child { count, .. } = &p {
                    counts = counts
                        .checked_add(*count)
                        .ok_or_else(|| Error::Corrupt("prolly node: subtree count above u64".into()))?;
                }
                if let Some(r) = push(w, &mut lw, k, p)? {
                    out_bytes += r.last_key.len() + ID_LEN + 8;
                    out.new.push(r);
                }
                w.note(held + lw.buffered_bytes() + out_bytes);
            }
            i = j;
            out_bytes += c.last_key.len() + ID_LEN + 8;
            out.old.push(c);
            if !cur.advance(w)? {
                if let Some(r) = lw.flush(w.ctx.store, &mut w.writes)? {
                    out.new.push(r);
                }
                break;
            }
            let next_touched = i < ops.len() && (cur.is_last() || ops[i].0 <= cur.current().last_key);
            if lw.is_empty() && !next_touched {
                break;
            }
        }
    }
    Ok(out)
}

/// The edits of the level above: a delete for each replaced node and a put
/// for each new node, in key order; a put equal to the old entry of its key
/// is dropped (the node did not change).
fn parent_ops(r: Rewrite) -> Vec<Op> {
    let mut ops = Vec::with_capacity(r.old.len() + r.new.len());
    let mut old = r.old.into_iter().peekable();
    let mut new = r.new.into_iter().peekable();
    loop {
        let ord = match (old.peek(), new.peek()) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(o), Some(n)) => o.last_key.cmp(&n.last_key),
        };
        match ord {
            Ordering::Less => {
                if let Some(o) = old.next() {
                    ops.push((o.last_key, None));
                }
            }
            Ordering::Greater | Ordering::Equal => {
                let o = if ord == Ordering::Equal { old.next() } else { None };
                if let Some(n) = new.next() {
                    if o.is_some_and(|o| o.id == n.id && o.count == n.count) {
                        continue;
                    }
                    ops.push((n.last_key, Some(Payload::Child { id: n.id, count: n.count })));
                }
            }
        }
    }
    ops
}

fn check(cfg: &Config, batch: &[Edit]) -> Result<()> {
    if batch.windows(2).any(|p| p[0].key() >= p[1].key()) {
        return Err(Error::Unsorted);
    }
    for e in batch {
        let k = e.key().len();
        if k > cfg.max_key() {
            return Err(Error::KeyTooLarge { len: k, max: cfg.max_key() });
        }
        if let Edit::Put { value, .. } = e {
            let len = leaf_entry_len(k, value.len());
            if len > cfg.max_entry() {
                return Err(Error::EntryTooLarge { len, max: cfg.max_entry() });
            }
        }
    }
    Ok(())
}

/// Applies one batch of edits, sorted by strictly ascending key, to the
/// tree `root`; returns the new root. Only the touched nodes, the nodes up
/// to the next resync point and their paths are written; every other node
/// keeps its ID. The new root equals the `build` root of the final set.
pub fn apply(store: &ChunkStore, cfg: Config, root: &ObjectId, batch: &[Edit]) -> Result<(ObjectId, ApplyStats)> {
    apply_in(&Ctx::uncached(store), cfg, root, batch)
}

/// `apply` with the old nodes read through a `Ctx` (they may come from its
/// cache). A cached node is checked against `cfg` like a node read from
/// the store. New nodes are written to the store, not to the cache.
pub fn apply_in(ctx: &Ctx, cfg: Config, root: &ObjectId, batch: &[Edit]) -> Result<(ObjectId, ApplyStats)> {
    let store = ctx.store;
    check(&cfg, batch)?;
    if batch.is_empty() {
        return Ok((*root, ApplyStats::default()));
    }
    let mut w = Work { ctx: *ctx, cfg, writes: BuildStats::default(), reads: 0, hits: 0, high: 0 };
    let (node, bytes) = w.load(root, None)?;
    let top = node.level();
    let old_root = Root { id: *root, node, bytes };
    let mut ops: Vec<Op> = batch
        .iter()
        .map(|e| match e {
            Edit::Put { key, value } => (key.clone(), Some(Payload::Value(value.clone()))),
            Edit::Delete { key } => (key.clone(), None),
        })
        .collect();
    let mut level = 0u8;
    let new = loop {
        let r = rewrite_level(&mut w, &old_root, level, &ops)?;
        if level == top {
            break r.new;
        }
        ops = parent_ops(r);
        if ops.is_empty() {
            return Ok((*root, w.stats()));
        }
        level += 1;
    };
    drop(old_root);
    let mut new = new.into_iter();
    let root = match (new.next(), new.len()) {
        (None, _) => {
            w.writes.nodes_written += 1;
            w.writes.bytes_written += crate::NODE_HEADER as u64;
            empty_root(store)?
        }
        (Some(r), 0) => collapse(&mut w, r.id, top)?,
        (Some(r), _) => {
            let above = top.checked_add(1).ok_or_else(|| Error::Corrupt("prolly: tree too deep".into()))?;
            let mut b = Builder::with_level(store, cfg, above);
            b.push_node(r)?;
            for r in new {
                b.push_node(r)?;
            }
            let (id, st) = b.finish()?;
            w.writes.nodes_written += st.nodes_written;
            w.writes.bytes_written += st.bytes_written;
            w.writes.forced_cuts += st.forced_cuts;
            w.note(st.high_water_bytes);
            id
        }
    };
    Ok((root, w.stats()))
}

/// The canonical root under the single node `id` of level `level`: while
/// it is an internal node with one child, that child.
fn collapse(w: &mut Work, mut id: ObjectId, mut level: u8) -> Result<ObjectId> {
    // The first node is new (this apply wrote it); each later one is the
    // only child of the node before it, checked against that entry.
    let mut entry: Option<Child> = None;
    while level > 0 {
        let (n, bytes) = match &entry {
            None => w.load(&id, Some(level))?,
            Some(c) => w.load_child(level, c, None)?,
        };
        w.note(bytes);
        let children = children_of(n)?;
        let [only] = children.as_slice() else { break };
        id = only.id;
        level -= 1;
        entry = Some(only.clone());
    }
    Ok(id)
}
