//! The layout engine of the Q-2 test, used by `synth`: it builds prolly
//! trees of one layout from sorted native entries and applies commits to
//! them. The modes that read the flat maps of a product store (`maps`,
//! `layout`) are retired; tag `v2.3.0` keeps them.
//!
//! Three layouts (D-02, D-04):
//! - a: five trees, one per map (inode, dir, manifest, xattr, refcounts);
//!   the prolly key is the native key of the map.
//! - b: one tree `ns`. Key of a per-inode record: `0x01 || ino (8 BE) ||
//!   kind (1)`, kind 0 inode, 1 dir, 2 manifest, 3 xattr (10 bytes), so the
//!   records of one file share a leaf. Key of a refcount record: `0x02 ||
//!   content key (33)`, which sorts after every `0x01` key.
//! - c: tree `ns` for the four per-inode maps as in b, and a separate tree
//!   `refcounts` as in a (refcounts have another key space and lifetime).
//!
//! The key grammar has room for a later name suffix (`0x01 || ino || kind ||
//! name`, at most 8 + 255 + 2 bytes, under `MAX_KEY` 1,024), but a
//! directory stays one value per directory here: by-name entries are a
//! Phase 26 decision, and the bench uses the value shape of today's maps.
//!
//! Output (one line per record, `key=value`): `commit`, `tree` and `check`
//! lines, each with `mode=layout source n layout l node_max`. Counts and
//! sizes only.
//!
//! Memory: the base maps are streamed (one stream per tree in a build, a
//! four-way merge for `ns`); only the edits of one commit are held in a
//! `Vec`.

use chunk_store::{ChunkStore, ObjectId};
use prolly::{ApplyStats, BuildStats, Config, Edit};

use crate::replay::measure;
use crate::{run_fail, stats_delta, usage, usage_fields, Args, Fail, Res};

/// One entry of a prolly tree or of a native map stream.
pub(crate) type Kv = (Vec<u8>, Vec<u8>);

/// A stream of sorted native entries.
pub(crate) type KvStream<'s> = Box<dyn Iterator<Item = Res<Kv>> + 's>;

/// How the five maps are put into trees.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Layout {
    /// Five trees, one per map.
    A,
    /// One combined tree, refcounts inside.
    B,
    /// One combined tree for the per-inode maps, a tree for refcounts.
    C,
}

impl Layout {
    pub(crate) fn parse(s: &str) -> Option<Layout> {
        match s {
            "a" => Some(Layout::A),
            "b" => Some(Layout::B),
            "c" => Some(Layout::C),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Layout::A => "a",
            Layout::B => "b",
            Layout::C => "c",
        }
    }
}

/// One of the five metadata maps, in the order of the root record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MapKind {
    Inode,
    Dir,
    Manifest,
    Xattr,
    Refcounts,
}

impl MapKind {
    pub(crate) const ALL: [MapKind; 5] =
        [MapKind::Inode, MapKind::Dir, MapKind::Manifest, MapKind::Xattr, MapKind::Refcounts];

    pub(crate) fn name(self) -> &'static str {
        match self {
            MapKind::Inode => "inode",
            MapKind::Dir => "dir",
            MapKind::Manifest => "manifest",
            MapKind::Xattr => "xattr",
            MapKind::Refcounts => "refcounts",
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    /// The kind byte of a per-inode record in the combined key.
    fn kind_byte(self) -> u8 {
        self as u8
    }

    /// True for the four maps keyed by inode number.
    fn per_inode(self) -> bool {
        self != MapKind::Refcounts
    }
}

/// One edit of one map. Native key: the ino as 8 bytes big endian for the
/// four per-inode maps, the 33-byte content key for refcounts. Values: the
/// 33-byte key, or the 8-byte count LE for refcounts. `None` deletes.
pub(crate) struct MapEdit {
    pub(crate) map: MapKind,
    pub(crate) key: Vec<u8>,
    pub(crate) value: Option<Vec<u8>>,
}

/// The fields of every output line.
pub(crate) struct Ctx {
    pub(crate) source: &'static str,
    pub(crate) n: u64,
    pub(crate) layout: Layout,
    pub(crate) cfg: Config,
}

impl Ctx {
    /// `source=.. n=.. layout=.. l=.. node_max=..`
    pub(crate) fn fields(&self) -> String {
        format!(
            "source={} n={} layout={} l={} node_max={}",
            self.source,
            self.n,
            self.layout.name(),
            self.cfg.weibull_l(),
            self.cfg.node_max()
        )
    }
}

/// First byte of a combined key of a per-inode record.
const NS_INODE: u8 = 0x01;
/// First byte of a combined key of a refcount record.
const NS_REFCOUNT: u8 = 0x02;

/// The combined key of a native key: `0x01 || ino || kind` for the
/// per-inode maps, `0x02 || content key` for refcounts.
fn ns_key(kind: MapKind, native: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(native.len() + 2);
    if kind.per_inode() {
        k.push(NS_INODE);
        k.extend_from_slice(native);
        k.push(kind.kind_byte());
    } else {
        k.push(NS_REFCOUNT);
        k.extend_from_slice(native);
    }
    k
}

/// The native key of a combined key, if it belongs to `map`.
#[cfg(test)]
fn ns_native(map: MapKind, k: &[u8]) -> Option<Vec<u8>> {
    if map.per_inode() {
        (k.len() == 10 && k[0] == NS_INODE && k[9] == map.kind_byte()).then(|| k[1..9].to_vec())
    } else {
        (k.first() == Some(&NS_REFCOUNT)).then(|| k[1..].to_vec())
    }
}

// ── tree shape ───────────────────────────────────────────────────────────────

/// The shape of one tree, from one `walk`.
struct Shape {
    entries: u64,
    depth: u8,
    /// Nodes per level, from the leaves up.
    levels: Vec<u64>,
    leaf_bytes: u64,
    internal_bytes: u64,
    internal_entries: u64,
}

fn shape_of(cs: &ChunkStore, root: &ObjectId) -> Res<Shape> {
    let mut s = Shape { entries: 0, depth: 0, levels: Vec::new(), leaf_bytes: 0, internal_bytes: 0, internal_entries: 0 };
    prolly::walk(cs, root, |n| {
        let level = usize::from(n.level);
        if s.levels.len() <= level {
            s.levels.resize(level + 1, 0);
        }
        s.levels[level] += 1;
        s.depth = s.depth.max(n.level + 1);
        if n.level == 0 {
            s.entries += n.entries as u64;
            s.leaf_bytes += n.bytes as u64;
        } else {
            s.internal_bytes += n.bytes as u64;
            s.internal_entries += n.entries as u64;
        }
    })?;
    Ok(s)
}

/// The combined stream of `ns`: a four-way merge of the per-inode streams by
/// (ino, kind), then the refcount stream when refcounts are inside. Holds
/// one head per stream.
struct NsMerge<'s> {
    per_inode: Vec<KvStream<'s>>,
    refcounts: Option<KvStream<'s>>,
    heads: Vec<Option<Kv>>,
    primed: bool,
}

impl Iterator for NsMerge<'_> {
    type Item = Res<Kv>;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.primed {
            self.primed = true;
            for (src, head) in self.per_inode.iter_mut().zip(&mut self.heads) {
                match src.next() {
                    Some(Ok(kv)) => *head = Some(kv),
                    Some(Err(e)) => return Some(Err(e)),
                    None => {}
                }
            }
        }
        // The smallest head; a tie (the same ino in two maps) goes to the
        // lower kind, because only a strictly smaller key replaces `best`.
        let mut best: Option<usize> = None;
        for (i, h) in self.heads.iter().enumerate() {
            if let Some((k, _)) = h {
                if best.is_none_or(|b| self.heads[b].as_ref().is_some_and(|(bk, _)| k < bk)) {
                    best = Some(i);
                }
            }
        }
        if let Some(i) = best {
            let (k, v) = self.heads[i].take()?;
            match self.per_inode[i].next() {
                Some(Ok(kv)) => self.heads[i] = Some(kv),
                Some(Err(e)) => return Some(Err(e)),
                None => {}
            }
            return Some(Ok((ns_key(MapKind::ALL[i], &k), v)));
        }
        let (k, v) = match self.refcounts.as_mut()?.next()? {
            Ok(kv) => kv,
            Err(e) => return Some(Err(e)),
        };
        Some(Ok((ns_key(MapKind::Refcounts, &k), v)))
    }
}

// ── engine ───────────────────────────────────────────────────────────────────

/// One prolly tree of a layout.
struct Tree {
    name: &'static str,
    root: ObjectId,
    /// Forced cuts of the build (phase base).
    built_forced: u64,
    /// Forced cuts summed over the applies (phase final).
    applied_forced: u64,
}

/// The trees of a layout, the base build, and the commit replay.
pub(crate) struct Engine<'s> {
    cs: &'s ChunkStore,
    ctx: Ctx,
    trees: Vec<Tree>,
}

/// Bulk build of one tree from a sorted stream.
fn build_tree(cs: &ChunkStore, cfg: Config, it: impl Iterator<Item = Res<Kv>>) -> Res<(ObjectId, BuildStats)> {
    let mut b = prolly::Builder::new(cs, cfg);
    for r in it {
        let (k, v) = r?;
        b.push(&k, &v)?;
    }
    Ok(b.finish()?)
}

impl<'s> Engine<'s> {
    /// Builds the trees from five sorted native streams (in `MapKind`
    /// order) and prints the `tree` lines of phase base.
    pub(crate) fn build(cs: &'s ChunkStore, ctx: Ctx, base: [KvStream<'s>; 5]) -> Res<Engine<'s>> {
        let cfg = ctx.cfg;
        let layout = ctx.layout;
        let (trees, m) = measure(cs, || {
            let mut trees = Vec::new();
            match layout {
                Layout::A => {
                    for (kind, src) in MapKind::ALL.into_iter().zip(base) {
                        let (root, st) = build_tree(cs, cfg, src)?;
                        trees.push((Tree { name: kind.name(), root, built_forced: st.forced_cuts, applied_forced: 0 }, st));
                    }
                }
                Layout::B | Layout::C => {
                    let mut per_inode: Vec<KvStream<'s>> = base.into_iter().collect();
                    let refs = per_inode.pop().expect("five streams");
                    let (inside, apart) = if layout == Layout::B { (Some(refs), None) } else { (None, Some(refs)) };
                    let ns = NsMerge { per_inode, refcounts: inside, heads: vec![None; 4], primed: false };
                    let (root, st) = build_tree(cs, cfg, ns)?;
                    trees.push((Tree { name: "ns", root, built_forced: st.forced_cuts, applied_forced: 0 }, st));
                    if let Some(refs) = apart {
                        let (root, st) = build_tree(cs, cfg, refs)?;
                        trees.push((Tree { name: "refcounts", root, built_forced: st.forced_cuts, applied_forced: 0 }, st));
                    }
                }
            }
            cs.sync()?;
            Ok(trees)
        })?;
        let (mut nodes, mut bytes, mut forced) = (0u64, 0u64, 0u64);
        let trees: Vec<Tree> = trees
            .into_iter()
            .map(|(t, st)| {
                nodes += st.nodes_written;
                bytes += st.bytes_written;
                forced += st.forced_cuts;
                t
            })
            .collect();
        // The time of a build includes the time to make its input streams.
        println!(
            "build mode=layout {} ms={:.3} {} nodes_written={nodes} bytes_written={bytes} forced_cuts={forced}",
            ctx.fields(),
            m.ms,
            usage_fields(&m.u0, &m.u1)
        );
        let e = Engine { cs, ctx, trees };
        e.print_trees("base")?;
        Ok(e)
    }

    /// The tree and the prolly key of a native key.
    fn route(&self, map: MapKind, key: &[u8]) -> (usize, Vec<u8>) {
        match self.ctx.layout {
            Layout::A => (map.index(), key.to_vec()),
            Layout::B => (0, ns_key(map, key)),
            Layout::C if map.per_inode() => (0, ns_key(map, key)),
            Layout::C => (1, key.to_vec()),
        }
    }

    /// Replays one commit: one `apply` per tree with edits, one sync, one
    /// `commit` line.
    pub(crate) fn commit(&mut self, pattern: &str, k: u64, seq: u64, edits: Vec<MapEdit>) -> Res<()> {
        let n_edits = edits.len();
        let mut groups: Vec<Vec<Edit>> = vec![Vec::new(); self.trees.len()];
        for e in edits {
            let (ti, key) = self.route(e.map, &e.key);
            groups[ti].push(match e.value {
                Some(value) => Edit::Put { key, value },
                None => Edit::Delete { key },
            });
        }
        for g in &mut groups {
            g.sort_by(|a, b| a.key().cmp(b.key()));
        }
        let (cs, cfg) = (self.cs, self.ctx.cfg);
        let trees = &mut self.trees;
        let (st, m) = measure(cs, || {
            let mut sum = ApplyStats::default();
            for (t, g) in trees.iter_mut().zip(&groups) {
                if g.is_empty() {
                    continue;
                }
                let (root, st) = prolly::apply(cs, cfg, &t.root, g)?;
                t.root = root;
                t.applied_forced += st.forced_cuts;
                sum.nodes_read += st.nodes_read;
                sum.nodes_written += st.nodes_written;
                sum.bytes_written += st.bytes_written;
                sum.forced_cuts += st.forced_cuts;
            }
            cs.sync()?;
            Ok(sum)
        })?;
        println!(
            "commit mode=layout {} pattern={pattern} k={k} seq={seq} edits={n_edits} ms={:.3} {} {} nodes_read={} nodes_written={} bytes_written={} forced_cuts={}",
            self.ctx.fields(),
            m.ms,
            usage_fields(&m.u0, &m.u1),
            stats_delta(&m.s0, &m.s1),
            st.nodes_read,
            st.nodes_written,
            st.bytes_written,
            st.forced_cuts
        );
        Ok(())
    }

    /// Prints one `tree` line per tree; `phase` is `base` or `final`.
    pub(crate) fn print_trees(&self, phase: &str) -> Res<()> {
        for t in &self.trees {
            let s = shape_of(self.cs, &t.root)?;
            let levels = s.levels.iter().map(u64::to_string).collect::<Vec<_>>().join("/");
            let forced = if phase == "base" { t.built_forced } else { t.applied_forced };
            println!(
                "tree mode=layout {} phase={phase} map={} entries={} depth={} levels={levels} leaf_bytes={} internal_bytes={} internal_entries={} forced_cuts={forced}",
                self.ctx.fields(),
                t.name,
                s.entries,
                s.depth,
                s.leaf_bytes,
                s.internal_bytes,
                s.internal_entries
            );
        }
        Ok(())
    }

    /// The entries of one map in order, with native keys.
    #[cfg(test)]
    pub(crate) fn iter_map(&self, map: MapKind) -> Res<KvStream<'_>> {
        match self.ctx.layout {
            Layout::A => {
                let it = prolly::iter(self.cs, &self.trees[map.index()].root)?;
                Ok(Box::new(it.map(|r| r.map_err(Fail::from))))
            }
            Layout::C if !map.per_inode() => {
                let it = prolly::iter(self.cs, &self.trees[1].root)?;
                Ok(Box::new(it.map(|r| r.map_err(Fail::from))))
            }
            Layout::B | Layout::C => {
                let it = prolly::iter(self.cs, &self.trees[0].root)?;
                Ok(Box::new(it.filter_map(move |r| match r {
                    Err(e) => Some(Err(Fail::from(e))),
                    Ok((k, v)) => ns_native(map, &k).map(|key| Ok((key, v))),
                })))
            }
        }
    }
}

impl Engine<'_> {
    /// The value of a native key of one map, reading one path of nodes.
    pub(crate) fn get(&self, map: MapKind, key: &[u8]) -> Res<Option<Vec<u8>>> {
        let (ti, k) = self.route(map, key);
        Ok(prolly::get(self.cs, &self.trees[ti].root, &k)?)
    }

    /// The entry count of each map, in `MapKind` order, in one pass over the
    /// combined tree (layouts b and c) or from the leaves of each tree (a).
    pub(crate) fn map_counts(&self) -> Res<[u64; 5]> {
        let mut c = [0u64; 5];
        match self.ctx.layout {
            Layout::A => {
                for (i, t) in self.trees.iter().enumerate() {
                    c[i] = shape_of(self.cs, &t.root)?.entries;
                }
            }
            Layout::B | Layout::C => {
                for r in prolly::iter(self.cs, &self.trees[0].root)? {
                    let (k, _) = r?;
                    match (k.first(), k.len(), k.last()) {
                        (Some(&NS_REFCOUNT), _, _) => c[MapKind::Refcounts.index()] += 1,
                        (Some(&NS_INODE), 10, Some(&kind)) if kind < 4 => c[usize::from(kind)] += 1,
                        _ => return run_fail("a key of the combined tree is not of the grammar"),
                    }
                }
                if self.ctx.layout == Layout::C {
                    c[MapKind::Refcounts.index()] = shape_of(self.cs, &self.trees[1].root)?.entries;
                }
            }
        }
        Ok(c)
    }

    /// Prints the `check` line.
    pub(crate) fn print_check(&self, ok: bool) {
        println!("check mode=layout {} ok={}", self.ctx.fields(), u8::from(ok));
    }
}

/// The `--layout`, `--l` and `--node-max` options (shared with `synth`).
pub(crate) fn layout_opts(a: &Args) -> Res<(Layout, Config)> {
    let which = a.str("layout")?;
    let Some(layout) = Layout::parse(which) else { return usage(format!("--layout: want a, b or c, got {which}")) };
    let (l, node_max) = (a.num("l", Some(8192))?, a.num("node-max", Some(16384))?);
    let cfg = usize::try_from(l)
        .ok()
        .zip(usize::try_from(node_max).ok())
        .and_then(|(l, m)| Config::new(l, m))
        .ok_or_else(|| Fail::Usage(format!("--l {l} --node-max {node_max}: want l in 256..=16384 and node-max in 4096..=32768")))?;
    Ok((layout, cfg))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chunk_store::Options;

    use super::*;
    use crate::KEY_LEN;

    fn store() -> (tempfile::TempDir, ChunkStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let cs = ChunkStore::open(dir.path(), Options::default()).expect("open");
        (dir, cs)
    }

    /// A refcount key: 33 bytes with the number `tag`.
    fn rc_key(tag: u16) -> Vec<u8> {
        let mut key = vec![0u8; KEY_LEN];
        key[0] = 0x1e;
        key[1..3].copy_from_slice(&tag.to_be_bytes());
        key
    }

    /// The native base of each map: ino 2.. for the per-inode maps, content
    /// keys for refcounts.
    fn base_entries(kind: MapKind, n: u64) -> Vec<Kv> {
        if kind.per_inode() {
            (2..n + 2).map(|ino| (ino.to_be_bytes().to_vec(), vec![kind.index() as u8; KEY_LEN])).collect()
        } else {
            (0..n).map(|i| (rc_key(i as u16), 1u64.to_le_bytes().to_vec())).collect()
        }
    }

    /// Replays two commits in every layout and compares each map with a
    /// model of the final state.
    fn replay_matches_model(layout: Layout) {
        let (_dir, cs) = store();
        let n = 300u64;
        let mut model: Vec<BTreeMap<Vec<u8>, Vec<u8>>> =
            MapKind::ALL.iter().map(|k| base_entries(*k, n).into_iter().collect()).collect();
        let base: [KvStream<'_>; 5] = MapKind::ALL.map(|k| -> KvStream<'_> { Box::new(base_entries(k, n).into_iter().map(Ok)) });
        let ctx = Ctx { source: "boost", n, layout, cfg: Config::SMALL };
        let mut e = Engine::build(&cs, ctx, base).expect("build");
        // Commit 1: replace, insert and delete in every map. Commit 2:
        // delete again, and put a key that commit 1 deleted.
        let mut commits: Vec<Vec<MapEdit>> = Vec::new();
        for round in 0..2u8 {
            let mut edits = Vec::new();
            for kind in MapKind::ALL {
                let keys: Vec<Vec<u8>> = model[kind.index()].keys().cloned().collect();
                let fresh = if kind.per_inode() {
                    (1_000 + u64::from(round)).to_be_bytes().to_vec()
                } else {
                    rc_key(1_000 + u16::from(round))
                };
                let val = |b: u8| if kind.per_inode() { vec![b; KEY_LEN] } else { u64::from(b).to_le_bytes().to_vec() };
                edits.push(MapEdit { map: kind, key: keys[7].clone(), value: Some(val(9)) });
                edits.push(MapEdit { map: kind, key: fresh, value: Some(val(8)) });
                edits.push(MapEdit { map: kind, key: keys[40 + usize::from(round)].clone(), value: None });
            }
            commits.push(edits);
        }
        for (i, edits) in commits.into_iter().enumerate() {
            for ed in &edits {
                let m = &mut model[ed.map.index()];
                match &ed.value {
                    Some(v) => m.insert(ed.key.clone(), v.clone()),
                    None => m.remove(&ed.key),
                };
            }
            e.commit("test", 3, i as u64 + 1, edits).expect("commit");
        }
        e.print_trees("final").expect("trees");
        for kind in MapKind::ALL {
            let got: Vec<Kv> = e.iter_map(kind).expect("iter").collect::<Res<_>>().expect("read");
            let want: Vec<Kv> = model[kind.index()].clone().into_iter().collect();
            assert_eq!(got, want, "{} in layout {}", kind.name(), layout.name());
        }
    }

    #[test]
    fn every_layout_replay_matches_the_model() {
        for layout in [Layout::A, Layout::B, Layout::C] {
            replay_matches_model(layout);
        }
    }

    #[test]
    fn a_combined_key_sorts_by_ino_then_kind_and_refcounts_last() {
        let k = |kind, ino: u64| ns_key(kind, &ino.to_be_bytes());
        assert!(k(MapKind::Inode, 2) < k(MapKind::Dir, 2));
        assert!(k(MapKind::Xattr, 2) < k(MapKind::Inode, 3));
        assert!(k(MapKind::Xattr, u64::MAX) < ns_key(MapKind::Refcounts, &[0u8; KEY_LEN]));
        assert_eq!(ns_key(MapKind::Manifest, &7u64.to_be_bytes()).len(), 10);
        assert_eq!(ns_key(MapKind::Refcounts, &[0u8; KEY_LEN]).len(), 34);
        let key = k(MapKind::Manifest, 7);
        assert_eq!(ns_native(MapKind::Manifest, &key), Some(7u64.to_be_bytes().to_vec()));
        assert_eq!(ns_native(MapKind::Dir, &key), None);
    }
}
