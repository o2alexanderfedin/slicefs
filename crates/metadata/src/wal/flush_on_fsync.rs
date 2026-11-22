//! Flush-on-fsync WAL strategy — holds records in RAM; writes and syncs them
//! on an explicit flush (fsync, fsyncdir, commit). Past `BUFFER_CAP_BYTES`,
//! the held records go to the segment with no sync (#98).

use std::io;
use std::path::Path;

use super::buffered::BufferedLog;
use super::{WalEntry, WalError, WalStrategy};

/// WAL that holds mutations in memory until `flush_and_sync` is called, or
/// until they pass `BUFFER_CAP_BYTES` (then they are written with no sync).
///
/// Suitable when the caller guarantees periodic explicit flushes (e.g., on fsync).
pub struct FlushOnFsyncWal {
    log: BufferedLog,
}

impl FlushOnFsyncWal {
    /// Create a new `FlushOnFsyncWal` writing to `path` with the given `segment_id`.
    pub fn new(path: &Path, segment_id: u64) -> io::Result<Self> {
        Ok(FlushOnFsyncWal { log: BufferedLog::new(path, segment_id)? })
    }

    /// Shutdown without flushing buffered entries (for testing: verify entries stay buffered).
    pub fn shutdown_without_flush(self) {
        // Drop without flushing — buffered entries are lost
        drop(self);
    }
}

impl WalStrategy for FlushOnFsyncWal {
    fn rotate(&self, seed: &[WalEntry]) -> Result<(), WalError> {
        self.log.rotate(seed)
    }
    fn cut(&self) -> Result<Option<super::WalCut>, WalError> {
        self.log.cut()
    }
    fn install_root(&self, cut: &super::WalCut, seed: &[WalEntry]) -> Result<(), WalError> {
        self.log.install_root(cut, seed)
    }
    fn append(&self, entry: &WalEntry) -> Result<super::Lsn, WalError> {
        self.log.append(entry)
    }
    fn write_out(&self, upto: super::Lsn) -> Result<(), WalError> {
        self.log.write_out(upto)
    }

    fn flush_and_sync(&self) -> Result<(), WalError> {
        self.log.flush_and_sync()
    }

    fn shutdown(&self) -> Result<(), WalError> {
        self.flush_and_sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_stays_at_most_the_cap_with_no_fsync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.seg");
        let wal = FlushOnFsyncWal::new(&path, 1).unwrap();
        super::super::buffered::assert_cap_holds(&wal, &wal.log, &path);
    }

    /// LOCK-04: append only holds the record in RAM; write_out writes the
    /// held records when they pass the cap, and not before.
    #[test]
    fn append_holds_records_until_a_write_out_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.seg");
        let wal = FlushOnFsyncWal::new(&path, 1).unwrap();
        let record = 64 * 1024;
        let small = wal.append(&WalEntry::Op { bytes: vec![1; 10] }).unwrap();
        wal.write_out(small).unwrap();
        assert!(wal.log.buffered_bytes() > 0, "under the cap, write_out writes nothing");
        let mut last = small;
        for i in 0..(super::super::BUFFER_CAP_BYTES / record + 2) {
            last = wal.append(&WalEntry::Op { bytes: vec![i as u8; record] }).unwrap();
        }
        assert!(
            wal.log.buffered_bytes() > super::super::BUFFER_CAP_BYTES,
            "append must only hold the records (memory only), held {}",
            wal.log.buffered_bytes()
        );
        let on_disk = |p: &Path| crate::segment::SegmentReader::open(p).map(|r| r.count()).unwrap_or(0);
        assert_eq!(on_disk(&path), 0, "append must not write the segment");
        wal.write_out(last).unwrap();
        assert_eq!(wal.log.buffered_bytes(), 0, "past the cap, write_out writes every held record");
        assert_eq!(on_disk(&path), super::super::BUFFER_CAP_BYTES / record + 3);
    }

    /// A failed write-out at the cap makes every later fsync fail until the
    /// next rotate (commit), even though no record is held any more.
    #[test]
    fn failed_cap_write_out_fails_fsync_until_rotate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("segment-000001.seg");
        let wal = FlushOnFsyncWal::new(&path, 1).unwrap();
        wal.log.fail_next_io();
        let record = 64 * 1024;
        let mut failed = false;
        for i in 0..(super::super::BUFFER_CAP_BYTES / record + 2) {
            failed |= wal.log_mutation(&WalEntry::Op { bytes: vec![i as u8; record] }).is_err();
        }
        assert!(failed, "the injected fault must fail the write-out at the cap");
        assert!(wal.flush_and_sync().is_err(), "fsync after a failed write-out must fail");
        assert!(wal.flush_and_sync().is_err(), "the failure must stay until rotate");
        let root = slicefs_traits::key::ObjectKey::example(1);
        wal.rotate(&[WalEntry::RootUpdate { root }]).unwrap();
        wal.log_mutation(&WalEntry::Op { bytes: vec![1; 10] }).unwrap();
        assert!(wal.flush_and_sync().is_ok(), "after rotate (commit), fsync works again");
    }

    // #108: a commit cuts the log instead of rotating it. After a failed
    // write-out, the commit must still work (it is the way back), fsync must
    // fail until its root is logged and work after it, and the log after the
    // root must replay.
    #[test]
    fn a_failed_write_out_fails_fsync_until_a_commit_and_the_commit_works() {
        use crate::store::DictMetadataStore;
        use crate::store_io::StoreIo;
        use slicefs_traits::metadata::MetadataStore;
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let wal = FlushOnFsyncWal::new(&dir.path().join("segments").join("segment-000001.seg"), 1).unwrap();
        wal.log.fail_next_io();
        let mut meta = DictMetadataStore::new(Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap())));
        meta.set_wal(Box::new(wal));
        let record = 64 * 1024;
        for i in 0..(super::super::BUFFER_CAP_BYTES / record + 2) {
            meta.log_op(vec![i as u8; record]);
        }
        assert!(meta.flush_wal().is_err(), "fsync after a failed write-out must fail");
        let root = meta.commit().expect("the commit after a failed write-out");
        assert!(meta.flush_wal().is_ok(), "after the commit, fsync works again");
        meta.log_op(b"after".to_vec());
        meta.flush_wal().unwrap();
        let (last, _, ops) = crate::segment::load_store_with_log(&dir.path().join("segments")).unwrap();
        assert_eq!(last, Some(root));
        assert_eq!(ops, vec![b"after".to_vec()]);
    }

    /// T-19-50, buffered: after a failed write-out, a record logged before
    /// the commit is dropped as lost and not waited for; the commit
    /// succeeds, fsync works again, and the records after it replay.
    #[test]
    fn a_buffered_commit_after_a_failed_write_with_queued_records_succeeds() {
        use crate::store::DictMetadataStore;
        use crate::store_io::StoreIo;
        use slicefs_traits::metadata::MetadataStore;
        use std::sync::{mpsc, Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let wal = FlushOnFsyncWal::new(&dir.path().join("segments").join("segment-000001.seg"), 1).unwrap();
        wal.log.fail_next_io();
        let mut meta = DictMetadataStore::new(Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap())));
        meta.set_wal(Box::new(wal));
        let record = 64 * 1024;
        for i in 0..(super::super::BUFFER_CAP_BYTES / record + 2) {
            meta.log_op(vec![i as u8; record]);
        }
        assert!(meta.log_append(b"queued".to_vec()).is_some());
        let meta = Arc::new(meta);
        let (tx, rx) = mpsc::channel();
        let c = {
            let meta = Arc::clone(&meta);
            std::thread::spawn(move || tx.send(meta.commit().is_ok()).unwrap())
        };
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok(true), "the commit must succeed");
        c.join().unwrap();
        assert!(meta.flush_wal().is_ok(), "after the commit, fsync works again");
        for r in [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()] {
            meta.log_op(r);
        }
        meta.flush_wal().unwrap();
        let (_, _, ops) = crate::segment::load_store_with_log(&dir.path().join("segments")).unwrap();
        assert_eq!(ops, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    }
}
