//! Crash loop (plan section 4): process crashes by `kill -9`.
//!
//! `crashloop run --variant V1 [--kills 50] [--size_mib 4] [--seed 1]`
//!
//! The parent keeps one store directory for all kills. Each round it starts
//! a child (`crashloop child ...`) that opens the store (this runs the
//! recovery of the previous kill) and loops: put 8 objects (6 recorded as
//! roots, 2 garbage), `sync()`, append the 6 root ids to the side file
//! `roots` and `full_sync` it. Right after open and every `--gc_every`
//! (3) rounds it runs a GC with the root set as the live set (a `#gc` line
//! in the side file counts them; spike A ran it every 16 rounds, which a
//! child killed within 730 ms seldom reached) (the garbage is removed, small or mostly-dead arenas
//! are compacted). The parent sends SIGKILL at a random time, then opens
//! the store itself and checks that every recorded id `get()`s with bytes
//! whose BLAKE3 is the id (`get` checks the hash; the parent checks it
//! again). A torn last line of the side file is ignored (it was not yet
//! `full_sync`ed, so no root refers to it).
//!
//! Spike C (`--variant C1`, `C4`, ...: reuse mode, see `bench::reuse_opts`)
//! adds churn, so that holes are really reused: the side file has lines
//! `+<id> <seed> <i>` (a new root) and `-<id>` (a retired root). Each round
//! the child retires the oldest roots so at most `--live_cap` (default 400)
//! stay, and two of its six new roots are re-puts of objects retired (and
//! maybe freed and overwritten) earlier. The retire lines are durable
//! before the GC that drops them. The parent checks every root that is
//! live in the side file. `--churn 1` turns this on for V0-V3 too.
//!
//! Only process crashes are tested: power loss cannot be tested here.

use arena_spike::bench::{reuse_opts, spike_opts, Args};
use arena_spike::measure::Rec;
use arena_spike::workload::{self, Rng};
use arena_spike::{sys, GcToken, ObjectId, ReuseStore, SpikeStore};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MIB: u64 = 1 << 20;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let a = Args::parse(&argv);
    match argv.first().map(String::as_str) {
        Some("run") => run(&a),
        Some("child") => child(&a),
        _ => {
            eprintln!("usage: crashloop run --variant V1 [--kills 50] [--size_mib 4] [--seed 1]");
            std::process::exit(2);
        }
    }
}

fn hex(id: &ObjectId) -> String {
    id.0.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<ObjectId> {
    if s.len() != 56 {
        return None;
    }
    let mut b = [0u8; 28];
    for (i, x) in b.iter_mut().enumerate() {
        *x = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(ObjectId(b))
}

/// The live roots of complete lines, in order: `+id seed i` adds, `-id`
/// retires, a bare id (spike A format) adds. A torn last line (no newline)
/// is ignored. Also returns the retired (id, seed, i) that are not live.
fn roots_full(dir: &Path) -> (Vec<ObjectId>, Vec<(ObjectId, u64, usize)>) {
    let Ok(s) = fs::read_to_string(dir.join("roots")) else { return (Vec::new(), Vec::new()) };
    let complete = match s.rfind('\n') {
        Some(i) => &s[..=i],
        None => "",
    };
    let mut live: Vec<ObjectId> = Vec::new();
    let mut pos: std::collections::HashMap<ObjectId, (u64, usize)> = Default::default();
    let mut dead: std::collections::HashMap<ObjectId, (u64, usize)> = Default::default();
    let mut set: HashSet<ObjectId> = HashSet::new();
    for l in complete.lines() {
        if let Some(r) = l.strip_prefix('-') {
            if let Some(id) = unhex(r) {
                set.remove(&id);
                if let Some(x) = pos.get(&id) {
                    dead.insert(id, *x);
                }
            }
            continue;
        }
        let mut it = l.trim_start_matches('+').split(' ');
        let Some(id) = it.next().and_then(unhex) else { continue };
        let seed = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let i = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        pos.insert(id, (seed, i));
        dead.remove(&id);
        if set.insert(id) {
            live.push(id);
        }
    }
    live.retain(|id| set.contains(id));
    (live, dead.into_iter().map(|(id, (s, i))| (id, s, i)).collect())
}

fn roots(dir: &Path) -> Vec<ObjectId> {
    roots_full(dir).0
}

enum Store {
    A(SpikeStore),
    C(ReuseStore),
}

impl Store {
    fn open(dir: &Path, variant: &str, size: u64) -> Store {
        match reuse_opts(variant, size) {
            Some(o) => Store::C(ReuseStore::open(dir, o).unwrap()),
            None => Store::A(SpikeStore::open(dir, spike_opts(variant, size)).unwrap()),
        }
    }
    fn put(&self, d: &[u8]) -> ObjectId {
        match self {
            Store::A(s) => s.put(d).unwrap(),
            Store::C(s) => s.put(d).unwrap(),
        }
    }
    fn get(&self, id: &ObjectId) -> Option<Vec<u8>> {
        match self {
            Store::A(s) => s.get(id),
            Store::C(s) => s.get(id),
        }
    }
    fn sync(&self) {
        match self {
            Store::A(s) => s.sync().unwrap(),
            Store::C(s) => s.sync().unwrap(),
        }
    }
    fn gc(&self, live: &HashSet<ObjectId>) {
        match self {
            Store::A(s) => {
                let t: GcToken = s.gc_begin().unwrap();
                s.gc_finish(live, t).unwrap();
            }
            Store::C(s) => {
                let t = s.gc_begin().unwrap();
                s.gc_finish(live, t).unwrap();
            }
        }
    }
}

fn child(a: &Args) {
    let dir = Path::new(&a.0["dir"]).to_owned();
    let variant = a.str("variant", "V1");
    let size = a.u64("size_mib", 4) * MIB;
    let seed = a.u64("seed", 1);
    let churn = a.u64("churn", variant.starts_with('C') as u64) == 1;
    let cap = a.u64("live_cap", 400) as usize;
    let store = Store::open(&dir, &variant, size);
    let (mut order, retired) = roots_full(&dir);
    let mut retired: Vec<(ObjectId, u64, usize)> = retired;
    let mut live: HashSet<ObjectId> = order.iter().copied().collect();
    let mut meta: std::collections::HashMap<ObjectId, (u64, usize)> = Default::default();
    let mut side = OpenOptions::new().create(true).append(true).open(dir.join("roots")).unwrap();
    let mut rng = Rng::new(seed);
    let gc_every = a.u64("gc_every", 3);
    // A GC right after open (children live only a few rounds).
    store.gc(&live);
    side.write_all(b"#gc\n").unwrap();
    let mut round = 0u64;
    loop {
        let s = rng.next();
        let objs = workload::mixed(8, s);
        let mut line = String::new();
        for (i, o) in objs.iter().enumerate() {
            let id = store.put(o);
            if i < 6 {
                // With churn, two of the six roots are re-puts of retired ones.
                if churn && i >= 4 && !retired.is_empty() {
                    let k = rng.below(retired.len() as u64) as usize;
                    let (rid, rs, ri) = retired.swap_remove(k);
                    let d = &workload::mixed(8, rs)[ri];
                    assert_eq!(store.put(d), rid);
                    line.push_str(&format!("+{} {rs} {ri}\n", hex(&rid)));
                    if live.insert(rid) {
                        order.push(rid);
                    }
                    meta.insert(rid, (rs, ri));
                    continue;
                }
                line.push_str(&format!("+{} {s} {i}\n", hex(&id)));
                if live.insert(id) {
                    order.push(id);
                }
                meta.insert(id, (s, i));
            }
        }
        store.sync();
        if churn && order.len() > cap {
            let n = order.len() - cap;
            for id in order.drain(..n) {
                live.remove(&id);
                line.push_str(&format!("-{}\n", hex(&id)));
                if let Some(&(rs, ri)) = meta.get(&id) {
                    retired.push((id, rs, ri));
                }
            }
        }
        side.write_all(line.as_bytes()).unwrap();
        sys::full_sync(&side).unwrap();
        round += 1;
        if round % gc_every == 0 {
            store.gc(&live);
            side.write_all(b"#gc\n").unwrap();
        }
    }
}

fn run(a: &Args) {
    let variant = a.str("variant", "V1");
    let kills = a.u64("kills", 50);
    let size_mib = a.u64("size_mib", 4);
    let mut rng = Rng::new(a.u64("seed", 1) ^ 0xC4A5);
    let td = arena_spike::bench::run_dir();
    let dir = td.path().to_owned();
    let exe = std::env::current_exe().unwrap();
    let mut extra = Vec::new();
    for k in ["churn", "live_cap", "gc_every"] {
        if let Some(v) = a.0.get(k) {
            extra.push(format!("--{k}"));
            extra.push(v.clone());
        }
    }
    let t0 = Instant::now();
    let (mut checked, mut bad, mut max_roots, mut open_ms_max) = (0u64, 0u64, 0usize, 0f64);
    let mut child_fail = 0u64;
    for k in 0..kills {
        let mut c = Command::new(&exe)
            .args(["child", "--dir", dir.to_str().unwrap(), "--variant", &variant])
            .args(["--size_mib", &size_mib.to_string(), "--seed", &(rng.next() >> 1).to_string()])
            .args(extra.iter())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Give the child time to open (recovery), then kill at a random time.
        let wait = 30 + rng.below(a.u64("wait_ms", 700));
        std::thread::sleep(Duration::from_millis(wait));
        if let Ok(Some(st)) = c.try_wait() {
            // The child must not exit on its own.
            let mut err = String::new();
            use std::io::Read;
            c.stderr.take().unwrap().read_to_string(&mut err).unwrap();
            eprintln!("kill {k}: child exited early ({st}): {err}");
            child_fail += 1;
        } else {
            c.kill().unwrap(); // SIGKILL
            c.wait().unwrap();
        }
        let ids = roots(&dir);
        max_roots = max_roots.max(ids.len());
        let t = Instant::now();
        let store = Store::open(&dir, &variant, size_mib * MIB);
        open_ms_max = open_ms_max.max(t.elapsed().as_secs_f64() * 1e3);
        for id in &ids {
            checked += 1;
            match store.get(id) {
                Some(d) if ObjectId::of(&d) == *id => {}
                _ => {
                    bad += 1;
                    if bad <= 10 {
                        eprintln!("kill {k}: root {} missing or wrong", hex(id));
                    }
                }
            }
        }
        drop(store);
    }
    let arenas = fs::read_dir(dir.join("packs")).unwrap().filter(|e| {
        e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".arena")
    }).count();
    let mut r = Rec::new("crashloop", &variant);
    r.s("cfg", &format!("kills={kills} size_mib={size_mib}"))
        .n("kills", kills as f64)
        .n("child_early_exits", child_fail as f64)
        .n("roots_checked", checked as f64)
        .n("roots_bad", bad as f64)
        .n("roots_final", max_roots as f64)
        .n("arenas_final", arenas as f64)
        .n("reopen_ms_max", open_ms_max)
        .n("retired_final", roots_full(&dir).1.len() as f64)
        .n("gcs_done", fs::read_to_string(dir.join("roots")).unwrap_or_default().lines().filter(|l| *l == "#gc").count() as f64)
        .n("wall_s", t0.elapsed().as_secs_f64());
    r.emit();
    if bad > 0 || child_fail > 0 {
        std::process::exit(1);
    }
}
