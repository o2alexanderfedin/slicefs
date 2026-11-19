//! Write log of file system requests (#20, part 1).
//!
//! Each mutating request that succeeds is appended to the segment log as an
//! [`Record`]: the request's arguments, its result (the inode number a create
//! gave), and the time it used. `fsync` syncs the log; a commit (RootUpdate)
//! cuts it. After a crash, the mount loads the last root and replays the
//! records after it through the same handlers ([`replay`]).
//!
//! Determinism: while a request runs, [`Clock`] freezes "now" for the
//! thread, so all times of one request are equal; replay freezes the logged
//! time. Inode numbers follow from `next_ino` in the root plus the same
//! sequence of requests; replay checks them.

use std::cell::Cell;
use std::collections::HashMap;

use crate::filesystem::SliceFsFilesystem;
use crate::handlers;

/// One logged request.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Create { parent: u64, name: String, mode: u32, umask: u32, uid: u32, gid: u32, flags: i32, ino: u64 },
    Mkdir { parent: u64, name: String, mode: u32, umask: u32, uid: u32, gid: u32, ino: u64 },
    Mknod { parent: u64, name: String, mode: u32, umask: u32, rdev: u32, uid: u32, gid: u32, ino: u64 },
    Symlink { parent: u64, name: String, target: String, uid: u32, gid: u32, ino: u64 },
    Link { ino: u64, newparent: u64, newname: String },
    Unlink { parent: u64, name: String },
    Rmdir { parent: u64, name: String },
    Rename { parent: u64, name: String, newparent: u64, newname: String, flags: u32 },
    Setattr {
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<(i64, u32)>,
        mtime: Option<(i64, u32)>,
    },
    Setxattr { ino: u64, name: String, value: Vec<u8> },
    Removexattr { ino: u64, name: String },
    Write { ino: u64, offset: u64, data: Vec<u8> },
    /// `open` with `O_TRUNC` on a writable handle.
    Truncate { ino: u64 },
    /// Release of a write handle: it finalizes the content and sets times.
    Release { ino: u64 },
    /// `flush` or `fsync` of a write handle: writes the manifest, sets times.
    Flush { ino: u64 },
}

/// A logged request and the time it used.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub time: (i64, u32),
    pub op: Op,
}

// ─── clock ──────────────────────────────────────────────────────────────────

thread_local! {
    static FROZEN: Cell<Option<(i64, u32)>> = const { Cell::new(None) };
}

/// While alive, `frozen_now()` returns one fixed time on this thread.
pub struct Clock {
    pub time: (i64, u32),
    prev: Option<(i64, u32)>,
}

impl Clock {
    /// Freeze the clock at `time` until the value is dropped.
    pub fn freeze_at(time: (i64, u32)) -> Clock {
        let prev = FROZEN.with(|f| f.replace(Some(time)));
        Clock { time, prev }
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        FROZEN.with(|f| f.set(self.prev));
    }
}

/// The frozen time of this thread, if a request froze it.
pub fn frozen_now() -> Option<(i64, u32)> {
    FROZEN.with(|f| f.get())
}

// ─── encoding ───────────────────────────────────────────────────────────────

struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) { self.0.push(v); }
    fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn i32(&mut self, v: i32) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn u64(&mut self, v: u64) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn i64(&mut self, v: i64) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn bytes(&mut self, v: &[u8]) { self.u32(v.len() as u32); self.0.extend_from_slice(v); }
    fn str(&mut self, v: &str) { self.bytes(v.as_bytes()); }
    fn time(&mut self, v: (i64, u32)) { self.i64(v.0); self.u32(v.1); }
    fn opt_u32(&mut self, v: Option<u32>) { match v { Some(x) => { self.u8(1); self.u32(x) } None => self.u8(0) } }
    fn opt_u64(&mut self, v: Option<u64>) { match v { Some(x) => { self.u8(1); self.u64(x) } None => self.u8(0) } }
    fn opt_time(&mut self, v: Option<(i64, u32)>) { match v { Some(x) => { self.u8(1); self.time(x) } None => self.u8(0) } }
}

struct Dec<'a>(&'a [u8]);

impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n { return None; }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> { Some(self.take(1)?[0]) }
    fn u32(&mut self) -> Option<u32> { Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?)) }
    fn i32(&mut self) -> Option<i32> { Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?)) }
    fn u64(&mut self) -> Option<u64> { Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?)) }
    fn i64(&mut self) -> Option<i64> { Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?)) }
    fn bytes(&mut self) -> Option<Vec<u8>> { let n = self.u32()? as usize; Some(self.take(n)?.to_vec()) }
    fn str(&mut self) -> Option<String> { String::from_utf8(self.bytes()?).ok() }
    fn time(&mut self) -> Option<(i64, u32)> { Some((self.i64()?, self.u32()?)) }
    fn opt<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<Option<T>> {
        match self.u8()? { 0 => Some(None), 1 => Some(Some(f(self)?)), _ => None }
    }
}

impl Record {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc(Vec::new());
        e.time(self.time);
        match &self.op {
            Op::Create { parent, name, mode, umask, uid, gid, flags, ino } => {
                e.u8(1); e.u64(*parent); e.str(name); e.u32(*mode); e.u32(*umask); e.u32(*uid); e.u32(*gid); e.i32(*flags); e.u64(*ino);
            }
            Op::Mkdir { parent, name, mode, umask, uid, gid, ino } => {
                e.u8(2); e.u64(*parent); e.str(name); e.u32(*mode); e.u32(*umask); e.u32(*uid); e.u32(*gid); e.u64(*ino);
            }
            Op::Mknod { parent, name, mode, umask, rdev, uid, gid, ino } => {
                e.u8(3); e.u64(*parent); e.str(name); e.u32(*mode); e.u32(*umask); e.u32(*rdev); e.u32(*uid); e.u32(*gid); e.u64(*ino);
            }
            Op::Symlink { parent, name, target, uid, gid, ino } => {
                e.u8(4); e.u64(*parent); e.str(name); e.str(target); e.u32(*uid); e.u32(*gid); e.u64(*ino);
            }
            Op::Link { ino, newparent, newname } => { e.u8(5); e.u64(*ino); e.u64(*newparent); e.str(newname); }
            Op::Unlink { parent, name } => { e.u8(6); e.u64(*parent); e.str(name); }
            Op::Rmdir { parent, name } => { e.u8(7); e.u64(*parent); e.str(name); }
            Op::Rename { parent, name, newparent, newname, flags } => {
                e.u8(8); e.u64(*parent); e.str(name); e.u64(*newparent); e.str(newname); e.u32(*flags);
            }
            Op::Setattr { ino, mode, uid, gid, size, atime, mtime } => {
                e.u8(9); e.u64(*ino); e.opt_u32(*mode); e.opt_u32(*uid); e.opt_u32(*gid); e.opt_u64(*size); e.opt_time(*atime); e.opt_time(*mtime);
            }
            Op::Setxattr { ino, name, value } => { e.u8(10); e.u64(*ino); e.str(name); e.bytes(value); }
            Op::Removexattr { ino, name } => { e.u8(11); e.u64(*ino); e.str(name); }
            Op::Write { ino, offset, data } => { e.u8(12); e.u64(*ino); e.u64(*offset); e.bytes(data); }
            Op::Truncate { ino } => { e.u8(13); e.u64(*ino); }
            Op::Release { ino } => { e.u8(14); e.u64(*ino); }
            Op::Flush { ino } => { e.u8(15); e.u64(*ino); }
        }
        e.0
    }

    pub fn decode(bytes: &[u8]) -> Option<Record> {
        let mut d = Dec(bytes);
        let time = d.time()?;
        let op = match d.u8()? {
            1 => Op::Create { parent: d.u64()?, name: d.str()?, mode: d.u32()?, umask: d.u32()?, uid: d.u32()?, gid: d.u32()?, flags: d.i32()?, ino: d.u64()? },
            2 => Op::Mkdir { parent: d.u64()?, name: d.str()?, mode: d.u32()?, umask: d.u32()?, uid: d.u32()?, gid: d.u32()?, ino: d.u64()? },
            3 => Op::Mknod { parent: d.u64()?, name: d.str()?, mode: d.u32()?, umask: d.u32()?, rdev: d.u32()?, uid: d.u32()?, gid: d.u32()?, ino: d.u64()? },
            4 => Op::Symlink { parent: d.u64()?, name: d.str()?, target: d.str()?, uid: d.u32()?, gid: d.u32()?, ino: d.u64()? },
            5 => Op::Link { ino: d.u64()?, newparent: d.u64()?, newname: d.str()? },
            6 => Op::Unlink { parent: d.u64()?, name: d.str()? },
            7 => Op::Rmdir { parent: d.u64()?, name: d.str()? },
            8 => Op::Rename { parent: d.u64()?, name: d.str()?, newparent: d.u64()?, newname: d.str()?, flags: d.u32()? },
            9 => Op::Setattr {
                ino: d.u64()?,
                mode: d.opt(|d| d.u32())?,
                uid: d.opt(|d| d.u32())?,
                gid: d.opt(|d| d.u32())?,
                size: d.opt(|d| d.u64())?,
                atime: d.opt(|d| d.time())?,
                mtime: d.opt(|d| d.time())?,
            },
            10 => Op::Setxattr { ino: d.u64()?, name: d.str()?, value: d.bytes()? },
            11 => Op::Removexattr { ino: d.u64()?, name: d.str()? },
            12 => Op::Write { ino: d.u64()?, offset: d.u64()?, data: d.bytes()? },
            13 => Op::Truncate { ino: d.u64()? },
            14 => Op::Release { ino: d.u64()? },
            15 => Op::Flush { ino: d.u64()? },
            _ => return None,
        };
        if !d.0.is_empty() {
            return None;
        }
        Some(Record { time, op })
    }
}

// ─── replay ─────────────────────────────────────────────────────────────────

/// Outcome of a replay.
#[derive(Debug, Default)]
pub struct ReplayStats {
    pub applied: usize,
    /// Writes to an inode that was already reclaimed (unlinked while open,
    /// and the handle that kept it is not in the log).
    pub skipped: usize,
}

/// Replay the write log of the store at `segments_dir` on `fs` (which holds
/// the last committed root): what a mount does before it commits.
///
/// Two passes over the segment files (#106): the first finds where the log
/// starts and ends; the second reads one record at a time and applies it
/// before it reads the next. Memory does not grow with the log.
pub fn replay_log(fs: &SliceFsFilesystem, segments_dir: &std::path::Path) -> Result<ReplayStats, String> {
    let (_, _, log) = metadata::segment::scan_store_with_log(segments_dir)
        .map_err(|e| format!("failed to read the write log: {}", e))?;
    if log.is_empty() {
        return Ok(ReplayStats::default());
    }
    replay_stream(fs, log.records().map(|r| r.map_err(|e| format!("failed to read the write log: {}", e))))
}

/// Replay `records` (encoded) on `fs`, which holds the last committed root.
/// Logging is off while it runs. Every replay handle is released at the end.
/// The caller commits afterwards.
///
/// Errors name the position of the record that did not apply: such a
/// mismatch means the log and the root do not belong together.
pub fn replay(fs: &SliceFsFilesystem, records: &[Vec<u8>]) -> Result<ReplayStats, String> {
    replay_stream(fs, records.iter().map(|r| Ok(r.as_slice())))
}

/// [`replay`] of records that come one at a time; the first error of
/// `records` stops the replay.
pub fn replay_stream<B: AsRef<[u8]>>(
    fs: &SliceFsFilesystem,
    records: impl IntoIterator<Item = Result<B, String>>,
) -> Result<ReplayStats, String> {
    let _quiet = fs.replay_guard();
    let mut stats = ReplayStats::default();
    let mut handles: HashMap<u64, u64> = HashMap::new(); // ino -> fh
    for (i, bytes) in records.into_iter().enumerate() {
        let bytes = bytes?;
        let rec = Record::decode(bytes.as_ref()).ok_or_else(|| format!("write log record {i}: cannot decode"))?;
        drop(bytes); // the decoded record has its own copy
        let _clock = Clock::freeze_at(rec.time);
        let bad = |what: &str| format!("write log record {i} ({what}) did not replay: {:?}", rec.op);
        // A create logged the inode it got: take the same one, skipping any
        // number that a failed request used up without a record.
        if let Op::Create { ino, .. } | Op::Mkdir { ino, .. } | Op::Mknod { ino, .. } | Op::Symlink { ino, .. } = &rec.op {
            fs.meta().advance_next_ino(*ino);
        }
        match &rec.op {
            Op::Create { parent, name, mode, umask, uid, gid, flags, ino } => {
                match handlers::handle_create(fs, *parent, name, *mode, *umask, *uid, *gid, *flags) {
                    handlers::CreateResult::Ok(got, fh, _) if got == *ino => { handles.insert(got, fh); }
                    _ => return Err(bad("create")),
                }
            }
            Op::Mkdir { parent, name, mode, umask, uid, gid, ino } => {
                match handlers::handle_mkdir(fs, *parent, name, *mode, *umask, *uid, *gid) {
                    handlers::EntryResult::Ok(got, _) if got == *ino => {}
                    _ => return Err(bad("mkdir")),
                }
            }
            Op::Mknod { parent, name, mode, umask, rdev, uid, gid, ino } => {
                match handlers::handle_mknod(fs, *parent, name, *mode, *umask, *rdev, *uid, *gid) {
                    handlers::EntryResult::Ok(got, _) if got == *ino => {}
                    _ => return Err(bad("mknod")),
                }
            }
            Op::Symlink { parent, name, target, uid, gid, ino } => {
                match handlers::handle_symlink(fs, *parent, name, target, *uid, *gid) {
                    handlers::EntryResult::Ok(got, _) if got == *ino => {}
                    _ => return Err(bad("symlink")),
                }
            }
            Op::Link { ino, newparent, newname } => {
                if !matches!(handlers::handle_link(fs, *ino, *newparent, newname), handlers::EntryResult::Ok(..)) {
                    return Err(bad("link"));
                }
            }
            Op::Unlink { parent, name } => {
                if !matches!(handlers::handle_unlink(fs, *parent, name), handlers::EmptyResult::Ok) {
                    return Err(bad("unlink"));
                }
            }
            Op::Rmdir { parent, name } => {
                if !matches!(handlers::handle_rmdir(fs, *parent, name), handlers::EmptyResult::Ok) {
                    return Err(bad("rmdir"));
                }
            }
            Op::Rename { parent, name, newparent, newname, flags } => {
                if !matches!(handlers::handle_rename(fs, *parent, name, *newparent, newname, *flags), handlers::EmptyResult::Ok) {
                    return Err(bad("rename"));
                }
            }
            Op::Setattr { ino, mode, uid, gid, size, atime, mtime } => {
                let fh = handles.get(ino).copied();
                if !matches!(
                    handlers::handle_setattr(fs, *ino, *mode, *uid, *gid, *size, fh, *atime, *mtime),
                    handlers::AttrResult::Ok(_)
                ) {
                    return Err(bad("setattr"));
                }
            }
            Op::Setxattr { ino, name, value } => {
                if !matches!(handlers::handle_setxattr(fs, *ino, name, value), handlers::EmptyResult::Ok) {
                    return Err(bad("setxattr"));
                }
            }
            Op::Removexattr { ino, name } => {
                if !matches!(handlers::handle_removexattr(fs, *ino, name), handlers::EmptyResult::Ok) {
                    return Err(bad("removexattr"));
                }
            }
            Op::Write { ino, offset, data } => {
                let fh = match handles.get(ino) {
                    Some(&fh) => fh,
                    None => match handlers::handle_open(fs, *ino, libc::O_RDWR) {
                        handlers::OpenResult::Ok(fh, _) => { handles.insert(*ino, fh); fh }
                        _ => { stats.skipped += 1; continue; }
                    },
                };
                if !matches!(handlers::handle_write(fs, fh, *offset, data), handlers::WriteResult::Ok(_)) {
                    return Err(bad("write"));
                }
            }
            Op::Flush { ino } => {
                if let Some(&fh) = handles.get(ino) {
                    if !matches!(handlers::handle_flush(fs, *ino, fh), handlers::EmptyResult::Ok) {
                        return Err(bad("flush"));
                    }
                }
            }
            Op::Release { ino } => {
                if let Some(fh) = handles.remove(ino) {
                    if !matches!(handlers::handle_release(fs, *ino, fh), handlers::EmptyResult::Ok) {
                        return Err(bad("release"));
                    }
                }
            }
            Op::Truncate { ino } => {
                if let Some(fh) = handles.remove(ino) {
                    let _ = handlers::handle_release(fs, *ino, fh);
                }
                match handlers::handle_open(fs, *ino, libc::O_RDWR | libc::O_TRUNC) {
                    handlers::OpenResult::Ok(fh, _) => { handles.insert(*ino, fh); }
                    _ => { stats.skipped += 1; continue; }
                }
            }
        }
        stats.applied += 1;
    }
    for (ino, fh) in handles {
        let _ = handlers::handle_release(fs, ino, fh);
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_op_round_trips_through_the_encoding() {
        let ops = vec![
            Op::Create { parent: 1, name: "a".into(), mode: 0o100644, umask: 0o22, uid: 501, gid: 20, flags: 0x201, ino: 7 },
            Op::Mkdir { parent: 1, name: "d".into(), mode: 0o40755, umask: 0, uid: 1, gid: 2, ino: 8 },
            Op::Mknod { parent: 8, name: "f".into(), mode: 0o10644, umask: 0, rdev: 0, uid: 1, gid: 2, ino: 9 },
            Op::Symlink { parent: 1, name: "l".into(), target: "a".into(), uid: 1, gid: 2, ino: 10 },
            Op::Link { ino: 7, newparent: 8, newname: "b".into() },
            Op::Unlink { parent: 8, name: "b".into() },
            Op::Rmdir { parent: 1, name: "d".into() },
            Op::Rename { parent: 1, name: "a".into(), newparent: 1, newname: "c".into(), flags: 0 },
            Op::Setattr { ino: 7, mode: Some(0o600), uid: None, gid: Some(3), size: Some(0), atime: None, mtime: Some((5, 6)) },
            Op::Setxattr { ino: 7, name: "user.k".into(), value: vec![1, 2] },
            Op::Removexattr { ino: 7, name: "user.k".into() },
            Op::Write { ino: 7, offset: 4096, data: vec![9; 100] },
            Op::Truncate { ino: 7 },
            Op::Release { ino: 7 },
            Op::Flush { ino: 7 },
        ];
        for op in ops {
            let rec = Record { time: (1_700_000_000, 123), op };
            assert_eq!(Record::decode(&rec.encode()), Some(rec.clone()));
            let mut cut = rec.encode();
            cut.pop();
            assert_eq!(Record::decode(&cut), None, "a short record must not decode");
        }
    }

    #[test]
    fn the_clock_freeze_nests_and_ends() {
        assert_eq!(frozen_now(), None);
        {
            let _a = Clock::freeze_at((1, 2));
            assert_eq!(frozen_now(), Some((1, 2)));
            {
                let _b = Clock::freeze_at((3, 4));
                assert_eq!(frozen_now(), Some((3, 4)));
            }
            assert_eq!(frozen_now(), Some((1, 2)));
        }
        assert_eq!(frozen_now(), None);
    }
}
