//! The layout engine of the Q-2 test: `maps` extracts the change of all
//! five metadata maps of each real commit; `layout` replays the commits
//! into prolly trees.
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
//! Output (one line per record, `key=value`): `maps` and `final` lines of
//! `maps`; `commit`, `tree` and `check` lines of `layout`, each with
//! `mode=layout source n layout l node_max`. Counts and sizes only.
//!
//! Memory: the base maps are streamed (two map streams at a time in
//! `maps`, one stream per tree in a build, a four-way merge for `ns`);
//! only the edits of one commit are held in a `Vec`.

use std::cmp::Ordering;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;

use chunk_store::{ChunkStore, ObjectId, Options};
use prolly::{ApplyStats, BuildStats, Config, Edit};

use crate::replay::{batch_path, field, measure, num_field, read_roots, Digest, MapStream, BATCH_REC};
use crate::{root_record, run_fail, stats_delta, usage, usage_fields, Args, Fail, Res, KEY_LEN, MAP_ENTRY};

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
fn ns_native(map: MapKind, k: &[u8]) -> Option<Vec<u8>> {
    if map.per_inode() {
        (k.len() == 10 && k[0] == NS_INODE && k[9] == map.kind_byte()).then(|| k[1..9].to_vec())
    } else {
        (k.first() == Some(&NS_REFCOUNT)).then(|| k[1..].to_vec())
    }
}

// ── raw records ──────────────────────────────────────────────────────────────

/// The native entry of a raw 41-byte record of a flat map: per-inode maps
/// hold `(ino LE, key)`, refcounts `(key, count LE)`.
fn native_of(kind: MapKind, rec: &[u8; MAP_ENTRY]) -> Kv {
    if kind.per_inode() {
        let ino = u64::from_le_bytes(rec[..8].try_into().expect("8 bytes"));
        (ino.to_be_bytes().to_vec(), rec[8..].to_vec())
    } else {
        (rec[..KEY_LEN].to_vec(), rec[KEY_LEN..].to_vec())
    }
}

/// The raw record of a native entry (the inverse of `native_of`).
fn raw_of(kind: MapKind, key: &[u8], value: &[u8]) -> Res<[u8; MAP_ENTRY]> {
    let mut rec = [0u8; MAP_ENTRY];
    if kind.per_inode() {
        let ino = u64::from_be_bytes(key.try_into().map_err(|_| Fail::Run("per-inode key is not 8 bytes".into()))?);
        if value.len() != KEY_LEN {
            return run_fail("per-inode value is not 33 bytes");
        }
        rec[..8].copy_from_slice(&ino.to_le_bytes());
        rec[8..].copy_from_slice(value);
    } else {
        if key.len() != KEY_LEN || value.len() != 8 {
            return run_fail("refcount entry is not 33 + 8 bytes");
        }
        rec[..KEY_LEN].copy_from_slice(key);
        rec[KEY_LEN..].copy_from_slice(value);
    }
    Ok(rec)
}

/// The map order of two raw records: by ino (a number) for the per-inode
/// maps, by the 33-byte key for refcounts.
fn rec_cmp(kind: MapKind, a: &[u8; MAP_ENTRY], b: &[u8; MAP_ENTRY]) -> Ordering {
    if kind.per_inode() {
        u64::from_le_bytes(a[..8].try_into().expect("8 bytes")).cmp(&u64::from_le_bytes(b[..8].try_into().expect("8 bytes")))
    } else {
        a[..KEY_LEN].cmp(&b[..KEY_LEN])
    }
}

// ── maps ─────────────────────────────────────────────────────────────────────

/// Writes the diff of two consecutive maps to `path` as records
/// `op (0 delete, 1 put) ‖ raw record`; returns (puts, dels). Memory: two
/// map streams.
fn write_batch(
    cs: &ChunkStore,
    kind: MapKind,
    old_key: &slicefs_traits::key::ObjectKey,
    new_key: &slicefs_traits::key::ObjectKey,
    path: &Path,
) -> Res<(u64, u64)> {
    let mut old = MapStream::open(cs, old_key)?;
    let mut new = MapStream::open(cs, new_key)?;
    let mut f = BufWriter::new(fs::File::create(path)?);
    let (mut puts, mut dels) = (0u64, 0u64);
    let rec = |f: &mut BufWriter<fs::File>, op: u8, r: &[u8; MAP_ENTRY]| -> Res<()> {
        f.write_all(&[op])?;
        f.write_all(r)?;
        Ok(())
    };
    let (mut o, mut n) = (old.next_raw()?, new.next_raw()?);
    loop {
        match (o, n) {
            (None, None) => break,
            (Some(a), Some(b)) => match rec_cmp(kind, &a, &b) {
                Ordering::Equal => {
                    if a != b {
                        rec(&mut f, 1, &b)?;
                        puts += 1;
                    }
                    o = old.next_raw()?;
                    n = new.next_raw()?;
                }
                Ordering::Less => {
                    rec(&mut f, 0, &a)?;
                    dels += 1;
                    o = old.next_raw()?;
                }
                Ordering::Greater => {
                    rec(&mut f, 1, &b)?;
                    puts += 1;
                    n = new.next_raw()?;
                }
            },
            (Some(a), None) => {
                rec(&mut f, 0, &a)?;
                dels += 1;
                o = old.next_raw()?;
            }
            (None, Some(b)) => {
                rec(&mut f, 1, &b)?;
                puts += 1;
                n = new.next_raw()?;
            }
        }
    }
    f.flush()?;
    Ok((puts, dels))
}

/// Writes, per map, `DIR/<map>/base.bin` (the raw records of the seq-0
/// map), one `batch-<seq>.bin` per later commit, and `DIR/plan.txt`.
/// Memory: two map streams at a time, never a whole map (D-01).
pub(crate) fn maps(argv: &[String]) -> Res<()> {
    let a = Args::parse(argv)?;
    a.check(&["store", "roots", "out"])?;
    let store = a.path("store")?;
    let roots = read_roots(&a.path("roots")?)?;
    let out = a.path("out")?;
    fs::create_dir_all(&out)?;
    // Read only: the store of whole was measured; this step must not write it.
    let cs = ChunkStore::open(&store, Options { read_only: true, ..Options::default() })?;
    let recs = roots.iter().map(|r| root_record(&cs, &r.root)).collect::<Res<Vec<_>>>()?;

    // (commit index, map index, line): sorted by commit, then map.
    let mut lines: Vec<(usize, usize, String)> = Vec::new();
    let mut finals: Vec<String> = Vec::new();
    for kind in MapKind::ALL {
        let dir = out.join(kind.name());
        fs::create_dir_all(&dir)?;
        let map_of = |i: usize| recs[i].maps[kind.index()];

        let mut base = BufWriter::new(fs::File::create(dir.join("base.bin"))?);
        let mut s = MapStream::open(&cs, &map_of(0))?;
        while let Some(rec) = s.next_raw()? {
            base.write_all(&rec)?;
        }
        base.flush()?;

        for (i, r) in roots.iter().enumerate().skip(1) {
            let (puts, dels) = write_batch(&cs, kind, &map_of(i - 1), &map_of(i), &batch_path(&dir, r.seq))?;
            let line =
                format!("maps seq={} pattern={} k={} map={} puts={puts} dels={dels}", r.seq, r.pattern, r.k, kind.name());
            lines.push((i, kind.index(), line));
        }

        let mut d = Digest::new();
        let mut s = MapStream::open(&cs, &map_of(roots.len() - 1))?;
        while let Some(rec) = s.next_raw()? {
            d.add_raw(&rec);
        }
        finals.push(format!("final map={} entries={} digest={}", kind.name(), d.entries, d.hex()));
    }
    cs.close()?;

    lines.sort();
    let mut txt = BufWriter::new(fs::File::create(out.join("plan.txt"))?);
    for line in lines.iter().map(|l| &l.2).chain(&finals) {
        println!("{line}");
        writeln!(txt, "{line}")?;
    }
    txt.flush()?;
    Ok(())
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

// ── layout ───────────────────────────────────────────────────────────────────

/// A flat `base.bin` as a stream of native entries.
struct BaseFile {
    r: BufReader<fs::File>,
    kind: MapKind,
}

impl BaseFile {
    fn open(path: &Path, kind: MapKind) -> Res<(BaseFile, u64)> {
        let f = fs::File::open(path)?;
        let len = f.metadata()?.len();
        if len % MAP_ENTRY as u64 != 0 {
            return run_fail(format!("{} is not whole records", path.display()));
        }
        Ok((BaseFile { r: BufReader::new(f), kind }, len / MAP_ENTRY as u64))
    }
}

impl Iterator for BaseFile {
    type Item = Res<Kv>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut rec = [0u8; MAP_ENTRY];
        match self.r.read_exact(&mut rec) {
            Ok(()) => Some(Ok(native_of(self.kind, &rec))),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => None,
            Err(e) => Some(Err(e.into())),
        }
    }
}

/// The commits and the real final maps of `plan.txt`.
struct Plan {
    /// (seq, pattern, k) in commit order.
    commits: Vec<(u64, String, u64)>,
    /// (entries, digest) per map, in `MapKind` order.
    finals: [Option<(u64, String)>; 5],
}

fn read_plan(dir: &Path) -> Res<Plan> {
    let mut plan = Plan { commits: Vec::new(), finals: [None, None, None, None, None] };
    for line in BufReader::new(fs::File::open(dir.join("plan.txt"))?).lines() {
        let line = line?;
        match line.split_whitespace().next() {
            Some("maps") => {
                if field(&line, "map")? == MapKind::Inode.name() {
                    plan.commits.push((num_field(&line, "seq")?, field(&line, "pattern")?.to_string(), num_field(&line, "k")?));
                }
            }
            Some("final") => {
                let name = field(&line, "map")?;
                let Some(kind) = MapKind::ALL.into_iter().find(|k| k.name() == name) else {
                    return run_fail(format!("bad plan.txt line: {line}"));
                };
                plan.finals[kind.index()] = Some((num_field(&line, "entries")?, field(&line, "digest")?.to_string()));
            }
            _ => return run_fail(format!("bad plan.txt line: {line}")),
        }
    }
    if plan.finals.iter().any(Option::is_none) {
        return run_fail("plan.txt lacks a final line");
    }
    Ok(plan)
}

/// The edits of one map in one batch file.
fn read_batch(dir: &Path, kind: MapKind, seq: u64, out: &mut Vec<MapEdit>) -> Res<()> {
    let path = batch_path(&dir.join(kind.name()), seq);
    let len = fs::metadata(&path)?.len();
    if len % BATCH_REC as u64 != 0 {
        return run_fail(format!("batch {seq}: {len} bytes is not whole records"));
    }
    let mut r = BufReader::new(fs::File::open(&path)?);
    let mut rec = [0u8; BATCH_REC];
    for _ in 0..len / BATCH_REC as u64 {
        r.read_exact(&mut rec)?;
        let (key, value) = native_of(kind, rec[1..].try_into().expect("41 bytes"));
        let value = match rec[0] {
            1 => Some(value),
            0 => None,
            op => return run_fail(format!("batch {seq}: bad op {op}")),
        };
        out.push(MapEdit { map: kind, key, value });
    }
    Ok(())
}

/// The `--layout`, `--l` and `--node-max` options (shared with `synth`).
pub(crate) fn layout_opts(a: &Args) -> Res<(Layout, Config)> {
    let which = a.str("layout")?;
    let Some(layout) = Layout::parse(which) else { return usage(format!("--layout: want a, b or c, got {which}")) };
    let (l, node_max) = (a.num("l", Some(4096))?, a.num("node-max", Some(16384))?);
    let cfg = usize::try_from(l)
        .ok()
        .zip(usize::try_from(node_max).ok())
        .and_then(|(l, m)| Config::new(l, m))
        .ok_or_else(|| Fail::Usage(format!("--l {l} --node-max {node_max}: want l in 256..=16384 and node-max in 4096..=32768")))?;
    Ok((layout, cfg))
}

/// Replays the batches of `maps` into the trees of one layout and checks
/// the final trees against the real final maps.
pub(crate) fn layout(argv: &[String]) -> Res<()> {
    let a = Args::parse(argv)?;
    a.check(&["maps", "layout", "store", "l", "node-max"])?;
    let (layout, cfg) = layout_opts(&a)?;
    let dir = a.path("maps")?;
    let store = a.path("store")?;

    let plan = read_plan(&dir)?;
    let cs = ChunkStore::open(&store, Options::default())?;
    let mut streams: Vec<KvStream<'_>> = Vec::new();
    let mut n = 0;
    for kind in MapKind::ALL {
        let (f, count) = BaseFile::open(&dir.join(kind.name()).join("base.bin"), kind)?;
        if kind == MapKind::Inode {
            n = count;
        }
        streams.push(Box::new(f));
    }
    let Ok(base) = <[KvStream<'_>; 5]>::try_from(streams) else { return run_fail("five maps") };
    let ctx = Ctx { source: "boost", n, layout, cfg };
    let mut engine = Engine::build(&cs, ctx, base)?;

    for (seq, pattern, k) in &plan.commits {
        let mut edits = Vec::new();
        for kind in MapKind::ALL {
            read_batch(&dir, kind, *seq, &mut edits)?;
        }
        engine.commit(pattern, *k, *seq, edits)?;
    }
    engine.print_trees("final")?;

    let mut ok = true;
    for kind in MapKind::ALL {
        let mut d = Digest::new();
        for e in engine.iter_map(kind)? {
            let (key, value) = e?;
            d.add_raw(&raw_of(kind, &key, &value)?);
        }
        ok &= plan.finals[kind.index()].as_ref().is_some_and(|(entries, digest)| *entries == d.entries && *digest == d.hex());
    }
    engine.print_check(ok);
    drop(engine);
    cs.close()?;
    if !ok {
        return run_fail("layout: a final tree differs from the real final map");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use metadata::store_io::put_value;

    use super::*;

    fn store() -> (tempfile::TempDir, ChunkStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let cs = ChunkStore::open(dir.path(), Options::default()).expect("open");
        (dir, cs)
    }

    /// A refcount record: a 33-byte key with the number `tag`, and a count.
    fn rc_rec(tag: u16, count: u64) -> [u8; MAP_ENTRY] {
        let mut rec = [0u8; MAP_ENTRY];
        rec[0] = 0x1e;
        rec[1..3].copy_from_slice(&tag.to_be_bytes());
        rec[KEY_LEN..].copy_from_slice(&count.to_le_bytes());
        rec
    }

    /// Stores a flat map node of raw records, as the metadata store does.
    fn flat(cs: &ChunkStore, recs: &[[u8; MAP_ENTRY]]) -> slicefs_traits::key::ObjectKey {
        let mut bytes = (recs.len() as u64).to_le_bytes().to_vec();
        recs.iter().for_each(|r| bytes.extend_from_slice(r));
        put_value(cs, &bytes).expect("put map")
    }

    #[test]
    fn a_refcount_diff_has_puts_deletes_and_inserts() {
        let (dir, cs) = store();
        let old = flat(&cs, &[rc_rec(1, 1), rc_rec(2, 1), rc_rec(3, 2)]);
        let new = flat(&cs, &[rc_rec(1, 1), rc_rec(2, 5), rc_rec(4, 1)]);
        let path = dir.path().join("batch.bin");
        let (puts, dels) = write_batch(&cs, MapKind::Refcounts, &old, &new, &path).expect("diff");
        assert_eq!((puts, dels), (2, 1));
        let bytes = fs::read(&path).expect("batch");
        let recs: Vec<(u8, [u8; MAP_ENTRY])> =
            bytes.chunks(BATCH_REC).map(|c| (c[0], c[1..].try_into().expect("41 bytes"))).collect();
        assert_eq!(recs, vec![(1, rc_rec(2, 5)), (0, rc_rec(3, 2)), (1, rc_rec(4, 1))]);
        let (key, value) = native_of(MapKind::Refcounts, &recs[0].1);
        assert_eq!(raw_of(MapKind::Refcounts, &key, &value).expect("raw"), recs[0].1);
    }

    /// The native base of each map: ino 2.. for the per-inode maps, content
    /// keys for refcounts.
    fn base_entries(kind: MapKind, n: u64) -> Vec<Kv> {
        if kind.per_inode() {
            (2..n + 2).map(|ino| (ino.to_be_bytes().to_vec(), vec![kind.index() as u8; KEY_LEN])).collect()
        } else {
            (0..n).map(|i| (native_of(kind, &rc_rec(i as u16, 0)).0, 1u64.to_le_bytes().to_vec())).collect()
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
                    native_of(kind, &rc_rec(1_000 + u16::from(round), 0)).0
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
