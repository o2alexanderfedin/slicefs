//! SPIKE (wal-memory): cost of the write log options. Not a real test.
//! The option comes from the env var SLICEFS_WAL_SPIKE (unset, `a`, `b`).
//! Run one scenario per process, so peak RSS is per scenario:
//!   SLICEFS_WAL_SPIKE=a SPIKE_CASE=big cargo test --release -p slicefs-cli \
//!     --test wal_spike_bench -- --ignored --nocapture
//! SPIKE_CASE: big (1 GiB as 128 KiB writes), small (100k 4 KiB writes),
//! files (20k files of 4 KiB), tiny (200k 64-byte writes), fsync (1000 files,
//! one fsync each), mem / memp (flush-on-fsync / periodic, 1 GiB, no fsync).

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use metadata::segment::writer::{SEGMENT_SYNC_CALLS, SEGMENT_WRITE_CALLS};
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, spike_mode, WalConfig, SPIKE_BUF_PEAK};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::handlers::{self as h, CreateResult, EmptyResult, WriteResult};
use slicefs_traits::metadata::MetadataStore;

const S_IFREG: u32 = 0o100_000;

fn new_fs(store: &Path, config: WalConfig) -> SliceFsFilesystem {
    std::fs::create_dir_all(store.join("segments")).unwrap();
    let wal = create_wal(config, store, 1).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let mut meta = DictMetadataStore::new(io.clone());
    meta.set_wal(wal);
    let fs = SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()));
    fs.meta().commit().unwrap();
    fs
}

fn create(fs: &SliceFsFilesystem, name: &str) -> (u64, u64) {
    match h::handle_create(fs, 1, name, S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR) {
        CreateResult::Ok(ino, fh, _) => (ino, fh),
        _ => panic!("create {name}"),
    }
}

fn write(fs: &SliceFsFilesystem, fh: u64, off: u64, bytes: &[u8]) {
    assert!(matches!(h::handle_write(fs, fh, off, bytes), WriteResult::Ok(_)));
}

fn release(fs: &SliceFsFilesystem, ino: u64, fh: u64) {
    assert!(matches!(h::handle_release(fs, ino, fh), EmptyResult::Ok));
}

/// A block whose 4 KiB pieces are all unique (no dedup in the chunk store).
fn fill(buf: &mut [u8], seed: u64) {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for w in buf.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let b = x.to_le_bytes();
        w.copy_from_slice(&b[..w.len()]);
    }
}

fn rusage() -> (f64, u64) {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let t = |v: libc::timeval| v.tv_sec as f64 + v.tv_usec as f64 / 1e6;
    // ru_maxrss is in bytes on macOS, KiB on Linux.
    let rss = if cfg!(target_os = "macos") { u.ru_maxrss as u64 } else { u.ru_maxrss as u64 * 1024 };
    (t(u.ru_utime) + t(u.ru_stime), rss)
}

fn load() -> String {
    let mut l = [0f64; 3];
    unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    format!("{:.1}", l[0])
}

#[test]
#[ignore]
fn wal_spike() {
    let case = std::env::var("SPIKE_CASE").unwrap_or_else(|_| "big".into());
    let dir = tempfile::tempdir().unwrap();
    let config = match case.as_str() {
        "mem" => WalConfig::FlushOnFsync,
        "memp" => WalConfig::Periodic { interval_secs: 5 },
        _ => WalConfig::PerOp,
    };
    let fs = new_fs(dir.path(), config);
    let load0 = load();
    let (cpu0, _) = rusage();
    let w0 = SEGMENT_WRITE_CALLS.load(Ordering::Relaxed);
    let s0 = SEGMENT_SYNC_CALLS.load(Ordering::Relaxed);
    let t = Instant::now();
    let (ops, bytes): (u64, u64) = match case.as_str() {
        "big" | "mem" | "memp" => {
            let (ino, fh) = create(&fs, "big");
            let mut buf = vec![0u8; 128 << 10];
            let n = (1u64 << 30) / (128 << 10);
            for i in 0..n {
                fill(&mut buf, i + 1);
                write(&fs, fh, i * (128 << 10), &buf);
            }
            release(&fs, ino, fh);
            (n, 1 << 30)
        }
        "small" => {
            let (ino, fh) = create(&fs, "small");
            let mut buf = vec![0u8; 4096];
            let n = 100_000u64;
            for i in 0..n {
                fill(&mut buf, i + 1);
                write(&fs, fh, i * 4096, &buf);
            }
            release(&fs, ino, fh);
            (n, n * 4096)
        }
        "files" => {
            let mut buf = vec![0u8; 4096];
            let n = 20_000u64;
            for i in 0..n {
                fill(&mut buf, i + 1);
                let (ino, fh) = create(&fs, &format!("f{i}"));
                write(&fs, fh, 0, &buf);
                release(&fs, ino, fh);
            }
            (n, n * 4096)
        }
        "tiny" => {
            // Worst case for option A: records far below the 8 KiB buffer.
            let (ino, fh) = create(&fs, "tiny");
            let mut buf = vec![0u8; 64];
            let n = 200_000u64;
            for i in 0..n {
                fill(&mut buf, i + 1);
                write(&fs, fh, i * 64, &buf);
            }
            release(&fs, ino, fh);
            (n, n * 64)
        }
        "fsync" => {
            // fsync count check: one fsync per file.
            let mut buf = vec![0u8; 4096];
            let n = 1_000u64;
            for i in 0..n {
                fill(&mut buf, i + 1);
                let (ino, fh) = create(&fs, &format!("f{i}"));
                write(&fs, fh, 0, &buf);
                assert!(matches!(h::handle_fsync(&fs, ino, fh), EmptyResult::Ok));
                release(&fs, ino, fh);
            }
            (n, n * 4096)
        }
        other => panic!("unknown SPIKE_CASE {other}"),
    };
    let secs = t.elapsed().as_secs_f64();
    let (cpu1, rss) = rusage();
    let w = SEGMENT_WRITE_CALLS.load(Ordering::Relaxed) - w0;
    let s = SEGMENT_SYNC_CALLS.load(Ordering::Relaxed) - s0;
    println!(
        "SPIKE mode={:?} case={case} secs={secs:.3} cpu={:.3} ops_per_s={:.0} mb_per_s={:.1} seg_writes={w} seg_syncs={s} peak_rss_mib={:.1} wal_buf_peak_mib={:.2} load_start={load0} load_end={}",
        spike_mode(),
        cpu1 - cpu0,
        ops as f64 / secs,
        bytes as f64 / secs / 1e6,
        rss as f64 / (1 << 20) as f64,
        SPIKE_BUF_PEAK.load(Ordering::Relaxed) as f64 / (1 << 20) as f64,
        load(),
    );
    std::mem::forget(fs); // no commit, no shutdown: same as the crash case
}
