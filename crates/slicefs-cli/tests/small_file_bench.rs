//! Times create + write + flush + release of small files through the filesystem API.
//! Run: cargo test --release -p slicefs-cli --test small_file_bench -- --ignored --nocapture
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_cli::filesystem::SliceFsFilesystem;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[test]
#[ignore]
fn small_file_cost() {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let fs = SliceFsFilesystem::new(DictMetadataStore::new(io.clone()), io, None);
    let n = 2000;
    let (mut tc, mut tw, mut tf, mut tr) = (0.0, 0.0, 0.0, 0.0);
    for i in 0..n {
        let data = format!("file number {i} with some text\n").repeat(100);
        let t = Instant::now();
        let (ino, fh) = fs.test_create(1, &format!("f{i}"), 0o100_644, 0o022, 0, 0).unwrap();
        tc += t.elapsed().as_secs_f64();
        let t = Instant::now();
        fs.test_write(fh, 0, data.as_bytes()).unwrap();
        tw += t.elapsed().as_secs_f64();
        let t = Instant::now();
        fs.test_flush(ino, fh).unwrap();
        tf += t.elapsed().as_secs_f64();
        let t = Instant::now();
        fs.test_release(ino, fh).unwrap();
        tr += t.elapsed().as_secs_f64();
    }
    let ms = |x: f64| x / n as f64 * 1e3;
    println!("per file: create {:.3} ms, write {:.3} ms, flush {:.3} ms, release {:.3} ms", ms(tc), ms(tw), ms(tf), ms(tr));
}

/// pjdfstest open/25: one byte at 2 GiB + 1. Run with --ignored --nocapture.
#[test]
#[ignore]
fn sparse_2gib_write_cost() {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let fs = SliceFsFilesystem::new(DictMetadataStore::new(io.clone()), io, None);
    let (ino, fh) = fs.test_create(1, "big", 0o100_644, 0o022, 0, 0).unwrap();
    let t = Instant::now();
    fs.test_write(fh, (2u64 << 30) + 1, b"x").unwrap();
    fs.test_fsync(ino, fh).unwrap();
    let w = t.elapsed();
    fs.test_release(ino, fh).unwrap();
    println!("2 GiB gap: write+fsync {:.1}s, total {:.1}s", w.as_secs_f64(), t.elapsed().as_secs_f64());
}

/// Memory of a small read in the middle of a large file. Run with --ignored --nocapture.
#[test]
#[ignore]
fn mid_file_read_cost() {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let fs = SliceFsFilesystem::new(DictMetadataStore::new(io.clone()), io, None);
    let (ino, fh) = fs.test_create(1, "big", 0o100_644, 0o022, 0, 0).unwrap();
    let chunk: Vec<u8> = (0..1u32 << 20).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    for i in 0..64u64 {
        fs.test_write(fh, i << 20, &chunk).unwrap();
    }
    fs.test_release(ino, fh).unwrap();
    let rss = || { let o = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap(); String::from_utf8_lossy(&o.stdout).trim().to_string() };
    println!("rss before read: {} KB", rss());
    let t = Instant::now();
    let got = fs.test_read(ino, 40 << 20, 8).unwrap();
    println!("read 8 B at 40 MiB: {:.2}s, rss {} KB", t.elapsed().as_secs_f64(), rss());
    assert_eq!(got, chunk[..8].to_vec());
}

/// Memory around a 200 MiB zero gap. Run with --ignored --nocapture.
#[test]
#[ignore]
fn zero_gap_memory() {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let fs = SliceFsFilesystem::new(DictMetadataStore::new(io.clone()), io, None);
    let rss = || { let o = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap(); String::from_utf8_lossy(&o.stdout).trim().to_string() };
    let (ino, fh) = fs.test_create(1, "z", 0o100_644, 0o022, 0, 0).unwrap();
    fs.test_write(fh, 200 << 20, b"X").unwrap();
    println!("after gap write: rss {} KB", rss());
    fs.test_fsync(ino, fh).unwrap();
    println!("after fsync: rss {} KB", rss());
    fs.test_release(ino, fh).unwrap();
    println!("after release: rss {} KB", rss());
    let _ = fs.test_read(ino, 100 << 20, 8).unwrap();
    println!("after mid read: rss {} KB", rss());
    let _ = fs.test_read(ino, 200 << 20, 1).unwrap();
    println!("after end read: rss {} KB", rss());
}
