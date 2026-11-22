//! Readers and writers of a file do not wait for the I/O of a rebase step
//! (Phase 19, D-02, ROADMAP criterion 1): the step copies its extents under
//! the cell lock, chunks, hashes and puts with no cell lock held, and
//! publishes only if the overlay did not change meanwhile.
//!
//! The parked tests hold no commit gate and run no commit thread while a
//! step is parked (RESEARCH Pitfall 8).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_cli::filesystem::SliceFsFilesystem;
use tempfile::TempDir;

const KIB: usize = 1 << 10;
const WAIT: Duration = Duration::from_secs(5);

fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let meta = DictMetadataStore::new(io.clone());
    let fs = SliceFsFilesystem::new(meta, io, None);
    (fs, dir)
}

fn seeded(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn put(model: &mut Vec<u8>, off: usize, bytes: &[u8]) {
    if model.len() < off + bytes.len() {
        model.resize(off + bytes.len(), 0);
    }
    model[off..off + bytes.len()].copy_from_slice(bytes);
}

/// A 1 MiB file, closed, then open read-write in overlay mode with a dirty
/// limit of 64 KiB. Returns the inode, the handle and the model.
fn overlay_file(fs: &SliceFsFilesystem) -> (u64, u64, Vec<u8>) {
    let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
    let mut model = seeded(1, 1 << 20);
    fs.test_write(fh, 0, &model).unwrap();
    fs.test_release(ino, fh).unwrap();
    fs.test_set_dirty_limit(64 * KIB);
    let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let a = seeded(2, 4 * KIB);
    fs.test_write(fh, 512 * KIB as u64, &a).unwrap();
    put(&mut model, 512 * KIB, &a);
    (ino, fh, model)
}

/// A rebase hook that parks its first call until `go` (at most 10 s) and
/// tells `parked` when it gets there. Later calls do not park.
fn park_first(fs: &SliceFsFilesystem) -> (Receiver<()>, Sender<()>) {
    let (parked_tx, parked_rx) = channel();
    let (go_tx, go_rx) = channel::<()>();
    let (first, go_rx) = (AtomicBool::new(true), Mutex::new(go_rx));
    fs.test_set_rebase_hook(Some(Arc::new(move || {
        if first.swap(false, Ordering::SeqCst) {
            parked_tx.send(()).ok();
            let _ = go_rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
        }
    })));
    (parked_rx, go_tx)
}

#[test]
fn reader_does_not_wait_for_rebase_io() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh, mut model) = overlay_file(&fs);
    let (parked, go) = park_first(&fs);
    let big = seeded(3, 128 * KIB);
    put(&mut model, 256 * KIB, &big);
    std::thread::scope(|s| {
        let w = s.spawn(|| fs.test_write(fh, 256 * KIB as u64, &big));
        parked.recv_timeout(WAIT).expect("the rebase reached its I/O phase");
        let (tx, rx) = channel();
        let fs = &fs;
        s.spawn(move || tx.send(fs.test_read(ino, 256 * KIB as u64, 4096)).ok());
        let got = match rx.recv_timeout(WAIT) {
            Ok(r) => r,
            Err(e) => {
                go.send(()).ok();
                panic!("the read waited for the parked rebase: {e:?}");
            }
        };
        assert!(got.unwrap() == model[256 * KIB..260 * KIB], "the read sees the whole write");
        go.send(()).unwrap();
        assert_eq!(w.join().unwrap(), Ok(big.len() as u32));
    });
    fs.test_set_rebase_hook(None);
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content with the handle open");
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "content after release");
}

#[test]
fn writer_does_not_wait_for_rebase_io() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh, mut model) = overlay_file(&fs);
    let (other, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let (parked, go) = park_first(&fs);
    let big = seeded(4, 128 * KIB);
    let patch = seeded(5, 4 * KIB);
    put(&mut model, 256 * KIB, &big);
    put(&mut model, 768 * KIB, &patch);
    std::thread::scope(|s| {
        let w = s.spawn(|| fs.test_write(fh, 256 * KIB as u64, &big));
        parked.recv_timeout(WAIT).expect("the rebase reached its I/O phase");
        let (tx, rx) = channel();
        let (fs, patch) = (&fs, &patch);
        s.spawn(move || tx.send(fs.test_write(other, 768 * KIB as u64, patch)).ok());
        let got = match rx.recv_timeout(WAIT) {
            Ok(r) => r,
            Err(e) => {
                go.send(()).ok();
                panic!("the write waited for the parked rebase: {e:?}");
            }
        };
        assert_eq!(got, Ok(patch.len() as u32));
        go.send(()).unwrap();
        assert_eq!(w.join().unwrap(), Ok(big.len() as u32));
    });
    fs.test_set_rebase_hook(None);
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "both writes with the handles open");
    fs.test_release(ino, other).unwrap();
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_read(ino, 0, 2 << 20).unwrap() == model, "both writes after release");
}
