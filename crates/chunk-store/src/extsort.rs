//! External sort of fixed-size records in bounded RAM: the keys of the GC
//! mark walk (`ChunkStore::gc_sorter`, #207), the exact mark of a full GC
//! (see `ChunkStore::gc_finish_full`), and the refcount rows of
//! `slicefs rehash` (sorted by their new keys).
//!
//! Records go into a buffer of at most `SortCfg::chunk_records`; a full
//! buffer is sorted, deduplicated and written to a run file. `finish` merges
//! the runs, at most `SortCfg::fan_in` at a time (each input through a
//! [`READ_BUF`] reader), pass after pass, until one sorted file without
//! duplicates is left. RAM: one buffer, or `fan_in` read buffers, never the
//! record count. The files live in a [`TmpDir`], removed on drop (and at a
//! writable open of the store after a crash).

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Default bytes of records in the sort buffer (4 MiB).
pub const SORT_CHUNK_BYTES: usize = 4 << 20;

/// Default most runs that one merge reads at a time (open files).
pub const MERGE_FAN_IN: usize = 32;

/// The read buffer of each merge input, and the write buffer of a run.
pub const READ_BUF: usize = 64 << 10;

/// The sizes of an external sort.
#[derive(Clone, Copy, Debug)]
pub struct SortCfg {
    /// Records in the sort buffer (at least 1).
    pub chunk_records: usize,
    /// Most runs in one merge (at least 2).
    pub fan_in: usize,
}

/// A directory for temp files, removed with its files on drop.
pub struct TmpDir {
    path: PathBuf,
}

impl TmpDir {
    /// A new, empty directory below `parent` (made if missing).
    pub fn new(parent: &Path) -> io::Result<TmpDir> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(parent)?;
        let path = parent.join(format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path)?;
        Ok(TmpDir { path })
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// An external sorter of `N`-byte records (ordered as bytes).
pub struct Sorter<const N: usize> {
    dir: Arc<TmpDir>,
    /// Temp dirs of absorbed sorters (their runs live there).
    keep: Vec<Arc<TmpDir>>,
    name: &'static str,
    cfg: SortCfg,
    buf: Vec<[u8; N]>,
    runs: Vec<PathBuf>,
    next: u64,
}

impl<const N: usize> Sorter<N> {
    pub fn new(dir: Arc<TmpDir>, name: &'static str, cfg: SortCfg) -> Self {
        let cfg = SortCfg { chunk_records: cfg.chunk_records.max(1), fan_in: cfg.fan_in.max(2) };
        Sorter { dir, keep: Vec::new(), name, cfg, buf: Vec::new(), runs: Vec::new(), next: 0 }
    }

    fn new_path(&mut self) -> PathBuf {
        self.next += 1;
        self.dir.path.join(format!("{}-{:06}", self.name, self.next))
    }

    pub fn push(&mut self, r: [u8; N]) -> io::Result<()> {
        self.buf.push(r);
        if self.buf.len() >= self.cfg.chunk_records {
            self.spill()?;
        }
        Ok(())
    }

    /// Sort the buffer and write it as a run.
    fn spill(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        self.buf.sort_unstable();
        self.buf.dedup();
        let path = self.new_path();
        let mut w = BufWriter::with_capacity(READ_BUF, File::create(&path)?);
        for r in &self.buf {
            w.write_all(r)?;
        }
        w.into_inner().map_err(|e| e.into_error())?;
        self.runs.push(path);
        self.buf.clear();
        Ok(())
    }

    /// Take the records of `other` (its runs stay where they are).
    pub fn absorb(&mut self, mut other: Sorter<N>) -> io::Result<()> {
        other.spill()?;
        self.runs.append(&mut other.runs);
        self.keep.push(Arc::clone(&other.dir));
        self.keep.append(&mut other.keep);
        Ok(())
    }

    /// One sorted file of every record pushed, without duplicates.
    pub fn finish(mut self) -> io::Result<SortedFile<N>> {
        self.spill()?;
        let mut passes = 0;
        while self.runs.len() > 1 {
            let inputs = std::mem::take(&mut self.runs);
            for group in inputs.chunks(self.cfg.fan_in) {
                if group.len() == 1 {
                    self.runs.push(group[0].clone());
                    continue;
                }
                let out = self.new_path();
                merge::<N>(group, &out)?;
                for p in group {
                    let _ = std::fs::remove_file(p);
                }
                self.runs.push(out);
            }
            passes += 1;
        }
        let path = match self.runs.pop() {
            Some(p) => p,
            None => {
                let p = self.new_path();
                File::create(&p)?;
                p
            }
        };
        let len = std::fs::metadata(&path)?.len() / N as u64;
        let mut keep = std::mem::take(&mut self.keep);
        keep.push(Arc::clone(&self.dir));
        Ok(SortedFile { file: File::open(&path)?, path, len, passes, _dirs: keep })
    }
}

/// Merge the sorted runs `inputs` into `out`, without duplicates.
fn merge<const N: usize>(inputs: &[PathBuf], out: &Path) -> io::Result<()> {
    let mut readers = Vec::with_capacity(inputs.len());
    for p in inputs {
        let f = File::open(p)?;
        let n = f.metadata()?.len() / N as u64;
        readers.push(RecReader::<N> { r: BufReader::with_capacity(READ_BUF, f), left: n });
    }
    let mut heap = BinaryHeap::with_capacity(readers.len());
    for (i, r) in readers.iter_mut().enumerate() {
        if let Some(rec) = r.next_rec()? {
            heap.push(Reverse((rec, i)));
        }
    }
    let mut w = BufWriter::with_capacity(READ_BUF, File::create(out)?);
    let mut last: Option<[u8; N]> = None;
    while let Some(Reverse((rec, i))) = heap.pop() {
        if last != Some(rec) {
            w.write_all(&rec)?;
            last = Some(rec);
        }
        if let Some(next) = readers[i].next_rec()? {
            heap.push(Reverse((next, i)));
        }
    }
    w.into_inner().map_err(|e| e.into_error())?;
    Ok(())
}

/// A sorted file of distinct `N`-byte records (removed with its temp dir).
pub struct SortedFile<const N: usize> {
    path: PathBuf,
    file: File,
    len: u64,
    passes: u32,
    _dirs: Vec<Arc<TmpDir>>,
}

impl<const N: usize> SortedFile<N> {
    /// The records.
    #[cfg(test)]
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// The merge passes that made the file.
    pub fn passes(&self) -> u32 {
        self.passes
    }

    /// A stream of the records in order.
    pub fn reader(&self) -> io::Result<RecReader<N>> {
        Ok(RecReader { r: BufReader::with_capacity(READ_BUF, File::open(&self.path)?), left: self.len })
    }

    /// Is `r` in the file? A binary search with one read per step.
    pub fn contains(&self, r: &[u8; N]) -> io::Result<bool> {
        let (mut lo, mut hi) = (0u64, self.len);
        let mut rec = [0u8; N];
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            self.file.read_exact_at(&mut rec, mid * N as u64)?;
            match rec.cmp(r) {
                std::cmp::Ordering::Equal => return Ok(true),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        Ok(false)
    }
}

/// Reads the records of a run in order.
pub struct RecReader<const N: usize> {
    r: BufReader<File>,
    left: u64,
}

impl<const N: usize> RecReader<N> {
    pub fn next_rec(&mut self) -> io::Result<Option<[u8; N]>> {
        if self.left == 0 {
            return Ok(None);
        }
        let mut rec = [0u8; N];
        self.r.read_exact(&mut rec)?;
        self.left -= 1;
        Ok(Some(rec))
    }
}

/// A position in a sorted stream, for a merge join with another sorted
/// stream.
pub(crate) struct Cursor<const N: usize> {
    r: RecReader<N>,
    cur: Option<[u8; N]>,
}

impl<const N: usize> Cursor<N> {
    pub fn new(mut r: RecReader<N>) -> io::Result<Self> {
        let cur = r.next_rec()?;
        Ok(Cursor { r, cur })
    }

    /// The current record (`None` at the end).
    pub fn peek(&self) -> Option<&[u8; N]> {
        self.cur.as_ref()
    }

    pub fn advance(&mut self) -> io::Result<()> {
        self.cur = self.r.next_rec()?;
        Ok(())
    }

    /// Move past the records less than `key`; true if the next one is `key`.
    /// The keys of the calls must not go down.
    pub fn seek(&mut self, key: &[u8; N]) -> io::Result<bool> {
        while matches!(&self.cur, Some(c) if c < key) {
            self.advance()?;
        }
        Ok(self.cur.as_ref() == Some(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(i: u64) -> [u8; 8] {
        // A scrambled order, with duplicates.
        (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) % 500).to_be_bytes()
    }

    #[test]
    fn tiny_chunks_and_fan_in_take_several_merge_passes() {
        let parent = tempfile::TempDir::new().unwrap();
        let dir = Arc::new(TmpDir::new(parent.path()).unwrap());
        let mut s = Sorter::<8>::new(dir, "t", SortCfg { chunk_records: 4, fan_in: 2 });
        for i in 0..2000 {
            s.push(rec(i)).unwrap();
        }
        let f = s.finish().unwrap();
        // 500 runs at fan-in 2: 9 passes.
        assert!(f.passes() >= 2, "{} passes", f.passes());
        let mut want: Vec<_> = (0..2000).map(rec).collect();
        want.sort_unstable();
        want.dedup();
        let mut r = f.reader().unwrap();
        let mut got = Vec::new();
        while let Some(x) = r.next_rec().unwrap() {
            got.push(x);
        }
        assert_eq!(got, want);
        assert!(want.iter().all(|w| f.contains(w).unwrap()));
        assert!(!f.contains(&600u64.to_be_bytes()).unwrap());
        // Only the result is left; the runs of the passes are gone.
        let tmp = std::fs::read_dir(parent.path()).unwrap().next().unwrap().unwrap().path();
        assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 1);
        drop(f);
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0, "the temp dir is removed");
    }

    #[test]
    fn an_absorbed_sorter_joins_and_a_cursor_seeks() {
        let parent = tempfile::TempDir::new().unwrap();
        let cfg = SortCfg { chunk_records: 3, fan_in: 3 };
        let mut a = Sorter::<8>::new(Arc::new(TmpDir::new(parent.path()).unwrap()), "a", cfg);
        let mut b = Sorter::<8>::new(Arc::new(TmpDir::new(parent.path()).unwrap()), "b", cfg);
        for i in (0..40u64).step_by(2) {
            a.push(i.to_be_bytes()).unwrap();
        }
        for i in (1..40u64).step_by(4) {
            b.push(i.to_be_bytes()).unwrap();
        }
        a.absorb(b).unwrap();
        let f = a.finish().unwrap();
        assert_eq!(f.len(), 30);
        let mut c = Cursor::new(f.reader().unwrap()).unwrap();
        for i in 0..40u64 {
            assert_eq!(c.seek(&i.to_be_bytes()).unwrap(), i % 2 == 0 || i % 4 == 1, "{i}");
        }
        drop(f);
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
    }
}
