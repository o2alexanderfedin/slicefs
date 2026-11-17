//! Spike C benches (plan section 6): cb1 churn (and cb4 sequential read
//! after churn), cb2 sync of N dirty arenas, cb3 reopen with dirty arenas.
//!
//! Variants: `RS` (real `ChunkStore` + compaction), `V1` (spike A arenas,
//! sparse, + compaction), `C<k>` (reuse with allocator V<k>, G = 64; see
//! `reuse_opts`).
//!
//! Fill level f (cb1): at every GC a fraction f of the store's data is live.
//! The store first gets `live_mib` of files; then each GC cycle writes
//! B = live * (1/f - 1) bytes of new files and deletes files of about the
//! same size, in 4 rounds, then runs a GC with the objects of the remaining
//! files as the live set. The same definition holds for every variant.

use super::{reuse_opts, run_dir, spike_opts, Args};
use crate::file_fork::{file_objects, FileReader, FileWriter, ObjStore};
use crate::measure::{median, pct, Rec, Span, Usage};
use crate::workload::{self, Rng};
use crate::{sys, GcStats, ObjectId, ReuseStore, SpikeStore};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

const MIB: u64 = 1 << 20;
const REC_OVER: u64 = 45;

pub fn run(bench: &str, variant: &str, a: &Args) -> bool {
    match bench {
        "cb1" => cb1(variant, a),
        "cb2" => cb2(variant, a),
        "cb3" => cb3(variant, a),
        "cb5" => cb5(variant, a),
        _ => return false,
    }
    true
}

enum Any {
    Rs(chunk_store::ChunkStore),
    A(SpikeStore),
    C(ReuseStore),
}

impl Any {
    fn open(variant: &str, dir: &Path, size: u64) -> Any {
        if variant == "RS" {
            let o = chunk_store::Options { pack_limit: size, ..Default::default() };
            return Any::Rs(chunk_store::ChunkStore::open(dir, o).unwrap());
        }
        if let Some(o) = reuse_opts(variant, size) {
            return Any::C(ReuseStore::open(dir, o).unwrap());
        }
        Any::A(SpikeStore::open(dir, spike_opts(variant, size)).unwrap())
    }

    fn s(&self) -> &dyn ObjStore {
        match self {
            Any::Rs(s) => s,
            Any::A(s) => s,
            Any::C(s) => s,
        }
    }

    fn gc(&self, live: &HashSet<ObjectId>) -> GcStats {
        match self {
            Any::Rs(s) => {
                let b = s.gc_begin().unwrap();
                s.gc_finish(live, b).unwrap()
            }
            Any::A(s) => {
                let t = s.gc_begin().unwrap();
                s.gc_finish(live, t).unwrap()
            }
            Any::C(s) => {
                let t = s.gc_begin().unwrap();
                s.gc_finish(live, t).unwrap()
            }
        }
    }

    fn stored_len(&self, id: &ObjectId) -> Option<u32> {
        match self {
            Any::Rs(s) => s.stored_len(id),
            Any::A(s) => s.stored_len(id),
            Any::C(s) => s.loc(id).map(|l| l.stored),
        }
    }

    /// Record bytes of the live set (stored + 45 each), the same measure
    /// for every variant (the index of a compacting store still holds dead
    /// objects of the arenas it kept).
    fn live_rec(&self, live: &HashSet<ObjectId>) -> u64 {
        live.iter().map(|id| self.stored_len(id).expect("live object missing") as u64 + REC_OVER).sum()
    }

    /// Bytes written so far, by the store's own counters (RS: none).
    fn io(&self) -> Option<crate::reuse::IoCounters> {
        match self {
            Any::Rs(_) => None,
            Any::A(s) => Some(s.io()),
            Any::C(s) => Some(s.io()),
        }
    }
}

/// RS has no byte counters: its packs and idx files are append-only /
/// written once, so the largest size seen of each file name is what was
/// written to it. Polled after every sync and around every GC.
#[derive(Default)]
struct DirPoll {
    max: HashMap<PathBuf, u64>,
}

impl DirPoll {
    fn poll(&mut self, dir: &Path) {
        for e in std::fs::read_dir(dir.join("packs")).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "pack" || x == "idx") {
                if let Ok(m) = e.metadata() {
                    let v = self.max.entry(p).or_insert(0);
                    *v = (*v).max(m.len());
                }
            }
        }
    }
    fn total(&self) -> u64 {
        self.max.values().sum()
    }
}

/// Allocated bytes (st_blocks * 512) of the store's data files.
fn allocated(dir: &Path) -> u64 {
    std::fs::read_dir(dir.join("packs"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack" || x == "arena" || x == "idx"))
        .map(|e| sys::allocated(&e.path()).unwrap_or(0))
        .sum()
}

struct FileRec {
    key: ObjectId,
    objs: Vec<ObjectId>,
    len: u64,
    seed: u64,
}

/// File contents: sizes log-uniform 64 KiB .. 4 MiB, half random, half 2:1.
fn file_bytes(seed: u64) -> Vec<u8> {
    let mut r = Rng::new(seed);
    let lg = 16.0 + (r.below(1_000_000) as f64 / 1e6) * 6.0;
    let len = 2f64.powf(lg) as usize;
    if seed % 2 == 0 {
        workload::random(len, seed)
    } else {
        workload::compressible(len, seed)
    }
}

struct Churn {
    sync_lat: Vec<f64>,
    syncs_per_commit: Vec<f64>,
    since_sync: u64,
    user_raw: u64,
}

impl Churn {
    fn write_file(&mut self, s: &dyn ObjStore, seed: u64) -> FileRec {
        let data = file_bytes(seed);
        let mut w = FileWriter::new();
        for piece in data.chunks(256 << 10) {
            w.write(s, piece).unwrap();
            self.since_sync += piece.len() as u64;
            if self.since_sync >= 8 * MIB {
                self.commit(s);
            }
        }
        let key = w.finish(s).unwrap();
        let objs = file_objects(s, key).unwrap();
        self.user_raw += data.len() as u64;
        FileRec { key, objs, len: data.len() as u64, seed }
    }

    fn commit(&mut self, s: &dyn ObjStore) {
        let f0 = sys::full_syncs();
        let t = Instant::now();
        s.sync().unwrap();
        self.sync_lat.push(t.elapsed().as_secs_f64() * 1e3);
        self.syncs_per_commit.push((sys::full_syncs() - f0) as f64);
        self.since_sync = 0;
    }
}

fn live_set(files: &[FileRec]) -> HashSet<ObjectId> {
    files.iter().flat_map(|f| f.objs.iter().copied()).collect()
}

fn cpu(u0: &Usage, u1: &Usage) -> f64 {
    ((u1.user_us - u0.user_us) + (u1.sys_us - u0.sys_us)) as f64 / 1e6
}

/// C-B1: churn at fill level `--fill` (0.85), `--live_mib` (512) of files,
/// `--churn` (1.5) * live bytes of new files after the first fill, sync
/// every 8 MiB, GC every 4 rounds. `--seqread 1` adds C-B4: sequential read
/// of every live file after the churn, and the same on a fresh store with
/// the same files.
fn cb1(variant: &str, a: &Args) {
    let live_target = a.u64("live_mib", 512) * MIB;
    let fill = a.f64("fill", 0.85);
    let churn = a.f64("churn", 1.5);
    let size = a.u64("size_mib", 64) * MIB;
    let seed0 = a.u64("seed", 11) * 1_000_003;
    let d = run_dir();
    let st = Any::open(variant, d.path(), size);
    let s = st.s();
    let mut rng = Rng::new(seed0 ^ 0xC1);
    let mut next_seed = seed0;
    let mut files: Vec<FileRec> = Vec::new();
    let mut c = Churn { sync_lat: Vec::new(), syncs_per_commit: Vec::new(), since_sync: 0, user_raw: 0 };
    let mut poll = DirPoll::default();
    let t_fill = Instant::now();
    let mut live_raw = 0u64;
    while live_raw < live_target {
        let f = c.write_file(s, next_seed);
        next_seed += 1;
        live_raw += f.len;
        files.push(f);
    }
    c.commit(s);
    poll.poll(d.path());
    let fill_s = t_fill.elapsed().as_secs_f64();
    // Churn phase: everything below is measured.
    c.sync_lat.clear();
    c.syncs_per_commit.clear();
    c.user_raw = 0;
    let io0 = st.io();
    let poll0 = poll.total();
    let fs0 = sys::full_syncs();
    let b = (live_target as f64 * (1.0 / fill - 1.0)) as u64;
    let round = (b / 4).max(1);
    let (mut gc_wall, mut gc_cpu, mut put_wall, mut put_cpu) = (0.0, 0.0, 0.0, 0.0);
    let (mut amp_pre, mut amp_post, mut nalloc) = (Vec::new(), Vec::new(), Vec::new());
    let mut user_rec = 0u64; // record bytes of new objects (unpadded)
    let mut gcs = 0u64;
    let mut gc_syncs = 0u64;
    let mut index_extra = Vec::new();
    let mut gcst = GcStats::default();
    let mut rounds = 0u64;
    let base = |s: &dyn ObjStore| {
        let x = s.stats();
        x.stored_bytes + REC_OVER * x.objects
    };
    while (c.user_raw as f64) < churn * live_target as f64 {
        let (t, u0) = (Instant::now(), Usage::now());
        let b0 = base(s);
        let mut w = 0;
        while w < round {
            let f = c.write_file(s, next_seed);
            next_seed += 1;
            w += f.len;
            files.push(f);
        }
        let mut del = 0;
        while del < round && files.len() > 1 {
            let i = rng.below(files.len() as u64 - 1) as usize; // never the newest
            del += files.swap_remove(i).len;
        }
        user_rec += base(s) - b0;
        put_wall += t.elapsed().as_secs_f64();
        put_cpu += cpu(&u0, &Usage::now());
        rounds += 1;
        if rounds % 4 == 0 {
            c.commit(s);
            poll.poll(d.path());
            let pre = allocated(d.path());
            let live = live_set(&files);
            let (t, u0, f0) = (Instant::now(), Usage::now(), sys::full_syncs());
            let g = st.gc(&live);
            gc_wall += t.elapsed().as_secs_f64();
            gc_cpu += cpu(&u0, &Usage::now());
            gc_syncs += sys::full_syncs() - f0;
            poll.poll(d.path());
            gcs += 1;
            gcst.removed_objects += g.removed_objects;
            gcst.packs_deleted += g.packs_deleted;
            gcst.packs_compacted += g.packs_compacted;
            let x = s.stats();
            let live_rec = st.live_rec(&live).max(1) as f64;
            amp_pre.push(pre as f64 / live_rec);
            amp_post.push(allocated(d.path()) as f64 / live_rec);
            nalloc.push(x.packs as f64);
            index_extra.push(x.objects as f64 - live.len() as f64);
        }
    }
    c.commit(s);
    poll.poll(d.path());
    let user_raw = c.user_raw;
    let half = |v: &[f64]| -> f64 {
        let h = &v[v.len() / 2..];
        h.iter().sum::<f64>() / h.len().max(1) as f64
    };
    let mut r = Rec::new("cb1", variant);
    r.s("cfg", &format!("live_mib={} fill={fill} churn={churn} size_mib={}", live_target / MIB, size / MIB))
        .n("rep", a.u64("rep", 0) as f64)
        .n("fill", fill)
        .n("fill_s", fill_s)
        .n("gcs", gcs as f64)
        .n("put_wall_s", put_wall)
        .n("put_cpu_s", put_cpu)
        .n("put_mb_s", user_raw as f64 / MIB as f64 / put_wall)
        .n("put_cpu_s_per_gib", put_cpu / (user_raw as f64 / (1u64 << 30) as f64))
        .n("gc_wall_s", gc_wall)
        .n("gc_cpu_s", gc_cpu)
        .n("commits", c.sync_lat.len() as f64)
        .n("sync_ms_mean", c.sync_lat.iter().sum::<f64>() / c.sync_lat.len().max(1) as f64)
        .n("sync_ms_p50", pct(&mut c.sync_lat.clone(), 50.0))
        .n("sync_ms_p99", pct(&mut c.sync_lat.clone(), 99.0))
        .n("removed_objects", gcst.removed_objects as f64)
        .n("packs_deleted", gcst.packs_deleted as f64)
        .n("packs_compacted", gcst.packs_compacted as f64)
        .n("amp_pre", half(&amp_pre))
        .n("amp_pre_max", amp_pre.iter().copied().fold(0.0, f64::max))
        .n("amp_post", half(&amp_post))
        .n("arenas", half(&nalloc))
        .n("user_raw_mib", user_raw as f64 / MIB as f64)
        .n("user_rec_mib", user_rec as f64 / MIB as f64);
    r.n("index_extra_objects", half(&index_extra));
    if variant != "RS" {
        // Commit only (the sync() call), and everything incl. GC per commit.
        let ncommit = c.sync_lat.len().max(1) as f64;
        r.n("commit_full_syncs_mean", c.syncs_per_commit.iter().sum::<f64>() / ncommit);
        r.n("commit_full_syncs_p50", median(&c.syncs_per_commit));
        r.n("commit_full_syncs_max", c.syncs_per_commit.iter().copied().fold(0.0, f64::max));
        r.n("gc_full_syncs", gc_syncs as f64);
        r.n("all_full_syncs_per_commit", (sys::full_syncs() - fs0) as f64 / ncommit);
    }
    match (io0, st.io()) {
        (Some(i0), Some(i1)) => {
            let w = (i1.total() - i0.total()) as f64;
            r.n("wa", w / user_rec.max(1) as f64)
                .n("wa_records", (i1.records - i0.records) as f64 / user_rec.max(1) as f64)
                .n("wa_padding", (i1.padding - i0.padding) as f64 / user_rec.max(1) as f64)
                .n("wa_copies", (i1.copies - i0.copies) as f64 / user_rec.max(1) as f64)
                .n("wa_idx", (i1.idx - i0.idx) as f64 / user_rec.max(1) as f64)
                .n("wa_dirty", (i1.dirty_list - i0.dirty_list) as f64 / user_rec.max(1) as f64)
                .n("dirty_writes", (i1.dirty_writes - i0.dirty_writes) as f64)
                .n("checkpoints", (i1.checkpoints - i0.checkpoints) as f64);
        }
        _ => {
            r.n("wa", (poll.total() - poll0) as f64 / user_rec.max(1) as f64).s("wa_method", "dir-poll");
        }
    }
    if a.u64("seqread", 0) == 1 {
        let (mbs, cpus) = seq_read(s, &files);
        r.n("seq_mb_s", mbs).n("seq_cpu_s_per_gib", cpus);
        // The same files, written once into a fresh store.
        let d2 = run_dir();
        let st2 = Any::open(variant, d2.path(), size);
        let mut c2 = Churn { sync_lat: Vec::new(), syncs_per_commit: Vec::new(), since_sync: 0, user_raw: 0 };
        let mut order: Vec<&FileRec> = files.iter().collect();
        order.sort_by_key(|f| f.seed);
        let fresh: Vec<FileRec> = order.iter().map(|f| c2.write_file(st2.s(), f.seed)).collect();
        c2.commit(st2.s());
        for (x, y) in fresh.iter().zip(order.iter()) {
            assert_eq!(x.key, y.key);
        }
        let fresh_sorted: Vec<FileRec> = files
            .iter()
            .map(|f| FileRec { key: f.key, objs: Vec::new(), len: f.len, seed: f.seed })
            .collect();
        let (mbs2, cpus2) = seq_read(st2.s(), &fresh_sorted);
        r.n("fresh_seq_mb_s", mbs2).n("fresh_seq_cpu_s_per_gib", cpus2);
    }
    r.emit();
}

/// Read every file front to back in 1 MiB pieces, in the order of `files`.
fn seq_read(s: &dyn ObjStore, files: &[FileRec]) -> (f64, f64) {
    let span = Span::start();
    let mut n = 0u64;
    for f in files {
        let mut rd = FileReader::open(s, f.key).unwrap();
        let mut off = 0;
        while off < f.len {
            let b = rd.read(s, off, MIB as usize).unwrap();
            assert!(!b.is_empty());
            off += b.len() as u64;
            n += b.len() as u64;
        }
    }
    let m = span.end();
    let gib = n as f64 / (1u64 << 30) as f64;
    (n as f64 / MIB as f64 / m.wall_s, m.cpu_s() / gib)
}

/// C-B2: one sync after writing 64 KiB into each of N dirty arenas (sparse
/// 64 MiB files). Variant = method: `full` (F_FULLFSYNC on each file) or
/// `fsync1` (fsync on each file, then one F_FULLFSYNC; measured only, the
/// store never relies on it). `--n` arenas, `--iters` syncs.
fn cb2(method: &str, a: &Args) {
    let n = a.u64("n", 4) as usize;
    let iters = a.u64("iters", 40);
    let d = run_dir();
    let files: Vec<_> = (0..n)
        .map(|i| {
            let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(d.path().join(format!("a{i}"))).unwrap();
            f.set_len(64 * MIB).unwrap();
            f
        })
        .collect();
    let buf = workload::random(64 << 10, 3);
    let mut rng = Rng::new(a.u64("rep", 0) + 5);
    let mut lat = Vec::new();
    let span = Span::start();
    for _ in 0..iters {
        for f in &files {
            let off = rng.below(1000) * (64 << 10);
            f.write_all_at(&buf, off).unwrap();
        }
        let t = Instant::now();
        match method {
            "full" => {
                for f in &files {
                    sys::full_sync(f).unwrap();
                }
            }
            _ => {
                for f in &files {
                    sys::plain_fsync(f).unwrap();
                }
                sys::full_sync(&files[n - 1]).unwrap();
            }
        }
        lat.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let m = span.end();
    let mut r = Rec::new("cb2", method);
    r.s("cfg", &format!("n={n} iters={iters}")).n("n", n as f64).n("rep", a.u64("rep", 0) as f64);
    r.m(&m)
        .n("sync_ms_p50", pct(&mut lat.clone(), 50.0))
        .n("sync_ms_p99", pct(&mut lat.clone(), 99.0))
        .n("sync_ms_mean", lat.iter().sum::<f64>() / lat.len() as f64)
        .n("cpu_ms_per_sync", m.cpu_s() / iters as f64 * 1e3);
    r.emit();
}

/// C-B3: reopen of a reuse store of `--arenas` (9) full 64 MiB arenas,
/// after a clean close and after a crash (the files as kill -9 leaves them)
/// with 1, 2 and 8 arenas in the dirty list. Each case is reopened `--reps_in`
/// (5) times in this process (page cache warm: no `purge` without root).
fn cb3(variant: &str, a: &Args) {
    let size = a.u64("size_mib", 64) * MIB;
    let narenas = a.u64("arenas", 9);
    let reps = a.u64("reps_in", 5);
    let o = reuse_opts(variant, size).expect("cb3 takes a C variant");
    let d = run_dir();
    let t0 = Instant::now();
    {
        let s = ReuseStore::open(d.path(), o).unwrap();
        let mut i = 0u64;
        while s.stats().packs < narenas {
            for obj in workload::mixed(64, 77 + i) {
                s.put(&obj).unwrap();
            }
            i += 1;
        }
        s.close().unwrap();
    }
    let build_s = t0.elapsed().as_secs_f64();
    for k in [0usize, 1, 2, 8] {
        {
            let s = ReuseStore::open(d.path(), o).unwrap();
            s.close().unwrap();
            if k > 0 {
                for n in s.arena_nums().into_iter().take(k) {
                    s.mark_dirty(n).unwrap();
                }
            }
            // dropped without close: a crash if k > 0
        }
        for rep in 0..reps {
            let span = Span::start();
            let (s, rep_o) = ReuseStore::open_report(d.path(), o).unwrap();
            let m = span.end();
            let objects = s.stats().objects;
            drop(s); // crash again: the dirty arenas stay dirty
            let mut r = Rec::new("cb3", variant);
            r.s("cfg", &format!("dirty={k} arenas={narenas}")).n("dirty", k as f64).n("rep", rep as f64);
            r.m(&m)
                .n("scanned", rep_o.scanned as f64)
                .n("scan_mib", rep_o.bytes_read as f64 / MIB as f64)
                .n("objects", objects as f64)
                .n("build_s", build_s);
            r.emit();
        }
    }
}

/// C-B5: churn under an APFS local snapshot (`tmutil localsnapshot`). Fill
/// `--live_mib` (256) of files, take a snapshot, churn `--churn` (1.0) x
/// live at 85 % with GCs, then delete the snapshot. Reports the volume's
/// free bytes (f_bavail, volume-wide, so other writers add noise) and the
/// store's st_blocks at each step. If `tmutil` refuses, the run says so.
fn cb5(variant: &str, a: &Args) {
    use std::process::Command;
    let live_target = a.u64("live_mib", 256) * MIB;
    let churn = a.f64("churn", 1.0);
    let fill = 0.85;
    let d = run_dir();
    let st = Any::open(variant, d.path(), 64 * MIB);
    let s = st.s();
    let mut c = Churn { sync_lat: Vec::new(), syncs_per_commit: Vec::new(), since_sync: 0, user_raw: 0 };
    let mut rng = Rng::new(55);
    let mut seed = 5_000_000u64;
    let mut files = Vec::new();
    let mut live_raw = 0;
    while live_raw < live_target {
        let f = c.write_file(s, seed);
        seed += 1;
        live_raw += f.len;
        files.push(f);
    }
    c.commit(s);
    let mut r = Rec::new("cb5", variant);
    let free0 = sys::free_bytes(d.path()).unwrap_or(0) as f64;
    let blocks0 = allocated(d.path()) as f64;
    let out = Command::new("tmutil").arg("localsnapshot").output();
    let (ok, msg) = match &out {
        Ok(o) => (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))),
        Err(e) => (false, e.to_string()),
    };
    let date = msg.split_whitespace().last().unwrap_or("").to_string();
    r.s("snapshot", if ok { "taken" } else { "not run" }).s("tmutil", &msg.trim().replace('\n', " "));
    if !ok {
        r.emit();
        return;
    }
    let free1 = sys::free_bytes(d.path()).unwrap_or(0) as f64;
    let b = (live_target as f64 * (1.0 / fill - 1.0)) as u64;
    let round = (b / 4).max(1);
    c.user_raw = 0;
    let mut rounds = 0;
    while (c.user_raw as f64) < churn * live_target as f64 {
        let mut w = 0;
        while w < round {
            let f = c.write_file(s, seed);
            seed += 1;
            w += f.len;
            files.push(f);
        }
        let mut del = 0;
        while del < round && files.len() > 1 {
            let i = rng.below(files.len() as u64 - 1) as usize;
            del += files.swap_remove(i).len;
        }
        rounds += 1;
        if rounds % 4 == 0 {
            c.commit(s);
            st.gc(&live_set(&files));
        }
    }
    c.commit(s);
    let free2 = sys::free_bytes(d.path()).unwrap_or(0) as f64;
    let blocks2 = allocated(d.path()) as f64;
    let del = Command::new("tmutil").args(["deletelocalsnapshots", &date]).output();
    let del_ok = del.as_ref().is_ok_and(|o| o.status.success());
    std::thread::sleep(std::time::Duration::from_secs(2));
    let free3 = sys::free_bytes(d.path()).unwrap_or(0) as f64;
    let m = MIB as f64;
    r.s("snapshot_date", &date)
        .s("delete", if del_ok { "ok" } else { "failed" })
        .n("user_raw_mib", c.user_raw as f64 / m)
        .n("store_blocks_before_mib", blocks0 / m)
        .n("store_blocks_after_mib", blocks2 / m)
        .n("free_drop_during_churn_mib", (free1 - free2) / m)
        .n("free_drop_total_mib", (free0 - free2) / m)
        .n("free_back_after_delete_mib", (free3 - free2) / m);
    r.emit();
}
