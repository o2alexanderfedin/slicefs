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
        merged.resize((stop - start) as usize, 0);
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
    /// zeros, then the extent parts on top. A base read of the wrong length
    /// is an error (a damaged base must not turn into zeros).
    pub fn read(
        self,
        mut base_read: impl FnMut(u64, usize) -> std::io::Result<Vec<u8>>,
    ) -> std::io::Result<Vec<u8>> {
        let n = (self.end - self.off) as usize;
        let base_end = self.end.min(self.visible);
        let mut out = if self.off < base_end {
            let want = (base_end - self.off) as usize;
            let got = base_read(self.off, want)?;
            if got.len() != want {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("overlay base read: {} of {} bytes at {}", got.len(), want, self.off),
                ));
            }
            got
        } else {
            Vec::new()
        };
        out.resize(n, 0);
        for (a, bytes) in self.parts {
            let at = (a - self.off) as usize;
            out[at..at + bytes.len()].copy_from_slice(&bytes);
        }
        Ok(out)
    }
}
