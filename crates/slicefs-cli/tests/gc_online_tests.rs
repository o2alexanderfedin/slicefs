//! Online GC on a mounted store (#25): each test acts between the GC steps
//! (start, mark, sweep) and reads the whole file back afterwards. A write
//! that only succeeds proves nothing: the data must read back.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use metadata::segment::load_store_from_segments;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::gc::run_gc;
use slicefs_traits::metadata::MetadataStore;
use tempfile::TempDir;

fn make_fs(store: &Path) -> SliceFsFilesystem {
    std::fs::create_dir_all(store.join("segments")).unwrap();
    let wal = create_wal(WalConfig::PerOp, store, 1).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let mut meta = DictMetadataStore::new(io.clone());
    meta.set_wal(wal);
    SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()))
}

fn data(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

fn write_file(fs: &SliceFsFilesystem, name: &str, bytes: &[u8]) -> u64 {
    let (ino, fh) = fs.test_create(1, name, 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, bytes).unwrap();
    fs.test_release(ino, fh).unwrap();
    ino
}

/// A file that is deleted and committed: its objects are garbage.
fn make_garbage(fs: &SliceFsFilesystem, name: &str, bytes: &[u8]) {
    write_file(fs, name, bytes);
    fs.meta().commit().unwrap();
    fs.simulate_unlink(1, name).unwrap();
    fs.meta().commit().unwrap();
}

/// Unmount, then check from disk: `name` reads back as `want`, and an offline
/// GC finds every object of the store (its mark fails on a missing object).
fn check_after_remount(fs: SliceFsFilesystem, store: &Path, files: &[(&str, &[u8])]) {
    fs.meta().commit().unwrap();
    fs.meta().shutdown_wal().unwrap();
    drop(fs);
    run_gc(store).expect("offline gc: every object must be reachable");
    let (root, _) = load_store_from_segments(&store.join("segments")).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let meta = DictMetadataStore::load_from_root(io, &root.unwrap()).unwrap();
    for (name, want) in files {
        let ino = meta.lookup(1, name).unwrap();
        let manifest = meta.get_manifest(ino).unwrap();
        let cs = slicefs_cli::content::open(store).unwrap();
        let got = slicefs_cli::content::get_all(&cs, &manifest[0]).unwrap();
        assert!(got == *want, "{name}: {} bytes back, want {}", got.len(), want.len());
    }
}

fn no_stop() -> AtomicBool {
    AtomicBool::new(false)
}

#[test]
fn online_gc_frees_garbage_and_keeps_live_files() {
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    let keep = data(1, 300_000);
    write_file(&fs, "keep.bin", &keep);
    make_garbage(&fs, "del.bin", &data(2, 300_000));

    let stats = fs.gc_online(&no_stop()).unwrap().expect("not stopped");
    assert!(stats.removed_objects > 30, "{stats:?}");
    let ino = fs.test_lookup(1, "keep.bin").unwrap().0;
    assert_eq!(fs.test_read(ino, 0, keep.len() as u32 + 1).unwrap(), keep);
    check_after_remount(fs, dir.path(), &[("keep.bin", &keep)]);
}

#[test]
fn a_write_that_dedups_against_garbage_during_gc_keeps_its_objects() {
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    let same = data(3, 300_000);
    make_garbage(&fs, "old.bin", &same);

    let (gc, boundary) = fs.gc_start().unwrap();
    let live = gc.live(&no_stop()).unwrap().unwrap();
    // The same bytes again: the chunk store finds the existing (garbage)
    // objects and does not write them; only the touch protects them.
    write_file(&fs, "new.bin", &same);
    let content = fs.content().gc_finish(&live, boundary).unwrap();
    drop(gc);
    assert!(content.kept_objects > 0, "{content:?}");

    let ino = fs.test_lookup(1, "new.bin").unwrap().0;
    assert_eq!(fs.test_read(ino, 0, same.len() as u32 + 1).unwrap(), same);
    check_after_remount(fs, dir.path(), &[("new.bin", &same)]);
}

#[test]
fn an_open_streaming_handle_keeps_its_partial_tree() {
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    make_garbage(&fs, "junk.bin", &data(4, 300_000));
    let content = data(5, 1_000_000);
    let (ino, fh) = fs.test_create(1, "big.bin", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, &content[..300_000]).unwrap();
    fs.test_write(fh, 300_000, &content[300_000..600_000]).unwrap();
    assert!(fs.test_is_streaming(fh));

    let stats = fs.gc_online(&no_stop()).unwrap().unwrap();
    assert!(stats.removed_objects > 30, "garbage must go: {stats:?}");

    fs.test_write(fh, 600_000, &content[600_000..]).unwrap();
    fs.test_release(ino, fh).unwrap();
    assert_eq!(fs.test_read(ino, 0, content.len() as u32 + 1).unwrap(), content);
    check_after_remount(fs, dir.path(), &[("big.bin", &content)]);
}

#[test]
fn uncommitted_files_survive_online_gc() {
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    make_garbage(&fs, "junk.bin", &data(6, 300_000));
    // Released but not committed: only the GC start commit records it.
    let fresh = data(7, 300_000);
    write_file(&fs, "fresh.bin", &fresh);

    fs.gc_online(&no_stop()).unwrap().unwrap();
    check_after_remount(fs, dir.path(), &[("fresh.bin", &fresh)]);
}

#[test]
fn a_stop_request_ends_the_gc_without_more_deletes() {
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    let keep = data(8, 300_000);
    write_file(&fs, "keep.bin", &keep);
    make_garbage(&fs, "junk.bin", &data(9, 300_000));

    let stop = AtomicBool::new(true);
    assert!(fs.gc_online(&stop).unwrap().is_none());
    // Tracking is off again: a later full run works and frees the garbage.
    assert!(fs.gc_online(&no_stop()).unwrap().unwrap().removed_objects > 30);
    check_after_remount(fs, dir.path(), &[("keep.bin", &keep)]);
}

/// The metadata nodes are in the chunk store too: the nodes of deleted
/// files (inodes, old directory lists, old maps) are freed by the GC.
#[test]
fn the_nodes_of_deleted_files_are_freed() {
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    let keep = data(30, 1000);
    write_file(&fs, "keep", &keep);
    fs.gc_online(&no_stop()).unwrap().unwrap();
    let before = fs.content().stats().objects;
    for i in 0..50 {
        write_file(&fs, &format!("tmp{i}"), format!("small file {i}").as_bytes());
    }
    fs.gc_online(&no_stop()).unwrap().unwrap();
    let with_files = fs.content().stats().objects;
    for i in 0..50 {
        fs.simulate_unlink(1, &format!("tmp{i}")).unwrap();
    }
    let stats = fs.gc_online(&no_stop()).unwrap().unwrap();
    // 50 files: each has a content (chunk + manifest) and an inode node
    // (chunk + manifest), so at least 4 objects each.
    assert!(with_files >= before + 200, "{before} -> {with_files}");
    assert!(stats.removed_objects >= 200, "{stats:?}");
    assert!(fs.content().stats().objects <= before + 8, "{before} -> {}", fs.content().stats().objects);
    check_after_remount(fs, dir.path(), &[("keep", &keep)]);
}

// ── Online GC and the overlay rebase (phase 14, D-06, D-19) ─────────────────

/// Stream `n` MiB into a new file (no fsync, no close), then write 4 KiB
/// back in the middle: the switch installs the ended stream as the base.
fn open_overlay(fs: &SliceFsFilesystem, name: &str, n: usize, seed: u64) -> (u64, u64, Vec<u8>) {
    let (ino, fh) = fs.test_create(1, name, 0o644, 0, 0, 0).unwrap();
    let mut model = Vec::with_capacity(n << 20);
    for i in 0..n {
        let piece = data(seed + i as u64, 1 << 20);
        fs.test_write(fh, (i as u64) << 20, &piece).unwrap();
        model.extend_from_slice(&piece);
    }
    let at = (n << 19) + 1234;
    let patch = data(seed + 1000, 4096);
    fs.test_write(fh, at as u64, &patch).unwrap();
    model[at..at + 4096].copy_from_slice(&patch);
    assert!(!fs.test_is_streaming(fh), "the write back switched to an overlay");
    (ino, fh, model)
}

/// Mark and sweep run inside the rebase of the release: after the splice
/// finished the new root and before `install_root`. The base exists only
/// because the switch installed the ended stream (it was never committed,
/// fsynced or closed). The splice reuses most of its chunks by ID, without
/// a read, so only the marks protect them: the base is in the manifest of
/// the `gc_start` commit and in its file roots. Without the base install at
/// the switch and the overlay file root in `gc_start`, the sweep deletes
/// those chunks and the read after remount (or the offline GC mark) fails.
#[test]
fn online_gc_during_a_rebase_keeps_every_chunk() {
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    let (ino, fh, model) = open_overlay(&fs, "big.bin", 32, 100);
    make_garbage(&fs, "junk.bin", &data(99, 300_000));

    let (gc, boundary) = fs.gc_start().unwrap();
    let pending = Arc::new(Mutex::new(Some((gc, boundary))));
    let swept: Arc<Mutex<Option<chunk_store::GcStats>>> = Arc::new(Mutex::new(None));
    let (cs, slot, out) = (Arc::clone(fs.content()), Arc::clone(&pending), Arc::clone(&swept));
    fs.test_set_rebase_hook(Some(Arc::new(move || {
        if let Some((gc, boundary)) = slot.lock().unwrap().take() {
            let ids = gc.live(&no_stop()).unwrap().expect("not stopped");
            *out.lock().unwrap() = Some(cs.gc_finish(&ids, boundary).unwrap());
        }
    })));
    let before = fs.test_rebase_count();
    {
        // As the release callback does: the rebase runs under the shared gate.
        let gate = fs.commit_gate();
        let _shared = gate.read().unwrap();
        fs.test_release(ino, fh).unwrap();
    }
    fs.test_set_rebase_hook(None);
    assert_eq!(fs.test_rebase_count(), before + 1, "one rebase, at release");
    let stats = swept.lock().unwrap().take().expect("the GC ran inside the rebase");
    // Every byte of the (incompressible) garbage file is freed. The object
    // count also holds metadata nodes and differs from run to run.
    assert!(stats.removed_bytes >= 300_000, "the garbage must go: {stats:?}");
    assert!(fs.test_read(ino, 0, (33 << 20) as u32).unwrap() == model, "content before remount");
    check_after_remount(fs, dir.path(), &[("big.bin", &model)]);
}

/// `gc_start` takes the commit gate exclusively before `gc_begin`, so it
/// waits while a callback (holding the gate shared) is inside a rebase.
/// Without the gate, `gc_begin` would run during the rebase: the chunks the
/// splice put before `gc_begin` are in no root and not touched, so the
/// sweep would delete them.
#[test]
fn gc_start_waits_for_a_rebase_in_a_callback() {
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    let (ino, fh, model) = open_overlay(&fs, "f.bin", 4, 500);
    make_garbage(&fs, "junk.bin", &data(499, 300_000));

    let (inside_tx, inside_rx) = channel::<()>();
    let (go_tx, go_rx) = channel::<()>();
    let (inside_tx, go_rx) = (Mutex::new(inside_tx), Mutex::new(go_rx));
    fs.test_set_rebase_hook(Some(Arc::new(move || {
        inside_tx.lock().unwrap().send(()).unwrap();
        go_rx.lock().unwrap().recv_timeout(Duration::from_secs(30)).expect("go");
    })));
    let returned = AtomicBool::new(false);
    let (waited, started) = std::thread::scope(|sc| {
        let callback = sc.spawn(|| {
            let gate = fs.commit_gate();
            let _shared = gate.read().unwrap();
            fs.test_release(ino, fh).unwrap();
        });
        inside_rx.recv_timeout(Duration::from_secs(30)).expect("the release reached the rebase");
        let gc = sc.spawn(|| {
            let r = fs.gc_start();
            returned.store(true, Ordering::SeqCst);
            r
        });
        std::thread::sleep(Duration::from_millis(300));
        let waited = !returned.load(Ordering::SeqCst);
        go_tx.send(()).unwrap();
        callback.join().unwrap();
        (waited, gc.join().unwrap())
    });
    fs.test_set_rebase_hook(None);
    assert!(waited, "gc_start returned while a callback was inside a rebase");
    let (gc, boundary) = started.unwrap();
    let stats = fs.gc_content(&gc, boundary, &no_stop()).unwrap().expect("not stopped");
    assert!(stats.removed_bytes >= 300_000, "the garbage must go: {stats:?}");
    check_after_remount(fs, dir.path(), &[("f.bin", &model)]);
}

/// A full online GC while an overlay handle is open (switch done, not
/// rebased): the base is only in the manifest the switch set and in the
/// overlay file root of `gc_start`. Without them the sweep deletes the
/// chunks of the ended stream, and the rebase at release reuses them by ID.
#[test]
fn an_open_overlay_keeps_its_base_through_online_gc() {
    let dir = TempDir::new().unwrap();
    let fs = make_fs(dir.path());
    make_garbage(&fs, "junk.bin", &data(799, 300_000));
    let (ino, fh, mut model) = open_overlay(&fs, "open.bin", 4, 800);

    let stats = fs.gc_online(&no_stop()).unwrap().expect("not stopped");
    assert!(stats.removed_bytes >= 300_000, "the garbage must go: {stats:?}");

    let more = data(900, 4096);
    fs.test_write(fh, 3 << 20, &more).unwrap();
    model[3 << 20..(3 << 20) + 4096].copy_from_slice(&more);
    let tail = data(901, 4096);
    fs.test_write(fh, model.len() as u64, &tail).unwrap();
    model.extend_from_slice(&tail);
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_read(ino, 0, (5 << 20) as u32).unwrap() == model, "content before remount");
    check_after_remount(fs, dir.path(), &[("open.bin", &model)]);
}
