//! Spike B benches (plan section 5): pread against mmap on P* = V1.
//!
//! Variants: `RS` (the real `ChunkStore`, pread), `V1` (spike A arenas,
//! pread), `V1M` (the same arenas, mmap, advice Default), `V1R` (mmap,
//! MADV_RANDOM). A trailing `b` (e.g. `V1b`) is a second label for an A/A
//! noise run.
//!
//! Data sets are built once (`bprep`) under `--data` and reused by every run:
//! `<data>/<set>-<V1|RS>/` with `ids.bin` (every object, in write order) and
//! `files.bin` (file keys). `set` = `rand` (incompressible) or `z2` (about
//! 2:1). Each file is written through `file_fork::FileWriter`, so a file's
//! chunks are contiguous in the arenas.
//!
//! Benches:
//! - `bprep`  build a data set.
//! - `bb1`    raw read + CRC only (variants `pread`, `mmap`), warm, prefaulted.
//! - `bget`   full `get()`: `--order random|seq`, `--state warm_mapped|warm_fresh|cold`
//!            (B-B2, B-B3, B-B5 are suites of it).
//! - `bb4`    `--threads N` random gets, warm and mapped; `--gate 1` holds an
//!            RwLock read guard per get (S3's `reuse_gate`); `--lookup 1` only
//!            does the index lookup (`contains`), to estimate the lock share.
//! - `bb6`    memory before / after a full pass / after unmapping.
//! - `bprep7` + `bb7` fd use: 2,000 arenas of 64 KiB under RLIMIT_NOFILE 256.

use super::{data_files, spike_opts, Args};
use crate::file_fork::{file_objects, FileReader, FileWriter, ObjStore};
use crate::map::{self, ArenaMap, MmapPath};
use crate::measure::{Rec, Span};
use crate::workload::{compressible, random, Rng};
use crate::{codec, sys, ObjectId, SpikeStore};
use chunk_store::ChunkStore;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

const MIB: u64 = 1 << 20;
const ARENA: u64 = 64 * MIB;

pub fn run(bench: &str, variant: &str, a: &Args) -> bool {
    match bench {
        "bprep" => bprep(variant, a),
        "bb1" => bb1(variant, a),
        "bget" => bget(variant, a),
        "bb4" => bb4(variant, a),
        "bb6" => bb6(variant, a),
        "bprep7" => bprep7(variant, a),
        "bb7" => bb7(variant, a),
        _ => return false,
    }
    true
}

fn data_root(a: &Args) -> PathBuf {
    PathBuf::from(a.str("data", &std::env::var("ARENA_BENCH_DATA").unwrap_or_else(|_| "bdata".into())))
}

/// `RS*` -> "RS", every spike variant -> "V1" (they share the arenas).
fn kind(variant: &str) -> &'static str {
    if variant.starts_with("RS") {
        "RS"
    } else {
        "V1"
    }
}

fn set_dir(a: &Args, variant: &str) -> PathBuf {
    data_root(a).join(format!("{}-{}", a.str("set", "rand"), kind(variant)))
}

/// A data-set store: the real one or the spike one.
enum Ds {
    Rs(ChunkStore),
    Sp(SpikeStore),
}

impl Ds {
    fn open(dir: &Path, variant: &str, size: u64) -> Ds {
        if variant.starts_with("RS") {
            let o = chunk_store::Options { pack_limit: size, ..Default::default() };
            Ds::Rs(ChunkStore::open(dir, o).unwrap())
        } else {
            let base = variant.trim_end_matches('b');
            Ds::Sp(SpikeStore::open(dir, spike_opts(base, size)).unwrap())
        }
    }

    fn obj(&self) -> &dyn ObjStore {
        match self {
            Ds::Rs(s) => s,
            Ds::Sp(s) => s,
        }
    }

    fn mmap(&self) -> Option<&MmapPath> {
        match self {
            Ds::Sp(s) => s.read_path().as_any().downcast_ref::<MmapPath>(),
            Ds::Rs(_) => None,
        }
    }

    fn arenas(&self, ids: &[ObjectId]) -> Vec<u64> {
        let Ds::Sp(s) = self else { return Vec::new() };
        let mut v: Vec<u64> = ids.iter().filter_map(|id| s.loc(id).map(|l| l.arena)).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Map every arena and touch every page (warm and mapped).
    fn prefault(&self, ids: &[ObjectId]) -> u64 {
        let Some(m) = self.mmap() else { return 0 };
        let mut s = 0;
        for n in self.arenas(ids) {
            if let Some(map) = m.map(n) {
                s += map.touch_all();
            }
        }
        s
    }
}

fn write_ids(path: &Path, ids: &[ObjectId]) {
    let mut f = File::create(path).unwrap();
    for id in ids {
        f.write_all(&id.0).unwrap();
    }
}

fn read_ids(path: &Path) -> Vec<ObjectId> {
    let mut b = Vec::new();
    File::open(path).unwrap_or_else(|e| panic!("{}: {e} (run bprep first)", path.display())).read_to_end(&mut b).unwrap();
    b.chunks_exact(chunk_store::ID_LEN).map(|c| ObjectId(c.try_into().unwrap())).collect()
}

/// Build data set `--set` for `variant` (V1 or RS): `--mib` MiB of raw data
/// in files of `--file_mib` MiB.
fn bprep(variant: &str, a: &Args) {
    let set = a.str("set", "rand");
    let mib = a.u64("mib", 1024);
    let file_mib = a.u64("file_mib", 8);
    let dir = set_dir(a, variant);
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();
    let ds = Ds::open(&dir, kind(variant), ARENA);
    let s = ds.obj();
    let span = Span::start();
    let (mut ids, mut keys) = (Vec::new(), Vec::new());
    for f in 0..mib / file_mib {
        let n = (file_mib * MIB) as usize;
        let data = if set == "z2" { compressible(n, 1000 + f) } else { random(n, 1000 + f) };
        let mut w = FileWriter::new();
        w.write(s, &data).unwrap();
        let key = w.finish(s).unwrap();
        ids.extend(file_objects(s, key).unwrap());
        keys.push(key);
    }
    s.sync().unwrap();
    if let Ds::Sp(sp) = &ds {
        sp.seal_active().unwrap();
    }
    s.close().unwrap();
    let m = span.end();
    let st = s.stats();
    write_ids(&dir.join("ids.bin"), &ids);
    write_ids(&dir.join("files.bin"), &keys);
    let mut r = Rec::new("bprep", kind(variant));
    r.s("set", &set).n("mib", mib as f64).m(&m);
    r.n("objects", ids.len() as f64)
        .n("files", keys.len() as f64)
        .n("arenas", st.packs as f64)
        .n("stored_mib", st.stored_bytes as f64 / MIB as f64);
    r.emit();
}

/// Copy `path` with F_NOCACHE on both sides and rename the copy over it, so
/// none of its pages are in the cache. Returns the resident fraction after.
fn evict(path: &Path) -> f64 {
    let tmp = path.with_extension("evict.tmp");
    let src = File::open(path).unwrap();
    sys::set_nocache(&src, true).unwrap();
    let dst = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp).unwrap();
    sys::set_nocache(&dst, true).unwrap();
    let len = src.metadata().unwrap().len();
    let mut buf = vec![0u8; MIB as usize];
    let mut off = 0;
    while off < len {
        let n = (len - off).min(MIB) as usize;
        src.read_exact_at(&mut buf[..n], off).unwrap();
        // Keep sparse ranges sparse (V1 arenas are mostly full, but the
        // active-arena tail is a hole).
        if buf[..n].iter().any(|&b| b != 0) {
            dst.write_all_at(&buf[..n], off).unwrap();
        }
        off += n as u64;
    }
    dst.set_len(len).unwrap();
    sys::plain_fsync(&dst).unwrap();
    drop((src, dst));
    fs::rename(&tmp, path).unwrap();
    map::resident_fraction(path).unwrap_or(f64::NAN)
}

/// Read every data file once (warm page cache).
fn warm_files(dir: &Path) -> u64 {
    let mut s = 0u64;
    let mut buf = vec![0u8; MIB as usize];
    for p in data_files(dir) {
        let mut f = File::open(&p).unwrap();
        loop {
            let n = f.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            s = s.wrapping_add(buf[n - 1] as u64);
        }
    }
    s
}

fn resident(dir: &Path) -> (f64, f64) {
    let (mut sum, mut max, mut n) = (0.0, 0.0f64, 0);
    for p in data_files(dir) {
        let r = map::resident_fraction(&p).unwrap_or(f64::NAN);
        sum += r;
        max = max.max(r);
        n += 1;
    }
    (sum / n.max(1) as f64, max)
}

/// Put the data set in `state` before the store is opened (cold) or after
/// (warm). Returns (resident mean, resident max) after the preparation.
fn prepare_state(state: &str, dir: &Path) -> (f64, f64) {
    match state {
        "cold" => {
            for p in data_files(dir) {
                evict(&p);
            }
        }
        _ => {
            warm_files(dir);
        }
    }
    resident(dir)
}

/// Full `get()` bench (B-B2, B-B3, B-B5).
fn bget(variant: &str, a: &Args) {
    let dir = set_dir(a, variant);
    let order = a.str("order", "random");
    let state = a.str("state", "warm_mapped");
    let n = a.u64("n", 200_000) as usize;
    let (res_mean, res_max) = prepare_state(&state, &dir);
    let ds = Ds::open(&dir, variant, ARENA);
    let ids = read_ids(&dir.join("ids.bin"));
    if state == "warm_mapped" {
        ds.prefault(&ids);
    }
    let s = ds.obj();
    let mut rng = Rng::new(a.u64("seed", 1) + a.u64("rep", 0) * 7919);
    let pick: Vec<usize> = (0..n).map(|_| rng.below(ids.len() as u64) as usize).collect();
    let (mut ops, mut bytes, mut miss) = (0u64, 0u64, 0u64);
    let span = Span::start();
    if order == "seq" {
        let block = a.u64("block_kib", 1024) as usize * 1024;
        for key in read_ids(&dir.join("files.bin")) {
            let mut r = FileReader::open(s, key).unwrap();
            let mut off = 0;
            while off < r.len() {
                match r.read(s, off, block) {
                    Ok(b) => {
                        off += b.len() as u64;
                        bytes += b.len() as u64;
                    }
                    Err(_) => {
                        miss += 1;
                        break;
                    }
                }
                ops += 1;
            }
        }
    } else {
        for &i in &pick {
            match s.get(&ids[i]) {
                Some(d) => bytes += d.len() as u64,
                None => miss += 1,
            }
            ops += 1;
        }
    }
    let m = span.end();
    let mut r = Rec::new("bget", variant);
    r.s("set", &a.str("set", "rand")).s("order", &order).s("state", &state).n("rep", a.u64("rep", 0) as f64);
    r.n("resident_mean", res_mean).n("resident_max", res_max);
    r.s("cold_valid", if state != "cold" { "n/a" } else if res_max < 0.01 { "yes" } else { "no" });
    r.m(&m)
        .n("ops", ops as f64)
        .n("miss", miss as f64)
        .n("mib", bytes as f64 / MIB as f64)
        .n("cpu_us_per_op", m.cpu_s() / ops as f64 * 1e6)
        .n("wall_us_per_op", m.wall_s / ops as f64 * 1e6)
        .n("mib_per_s_wall", bytes as f64 / MIB as f64 / m.wall_s)
        .n("mib_per_s_cpu", bytes as f64 / MIB as f64 / m.cpu_s())
        .n("mapped", ds.mmap().map_or(0, |m| m.mapped()) as f64);
    r.emit();
}

/// B-B1: raw read + CRC only, on the V1 data set. Variant `pread` or `mmap`.
/// `--order random|offset`. Warm, maps prefaulted.
fn bb1(variant: &str, a: &Args) {
    let dir = set_dir(a, "V1");
    let order = a.str("order", "random");
    let n = a.u64("n", 200_000) as usize;
    warm_files(&dir);
    let ds = Ds::open(&dir, "V1", ARENA);
    let Ds::Sp(sp) = &ds else { unreachable!() };
    let ids = read_ids(&dir.join("ids.bin"));
    let mut locs: Vec<_> = ids.iter().filter_map(|id| sp.loc(id)).collect();
    locs.sort_by_key(|l| (l.arena, l.off));
    let mut rng = Rng::new(a.u64("seed", 1) + a.u64("rep", 0) * 7919);
    let seq: Vec<usize> = if order == "offset" {
        (0..n).map(|i| i % locs.len()).collect()
    } else {
        (0..n).map(|_| rng.below(locs.len() as u64) as usize).collect()
    };
    let max_arena = locs.iter().map(|l| l.arena).max().unwrap() as usize;
    let mut files: Vec<Option<File>> = (0..=max_arena).map(|_| None).collect();
    let mut maps: Vec<Option<ArenaMap>> = (0..=max_arena).map(|_| None).collect();
    for l in &locs {
        let k = l.arena as usize;
        let p = sp.packs_dir().join(format!("{:016x}.arena", l.arena));
        if variant == "mmap" {
            if maps[k].is_none() {
                let len = fs::metadata(&p).unwrap().len();
                let m = ArenaMap::open(&p, len, crate::Advice::Default).unwrap();
                m.touch_all();
                maps[k] = Some(m);
            }
        } else if files[k].is_none() {
            files[k] = Some(File::open(&p).unwrap());
        }
    }
    let mut buf = vec![0u8; codec::rec_len(codec::MAX_STORED)];
    let mut acc = 0u32;
    let span = Span::start();
    for &i in &seq {
        let l = &locs[i];
        let len = codec::rec_len(l.stored);
        let b: &[u8] = if variant == "mmap" {
            maps[l.arena as usize].as_ref().unwrap().bytes(l.off, len).unwrap()
        } else {
            files[l.arena as usize].as_ref().unwrap().read_exact_at(&mut buf[..len], l.off).unwrap();
            &buf[..len]
        };
        acc ^= crc32c::crc32c(&b[..len - 4]);
    }
    let m = span.end();
    let mut r = Rec::new("bb1", variant);
    r.s("order", &order).n("rep", a.u64("rep", 0) as f64).n("reads", n as f64).n("acc", acc as f64);
    r.m(&m).n("cpu_us_per_op", m.cpu_s() / n as f64 * 1e6).n("wall_us_per_op", m.wall_s / n as f64 * 1e6);
    r.emit();
}

/// B-B4: `--threads` random gets, warm and mapped.
fn bb4(variant: &str, a: &Args) {
    let dir = set_dir(a, variant);
    let threads = a.u64("threads", 4) as usize;
    let per = a.u64("n", 100_000) as usize;
    let gate = a.u64("gate", 0) == 1;
    let lookup = a.u64("lookup", 0) == 1;
    warm_files(&dir);
    let ds = Arc::new(Ds::open(&dir, variant, ARENA));
    let ids = Arc::new(read_ids(&dir.join("ids.bin")));
    ds.prefault(&ids);
    let g = Arc::new(RwLock::new(()));
    let span = Span::start();
    let hs: Vec<_> = (0..threads)
        .map(|t| {
            let (ds, ids, g) = (ds.clone(), ids.clone(), g.clone());
            let seed = a.u64("rep", 0) * 7919 + t as u64 + 1;
            std::thread::spawn(move || {
                let s = ds.obj();
                let mut rng = Rng::new(seed);
                let mut miss = 0u64;
                for _ in 0..per {
                    let id = &ids[rng.below(ids.len() as u64) as usize];
                    let _guard = gate.then(|| g.read().unwrap());
                    let ok = if lookup {
                        match &*ds {
                            Ds::Sp(sp) => sp.contains(id),
                            Ds::Rs(rs) => rs.contains(id),
                        }
                    } else {
                        s.get(id).is_some()
                    };
                    miss += !ok as u64;
                }
                miss
            })
        })
        .collect();
    let miss: u64 = hs.into_iter().map(|h| h.join().unwrap()).sum();
    let m = span.end();
    let ops = (threads * per) as f64;
    let mut r = Rec::new("bb4", variant);
    r.n("threads", threads as f64).n("gate", gate as u8 as f64).n("lookup", lookup as u8 as f64);
    r.n("rep", a.u64("rep", 0) as f64).m(&m);
    r.n("ops", ops)
        .n("miss", miss as f64)
        .n("ops_per_s", ops / m.wall_s)
        .n("cpu_us_per_op", m.cpu_s() / ops * 1e6);
    r.emit();
}

/// B-B6: memory (resident, phys_footprint) after open, after a full pass of
/// gets of every object, after every map is dropped.
fn bb6(variant: &str, a: &Args) {
    let dir = set_dir(a, variant);
    warm_files(&dir);
    let ds = Ds::open(&dir, variant, ARENA);
    let ids = read_ids(&dir.join("ids.bin"));
    let mut r = Rec::new("bb6", variant);
    let (rss0, fp0) = map::footprint();
    let mut miss = 0;
    for id in &ids {
        miss += ds.obj().get(id).is_none() as u64;
    }
    let (rss1, fp1) = map::footprint();
    let mapped = ds.mmap().map_or(0, |m| m.mapped());
    if let Some(m) = ds.mmap() {
        m.unmap_all();
    }
    let (rss2, fp2) = map::footprint();
    r.n("objects", ids.len() as f64).n("miss", miss as f64).n("mapped", mapped as f64);
    r.n("rss_open_mib", rss0 as f64 / MIB as f64)
        .n("rss_pass_mib", rss1 as f64 / MIB as f64)
        .n("rss_unmap_mib", rss2 as f64 / MIB as f64)
        .n("footprint_open_mib", fp0 as f64 / MIB as f64)
        .n("footprint_pass_mib", fp1 as f64 / MIB as f64)
        .n("footprint_unmap_mib", fp2 as f64 / MIB as f64);
    r.emit();
}

fn set7(a: &Args, variant: &str) -> PathBuf {
    data_root(a).join(format!("fd-{}", kind(variant)))
}

/// B-B7 data set: `--arenas` arenas of 64 KiB (V1 with prepare_next, or RS).
fn bprep7(variant: &str, a: &Args) {
    let dir = set7(a, variant);
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();
    let want = a.u64("arenas", 2000);
    let size = 64 << 10;
    let ds = if kind(variant) == "RS" { Ds::open(&dir, "RS", size) } else { Ds::open(&dir, "V1P", size) };
    let s = ds.obj();
    let span = Span::start();
    let mut ids = Vec::new();
    let mut i = 0;
    while s.stats().packs < want {
        ids.push(s.put(&random(8000, 50_000 + i)).unwrap());
        i += 1;
    }
    s.sync().unwrap();
    if let Ds::Sp(sp) = &ds {
        sp.seal_active().unwrap();
    }
    s.close().unwrap();
    let m = span.end();
    write_ids(&dir.join("ids.bin"), &ids);
    let mut r = Rec::new("bprep7", kind(variant));
    r.m(&m).n("objects", ids.len() as f64).n("arenas", s.stats().packs as f64);
    r.emit();
}

fn open_fds() -> usize {
    map::open_fds(4096)
}

/// B-B7: read every object of the 2,000-arena store under RLIMIT_NOFILE 256.
fn bb7(variant: &str, a: &Args) {
    let dir = set7(a, variant);
    let ds = Ds::open(&dir, variant, 64 << 10);
    let ids = read_ids(&dir.join("ids.bin"));
    let mut r = Rec::new("bb7", variant); // before the limit: git sha needs fds
    let fds0 = open_fds();
    let limit = a.u64("nofile", 256);
    let set = map::set_nofile(limit);
    let span = Span::start();
    let mut miss = 0u64;
    let mut first_miss = None;
    for (k, id) in ids.iter().enumerate() {
        if ds.obj().get(id).is_none() {
            miss += 1;
            first_miss.get_or_insert(k);
        }
    }
    let m = span.end();
    let fds1 = open_fds();
    r.n("nofile", limit as f64).s("setrlimit", if set { "ok" } else { "failed" });
    r.n("objects", ids.len() as f64).n("arenas", ds.obj().stats().packs as f64);
    r.n("miss", miss as f64).n("first_miss", first_miss.map_or(-1.0, |k| k as f64));
    r.n("fds_before", fds0 as f64).n("fds_after", fds1 as f64);
    r.n("mapped", ds.mmap().map_or(0, |m| m.mapped()) as f64);
    r.m(&m);
    r.emit();
}
