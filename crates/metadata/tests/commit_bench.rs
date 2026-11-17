//! Measures DictMetadataStore::commit() time against the number of files
//! (issue #18). Run: cargo test --release -p metadata --test commit_bench -- --ignored --nocapture
use std::sync::{Arc, Mutex};
use std::time::Instant;

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_traits::metadata::{InodeMeta, MetadataStore};

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
