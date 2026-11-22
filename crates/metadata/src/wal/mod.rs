//! Pluggable WAL (Write-Ahead Log) strategy trait and implementations.
//!
//! The `WalStrategy` trait is the abstraction through which all Dictionary mutations flow.
//! Four implementations cover the full durability/throughput trade-off spectrum:
//!
//! | Strategy     | Survives a crash of the daemon         | Survives a power loss      |
//! |--------------|----------------------------------------|----------------------------|
//! | PerOp        | Each completed request                 | After fsync or commit      |
//! | FlushOnFsync | All but at most the last 4 MiB of records | After fsync or commit   |
//! | Periodic     | As FlushOnFsync                        | After fsync, commit, timer |
//! | NoWal        | None (testing only)                    | None                       |
//!
//! PerOp writes each operation record into the segment file at once (one
//! `write()`, no disk sync). FlushOnFsync and Periodic hold records in RAM,
//! at most `BUFFER_CAP_BYTES`; past it they write them with no sync. After a
//! crash with no fsync, these two modes can bring back a file as a correct
//! first part of its data (#98).

pub mod no_wal;
pub mod per_op;
pub mod flush_on_fsync;
pub mod periodic;
mod buffered;

pub use no_wal::NoWal;
pub use per_op::PerOpWal;
pub use flush_on_fsync::FlushOnFsyncWal;
pub use periodic::PeriodicWal;

use std::path::Path;
use slicefs_traits::key::ObjectKey;
use thiserror::Error;

/// Flush-on-fsync and periodic write their held records to the segment (no
/// fsync) when the records pass this size, so the RAM of the write log stays
/// at most this size (#98).
pub const BUFFER_CAP_BYTES: usize = 4 * 1024 * 1024;

/// A logical WAL entry representing a metadata state change.
#[derive(Debug, Clone)]
pub enum WalEntry {
    /// Update the filesystem root digest.
    RootUpdate { root: ObjectKey },
    /// Persist a snapshot record with version, root, timestamp, and optional name.
    Snapshot { version: u64, root: ObjectKey, created_at: u64, name: Option<String> },
    /// A file system request since the last root (#20 write log). The bytes
    /// are encoded by the file system; the store only keeps them in order.
    /// Not synced on its own: `flush_and_sync` (fsync, commit) makes it
    /// survive a power loss. PerOp writes it into the file at once.
    Op { bytes: Vec<u8> },
}

/// Errors returned by WAL operations.
#[derive(Debug, Error)]
pub enum WalError {
    #[error("segment I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Selects which WAL strategy to use.
#[derive(Debug, Clone)]
pub enum WalConfig {
    /// Sync each root and snapshot record; write each operation record into
    /// the file at once with no sync.
    PerOp,
    /// Hold up to `BUFFER_CAP_BYTES` of records in RAM; write and sync them
    /// on an explicit `flush_and_sync`.
    FlushOnFsync,
    /// As `FlushOnFsync`, and a timer also syncs every `interval_secs`
    /// (0: no timer).
    Periodic { interval_secs: u64 },
    /// No-op WAL (testing / in-memory only).
    NoWal,
}

/// Log sequence number: the place of a record in the write log. Given out
/// by [`WalStrategy::append`] in the order the records are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lsn(pub u64);

/// Abstraction over durability strategies for Dictionary mutations.
///
/// All implementations must be `Send + Sync` to allow use behind `Arc`.
///
/// A request logs in two steps (LOCK-04): `append` under the locks of its
/// apply (a memory copy and an LSN, no I/O), then `write_out` after it
/// released them and before it replies.
pub trait WalStrategy: Send + Sync {
    /// Put the record in the in-memory queue of the log and return its LSN.
    /// Memory only: takes one short lock and never waits for the segment
    /// writer or the disk.
    fn append(&self, entry: &WalEntry) -> Result<Lsn, WalError>;
    /// Make the records up to `upto` reach the place the strategy promises
    /// for a completed request (PerOp: written into the segment file and
    /// flushed to the kernel, in LSN order; buffered: held in RAM up to the
    /// cap). Idempotent: an LSN already written out returns at once. An
    /// error means a record up to `upto` may be lost.
    fn write_out(&self, upto: Lsn) -> Result<(), WalError>;
    /// Record a single mutation: `append`, then `write_out`.
    fn log_mutation(&self, entry: &WalEntry) -> Result<(), WalError> {
        let lsn = self.append(entry)?;
        self.write_out(lsn)
    }
    /// Flush all buffered mutations to disk and call `sync_all`. Holds no
    /// lock of the log across a disk sync.
    fn flush_and_sync(&self) -> Result<(), WalError>;
    /// Flush remaining mutations and release all resources.
    fn shutdown(&self) -> Result<(), WalError>;
    /// After a commit: continue in a new segment that starts with `seed`
    /// (the new root and all snapshot records), and delete the older
    /// segments, whose records are now in the root or dead (#20). Keeps the
    /// write log at one segment. Default: nothing.
    fn rotate(&self, _seed: &[WalEntry]) -> Result<(), WalError> {
        Ok(())
    }
    /// Start a new generation of the log (#108), for a commit whose root is
    /// written later: write the records so far into the current segment
    /// (no disk sync) and go on in a new segment, two ids on, that starts
    /// with a `Cut` record. The id between is kept for the root segment
    /// (`install_root`), so the root sorts after every record of the cut and
    /// before every record after it. Fast: no disk sync. `None`: no log.
    fn cut(&self) -> Result<Option<WalCut>, WalError> {
        Ok(None)
    }
    /// After `cut`, when every object the root refers to is durable: sync
    /// the records logged since the cut (they hold the relog of the open
    /// files), write the root segment with `seed` (the root and the snapshot
    /// records), sync it and the directory, then delete the segments before
    /// it. Holds no lock of the log across a disk sync, so requests can log
    /// meanwhile.
    fn install_root(&self, _cut: &WalCut, _seed: &[WalEntry]) -> Result<(), WalError> {
        Ok(())
    }
}

/// Test hook: park a thread at a named point of a WAL path until the test
/// releases it (at most 5 s). Fires once per `arm`.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Parker {
    #[allow(clippy::type_complexity)]
    armed: std::sync::Mutex<Option<(&'static str, std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

#[cfg(test)]
impl Parker {
    /// Arm point `at`. Returns (parked, release): `parked` gets a message
    /// when a thread reaches the point; sending on `release` lets it go.
    pub(crate) fn arm(&self, at: &'static str) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (parked_tx, parked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *self.armed.lock().unwrap() = Some((at, parked_tx, release_rx));
        (parked_rx, release_tx)
    }

    /// Park here if point `at` is armed.
    pub(crate) fn point(&self, at: &'static str) {
        let taken = {
            let mut g = self.armed.lock().unwrap();
            if matches!(&*g, Some((p, _, _)) if *p == at) { g.take() } else { None }
        };
        if let Some((_, parked, release)) = taken {
            let _ = parked.send(());
            let _ = release.recv_timeout(std::time::Duration::from_secs(5));
        }
    }
}

/// How long a write-out, fsync, cut, rotate or install waits for another
/// thread's record write before it fails with an I/O error of kind
/// `TimedOut` (nothing is lost then: the log is not marked failed).
pub(crate) const WRITE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Wait for the record write of `wait` to end, at most until `deadline`.
pub(crate) fn wait_for(wait: &chunk_store::pack_io::SyncWait, deadline: std::time::Instant) -> Result<(), WalError> {
    let left = deadline.saturating_duration_since(std::time::Instant::now());
    if wait.wait_timeout(left) {
        return Ok(());
    }
    Err(WalError::Io(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "a write-log record write did not end in time",
    )))
}

/// A generation switch of the write log (see [`WalStrategy::cut`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalCut {
    /// Id kept for the root segment of this commit.
    pub root_id: u64,
    /// A write or sync of the segment before the cut failed: records before
    /// the cut may be lost, and fsync must fail until this commit's root.
    pub old_failed: bool,
}

/// Segments that a cut left behind, not yet synced (see [`WalStrategy::cut`]).
/// An fsync after the cut syncs them first, in order, and the directory with
/// the new segment's entry: the log after the cut is good only on top of
/// every record before it.
#[derive(Default)]
pub(crate) struct Behind {
    old: Vec<crate::segment::SegmentWriter>,
    dir: Option<std::path::PathBuf>,
    /// Number of cuts so far (never reset): a settle clears `dir` only if
    /// no cut came while it synced.
    cuts: u64,
}

impl Behind {
    /// Sync the segments left behind and the directory, then forget them.
    /// The disk syncs run on second handles of the files with the `behind`
    /// lock released (`between` runs at that point, for tests). The writers
    /// stay in place until their sync succeeded, so a second fsync meanwhile
    /// syncs them too, and a failed one is synced again next time.
    pub(crate) fn settle(behind: &chunk_store::sync::Mutex<Behind>, between: impl FnOnce()) -> std::io::Result<()> {
        let (files, ids, dir, cuts) = {
            let mut b = behind.lock();
            let mut files = Vec::new();
            for w in b.old.iter_mut() {
                if let Some((file, _)) = w.begin_sync()? {
                    files.push(file);
                }
            }
            let ids: Vec<u64> = b.old.iter().map(|w| w.id()).collect();
            (files, ids, b.dir.clone(), b.cuts)
        };
        between();
        for file in &files {
            file.sync_all()?;
        }
        if let Some(dir) = &dir {
            std::fs::File::open(dir)?.sync_all()?;
        }
        let mut b = behind.lock();
        b.old.retain(|w| !ids.contains(&w.id()));
        if b.cuts == cuts {
            b.dir = None;
        }
        Ok(())
    }

    /// Forget the segments left behind (a root or a rotate made them dead).
    pub(crate) fn reset(&mut self) {
        self.old.clear();
        self.dir = None;
    }
}

/// Sync the current segment with no lock held across the disk sync: under
/// `writer`, run `prepare` (write the held records out) and take a second
/// handle of the file; release `writer`; `sync_all` on the handle (`between`
/// runs just before, for tests); then take `writer` again to note the sync.
pub(crate) fn sync_current(
    writer: &chunk_store::sync::Mutex<crate::segment::SegmentWriter>,
    prepare: impl FnOnce(&mut crate::segment::SegmentWriter) -> Result<(), WalError>,
    between: impl FnOnce(),
) -> Result<(), WalError> {
    let (file, mark) = {
        let mut w = writer.lock();
        prepare(&mut w)?;
        match w.begin_sync()? {
            Some(sync) => sync,
            None => return Ok(()),
        }
    };
    between();
    file.sync_all()?;
    writer.lock().end_sync(mark);
    Ok(())
}

/// The next segment of a cut: its writer and its location.
pub(crate) type NextSegment = (crate::segment::SegmentWriter, (std::path::PathBuf, u64));

/// Create the segment that a cut of segment `loc` goes on in, two ids on
/// (the id between is kept for the root segment), with its header only. A
/// strategy calls it before it takes `writer`, from a copy of `loc`: `loc`
/// changes only under `writer`, in a cut or a rotate, and those never run at
/// the same time (the exclusive commit gate, or offline).
pub(crate) fn open_next_segment(loc: &(std::path::PathBuf, u64)) -> Result<NextSegment, WalError> {
    let dir = loc.0.parent().ok_or_else(|| std::io::Error::other("segment has no directory"))?;
    let new_id = loc.1 + 2;
    let new_path = dir.join(format!("segment-{:06}.seg", new_id));
    let writer = crate::segment::SegmentWriter::new(&new_path, new_id)?;
    Ok((writer, (new_path, new_id)))
}

/// The swap of [`WalStrategy::cut`], under `writer` (segment `loc`), with
/// no record write in flight and the caller's records before the cut
/// written out or dropped: move the old writer to `behind`, install `next`
/// and its location, and reserve the first record of the new segment for
/// the Cut record. The caller writes it with no lock
/// ([`write_cut_record`]); a write-out after the cut waits for that write by
/// the one-write rule, so the Cut record stays first. Creates no file. If
/// `loc` moved since `next` was made, `next` is removed and nothing changes.
pub(crate) fn cut_segment(
    writer: &mut crate::segment::SegmentWriter,
    next: NextSegment,
    loc: &mut (std::path::PathBuf, u64),
    behind: &chunk_store::sync::Mutex<Behind>,
) -> Result<(WalCut, crate::segment::writer::WriteSlot, crate::segment::SegmentEntry), WalError> {
    let (mut new_writer, new_loc) = next;
    if new_loc.1 != loc.1 + 2 {
        drop(new_writer);
        let _ = std::fs::remove_file(&new_loc.0);
        return Err(WalError::Io(std::io::Error::other("the write log moved to another segment during a cut")));
    }
    // The old segment is not needed once the root is logged: a failure here
    // only makes fsync fail until then (and the Cut record below no longer
    // matches the segment, so replay stops before the new one).
    let old_failed = writer.flush().is_err();
    let dir = loc.0.parent().ok_or_else(|| std::io::Error::other("segment has no directory"))?.to_path_buf();
    let (old_id, records) = (loc.1, writer.records());
    let slot = new_writer.begin_write(1)?;
    let old = std::mem::replace(writer, new_writer);
    let mut b = behind.lock();
    b.old.push(old);
    b.dir = Some(dir);
    b.cuts += 1;
    *loc = new_loc;
    Ok((WalCut { root_id: old_id + 1, old_failed }, slot, crate::segment::SegmentEntry::Cut { prev_id: old_id, records }))
}

/// Write the Cut record of a cut into its slot with no WAL lock held, then
/// publish it under `writer`. On an error `failed` runs under `writer`
/// (the strategy marks itself failed): the new segment is not trusted, and
/// every write-out and fsync fails until the next cut. The cut itself still
/// stands, since the root of its commit holds every record before it.
pub(crate) fn write_cut_record(
    writer: &chunk_store::sync::Mutex<crate::segment::SegmentWriter>,
    slot: crate::segment::writer::WriteSlot,
    record: &crate::segment::SegmentEntry,
    failed: impl FnOnce(),
) {
    let r = slot.write([record]);
    let mut w = writer.lock();
    w.end_write(slot, &r);
    if r.is_err() {
        failed();
    }
}

/// The second half of [`WalStrategy::install_root`], after the records of
/// the current segment are durable: write segment `cut.root_id` with `seed`,
/// sync it and the directory, delete the segments before it, sync the
/// directory again, and forget the segments left behind (they are deleted).
pub(crate) fn write_root_segment(
    current: &Path,
    cut: &WalCut,
    seed: &[WalEntry],
    behind: &chunk_store::sync::Mutex<Behind>,
) -> Result<(), WalError> {
    let dir = current.parent().ok_or_else(|| std::io::Error::other("segment has no directory"))?;
    let path = dir.join(format!("segment-{:06}.seg", cut.root_id));
    let mut writer = crate::segment::SegmentWriter::new(&path, cut.root_id)?;
    for entry in seed {
        writer.write_entry(&per_op::wal_entry_to_segment(entry))?;
    }
    writer.sync()?;
    std::fs::File::open(dir)?.sync_all()?;
    remove_segments_before(dir, cut.root_id)?;
    std::fs::File::open(dir)?.sync_all()?;
    behind.lock().reset();
    Ok(())
}

/// Delete the segment files of `dir` whose id is lower than `id`.
fn remove_segments_before(dir: &Path, id: u64) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)?.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        let old = name.strip_prefix("segment-").and_then(|n| n.strip_suffix(".seg")).and_then(|n| n.parse::<u64>().ok());
        if matches!(old, Some(o) if o < id) {
            std::fs::remove_file(e.path())?;
        }
    }
    Ok(())
}

/// Start segment `id + 1` next to `path` with `seed`, sync it and the
/// directory, then delete the older segment files. The caller holds
/// `writer` across this (offline only: `snapshot switch`), with no record
/// write in flight. Returns the new writer,
/// its path and id. On error before the new segment is synced, nothing is
/// deleted (the current segment stays valid).
pub(crate) fn rotate_segment(
    path: &Path,
    id: u64,
    seed: &[WalEntry],
) -> Result<(crate::segment::SegmentWriter, std::path::PathBuf, u64), WalError> {
    let dir = path.parent().ok_or_else(|| std::io::Error::other("segment has no directory"))?;
    let new_id = id + 1;
    let new_path = dir.join(format!("segment-{:06}.seg", new_id));
    let mut writer = crate::segment::SegmentWriter::new(&new_path, new_id)?;
    for entry in seed {
        writer.write_entry(&per_op::wal_entry_to_segment(entry))?;
    }
    writer.sync()?;
    std::fs::File::open(dir)?.sync_all()?;
    remove_segments_before(dir, new_id)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok((writer, new_path, new_id))
}

/// Factory: create a boxed `WalStrategy` from a `WalConfig`.
///
/// Segment files are written to `<store_path>/segments/segment-{segment_id:06}.seg`.
/// The `segments/` subdirectory must already exist before calling this function.
pub fn create_wal(
    config: WalConfig,
    store_path: &Path,
    segment_id: u64,
) -> Result<Box<dyn WalStrategy>, WalError> {
    let seg_path = store_path
        .join("segments")
        .join(format!("segment-{:06}.seg", segment_id));
    match config {
        WalConfig::PerOp => {
            let wal = PerOpWal::new(&seg_path, segment_id)?;
            Ok(Box::new(wal))
        }
        WalConfig::FlushOnFsync => {
            let wal = FlushOnFsyncWal::new(&seg_path, segment_id)?;
            Ok(Box::new(wal))
        }
        WalConfig::Periodic { interval_secs } => {
            let interval = std::time::Duration::from_secs(interval_secs);
            let wal = PeriodicWal::with_interval(&seg_path, segment_id, interval)?;
            Ok(Box::new(wal))
        }
        WalConfig::NoWal => Ok(Box::new(NoWal)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::{SegmentEntry, SegmentReader};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    fn op(i: u32) -> WalEntry {
        WalEntry::Op { bytes: i.to_le_bytes().to_vec() }
    }

    fn entries(path: &Path) -> Vec<SegmentEntry> {
        SegmentReader::open(path).map(|r| r.collect()).unwrap_or_default()
    }

    fn ops(path: &Path) -> Vec<Vec<u8>> {
        entries(path)
            .into_iter()
            .filter_map(|e| match e { SegmentEntry::Op { bytes } => Some(bytes), _ => None })
            .collect()
    }

    fn seg(dir: &Path, id: u64) -> std::path::PathBuf {
        dir.join(format!("segment-{id:06}.seg"))
    }

    const KINDS: [&str; 3] = ["per_op", "flush_on_fsync", "periodic"];

    /// A strategy that writes a segment, on segment 1 of `dir`.
    fn make(kind: &str, dir: &Path) -> Arc<dyn WalStrategy> {
        match kind {
            "per_op" => Arc::new(PerOpWal::new(&seg(dir, 1), 1).unwrap()),
            "flush_on_fsync" => Arc::new(FlushOnFsyncWal::new(&seg(dir, 1), 1).unwrap()),
            _ => Arc::new(PeriodicWal::with_interval(&seg(dir, 1), 1, Duration::from_millis(20)).unwrap()),
        }
    }

    /// Records appended and not written out are in the segment before the
    /// cut, in LSN order; a record after the cut is not.
    #[test]
    fn cut_keeps_unwritten_appends_in_lsn_order() {
        for kind in ["per_op", "flush_on_fsync"] {
            let dir = tempfile::tempdir().unwrap();
            let wal = make(kind, dir.path());
            for i in 0..5 {
                wal.append(&op(i)).unwrap();
            }
            let cut = wal.cut().unwrap().unwrap();
            assert_eq!(cut.root_id, 2);
            let after = wal.append(&op(99)).unwrap();
            wal.write_out(after).unwrap();
            wal.flush_and_sync().unwrap();
            let want: Vec<Vec<u8>> = (0..5).map(|i: u32| i.to_le_bytes().to_vec()).collect();
            assert_eq!(ops(&seg(dir.path(), 1)), want, "{kind}: every record before the cut, in order");
        }
    }

    /// Records appended before a rotate never follow the seed of the new
    /// segment (the new root holds them; replay would apply them twice).
    #[test]
    fn rotate_after_unwritten_appends_starts_with_the_seed_only() {
        for kind in ["per_op", "flush_on_fsync"] {
            let dir = tempfile::tempdir().unwrap();
            let wal = make(kind, dir.path());
            for i in 0..3 {
                wal.append(&op(i)).unwrap();
            }
            let root = ObjectKey::example(3);
            wal.rotate(&[WalEntry::RootUpdate { root }]).unwrap();
            let after = wal.append(&op(7)).unwrap();
            wal.write_out(after).unwrap();
            wal.flush_and_sync().unwrap();
            let got = entries(&seg(dir.path(), 2));
            assert!(
                matches!(got.as_slice(), [SegmentEntry::RootUpdate { root: r }, SegmentEntry::Op { bytes }]
                    if *r == root && bytes == &7u32.to_le_bytes().to_vec()),
                "{kind}: the new segment must hold the seed and the record after the rotate, got {} entries",
                got.len()
            );
        }
    }

    /// One lock order in every path (T-19-13): append + write_out, fsync
    /// (and the periodic timer), and cut/install_root/rotate run at once on
    /// one strategy. A watchdog fails the test if they do not end in 5 s.
    #[test]
    fn no_deadlock_between_append_cut_and_sync() {
        for kind in KINDS {
            let dir = tempfile::tempdir().unwrap();
            let wal = make(kind, dir.path());
            let stop = Arc::new(AtomicBool::new(false));
            let (done, finished) = mpsc::channel::<&'static str>();
            let mut threads = Vec::new();
            let spawn = |name: &'static str, f: Box<dyn Fn(&dyn WalStrategy, u32) + Send>| {
                let (wal, stop, done) = (Arc::clone(&wal), Arc::clone(&stop), done.clone());
                std::thread::spawn(move || {
                    let mut n = 0;
                    while !stop.load(Ordering::Relaxed) {
                        f(&*wal, n);
                        n += 1;
                    }
                    let _ = done.send(name);
                })
            };
            threads.push(spawn("append", Box::new(|w, n| {
                let lsn = w.append(&WalEntry::Op { bytes: vec![n as u8; 16 * 1024] }).unwrap();
                w.write_out(lsn).unwrap();
            })));
            threads.push(spawn("sync", Box::new(|w, _| w.flush_and_sync().unwrap())));
            threads.push(spawn("generation", Box::new(|w, n| {
                let seed = [WalEntry::RootUpdate { root: ObjectKey::example(n) }];
                if n % 2 == 0 {
                    let cut = w.cut().unwrap().unwrap();
                    w.install_root(&cut, &seed).unwrap();
                } else {
                    w.rotate(&seed).unwrap();
                }
            })));
            std::thread::sleep(Duration::from_millis(400));
            stop.store(true, Ordering::Relaxed);
            for _ in 0..threads.len() {
                if finished.recv_timeout(Duration::from_secs(5)).is_err() {
                    panic!("{kind}: a WAL thread did not finish within 5 s (deadlock)");
                }
            }
            for t in threads {
                t.join().unwrap();
            }
        }
    }
}
