//! The synthetic source of the Q-2 and constants test (D-01, D-02, D-04):
//! `synth` builds the five metadata maps of N inodes from sorted streams made
//! on the fly, replays the D-02 commit mix through the engine of `layout.rs`
//! (output `source=synth`), and checks the final trees.
//!
//! The maps: inode `i` in 1..=N has an inode entry. Inode 1 and about 10% of
//! the others are directories (a dir entry); the others are files (a manifest
//! entry and 2 refcount keys each). About 5% of the inodes have an xattr entry.
//! The four per-inode maps hold the key `ino (8 BE)` and a 33-byte value: the
//! tag `0x1e` and a seeded BLAKE3 of (map, inode, version). The refcount key is
//! `0x1e || BE u64 slot || 24 zero bytes`, the value is the count 1 (8 bytes
//! LE). The slot space has M = 2N slots; file `i` owns the slots `perm(2(i-1))`
//! and `perm(2(i-1)+1)`, with `perm(x) = (A x + B) mod M` a bijection, so the
//! keys of one file land at random leaves while the stream (slots 0..M, each
//! with its owner by the inverse permutation) stays in key order.
//!
//! Commit patterns (`--patterns`, run in this order):
//! - `one` (k=1): a content change of one random live file: put its inode and
//!   manifest with the next version, delete its first base refcount slot that
//!   is still present, insert one refcount key. An inserted key copies a random
//!   slot and sets the last 8 filler bytes to a counter (from 1), so it sorts
//!   next to that slot at a random leaf.
//! - `mixed` (each k of `--ks`): k such content changes at distinct files.
//! - `insdel` (each k of `--ks`): k/2 creates (new inodes N+1.. at the right
//!   edge: inode, manifest, 2 refcount inserts, and a new version of the dir
//!   of a random parent) and k - k/2 deletes (a random live base file: its
//!   inode, manifest, xattr when it has one, and its base refcount slots that
//!   are still present).
//!
//! Memory (D-01): the streams are generated in key order and never collected.
//! Only the overlay of the commits is kept, in sorted vectors that grow with
//! the number of edits, never with N.
//!
//! Output: the `build`, `commit` and `tree` lines of `layout.rs`, and one
//! `check` line: `ok=1` when the entry counts of the five maps and every key of
//! the overlay (changed, inserted, created and deleted) are as expected.

use chunk_store::{ChunkStore, Options};

use crate::layout::{layout_opts, Ctx, Engine, Kv, KvStream, MapEdit, MapKind};
use crate::{run_fail, usage, Args, Res, Rng, KEY_LEN};

/// Tag of the 33-byte values (the first byte of an object key).
const TAG: u8 = 0x1e;
/// Hash domains of the kind of an inode and of its xattr.
const DOM_KIND: u8 = 1;
const DOM_XATTR: u8 = 2;
/// Largest N: the permutation math stays far from `u128` overflow.
const MAX_N: u64 = 1 << 40;
/// Tries to find a live file or slot before the run fails.
const TRIES: u32 = 4096;

/// The permutation of the slot space: `x -> (a x + b) mod m`, with `a` coprime
/// to `m`, and its inverse.
#[derive(Clone, Copy)]
struct Perm {
    a: u128,
    b: u128,
    m: u128,
    a_inv: u128,
}

fn gcd(a: u128, b: u128) -> u128 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// The inverse of `a` modulo `m` (they are coprime), by the extended Euclid.
fn modinv(a: u128, m: u128) -> u128 {
    let (mut r0, mut r1) = (m as i128, a as i128);
    let (mut t0, mut t1) = (0i128, 1i128);
    while r1 != 0 {
        let q = r0 / r1;
        (r0, r1) = (r1, r0 - q * r1);
        (t0, t1) = (t1, t0 - q * t1);
    }
    t0.rem_euclid(m as i128) as u128
}

impl Perm {
    fn new(m: u64, seed: u64) -> Perm {
        let m = u128::from(m);
        // About 0.618 m, odd, then up to the next number coprime to m.
        let mut a = ((m * 0x9E37_79B9_7F4A_7C15) >> 64) | 1;
        while gcd(a, m) != 1 {
            a += 2;
        }
        a %= m;
        let b = (u128::from(seed) * 0x2545_F491_4F6C_DD1D) % m;
        Perm { a, b, m, a_inv: modinv(a, m) }
    }

    fn fwd(&self, x: u64) -> u64 {
        ((self.a * u128::from(x) + self.b) % self.m) as u64
    }

    fn inv(&self, s: u64) -> u64 {
        ((self.a_inv * ((u128::from(s) + self.m - self.b) % self.m)) % self.m) as u64
    }
}

/// The generator of the base maps: pure functions of (seed, inode).
#[derive(Clone, Copy)]
struct Gen {
    seed: u64,
    n: u64,
    perm: Perm,
}

impl Gen {
    fn new(seed: u64, n: u64) -> Gen {
        Gen { seed, n, perm: Perm::new(2 * n, seed) }
    }

    /// A seeded hash of (domain, inode) as a number.
    fn h64(&self, domain: u8, i: u64) -> u64 {
        let mut b = [0u8; 17];
        b[..8].copy_from_slice(&self.seed.to_le_bytes());
        b[8] = domain;
        b[9..].copy_from_slice(&i.to_le_bytes());
        let h = blake3::hash(&b);
        u64::from_le_bytes(h.as_bytes()[..8].try_into().expect("8 bytes"))
    }

    fn is_dir(&self, i: u64) -> bool {
        i == 1 || self.h64(DOM_KIND, i).is_multiple_of(10)
    }

    fn has_xattr(&self, i: u64) -> bool {
        self.h64(DOM_XATTR, i).is_multiple_of(20)
    }

    /// The 33-byte value of an entry of a per-inode map at a version.
    fn value(&self, map: MapKind, i: u64, version: u32) -> Vec<u8> {
        let mut b = [0u8; 21];
        b[..8].copy_from_slice(&self.seed.to_le_bytes());
        b[8] = map as u8;
        b[9..17].copy_from_slice(&i.to_le_bytes());
        b[17..].copy_from_slice(&version.to_le_bytes());
        let mut v = Vec::with_capacity(KEY_LEN);
        v.push(TAG);
        v.extend_from_slice(blake3::hash(&b).as_bytes());
        v
    }

    /// The slot of the `j`-th refcount key of file `i`.
    fn slot(&self, i: u64, j: u64) -> u64 {
        self.perm.fwd(2 * (i - 1) + j)
    }

    /// The inode that owns a slot.
    fn owner(&self, s: u64) -> u64 {
        self.perm.inv(s) / 2 + 1
    }
}

/// The native key of a per-inode map.
fn ino_key(i: u64) -> Vec<u8> {
    i.to_be_bytes().to_vec()
}

/// The refcount key of a slot; `counter` 0 is the base key, a larger one an
/// inserted key that sorts right after it.
fn rc_key(slot: u64, counter: u64) -> Vec<u8> {
    let mut k = vec![0u8; KEY_LEN];
    k[0] = TAG;
    k[1..9].copy_from_slice(&slot.to_be_bytes());
    k[KEY_LEN - 8..].copy_from_slice(&counter.to_be_bytes());
    k
}

fn count_value() -> Vec<u8> {
    1u64.to_le_bytes().to_vec()
}

/// One base map as a sorted stream made on the fly.
struct Stream {
    g: Gen,
    map: MapKind,
    /// The next inode (per-inode maps) or slot (refcounts).
    pos: u64,
}

impl Iterator for Stream {
    type Item = Res<Kv>;

    fn next(&mut self) -> Option<Self::Item> {
        let g = self.g;
        if self.map == MapKind::Refcounts {
            while self.pos < 2 * g.n {
                let s = self.pos;
                self.pos += 1;
                if !g.is_dir(g.owner(s)) {
                    return Some(Ok((rc_key(s, 0), count_value())));
                }
            }
            return None;
        }
        while self.pos <= g.n {
            let i = self.pos;
            self.pos += 1;
            let keep = match self.map {
                MapKind::Dir => g.is_dir(i),
                MapKind::Manifest => !g.is_dir(i),
                MapKind::Xattr => g.has_xattr(i),
                _ => true,
            };
            if keep {
                return Some(Ok((ino_key(i), g.value(self.map, i, 0))));
            }
        }
        None
    }
}

/// The five base streams, in `MapKind` order.
fn streams(g: Gen) -> [KvStream<'static>; 5] {
    MapKind::ALL.map(|map| -> KvStream<'static> {
        let pos = if map == MapKind::Refcounts { 0 } else { 1 };
        Box::new(Stream { g, map, pos })
    })
}

/// Inodes of the base by kind, from one pass over the inode numbers.
struct Counts {
    dirs: u64,
    files: u64,
    xattrs: u64,
}

fn count_base(g: Gen) -> Counts {
    let mut c = Counts { dirs: 0, files: 0, xattrs: 0 };
    for i in 1..=g.n {
        if g.is_dir(i) {
            c.dirs += 1;
        } else {
            c.files += 1;
        }
        c.xattrs += u64::from(g.has_xattr(i));
    }
    c
}

// ── overlay ──────────────────────────────────────────────────────────────────

/// Inserts into a sorted vector; false when the item is already there.
fn insert_sorted<T: Ord>(v: &mut Vec<T>, x: T) -> bool {
    match v.binary_search(&x) {
        Ok(_) => false,
        Err(at) => {
            v.insert(at, x);
            true
        }
    }
}

/// The version of `key` in a sorted vector of (key, version); 0 if absent.
fn version_of(v: &[(u64, u32)], key: u64) -> u32 {
    v.binary_search_by_key(&key, |e| e.0).map_or(0, |at| v[at].1)
}

fn set_version(v: &mut Vec<(u64, u32)>, key: u64, version: u32) {
    match v.binary_search_by_key(&key, |e| e.0) {
        Ok(at) => v[at].1 = version,
        Err(at) => v.insert(at, (key, version)),
    }
}

/// What the commits changed, and the random choices of the run. Every vector
/// is sorted and grows with the edits.
struct State {
    g: Gen,
    base: Counts,
    rng: Rng,
    /// (base file, version) of the content changes.
    changed: Vec<(u64, u32)>,
    /// (directory, version) of the parent updates.
    dirs: Vec<(u64, u32)>,
    /// Deleted base files.
    dead: Vec<u64>,
    /// Deleted base refcount slots.
    dead_slots: Vec<u64>,
    /// Inserted refcount keys as (slot, counter).
    inserted: Vec<(u64, u64)>,
    /// The next inode number of a create.
    next_ino: u64,
    /// The last counter of an inserted key.
    counter: u64,
}

fn put(map: MapKind, key: Vec<u8>, value: Vec<u8>) -> MapEdit {
    MapEdit { map, key, value: Some(value) }
}

fn del(map: MapKind, key: Vec<u8>) -> MapEdit {
    MapEdit { map, key, value: None }
}

impl State {
    fn new(seed: u64, n: u64) -> State {
        let g = Gen::new(seed, n);
        State {
            g,
            base: count_base(g),
            rng: Rng::new(seed),
            changed: Vec::new(),
            dirs: Vec::new(),
            dead: Vec::new(),
            dead_slots: Vec::new(),
            inserted: Vec::new(),
            next_ino: n + 1,
            counter: 0,
        }
    }

    /// A random live base file that is not in the sorted `taken`.
    fn live_file(&mut self, taken: &[u64]) -> Res<u64> {
        for _ in 0..TRIES {
            let f = 2 + self.rng.upto(self.g.n - 2);
            if !self.g.is_dir(f) && self.dead.binary_search(&f).is_err() && taken.binary_search(&f).is_err() {
                return Ok(f);
            }
        }
        run_fail("synth: no live file left to choose (K is too large for N)")
    }

    /// A random directory (inode 1 when none is found).
    fn live_dir(&mut self) -> u64 {
        for _ in 0..TRIES {
            let p = 1 + self.rng.upto(self.g.n - 1);
            if self.g.is_dir(p) {
                return p;
            }
        }
        1
    }

    /// Inserts a refcount key next to a random base slot.
    fn insert_rc(&mut self, edits: &mut Vec<MapEdit>) -> Res<()> {
        for _ in 0..TRIES {
            let s = self.rng.upto(2 * self.g.n - 1);
            if !self.g.is_dir(self.g.owner(s)) {
                self.counter += 1;
                insert_sorted(&mut self.inserted, (s, self.counter));
                edits.push(put(MapKind::Refcounts, rc_key(s, self.counter), count_value()));
                return Ok(());
            }
        }
        run_fail("synth: no base slot found")
    }

    /// Deletes the base refcount slots of `f` that are still present: the
    /// first only (`all` false) or both.
    fn drop_slots(&mut self, f: u64, all: bool, edits: &mut Vec<MapEdit>) {
        for j in 0..2 {
            let s = self.g.slot(f, j);
            if insert_sorted(&mut self.dead_slots, s) {
                edits.push(del(MapKind::Refcounts, rc_key(s, 0)));
                if !all {
                    break;
                }
            }
        }
    }

    /// One content change of file `f` (the `one` unit).
    fn change_file(&mut self, f: u64, edits: &mut Vec<MapEdit>) -> Res<()> {
        let v = version_of(&self.changed, f) + 1;
        set_version(&mut self.changed, f, v);
        edits.push(put(MapKind::Inode, ino_key(f), self.g.value(MapKind::Inode, f, v)));
        edits.push(put(MapKind::Manifest, ino_key(f), self.g.value(MapKind::Manifest, f, v)));
        self.drop_slots(f, false, edits);
        self.insert_rc(edits)
    }

    /// One create at the right edge.
    fn create_file(&mut self, edits: &mut Vec<MapEdit>) -> Res<()> {
        let f = self.next_ino;
        self.next_ino += 1;
        edits.push(put(MapKind::Inode, ino_key(f), self.g.value(MapKind::Inode, f, 0)));
        edits.push(put(MapKind::Manifest, ino_key(f), self.g.value(MapKind::Manifest, f, 0)));
        self.insert_rc(edits)?;
        self.insert_rc(edits)?;
        let p = self.live_dir();
        let v = version_of(&self.dirs, p) + 1;
        set_version(&mut self.dirs, p, v);
        edits.push(put(MapKind::Dir, ino_key(p), self.g.value(MapKind::Dir, p, v)));
        Ok(())
    }

    /// One delete of a random live base file with all its entries.
    fn delete_file(&mut self, edits: &mut Vec<MapEdit>) -> Res<()> {
        let f = self.live_file(&[])?;
        insert_sorted(&mut self.dead, f);
        edits.push(del(MapKind::Inode, ino_key(f)));
        edits.push(del(MapKind::Manifest, ino_key(f)));
        if self.g.has_xattr(f) {
            edits.push(del(MapKind::Xattr, ino_key(f)));
        }
        self.drop_slots(f, true, edits);
        Ok(())
    }

    /// The edits of one commit of a pattern.
    fn edits(&mut self, pattern: &str, k: u64) -> Res<Vec<MapEdit>> {
        let mut edits = Vec::new();
        if pattern == "insdel" {
            for _ in 0..k / 2 {
                self.create_file(&mut edits)?;
            }
            for _ in 0..k - k / 2 {
                self.delete_file(&mut edits)?;
            }
        } else {
            let mut taken: Vec<u64> = Vec::new();
            for _ in 0..k {
                let f = self.live_file(&taken)?;
                insert_sorted(&mut taken, f);
                self.change_file(f, &mut edits)?;
            }
        }
        Ok(merge(edits))
    }

    /// The expected entry count of each map, in `MapKind` order.
    fn expected_counts(&self) -> [u64; 5] {
        let created = self.next_ino - (self.g.n + 1);
        let dead = self.dead.len() as u64;
        let dead_xattrs = self.dead.iter().filter(|f| self.g.has_xattr(**f)).count() as u64;
        [
            self.g.n + created - dead,
            self.base.dirs,
            self.base.files + created - dead,
            self.base.xattrs - dead_xattrs,
            2 * self.base.files - self.dead_slots.len() as u64 + self.inserted.len() as u64,
        ]
    }

    /// True when the counts and every key of the overlay are as expected.
    fn check(&self, e: &Engine<'_>) -> Res<bool> {
        let g = self.g;
        let mut ok = e.map_counts()? == self.expected_counts();
        let mut expect = |map: MapKind, key: Vec<u8>, want: Option<Vec<u8>>| -> Res<()> {
            ok &= e.get(map, &key)? == want;
            Ok(())
        };
        for &(f, v) in &self.changed {
            if self.dead.binary_search(&f).is_ok() {
                continue;
            }
            expect(MapKind::Inode, ino_key(f), Some(g.value(MapKind::Inode, f, v)))?;
            expect(MapKind::Manifest, ino_key(f), Some(g.value(MapKind::Manifest, f, v)))?;
        }
        for &(p, v) in &self.dirs {
            expect(MapKind::Dir, ino_key(p), Some(g.value(MapKind::Dir, p, v)))?;
        }
        for f in g.n + 1..self.next_ino {
            expect(MapKind::Inode, ino_key(f), Some(g.value(MapKind::Inode, f, 0)))?;
            expect(MapKind::Manifest, ino_key(f), Some(g.value(MapKind::Manifest, f, 0)))?;
        }
        for &(s, c) in &self.inserted {
            expect(MapKind::Refcounts, rc_key(s, c), Some(count_value()))?;
        }
        for &f in &self.dead {
            for map in [MapKind::Inode, MapKind::Manifest, MapKind::Xattr] {
                expect(map, ino_key(f), None)?;
            }
        }
        for &s in &self.dead_slots {
            expect(MapKind::Refcounts, rc_key(s, 0), None)?;
        }
        Ok(ok)
    }
}

/// Sorts the edits of one commit by (map, key) and keeps the last edit of a
/// key (the dir of a parent can be updated twice in one commit).
fn merge(mut edits: Vec<MapEdit>) -> Vec<MapEdit> {
    edits.sort_by(|a, b| (a.map as usize, &a.key).cmp(&(b.map as usize, &b.key)));
    let mut out: Vec<MapEdit> = Vec::with_capacity(edits.len());
    for e in edits {
        match out.last_mut() {
            Some(l) if l.map == e.map && l.key == e.key => *l = e,
            _ => out.push(e),
        }
    }
    out
}

// ── synth ────────────────────────────────────────────────────────────────────

/// `synth --n N --layout a|b|c --store P [--seed S] [--l L] [--node-max M]
/// [--commits C] [--ks "100 1000"] [--patterns "one mixed insdel"]`.
pub(crate) fn synth(argv: &[String]) -> Res<()> {
    let a = Args::parse(argv)?;
    a.check(&["n", "layout", "store", "seed", "l", "node-max", "commits", "ks", "patterns"])?;
    let n = a.num("n", None)?;
    if !(10..=MAX_N).contains(&n) {
        return usage(format!("--n: want 10..={MAX_N}, got {n}"));
    }
    let (layout, cfg) = layout_opts(&a)?;
    let store = a.path("store")?;
    let seed = a.num("seed", Some(25))?;
    let commits = a.num("commits", Some(10))?;
    if commits == 0 {
        return usage("--commits: want at least 1");
    }
    let patterns = a.words("patterns", "one mixed insdel");
    if patterns.is_empty() || patterns.iter().any(|p| !["one", "mixed", "insdel"].contains(&p.as_str())) {
        return usage(format!("--patterns: want one, mixed or insdel, got {patterns:?}"));
    }
    let ks = a.nums("ks", "100 1000")?;
    let needs_ks = patterns.iter().any(|p| p != "one");
    if needs_ks && ks.iter().any(|&k| k == 0 || k > n / 4) {
        return usage(format!("--ks: each K must be in 1..=N/4 (N is {n}), got {ks:?}"));
    }

    let mut st = State::new(seed, n);
    let cs = ChunkStore::open(&store, Options::default())?;
    let mut engine = Engine::build(&cs, Ctx { source: "synth", n, layout, cfg }, streams(st.g))?;
    let mut seq = 0u64;
    // The order of the run: one, mixed for each K, insdel for each K.
    for pattern in ["one", "mixed", "insdel"] {
        if !patterns.iter().any(|p| p == pattern) {
            continue;
        }
        let run_ks = if pattern == "one" { vec![1] } else { ks.clone() };
        for k in run_ks {
            for _ in 0..commits {
                let edits = st.edits(pattern, k)?;
                seq += 1;
                engine.commit(pattern, k, seq, edits)?;
            }
        }
    }
    engine.print_trees("final")?;
    let ok = st.check(&engine)?;
    engine.print_check(ok);
    drop(engine);
    cs.close()?;
    if !ok {
        return run_fail("synth: a final tree differs from the expected state");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_permutation_is_a_bijection_with_an_inverse() {
        for (n, seed) in [(10u64, 1u64), (5000, 25), (65_536, 7), (1_000_000, 25)] {
            let p = Perm::new(2 * n, seed);
            let mut seen = vec![false; (2 * n).min(200_000) as usize];
            for x in 0..(2 * n).min(200_000) {
                let s = p.fwd(x);
                assert!(s < 2 * n);
                assert_eq!(p.inv(s), x);
                if let Some(slot) = seen.get_mut(s as usize) {
                    assert!(!*slot, "slot {s} twice");
                    *slot = true;
                }
            }
        }
    }

    #[test]
    fn the_streams_are_sorted_and_their_counts_match() {
        let g = Gen::new(25, 3000);
        let c = count_base(g);
        let all = streams(g);
        let counts: Vec<u64> = all
            .into_iter()
            .map(|s| {
                let keys: Vec<Vec<u8>> = s.map(|r| r.expect("entry").0).collect();
                assert!(keys.windows(2).all(|w| w[0] < w[1]));
                keys.len() as u64
            })
            .collect();
        assert_eq!(counts, vec![3000, c.dirs, c.files, c.xattrs, 2 * c.files]);
    }

    #[test]
    fn an_inserted_refcount_key_sorts_next_to_its_slot() {
        assert!(rc_key(5, 0) < rc_key(5, 1));
        assert!(rc_key(5, 1) < rc_key(5, 2));
        assert!(rc_key(5, 9) < rc_key(6, 0));
        assert_eq!(rc_key(5, 3).len(), KEY_LEN);
    }
}
