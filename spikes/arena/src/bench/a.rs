//! Spike A benches (plan section 4): ab1 put+sync, ab2 raw write+sync,
//! ab3 disk space, ab4 reopen, ab5 arena creation, ab6 CPU per put.

use super::{data_files, open_store, run_dir, spike_opts, Args};
use crate::arena::Arena;
use crate::measure::{pct, Rec, Span};
use crate::{sys, workload, ArenaCfg, ObjectId, Prealloc, SealRule, SpikeStore, Walk};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::time::Instant;

const MIB: u64 = 1 << 20;

pub fn run(bench: &str, variant: &str, a: &Args) -> bool {
    match bench {
        "ab1" => ab1(variant, a),
        "ab2" => ab2(variant, a),
        "ab3" => ab3(variant, a),
        "ab4" => ab4(variant, a),
        "ab5" => ab5(variant, a),
        "ab6" => ab6(variant, a),
        "probe" => probe(a),
        _ => return false,
    }
    true
}

/// One F_FULLFSYNC latency probe (to size the runs).
fn probe(a: &Args) {
    let d = run_dir();
    let f = File::create(d.path().join("p")).unwrap();
    let n = a.u64("n", 50);
    let mut lat = Vec::new();
    for i in 0..n {
        f.write_all_at(&[1u8; 8192], i * 8192).unwrap();
        let t = Instant::now();
        sys::full_sync(&f).unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let mut r = Rec::new("probe", "raw");
    r.n("fullfsync_p50_ms", pct(&mut lat, 50.0)).n("fullfsync_p99_ms", pct(&mut lat, 99.0));
    r.emit();
}

/// A-B1: put + sync every `k` puts, `mixed` workload.
fn ab1(variant: &str, a: &Args) {
    let k = a.u64("k", 8) as usize;
    let n = a.u64("n", 8000) as usize;
    let size = a.u64("size_mib", 64) * MIB;
    let seed = a.u64("seed", 1) + a.u64("rep", 0) * 7919;
    let objs = workload::mixed(n, seed);
    let d = run_dir();
    let s = open_store(variant, d.path(), size);
    let mut sync_lat = Vec::with_capacity(n / k + 1);
    let mut put_lat = Vec::with_capacity(n);
    let mut boundary = Vec::new();
    let span = Span::start();
    for batch in objs.chunks(k) {
        for o in batch {
            let p0 = s.stats().packs;
            let t = Instant::now();
            s.put(o).unwrap();
            let dt = t.elapsed().as_secs_f64() * 1e3;
            if s.stats().packs != p0 {
                boundary.push(dt);
            }
            put_lat.push(dt);
        }
        let t = Instant::now();
        s.sync().unwrap();
        sync_lat.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let m = span.end();
    let st = s.stats();
    let nsync = sync_lat.len();
    let slow = put_lat.iter().filter(|&&x| x > 1.0).count();
    let mut r = Rec::new("ab1", variant);
    r.s("cfg", &format!("k={k} n={n}")).n("k", k as f64).n("n", n as f64).n("rep", a.u64("rep", 0) as f64);
    r.m(&m)
        .n("sync_p50_ms", pct(&mut sync_lat.clone(), 50.0))
        .n("sync_p90_ms", pct(&mut sync_lat.clone(), 90.0))
        .n("sync_p99_ms", pct(&mut sync_lat, 99.0))
        .n("syncs_per_s", nsync as f64 / m.wall_s)
        .n("put_p50_us", pct(&mut put_lat.clone(), 50.0) * 1e3)
        .n("put_p99_us", pct(&mut put_lat.clone(), 99.0) * 1e3)
        .n("put_max_ms", pct(&mut put_lat, 100.0))
        .n("slow_puts_1ms", slow as f64)
        .n("boundary_puts", boundary.len() as f64)
        .n("boundary_put_max_ms", if boundary.is_empty() { 0.0 } else { pct(&mut boundary, 100.0) })
        .n("cpu_per_put_us", m.cpu_s() / n as f64 * 1e6)
        .n("arenas", st.packs as f64)
        .n("stored_mib", st.stored_bytes as f64 / MIB as f64);
    r.emit();
}

/// A-B6: CPU per put, no sync, `mixed`, `mib` MiB of raw data.
fn ab6(variant: &str, a: &Args) {
    let mib = a.u64("mib", 256);
    let seed = a.u64("seed", 6) + a.u64("rep", 0) * 7919;
    let objs = workload::mixed_bytes(mib * MIB, seed);
    let d = run_dir();
    let s = open_store(variant, d.path(), 64 * MIB);
    let span = Span::start();
    for o in &objs {
        s.put(o).unwrap();
    }
    let m = span.end();
    let mut r = Rec::new("ab6", variant);
    r.s("cfg", &format!("mib={mib}")).n("rep", a.u64("rep", 0) as f64).n("puts", objs.len() as f64);
    r.m(&m)
        .n("cpu_per_put_us", m.cpu_s() / objs.len() as f64 * 1e6)
        .n("wall_per_put_us", m.wall_s / objs.len() as f64 * 1e6)
        .n("arenas", s.stats().packs as f64);
    r.emit();
}

/// A-B2: raw 8 KiB `pwrite` + sync into one 64 MiB file. Variant = mode:
/// `eof` (past EOF), `sparse`, `prealloc`, `overwrite`, `zeroed`. `--sync`:
/// `full` (F_FULLFSYNC, default), `fsync`, `fdatasync`.
fn ab2(mode: &str, a: &Args) {
    let iters = a.u64("iters", 1000);
    let sync = a.str("sync", "full");
    let size = 64 * MIB;
    let d = run_dir();
    let path = d.path().join("raw");
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
    match mode {
        "eof" => {}
        "sparse" => f.set_len(size).unwrap(),
        "prealloc" => {
            sys::preallocate(&f, size).unwrap();
            f.set_len(size).unwrap();
        }
        "overwrite" | "zeroed" => {
            let fill = if mode == "zeroed" { vec![0u8; MIB as usize] } else { workload::random(MIB as usize, 9) };
            for i in 0..size / MIB {
                f.write_all_at(&fill, i * MIB).unwrap();
            }
        }
        _ => panic!("unknown ab2 mode {mode}"),
    }
    sys::full_sync(&f).unwrap();
    let alloc0 = sys::allocated(&path).unwrap();
    let block = workload::random(8192, a.u64("rep", 0) + 3);
    let mut lat = Vec::with_capacity(iters as usize);
    let span = Span::start();
    for i in 0..iters {
        let t = Instant::now();
        f.write_all_at(&block, 4096 + i * 8192).unwrap();
        match sync.as_str() {
            "full" => sys::full_sync(&f).unwrap(),
            "fsync" => sys::plain_fsync(&f).unwrap(),
            "fdatasync" => sys::data_sync(&f).unwrap(),
            _ => panic!("unknown sync {sync}"),
        }
        lat.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let m = span.end();
    let mut r = Rec::new("ab2", mode);
    r.s("cfg", &format!("sync={sync} iters={iters}")).n("rep", a.u64("rep", 0) as f64);
    r.m(&m)
        .n("op_p50_ms", pct(&mut lat.clone(), 50.0))
        .n("op_p99_ms", pct(&mut lat, 99.0))
        .n("cpu_per_op_us", m.cpu_s() / iters as f64 * 1e6)
        .n("alloc_before_mib", alloc0 as f64 / MIB as f64)
        .n("alloc_after_mib", sys::allocated(&path).unwrap() as f64 / MIB as f64);
    r.emit();
}

/// Put `mixed` objects until the active arena's hwm reaches `target`, or
/// (target = None) until the first arena is sealed. Returns the ids.
fn fill(s: &SpikeStore, target: Option<u64>, seed: u64) -> Vec<ObjectId> {
    let mut ids = Vec::new();
    let mut i = 0u64;
    let first = loop {
        if let Some((n, _)) = s.active() {
            break n;
        }
        ids.push(s.put(&workload::mixed(1, seed + i)[0]).unwrap());
        i += 1;
    };
    loop {
        match (s.active(), target) {
            (Some((n, hwm)), Some(t)) if n == first && hwm >= t => break,
            (Some((n, _)), _) if n != first => break,
            (None, _) => break,
            _ => {}
        }
        // Batches of 64 to keep object generation cheap relative to I/O.
        for o in workload::mixed(64, seed.wrapping_mul(31) + i) {
            ids.push(s.put(&o).unwrap());
            if let (Some((n, hwm)), Some(t)) = (s.active(), target) {
                if n == first && hwm >= t {
                    break;
                }
            }
            if s.active().map(|x| x.0) != Some(first) {
                break;
            }
        }
        i += 1;
    }
    ids
}

/// A-B3: disk space (deterministic, one rep).
fn ab3(variant: &str, _a: &Args) {
    let size = 64 * MIB;
    let p = Prealloc::parse(variant).unwrap();
    // Part 1: one arena filled to 1/10/50/100 %, trim off / on after seal.
    for pctf in [1u64, 10, 50, 100] {
        for trim in [false, true] {
            if trim && !p.fixed() {
                continue;
            }
            let d = run_dir();
            let free0 = sys::free_bytes(d.path()).unwrap();
            let mut o = spike_opts(variant, size);
            o.arena.trim_on_seal = trim;
            let s = SpikeStore::open(d.path(), o).unwrap();
            let target = (pctf < 100).then(|| 4096 + (size - 4096) * pctf / 100);
            fill(&s, target, 300 + pctf);
            s.sync().unwrap();
            let files = data_files(d.path());
            let first = &files[0];
            let logical = std::fs::metadata(first).unwrap().len();
            let alloc_open = sys::allocated(first).unwrap();
            let free1 = sys::free_bytes(d.path()).unwrap();
            s.seal_active().unwrap();
            let alloc_sealed = sys::allocated(first).unwrap();
            let logical_sealed = std::fs::metadata(first).unwrap().len();
            let hwm = {
                let idx = crate::arena::load_idx(&d.path().join("packs"), 1).unwrap();
                idx.hwm
            };
            let mut r = Rec::new("ab3", variant);
            r.s("part", "fill")
                .s("cfg", &format!("fill={pctf} trim={trim}"))
                .n("fill_pct", pctf as f64)
                .n("trim", trim as u8 as f64)
                .n("hwm_mib", hwm as f64 / MIB as f64)
                .n("logical_mib", logical as f64 / MIB as f64)
                .n("alloc_open_mib", alloc_open as f64 / MIB as f64)
                .n("logical_sealed_mib", logical_sealed as f64 / MIB as f64)
                .n("alloc_sealed_mib", alloc_sealed as f64 / MIB as f64)
                .n("bavail_delta_mib", (free0 as f64 - free1 as f64) / MIB as f64)
                .n("arena_files", files.len() as f64);
            r.emit();
        }
    }
    // Part 2: 20 close/open cycles of 1 MiB each: close seals (today) or
    // writes a resumable idx (new).
    for close_seals in [true, false] {
        let d = run_dir();
        let mut o = spike_opts(variant, size);
        o.close_seals = close_seals;
        for c in 0..20u64 {
            let s = SpikeStore::open(d.path(), o).unwrap();
            for obj in workload::mixed_bytes(MIB, 1000 + c) {
                s.put(&obj).unwrap();
            }
            s.close().unwrap();
        }
        space_line(variant, "close_open", &format!("close_seals={close_seals}"), d.path());
    }
    // Part 3: 20 gc_begin calls with 1 MiB put before each, everything live:
    // seal always (today) or only at size/8 (new).
    for rule in [SealRule::Always, SealRule::Eighth] {
        let d = run_dir();
        let mut o = spike_opts(variant, size);
        o.gc_seal = rule;
        let s = SpikeStore::open(d.path(), o).unwrap();
        let mut live = HashSet::new();
        let mut compacted = 0;
        for c in 0..20u64 {
            for obj in workload::mixed_bytes(MIB, 2000 + c) {
                live.insert(s.put(&obj).unwrap());
            }
            let t = s.gc_begin().unwrap();
            compacted += s.gc_finish(&live, t).unwrap().packs_compacted;
        }
        s.sync().unwrap();
        let mut r = space_rec(variant, "gc_begin", &format!("rule={rule:?}"), d.path());
        r.n("compactions", compacted as f64);
        r.emit();
    }
}

fn space_rec(variant: &str, part: &str, cfg: &str, dir: &std::path::Path) -> Rec {
    let files = data_files(dir);
    let logical: u64 = files.iter().map(|f| std::fs::metadata(f).unwrap().len()).sum();
    let alloc: u64 = files.iter().map(|f| sys::allocated(f).unwrap()).sum();
    let mut r = Rec::new("ab3", variant);
    r.s("part", part)
        .s("cfg", cfg)
        .n("arena_files", files.len() as f64)
        .n("logical_mib", logical as f64 / MIB as f64)
        .n("alloc_mib", alloc as f64 / MIB as f64);
    r
}

fn space_line(variant: &str, part: &str, cfg: &str, dir: &std::path::Path) {
    space_rec(variant, part, cfg, dir).emit();
}

/// A-B4: reopen of an active arena filled to `fill` % after a crash (no
/// idx) or after a close (resumable idx). `--method`: full | window | hole |
/// idx. Two timed opens: read-only (walk only) and read-write (walk + cut +
/// full_sync). The page cache is warm (the arena was just written).
fn ab4(variant: &str, a: &Args) {
    let size = 64 * MIB;
    let pctf = a.u64("fill", 50);
    let method = a.str("method", "window");
    let walk = match method.as_str() {
        "full" => Walk::FullRead,
        "hole" => Walk::HoleBounded,
        _ => Walk::Window,
    };
    let d = run_dir();
    let mut o = spike_opts(variant, size);
    o.walk = walk;
    {
        let s = SpikeStore::open(d.path(), o).unwrap();
        // 100 %: as full as it gets before the seal (within 64 KiB).
        let target = if pctf >= 100 { 4096 + size - 4096 - 64 * 1024 } else { 4096 + (size - 4096) * pctf / 100 };
        fill(&s, Some(target), 400 + pctf);
        s.sync().unwrap();
        if method == "idx" {
            s.close().unwrap();
        }
    }
    let mut ro = o;
    ro.read_only = true;
    let span = Span::start();
    let s = SpikeStore::open(d.path(), ro).unwrap();
    let walk_m = span.end();
    let objects = s.stats().objects;
    drop(s);
    let span = Span::start();
    let s = SpikeStore::open(d.path(), o).unwrap();
    let open_m = span.end();
    assert_eq!(s.stats().objects, objects);
    let mut r = Rec::new("ab4", variant);
    r.s("cfg", &format!("fill={pctf} method={method}"))
        .n("rep", a.u64("rep", 0) as f64)
        .n("objects", objects as f64)
        .n("walk_ms", walk_m.wall_s * 1e3)
        .n("walk_cpu_ms", walk_m.cpu_s() * 1e3)
        .n("open_ms", open_m.wall_s * 1e3)
        .n("open_cpu_ms", open_m.cpu_s() * 1e3)
        .n("cut_ms", (open_m.wall_s - walk_m.wall_s) * 1e3);
    r.emit();
}

/// A-B5: create + reserve + full_sync of `n` arenas (each deleted after).
fn ab5(variant: &str, a: &Args) {
    let p = Prealloc::parse(variant).unwrap();
    let n = a.u64("n", if p == Prealloc::Zeroed { 20 } else { 100 });
    let cfg = ArenaCfg { size: 64 * MIB, prealloc: p, granule: 1, trim_on_seal: false };
    let d = run_dir();
    let mut lat = Vec::new();
    let span = Span::start();
    for i in 1..=n {
        let t = Instant::now();
        let ar = Arena::create(d.path(), i, &cfg).unwrap();
        if !p.fixed() {
            // V0 defers the header sync to the first `sync`; count it here.
            sys::full_sync(ar.file()).unwrap();
        }
        lat.push(t.elapsed().as_secs_f64() * 1e3);
        let path = ar.path().to_owned();
        drop(ar);
        std::fs::remove_file(path).unwrap();
    }
    let m = span.end();
    let mut r = Rec::new("ab5", variant);
    r.s("cfg", &format!("n={n}")).n("rep", a.u64("rep", 0) as f64);
    r.m(&m)
        .n("create_p50_ms", pct(&mut lat.clone(), 50.0))
        .n("create_p99_ms", pct(&mut lat.clone(), 99.0))
        .n("create_max_ms", pct(&mut lat, 100.0))
        .n("cpu_per_create_ms", m.cpu_s() / n as f64 * 1e3);
    r.emit();
}
