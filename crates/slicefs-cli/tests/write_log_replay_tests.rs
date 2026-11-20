//! Write log replay (#20): requests made through the handlers after the last
//! commit, then a crash (no commit, no unmount). Loading the root and
//! replaying the log must give the same file system as before the crash:
//! names, types, modes, owners, sizes, times, link counts, xattrs, content.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use metadata::segment::load_store_from_segments;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::handlers::{self as h, CreateResult, EmptyResult, EntryResult, OpenResult, WriteResult};
use slicefs_cli::oplog;
use slicefs_traits::metadata::{InodeMeta, MetadataStore};
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

fn new_fs(store: &Path) -> SliceFsFilesystem {
    new_fs_with(store, WalConfig::PerOp)
}

fn new_fs_with(store: &Path, config: WalConfig) -> SliceFsFilesystem {
    std::fs::create_dir_all(store.join("segments")).unwrap();
    let wal = create_wal(config, store, 1).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let mut meta = DictMetadataStore::new(io.clone());
    meta.set_wal(wal);
    let fs = SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()));
    fs.meta().commit().unwrap();
    fs
}

/// What a mount after a crash does: the last root, then replay the log.
fn reload_and_replay(store: &Path) -> SliceFsFilesystem {
    reload_and_replay_with(store, |_| {})
}

/// `reload_and_replay` with `setup` run on the new file system before replay.
fn reload_and_replay_with(store: &Path, setup: impl FnOnce(&SliceFsFilesystem)) -> SliceFsFilesystem {
    let (root, snaps) = load_store_from_segments(&store.join("segments")).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let mut meta = DictMetadataStore::load_from_root(io.clone(), &root.unwrap()).unwrap();
    meta.set_snapshots(snaps);
    // A new segment after the last one, as a mount makes.
    let next = segment_paths(store).last()
        .and_then(|p| p.file_name()?.to_str()?.strip_prefix("segment-")?.strip_suffix(".seg")?.parse::<u64>().ok())
        .map_or(1, |n| n + 1);
    let wal = create_wal(WalConfig::PerOp, store, next).unwrap();
    meta.set_wal(wal);
    let fs = SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()));
    setup(&fs);
    // The mount's replay: it streams the log (#106).
    oplog::replay_log(&fs, &store.join("segments")).unwrap();
    fs
}

/// Path -> description of every entry under the root.
fn dump(fs: &SliceFsFilesystem) -> BTreeMap<String, String> {
    dump_by(fs, |ino| fs.meta().get_inode(ino).unwrap())
}

/// `dump` as stat sees it: a file with an open write handle shows the size
/// and times of that handle (its overlay is not in the inode until release).
fn dump_open(fs: &SliceFsFilesystem) -> BTreeMap<String, String> {
    dump_by(fs, |ino| fs.test_getattr(ino).unwrap())
}

fn dump_by(fs: &SliceFsFilesystem, inode: impl Fn(u64) -> InodeMeta) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut todo = vec![(1u64, String::new())];
    while let Some((dir, path)) = todo.pop() {
        for e in fs.meta().list_directory(dir).unwrap() {
            if e.name == "." || e.name == ".." {
                continue;
            }
            let p = format!("{path}/{}", e.name);
            let m = inode(e.ino);
            let mut xattrs: Vec<(String, Vec<u8>)> = fs.meta().list_xattrs(e.ino).unwrap_or_default()
                .into_iter().map(|n| { let v = fs.meta().get_xattr(e.ino, &n).unwrap(); (n, v) }).collect();
            xattrs.sort();
            let body = match m.mode & 0o170_000 {
                S_IFDIR => { todo.push((e.ino, p.clone())); String::new() }
                S_IFREG => format!("content={:x?}", blake(&fs.test_read(e.ino, 0, m.size as u32 + 1).unwrap())),
                0o120_000 => format!("target={}", fs.simulate_readlink(e.ino).unwrap()),
                _ => String::new(),
            };
            out.insert(p, format!(
                "ino={} mode={:o} uid={} gid={} size={} nlinks={} mtime={}.{} ctime={}.{} atime={}.{} xattrs={:?} {body}",
                e.ino, m.mode, m.uid, m.gid, m.size, m.nlinks, m.mtime_sec, m.mtime_nsec, m.ctime_sec, m.ctime_nsec,
                m.atime_sec, m.atime_nsec, xattrs
            ));
        }
    }
    out
}

fn blake(data: &[u8]) -> u64 {
    // A short content fingerprint (FNV-1a) keeps the dump readable.
    data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x1_0000_0001_b3))
}

fn data(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

fn create(fs: &SliceFsFilesystem, parent: u64, name: &str) -> (u64, u64) {
    match h::handle_create(fs, parent, name, S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR) {
        CreateResult::Ok(ino, fh, _) => (ino, fh),
        _ => panic!("create {name}"),
    }
}

fn write(fs: &SliceFsFilesystem, fh: u64, off: u64, bytes: &[u8]) {
    assert!(matches!(h::handle_write(fs, fh, off, bytes), WriteResult::Ok(_)));
}

fn fsync(fs: &SliceFsFilesystem, ino: u64, fh: u64) {
    assert!(matches!(h::handle_fsync(fs, ino, fh), EmptyResult::Ok));
}

fn release(fs: &SliceFsFilesystem, ino: u64, fh: u64) {
    assert!(matches!(h::handle_release(fs, ino, fh), EmptyResult::Ok));
}

/// Run `scenario`, release nothing more, sync the log (as fsync does), take
/// the live dump, "crash", replay, and compare.
fn check(scenario: impl FnOnce(&SliceFsFilesystem)) {
    let dir = TempDir::new().unwrap();
    let live = {
        let fs = new_fs(dir.path());
        scenario(&fs);
        fs.meta().flush_wal().unwrap();
        let d = dump(&fs);
        std::mem::forget(fs); // a crash: no commit, no destroy
        d
    };
    let replayed = dump(&reload_and_replay(dir.path()));
    assert_eq!(replayed, live);
}

/// `check` with a log mode, for scenarios that crash with write handles
/// open: both sides are dumped as stat sees them (`dump_open`). Returns the
/// replayed file system (and its store) for more checks.
fn check_with(config: WalConfig, scenario: impl FnOnce(&SliceFsFilesystem)) -> (TempDir, SliceFsFilesystem) {
    let dir = TempDir::new().unwrap();
    let live = {
        let fs = new_fs_with(dir.path(), config.clone());
        scenario(&fs);
        fs.meta().flush_wal().unwrap();
        let d = dump_open(&fs);
        std::mem::forget(fs); // a crash: no commit, no destroy
        d
    };
    let fs = reload_and_replay(dir.path());
    assert_eq!(dump_open(&fs), live, "{config:?}");
    (dir, fs)
}

#[test]
fn files_written_and_fsynced_survive_a_crash() {
    check(|fs| {
        let (a, fa) = create(fs, 1, "a.bin");
        write(fs, fa, 0, &data(1, 300_000));
        fsync(fs, a, fa);
        release(fs, a, fa);
        let (b, fb) = create(fs, 1, "b.txt");
        write(fs, fb, 0, b"hello");
        release(fs, b, fb);
    });
}

#[test]
fn directories_renames_links_and_unlinks_replay() {
    check(|fs| {
        let d = match h::handle_mkdir(fs, 1, "d", S_IFDIR | 0o755, 0o022, 501, 20) { EntryResult::Ok(i, _) => i, _ => panic!() };
        let (a, fa) = create(fs, d, "a");
        write(fs, fa, 0, &data(2, 50_000));
        release(fs, a, fa);
        assert!(matches!(h::handle_link(fs, a, 1, "hard"), EntryResult::Ok(..)));
        assert!(matches!(h::handle_rename(fs, d, "a", 1, "moved", 0), EmptyResult::Ok));
        assert!(matches!(h::handle_symlink(fs, 1, "sym", "moved", 501, 20), EntryResult::Ok(..)));
        let (x, fx) = create(fs, 1, "gone");
        release(fs, x, fx);
        assert!(matches!(h::handle_unlink(fs, 1, "gone"), EmptyResult::Ok));
        let e = match h::handle_mkdir(fs, 1, "e", S_IFDIR | 0o700, 0, 501, 20) { EntryResult::Ok(i, _) => i, _ => panic!() };
        let _ = e;
        assert!(matches!(h::handle_rmdir(fs, 1, "e"), EmptyResult::Ok));
    });
}

#[test]
fn attributes_xattrs_truncate_and_overwrite_replay() {
    check(|fs| {
        let (a, fa) = create(fs, 1, "a");
        write(fs, fa, 0, &data(3, 100_000));
        release(fs, a, fa);
        assert!(matches!(h::handle_setattr(fs, a, Some(S_IFREG | 0o600), None, Some(30), None, None, None, Some((1_600_000_000, 7))), h::AttrResult::Ok(_)));
        assert!(matches!(h::handle_setxattr(fs, a, "user.k", b"v1"), EmptyResult::Ok));
        assert!(matches!(h::handle_setxattr(fs, a, "user.j", b"v2"), EmptyResult::Ok));
        assert!(matches!(h::handle_removexattr(fs, a, "user.j"), EmptyResult::Ok));
        // Overwrite with O_TRUNC, then shrink with setattr.
        let fh = match h::handle_open(fs, a, libc::O_RDWR | libc::O_TRUNC) { OpenResult::Ok(fh, _) => fh, _ => panic!() };
        write(fs, fh, 0, &data(4, 20_000));
        release(fs, a, fh);
        assert!(matches!(h::handle_setattr(fs, a, None, None, None, Some(1000), None, None, None), h::AttrResult::Ok(_)));
    });
}

#[test]
fn a_file_still_open_at_the_crash_keeps_its_fsynced_bytes() {
    let dir = TempDir::new().unwrap();
    let want = data(5, 200_000);
    {
        let fs = new_fs(dir.path());
        let (a, fa) = create(&fs, 1, "open.bin");
        write(&fs, fa, 0, &want);
        fsync(&fs, a, fa);
        std::mem::forget(fs); // crash with the handle open
    }
    let fs = reload_and_replay(dir.path());
    let ino = fs.meta().lookup(1, "open.bin").unwrap();
    assert_eq!(fs.test_read(ino, 0, want.len() as u32 + 1).unwrap(), want);
}

#[test]
fn a_commit_with_open_files_is_a_clean_cut() {
    let dir = TempDir::new().unwrap();
    let (s1, s2) = (data(6, 400_000), data(7, 200_000));
    let mut b = data(8, 300_000);
    {
        let fs = new_fs(dir.path());
        // Streaming handle, open across the commit.
        let (a, fa) = create(&fs, 1, "stream.bin");
        write(&fs, fa, 0, &s1);
        // A handle that overwrites earlier bytes (not sequential).
        let (bi, fb) = create(&fs, 1, "rewrite.bin");
        write(&fs, fb, 0, &b);
        write(&fs, fb, 1000, &[0xAB; 5000]);
        b[1000..6000].fill(0xAB);
        // The commit: everything logged before it must be in the root.
        let _gate = fs.commit_gate();
        fs.commit_cut().unwrap();
        // After the cut: more writes, then fsync, then a crash.
        write(&fs, fa, s1.len() as u64, &s2);
        write(&fs, fb, 200_000, &[0xCD; 100]);
        b[200_000..200_100].fill(0xCD);
        fsync(&fs, a, fa);
        fsync(&fs, bi, fb);
        std::mem::forget(fs);
    }
    let fs = reload_and_replay(dir.path());
    let a = fs.meta().lookup(1, "stream.bin").unwrap();
    let want: Vec<u8> = s1.iter().chain(s2.iter()).copied().collect();
    assert_eq!(fs.test_read(a, 0, want.len() as u32 + 1).unwrap(), want);
    let bi = fs.meta().lookup(1, "rewrite.bin").unwrap();
    assert_eq!(fs.test_read(bi, 0, b.len() as u32 + 1).unwrap(), b);
}

#[test]
fn a_commit_keeps_out_of_order_bytes_pending_and_the_handle_streaming() {
    let dir = TempDir::new().unwrap();
    let want = data(9, 3 << 20);
    let (a, b, c) = (&want[..1 << 20], &want[1 << 20..2 << 20], &want[2 << 20..]);
    {
        let fs = new_fs(dir.path());
        let (ino, fh) = create(&fs, 1, "ooo.bin");
        write(&fs, fh, 0, a);
        write(&fs, fh, 2 << 20, c); // ahead of the stream: pending (SMB/NFS parallel writes)
        let _gate = fs.commit_gate();
        fs.commit_cut().unwrap();
        write(&fs, fh, 1 << 20, b); // the late write fills the gap
        assert!(fs.test_is_streaming(fh), "the cut must not push the handle into Buffered mode");
        assert_eq!(fs.test_read(ino, 0, want.len() as u32 + 1).unwrap(), want);
        fsync(&fs, ino, fh);
        std::mem::forget(fs);
    }
    let fs = reload_and_replay(dir.path());
    let ino = fs.meta().lookup(1, "ooo.bin").unwrap();
    assert_eq!(fs.test_read(ino, 0, want.len() as u32 + 1).unwrap(), want);
}

// A request that fails after it took an inode number (EEXIST here) logs
// nothing. Replay must still give later creates the inodes they had, or the
// store would not mount after a crash.
#[test]
fn a_failed_create_or_mkdir_does_not_break_replay() {
    check(|fs| {
        let (a, fa) = create(fs, 1, "a");
        release(fs, a, fa);
        assert!(matches!(h::handle_create(fs, 1, "a", S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR), CreateResult::Error(_)));
        assert!(matches!(h::handle_mkdir(fs, 1, "d", S_IFDIR | 0o755, 0o022, 501, 20), EntryResult::Ok(..)));
        assert!(matches!(h::handle_mkdir(fs, 1, "d", S_IFDIR | 0o755, 0o022, 501, 20), EntryResult::Error(_)));
        let (b, fb) = create(fs, 1, "b");
        write(fs, fb, 0, b"data");
        fsync(fs, b, fb);
        release(fs, b, fb);
    });
}

// Two write handles on one file share one write state (POSIX: both write
// the same file). Before, each handle kept its own copy and the last close
// won, so the first handle's bytes were lost, and replay gave a different
// result than the live mount.
#[test]
fn two_write_handles_on_one_file_share_their_writes() {
    check(|fs| {
        let (f, h1) = create(fs, 1, "f");
        let h2 = match h::handle_open(fs, f, libc::O_RDWR) { OpenResult::Ok(fh, _) => fh, _ => panic!("open") };
        write(fs, h1, 0, b"AAAAAAAA");
        write(fs, h2, 0, b"BB");
        release(fs, f, h1);
        write(fs, h2, 8, b"CC");
        release(fs, f, h2);
        let m = fs.meta().get_inode(f).unwrap();
        assert_eq!(fs.test_read(f, 0, m.size as u32 + 1).unwrap(), b"BBAAAAAACC", "live content");
    });
}

// A commit logs the bytes of a Buffered handle again (they are only in RAM).
// After a crash, replay must give the file the mtime of its last write and
// the ctime of its last change, not the time of the commit or of the replay.
#[test]
fn a_commit_keeps_the_times_of_a_buffered_file() {
    let dir = TempDir::new().unwrap();
    let mut want = data(10, 50_000);
    want[100..400].fill(0xEE);
    {
        let fs = new_fs(dir.path());
        let (ino, fh) = {
            let _t = oplog::Clock::freeze_at((1_700_000_000, 0));
            let (ino, fh) = create(&fs, 1, "buf.bin");
            write(&fs, fh, 0, &data(10, 50_000));
            write(&fs, fh, 100, &[0xEE; 300]); // not sequential: Buffered
            (ino, fh)
        };
        assert!(!fs.test_is_streaming(fh));
        {
            let _t = oplog::Clock::freeze_at((1_700_000_100, 0));
            assert!(matches!(h::handle_setattr(&fs, ino, Some(S_IFREG | 0o600), None, None, None, None, None, None),
                h::AttrResult::Ok(_)));
        }
        {
            let _t = oplog::Clock::freeze_at((1_700_000_500, 0));
            let _gate = fs.commit_gate();
            fs.commit_cut().unwrap();
        }
        fs.meta().flush_wal().unwrap();
        std::mem::forget(fs); // a crash with the handle open
    }
    let fs = reload_and_replay(dir.path());
    let ino = fs.meta().lookup(1, "buf.bin").unwrap();
    let m = fs.meta().get_inode(ino).unwrap();
    assert_eq!(fs.test_read(ino, 0, want.len() as u32 + 1).unwrap(), want);
    assert_eq!((m.size, m.mode), (50_000, S_IFREG | 0o600));
    assert_eq!((m.mtime_sec, m.mtime_nsec), (1_700_000_000, 0), "mtime is the time of the last write");
    assert_eq!((m.ctime_sec, m.ctime_nsec), (1_700_000_100, 0), "ctime is the time of the chmod");
}

// A commit deletes the old log segments and logs the RAM-only bytes again.
// Those records must be on disk before the commit returns: an fsync before
// the commit made them durable, and no later fsync may come before a crash.
// Only a log that buffers records (--wal flush-on-fsync) can lose them.
#[test]
fn fsynced_buffered_bytes_survive_a_commit_and_a_crash() {
    let dir = TempDir::new().unwrap();
    let mut want = data(11, 2_000);
    want[100..400].fill(0xEE);
    {
        let fs = new_fs_with(dir.path(), WalConfig::FlushOnFsync);
        let (ino, fh) = create(&fs, 1, "buf.bin");
        write(&fs, fh, 0, &data(11, 2_000));
        write(&fs, fh, 100, &[0xEE; 300]); // not sequential: Buffered
        assert!(!fs.test_is_streaming(fh));
        fsync(&fs, ino, fh);
        fs.commit_cut().unwrap();
        std::mem::forget(fs); // a crash right after the commit, no sync
    }
    let fs = reload_and_replay(dir.path());
    let ino = fs.meta().lookup(1, "buf.bin").unwrap();
    assert_eq!(fs.test_read(ino, 0, want.len() as u32 + 1).unwrap(), want);
}

// #20: the safe-save pattern. Write a temp file, fsync it, rename it over the
// old file, then fsync the directory. With a log that buffers records
// (--wal flush-on-fsync), only the fsync of the directory makes the rename
// durable. `fsync_dir` selects whether that last step runs.
fn safe_save_then_crash(store: &Path, fsync_dir: bool) {
    let fs = new_fs_with(store, WalConfig::FlushOnFsync);
    let (old, fo) = create(&fs, 1, "doc.txt");
    write(&fs, fo, 0, b"old");
    release(&fs, old, fo);
    fs.commit_cut().unwrap(); // doc.txt is in the root
    let (tmp, ft) = create(&fs, 1, "doc.txt.tmp");
    write(&fs, ft, 0, &data(14, 30_000));
    fsync(&fs, tmp, ft); // the temp file is durable
    release(&fs, tmp, ft);
    assert!(matches!(h::handle_rename(&fs, 1, "doc.txt.tmp", 1, "doc.txt", 0), EmptyResult::Ok));
    if fsync_dir {
        assert!(matches!(h::handle_fsyncdir(&fs), EmptyResult::Ok));
    }
    // A crash. drop (not mem::forget) runs every Drop implementation: if one
    // flushed the log, the control test below would fail.
    drop(fs);
}

#[test]
fn a_rename_survives_a_crash_after_fsync_of_the_directory() {
    let dir = TempDir::new().unwrap();
    safe_save_then_crash(dir.path(), true);
    let fs = reload_and_replay(dir.path());
    let ino = fs.meta().lookup(1, "doc.txt").unwrap();
    let want = data(14, 30_000);
    assert_eq!(fs.test_read(ino, 0, want.len() as u32 + 1).unwrap(), want);
    assert!(fs.meta().lookup(1, "doc.txt.tmp").is_err());
}

// The control for the test above: with a buffered log and no sync after the
// rename, the rename is lost. So only the fsync of the directory keeps it.
#[test]
fn a_rename_with_no_sync_after_it_is_lost_with_a_buffered_log() {
    let dir = TempDir::new().unwrap();
    safe_save_then_crash(dir.path(), false);
    let fs = reload_and_replay(dir.path());
    let old = fs.meta().lookup(1, "doc.txt").unwrap();
    assert_eq!(fs.test_read(old, 0, 4).unwrap(), b"old");
    let tmp = fs.meta().lookup(1, "doc.txt.tmp").unwrap();
    let want = data(14, 30_000);
    assert_eq!(fs.test_read(tmp, 0, want.len() as u32 + 1).unwrap(), want);
}

/// The log segment with the largest number (by number, not by name).
fn newest_segment(store: &Path) -> PathBuf {
    std::fs::read_dir(store.join("segments")).unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            let n: u64 = p.file_name()?.to_str()?.strip_prefix("segment-")?.strip_suffix(".seg")?.parse().ok()?;
            Some((n, p))
        })
        .max_by_key(|(n, _)| *n)
        .unwrap()
        .1
}

// #20: a power loss can leave the last record of the log half written. The
// mount must still work: the reader stops at the torn record, replay keeps
// every record before it, and new writes work after the commit a mount does.
#[test]
fn a_torn_last_record_does_not_stop_the_mount() {
    let dir = TempDir::new().unwrap();
    let kept = data(15, 200_000);
    let before = {
        let fs = new_fs(dir.path());
        let (k, fk) = create(&fs, 1, "kept.bin");
        write(&fs, fk, 0, &kept);
        fsync(&fs, k, fk);
        release(&fs, k, fk);
        let d = match h::handle_mkdir(&fs, 1, "d", S_IFDIR | 0o755, 0o022, 501, 20) { EntryResult::Ok(i, _) => i, _ => panic!() };
        let (s, fsm) = create(&fs, d, "small");
        write(&fs, fsm, 0, b"x");
        release(&fs, s, fsm);
        assert!(matches!(h::handle_rename(&fs, d, "small", d, "renamed", 0), EmptyResult::Ok));
        fs.meta().flush_wal().unwrap();
        let before = dump(&fs);
        // The sacrificial last record: one mkdir logs one record.
        assert!(matches!(h::handle_mkdir(&fs, 1, "last", S_IFDIR | 0o755, 0o022, 501, 20), EntryResult::Ok(..)));
        fs.meta().flush_wal().unwrap();
        std::mem::forget(fs); // a crash
        before
    };
    // Cut before the reload: reload_and_replay makes a new segment.
    let seg = newest_segment(dir.path());
    let f = std::fs::OpenOptions::new().write(true).open(&seg).unwrap();
    let len = f.metadata().unwrap().len();
    f.set_len(len - 3).unwrap();
    drop(f);

    let after = data(16, 50_000);
    {
        let fs = reload_and_replay(dir.path());
        assert_eq!(dump(&fs), before);
        assert!(fs.meta().lookup(1, "last").is_err(), "the torn record must not be applied");
        // What a mount does after replay, then new writes.
        fs.meta().commit().unwrap();
        let (a, fa) = create(&fs, 1, "after.bin");
        write(&fs, fa, 0, &after);
        fsync(&fs, a, fa);
        release(&fs, a, fa);
        assert_eq!(fs.test_read(a, 0, after.len() as u32 + 1).unwrap(), after);
        std::mem::forget(fs); // a second crash
    }
    let fs = reload_and_replay(dir.path());
    let a = fs.meta().lookup(1, "after.bin").unwrap();
    assert_eq!(fs.test_read(a, 0, after.len() as u32 + 1).unwrap(), after);
    let k = fs.meta().lookup(1, "kept.bin").unwrap();
    assert_eq!(fs.test_read(k, 0, kept.len() as u32 + 1).unwrap(), kept);
    let d = fs.meta().lookup(1, "d").unwrap();
    assert!(fs.meta().lookup(d, "renamed").is_ok());
}

// utimens between a write and the close sets the mtime (cp -p, install -p):
// the close does not stamp the time of the write over it.
#[test]
fn an_explicit_mtime_after_a_write_is_kept_at_close_and_replay() {
    let dir = TempDir::new().unwrap();
    {
        let fs = new_fs(dir.path());
        let (ino, fh) = {
            let _t = oplog::Clock::freeze_at((1_700_000_000, 0));
            let (ino, fh) = create(&fs, 1, "p.bin");
            write(&fs, fh, 0, &data(12, 10_000));
            (ino, fh)
        };
        {
            let _t = oplog::Clock::freeze_at((1_700_000_100, 0));
            assert!(matches!(h::handle_setattr(&fs, ino, None, None, None, None, Some(fh), None, Some((1_600_000_000, 7))),
                h::AttrResult::Ok(_)));
        }
        {
            let _t = oplog::Clock::freeze_at((1_700_000_200, 0));
            release(&fs, ino, fh);
        }
        let m = fs.meta().get_inode(ino).unwrap();
        assert_eq!((m.mtime_sec, m.mtime_nsec), (1_600_000_000, 7), "live mtime after close");
        fs.meta().flush_wal().unwrap();
        std::mem::forget(fs);
    }
    let fs = reload_and_replay(dir.path());
    let ino = fs.meta().lookup(1, "p.bin").unwrap();
    let m = fs.meta().get_inode(ino).unwrap();
    assert_eq!((m.mtime_sec, m.mtime_nsec), (1_600_000_000, 7), "replayed mtime");
}

// stat of a file open for writing shows the time of the last write, as on
// other file systems, before the first flush.
#[test]
fn stat_of_an_open_file_shows_the_time_of_the_last_write() {
    let dir = TempDir::new().unwrap();
    let fs = new_fs(dir.path());
    let (ino, fh) = {
        let _t = oplog::Clock::freeze_at((1_700_000_000, 0));
        create(&fs, 1, "s.bin")
    };
    {
        let _t = oplog::Clock::freeze_at((1_700_000_300, 0));
        write(&fs, fh, 0, &data(13, 1_000));
    }
    let h::AttrResult::Ok(a) = h::handle_getattr(&fs, ino) else { panic!("getattr") };
    let secs = |t: std::time::SystemTime| t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert_eq!(a.size, 1_000);
    assert_eq!(secs(a.mtime), 1_700_000_300);
    assert_eq!(secs(a.ctime), 1_700_000_300);
}

/// Open `ino` for writing through the handler (O_RDWR, no truncate).
fn open_rw(fs: &SliceFsFilesystem, ino: u64) -> u64 {
    match h::handle_open(fs, ino, libc::O_RDWR) {
        OpenResult::Ok(fh, true) => fh,
        _ => panic!("open {ino}"),
    }
}

/// Write through the handler and into the model of the file.
fn put(fs: &SliceFsFilesystem, fh: u64, model: &mut Vec<u8>, off: usize, bytes: &[u8]) {
    write(fs, fh, off as u64, bytes);
    if model.len() < off + bytes.len() {
        model.resize(off + bytes.len(), 0);
    }
    model[off..off + bytes.len()].copy_from_slice(bytes);
}

/// Freeze the clock at `t` seconds (one distinct time per step).
fn at(t: i64) -> oplog::Clock {
    oplog::Clock::freeze_at((t, 0))
}

// BUFR-01 (D-05, #93): a commit logs again only the bytes changed since the
// last rebase of an open overlay. That is bounded by the dirty limit, not by
// the file size: the three values below are the same for 4 MiB and 16 MiB.
// Before #98 each commit logged the whole Buffered file again.
#[test]
fn a_commit_relogs_only_the_bytes_changed_since_the_last_rebase() {
    let mut seen = Vec::new();
    for (seed, size) in [(30u64, 4usize << 20), (31, 16 << 20)] {
        let dir = TempDir::new().unwrap();
        let fs = new_fs(dir.path());
        let mut model = data(seed, size);
        let t0 = 1_700_000_000i64;
        let ino = {
            let _t = at(t0);
            let (ino, fh) = create(&fs, 1, "big.bin");
            for (i, piece) in model.chunks(1 << 20).enumerate() {
                write(&fs, fh, (i << 20) as u64, piece);
            }
            release(&fs, ino, fh);
            ino
        };
        let fh = open_rw(&fs, ino);
        let rebases = fs.test_rebase_count();

        // One 4 KiB write (the switch to an overlay), then a commit.
        { let _t = at(t0 + 10); put(&fs, fh, &mut model, 1 << 20, &[0x11; 4096]); }
        { let _t = at(t0 + 20); fs.commit_cut().unwrap(); }
        let one = fs.test_last_relog_bytes();

        // A second 4 KiB write far from the first, no rebase between: both
        // extents are still dirty against the same base.
        { let _t = at(t0 + 30); put(&fs, fh, &mut model, size / 2 + 12_345, &[0x22; 4096]); }
        { let _t = at(t0 + 40); fs.commit_cut().unwrap(); }
        let two = fs.test_last_relog_bytes();
        assert_eq!(fs.test_rebase_count(), rebases, "no rebase at a commit");

        // One forced rebase: a low limit for one 8 KiB write only (an extent
        // costs its capacity + 64 B, so a 4 KiB write alone passes 4096),
        // then the default limit back before the last write.
        fs.test_set_dirty_limit(4096);
        { let _t = at(t0 + 50); put(&fs, fh, &mut model, size - (64 << 10), &[0x33; 8192]); }
        assert_eq!(fs.test_rebase_count(), rebases + 1, "the 8 KiB write rebases once");
        fs.test_set_dirty_limit(slicefs_cli::filesystem::DIRTY_LIMIT);
        { let _t = at(t0 + 60); put(&fs, fh, &mut model, (3 << 20) + 7, &[0x44; 4096]); }
        { let _t = at(t0 + 70); fs.commit_cut().unwrap(); }
        let three = fs.test_last_relog_bytes();
        assert_eq!(fs.test_rebase_count(), rebases + 1, "no rebase at a commit");

        eprintln!("relog bytes, {} MiB file: {one}, {two}, {three}", size >> 20);
        assert_eq!((one, two, three), (4096, 8192, 4096), "{} MiB file", size >> 20);
        assert_eq!(fs.test_read(ino, 0, size as u32 + 1).unwrap(), model, "live content");
        seen.push((one, two, three));

        // A crash with the handle open: replay gives the same file.
        fs.meta().flush_wal().unwrap();
        let live = dump_open(&fs);
        std::mem::forget(fs);
        let fs = reload_and_replay(dir.path());
        assert_eq!(dump_open(&fs), live, "{} MiB file after replay", size >> 20);
        assert_eq!(fs.test_read(ino, 0, size as u32 + 1).unwrap(), model, "replayed content");
    }
    assert_eq!(seen[0], seen[1], "the relog does not depend on the file size");
}

#[derive(Clone, Copy, Debug)]
enum CrashPoint {
    /// Only the logged requests after the last commit.
    RawRecords,
    /// Right after a commit that relogged a shrunk, extended, dirty overlay.
    RelogRecords,
    /// After a rebase, a commit and more writes.
    AfterRebase,
}

const MIB: usize = 1 << 20;

/// The overlay work of `an_overlay_replays_after_a_crash_with_each_log_mode`,
/// then the steps of `crash`. Returns the ino and the model of a.bin.
fn overlay_scenario(fs: &SliceFsFilesystem, crash: CrashPoint) -> (u64, Vec<u8>) {
    let t0 = 1_700_000_000i64;
    let mut model = data(20, 3 * MIB);
    let ino = {
        let _t = at(t0);
        let (ino, fh) = create(fs, 1, "a.bin");
        for (i, piece) in model.chunks(64 << 10).enumerate() {
            write(fs, fh, (i << 16) as u64, piece);
        }
        release(fs, ino, fh);
        ino
    };
    { let _t = at(t0 + 10); fs.commit_cut().unwrap(); }
    let fh = open_rw(fs, ino);
    let size = |n: usize| {
        assert!(matches!(h::handle_setattr(fs, ino, None, None, None, Some(n as u64), Some(fh), None, None),
            h::AttrResult::Ok(_)));
    };
    { let _t = at(t0 + 20); put(fs, fh, &mut model, MIB, &[0x11; 4096]); } // the switch
    { let _t = at(t0 + 30); size(2 * MIB); model.truncate(2 * MIB); } // shrink below the base
    // Extend: [2 MiB, 3.5 MiB) must read as zeros, not as the old bytes (D-15).
    { let _t = at(t0 + 40); size(7 * MIB / 2); model.resize(7 * MIB / 2, 0); }
    { let _t = at(t0 + 50); put(fs, fh, &mut model, 3 * MIB, &[0x22; 4096]); }
    // Sparse: 1 MiB past the end, so [3.5 MiB, 4.5 MiB) is a hole.
    { let _t = at(t0 + 60); put(fs, fh, &mut model, 9 * MIB / 2, &[0x33; 4096]); }
    {
        let _t = at(t0 + 70); // a chmod: ctime after the last write
        assert!(matches!(h::handle_setattr(fs, ino, Some(S_IFREG | 0o600), None, None, None, None, None, None),
            h::AttrResult::Ok(_)));
    }
    assert!(!fs.test_is_streaming(fh));
    match crash {
        CrashPoint::RawRecords => {}
        CrashPoint::RelogRecords => {
            // b.bin switches to an overlay before its first flush (D-18).
            {
                let _t = at(t0 + 80);
                let (_, fb) = create(fs, 1, "b.bin");
                for (i, piece) in data(21, 50_000).chunks(10_000).enumerate() {
                    write(fs, fb, (i * 10_000) as u64, piece);
                }
                write(fs, fb, 100, &[0xBB; 300]);
                assert!(!fs.test_is_streaming(fb));
            }
            // The overlay is still shrunk, extended and dirty: the relog has
            // Setattr(visible), Setattr(size), the Write pieces and the ctime.
            { let _t = at(t0 + 90); fs.commit_cut().unwrap(); }
            // a.bin: 3 extents of 4 KiB; b.bin: 300 bytes.
            assert_eq!(fs.test_last_relog_bytes(), 3 * 4096 + 300);
        }
        CrashPoint::AfterRebase => {
            let rebases = fs.test_rebase_count();
            fs.test_set_dirty_limit(64 << 10);
            { let _t = at(t0 + 80); put(fs, fh, &mut model, 512 << 10, &[0x44; 128 << 10]); }
            assert_eq!(fs.test_rebase_count(), rebases + 1, "the 128 KiB write rebases");
            fs.test_set_dirty_limit(slicefs_cli::filesystem::DIRTY_LIMIT);
            { let _t = at(t0 + 90); put(fs, fh, &mut model, 200, &[0x55; 4096]); }
            { let _t = at(t0 + 100); fs.commit_cut().unwrap(); }
            assert_eq!(fs.test_last_relog_bytes(), 4096, "only the write after the rebase");
            { let _t = at(t0 + 110); put(fs, fh, &mut model, 100, &[0x66; 4096]); }
            { let _t = at(t0 + 120); fsync(fs, ino, fh); }
        }
    }
    assert_eq!(fs.test_read(ino, 0, model.len() as u32 + 1).unwrap(), model, "live content");
    (ino, model)
}

// BUFR-02 (D-14, D-15, D-18): replay after a crash gives the same names,
// sizes, content, mtime and ctime as before it, for both log modes and three
// crash points. Crash point RelogRecords tells the order of the relog: if
// Setattr(size) came before Setattr(visible), or one was left out, the old
// bytes of [2 MiB, 3.5 MiB) would come back and the dumps would differ.
#[test]
fn an_overlay_replays_after_a_crash_with_each_log_mode() {
    for config in [WalConfig::PerOp, WalConfig::FlushOnFsync] {
        for crash in [CrashPoint::RawRecords, CrashPoint::RelogRecords, CrashPoint::AfterRebase] {
            let mut want = None;
            let (_dir, fs) = check_with(config.clone(), |fs| want = Some(overlay_scenario(fs, crash)));
            let (ino, model) = want.unwrap();
            let what = format!("{config:?} {crash:?}");
            assert_eq!(fs.test_read(ino, 0, model.len() as u32 + 1).unwrap(), model, "{what}: content");
            // Old base bytes there, before the write at 3 MiB.
            assert!(fs.test_read(ino, 2 * MIB as u64, MIB as u32).unwrap().iter().all(|&b| b == 0),
                "{what}: [2 MiB, 3 MiB) is zeros");
            let m = fs.test_getattr(ino).unwrap();
            assert_eq!((m.size, m.mode), (model.len() as u64, S_IFREG | 0o600), "{what}");
        }
    }
}

// D-20: a commit relogs an overlay in pieces of at most 1 MiB. Before #98 one
// record held the whole buffer, and a record over MAX_PAYLOAD (64 MiB)
// stopped the segment reader, so this record and every later one were lost.
#[test]
fn an_overlay_with_more_than_64_mib_of_relog_replays() {
    let dir = TempDir::new().unwrap();
    let mut model = data(40, MIB);
    let ino = {
        let fs = new_fs(dir.path());
        let (ino, fh) = create(&fs, 1, "huge.bin");
        write(&fs, fh, 0, &model.clone());
        put(&fs, fh, &mut model, 0, &[0x77; 4096]); // the switch
        // One extent that grows by 1 MiB per write. An extent costs its
        // capacity, which grows by at most 1/8 (overlay::GROW_DIV): at most
        // about 73 MiB at 65 MiB of length. So the limit is above that, or a
        // rebase would run in the loop. The budget of all files too (#105).
        fs.test_set_dirty_limit(96 << 20);
        fs.test_set_dirty_budget(96 << 20);
        let rebases = fs.test_rebase_count();
        // 65 MiB: the smallest whole number of MiB above MAX_PAYLOAD. Each
        // MiB is one random MiB with its index in the first 8 bytes.
        let mut piece = data(41, MIB);
        for i in 0..65usize {
            piece[..8].copy_from_slice(&(i as u64).to_le_bytes());
            put(&fs, fh, &mut model, (i + 1) * MIB, &piece);
        }
        assert_eq!(fs.test_rebase_count(), rebases, "no rebase: one 65 MiB extent");
        fs.commit_cut().unwrap();
        assert!(fs.test_last_relog_bytes() > 64 << 20, "relog {}", fs.test_last_relog_bytes());
        fs.meta().flush_wal().unwrap();
        std::mem::forget(fs); // a crash with the handle open
        ino
    };
    // The same high limit in replay: no rebase while the 65 pieces come in
    // (each would hash the whole file again), one at the end of replay.
    let fs = reload_and_replay_with(dir.path(), |fs| {
        fs.test_set_dirty_limit(96 << 20);
        fs.test_set_dirty_budget(96 << 20);
    });
    let got = fs.test_read(ino, 0, model.len() as u32 + 1).unwrap();
    if got != model {
        // Only on a failure: a byte loop is slow in a debug build.
        let diff = got.iter().zip(&model).position(|(a, b)| a != b);
        panic!("replayed {} bytes, want {}; first difference at {diff:?}", got.len(), model.len());
    }
}

// #108: a write past the dirty limit rebases one bounded step, so an overlay
// gets many new roots while it is open, with commits between them. A crash
// after more steps and writes replays to the same content and stat, with the
// same limit in replay (steps again) and with the default limit (none).
#[test]
fn rebase_steps_with_commits_between_them_replay_after_a_crash() {
    for replay_limit in [5usize << 18, slicefs_cli::filesystem::DIRTY_LIMIT] {
        let dir = TempDir::new().unwrap();
        let mut model = data(50, 8 * MIB);
        let (ino, live) = {
            let fs = new_fs_with(dir.path(), WalConfig::FlushOnFsync);
            let t0 = 1_700_000_000i64;
            let ino = {
                let _t = at(t0);
                let (ino, fh) = create(&fs, 1, "steps.bin");
                for (i, piece) in model.clone().chunks(MIB).enumerate() {
                    write(&fs, fh, (i * MIB) as u64, piece);
                }
                release(&fs, ino, fh);
                ino
            };
            { let _t = at(t0 + 1); fs.commit_cut().unwrap(); }
            fs.test_set_dirty_limit(5 << 18);
            let fh = open_rw(&fs, ino);
            let rebases = fs.test_rebase_count();
            let mut commits_after_a_step = 0;
            for i in 0..1200usize {
                let _t = at(t0 + 10 + i as i64);
                let r = data(1000 + i as u64, 8).iter().fold(0u64, |a, &b| a << 8 | b as u64);
                let off = (r % (8 * MIB - 4096) as u64) as usize;
                put(&fs, fh, &mut model, off, &data(3000 + i as u64, 4096));
                if i % 300 == 299 {
                    if fs.test_rebase_count() > rebases {
                        commits_after_a_step += 1;
                    }
                    fs.commit_cut().unwrap();
                }
            }
            let steps = fs.test_rebase_count() - rebases;
            assert!(steps >= 3, "{steps} rebase steps");
            assert!(commits_after_a_step >= 2, "{commits_after_a_step} commits after a step");
            assert!(fs.test_read(ino, 0, model.len() as u32).unwrap() == model, "live content");
            fs.meta().flush_wal().unwrap();
            let d = dump_open(&fs);
            std::mem::forget(fs); // a crash with the handle open
            (ino, d)
        };
        let fs = reload_and_replay_with(dir.path(), |fs| fs.test_set_dirty_limit(replay_limit));
        assert!(fs.test_read(ino, 0, model.len() as u32 + 1).unwrap() == model, "replay limit {replay_limit}: content");
        assert_eq!(dump_open(&fs), live, "replay limit {replay_limit}");
    }
}

// #105: under one budget for all open files, a write to one file stores
// steps of another. Two open files, writes to each in turn, commits between
// the cross-file steps, then a crash: replay gives the same content and
// stat, with the same budget in replay and with the default one.
#[test]
fn cross_file_budget_steps_with_commits_replay_after_a_crash() {
    let budget = 5usize << 18;
    for replay_budget in [budget, slicefs_cli::filesystem::DIRTY_BUDGET] {
        let dir = TempDir::new().unwrap();
        let mut models = [data(60, 4 * MIB), data(61, 4 * MIB)];
        let (inos, live) = {
            let fs = new_fs_with(dir.path(), WalConfig::FlushOnFsync);
            let t0 = 1_700_000_000i64;
            let mut inos = [0u64; 2];
            for (k, name) in ["x.bin", "y.bin"].iter().enumerate() {
                let _t = at(t0 + k as i64);
                let (ino, fh) = create(&fs, 1, name);
                for (i, piece) in models[k].clone().chunks(MIB).enumerate() {
                    write(&fs, fh, (i * MIB) as u64, piece);
                }
                release(&fs, ino, fh);
                inos[k] = ino;
            }
            { let _t = at(t0 + 5); fs.commit_cut().unwrap(); }
            fs.test_set_dirty_budget(budget);
            let fhs = [open_rw(&fs, inos[0]), open_rw(&fs, inos[1])];
            let rebases = fs.test_rebase_count();
            let mut commits_after_a_step = 0;
            for i in 0..800usize {
                let _t = at(t0 + 10 + i as i64);
                let k = i % 2;
                let r = data(2000 + i as u64, 8).iter().fold(0u64, |a, &b| a << 8 | b as u64);
                let off = (r % (4 * MIB - 4096) as u64) as usize;
                put(&fs, fhs[k], &mut models[k], off, &data(4000 + i as u64, 4096));
                assert!(fs.test_dirty_total() <= budget, "dirty total {}", fs.test_dirty_total());
                if i % 200 == 199 {
                    if fs.test_rebase_count() > rebases {
                        commits_after_a_step += 1;
                    }
                    fs.commit_cut().unwrap();
                }
            }
            let steps = fs.test_rebase_count() - rebases;
            assert!(steps >= 3, "{steps} rebase steps");
            assert!(commits_after_a_step >= 2, "{commits_after_a_step} commits after a step");
            for k in 0..2 {
                assert!(fs.test_read(inos[k], 0, (4 * MIB) as u32).unwrap() == models[k], "live content {k}");
            }
            fs.meta().flush_wal().unwrap();
            let d = dump_open(&fs);
            std::mem::forget(fs); // a crash with both handles open
            (inos, d)
        };
        let fs = reload_and_replay_with(dir.path(), |fs| fs.test_set_dirty_budget(replay_budget));
        for k in 0..2 {
            assert!(fs.test_read(inos[k], 0, (4 * MIB) as u32 + 1).unwrap() == models[k],
                "replay budget {replay_budget}: content {k}");
        }
        assert_eq!(dump_open(&fs), live, "replay budget {replay_budget}");
    }
}

// ── #108: a crash between the generation switch and the root ────────────────

/// Where `cut_window_scenario` stops the commit that it starts.
#[derive(Clone, Copy, Debug)]
enum Window {
    /// Right after the cut (the gate is released), before any node is written.
    AfterCut,
    /// The nodes of the root are written, nothing is synced.
    AfterNodes,
    /// The nodes and the content are synced, the root is not logged.
    AfterSyncs,
    /// The root segment is durable, the old segments are not deleted yet.
    AfterRoot,
    /// A power loss while the root segment is written (before its sync): the
    /// old segments are all there, and the root segment lost its last 3
    /// bytes, has only its header, or is empty.
    RootTorn,
    RootHeaderOnly,
    RootEmpty,
    /// The commit is done.
    Done,
    /// The commit is done, and an online GC ran beside its finish.
    GcDuringFinish,
}

/// The segment files of `store`, by id.
fn segment_paths(store: &Path) -> Vec<PathBuf> {
    let mut v: Vec<(u64, PathBuf)> = std::fs::read_dir(store.join("segments")).unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            let n: u64 = p.file_name()?.to_str()?.strip_prefix("segment-")?.strip_suffix(".seg")?.parse().ok()?;
            Some((n, p))
        })
        .collect();
    v.sort();
    v.into_iter().map(|(_, p)| p).collect()
}

/// Files in the old generation (closed, an overlay, a streaming handle with
/// pending bytes, a directory of new files), a commit cut, requests after
/// the cut (in the next generation, some fsynced), then the commit stopped
/// at `w`, then one more write on each open handle.
fn cut_window_scenario(fs: &SliceFsFilesystem, store: &Path, w: Window) {
    use metadata::store::CommitFault;
    let t0 = 1_700_000_000i64;
    let a = {
        let _t = at(t0);
        let (k, fk) = create(fs, 1, "kept.bin");
        write(fs, fk, 0, &data(50, 100_000));
        fsync(fs, k, fk);
        release(fs, k, fk);
        let (a, fa) = create(fs, 1, "a.bin");
        for (i, piece) in data(51, MIB).chunks(64 << 10).enumerate() {
            write(fs, fa, (i << 16) as u64, piece);
        }
        release(fs, a, fa);
        a
    };
    let r = {
        let _t = at(t0 + 1);
        let (r, fr) = create(fs, 1, "r.bin");
        write(fs, fr, 0, &data(56, 512 << 10));
        release(fs, r, fr);
        r
    };
    { let _t = at(t0 + 10); fs.commit_cut().unwrap(); } // the bases are in the root
    let fh = open_rw(fs, a);
    let mut model = data(51, MIB);
    let size = |n: usize| {
        assert!(matches!(h::handle_setattr(fs, a, None, None, None, Some(n as u64), Some(fh), None, None),
            h::AttrResult::Ok(_)));
    };
    // The relog shapes (as in `overlay_scenario`): shrunk below the base,
    // extended, written, then a chmod (ctime after the last write).
    { let _t = at(t0 + 20); put(fs, fh, &mut model, MIB / 2, &[0x11; 4096]); }
    { let _t = at(t0 + 21); size(3 * MIB / 4); model.truncate(3 * MIB / 4); }
    { let _t = at(t0 + 22); size(5 * MIB / 4); model.resize(5 * MIB / 4, 0); }
    { let _t = at(t0 + 23); put(fs, fh, &mut model, MIB + 100, &[0x12; 4096]); }
    {
        let _t = at(t0 + 24);
        assert!(matches!(h::handle_setattr(fs, a, Some(S_IFREG | 0o600), None, None, None, None, None, None),
            h::AttrResult::Ok(_)));
    }
    assert!(!fs.test_is_streaming(fh));
    // An overlay after a rebase, with a write after it.
    let fr = open_rw(fs, r);
    {
        let _t = at(t0 + 25);
        let mut rm = data(56, 512 << 10);
        put(fs, fr, &mut rm, 1000, &[0x13; 4096]); // the switch
        let rebases = fs.test_rebase_count();
        fs.test_set_dirty_limit(64 << 10);
        put(fs, fr, &mut rm, 100 << 10, &[0x14; 128 << 10]);
        assert_eq!(fs.test_rebase_count(), rebases + 1, "the 128 KiB write rebases");
        fs.test_set_dirty_limit(slicefs_cli::filesystem::DIRTY_LIMIT);
        put(fs, fr, &mut rm, 300 << 10, &[0x15; 4096]);
    }
    // b.bin turns into an overlay before its first flush.
    {
        let _t = at(t0 + 26);
        let (_, fb) = create(fs, 1, "b.bin");
        for (i, piece) in data(57, 50_000).chunks(10_000).enumerate() {
            write(fs, fb, (i * 10_000) as u64, piece);
        }
        write(fs, fb, 100, &[0xBB; 300]);
        assert!(!fs.test_is_streaming(fb));
    }
    let fo = {
        let _t = at(t0 + 30);
        let (_, fo) = create(fs, 1, "ooo.bin");
        write(fs, fo, 0, &data(52, 64 << 10));
        write(fs, fo, 128 << 10, &data(53, 64 << 10)); // ahead of the stream: pending
        assert!(fs.test_is_streaming(fo));
        let d = match h::handle_mkdir(fs, 1, "d", S_IFDIR | 0o755, 0o022, 501, 20) { EntryResult::Ok(i, _) => i, _ => panic!() };
        for i in 0..20u8 {
            let (x, fx) = create(fs, d, &format!("x{i}"));
            write(fs, fx, 0, &[i; 100]);
            release(fs, x, fx);
        }
        fo
    };

    // The generation switch (what the commit timer does under the gate).
    let cut = { let _t = at(t0 + 40); fs.commit_cut_begin().unwrap() };
    // Requests after the cut, as they run while the commit finishes.
    {
        let _t = at(t0 + 50);
        put(fs, fh, &mut model, 100, &[0x22; 4096]);
        write(fs, fo, 64 << 10, &data(54, 64 << 10)); // fills the gap
        let (l, fl) = create(fs, 1, "late.bin");
        write(fs, fl, 0, &data(55, 30_000));
        fsync(fs, l, fl);
        release(fs, l, fl);
        assert!(matches!(h::handle_rename(fs, 1, "kept.bin", 1, "kept2.bin", 0), EmptyResult::Ok));
        fsync(fs, a, fh);
    }
    let _t = at(t0 + 60);
    match w {
        Window::AfterCut => std::mem::forget(cut),
        Window::AfterNodes => {
            fs.meta().test_fail_next_commit_at(CommitFault::BeforeSync);
            assert!(fs.commit_cut_finish(cut).is_err());
        }
        Window::AfterSyncs => {
            fs.meta().test_fail_next_commit_at(CommitFault::BeforeRoot);
            assert!(fs.commit_cut_finish(cut).is_err());
        }
        Window::AfterRoot | Window::RootTorn | Window::RootHeaderOnly | Window::RootEmpty => {
            // The old segments are final at the cut: keep a copy, and put back
            // the ones the commit deletes, as a crash before the delete leaves them.
            let saved: Vec<(PathBuf, Vec<u8>)> = segment_paths(store).into_iter()
                .map(|p| { let b = std::fs::read(&p).unwrap(); (p, b) }).collect();
            fs.commit_cut_finish(cut).unwrap();
            let root_seg = segment_paths(store).into_iter()
                .find(|p| !saved.iter().any(|(q, _)| q == p))
                .expect("the commit wrote a root segment");
            for (p, b) in saved {
                if !p.exists() {
                    std::fs::write(&p, b).unwrap();
                }
            }
            let f = std::fs::OpenOptions::new().write(true).open(&root_seg).unwrap();
            let len = f.metadata().unwrap().len();
            match w {
                Window::RootTorn => f.set_len(len - 3).unwrap(),
                Window::RootHeaderOnly => f.set_len(16).unwrap(),
                Window::RootEmpty => f.set_len(0).unwrap(),
                _ => {}
            }
        }
        Window::Done => {
            fs.commit_cut_finish(cut).unwrap();
        }
        Window::GcDuringFinish => {
            // An online GC starts while the commit finishes: it waits for
            // the root, then marks and sweeps beside the requests.
            let stop = std::sync::atomic::AtomicBool::new(false);
            std::thread::scope(|s| {
                let gc = s.spawn(|| fs.gc_online(&stop));
                std::thread::sleep(std::time::Duration::from_millis(50));
                fs.commit_cut_finish(cut).unwrap();
                assert!(matches!(gc.join().unwrap(), Ok(Some(_))), "the GC completes");
            });
        }
    }
    // One more request on each open handle after the stop.
    put(fs, fh, &mut model, 3000, &[0x33; 100]);
    write(fs, fo, 192 << 10, &[0x44; 1000]);
    assert_eq!(fs.test_read(a, 0, model.len() as u32 + 1).unwrap(), model, "live content");
}

// #108: a timed commit releases the gate after the generation switch, and
// logs the root later. A crash at any point between gives the last durable
// root plus the replayed log: the same file system as before the crash.
#[test]
fn a_crash_between_the_cut_and_the_root_replays() {
    // Periodic with a 1 s timer: its syncs run beside the cut and the root install.
    for config in [WalConfig::PerOp, WalConfig::FlushOnFsync, WalConfig::Periodic { interval_secs: 1 }] {
        for w in [Window::AfterCut, Window::AfterNodes, Window::AfterSyncs, Window::AfterRoot,
                  Window::RootTorn, Window::RootHeaderOnly, Window::RootEmpty, Window::Done,
                  Window::GcDuringFinish] {
            let dir = TempDir::new().unwrap();
            let live = {
                let fs = new_fs_with(dir.path(), config.clone());
                cut_window_scenario(&fs, dir.path(), w);
                fs.meta().flush_wal().unwrap();
                let d = dump_open(&fs);
                std::mem::forget(fs); // a crash with the handles open
                d
            };
            let fs = reload_and_replay(dir.path());
            assert_eq!(dump_open(&fs), live, "{config:?} {w:?}");
        }
    }
}

// #108: a power loss after a cut can keep the new segment while the tail of
// the segment before it is lost (the cut does not sync it). The log after
// the cut is then not replayed: replay must not apply it on top of a gap.
#[test]
fn a_torn_segment_before_a_cut_ends_the_log_there() {
    let dir = TempDir::new().unwrap();
    let kept = data(60, 50_000);
    {
        let fs = new_fs(dir.path());
        let (k, fk) = create(&fs, 1, "kept.bin");
        write(&fs, fk, 0, &kept);
        fsync(&fs, k, fk);
        release(&fs, k, fk);
        // The sacrificial last record before the cut.
        assert!(matches!(h::handle_mkdir(&fs, 1, "last", S_IFDIR | 0o755, 0o022, 501, 20), EntryResult::Ok(..)));
        let cut = fs.commit_cut_begin().unwrap();
        let (l, fl) = create(&fs, 1, "late.bin");
        write(&fs, fl, 0, b"late");
        release(&fs, l, fl);
        std::mem::forget(cut);
        // The new segment is on disk; the one before it loses its last record.
        let segs = segment_paths(dir.path());
        let old = &segs[segs.len() - 2];
        let f = std::fs::OpenOptions::new().write(true).open(old).unwrap();
        f.set_len(f.metadata().unwrap().len() - 3).unwrap();
        std::mem::forget(fs);
    }
    let fs = reload_and_replay(dir.path());
    let k = fs.meta().lookup(1, "kept.bin").unwrap();
    assert_eq!(fs.test_read(k, 0, kept.len() as u32 + 1).unwrap(), kept);
    assert!(fs.meta().lookup(1, "last").is_err(), "the torn record is not applied");
    assert!(fs.meta().lookup(1, "late.bin").is_err(), "the log after the gap is not applied");
}

// #108: the break of the test above ends the log only up to the next mount.
// When that replay applies nothing, the mount makes no commit, and its log
// goes into a new segment (no Cut record). Its fsynced requests replay after
// a second crash.
#[test]
fn the_log_of_a_mount_after_a_torn_cut_replays() {
    let dir = TempDir::new().unwrap();
    {
        let fs = new_fs(dir.path());
        // The only record before the cut: torn below.
        assert!(matches!(h::handle_mkdir(&fs, 1, "last", S_IFDIR | 0o755, 0o022, 501, 20), EntryResult::Ok(..)));
        let cut = fs.commit_cut_begin().unwrap();
        let (l, fl) = create(&fs, 1, "late.bin");
        release(&fs, l, fl);
        std::mem::forget(cut);
        let segs = segment_paths(dir.path());
        let old = &segs[segs.len() - 2];
        let f = std::fs::OpenOptions::new().write(true).open(old).unwrap();
        f.set_len(f.metadata().unwrap().len() - 3).unwrap();
        std::mem::forget(fs);
    }
    let after = data(61, 20_000);
    {
        let fs = reload_and_replay(dir.path()); // applies nothing: no commit
        assert!(fs.meta().lookup(1, "late.bin").is_err());
        let (a, fa) = create(&fs, 1, "after.bin");
        write(&fs, fa, 0, &after);
        fsync(&fs, a, fa);
        release(&fs, a, fa);
        fs.meta().flush_wal().unwrap();
        std::mem::forget(fs); // a second crash
    }
    let fs = reload_and_replay(dir.path());
    let a = fs.meta().lookup(1, "after.bin").expect("the mount's log replays");
    assert_eq!(fs.test_read(a, 0, after.len() as u32 + 1).unwrap(), after);
    assert!(fs.meta().lookup(1, "late.bin").is_err());
}
