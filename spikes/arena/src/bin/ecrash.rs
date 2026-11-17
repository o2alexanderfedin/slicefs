//! Spike E crash loop: `kill -9` of a writer that puts through the mmap path.
//!
//! `ecrash run [--kills 40] [--size_mib 8] [--seed 1] [--mode mmap]`
//!
//! One directory for all kills. Each round a child (`ecrash child`) resumes
//! the newest arena (`Writer::open`: recovery walk and cut) or makes arena
//! 1, then loops: put 8 objects, `sync` (msync + F_FULLFSYNC), append their
//! ids to the side file `synced` and `F_FULLFSYNC` it. Small arenas, so the
//! child also rotates. The parent kills the child at a random time, then:
//! 1. opens each arena read-only (no cut) and records the end of the last
//!    good record and how many nonzero bytes follow it (the torn tail);
//! 2. opens each arena for real (the cut runs) and checks that every synced
//!    id reads back with `pread` (CRC and BLAKE3), and that every record the
//!    walk found reads back too.
//! Only process crashes are tested; power loss cannot be tested here.

use arena_spike::arena::{self, Arena, Recovery};
use arena_spike::bench::Args;
use arena_spike::measure::Rec;
use arena_spike::mmap_write::{Mode, Writer};
use arena_spike::workload::{self, Rng};
use arena_spike::{sys, ArenaCfg, ObjectId, Prealloc, Walk};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
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
            eprintln!("usage: ecrash run [--kills 40] [--size_mib 8] [--seed 1] [--mode mmap]");
            std::process::exit(2);
        }
    }
}

fn cfg(size_mib: u64) -> ArenaCfg {
    ArenaCfg { size: size_mib * MIB, prealloc: Prealloc::Reserve, granule: 1, trim_on_seal: false }
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

fn synced(dir: &Path) -> Vec<ObjectId> {
    let Ok(s) = fs::read_to_string(dir.join("synced")) else { return Vec::new() };
    let complete = match s.rfind('\n') {
        Some(i) => &s[..=i],
        None => "",
    };
    complete.lines().filter_map(unhex).collect()
}

fn arenas(dir: &Path) -> Vec<u64> {
    let mut v: Vec<u64> = fs::read_dir(dir)
        .map(|r| {
            r.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.strip_suffix(".arena").and_then(|h| u64::from_str_radix(h, 16).ok())
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_unstable();
    v
}

fn child(a: &Args) {
    let dir = Path::new(&a.0["dir"]).to_owned();
    let size = a.u64("size_mib", 8);
    let mode = Mode::parse(&a.str("mode", "mmap")).unwrap();
    let mut w = match arenas(&dir).last() {
        Some(&n) => Writer::open(&dir, n, cfg(size), mode, 3).unwrap(),
        None => Writer::create(&dir, 1, cfg(size), mode, 3).unwrap(),
    };
    let mut side = OpenOptions::new().create(true).append(true).open(dir.join("synced")).unwrap();
    let mut rng = Rng::new(a.u64("seed", 1));
    loop {
        let objs = workload::mixed(8, rng.next());
        let mut line = String::new();
        for o in &objs {
            w.put(o).unwrap();
            line.push_str(&hex(&ObjectId::of(o)));
            line.push('\n');
        }
        w.sync().unwrap();
        side.write_all(line.as_bytes()).unwrap();
        sys::full_sync(&side).unwrap();
    }
}

fn run(a: &Args) {
    let kills = a.u64("kills", 40);
    let size = a.u64("size_mib", 8);
    let mode = a.str("mode", "mmap");
    let mut rng = Rng::new(a.u64("seed", 1) ^ 0xE5E5);
    let td = arena_spike::bench::run_dir();
    let dir = td.path().to_owned();
    let exe = std::env::current_exe().unwrap();
    let t0 = Instant::now();
    let (mut checked, mut bad, mut found_bad, mut early) = (0u64, 0u64, 0u64, 0u64);
    let (mut torn_rounds, mut torn_max, mut unsynced_found) = (0u64, 0u64, 0u64);
    for k in 0..kills {
        let mut c = Command::new(&exe)
            .args(["child", "--dir", dir.to_str().unwrap(), "--mode", &mode])
            .args(["--size_mib", &size.to_string(), "--seed", &(rng.next() >> 1).to_string()])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(30 + rng.below(700)));
        if let Ok(Some(st)) = c.try_wait() {
            let mut e = String::new();
            use std::io::Read;
            c.stderr.take().unwrap().read_to_string(&mut e).unwrap();
            eprintln!("kill {k}: child exited early ({st}): {e}");
            early += 1;
        } else {
            c.kill().unwrap(); // SIGKILL
            c.wait().unwrap();
        }
        let ids = synced(&dir);
        let nums = arenas(&dir);
        // 1. Read-only walk: end of the good records and the torn tail.
        let mut ro_ids = HashSet::new();
        for &n in &nums {
            let o = Arena::open(&dir, n, &cfg(size), arena::load_idx(&dir, n), Recovery::Append(Walk::Window), true, true)
                .unwrap()
                .unwrap();
            ro_ids.extend(o.entries.iter().map(|(id, _)| *id));
            if Some(&n) == nums.last() {
                let mut b = vec![0u8; (o.arena.len() - o.arena.hwm) as usize];
                o.arena.read_at(o.arena.hwm, &mut b).unwrap();
                let tail = b.iter().rposition(|&x| x != 0).map_or(0, |p| p as u64 + 1);
                if tail > 0 {
                    torn_rounds += 1;
                    torn_max = torn_max.max(tail);
                }
            }
        }
        let synced_set: HashSet<_> = ids.iter().copied().collect();
        unsynced_found += ro_ids.difference(&synced_set).count() as u64;
        // 2. Real open (cut) and read back.
        for &n in &nums {
            let w = Writer::open(&dir, n, cfg(size), Mode::PwBuf, 3).unwrap();
            for id in w.index.keys() {
                if w.get(id).is_none() {
                    found_bad += 1;
                }
            }
            for id in &ids {
                if w.index.contains_key(id) {
                    checked += 1;
                    if w.get(id).is_none() {
                        bad += 1;
                    }
                }
            }
        }
        let missing = ids.iter().filter(|id| !ro_ids.contains(id)).count() as u64;
        if missing > 0 {
            eprintln!("kill {k}: {missing} synced ids missing");
        }
        bad += missing;
    }
    let mut r = Rec::new("ecrash", &mode);
    r.s("cfg", &format!("kills={kills} size_mib={size}"))
        .n("kills", kills as f64)
        .n("child_early_exits", early as f64)
        .n("synced_final", synced(&dir).len() as f64)
        .n("synced_checked", checked as f64)
        .n("synced_bad_or_missing", bad as f64)
        .n("found_records_bad", found_bad as f64)
        .n("unsynced_records_found_sum", unsynced_found as f64)
        .n("rounds_with_torn_tail", torn_rounds as f64)
        .n("torn_tail_max_bytes", torn_max as f64)
        .n("arenas_final", arenas(&dir).len() as f64)
        .n("wall_s", t0.elapsed().as_secs_f64());
    r.emit();
    if bad > 0 || found_bad > 0 || early > 0 {
        std::process::exit(1);
    }
}

#[allow(dead_code)]
fn tail_zero(f: &std::fs::File, from: u64, len: u64) -> bool {
    let mut b = vec![0u8; len as usize];
    f.read_exact_at(&mut b, from).is_ok() && b.iter().all(|&x| x == 0)
}
