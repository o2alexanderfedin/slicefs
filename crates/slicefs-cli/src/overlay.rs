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

use slicefs_traits::key::ObjectKey;

/// Bytes charged per extent on top of its buffer (map node and Vec header),
/// so that many one-byte writes cannot grow the map without a cost.
pub const EXTENT_OVERHEAD: usize = 64;

/// A growing extent gets at most 1/GROW_DIV of its length as spare capacity
/// (not the 2x of `Vec` doubling), so its cost stays near its length.
pub const GROW_DIV: usize = 8;

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
}

impl Overlay {
    /// A clean overlay on `base` (of `base_len` bytes) with logical size `size`.
    pub fn new(base: Option<ObjectKey>, base_len: u64, size: u64) -> Self {
        Overlay { base, base_len, visible: base_len, size, extents: BTreeMap::new(), cost: 0 }
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
        let mut read_counted = |off: u64, len: u64, asked: &mut Vec<(u64, usize)>| {
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
