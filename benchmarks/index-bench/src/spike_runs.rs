//! Spike backend `spike-runs` (Phase 17 plan 01): an index-only prototype of
//! the locked on-disk index layout. Throwaway code on the spike branch.
//!
//! - memtable: a BTreeMap from the 33-byte ID to a 25-byte Loc, flushed to a
//!   run at `FLUSH_AT` entries;
//! - run file `DIR/index/NNNN.run`: 4 KiB pages of 58-byte entries sorted by
//!   the full ID, then a fence table (8 bytes per page: digest bytes 0..8 of
//!   the first entry, big-endian), then a blocked Bloom filter (512-bit
//!   blocks, 7 probes, about 10 bits per key, positions from digest bytes, no
//!   hashing), then a footer;
//! - open reads only the footer, the fence table and the filter of each run;
//! - lookup: memtable, then runs newest first: filter, fence search, one pread
//!   of one page;
//! - close: flush, merge all runs into one, write the exact object count.
//!
//! No pack bytes are written: put only hashes the data and updates the index.

use std::cell::RefCell;
use std::collections::{BTreeMap, BinaryHeap};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use chunk_store::{ObjectId, DEFAULT_HASHER};

use crate::backend::{Backend, Store};

const ID: usize = 33;
const LOC: usize = 25;
const ENTRY: usize = ID + LOC; // 58
const PAGE: usize = 4096;
const PER_PAGE: usize = PAGE / ENTRY; // 70
const FLUSH_AT: usize = 1_000_000;
const BITS_PER_KEY: u64 = 10;
const BLOCK_BITS: u64 = 512;
const BLOCK_WORDS: usize = (BLOCK_BITS / 64) as usize;
const PROBES: u64 = 7;
const MAGIC: [u8; 8] = *b"SPKRUN01";
const FOOTER: usize = 48;
const COUNT_FILE: &str = "COUNT";

type Key = [u8; ID];
type Loc = [u8; LOC];

/// Digest bytes `off..off+8` (the tag byte is ID byte 0), big-endian.
fn dig(id: &Key, off: usize) -> u64 {
    u64::from_be_bytes(id[1 + off..9 + off].try_into().unwrap())
}

fn bloom_block(id: &Key, nblocks: u64) -> usize {
    ((dig(id, 0) as u128 * nblocks as u128) >> 64) as usize
}

fn bloom_bits(id: &Key) -> impl Iterator<Item = u64> {
    let h1 = dig(id, 8);
    let h2 = dig(id, 16) | 1;
    (0..PROBES).map(move |j| h1.wrapping_add(j.wrapping_mul(h2)) & (BLOCK_BITS - 1))
}

fn nblocks_for(n: u64) -> u64 {
    (n * BITS_PER_KEY).div_ceil(BLOCK_BITS).max(1)
}

fn io(e: std::io::Error) -> String {
    e.to_string()
}

/// How the page is found and read (spike-runs-mmap variants; from the
/// environment, so all variants open the same spike-runs store).
#[derive(Clone, Copy, Debug)]
pub struct Cfg {
    /// Map each sealed run read-only and search the page in the mapping.
    mmap: bool,
    /// Check the page CRC32C on each page search (variant B2).
    crc: bool,
    /// Fence search: 0 binary, 1 interpolation, 2 Eytzinger.
    fence: u8,
    /// Search inside the page: interpolation (else binary).
    page_interp: bool,
    /// madvise(MADV_RANDOM) on each mapping.
    random: bool,
}

impl Cfg {
    fn pread() -> Cfg {
        Cfg { mmap: false, crc: false, fence: 0, page_interp: false, random: false }
    }
    fn from_env() -> Cfg {
        let v = |k: &str| std::env::var(k).unwrap_or_default();
        Cfg {
            mmap: true,
            crc: v("SPIKE_CRC") == "1",
            fence: match v("SPIKE_FENCE").as_str() {
                "interp" => 1,
                "eytz" => 2,
                _ => 0,
            },
            page_interp: v("SPIKE_PAGE") == "interp",
            random: v("SPIKE_ADVISE") == "random",
        }
    }
    fn label(&self) -> String {
        format!(
            "{}{}-fence_{}-page_{}{}",
            if self.mmap { "mmap" } else { "pread" },
            if self.crc { "-crc" } else { "" },
            ["bin", "interp", "eytz"][self.fence as usize],
            if self.page_interp { "interp" } else { "bin" },
            if self.random { "-random" } else { "" }
        )
    }
}

/// A read-only shared mapping of the page area of a sealed run.
struct Map {
    ptr: *const u8,
    len: usize,
}

impl Map {
    fn new(file: &File, len: usize, random: bool) -> Result<Map, String> {
        use std::os::unix::io::AsRawFd;
        if len == 0 {
            return Ok(Map { ptr: std::ptr::null(), len: 0 });
        }
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0)
        };
        if p == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }
        if random {
            unsafe { libc::madvise(p, len, libc::MADV_RANDOM) };
        }
        Ok(Map { ptr: p as *const u8, len })
    }
    fn bytes(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
    /// Resident pages of the mapping (mincore: in the page cache).
    fn resident_pages(&self) -> u64 {
        if self.len == 0 {
            return 0;
        }
        let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let n = self.len.div_ceil(ps);
        let mut v = vec![0u8; n];
        let r = unsafe { libc::mincore(self.ptr as *mut libc::c_void as _, self.len, v.as_mut_ptr() as _) };
        if r != 0 {
            return u64::MAX;
        }
        v.iter().filter(|&&b| b & 1 != 0).count() as u64
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        if self.len != 0 {
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
        }
    }
}

/// A run file, opened: only fences and filter in RAM.
struct Run {
    path: PathBuf,
    file: File,
    entries: u64,
    fences: Vec<u64>,
    filter: Vec<u64>,
    nblocks: u64,
    cfg: Cfg,
    map: Option<Map>,
    /// Eytzinger (BFS) copy of the fences, 1-based, and the sorted index of
    /// each BFS slot; only for fence == 2.
    eytz: Vec<u64>,
    eytz_idx: Vec<u32>,
}

/// Fill `out` (1-based BFS order) from sorted `src`.
fn eytz_fill(src: &[u64], out: &mut [u64], idx: &mut [u32], i: &mut usize, k: usize) {
    if k < out.len() {
        eytz_fill(src, out, idx, i, 2 * k);
        out[k] = src[*i];
        idx[k] = *i as u32;
        *i += 1;
        eytz_fill(src, out, idx, i, 2 * k + 1);
    }
}

/// Read `out.len()` u64 words straight into their final buffer (no temporary
/// copy, so no freed buffer stays in the RSS).
fn read_words(file: &File, off: u64, out: &mut [u64]) -> Result<(), String> {
    let bytes = unsafe { std::slice::from_raw_parts_mut(out.as_mut_ptr() as *mut u8, out.len() * 8) };
    file.read_exact_at(bytes, off).map_err(io)?;
    for w in out.iter_mut() {
        *w = u64::from_le(*w);
    }
    Ok(())
}

impl Run {
    fn open(path: PathBuf, cfg: Cfg) -> Result<Run, String> {
        let file = File::open(&path).map_err(io)?;
        let len = file.metadata().map_err(io)?.len();
        let mut f = [0u8; FOOTER];
        file.read_exact_at(&mut f, len - FOOTER as u64).map_err(io)?;
        if f[0..8] != MAGIC {
            return Err(format!("bad run footer in {}", path.display()));
        }
        let g = |i: usize| u64::from_le_bytes(f[8 + i * 8..16 + i * 8].try_into().unwrap());
        let (entries, pages, fence_off, filter_off, nblocks) = (g(0), g(1), g(2), g(3), g(4));
        let mut fences = vec![0u64; pages as usize];
        read_words(&file, fence_off, &mut fences)?;
        let mut filter = vec![0u64; nblocks as usize * BLOCK_WORDS];
        read_words(&file, filter_off, &mut filter)?;
        let map = if cfg.mmap { Some(Map::new(&file, pages as usize * PAGE, cfg.random)?) } else { None };
        let (mut eytz, mut eytz_idx) = (Vec::new(), Vec::new());
        if cfg.fence == 2 {
            eytz = vec![0u64; fences.len() + 1];
            eytz_idx = vec![0u32; fences.len() + 1];
            let mut i = 0;
            eytz_fill(&fences, &mut eytz, &mut eytz_idx, &mut i, 1);
        }
        Ok(Run { path, file, entries, fences, filter, nblocks, cfg, map, eytz, eytz_idx })
    }

    /// Map the run again (drop the process mapping); on Linux also ask the
    /// kernel to drop the clean file pages from the page cache.
    fn make_cold(&mut self) -> Result<(), String> {
        let len = self.fences.len() * PAGE;
        self.map = None;
        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::io::AsRawFd;
            libc::posix_fadvise(self.file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        }
        self.map = Some(Map::new(&self.file, len, self.cfg.random)?);
        Ok(())
    }

    /// Index of the page that can hold prefix `p` (the number of fences
    /// <= p, minus one); None if p is below the first fence. `probes` counts
    /// the fence reads.
    #[inline]
    fn find_page(&self, p: u64, probes: &mut u32) -> Option<usize> {
        let f = &self.fences;
        let n = f.len();
        match self.cfg.fence {
            1 => {
                // Interpolation search: the keys are uniform digests.
                // Invariant: fences[..lo] <= p, fences[hi..] > p.
                let (mut lo, mut hi) = (0usize, n);
                let (mut vlo, mut vhi) = (0u64, u64::MAX);
                while hi - lo > 8 {
                    // f64, not u128: a u128 division is a slow library call.
                    let g = lo + (p.saturating_sub(vlo) as f64 / ((vhi - vlo) as f64 + 1.0) * (hi - lo) as f64) as usize;
                    let g = g.min(hi - 1);
                    *probes += 1;
                    if f[g] <= p {
                        lo = g + 1;
                        vlo = f[g];
                    } else {
                        hi = g;
                        vhi = f[g];
                    }
                }
                while lo < hi && f[lo] <= p {
                    *probes += 1;
                    lo += 1;
                }
                if lo < hi {
                    *probes += 1;
                }
                lo.checked_sub(1)
            }
            2 => {
                // Eytzinger: the first slot with a value > p.
                let e = &self.eytz;
                let mut k = 1usize;
                while k < e.len() {
                    #[cfg(target_arch = "x86_64")]
                    unsafe {
                        let pf = (8 * k).min(e.len() - 1);
                        std::arch::x86_64::_mm_prefetch(e.as_ptr().add(pf) as *const i8, std::arch::x86_64::_MM_HINT_T0);
                    }
                    *probes += 1;
                    k = 2 * k + (e[k] <= p) as usize;
                }
                k >>= k.trailing_ones() + 1;
                let idx = if k == 0 { n } else { self.eytz_idx[k] as usize };
                idx.checked_sub(1)
            }
            _ => {
                let mut pb = 0;
                let (mut lo, mut hi) = (0usize, n);
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    pb += 1;
                    if f[mid] <= p {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                *probes += pb;
                lo.checked_sub(1)
            }
        }
    }

    /// Search one page in the mapping. `probes` counts the entry reads.
    #[inline]
    fn search_mapped(&self, page: usize, id: &Key, probes: &mut u32) -> Option<Loc> {
        let m = self.map.as_ref().unwrap().bytes();
        let pg = &m[page * PAGE..(page + 1) * PAGE];
        if self.cfg.crc {
            let want = u32::from_le_bytes(pg[PAGE - 4..].try_into().unwrap());
            if crc32c::crc32c(&pg[..PAGE - 4]) != want {
                return None; // a real store reports damage here
            }
        }
        let cnt = self.page_entries(page);
        let b = &pg[..cnt * ENTRY];
        let key = |i: usize| &b[i * ENTRY..i * ENTRY + ID];
        let loc = |i: usize| -> Loc { b[i * ENTRY + ID..(i + 1) * ENTRY].try_into().unwrap() };
        if self.cfg.page_interp && cnt > 0 {
            let p = dig(id, 0);
            let lo_v = self.fences[page];
            let hi_v = self.fences.get(page + 1).copied().unwrap_or(u64::MAX);
            let mut i = (p.saturating_sub(lo_v) as f64 / (hi_v.saturating_sub(lo_v) as f64 + 1.0) * cnt as f64) as usize;
            i = i.min(cnt - 1);
            *probes += 1;
            match key(i).cmp(&id[..]) {
                std::cmp::Ordering::Equal => return Some(loc(i)),
                std::cmp::Ordering::Less => {
                    i += 1;
                    while i < cnt {
                        *probes += 1;
                        match key(i).cmp(&id[..]) {
                            std::cmp::Ordering::Equal => return Some(loc(i)),
                            std::cmp::Ordering::Greater => return None,
                            _ => i += 1,
                        }
                    }
                }
                std::cmp::Ordering::Greater => {
                    while i > 0 {
                        i -= 1;
                        *probes += 1;
                        match key(i).cmp(&id[..]) {
                            std::cmp::Ordering::Equal => return Some(loc(i)),
                            std::cmp::Ordering::Less => return None,
                            _ => {}
                        }
                    }
                }
            }
            return None;
        }
        let (mut lo, mut hi) = (0, cnt);
        while lo < hi {
            let mid = (lo + hi) / 2;
            *probes += 1;
            match key(mid).cmp(&id[..]) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(loc(mid)),
            }
        }
        None
    }

    fn may_contain(&self, id: &Key) -> bool {
        let b = bloom_block(id, self.nblocks) * BLOCK_WORDS;
        bloom_bits(id).all(|bit| self.filter[b + (bit / 64) as usize] & (1 << (bit % 64)) != 0)
    }

    fn page_entries(&self, page: usize) -> usize {
        (self.entries as usize - page * PER_PAGE).min(PER_PAGE)
    }

    fn search_page(&self, page: usize, id: &Key, buf: &mut [u8; PAGE]) -> Option<Loc> {
        let cnt = self.page_entries(page);
        let b = &mut buf[..cnt * ENTRY];
        self.file.read_exact_at(b, (page * PAGE) as u64).ok()?;
        let (mut lo, mut hi) = (0, cnt);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let e = &b[mid * ENTRY..mid * ENTRY + ID];
            match e.cmp(&id[..]) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    return Some(b[mid * ENTRY + ID..(mid + 1) * ENTRY].try_into().unwrap());
                }
            }
        }
        None
    }

    fn get(&self, id: &Key) -> Option<Loc> {
        if !self.may_contain(id) {
            return None;
        }
        if self.map.is_some() {
            let (mut fp, mut pp) = (0, 0);
            return self.get_mapped(id, &mut fp, &mut pp);
        }
        let p = dig(id, 0);
        let idx = self.fences.partition_point(|&f| f <= p);
        if idx == 0 {
            return None;
        }
        let page = idx - 1;
        let mut buf = [0u8; PAGE];
        if let Some(l) = self.search_page(page, id, &mut buf) {
            return Some(l);
        }
        // An 8-byte prefix equal to the fence: the key can end the page before.
        if page > 0 && self.fences[page] == p {
            return self.search_page(page - 1, id, &mut buf);
        }
        None
    }

    /// Mapped lookup after the filter. `fp` / `pp` count fence and entry probes.
    #[inline]
    fn get_mapped(&self, id: &Key, fp: &mut u32, pp: &mut u32) -> Option<Loc> {
        let p = dig(id, 0);
        let page = self.find_page(p, fp)?;
        if let Some(l) = self.search_mapped(page, id, pp) {
            return Some(l);
        }
        if page > 0 && self.fences[page] == p {
            return self.search_mapped(page - 1, id, pp);
        }
        None
    }
}

/// Write a run of `n` sorted entries to `path` (fsync), then open it.
fn write_run(path: PathBuf, n: u64, entries: impl Iterator<Item = (Key, Loc)>, cfg: Cfg) -> Result<Run, String> {
    let tmp = path.with_extension("tmp");
    let file = File::create(&tmp).map_err(io)?;
    let mut w = BufWriter::with_capacity(1 << 20, &file);
    let nblocks = nblocks_for(n);
    let mut filter = vec![0u64; nblocks as usize * BLOCK_WORDS];
    let mut fences = Vec::with_capacity((n as usize).div_ceil(PER_PAGE));
    let mut in_page = 0;
    let mut written = 0u64;
    // Each page: the entries, zero padding, then the CRC32C of the first
    // PAGE - 4 bytes in its last 4 bytes.
    let mut page_buf = Vec::with_capacity(PAGE);
    let end_page = |w: &mut BufWriter<&File>, pb: &mut Vec<u8>| -> Result<(), String> {
        pb.resize(PAGE - 4, 0);
        let c = crc32c::crc32c(pb);
        w.write_all(pb).map_err(io)?;
        w.write_all(&c.to_le_bytes()).map_err(io)?;
        pb.clear();
        Ok(())
    };
    for (k, l) in entries {
        if in_page == 0 {
            fences.push(dig(&k, 0));
        }
        let b = bloom_block(&k, nblocks) * BLOCK_WORDS;
        for bit in bloom_bits(&k) {
            filter[b + (bit / 64) as usize] |= 1 << (bit % 64);
        }
        page_buf.extend_from_slice(&k);
        page_buf.extend_from_slice(&l);
        written += 1;
        in_page += 1;
        if in_page == PER_PAGE {
            end_page(&mut w, &mut page_buf)?;
            in_page = 0;
        }
    }
    assert_eq!(written, n);
    if in_page != 0 {
        end_page(&mut w, &mut page_buf)?;
    }
    let pages = fences.len() as u64;
    let fence_off = pages * PAGE as u64;
    for f in &fences {
        w.write_all(&f.to_le_bytes()).map_err(io)?;
    }
    let filter_off = fence_off + pages * 8;
    for x in &filter {
        w.write_all(&x.to_le_bytes()).map_err(io)?;
    }
    let mut foot = Vec::with_capacity(FOOTER);
    foot.extend_from_slice(&MAGIC);
    for v in [n, pages, fence_off, filter_off, nblocks] {
        foot.extend_from_slice(&v.to_le_bytes());
    }
    w.write_all(&foot).map_err(io)?;
    w.flush().map_err(io)?;
    drop(w);
    file.sync_all().map_err(io)?;
    std::fs::rename(&tmp, &path).map_err(io)?;
    Run::open(path, cfg)
}

/// Sequential reader of the entries of a run file.
struct RunReader {
    r: BufReader<File>,
    left: u64,
    in_page: usize,
}

impl RunReader {
    fn new(run: &Run) -> Result<Self, String> {
        let f = File::open(&run.path).map_err(io)?;
        Ok(RunReader { r: BufReader::with_capacity(1 << 20, f), left: run.entries, in_page: 0 })
    }
    fn next(&mut self) -> Result<Option<(Key, Loc)>, String> {
        if self.left == 0 {
            return Ok(None);
        }
        let mut e = [0u8; ENTRY];
        self.r.read_exact(&mut e).map_err(io)?;
        self.left -= 1;
        self.in_page += 1;
        if self.in_page == PER_PAGE {
            let mut pad = [0u8; PAGE - PER_PAGE * ENTRY];
            self.r.read_exact(&mut pad).map_err(io)?;
            self.in_page = 0;
        }
        Ok(Some((e[..ID].try_into().unwrap(), e[ID..].try_into().unwrap())))
    }
}

struct State {
    dir: PathBuf,
    mem: BTreeMap<Key, Loc>,
    /// Oldest first.
    runs: Vec<Run>,
    next_run: u64,
    count: u64,
    cfg: Cfg,
}

impl State {
    fn index_dir(&self) -> PathBuf {
        self.dir.join("index")
    }

    fn run_path(&mut self) -> PathBuf {
        let p = self.index_dir().join(format!("{:016x}.run", self.next_run));
        self.next_run += 1;
        p
    }

    fn get(&self, id: &Key) -> Option<Loc> {
        if let Some(l) = self.mem.get(id) {
            return Some(*l);
        }
        self.runs.iter().rev().find_map(|r| r.get(id))
    }

    fn flush(&mut self) -> Result<(), String> {
        if self.mem.is_empty() {
            return Ok(());
        }
        let mem = std::mem::take(&mut self.mem);
        let path = self.run_path();
        let run = write_run(path, mem.len() as u64, mem.into_iter(), self.cfg)?;
        self.runs.push(run);
        Ok(())
    }

    /// Merge all runs into one (newest wins on equal keys).
    fn merge_all(&mut self) -> Result<(), String> {
        if self.runs.len() <= 1 {
            return Ok(());
        }
        let mut readers = self.runs.iter().map(RunReader::new).collect::<Result<Vec<_>, _>>()?;
        let total: u64 = self.runs.iter().map(|r| r.entries).sum();
        // Min-heap on (key, newest first).
        let mut heap = BinaryHeap::new();
        for (i, r) in readers.iter_mut().enumerate() {
            if let Some((k, l)) = r.next()? {
                heap.push(std::cmp::Reverse((k, usize::MAX - i, l)));
            }
        }
        let mut err = None;
        let mut last: Option<Key> = None;
        // Stream into the writer through an iterator; dedup equal keys.
        let iter = std::iter::from_fn(|| loop {
            let std::cmp::Reverse((k, inv, l)) = heap.pop()?;
            let i = usize::MAX - inv;
            match readers[i].next() {
                Ok(Some((k2, l2))) => heap.push(std::cmp::Reverse((k2, usize::MAX - i, l2))),
                Ok(None) => {}
                Err(e) => {
                    err = Some(e);
                    return None;
                }
            }
            if last == Some(k) {
                continue;
            }
            last = Some(k);
            return Some((k, l));
        });
        // Duplicates never happen here (put checks every run), so the count
        // is the sum; a duplicate would make write_run's assert fail.
        let path = self.run_path();
        let merged = write_run(path, total, iter, self.cfg)?;
        if let Some(e) = err {
            return Err(e);
        }
        for r in self.runs.drain(..) {
            std::fs::remove_file(&r.path).map_err(io)?;
        }
        self.runs.push(merged);
        Ok(())
    }
}

pub struct SpikeRunsBackend {
    pub mmap: bool,
}

impl SpikeRunsBackend {
    fn cfg(&self) -> Cfg {
        if self.mmap {
            Cfg::from_env()
        } else {
            Cfg::pread()
        }
    }
}

struct Handle(RefCell<State>);

impl Store for Handle {
    fn put(&self, data: &[u8]) -> Result<(), String> {
        let id: ObjectId = DEFAULT_HASHER.id(data);
        let mut s = self.0.borrow_mut();
        if s.get(&id.0).is_some() {
            return Ok(());
        }
        let i = s.count;
        let mut l = [0u8; LOC];
        l[0..8].copy_from_slice(&(i / 1_240_000 + 1).to_le_bytes());
        l[8..16].copy_from_slice(&(14 + (i % 1_240_000) * 54).to_le_bytes());
        l[16..20].copy_from_slice(&8u32.to_le_bytes());
        l[20..24].copy_from_slice(&8u32.to_le_bytes());
        s.mem.insert(id.0, l);
        s.count += 1;
        if s.mem.len() >= FLUSH_AT {
            s.flush()?;
        }
        Ok(())
    }
    fn contains(&self, id: &ObjectId) -> bool {
        self.0.borrow().get(&id.0).is_some()
    }
    fn objects(&self) -> u64 {
        self.0.borrow().count
    }
    /// The number of runs (the "packs" output key of the bench).
    fn packs(&self) -> u64 {
        self.0.borrow().runs.len() as u64
    }
    fn variant(&self) -> String {
        self.0.borrow().cfg.label()
    }
    /// SPIKE_COLD=1: map every run again before the hit round (and on Linux
    /// drop its clean pages from the page cache).
    fn before_hits(&self) -> Vec<(&'static str, String)> {
        let mut s = self.0.borrow_mut();
        if std::env::var("SPIKE_COLD").as_deref() != Ok("1") || !s.cfg.mmap {
            return vec![];
        }
        let before: u64 = s.runs.iter().map(|r| r.map.as_ref().map_or(0, |m| m.resident_pages())).sum();
        for r in s.runs.iter_mut() {
            r.make_cold().expect("remap");
        }
        let after: u64 = s.runs.iter().map(|r| r.map.as_ref().map_or(0, |m| m.resident_pages())).sum();
        vec![("cold_incore_pages_before", before.to_string()), ("cold_incore_pages_after", after.to_string())]
    }
    fn after_hits(&self) -> Vec<(&'static str, String)> {
        let s = self.0.borrow();
        let res: u64 = s.runs.iter().map(|r| r.map.as_ref().map_or(0, |m| m.resident_pages())).sum();
        let pages: u64 = s.runs.iter().map(|r| r.fences.len() as u64).sum();
        vec![("map_incore_pages", res.to_string()), ("run_pages", pages.to_string())]
    }
    /// The cost of the parts of one hit: each prefix of the path timed alone
    /// over all `ids` (one run, empty memtable: the state after open).
    fn breakdown(&self, ids: &[ObjectId]) -> Vec<(&'static str, String)> {
        use std::hint::black_box;
        let s = self.0.borrow();
        if !s.cfg.mmap || s.runs.len() != 1 || !s.mem.is_empty() || ids.is_empty() {
            return vec![];
        }
        let r = &s.runs[0];
        let n = ids.len() as f64;
        let time = |f: &mut dyn FnMut() -> u64| {
            let t = std::time::Instant::now();
            let c = crate::cpu_secs();
            black_box(f());
            (t.elapsed().as_secs_f64() * 1e9 / n, (crate::cpu_secs() - c) * 1e9 / n)
        };
        let m = r.map.as_ref().unwrap().bytes();
        let mut out = vec![];
        for _round in 0..2 {
            // Round 0 warms the caches; round 1 is the result.
            let bloom = time(&mut || ids.iter().filter(|id| r.may_contain(&black_box(id).0)).count() as u64);
            let fence = time(&mut || {
                let mut acc = 0u64;
                for id in ids {
                    let mut fp = 0;
                    acc = acc.wrapping_add(r.find_page(dig(&black_box(id).0, 0), &mut fp).unwrap_or(0) as u64);
                }
                acc
            });
            let touch = time(&mut || {
                let mut acc = 0u64;
                for id in ids {
                    let mut fp = 0;
                    let pg = r.find_page(dig(&black_box(id).0, 0), &mut fp).unwrap_or(0);
                    let b = unsafe { std::ptr::read_volatile(m.as_ptr().add(pg * PAGE + (PER_PAGE / 2) * ENTRY)) };
                    acc = acc.wrapping_add(b as u64);
                }
                acc
            });
            let search = time(&mut || {
                let mut acc = 0u64;
                for id in ids {
                    let (mut fp, mut pp) = (0, 0);
                    acc = acc.wrapping_add(r.get_mapped(&black_box(id).0, &mut fp, &mut pp).is_some() as u64);
                }
                acc
            });
            let full = time(&mut || ids.iter().filter(|id| r.get(&black_box(id).0).is_some()).count() as u64);
            out = vec![
                ("bd_bloom_ns", crate::fmt_f(bloom.0)),
                ("bd_fence_ns", crate::fmt_f(fence.0)),
                ("bd_fence_touch_ns", crate::fmt_f(touch.0)),
                ("bd_fence_touch_search_ns", crate::fmt_f(search.0)),
                ("bd_full_get_ns", crate::fmt_f(full.0)),
                ("bd_bloom_cpu_ns", crate::fmt_f(bloom.1)),
                ("bd_fence_cpu_ns", crate::fmt_f(fence.1)),
                ("bd_fence_touch_cpu_ns", crate::fmt_f(touch.1)),
                ("bd_fence_touch_search_cpu_ns", crate::fmt_f(search.1)),
                ("bd_full_get_cpu_ns", crate::fmt_f(full.1)),
            ];
        }
        let (mut fps, mut pps) = (0u64, 0u64);
        for id in ids {
            let (mut fp, mut pp) = (0, 0);
            r.get_mapped(&id.0, &mut fp, &mut pp);
            fps += fp as u64;
            pps += pp as u64;
        }
        out.push(("bd_fence_probes_avg", crate::fmt_f(fps as f64 / n)));
        out.push(("bd_page_probes_avg", crate::fmt_f(pps as f64 / n)));
        out
    }
    fn close(&self) -> Result<(), String> {
        let mut s = self.0.borrow_mut();
        s.flush()?;
        s.merge_all()?;
        let count = s.count;
        std::fs::write(s.index_dir().join(COUNT_FILE), count.to_string()).map_err(io)?;
        File::open(s.index_dir()).and_then(|d| d.sync_all()).map_err(io)?;
        Ok(())
    }
}

fn open_state(dir: &Path, cfg: Cfg) -> Result<State, String> {
    let idx = dir.join("index");
    let count: u64 = std::fs::read_to_string(idx.join(COUNT_FILE))
        .map_err(io)?
        .trim()
        .parse()
        .map_err(|_| "bad COUNT file".to_string())?;
    let mut names: Vec<(u64, PathBuf)> = std::fs::read_dir(&idx)
        .map_err(io)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n = u64::from_str_radix(name.strip_suffix(".run")?, 16).ok()?;
            Some((n, e.path()))
        })
        .collect();
    names.sort();
    let next_run = names.last().map_or(0, |(n, _)| n + 1);
    let runs = names.into_iter().map(|(_, p)| Run::open(p, cfg)).collect::<Result<Vec<_>, _>>()?;
    Ok(State { dir: dir.to_path_buf(), mem: BTreeMap::new(), runs, next_run, count, cfg })
}

impl Backend for SpikeRunsBackend {
    fn name(&self) -> &'static str {
        if self.mmap {
            "spike-runs-mmap"
        } else {
            "spike-runs"
        }
    }
    fn create(&self, dir: &Path, _pack_limit: Option<u64>) -> Result<Box<dyn Store>, String> {
        std::fs::create_dir_all(dir.join("index")).map_err(io)?;
        // The create step counts the bytes in packs/; there are none.
        std::fs::create_dir_all(dir.join("packs")).map_err(io)?;
        let s = State { dir: dir.to_path_buf(), mem: BTreeMap::new(), runs: Vec::new(), next_run: 0, count: 0, cfg: self.cfg() };
        Ok(Box::new(Handle(RefCell::new(s))))
    }
    fn open(&self, dir: &Path, _read_only: bool) -> Result<Box<dyn Store>, String> {
        Ok(Box::new(Handle(RefCell::new(open_state(dir, self.cfg())?))))
    }
}
