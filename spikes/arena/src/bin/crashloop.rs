//! Crash loop (plan section 4): process crashes by `kill -9`.
//!
//! `crashloop run --variant V1 [--kills 50] [--size_mib 4] [--seed 1]`
//!
//! The parent keeps one store directory for all kills. Each round it starts
//! a child (`crashloop child ...`) that opens the store (this runs the
//! recovery of the previous kill) and loops: put 8 objects (6 recorded as
//! roots, 2 garbage), `sync()`, append the 6 root ids to the side file
//! `roots` and `full_sync` it. Every 16 rounds it runs a GC with the root
//! set as the live set (the garbage is removed, small or mostly-dead arenas
//! are compacted). The parent sends SIGKILL at a random time, then opens
//! the store itself and checks that every recorded id `get()`s with bytes
//! whose BLAKE3 is the id (`get` checks the hash; the parent checks it
//! again). A torn last line of the side file is ignored (it was not yet
//! `full_sync`ed, so no root refers to it).
//!
//! Only process crashes are tested: power loss cannot be tested here.

use arena_spike::bench::{spike_opts, Args};
use arena_spike::measure::Rec;
use arena_spike::workload::{self, Rng};
use arena_spike::{sys, ObjectId, SpikeStore};
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

/// Ids of complete lines (a torn last line has no newline, or is short).
fn roots(dir: &Path) -> Vec<ObjectId> {
    let Ok(s) = fs::read_to_string(dir.join("roots")) else { return Vec::new() };
    let complete = match s.rfind('\n') {
        Some(i) => &s[..=i],
        None => "",
    };
    complete.lines().filter_map(unhex).collect()
}

fn child(a: &Args) {
    let dir = Path::new(&a.0["dir"]).to_owned();
    let variant = a.str("variant", "V1");
    let size = a.u64("size_mib", 4) * MIB;
    let seed = a.u64("seed", 1);
    let store = SpikeStore::open(&dir, spike_opts(&variant, size)).unwrap();
    let mut live: HashSet<ObjectId> = roots(&dir).into_iter().collect();
    let mut side = OpenOptions::new().create(true).append(true).open(dir.join("roots")).unwrap();
    let mut rng = Rng::new(seed);
    let mut round = 0u64;
    loop {
        let objs = workload::mixed(8, rng.next());
        let mut ids = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            let id = store.put(o).unwrap();
            if i < 6 {
                ids.push(id);
            }
        }
        store.sync().unwrap();
        let mut line = String::new();
        for id in &ids {
            line.push_str(&hex(id));
            line.push('\n');
        }
        side.write_all(line.as_bytes()).unwrap();
        sys::full_sync(&side).unwrap();
        live.extend(ids);
        round += 1;
        if round % 16 == 0 {
            let t = store.gc_begin().unwrap();
            store.gc_finish(&live, t).unwrap();
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
    let t0 = Instant::now();
    let (mut checked, mut bad, mut max_roots, mut open_ms_max) = (0u64, 0u64, 0usize, 0f64);
    let mut child_fail = 0u64;
    for k in 0..kills {
        let mut c = Command::new(&exe)
            .args(["child", "--dir", dir.to_str().unwrap(), "--variant", &variant])
            .args(["--size_mib", &size_mib.to_string(), "--seed", &(rng.next() >> 1).to_string()])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Give the child time to open (recovery), then kill at a random time.
        let wait = 30 + rng.below(700);
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
        let store = SpikeStore::open(&dir, spike_opts(&variant, size_mib * MIB)).unwrap();
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
        .n("wall_s", t0.elapsed().as_secs_f64());
    r.emit();
    if bad > 0 || child_fail > 0 {
        std::process::exit(1);
    }
}
