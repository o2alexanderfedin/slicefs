//! Measures DictMetadataStore::commit() time against the number of files
//! (issue #18). Run: cargo test --release -p metadata --test commit_bench -- --ignored --nocapture
//!
//! `one_change_commit_at_scale` is the PMAP-07 commit benchmark: N files, then
//! K one-change commits, each timed in wall time and thread CPU time. It uses
//! only API that exists in this tree and in tag `v2.3.0`, so the same file
//! builds in both; the nodes of a commit come from the per-tree report module
//! `support/pmap07_report.rs`. Run by `scripts/prolly-bench/pmap07.sh`.
use std::sync::{Arc, Mutex};
use std::time::Instant;

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_traits::metadata::{InodeMeta, MetadataStore};

#[path = "support/pmap07_report.rs"]
mod report;

/// CPU time (user plus system) of the calling thread, in milliseconds.
fn thread_cpu_ms() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid, writable timespec for the whole call; the
    // clock id is a constant that every supported platform defines.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    assert_eq!(rc, 0, "clock_gettime(CLOCK_THREAD_CPUTIME_ID) failed");
    ts.tv_sec as f64 * 1e3 + ts.tv_nsec as f64 / 1e6
}

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().map(|v| v.parse().unwrap_or_else(|_| panic!("{name} is not a number: {v}"))).unwrap_or(default)
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

fn na<T: std::fmt::Display>(v: Option<T>, prec: usize) -> String {
    v.map_or_else(|| "NA".to_string(), |x| format!("{x:.prec$}"))
}

#[test]
#[ignore]
fn commit_time_vs_file_count() {
    for &n in &[1_000u64, 10_000, 78_000] {
        let dir = tempfile::tempdir().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
        let meta = DictMetadataStore::new(io);
        let dirs: Vec<u64> = (0..(n / 100).max(1))
            .map(|i| meta.create_directory(1, &format!("d{i}"), &InodeMeta::new_directory(0, 0, 0, 0o040_755)).unwrap())
            .collect();
        for i in 0..n {
            let mut m = InodeMeta::new_file(0, 0, 0, 0o100_644);
            m.size = 1000 + i;
            let ino = meta.create_inode(&m).unwrap();
            meta.link(dirs[(i % dirs.len() as u64) as usize], &format!("f{i}"), ino).unwrap();
            let d = slicefs_traits::key::ObjectKey::example((i as u32).wrapping_mul(2654435761));
            meta.set_manifest(ino, &[d]).unwrap();
            meta.increment_refcount(&d);
        }
        let t = Instant::now();
        meta.commit().unwrap();
        let first = t.elapsed();
        // One small change, then commit again (what a commit-on-fsync would pay).
        let mut m = InodeMeta::new_file(0, 0, 0, 0o100_644);
        m.size = 1;
        let ino = meta.create_inode(&m).unwrap();
        meta.link(1, "one-more", ino).unwrap();
        let t = Instant::now();
        meta.commit().unwrap();
        println!("files={n:>6}  first commit {:>8.1} ms   commit after 1 change {:>8.1} ms", first.as_secs_f64() * 1e3, t.elapsed().as_secs_f64() * 1e3);
    }
}

/// PMAP-07: N files, one big first commit, then `PMAP07_SAMPLES` commits of one
/// change each. Env: `PMAP07_N` (default 1,000,000), `PMAP07_SAMPLES` (11),
/// `PMAP07_LABEL` (`this`). Lines: `seed ...`, `sample ...`, `summary ...`.
#[test]
#[ignore]
fn one_change_commit_at_scale() {
    let n = env_or("PMAP07_N", 1_000_000);
    let samples = env_or("PMAP07_SAMPLES", 11);
    let label = std::env::var("PMAP07_LABEL").unwrap_or_else(|_| "this".to_string());
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let cs = io.lock().unwrap().store().clone();
    let meta = DictMetadataStore::new(io);

    let t = Instant::now();
    let dirs: Vec<u64> = (0..(n / 100).max(1))
        .map(|i| meta.create_directory(1, &format!("d{i}"), &InodeMeta::new_directory(0, 0, 0, 0o040_755)).unwrap())
        .collect();
    for i in 0..n {
        let mut m = InodeMeta::new_file(0, 0, 0, 0o100_644);
        m.size = 1000 + i;
        let ino = meta.create_inode(&m).unwrap();
        meta.link(dirs[(i % dirs.len() as u64) as usize], &format!("f{i}"), ino).unwrap();
        let d = slicefs_traits::key::ObjectKey::example((i as u32).wrapping_mul(2654435761));
        meta.set_manifest(ino, &[d]).unwrap();
        meta.increment_refcount(&d);
    }
    let seed_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    meta.commit().unwrap();
    let first_ms = t.elapsed().as_secs_f64() * 1e3;
    println!("seed tree={label} n={n} dirs={} seed_ms={seed_ms:.3} first_commit_ms={first_ms:.3}", dirs.len());

    let (mut walls, mut cpus) = (Vec::new(), Vec::new());
    for s in 0..samples {
        let mut m = InodeMeta::new_file(0, 0, 0, 0o100_644);
        m.size = 1;
        let ino = meta.create_inode(&m).unwrap();
        meta.link(1, &format!("one-more-{s}"), ino).unwrap();
        let cpu0 = thread_cpu_ms();
        let t = Instant::now();
        let root = meta.commit().unwrap();
        let wall = t.elapsed().as_secs_f64() * 1e3;
        let cpu = thread_cpu_ms() - cpu0;
        let (read, written, presync) = report::nodes(&meta, &cs, &root);
        println!(
            "sample tree={label} n={n} seq={s} wall_ms={wall:.3} cpu_ms={cpu:.3} nodes_read={} nodes_written={} presync_ms={}",
            na(read, 0), na(written, 0), na(presync, 3)
        );
        walls.push(wall);
        cpus.push(cpu);
    }
    println!(
        "summary tree={label} n={n} samples={samples} wall_ms_median={:.3} cpu_ms_median={:.3}",
        median(&mut walls), median(&mut cpus)
    );
}
