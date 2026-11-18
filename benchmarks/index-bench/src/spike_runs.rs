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

/// A run file, opened: only fences and filter in RAM.
struct Run {
    path: PathBuf,
    file: File,
    entries: u64,
    fences: Vec<u64>,
    filter: Vec<u64>,
    nblocks: u64,
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
    fn open(path: PathBuf) -> Result<Run, String> {
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
        Ok(Run { path, file, entries, fences, filter, nblocks })
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
}

/// Write a run of `n` sorted entries to `path` (fsync), then open it.
fn write_run(path: PathBuf, n: u64, entries: impl Iterator<Item = (Key, Loc)>) -> Result<Run, String> {
    let tmp = path.with_extension("tmp");
    let file = File::create(&tmp).map_err(io)?;
    let mut w = BufWriter::with_capacity(1 << 20, &file);
    let nblocks = nblocks_for(n);
    let mut filter = vec![0u64; nblocks as usize * BLOCK_WORDS];
    let mut fences = Vec::with_capacity((n as usize).div_ceil(PER_PAGE));
    let mut in_page = 0;
    let mut written = 0u64;
    let pad = [0u8; PAGE - PER_PAGE * ENTRY];
    for (k, l) in entries {
        if in_page == 0 {
            fences.push(dig(&k, 0));
        }
        let b = bloom_block(&k, nblocks) * BLOCK_WORDS;
        for bit in bloom_bits(&k) {
            filter[b + (bit / 64) as usize] |= 1 << (bit % 64);
        }
        w.write_all(&k).map_err(io)?;
        w.write_all(&l).map_err(io)?;
        written += 1;
        in_page += 1;
        if in_page == PER_PAGE {
            w.write_all(&pad).map_err(io)?;
            in_page = 0;
        }
    }
    assert_eq!(written, n);
    if in_page != 0 {
        w.write_all(&vec![0u8; PAGE - in_page * ENTRY]).map_err(io)?;
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
    Run::open(path)
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
        let run = write_run(path, mem.len() as u64, mem.into_iter())?;
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
        let merged = write_run(path, total, iter)?;
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

pub struct SpikeRunsBackend;

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

fn open_state(dir: &Path) -> Result<State, String> {
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
    let runs = names.into_iter().map(|(_, p)| Run::open(p)).collect::<Result<Vec<_>, _>>()?;
    Ok(State { dir: dir.to_path_buf(), mem: BTreeMap::new(), runs, next_run, count })
}

impl Backend for SpikeRunsBackend {
    fn name(&self) -> &'static str {
        "spike-runs"
    }
    fn create(&self, dir: &Path, _pack_limit: Option<u64>) -> Result<Box<dyn Store>, String> {
        std::fs::create_dir_all(dir.join("index")).map_err(io)?;
        // The create step counts the bytes in packs/; there are none.
        std::fs::create_dir_all(dir.join("packs")).map_err(io)?;
        let s = State { dir: dir.to_path_buf(), mem: BTreeMap::new(), runs: Vec::new(), next_run: 0, count: 0 };
        Ok(Box::new(Handle(RefCell::new(s))))
    }
    fn open(&self, dir: &Path, _read_only: bool) -> Result<Box<dyn Store>, String> {
        Ok(Box::new(Handle(RefCell::new(open_state(dir)?))))
    }
}
