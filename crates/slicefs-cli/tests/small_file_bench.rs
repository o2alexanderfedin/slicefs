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
