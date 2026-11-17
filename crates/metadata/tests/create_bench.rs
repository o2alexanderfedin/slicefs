//! Measures per-create cost of DictMetadataStore against directory size and total files.
//! Run: cargo test --release -p metadata --test create_bench -- --ignored --nocapture
use std::sync::{Arc, Mutex};
use std::time::Instant;

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_traits::metadata::{InodeMeta, MetadataStore};

fn run(label: &str, n: u64, files_per_dir: u64) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let meta = DictMetadataStore::new(io);
    let mut cur = 1;
    let t0 = Instant::now();
    let mut t = Instant::now();
    for i in 0..n {
        if i % files_per_dir == 0 {
            cur = meta.create_directory(1, &format!("d{i}"), &InodeMeta::new_directory(0, 0, 0, 0o040_755)).unwrap();
        }
        let name = format!("file-{i}.hpp");
        let _ = meta.lookup(cur, &name);
        let ino = meta.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100_644)).unwrap();
        meta.link(cur, &name, ino).unwrap();
        let mut m = meta.get_inode(ino).unwrap();
        m.mode = 0o100_600;
        meta.update_inode(&m).unwrap();
        if (i + 1) % (n / 4) == 0 {
            println!("{label}: files {:>6}  last quarter {:>7.0} files/s", i + 1, (n / 4) as f64 / t.elapsed().as_secs_f64());
            t = Instant::now();
        }
    }
    let t = Instant::now();
    meta.commit().unwrap();
    println!("{label}: total {:.1}s (commit {:.2}s)", t0.elapsed().as_secs_f64(), t.elapsed().as_secs_f64());
}

#[test]
#[ignore]
fn create_cost() {
    run("100/dir ", 40_000, 100);
    run("2000/dir", 8_000, 2_000);
}
