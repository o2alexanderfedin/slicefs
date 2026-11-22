//! Overlay of a file that is written out of order (#98).
//!
//! The file content is a base (a stored content root, which the manifest of
//! the inode names) plus the bytes written since that base: a sparse map of
//! extents. Reads merge the two. A truncate or an extend changes only the
//! sizes and the extents. Nothing here reads the base or allocates by file
//! offset or file size: memory follows the written bytes only.
//!
//! The logical content of `[0, size)` is:
//! - base bytes in `[0, visible)`,
//! - zeros in `[visible, size)`,
//! - with the extents laid over both.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use slicefs_traits::key::ObjectKey;

/// Bytes charged per extent on top of its buffer (map node and Vec header),
/// so that many one-byte writes cannot grow the map without a cost.
pub const EXTENT_OVERHEAD: usize = 64;

/// A growing extent gets at most 1/GROW_DIV of its length as spare capacity
/// (not the 2x of `Vec` doubling), so its cost stays near its length.
pub const GROW_DIV: usize = 8;

/// `Overlay::take` splits an extent only if it is longer than what is still
/// needed plus this, and then takes at least this many bytes of it.
pub const MIN_TAKE: usize = 64 << 10;

/// Source of overlay generations. One counter for the whole process, so no
/// two states of any overlays share a generation: an overlay made after a
/// truncate to 0 cannot repeat the generation of the one it replaced.
static NEXT_GEN: AtomicU64 = AtomicU64::new(1);

fn next_gen() -> u64 {
    NEXT_GEN.fetch_add(1, Ordering::Relaxed)
}

/// Base content plus the extents written since that base.
///
/// Invariants: `visible <= base_len`; extents do not overlap and do not touch
/// (touching ones are merged); every extent ends at or below `size`.
pub struct Overlay {
    /// The content root this overlay starts from (`None`: an empty base).
    pub base: Option<ObjectKey>,
    /// Length of the base content.
    pub base_len: u64,
    /// Base bytes in `[0, visible)` still show; past it the base is cut off.
    pub visible: u64,
    /// Logical file size.
    pub size: u64,
    extents: BTreeMap<u64, Vec<u8>>,
    cost: usize,
    /// Where the next `take` starts. Steps go through the file in offset
    /// order, so the extents of one step are close and change few nodes.
    sweep: u64,
    /// Moves at every change of the extents, `visible`, `size` or the sweep
    /// point (D-02): a rebase step publishes only if it did not move.
    generation: u64,
}

impl Overlay {
    /// A clean overlay on `base` (of `base_len` bytes) with logical size `size`.
    pub fn new(base: Option<ObjectKey>, base_len: u64, size: u64) -> Self {
        Overlay { base, base_len, visible: base_len, size, extents: BTreeMap::new(), cost: 0, sweep: 0, generation: next_gen() }
    }

    /// A clean overlay with the base, visible part and size of this one.
    pub fn empty_like(&self) -> Self {
        Overlay { extents: BTreeMap::new(), cost: 0, sweep: 0, generation: next_gen(), ..*self }
    }

    /// The generation (see the field). A new overlay, also one made after a
    /// publish, starts past every generation before it.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The pieces `take(free)` would take, as `(start, len)`, with no change.
    /// In offset order from the sweep point, then from 0; a whole extent
    /// frees its capacity plus `EXTENT_OVERHEAD`; a split head (at least
    /// `MIN_TAKE` bytes) is always the last piece.
    fn select(&self, free: usize) -> Vec<(u64, usize)> {
        let mut out = Vec::new();
        let mut freed = 0usize;
        for (&s, v) in self.extents.range(self.sweep..).chain(self.extents.range(..self.sweep)) {
            if freed >= free {
                break;
            }
            let need = free - freed;
            if v.len() > need.saturating_add(MIN_TAKE) {
                out.push((s, need.max(MIN_TAKE)));
                break;
            }
            freed += v.capacity() + EXTENT_OVERHEAD;
            out.push((s, v.len()));
        }
        out
    }

    /// Remove the pieces `sel` (heads of extents, from `select`) and return
    /// their bytes. The sweep point goes to the end of the last piece.
    fn cut(&mut self, sel: &[(u64, usize)]) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::with_capacity(sel.len());
        for &(s, k) in sel {
            let mut v = self.remove(s);
            if k < v.len() {
                let head = v[..k].to_vec();
                v.drain(..k);
                v.shrink_to_fit();
                let rest = s + k as u64;
                self.insert(rest, v);
                self.sweep = rest;
                out.push((s, head));
            } else {
                self.sweep = s + v.len() as u64;
                out.push((s, v));
            }
        }
        if !sel.is_empty() {
            self.generation = next_gen();
        }
        out
    }

    /// A copy of what `take(free)` would take, with no change (Phase A of a
    /// rebase step, D-02). Memory: about `max(free, MIN_TAKE)` bytes, never
    /// the whole overlay (#98).
    pub fn copy_step(&self, free: usize) -> Vec<(u64, Vec<u8>)> {
        self.select(free).into_iter().map(|(s, k)| (s, self.extents[&s][..k].to_vec())).collect()
    }

    /// The pieces `take(free)` would take, as `(start, len)`, with no copy.
    pub fn step_ranges(&self, free: usize) -> Vec<(u64, usize)> {
        self.select(free)
    }

    /// Remove exactly the pieces `copied` (from `copy_step` or `step_ranges`
    /// at the current generation), as `take` would.
    pub fn discard(&mut self, copied: &[(u64, usize)]) {
        self.cut(copied);
    }

    /// Take extents out until at least `free` bytes of cost are freed (or no
    /// extent is left): in offset order from the sweep point, then from 0.
    /// An extent longer than what is still needed plus `MIN_TAKE` gives only
    /// its head (at least `MIN_TAKE` bytes); its rest moves down in place,
    /// so no second buffer of the extent is made.
    pub fn take(&mut self, free: usize) -> Vec<(u64, Vec<u8>)> {
        let sel = self.select(free);
        self.cut(&sel)
    }

    /// The sweep point (see `take`).
    pub fn sweep(&self) -> u64 {
        self.sweep
    }

    /// Set the sweep point (an overlay on a new base goes on from the old one).
    pub fn set_sweep(&mut self, at: u64) {
        self.sweep = at;
        self.generation = next_gen();
    }

    /// All extents, in offset order (they move out, no copy).
    pub fn into_extents(self) -> impl Iterator<Item = (u64, Vec<u8>)> {
        self.extents.into_iter()
    }

    /// Write `data` at `off` (pwrite semantics: the file grows to the end).
    pub fn write(&mut self, off: u64, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.put(off, data.to_vec());
    }

    /// Write owned bytes at `off`. When they touch no extent, the Vec goes
    /// into the map as it is (no copy).
    pub fn put(&mut self, off: u64, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        self.generation = next_gen();
        let end = off + data.len() as u64;
        // Extents that overlap or touch [off, end]: they start at or before
        // `end` and end at or after `off`.
        let hits: Vec<u64> = self.extents.range(..=end).rev()
            .take_while(|&(&s, v)| s + v.len() as u64 >= off)
            .map(|(&s, _)| s)
            .collect();
        self.size = self.size.max(end);
        if hits.is_empty() {
            self.insert(off, data);
            return;
        }
        let lo = *hits.last().expect("not empty");
        let start = lo.min(off);
        let last_end = hits.iter().map(|s| s + self.extents[s].len() as u64).max().expect("not empty");
        let stop = last_end.max(end);
        // Grow the lowest extent if it starts at `start`, else a new Vec.
        let mut merged = if lo == start { self.remove(lo) } else { Vec::new() };
        // Grow by at most 1/GROW_DIV, not by doubling: the cost counts the
        // capacity, so doubling could hold 2x the dirty limit before the
        // rebase. Geometric growth keeps in-order writes amortized O(1).
        let need = (stop - start) as usize;
        if need > merged.capacity() {
            let cap = merged.capacity();
            let target = need.max(cap + cap / GROW_DIV);
            merged.reserve_exact(target - merged.len());
        }
        merged.resize(need, 0);
        for s in hits {
            if s == lo && lo == start {
                continue;
            }
            let v = self.remove(s);
            let at = (s - start) as usize;
            merged[at..at + v.len()].copy_from_slice(&v);
        }
        let at = (off - start) as usize;
        merged[at..at + data.len()].copy_from_slice(&data);
        self.insert(start, merged);
    }

    /// Set the size to `n`: drop the extents at or past `n`, cut the one
    /// across `n`, and hide the base past `n`. Growing adds zeros.
    pub fn truncate(&mut self, n: u64) {
        self.generation = next_gen();
        let past: Vec<u64> = self.extents.range(n..).map(|(&s, _)| s).collect();
        for s in past {
            self.remove(s);
        }
        let across = self.extents.range(..n).next_back()
            .filter(|&(&s, v)| s + v.len() as u64 > n)
            .map(|(&s, _)| s);
        if let Some(s) = across {
            let mut v = self.remove(s);
            v.truncate((n - s) as usize);
            v.shrink_to_fit();
            self.insert(s, v);
        }
        self.visible = self.visible.min(n);
        self.size = n;
    }

    /// RAM charged to this overlay: extent buffers plus `EXTENT_OVERHEAD` each.
    pub fn cost(&self) -> usize {
        self.cost
    }

    /// True if the content is the base as it is.
    pub fn is_clean(&self) -> bool {
        self.extents.is_empty() && self.visible == self.base_len && self.size == self.base_len
    }

    /// The extents as sorted `[start, end)` ranges.
    pub fn extent_ranges(&self) -> Vec<std::ops::Range<u64>> {
        self.extents.iter().map(|(&s, v)| s..s + v.len() as u64).collect()
    }

    pub fn extent_count(&self) -> usize {
        self.extents.len()
    }

    /// A copy of `[off, off + len)`, which must lie inside one extent.
    /// Empty if it does not.
    pub fn copy_out(&self, off: u64, len: usize) -> Vec<u8> {
        match self.extents.range(..=off).next_back() {
            Some((&s, v)) if off + len as u64 <= s + v.len() as u64 => {
                let at = (off - s) as usize;
                v[at..at + len].to_vec()
            }
            _ => Vec::new(),
        }
    }

    /// What a read of `[off, off + len)` needs: the sizes, the base key and a
    /// copy of the extent parts inside the range only.
    pub fn view(&self, off: u64, len: u64) -> RangeView {
        let end = off.saturating_add(len).min(self.size);
        let mut parts = Vec::new();
        if off < end {
            let first = self.extents.range(..=off).next_back().map_or(off, |(&s, _)| s);
            for (&s, v) in self.extents.range(first..end) {
                let e = s + v.len() as u64;
                if e <= off {
                    continue;
                }
                let (a, b) = (s.max(off), e.min(end));
                parts.push((a, v[(a - s) as usize..(b - s) as usize].to_vec()));
            }
        }
        RangeView { base: self.base, off, end: end.max(off), visible: self.visible, parts }
    }

    fn insert(&mut self, s: u64, v: Vec<u8>) {
        self.cost += v.capacity() + EXTENT_OVERHEAD;
        self.extents.insert(s, v);
    }

    fn remove(&mut self, s: u64) -> Vec<u8> {
        let v = self.extents.remove(&s).expect("extent exists");
        self.cost -= v.capacity() + EXTENT_OVERHEAD;
        v
    }
}

/// A read range of an overlay, taken out of the lock (see `Overlay::view`).
pub struct RangeView {
    /// The base content root to read `[off, min(end, visible))` from.
    pub base: Option<ObjectKey>,
    off: u64,
    end: u64,
    visible: u64,
    parts: Vec<(u64, Vec<u8>)>,
}

impl RangeView {
    /// The bytes of the range: base bytes from `base_read(offset, len)`, then
    /// zeros, then the extent parts on top. The base is asked only for the
    /// bytes below `visible` that no part covers (#108: a rebase does not read
    /// the base under its extents). A base read of the wrong length is an
    /// error (a damaged base must not turn into zeros).
    pub fn read(
        self,
        mut base_read: impl FnMut(u64, usize) -> std::io::Result<Vec<u8>>,
    ) -> std::io::Result<Vec<u8>> {
        let n = (self.end - self.off) as usize;
        let base_end = self.end.min(self.visible);
        let mut out = vec![0u8; n];
        let mut fill = |a: u64, b: u64, out: &mut [u8]| -> std::io::Result<()> {
            let b = b.min(base_end);
            if a >= b {
                return Ok(());
            }
            let want = (b - a) as usize;
            let got = base_read(a, want)?;
            if got.len() != want {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("overlay base read: {} of {} bytes at {}", got.len(), want, a),
                ));
            }
            let at = (a - self.off) as usize;
            out[at..at + want].copy_from_slice(&got);
            Ok(())
        };
        // The parts are sorted and do not overlap: read the gaps between them.
        let mut pos = self.off;
        for (a, bytes) in &self.parts {
            fill(pos, *a, &mut out)?;
            pos = a + bytes.len() as u64;
        }
        fill(pos, self.end, &mut out)?;
        for (a, bytes) in self.parts {
            let at = (a - self.off) as usize;
            out[at..at + bytes.len()].copy_from_slice(&bytes);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift64: a seeded generator without a new crate.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }

        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next() as u8).collect()
        }
    }

    /// Read `[off, off + len)` of `ov`, with the base served from `base`.
    fn read(ov: &Overlay, base: &[u8], off: u64, len: u64) -> Vec<u8> {
        ov.view(off, len).read(|o, n| {
            let o = o as usize;
            assert!(o + n <= base.len(), "base read past the base: {o}+{n} > {}", base.len());
            Ok(base[o..o + n].to_vec())
        }).unwrap()
    }

    fn model_range(model: &[u8], off: u64, len: u64) -> &[u8] {
        let a = (off as usize).min(model.len());
        let b = (off.saturating_add(len) as usize).min(model.len());
        &model[a..b]
    }

    #[test]
    fn overlay_reads_match_a_vec_model() {
        for seed in 1..=4u64 {
            let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let base_len = r.below(256 * 1024 + 1) as usize;
            let base = r.bytes(base_len);
            let mut model = base.clone();
            let mut ov = Overlay::new(None, base_len as u64, base_len as u64);
            for _ in 0..500 {
                let len = model.len() as u64;
                match r.below(10) {
                    0..=4 => {
                        let off = r.below(len + 64 * 1024);
                        let n = 1 + r.below(8 * 1024) as usize;
                        let data = r.bytes(n);
                        let end = off as usize + data.len();
                        if end > model.len() {
                            model.resize(end, 0);
                        }
                        model[off as usize..end].copy_from_slice(&data);
                        ov.write(off, &data);
                    }
                    5 => {
                        let n = r.below(len + 1);
                        model.truncate(n as usize);
                        ov.truncate(n);
                    }
                    6 => {
                        let n = len + r.below(64 * 1024);
                        model.resize(n as usize, 0);
                        ov.truncate(n);
                    }
                    _ => {
                        let off = r.below(len + 100);
                        let n = r.below(16 * 1024);
                        assert_eq!(read(&ov, &base, off, n), model_range(&model, off, n), "seed {seed} read {off}+{n}");
                    }
                }
                assert_eq!(ov.size, model.len() as u64);
                assert!(ov.visible <= ov.base_len);
            }
            assert_eq!(read(&ov, &base, 0, ov.size + 1), model, "seed {seed}: whole content");
        }
    }

    #[test]
    fn touching_writes_merge_and_cost_counts_each_extent() {
        let mut ov = Overlay::new(None, 0, 0);
        for i in 0..1000u64 {
            ov.write(i * 2, &[1]);
        }
        assert_eq!(ov.extent_count(), 1000);
        assert!(ov.cost() >= 1000 * EXTENT_OVERHEAD, "cost {}", ov.cost());
        // A write between two extents joins them into one.
        ov.write(1, &[2]);
        assert_eq!(ov.extent_count(), 999);
        assert_eq!(ov.extent_ranges()[0], 0..3);
        let before = ov.cost();
        ov.truncate(1000);
        assert!(ov.cost() < before, "truncate lowers the cost: {} -> {}", before, ov.cost());
        assert_eq!(ov.extent_count(), 499);

        let mut run = Overlay::new(None, 0, 0);
        for i in 0..1000u64 {
            run.write(i, &[i as u8]);
        }
        assert_eq!(run.extent_count(), 1);
        assert_eq!(run.extent_ranges(), vec![0..1000]);
        let want: Vec<u8> = (0..1000u64).map(|i| i as u8).collect();
        assert_eq!(read(&run, &[], 0, 1000), want);
        assert_eq!(run.copy_out(10, 5), want[10..15].to_vec());
    }

    #[test]
    fn a_growing_extent_does_not_double_its_capacity() {
        // One extent that grows in order. Its cost (capacity) must follow its
        // length, so the dirty limit bounds the RAM and not 2x the limit.
        const MIB: usize = 1 << 20;
        let mut ov = Overlay::new(None, 0, 0);
        let piece = vec![7u8; MIB];
        for i in 0..65 {
            ov.write((i * MIB) as u64, &piece);
            let len = (i + 1) * MIB;
            assert_eq!(ov.extent_count(), 1);
            assert!(ov.cost() <= len + len / GROW_DIV + EXTENT_OVERHEAD, "{} MiB: cost {}", i + 1, ov.cost());
        }
        // Small in-order writes: the same bound.
        let mut small = Overlay::new(None, 0, 0);
        for i in 0..4096u64 {
            small.write(i * 4096, &[1; 4096]);
            let len = (i as usize + 1) * 4096;
            assert!(small.cost() <= len + len / GROW_DIV + EXTENT_OVERHEAD, "{len}: cost {}", small.cost());
        }
        assert_eq!(read(&small, &[], 0, 16 * MIB as u64), vec![1u8; 16 * MIB]);
    }

    /// A read asks the base only for the bytes that no extent covers (#108):
    /// a range under one extent reads no base, a range across two extents
    /// reads only the gaps, each of the right length.
    #[test]
    fn a_read_asks_the_base_only_for_the_gaps_between_extents() {
        let base: Vec<u8> = (0..64 * 1024u32).map(|i| (i * 7) as u8).collect();
        let mut ov = Overlay::new(None, base.len() as u64, base.len() as u64);
        ov.write(1000, &[1; 3000]);
        ov.write(10_000, &[2; 5000]);
        let mut model = base.clone();
        model[1000..4000].fill(1);
        model[10_000..15_000].fill(2);
        let mut asked: Vec<(u64, usize)> = Vec::new();
        let read_counted = |off: u64, len: u64, asked: &mut Vec<(u64, usize)>| {
            ov.view(off, len).read(|o, n| {
                asked.push((o, n));
                Ok(base[o as usize..o as usize + n].to_vec())
            }).unwrap()
        };
        assert_eq!(read_counted(1500, 2000, &mut asked), model[1500..3500]);
        assert!(asked.is_empty(), "a range under one extent reads no base: {asked:?}");
        assert_eq!(read_counted(0, 20_000, &mut asked), model[..20_000]);
        assert_eq!(asked, vec![(0, 1000), (4000, 6000), (15_000, 5000)]);
        asked.clear();
        assert_eq!(read_counted(12_000, 10_000, &mut asked), model[12_000..22_000]);
        assert_eq!(asked, vec![(15_000, 7000)]);
    }

    /// `take` frees at least what is asked (or everything), in sweep order,
    /// splits a long extent, and the taken pieces plus the rest are the same
    /// bytes as before.
    #[test]
    fn take_frees_what_is_asked_and_keeps_every_byte() {
        for seed in 1..=4u64 {
            let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut ov = Overlay::new(None, 0, 0);
            for _ in 0..300 {
                let off = r.below(8 << 20);
                let n = if r.below(10) == 0 { 1 + r.below(1 << 20) as usize } else { 1 + r.below(8192) as usize };
                ov.write(off, &r.bytes(n));
            }
            let size = ov.size;
            let whole = read(&ov, &[], 0, size);
            let mut pieces: Vec<(u64, Vec<u8>)> = Vec::new();
            let mut last_sweep = 0;
            while ov.extent_count() > 0 {
                let before = ov.cost();
                let free = 1 + r.below(512 << 10) as usize;
                let got = ov.take(free);
                assert!(!got.is_empty());
                assert!(before - ov.cost() >= free.min(before), "seed {seed}: freed {} of {free}", before - ov.cost());
                // A step takes at most about what is asked: no piece is much
                // longer than `free` plus one split margin.
                let taken: usize = got.iter().map(|(_, v)| v.len()).sum();
                let last = got.last().map_or(0, |(_, v)| v.len());
                assert!(taken - last < free, "seed {seed}: took {taken} bytes for {free}");
                assert!(last <= free.max(MIN_TAKE) + MIN_TAKE, "seed {seed}: a piece of {last} bytes for {free}");
                // The first piece starts at the sweep point of the step
                // before, or the sweep went back to 0.
                let first = got[0].0;
                assert!(first >= last_sweep || ov.extent_ranges().iter().all(|e| e.start < last_sweep),
                    "seed {seed}: step started at {first}, sweep was {last_sweep}");
                last_sweep = ov.sweep();
                pieces.extend(got);
            }
            let mut back = Overlay::new(None, 0, size);
            for (o, v) in pieces {
                back.put(o, v);
            }
            assert!(read(&back, &[], 0, size) == whole, "seed {seed}: bytes changed");
        }
    }

    /// Every change moves the generation; a copy does not (D-02).
    #[test]
    fn every_change_moves_gen_and_a_copy_does_not() {
        let mut ov = Overlay::new(None, 0, 0);
        let mut last = ov.generation();
        let moved = |ov: &Overlay, what: &str, last: &mut u64| {
            assert!(ov.generation() > *last, "{what} must move gen");
            *last = ov.generation();
        };
        ov.put(0, vec![1; 100 << 10]);
        moved(&ov, "put", &mut last);
        ov.write(200 << 10, &[2; 4096]);
        moved(&ov, "write", &mut last);
        ov.truncate(250 << 10);
        moved(&ov, "truncate", &mut last);
        ov.set_sweep(0);
        moved(&ov, "set_sweep", &mut last);
        let copied = ov.copy_step(4096);
        assert_eq!(ov.generation(), last, "copy_step must not move gen");
        let pieces: Vec<(u64, usize)> = copied.iter().map(|(o, v)| (*o, v.len())).collect();
        assert_eq!(ov.step_ranges(4096), pieces);
        ov.discard(&pieces);
        moved(&ov, "discard", &mut last);
        ov.take(4096);
        moved(&ov, "take", &mut last);
        let fresh = Overlay::new(None, 0, 0);
        assert!(fresh.generation() > last && ov.empty_like().generation() > fresh.generation(), "a new overlay starts past every gen");
    }

    /// `copy_step` returns what `take` takes, and `discard` of those pieces
    /// leaves the overlay as `take` leaves it (ranges, cost, sweep, bytes).
    #[test]
    fn a_copy_then_discard_at_one_gen_equals_take() {
        for seed in 1..=4u64 {
            let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let (mut a, mut b) = (Overlay::new(None, 0, 0), Overlay::new(None, 0, 0));
            for _ in 0..300 {
                let off = r.below(8 << 20);
                let n = if r.below(10) == 0 { 1 + r.below(1 << 20) as usize } else { 1 + r.below(8192) as usize };
                let data = r.bytes(n);
                a.write(off, &data);
                b.write(off, &data);
            }
            while a.extent_count() > 0 {
                let free = 1 + r.below(512 << 10) as usize;
                let g0 = b.generation();
                let copied = b.copy_step(free);
                assert_eq!(b.generation(), g0);
                let taken = a.take(free);
                assert!(copied == taken, "seed {seed}: copy_step differs from take");
                let pieces: Vec<(u64, usize)> = copied.iter().map(|(o, v)| (*o, v.len())).collect();
                b.discard(&pieces);
                assert_eq!(a.extent_ranges(), b.extent_ranges(), "seed {seed}");
                assert_eq!((a.cost(), a.sweep()), (b.cost(), b.sweep()), "seed {seed}");
                assert!(read(&a, &[], 0, a.size) == read(&b, &[], 0, b.size), "seed {seed}: bytes");
            }
        }
    }

    #[test]
    fn a_far_write_and_a_huge_extend_allocate_nothing_by_offset() {
        const TIB: u64 = 1 << 40;
        let mut ov = Overlay::new(None, 0, 0);
        ov.write(TIB - 4, &[1, 2, 3, 4]);
        assert_eq!(ov.size, TIB);
        assert!(ov.cost() < 1024, "cost {}", ov.cost());
        ov.truncate(TIB);
        let mut ext = Overlay::new(None, 0, 0);
        ext.truncate(TIB);
        assert!(ov.cost() < 1024 && ext.cost() < 1024, "costs {} {}", ov.cost(), ext.cost());
        assert_eq!(read(&ov, &[], TIB - 8, 100), vec![0, 0, 0, 0, 1, 2, 3, 4]);
        assert_eq!(read(&ext, &[], TIB / 2, 4), vec![0; 4]);
    }
}
