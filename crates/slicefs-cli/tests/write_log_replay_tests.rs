//! Write log replay (#20): requests made through the handlers after the last
//! commit, then a crash (no commit, no unmount). Loading the root and
//! replaying the log must give the same file system as before the crash:
//! names, types, modes, owners, sizes, times, link counts, xattrs, content.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use metadata::segment::load_store_with_log;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::handlers::{self as h, CreateResult, EmptyResult, EntryResult, OpenResult, WriteResult};
use slicefs_cli::oplog;
use slicefs_traits::metadata::MetadataStore;
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
    let (root, snaps, ops) = load_store_with_log(&store.join("segments")).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let mut meta = DictMetadataStore::load_from_root(io.clone(), &root.unwrap()).unwrap();
    meta.set_snapshots(snaps);
    let wal = create_wal(WalConfig::PerOp, store, 99).unwrap();
    meta.set_wal(wal);
    let fs = SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()));
    oplog::replay(&fs, &ops).unwrap();
    fs
}

/// Path -> description of every entry under the root.
fn dump(fs: &SliceFsFilesystem) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut todo = vec![(1u64, String::new())];
    while let Some((dir, path)) = todo.pop() {
        for e in fs.meta().list_directory(dir).unwrap() {
            if e.name == "." || e.name == ".." {
                continue;
            }
            let p = format!("{path}/{}", e.name);
            let m = fs.meta().get_inode(e.ino).unwrap();
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
