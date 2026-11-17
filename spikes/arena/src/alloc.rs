//! Spike C: free-space allocators for reuse mode (plan section 6). No I/O:
//! the same code runs in `alloc_sim` and in `ReuseStore`.
//!
//! The free space of an arena is a set of extents `[off, off + len)`,
//! coalesced on free. The never-written space of an arena (at or above its
//! high-water mark `hwm`) is a free extent like any other; `bump_only`
//! restricts allocation to it (no extent freed by a GC is given out while a
//! GC runs).
//!
//! Policies:
//! - V1 `NextFit`: address order (arena, offset) from a cursor, first extent
//!   that fits, wrap around once.
//! - V2 `SizeClasses`: segregated fit. Extents are kept in quarter-power-of-2
//!   classes; the lowest address of the first class that fits is used.
//! - V3 `Hybrid`: next fit inside the current target arena, else V2. The
//!   store (or the simulator) also evacuates arenas below `evacuate_below`
//!   live at GC.
//! - V4 `ArenaBest` (added by spike C after the first grid): best fit
//!   (smallest extent that fits) inside the current target arena; if none
//!   fits, the target becomes the arena with the most free bytes that has a
//!   fit. Writes stay in one arena at a time, as V1, and holes are packed
//!   as tightly as V2.

use crate::AllocKind;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AllocStats {
    pub arenas: u64,
    pub extents: u64,
    pub free_bytes: u64,
    /// Free bytes in extents shorter than the `min` given to `stats`.
    pub unusable_bytes: u64,
    /// Free bytes at or above the high-water marks (never written).
    pub bump_bytes: u64,
}

pub trait Allocator: Send {
    /// A new arena whose free space is `[start, end)`, all of it bump space.
    fn add_arena(&mut self, arena: u64, start: u64, end: u64);
    /// An arena with records at `used` (offset, padded length; any order):
    /// the free space is the complement in `[start, end)`; the high-water
    /// mark is the end of the last record.
    fn add_arena_used(&mut self, arena: u64, start: u64, end: u64, used: &mut [(u64, u64)]);
    /// Forget an arena (deleted, or evacuated: nothing more goes into it).
    fn remove_arena(&mut self, arena: u64);
    /// `len` bytes; `None` if no extent fits.
    fn alloc(&mut self, len: u64) -> Option<(u64, u64)>;
    fn free(&mut self, arena: u64, off: u64, len: u64);
    fn set_bump_only(&mut self, on: bool);
    /// Free bytes of one arena.
    fn arena_free(&self, arena: u64) -> u64;
    fn stats(&self, min: u64) -> AllocStats;
}

pub fn new(kind: AllocKind) -> Box<dyn Allocator> {
    Box::new(FreeMap::new(kind))
}

#[derive(Default)]
struct ArenaFree {
    /// off -> len
    ext: BTreeMap<u64, u64>,
    /// (len, off), for best fit and the largest extent of the arena.
    by_len: BTreeSet<(u64, u64)>,
    hwm: u64,
    end: u64,
    free: u64,
}

impl ArenaFree {
    fn max_len(&self) -> u64 {
        self.by_len.iter().next_back().map_or(0, |x| x.0)
    }
    /// The largest bump piece (extent part at or above hwm).
    fn bump_len(&self) -> u64 {
        match self.ext.range(..self.end).next_back() {
            Some((&o, &l)) if o + l == self.end => self.end - o.max(self.hwm).min(self.end),
            _ => 0,
        }
    }
}

/// Quarter-power-of-2 size class of `len` (>= 1).
fn class(len: u64) -> usize {
    let lg = 63 - len.max(1).leading_zeros() as usize;
    let sub = if lg >= 2 { ((len >> (lg - 2)) & 3) as usize } else { 0 };
    lg * 4 + sub
}

pub struct FreeMap {
    kind: AllocKind,
    arenas: BTreeMap<u64, ArenaFree>,
    /// class -> (arena, off), for V2 / V3.
    classes: Vec<BTreeSet<(u64, u64)>>,
    cursor: (u64, u64),
    bump_only: bool,
}

impl FreeMap {
    pub fn new(kind: AllocKind) -> Self {
        FreeMap { kind, arenas: BTreeMap::new(), classes: vec![BTreeSet::new(); 64 * 4], cursor: (0, 0), bump_only: false }
    }

    fn uses_classes(&self) -> bool {
        matches!(self.kind, AllocKind::SizeClasses | AllocKind::Hybrid)
    }

    /// Best fit in `arena`: (extent offset, position).
    fn best_in(&self, arena: u64, len: u64) -> Option<(u64, u64)> {
        let a = self.arenas.get(&arena)?;
        if self.bump_only {
            let (&o, &l) = a.ext.range(..a.end).next_back()?;
            return Self::fit(a, o, l, len, true).map(|p| (o, p));
        }
        a.by_len.range((len, 0)..).next().map(|&(_, o)| (o, o))
    }

    fn arena_best(&mut self, len: u64) -> Option<(u64, u64)> {
        let ca = self.cursor.0;
        if let Some((o, p)) = self.best_in(ca, len) {
            return Some(self.take(ca, o, p, len));
        }
        let bump = self.bump_only;
        let pick = self
            .arenas
            .iter()
            .filter(|(_, a)| if bump { a.bump_len() >= len } else { a.max_len() >= len })
            .max_by_key(|(&n, a)| (a.free, std::cmp::Reverse(n)))
            .map(|(&n, _)| n)?;
        let (o, p) = self.best_in(pick, len)?;
        Some(self.take(pick, o, p, len))
    }

    fn insert_ext(&mut self, arena: u64, off: u64, len: u64) {
        if len == 0 {
            return;
        }
        let a = self.arenas.get_mut(&arena).unwrap();
        a.ext.insert(off, len);
        a.by_len.insert((len, off));
        a.free += len;
        if self.uses_classes() {
            self.classes[class(len)].insert((arena, off));
        }
    }

    fn remove_ext(&mut self, arena: u64, off: u64) -> u64 {
        let a = self.arenas.get_mut(&arena).unwrap();
        let len = a.ext.remove(&off).unwrap();
        a.by_len.remove(&(len, off));
        a.free -= len;
        if self.uses_classes() {
            self.classes[class(len)].remove(&(arena, off));
        }
        len
    }

    /// Where `len` bytes fit in extent `(o, l)` of `a`, if they do.
    fn fit(a: &ArenaFree, o: u64, l: u64, len: u64, bump_only: bool) -> Option<u64> {
        let p = if bump_only { o.max(a.hwm) } else { o };
        (p + len <= o + l).then_some(p)
    }

    /// Take `[p, p + len)` out of the extent at `o`.
    fn take(&mut self, arena: u64, o: u64, p: u64, len: u64) -> (u64, u64) {
        let l = self.remove_ext(arena, o);
        self.insert_ext(arena, o, p - o);
        self.insert_ext(arena, p + len, o + l - (p + len));
        let a = self.arenas.get_mut(&arena).unwrap();
        a.hwm = a.hwm.max(p + len);
        self.cursor = (arena, p + len);
        (arena, p)
    }

    /// First fit in `arena` at or after offset `from`.
    fn first_fit_in(&self, arena: u64, from: u64, len: u64) -> Option<(u64, u64)> {
        let a = self.arenas.get(&arena)?;
        if self.bump_only {
            if a.bump_len() < len {
                return None;
            }
        } else if a.max_len() < len {
            return None;
        }
        // An extent that starts before `from` but reaches past it counts.
        let start = a.ext.range(..from).next_back().filter(|(&o, &l)| o + l > from).map_or(from, |(&o, _)| o);
        for (&o, &l) in a.ext.range(start..) {
            if let Some(p) = Self::fit(a, o, l, len, self.bump_only) {
                return Some((o, p));
            }
        }
        None
    }

    fn next_fit(&mut self, len: u64) -> Option<(u64, u64)> {
        let (ca, co) = self.cursor;
        if let Some((o, p)) = self.first_fit_in(ca, co, len) {
            return Some(self.take(ca, o, p, len));
        }
        let nums: Vec<u64> = self.arenas.range(ca + 1..).map(|(&n, _)| n).chain(self.arenas.range(..=ca).map(|(&n, _)| n)).collect();
        for n in nums {
            if let Some((o, p)) = self.first_fit_in(n, 0, len) {
                return Some(self.take(n, o, p, len));
            }
        }
        None
    }

    fn size_class(&mut self, len: u64) -> Option<(u64, u64)> {
        let c0 = class(len);
        for c in c0..self.classes.len() {
            // In the first class an extent may be too short; look at a few.
            let mut found = None;
            for (k, &(n, o)) in self.classes[c].iter().enumerate() {
                if c == c0 && k >= 64 {
                    break;
                }
                let a = &self.arenas[&n];
                let l = a.ext[&o];
                if let Some(p) = Self::fit(a, o, l, len, self.bump_only) {
                    found = Some((n, o, p));
                    break;
                }
                if self.bump_only && k >= 64 {
                    break;
                }
            }
            if let Some((n, o, p)) = found {
                return Some(self.take(n, o, p, len));
            }
        }
        None
    }
}

impl Allocator for FreeMap {
    fn add_arena(&mut self, arena: u64, start: u64, end: u64) {
        self.arenas.insert(arena, ArenaFree { hwm: start, end, ..Default::default() });
        self.insert_ext(arena, start, end - start);
    }

    fn add_arena_used(&mut self, arena: u64, start: u64, end: u64, used: &mut [(u64, u64)]) {
        used.sort_unstable();
        let hwm = used.iter().map(|&(o, l)| o + l).max().unwrap_or(start).max(start);
        self.arenas.insert(arena, ArenaFree { hwm, end, ..Default::default() });
        let mut pos = start;
        for &(o, l) in used.iter() {
            if o > pos {
                self.insert_ext(arena, pos, o - pos);
            }
            pos = pos.max(o + l);
        }
        if end > pos {
            self.insert_ext(arena, pos, end - pos);
        }
    }

    fn remove_arena(&mut self, arena: u64) {
        let Some(a) = self.arenas.get(&arena) else { return };
        let offs: Vec<u64> = a.ext.keys().copied().collect();
        for o in offs {
            self.remove_ext(arena, o);
        }
        self.arenas.remove(&arena);
    }

    fn alloc(&mut self, len: u64) -> Option<(u64, u64)> {
        match self.kind {
            AllocKind::NextFit => self.next_fit(len),
            AllocKind::SizeClasses => self.size_class(len),
            AllocKind::Hybrid => {
                let (ca, co) = self.cursor;
                if let Some((o, p)) = self.first_fit_in(ca, co, len) {
                    return Some(self.take(ca, o, p, len));
                }
                self.size_class(len)
            }
            AllocKind::ArenaBest => self.arena_best(len),
        }
    }

    fn free(&mut self, arena: u64, off: u64, len: u64) {
        if len == 0 || !self.arenas.contains_key(&arena) {
            return;
        }
        let (mut o, mut l) = (off, len);
        let a = &self.arenas[&arena];
        let prev = a.ext.range(..off).next_back().map(|(&po, &pl)| (po, pl));
        let next = a.ext.range(off..).next().map(|(&no, &nl)| (no, nl));
        if let Some((po, pl)) = prev {
            debug_assert!(po + pl <= off, "double free in arena {arena} at {off}");
            if po + pl == off {
                self.remove_ext(arena, po);
                o = po;
                l += pl;
            }
        }
        if let Some((no, nl)) = next {
            debug_assert!(off + len <= no, "double free in arena {arena} at {off}");
            if off + len == no {
                self.remove_ext(arena, no);
                l += nl;
            }
        }
        self.insert_ext(arena, o, l);
    }

    fn set_bump_only(&mut self, on: bool) {
        self.bump_only = on;
    }

    fn arena_free(&self, arena: u64) -> u64 {
        self.arenas.get(&arena).map_or(0, |a| a.free)
    }

    fn stats(&self, min: u64) -> AllocStats {
        let mut s = AllocStats { arenas: self.arenas.len() as u64, ..Default::default() };
        for a in self.arenas.values() {
            for (&o, &l) in &a.ext {
                s.extents += 1;
                s.free_bytes += l;
                if l < min {
                    s.unusable_bytes += l;
                }
                if o + l > a.hwm {
                    s.bump_bytes += o + l - o.max(a.hwm);
                }
            }
        }
        s
    }
}

impl FreeMap {
    /// Test helper: the extents of `arena`.
    pub fn extents(&self, arena: u64) -> Vec<(u64, u64)> {
        self.arenas.get(&arena).map(|a| a.ext.iter().map(|(&o, &l)| (o, l)).collect()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> [AllocKind; 4] {
        [AllocKind::NextFit, AllocKind::SizeClasses, AllocKind::Hybrid, AllocKind::ArenaBest]
    }

    #[test]
    fn alloc_free_coalesce() {
        for k in all() {
            let mut m = FreeMap::new(k);
            m.add_arena(1, 4096, 4096 + 1000);
            let a = m.alloc(100).unwrap();
            let b = m.alloc(200).unwrap();
            let c = m.alloc(300).unwrap();
            assert_eq!((a, b, c), ((1, 4096), (1, 4196), (1, 4396)), "{k:?}");
            assert!(m.alloc(500).is_none());
            m.free(1, 4196, 200);
            m.free(1, 4096, 100);
            assert_eq!(m.extents(1), vec![(4096, 300), (4696, 400)]);
            m.free(1, 4396, 300);
            assert_eq!(m.extents(1), vec![(4096, 1000)]);
        }
    }

    #[test]
    fn bump_only_skips_holes() {
        for k in all() {
            let mut m = FreeMap::new(k);
            m.add_arena(1, 0, 1000);
            m.alloc(100).unwrap();
            m.alloc(100).unwrap();
            m.free(1, 0, 100);
            m.set_bump_only(true);
            assert_eq!(m.alloc(50), Some((1, 200)), "{k:?}");
            m.set_bump_only(false);
            assert!(m.alloc(800).is_none() && m.alloc(100).is_some(), "{k:?}");
        }
    }

    #[test]
    fn used_complement() {
        let mut m = FreeMap::new(AllocKind::NextFit);
        m.add_arena_used(3, 100, 1000, &mut [(500, 100), (100, 50)]);
        assert_eq!(m.extents(3), vec![(150, 350), (600, 400)]);
        let s = m.stats(400);
        assert_eq!(s.free_bytes, 750);
        assert_eq!(s.unusable_bytes, 350);
        assert_eq!(s.bump_bytes, 400);
    }
}
