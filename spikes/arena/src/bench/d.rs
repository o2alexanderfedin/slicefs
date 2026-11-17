//! Spike D benches: GC frees dead records by punching holes in sealed
//! arenas instead of compaction.
//!
//! - `dprobe`: raw facts about `F_PUNCHHOLE` on this volume (alignment,
//!   readback, st_blocks, cost, write into a hole).
//! - `dgc`: one GC after a fill, per dead fraction and delete pattern;
//!   variants `RS` (chunk-store), `A` (spike A compaction, V1), `D` (punch,
//!   V1), `D0` (punch, V0 grow files).
//! - `dchurn`: several rounds of write + delete + GC (does the waste add up).

use super::{data_files, run_dir, Args};
use crate::file_fork::{file_objects, FileReader, FileWriter, ObjStore};
use crate::measure::{pct, Rec, Span};
use crate::{sys, workload, GcMode, ObjectId, Options, Prealloc, SpikeStore};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

const MIB: u64 = 1 << 20;

pub fn run(bench: &str, variant: &str, a: &Args) -> bool {
    match bench {
        "dprobe" => dprobe(variant, a),
        "dgc" => dgc(variant, a),
        "dchurn" => dchurn(variant, a),
        _ => return false,
    }
    true
}

fn errno(r: std::io::Result<()>) -> f64 {
    match r {
        Ok(()) => 0.0,
        Err(e) => e.raw_os_error().unwrap_or(-1) as f64,
    }
}

fn blocks(f: &File) -> u64 {
    f.metadata().unwrap().blocks() * 512
}

/// A 64 MiB file with data in every block, made as `how`: `grow` (appends),
/// `sparse` (ftruncate, then writes), `reserve` (F_PREALLOCATE + ftruncate,
/// then writes), `zeroed` (zeros, then writes).
fn filled(path: &Path, how: &str, size: u64) -> File {
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path).unwrap();
    match how {
        "sparse" => f.set_len(size).unwrap(),
        "reserve" => {
            sys::preallocate(&f, size).unwrap();
            f.set_len(size).unwrap();
        }
        "zeroed" => crate::arena::write_zeros(&f, 0, size).unwrap(),
        _ => {}
    }
    let data = workload::random(MIB as usize, 9);
    let mut off = 0;
    while off < size {
        f.write_all_at(&data, off).unwrap();
        off += MIB;
    }
    sys::full_sync(&f).unwrap();
    f
}

/// Raw facts. `variant` = how the file is made (grow, sparse, reserve,
/// zeroed); every probe is on one 64 MiB file full of data.
fn dprobe(how: &str, a: &Args) {
    let d = run_dir();
    let size = a.u64("size_mib", 64) * MIB;
    let path = d.path().join("f");
    let bs = sys::fs_block_size(d.path()).unwrap();
    let f = filled(&path, how, size);
    let mut r = Rec::new("dprobe", how);
    r.n("fs_block", bs as f64).n("rep", a.u64("rep", 0) as f64);
    // Alignment: unaligned offset, unaligned length, aligned to 512 only.
    r.n("errno_off_unaligned", errno(sys::punch_hole(&f, 100, bs)));
    r.n("errno_len_unaligned", errno(sys::punch_hole(&f, bs, 100)));
    r.n("errno_512", errno(sys::punch_hole(&f, 512, bs)));
    // A read-only fd.
    let ro = File::open(&path).unwrap();
    r.n("errno_readonly_fd", errno(sys::punch_hole(&ro, 8 * MIB, bs)));
    // One aligned 1 MiB punch in the middle.
    let (off, len) = (16 * MIB, MIB);
    let b0 = blocks(&f);
    let free0 = sys::free_bytes(d.path()).unwrap();
    let t = Instant::now();
    r.n("errno_aligned", errno(sys::punch_hole(&f, off, len)));
    r.n("punch_1mib_ms", t.elapsed().as_secs_f64() * 1e3);
    let b1 = blocks(&f);
    let t = Instant::now();
    sys::full_sync(&f).unwrap();
    r.n("sync_after_punch_ms", t.elapsed().as_secs_f64() * 1e3);
    let b2 = blocks(&f);
    let free2 = sys::free_bytes(d.path()).unwrap();
    r.n("blocks_freed_before_sync", b0 as f64 - b1 as f64).n("blocks_freed_after_sync", b0 as f64 - b2 as f64);
    r.n("bavail_delta", free2 as f64 - free0 as f64);
    r.n("len_unchanged", (f.metadata().unwrap().len() == size) as u8 as f64);
    // Readback: zeros inside, data next to it.
    let mut buf = vec![0xffu8; len as usize];
    f.read_exact_at(&mut buf, off).unwrap();
    r.n("reads_zero_same_fd", buf.iter().all(|&b| b == 0) as u8 as f64);
    File::open(&path).unwrap().read_exact_at(&mut buf, off).unwrap();
    r.n("reads_zero_new_fd", buf.iter().all(|&b| b == 0) as u8 as f64);
    let data = workload::random(MIB as usize, 9);
    let mut edge = vec![0u8; 4096];
    f.read_exact_at(&mut edge, off - 4096).unwrap();
    let before_ok = edge == data[(MIB - 4096) as usize..];
    f.read_exact_at(&mut edge, off + len).unwrap();
    r.n("neighbours_intact", (before_ok && edge == data[..4096]) as u8 as f64);
    // SEEK_HOLE / SEEK_DATA see it.
    r.n("seek_hole_at", sys::seek_hole(&f, 8 * MIB).map(|x| x as f64).unwrap_or(-1.0));
    r.n("seek_data_from_hole", sys::seek_data(&f, off).ok().flatten().map(|x| x as f64).unwrap_or(-1.0));
    // Cost: 1000 punches of 8 KiB, 16 KiB apart, from 24 MiB.
    let n = a.u64("n", 1000);
    let mut lat = Vec::new();
    let span = Span::start();
    for i in 0..n {
        let t = Instant::now();
        sys::punch_hole(&f, 24 * MIB + i * 16384, 8192).unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let m = span.end();
    let t = Instant::now();
    sys::full_sync(&f).unwrap();
    r.n("sync_after_1000_ms", t.elapsed().as_secs_f64() * 1e3);
    r.n("punch8k_p50_us", pct(&mut lat, 50.0)).n("punch8k_p99_us", pct(&mut lat, 99.0));
    r.n("punch8k_cpu_us", m.cpu_s() * 1e6 / n as f64).n("punch8k_wall_us", m.wall_s * 1e6 / n as f64);
    // Spike C question: 8 KiB write into a punched hole + full_sync, against
    // an overwrite of allocated blocks + full_sync. Alternate them.
    let k = a.u64("k", 100);
    let rec = workload::random(8192, 3);
    let (mut hole, mut over) = (Vec::new(), Vec::new());
    for i in 0..k {
        let t = Instant::now();
        f.write_all_at(&rec, 24 * MIB + i * 16384).unwrap(); // a punched 8 KiB
        sys::full_sync(&f).unwrap();
        hole.push(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        f.write_all_at(&rec, 40 * MIB + i * 16384).unwrap(); // allocated
        sys::full_sync(&f).unwrap();
        over.push(t.elapsed().as_secs_f64() * 1e3);
    }
    r.n("write_hole_sync_p50_ms", pct(&mut hole, 50.0)).n("write_hole_sync_p99_ms", pct(&mut hole, 99.0));
    r.n("overwrite_sync_p50_ms", pct(&mut over, 50.0)).n("overwrite_sync_p99_ms", pct(&mut over, 99.0));
    r.n("blocks_end", blocks(&f) as f64);
    r.emit();
}

// ── GC benches ───────────────────────────────────────────────────────────────

/// A store for one variant: `RS`, `A` (V1 + compaction), `D` (V1 + punch),
/// `D0` (V0 + punch).
enum Store {
    Rs(chunk_store::ChunkStore),
    Spike(SpikeStore),
}

impl Store {
    fn open(variant: &str, dir: &Path, size: u64) -> Store {
        if variant == "RS" {
            let o = chunk_store::Options { pack_limit: size, ..Default::default() };
            return Store::Rs(chunk_store::ChunkStore::open(dir, o).unwrap());
        }
        Store::Spike(SpikeStore::open(dir, d_opts(variant, size)).unwrap())
    }

    fn obj(&self) -> &dyn ObjStore {
        match self {
            Store::Rs(s) => s,
            Store::Spike(s) => s,
        }
    }

    fn gc(&self, live: &HashSet<ObjectId>) -> chunk_store::GcStats {
        match self {
            Store::Rs(s) => {
                let b = s.gc_begin().unwrap();
                s.gc_finish(live, b).unwrap()
            }
            Store::Spike(s) => {
                let b = s.gc_begin().unwrap();
                s.gc_finish(live, b).unwrap()
            }
        }
    }

    fn punches(&self) -> u64 {
        match self {
            Store::Spike(s) => s.punch_stats().calls,
            _ => 0,
        }
    }
}

pub fn d_opts(variant: &str, size: u64) -> Options {
    let (p, g) = match variant {
        "A" => (Prealloc::Sparse, GcMode::Compact),
        "D" => (Prealloc::Sparse, GcMode::Punch),
        "D0" => (Prealloc::Grow, GcMode::Punch),
        "A0" => (Prealloc::Grow, GcMode::Compact),
        _ => panic!("unknown variant {variant}"),
    };
    let mut o = Options::with_prealloc(p);
    o.arena.size = size;
    o.gc = g;
    o
}

/// Per file: (length, inode, allocated bytes).
fn snapshot(dir: &Path) -> HashMap<PathBuf, (u64, u64, u64)> {
    let mut m = HashMap::new();
    for e in fs::read_dir(dir.join("packs")).unwrap().flatten() {
        if let Ok(md) = e.metadata() {
            if md.is_file() {
                m.insert(e.path(), (md.len(), md.ino(), md.blocks() * 512));
            }
        }
    }
    m
}

/// Bytes written between two snapshots: a new file (or a new inode, from a
/// rename) counts in full, a file that grew counts its growth. Data files
/// and idx files apart.
fn written(before: &HashMap<PathBuf, (u64, u64, u64)>, after: &HashMap<PathBuf, (u64, u64, u64)>) -> (u64, u64) {
    let (mut data, mut idx) = (0, 0);
    for (p, &(len, ino, _)) in after {
        let w = match before.get(p) {
            Some(&(l0, i0, _)) if i0 == ino => len.saturating_sub(l0),
            _ => len,
        };
        let is_data = p.extension().is_some_and(|x| x == "arena" || x == "pack");
        if is_data {
            data += w;
        } else {
            idx += w;
        }
    }
    (data, idx)
}

fn alloc_sum(s: &HashMap<PathBuf, (u64, u64, u64)>) -> u64 {
    s.values().map(|v| v.2).sum()
}

/// Fill a store with `bytes` of `mixed` data, as `files` of about `file_mib`
/// MiB through `file_fork` (pattern `file`), or as single objects (pattern
/// `obj`). Returns the units (each a set of object IDs and, for files, the
/// root key) in write order.
fn fill(s: &dyn ObjStore, bytes: u64, pattern: &str, file_mib: u64, seed: u64) -> Vec<(Option<ObjectId>, Vec<ObjectId>)> {
    let objs = workload::mixed_bytes(bytes, seed);
    let mut units = Vec::new();
    if pattern == "obj" {
        for (i, o) in objs.iter().enumerate() {
            units.push((None, vec![s.put(o).unwrap()]));
            if i % 1024 == 1023 {
                s.sync().unwrap();
            }
        }
    } else {
        let mut w = FileWriter::new();
        for o in &objs {
            w.write(s, o).unwrap();
            if w.len() >= file_mib * MIB {
                let key = std::mem::replace(&mut w, FileWriter::new()).finish(s).unwrap();
                units.push((Some(key), file_objects(s, key).unwrap()));
                s.sync().unwrap();
            }
        }
        if !w.is_empty() {
            let key = w.finish(s).unwrap();
            units.push((Some(key), file_objects(s, key).unwrap()));
        }
    }
    s.sync().unwrap();
    units
}

/// Which units die: `dead` fraction, by a seeded shuffle (both patterns;
/// for `file` a unit is a whole file, so its records are contiguous).
fn pick_dead(n: usize, dead: f64, seed: u64) -> Vec<bool> {
    let mut idx: Vec<usize> = (0..n).collect();
    let mut rng = workload::Rng::new(seed ^ 0xdead);
    for i in (1..n).rev() {
        idx.swap(i, rng.below(i as u64 + 1) as usize);
    }
    let k = (n as f64 * dead).round() as usize;
    let mut out = vec![false; n];
    for &i in &idx[..k] {
        out[i] = true;
    }
    out
}

fn live_of(units: &[(Option<ObjectId>, Vec<ObjectId>)], dead: &[bool]) -> HashSet<ObjectId> {
    let mut live = HashSet::new();
    for ((_, ids), &d) in units.iter().zip(dead) {
        if !d {
            live.extend(ids.iter().copied());
        }
    }
    live
}

/// Read every live unit back: files sequentially through `FileReader` in
/// 128 KiB reads, objects by `get`. Returns (bytes, seconds wall, cpu).
fn read_back(s: &dyn ObjStore, units: &[(Option<ObjectId>, Vec<ObjectId>)], dead: &[bool]) -> (u64, f64, f64) {
    let span = Span::start();
    let mut bytes = 0u64;
    for ((key, ids), &d) in units.iter().zip(dead) {
        if d {
            continue;
        }
        match key {
            Some(k) => {
                let mut r = FileReader::open(s, *k).unwrap();
                let mut off = 0;
                while off < r.len() {
                    let b = r.read(s, off, 128 << 10).unwrap();
                    assert!(!b.is_empty());
                    off += b.len() as u64;
                    bytes += b.len() as u64;
                }
            }
            None => {
                for id in ids {
                    bytes += s.get(id).expect("live object lost").len() as u64;
                }
            }
        }
    }
    let m = span.end();
    (bytes, m.wall_s, m.cpu_s())
}

/// Stored bytes of the dead records that no live unit shares, and the part
/// of them inside whole blocks of their own dead runs (the punch bound),
/// from the record offsets (spike stores only).
fn predicted(s: &SpikeStore, live: &HashSet<ObjectId>, all: &HashSet<ObjectId>, bs: u64) -> (u64, u64) {
    let mut per: HashMap<u64, Vec<(u64, u64, bool)>> = HashMap::new();
    for id in all {
        let l = s.loc(id).unwrap();
        let len = crate::codec::rec_len(l.stored) as u64;
        per.entry(l.arena).or_default().push((l.off, l.off + len, live.contains(id)));
    }
    let (mut dead, mut punchable) = (0u64, 0u64);
    for (_, mut v) in per {
        v.sort_unstable();
        let mut i = 0;
        while i < v.len() {
            if v[i].2 {
                i += 1;
                continue;
            }
            let start = v[i].0;
            let mut end = v[i].1;
            while i < v.len() && !v[i].2 {
                dead += v[i].1 - v[i].0;
                end = v[i].1;
                i += 1;
            }
            if let Some((_, n)) = sys::aligned_inside(start, end, bs) {
                punchable += n;
            }
        }
    }
    (dead, punchable)
}

/// D-B1: fill, sync, then one GC with `dead` of the units gone. Reports
/// allocated bytes before and after, bytes GC wrote, GC wall and CPU, punch
/// calls, the predicted and measured punch fraction, and the read speed of
/// the survivors (warm) before and after the GC.
fn dgc(variant: &str, a: &Args) {
    let size = a.u64("size_mib", 64) * MIB;
    let total = a.u64("mib", 256) * MIB;
    let pattern = a.str("pattern", "file");
    let dead_frac = a.f64("dead", 0.5);
    let seed = a.u64("seed", 1) + a.u64("rep", 0) * 7919;
    let d = run_dir();
    let bs = sys::fs_block_size(d.path()).unwrap();
    let s = Store::open(variant, d.path(), size);
    let units = fill(s.obj(), total, &pattern, a.u64("file_mib", 1), seed);
    let dead = pick_dead(units.len(), dead_frac, seed);
    let live = live_of(&units, &dead);
    let all: HashSet<ObjectId> = units.iter().flat_map(|u| u.1.iter().copied()).collect();
    let pred = match &s {
        Store::Spike(sp) => Some(predicted(sp, &live, &all, bs)),
        _ => None,
    };
    let (rb, rw0, rc0) = read_back(s.obj(), &units, &dead);
    let snap0 = snapshot(d.path());
    let stats0 = s.obj().stats();
    let span = Span::start();
    let gs = s.gc(&live);
    let m = span.end();
    let snap1 = snapshot(d.path());
    let (wd, wi) = written(&snap0, &snap1);
    let (_, rw1, rc1) = read_back(s.obj(), &units, &dead);
    let stats1 = s.obj().stats();
    let mut r = Rec::new("dgc", variant);
    r.s("pattern", &pattern).n("dead", dead_frac).n("rep", a.u64("rep", 0) as f64).n("fs_block", bs as f64);
    r.n("units", units.len() as f64).n("objects", all.len() as f64);
    r.m(&m);
    r.n("alloc_before", alloc_sum(&snap0) as f64).n("alloc_after", alloc_sum(&snap1) as f64);
    let freed = alloc_sum(&snap0) as f64 - alloc_sum(&snap1) as f64;
    r.n("freed", freed);
    r.n("stored_before", stats0.stored_bytes as f64).n("stored_after", stats1.stored_bytes as f64);
    let dead_stored = stats0.stored_bytes as f64 - stats1.stored_bytes as f64;
    r.n("dead_stored", dead_stored);
    r.n("gc_wrote_data", wd as f64).n("gc_wrote_idx", wi as f64);
    r.n("removed_objects", gs.removed_objects as f64).n("packs_deleted", gs.packs_deleted as f64);
    r.n("packs_compacted", gs.packs_compacted as f64).n("punch_calls", s.punches() as f64);
    if let Some((pd, pp)) = pred {
        r.n("pred_dead_rec_bytes", pd as f64).n("pred_punchable", pp as f64);
        r.n("pred_frac", if pd > 0 { pp as f64 / pd as f64 } else { f64::NAN });
    }
    r.n("freed_frac", if dead_stored > 0.0 { freed / dead_stored } else { f64::NAN });
    r.n("space_amp", alloc_sum(&snap1) as f64 / stats1.stored_bytes.max(1) as f64);
    r.n("read_mib", rb as f64 / MIB as f64);
    r.n("read_before_wall_s", rw0).n("read_before_cpu_s", rc0).n("read_after_wall_s", rw1).n("read_after_cpu_s", rc1);
    r.emit();
}

/// D-B2: `rounds` of (write `mib`/rounds of new files, delete the same
/// amount of old files at random, GC). Space amplification and bytes
/// written after each round; the live set stays about `mib`.
fn dchurn(variant: &str, a: &Args) {
    let size = a.u64("size_mib", 64) * MIB;
    let total = a.u64("mib", 192) * MIB;
    let rounds = a.u64("rounds", 6);
    let pattern = a.str("pattern", "file");
    let churn = a.f64("churn", 0.25);
    let seed = a.u64("seed", 1) + a.u64("rep", 0) * 7919;
    let d = run_dir();
    let s = Store::open(variant, d.path(), size);
    let mut units = fill(s.obj(), total, &pattern, a.u64("file_mib", 1), seed);
    let snap_start = snapshot(d.path());
    let (mut wd, mut wi) = (0u64, 0u64);
    let (mut gc_wall, mut gc_cpu) = (0.0, 0.0);
    let mut user_bytes = 0u64;
    let mut amps = Vec::new();
    let mut rng = workload::Rng::new(seed ^ 0xc0ffee);
    for round in 0..rounds {
        let n = (units.len() as f64 * churn) as usize;
        for _ in 0..n {
            let i = rng.below(units.len() as u64) as usize;
            units.swap_remove(i);
        }
        let s0 = snapshot(d.path());
        let new = fill(s.obj(), (total as f64 * churn) as u64, &pattern, a.u64("file_mib", 1), seed + 1000 + round);
        let s1 = snapshot(d.path());
        user_bytes += written(&s0, &s1).0;
        units.extend(new);
        let live: HashSet<ObjectId> = units.iter().flat_map(|u| u.1.iter().copied()).collect();
        let span = Span::start();
        s.gc(&live);
        let m = span.end();
        gc_wall += m.wall_s;
        gc_cpu += m.cpu_s();
        let s2 = snapshot(d.path());
        let (a1, b1) = written(&s1, &s2);
        wd += a1;
        wi += b1;
        let st = s.obj().stats();
        amps.push(alloc_sum(&s2) as f64 / st.stored_bytes.max(1) as f64);
    }
    let all_dead = vec![false; units.len()];
    let (rb, rw, rc) = read_back(s.obj(), &units, &all_dead);
    let snap_end = snapshot(d.path());
    let st = s.obj().stats();
    let mut r = Rec::new("dchurn", variant);
    r.s("pattern", &pattern).n("rounds", rounds as f64).n("churn", churn).n("rep", a.u64("rep", 0) as f64);
    r.n("gc_wall_s", gc_wall).n("gc_cpu_s", gc_cpu);
    r.n("user_bytes", user_bytes as f64).n("gc_wrote_data", wd as f64).n("gc_wrote_idx", wi as f64);
    r.n("wa_gc", (user_bytes + wd + wi) as f64 / user_bytes.max(1) as f64);
    for (i, x) in amps.iter().enumerate() {
        r.n(&format!("amp_r{}", i + 1), *x);
    }
    r.n("alloc_start", alloc_sum(&snap_start) as f64).n("alloc_end", alloc_sum(&snap_end) as f64);
    r.n("stored_end", st.stored_bytes as f64).n("files_end", data_files(d.path()).len() as f64);
    r.n("read_mib", rb as f64 / MIB as f64).n("read_wall_s", rw).n("read_cpu_s", rc);
    r.emit();
}
