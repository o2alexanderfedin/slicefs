//! Periodic WAL strategy — holds records in RAM as `FlushOnFsyncWal` does
//! (fsync, fsyncdir and commit write and sync them; past `BUFFER_CAP_BYTES`
//! they go to the segment with no sync), and a timer thread also writes and
//! syncs them every `interval`.

use std::io;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::buffered::BufferedLog;
use super::{WalEntry, WalError, WalStrategy};

/// The timer thread. Dropping `stop` ends it.
struct Timer {
    stop: mpsc::Sender<()>,
    thread: JoinHandle<()>,
}

/// WAL that holds mutations in RAM and syncs them on a timer, on an explicit
/// `flush_and_sync`, and at shutdown.
pub struct PeriodicWal {
    log: Arc<BufferedLog>,
    timer: Mutex<Option<Timer>>,
}

impl PeriodicWal {
    /// Create a new `PeriodicWal` writing to `path` with the given `segment_id`,
    /// with no timer: only `flush_and_sync`, the cap and shutdown write records.
    pub fn new(path: &Path, segment_id: u64) -> io::Result<Self> {
        Ok(PeriodicWal {
            log: Arc::new(BufferedLog::new(path, segment_id)?),
            timer: Mutex::new(None),
        })
    }

    /// Like `new`, with a thread that calls `flush_and_sync` each `interval`.
    /// A zero `interval` starts no thread. Drop and `shutdown` stop the thread.
    pub fn with_interval(path: &Path, segment_id: u64, interval: Duration) -> io::Result<Self> {
        let wal = Self::new(path, segment_id)?;
        if !interval.is_zero() {
            // The thread holds only the log, never the PeriodicWal, so the
            // drop of the PeriodicWal can stop and join it.
            let log = Arc::clone(&wal.log);
            let (stop, wake) = mpsc::channel::<()>();
            let thread = std::thread::Builder::new()
                .name("slicefs-wal-sync".into())
                .spawn(move || {
                    while let Err(RecvTimeoutError::Timeout) = wake.recv_timeout(interval) {
                        if let Err(e) = log.flush_and_sync() {
                            tracing::warn!("periodic write-log sync failed: {e}");
                        }
                    }
                })?;
            *wal.timer.lock().unwrap() = Some(Timer { stop, thread });
        }
        Ok(wal)
    }

    /// Stop the timer thread and wait for it to end. Does nothing if no
    /// thread runs.
    fn stop_timer(&self) {
        let timer = self.timer.lock().unwrap().take();
        if let Some(Timer { stop, thread }) = timer {
            drop(stop);
            let _ = thread.join();
        }
    }
}

impl Drop for PeriodicWal {
    fn drop(&mut self) {
        self.stop_timer();
    }
}

impl WalStrategy for PeriodicWal {
    fn rotate(&self, seed: &[WalEntry]) -> Result<(), WalError> {
        self.log.rotate(seed)
    }
    fn log_mutation(&self, entry: &WalEntry) -> Result<(), WalError> {
        self.log.log(entry)
    }

    fn flush_and_sync(&self) -> Result<(), WalError> {
        self.log.flush_and_sync()
    }

    fn shutdown(&self) -> Result<(), WalError> {
        self.stop_timer();
        self.flush_and_sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::{SegmentEntry, SegmentReader};
    use std::time::Instant;

    #[test]
    fn ram_stays_at_most_the_cap_with_no_fsync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.seg");
        let wal = PeriodicWal::new(&path, 1).unwrap();
        super::super::buffered::assert_cap_holds(&wal, &wal.log, &path);
    }

    /// The timer syncs the held records within about 2 x interval with no
    /// fsync from the caller, and drop stops the thread.
    #[test]
    fn timer_syncs_and_drop_stops_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.seg");
        let interval = Duration::from_millis(300);
        let wal = PeriodicWal::with_interval(&path, 1, interval).unwrap();
        let syncs = wal.log.disk_syncs();
        let start = Instant::now();
        wal.log_mutation(&WalEntry::Op { bytes: vec![9u8; 100] }).unwrap();
        let deadline = start + 2 * interval + Duration::from_millis(300);
        while wal.log.disk_syncs() == syncs && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            wal.log.disk_syncs() > syncs,
            "no sync within {:?} (interval {:?})",
            start.elapsed(),
            interval
        );
        let on_disk: Vec<SegmentEntry> =
            SegmentReader::open(&path).map(|r| r.collect()).unwrap_or_default();
        assert_eq!(on_disk.len(), 1, "the timer must write the record to the file");
        let log = Arc::downgrade(&wal.log);
        drop(wal);
        assert!(log.upgrade().is_none(), "drop must stop the timer thread");
    }
}
