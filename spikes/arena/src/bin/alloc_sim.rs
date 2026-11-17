//! C-sim (plan section 6): the allocator grid on a trace, no I/O.
//!
//! `alloc_sim --corpus <dir> | --synthetic [--live_mib 1024] [--churn 10]
//!            [--size_mib 64] [--threads 8] [--out results/c/sim.jsonl] [--quick]`
//!
//! Trace: the regular files of the corpus in sorted path order, cut with
//! FastCDC 2/8/32 KiB, each chunk compressed with zstd-3 (kept if it saves
//! 1/8, as the store does). A file also has manifest leaf nodes of
//! 9 + 36 * entries bytes (1024 entries per leaf, plus one level-1 node when
//! there is more than one leaf). The trace is replayed in passes; each pass
//! salts the chunk hashes, so dedup is within a pass only (files that share
//! chunks, identical files).
//!
//! Model: start with live = `live_mib` of record bytes. Then cycles: write
//! B = live * (1/f - 1) of new files, delete files (file mode: a uniform
//! random live file; shared chunks are counted by reference) until B record
//! bytes became garbage, then GC. So at each GC a fraction f of the store's
//! record bytes is live ("fill level f"). Object mode (adversarial): the
//! objects are independent and a uniform random live object is deleted.
//! Stop after `churn` * live bytes written.
//!
//! Reuse (V1/V2/V3 at granule G): records padded to G; GC frees garbage
//! extents (coalesced), an arena without live records is deleted; V3 also
//! evacuates arenas under 10 % live (copies counted). A record that fits
//! nowhere opens a new arena. Compaction ("CMP", G = 1): append to the
//! active arena, seal at the size; at GC an arena with no live record is
//! deleted, one under 1/2 live or small (< size/8) is copied into the
//! active arena (today's rules; the active arena is skipped).
//!
//! Metrics, averaged over the GCs of the second half of the run:
//! pre_amp (touched bytes (hwm) / live record bytes just before a GC: the
//! peak disk use of the cycle), space_amp (the same just after the GC), cap_amp (arenas *
//! size / live), padding % (of live padded bytes), unusable % (free extents
//! shorter than the smallest padded record, of arena capacity), dirty
//! arenas per 8 MiB commit (mean and max), evacuation and compaction copies,
//! WA (bytes placed incl. copies and padding / user record bytes).

use arena_spike::alloc::{self, Allocator};
use arena_spike::bench::Args;
use arena_spike::measure::{median, Rec};
use arena_spike::workload::{self, Rng};
use arena_spike::{chunk_ranges, AllocKind};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const HDR: u64 = 4096;
const MIB: u64 = 1 << 20;
const REC_OVER: u64 = 45; // record header (41) + CRC (4)
const LEAF: usize = 1024;

#[derive(Clone)]
struct FileT {
    /// (hash, stored bytes)
    chunks: Vec<(u64, u32)>,
}

fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
    x ^ (x >> 33)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut ents: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    ents.sort_by_key(|e| e.file_name());
    for e in ents {
        let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
        if m.is_dir() {
            walk(&e.path(), out);
        } else if m.is_file() {
            out.push(e.path());
        }
    }
}

fn stored_len(c: &[u8]) -> u32 {
    if c.len() >= 64 {
        if let Ok(z) = zstd::bulk::compress(c, 3) {
            if z.len() <= c.len() - c.len() / 8 {
                return z.len() as u32;
            }
        }
    }
    c.len() as u32
}

fn corpus_trace(dir: &Path) -> (Vec<FileT>, u64) {
    let mut paths = Vec::new();
    walk(dir, &mut paths);
    let mut raw = 0;
    let mut files = Vec::with_capacity(paths.len());
    for p in paths {
        let Ok(d) = std::fs::read(&p) else { continue };
        raw += d.len() as u64;
        let chunks = chunk_ranges(&d)
            .into_iter()
            .map(|r| {
                let c = &d[r];
                let h = u64::from_le_bytes(blake3::hash(c).as_bytes()[..8].try_into().unwrap());
                (h, stored_len(c))
            })
            .collect();
        files.push(FileT { chunks });
    }
    (files, raw)
}

/// Seeded synthetic files: sizes log-uniform 2 KiB .. 4 MiB, FastCDC chunk
/// sizes, half the files random (stored = raw), half about 2:1.
fn synthetic_trace(total: u64, seed: u64) -> (Vec<FileT>, u64) {
    let sizes = workload::cdc_sizes(seed);
    let mut r = Rng::new(seed);
    let mut files = Vec::new();
    let (mut raw, mut k) = (0u64, 0usize);
    while raw < total {
        let lg = 11.0 + (r.below(1_000_000) as f64 / 1e6) * 11.0;
        let mut left = 2f64.powf(lg) as u64;
        let zip = r.below(2) == 1;
        let mut chunks = Vec::new();
        while left > 0 {
            let n = (sizes[k % sizes.len()] as u64).min(left);
            k += 1;
            left -= n;
            raw += n;
            let st = if zip && n >= 64 { (n / 2) as u32 } else { n as u32 };
            chunks.push((r.next(), st));
        }
        files.push(FileT { chunks });
    }
    (files, raw)
}

// ── Simulated store ───────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Obj {
    hash: u64,
    rec: u64,
    pad: u64,
    refc: u32,
    arena: u64,
    off: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    File,
    Object,
}

#[derive(Clone, Copy, Debug)]
struct Cfg {
    policy: Option<AllocKind>, // None = compaction
    g: u64,
    mode: Mode,
    fill: f64,
    live: u64,
    churn: f64,
    size: u64,
    evac: Option<f64>,
}

impl Cfg {
    fn name(&self) -> String {
        self.policy.map_or("CMP", |p| p.name()).into()
    }
}

#[derive(Default)]
struct CArena {
    hwm: u64,
    total: u64,
    live: u64,
    objs: Vec<u32>,
}

struct Sim<'a> {
    cfg: Cfg,
    trace: &'a [FileT],
    objs: Vec<Obj>,
    slots: Vec<u32>,
    map: HashMap<u64, u32>,
    files: Vec<Vec<u32>>,
    garbage: Vec<u32>,
    // reuse
    alloc: Option<Box<dyn Allocator>>,
    // per arena: (live padded bytes, object slots (lazy), hwm)
    arenas: BTreeMap<u64, CArena>,
    active: Option<u64>,
    next_arena: u64,
    // trace position
    pos: usize,
    // counters
    live_rec: u64,
    user_bytes: u64,
    placed: u64,
    copies: u64,
    commit_bytes: u64,
    commit_set: HashSet<u64>,
    commits: Vec<f64>,
    min_pad: u64,
    rng: Rng,
}

impl<'a> Sim<'a> {
    fn new(cfg: Cfg, trace: &'a [FileT], seed: u64) -> Self {
        Sim {
            cfg,
            trace,
            objs: Vec::new(),
            slots: Vec::new(),
            map: HashMap::new(),
            files: Vec::new(),
            garbage: Vec::new(),
            alloc: cfg.policy.map(alloc::new),
            arenas: BTreeMap::new(),
            active: None,
            next_arena: 1,
            pos: 0,
            live_rec: 0,
            user_bytes: 0,
            placed: 0,
            copies: 0,
            commit_bytes: 0,
            commit_set: HashSet::new(),
            commits: Vec::new(),
            min_pad: u64::MAX,
            rng: Rng::new(seed),
        }
    }

    fn g(&self) -> u64 {
        if self.cfg.policy.is_none() {
            1
        } else {
            self.cfg.g
        }
    }

    /// Place `len` bytes; returns (arena, off).
    fn place(&mut self, len: u64) -> (u64, u64) {
        self.placed += len;
        let size = self.cfg.size;
        if let Some(a) = self.alloc.as_mut() {
            loop {
                if let Some(x) = a.alloc(len) {
                    let ar = self.arenas.get_mut(&x.0).unwrap();
                    ar.hwm = ar.hwm.max(x.1 + len);
                    return x;
                }
                let n = self.next_arena;
                self.next_arena += 1;
                a.add_arena(n, HDR, size);
                self.arenas.insert(n, CArena { hwm: HDR, ..Default::default() });
            }
        }
        loop {
            if let Some(n) = self.active {
                let ar = self.arenas.get_mut(&n).unwrap();
                if ar.hwm + len <= size {
                    let off = ar.hwm;
                    ar.hwm += len;
                    return (n, off);
                }
            }
            let n = self.next_arena;
            self.next_arena += 1;
            self.arenas.insert(n, CArena { hwm: HDR, ..Default::default() });
            self.active = Some(n);
        }
    }

    fn put(&mut self, hash: u64, stored: u32) -> u32 {
        if let Some(&s) = self.map.get(&hash) {
            let o = &mut self.objs[s as usize];
            if o.refc == 0 {
                self.live_rec += o.rec;
            }
            o.refc += 1;
            return s;
        }
        let rec = stored as u64 + REC_OVER;
        let g = self.g();
        let pad = rec.div_ceil(g) * g;
        self.min_pad = self.min_pad.min(pad);
        let (arena, off) = self.place(pad);
        self.user_bytes += rec;
        self.commit_set.insert(arena);
        self.commit_bytes += pad;
        if self.commit_bytes >= 8 * MIB {
            self.commits.push(self.commit_set.len() as f64);
            self.commit_set.clear();
            self.commit_bytes = 0;
        }
        let o = Obj { hash, rec, pad, refc: 1, arena, off };
        let s = match self.slots.pop() {
            Some(s) => {
                self.objs[s as usize] = o;
                s
            }
            None => {
                self.objs.push(o);
                (self.objs.len() - 1) as u32
            }
        };
        self.map.insert(hash, s);
        let ar = self.arenas.get_mut(&arena).unwrap();
        ar.total += pad;
        ar.live += pad;
        ar.objs.push(s);
        self.live_rec += rec;
        s
    }

    fn unref(&mut self, s: u32) -> u64 {
        let o = &mut self.objs[s as usize];
        o.refc -= 1;
        if o.refc == 0 {
            self.live_rec -= o.rec;
            self.garbage.push(s);
            o.rec
        } else {
            0
        }
    }

    /// Add the next trace file (file mode) or the next chunk (object mode).
    fn add_next(&mut self) -> u64 {
        let before = self.user_bytes;
        let nf = self.trace.len();
        let pass = (self.pos / nf) as u64;
        let f = &self.trace[self.pos % nf];
        self.pos += 1;
        let salt = mix(pass.wrapping_add(0x1234));
        match self.cfg.mode {
            Mode::File => {
                let mut slots = Vec::with_capacity(f.chunks.len() + 1);
                let mut leaves = Vec::new();
                let mut acc = salt;
                for (i, &(h, st)) in f.chunks.iter().enumerate() {
                    slots.push(self.put(mix(h ^ salt), st));
                    acc = mix(acc ^ h);
                    if (i + 1) % LEAF == 0 || i + 1 == f.chunks.len() {
                        let n = (i % LEAF) + 1;
                        leaves.push((acc, 9 + 36 * n as u32));
                    }
                }
                if f.chunks.is_empty() {
                    leaves.push((salt ^ 0xabc, 9));
                }
                let nl = leaves.len();
                for (h, st) in leaves {
                    slots.push(self.put(h, st));
                }
                if nl > 1 {
                    slots.push(self.put(mix(acc ^ 0x77), 9 + 36 * nl as u32));
                }
                self.files.push(slots);
            }
            Mode::Object => {
                for (i, &(_, st)) in f.chunks.iter().enumerate() {
                    // independent objects: unique hash per position and pass
                    let s = self.put(mix((self.pos as u64) << 20 ^ i as u64 ^ salt), st);
                    self.files.push(vec![s]);
                }
            }
        }
        self.user_bytes - before
    }

    fn delete_random(&mut self) -> u64 {
        if self.files.is_empty() {
            return 0;
        }
        let i = self.rng.below(self.files.len() as u64) as usize;
        let f = self.files.swap_remove(i);
        f.into_iter().map(|s| self.unref(s)).sum()
    }

    fn gc(&mut self) {
        let g = std::mem::take(&mut self.garbage);
        let mut touched: HashSet<u64> = HashSet::new();
        for s in g {
            let o = self.objs[s as usize];
            if o.refc > 0 || self.map.get(&o.hash) != Some(&s) {
                continue;
            }
            self.map.remove(&o.hash);
            self.slots.push(s);
            self.objs[s as usize].refc = u32::MAX; // freed marker
            let ar = self.arenas.get_mut(&o.arena).unwrap();
            ar.live -= o.pad;
            touched.insert(o.arena);
            if let Some(a) = self.alloc.as_mut() {
                a.free(o.arena, o.off, o.pad);
            }
        }
        if self.alloc.is_some() {
            self.gc_reuse(touched);
        } else {
            self.gc_compact();
        }
    }

    fn live_objs(&self, n: u64) -> Vec<u32> {
        let ar = &self.arenas[&n];
        let mut seen = HashSet::new();
        ar.objs
            .iter()
            .copied()
            .filter(|&s| {
                let o = &self.objs[s as usize];
                o.refc != u32::MAX && o.arena == n && self.map.get(&o.hash) == Some(&s) && seen.insert(s)
            })
            .collect()
    }

    fn move_obj(&mut self, s: u32) {
        let pad = self.objs[s as usize].pad;
        let (na, no) = self.place(pad);
        self.copies += pad;
        let o = &mut self.objs[s as usize];
        o.arena = na;
        o.off = no;
        let ar = self.arenas.get_mut(&na).unwrap();
        ar.total += pad;
        ar.live += pad;
        ar.objs.push(s);
    }

    fn gc_reuse(&mut self, _touched: HashSet<u64>) {
        let size = self.cfg.size;
        let nums: Vec<u64> = self.arenas.keys().copied().collect();
        for n in nums {
            let live = self.arenas[&n].live;
            let a = self.alloc.as_mut().unwrap();
            if live == 0 {
                a.remove_arena(n);
                self.arenas.remove(&n);
                continue;
            }
            if let Some(e) = self.cfg.evac {
                if (live as f64) < e * (size - HDR) as f64 {
                    a.remove_arena(n);
                    a.set_bump_only(true);
                    for s in self.live_objs(n) {
                        self.move_obj(s);
                    }
                    self.alloc.as_mut().unwrap().set_bump_only(false);
                    self.arenas.remove(&n);
                    continue;
                }
            }
            // compact the lazy object list now and then
            let ar = &self.arenas[&n];
            if ar.objs.len() > 4 * (ar.live / 2048 + 16) as usize {
                let l = self.live_objs(n);
                self.arenas.get_mut(&n).unwrap().objs = l;
            }
        }
    }

    fn gc_compact(&mut self) {
        let size = self.cfg.size;
        let nums: Vec<u64> = self.arenas.keys().copied().filter(|&n| Some(n) != self.active).collect();
        for n in nums {
            let (live, total) = (self.arenas[&n].live, self.arenas[&n].total);
            let small = total < size / 8;
            if live > 0 && live * 2 >= total && !small {
                continue;
            }
            for s in self.live_objs(n) {
                self.move_obj(s);
            }
            self.arenas.remove(&n);
        }
    }

    fn touched_bytes(&self) -> u64 {
        self.arenas.values().map(|a| a.hwm - HDR).sum()
    }

    fn live_pad(&self) -> u64 {
        self.arenas.values().map(|a| a.live).sum()
    }
}

#[derive(Default)]
struct Sample {
    /// Just before the GC (the peak of the cycle).
    pre_amp: f64,
    space_amp: f64,
    cap_amp: f64,
    padding: f64,
    unusable: f64,
    /// Free (not bump) bytes in extents shorter than the mean live record.
    small_holes: f64,
    /// Free bytes that are not bump space.
    holes: f64,
    arenas: f64,
}

fn run(cfg: Cfg, trace: &[FileT], seed: u64) -> Rec {
    let mut s = Sim::new(cfg, trace, seed);
    while s.live_rec < cfg.live {
        s.add_next();
    }
    let user0 = s.user_bytes;
    let placed0 = s.placed;
    let b = (cfg.live as f64 * (1.0 / cfg.fill - 1.0)) as u64;
    let mut samples = Vec::new();
    s.commits.clear();
    while ((s.user_bytes - user0) as f64) < cfg.churn * cfg.live as f64 {
        let mut w = 0;
        while w < b {
            w += s.add_next();
        }
        let mut d = 0;
        while d < b && !s.files.is_empty() {
            d += s.delete_random();
        }
        let pre_amp = s.touched_bytes() as f64 / s.live_rec.max(1) as f64;
        s.gc();
        let live = s.live_rec.max(1) as f64;
        let lp = s.live_pad();
        let st = s.alloc.as_ref().map(|a| a.stats(s.min_pad)).unwrap_or_default();
        let nlive = s.map.len().max(1) as u64;
        let mean = (lp / nlive).max(1);
        let small = s.alloc.as_ref().map(|a| a.stats(mean)).unwrap_or_default();
        let cap = s.arenas.len() as f64 * (cfg.size - HDR) as f64;
        samples.push(Sample {
            pre_amp,
            space_amp: s.touched_bytes() as f64 / live,
            cap_amp: cap / live,
            padding: if lp > 0 { (lp as f64 - s.live_rec as f64) / lp as f64 * 100.0 } else { 0.0 },
            unusable: st.unusable_bytes as f64 / cap.max(1.0) * 100.0,
            small_holes: small.unusable_bytes as f64 / cap.max(1.0) * 100.0,
            holes: (st.free_bytes - st.bump_bytes) as f64 / cap.max(1.0) * 100.0,
            arenas: s.arenas.len() as f64,
        });
    }
    let h = &samples[samples.len() / 2..];
    let avg = |f: &dyn Fn(&Sample) -> f64| h.iter().map(f).sum::<f64>() / h.len().max(1) as f64;
    let user = (s.user_bytes - user0) as f64;
    let mut r = Rec::new("csim", &cfg.name());
    r.n("g", cfg.g as f64)
        .s("mode", if cfg.mode == Mode::File { "file" } else { "object" })
        .n("fill", cfg.fill)
        .n("evac", cfg.evac.unwrap_or(0.0))
        .n("gcs", samples.len() as f64)
        .n("pre_amp", avg(&|x| x.pre_amp))
        .n("pre_amp_max", h.iter().map(|x| x.pre_amp).fold(0.0, f64::max))
        .n("space_amp", avg(&|x| x.space_amp))
        .n("space_amp_max", h.iter().map(|x| x.space_amp).fold(0.0, f64::max))
        .n("cap_amp", avg(&|x| x.cap_amp))
        .n("padding_pct", avg(&|x| x.padding))
        .n("unusable_pct", avg(&|x| x.unusable))
        .n("small_holes_pct", avg(&|x| x.small_holes))
        .n("holes_pct", avg(&|x| x.holes))
        .n("arenas", avg(&|x| x.arenas))
        .n("dirty_per_commit", s.commits.iter().sum::<f64>() / s.commits.len().max(1) as f64)
        .n("dirty_per_commit_p50", median(&s.commits))
        .n("dirty_per_commit_max", s.commits.iter().copied().fold(0.0, f64::max))
        .n("copies_mib", s.copies as f64 / MIB as f64)
        .n("wa", (s.placed - placed0) as f64 / user)
        .n("user_mib", user / MIB as f64);
    r
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let a = Args::parse(&argv);
    let live = a.u64("live_mib", 1024) * MIB;
    let size = a.u64("size_mib", 64) * MIB;
    let churn = a.f64("churn", 10.0);
    let seed = a.u64("seed", 1);
    let threads = a.u64("threads", 8) as usize;
    let out = PathBuf::from(a.str("out", "results/c/sim.jsonl"));
    let t0 = std::time::Instant::now();
    let (trace, raw, source) = if let Some(c) = a.0.get("corpus") {
        let (t, raw) = corpus_trace(Path::new(c));
        (t, raw, format!("corpus:{c}"))
    } else {
        let (t, raw) = synthetic_trace(a.u64("synthetic_mib", 512) * MIB, seed);
        (t, raw, "synthetic".to_string())
    };
    let nch: usize = trace.iter().map(|f| f.chunks.len()).sum();
    let st: u64 = trace.iter().flat_map(|f| f.chunks.iter()).map(|c| c.1 as u64).sum();
    eprintln!(
        "trace {source}: {} files, {nch} chunks, raw {:.1} MiB, stored {:.1} MiB, built in {:.1} s",
        trace.len(),
        raw as f64 / MIB as f64,
        st as f64 / MIB as f64,
        t0.elapsed().as_secs_f64()
    );
    let trace = Arc::new(trace);
    let gs: Vec<u64> = if a.0.contains_key("quick") { vec![64, 4096] } else { vec![64, 256, 512, 4096] };
    let fills = [0.5, 0.7, 0.85, 0.95];
    let mut cfgs = Vec::new();
    for mode in [Mode::File, Mode::Object] {
        for &fill in &fills {
            let base = Cfg { policy: None, g: 1, mode, fill, live, churn, size, evac: None };
            cfgs.push(base);
            for &g in &gs {
                for p in [AllocKind::NextFit, AllocKind::SizeClasses, AllocKind::Hybrid, AllocKind::ArenaBest] {
                    let evac = (p == AllocKind::Hybrid).then_some(0.10);
                    if a.0.contains_key("only_g64") && g != 64 && p == AllocKind::ArenaBest {
                        continue;
                    }
                    cfgs.push(Cfg { policy: Some(p), g, evac, ..base });
                }
            }
        }
    }
    let queue = Arc::new(Mutex::new(cfgs));
    let results = Arc::new(Mutex::new(Vec::new()));
    std::thread::scope(|sc| {
        for _ in 0..threads {
            let (q, res, tr, src) = (queue.clone(), results.clone(), trace.clone(), source.clone());
            sc.spawn(move || loop {
                let Some(c) = q.lock().unwrap().pop() else { break };
                let t = std::time::Instant::now();
                let mut r = run(c, &tr, seed);
                r.s("source", &src).n("live_mib", (c.live / MIB) as f64).n("size_mib", (c.size / MIB) as f64);
                r.n("churn", c.churn).n("sim_s", t.elapsed().as_secs_f64());
                eprintln!("{}", r.to_json());
                res.lock().unwrap().push(r);
            });
        }
    });
    if let Some(p) = out.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&out).unwrap();
    use std::io::Write;
    let res = results.lock().unwrap();
    for r in res.iter() {
        writeln!(f, "{}", r.to_json()).unwrap();
    }
    eprintln!("{} configs in {:.1} s -> {}", res.len(), t0.elapsed().as_secs_f64(), out.display());
    table(&res);
}

/// Markdown table of the grid on stdout.
fn table(res: &[Rec]) {
    let mut v: Vec<&Rec> = res.iter().collect();
    let key = |r: &Rec| {
        (
            r.str("mode").unwrap_or("").to_string(),
            (r.num("fill").unwrap_or(0.0) * 100.0) as u64,
            r.str("variant").unwrap_or("").to_string(),
            r.num("g").unwrap_or(0.0) as u64,
        )
    };
    v.sort_by_key(|r| key(r));
    println!("| mode | fill | alloc | G | pre_amp | pre_amp_max | post_amp | padding % | unusable % | pad+unus % | holes % | holes < mean rec % | dirty/commit mean | p50 | max | copies MiB | WA |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for r in v {
        let n = |k: &str| r.num(k).unwrap_or(f64::NAN);
        println!(
            "| {} | {:.0} | {} | {} | {:.3} | {:.3} | {:.3} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.0} | {:.0} | {:.0} | {:.3} |",
            r.str("mode").unwrap_or(""),
            n("fill") * 100.0,
            r.str("variant").unwrap_or(""),
            n("g"),
            n("pre_amp"),
            n("pre_amp_max"),
            n("space_amp"),
            n("padding_pct"),
            n("unusable_pct"),
            n("padding_pct") + n("unusable_pct"),
            n("holes_pct"),
            n("small_holes_pct"),
            n("dirty_per_commit"),
            n("dirty_per_commit_p50"),
            n("dirty_per_commit_max"),
            n("copies_mib"),
            n("wa"),
        );
    }
}
