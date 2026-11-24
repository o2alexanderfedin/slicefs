//! Append-only segment file writer.

use std::borrow::Borrow;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use chunk_store::pack_io::SyncWait;

use super::{SegmentEntry, SegmentHeader, RecordType};

/// The sequential writes (`write_entry`) go to the file when their buffer
/// passes this size, so a long sequential write holds little RAM.
const BUF_CAP: usize = 64 * 1024;

/// What a [`SegmentWriter::begin_sync`] covered: the segment and the
/// number of record writes started or finished then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncMark {
    id: u64,
    writes: u64,
}

/// A record write that runs with no lock held (see
/// [`SegmentWriter::begin_write`]): the sequence numbers and the file
/// offset are reserved for it.
pub struct WriteSlot {
    file: Arc<File>,
    id: u64,
    offset: u64,
    seq: u64,
    n: u64,
    done: SyncWait,
    /// Test fault hook: this write fails.
    #[cfg(test)]
    fail: bool,
}

impl WriteSlot {
    /// Frame each entry with its reserved sequence number (the CRC is
    /// computed here, outside any lock) into one buffer and write it at the
    /// reserved offset with one positional write. Returns the bytes written.
    /// `entries` must hold exactly the number of records reserved.
    pub fn write<I>(&self, entries: I) -> io::Result<u64>
    where
        I: IntoIterator,
        I::Item: Borrow<SegmentEntry>,
    {
        #[cfg(test)]
        if self.fail {
            return Err(io::Error::other("injected write-log fault"));
        }
        let mut buf = Vec::new();
        let mut seq = self.seq;
        for entry in entries {
            let entry = entry.borrow();
            buf.extend_from_slice(&super::frame_record(entry.record_type() as u8, seq, &entry.payload_bytes()));
            seq += 1;
        }
        if seq - self.seq != self.n {
            return Err(io::Error::other("a record write got another number of records than it reserved"));
        }
        self.file.write_all_at(&buf, self.offset)?;
        Ok(buf.len() as u64)
    }
}

/// Writes records to a segment file in append-only fashion.
///
/// Each record is framed as: record_type(u8) + payload_len(u32 LE) +
/// seq(u64 LE) + payload + crc32c(u32 LE) over all the bytes before it.
/// `seq` counts the records of this segment from 0. The file starts with a
/// 16-byte `SegmentHeader`.
///
/// All writes are positional, at the end offset. Two kinds of callers:
/// - sequential ones (seed records, the EOF marker): `write_entry` frames
///   into a small buffer, and `flush` (or `sync`) writes it at the end;
/// - the write log: `begin_write` reserves the sequence numbers and the
///   offset of a batch under the caller's lock, `WriteSlot::write` writes it
///   with no lock, and `end_write` publishes it. At most one such write is
///   in flight, at the end of the segment, so a segment never has a gap.
pub struct SegmentWriter {
    file: Arc<File>,
    /// End offset of the bytes written (header and published records).
    end: u64,
    /// Frames of `write_entry` not yet written, and their number.
    buf: Vec<u8>,
    buffered: u64,
    /// Sequence number of the next record.
    seq: u64,
    /// True when bytes were written since the last successful `sync`.
    unsynced: bool,
    /// Number of `sync_all` calls made (for tests).
    disk_syncs: u64,
    /// Id of this segment.
    id: u64,
    /// Number of record writes started or finished (for `end_sync`).
    writes: u64,
    /// The record write in flight (`begin_write` to `end_write`).
    in_flight: Option<SyncWait>,
    /// Test fault hook: the next `begin_write` (its slot write), `flush` or
    /// `sync` fails.
    #[cfg(test)]
    fail_next: bool,
}

impl SegmentWriter {
    /// Create a new segment file at `path` with the given `segment_id`.
    /// Writes the 16-byte header immediately.
    pub fn new(path: &Path, segment_id: u64) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
        let header = SegmentHeader::new(segment_id);
        file.write_all_at(&header.to_bytes(), 0)?;
        Ok(SegmentWriter {
            file: Arc::new(file),
            end: 16,
            buf: Vec::new(),
            buffered: 0,
            seq: 0,
            unsynced: true,
            disk_syncs: 0,
            id: segment_id,
            writes: 0,
            in_flight: None,
            #[cfg(test)]
            fail_next: false,
        })
    }

    /// Write a single entry (see the frame in the type docs). Returns its
    /// sequence number. The frame goes to a buffer; `flush` or `sync`
    /// writes it (and so does a buffer past `BUF_CAP`).
    pub fn write_entry(&mut self, entry: &SegmentEntry) -> io::Result<u64> {
        self.injected_fault()?;
        if self.in_flight.is_some() {
            return Err(in_flight_error());
        }
        let payload = entry.payload_bytes();
        let frame = super::frame_record(entry.record_type() as u8, self.seq, &payload);
        self.buf.extend_from_slice(&frame);
        self.buffered += 1;
        self.seq += 1;
        let seq = self.seq - 1;
        if self.buf.len() >= BUF_CAP {
            self.write_buf()?;
        }
        Ok(seq)
    }

    /// Write the buffer at the end offset with one positional write. On an
    /// error the buffered records are dropped and their sequence numbers
    /// given back, so the next write goes to the same place.
    fn write_buf(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let r = self.file.write_all_at(&self.buf, self.end);
        if r.is_ok() {
            self.end += self.buf.len() as u64;
            self.writes += 1;
            self.unsynced = true;
        } else {
            self.seq -= self.buffered;
        }
        self.buf.clear();
        self.buffered = 0;
        r
    }

    /// Flush buffered data and call `sync_all` on the underlying file.
    ///
    /// Does nothing when no bytes were written since the last successful sync.
    /// `sync_all` is a full disk flush (F_FULLFSYNC on macOS, ~10-20 ms), and
    /// With the NFS backend (now removed), FUSE fsync called this on every
    /// file close.
    pub fn sync(&mut self) -> io::Result<()> {
        self.injected_fault()?;
        self.write_buf()?;
        if !self.unsynced {
            return Ok(());
        }
        self.file.sync_all()?;
        self.disk_syncs += 1;
        self.unsynced = false;
        Ok(())
    }

    /// Write the records in the user-space buffer into the file (one
    /// `write()` into the kernel), with no disk sync. After this, a crash of
    /// the process does not lose them; a power loss can. A later `sync` still
    /// syncs them.
    pub fn flush(&mut self) -> io::Result<()> {
        self.injected_fault()?;
        self.write_buf()
    }

    /// Reserve a record write of `n` records with no lock held across it:
    /// the sequence numbers `seq..seq + n` and the end offset. The buffer
    /// must be empty and no write in flight (the one-write rule). The caller
    /// writes the slot with no lock, then calls [`Self::end_write`].
    pub fn begin_write(&mut self, n: usize) -> io::Result<WriteSlot> {
        if self.in_flight.is_some() || !self.buf.is_empty() {
            return Err(in_flight_error());
        }
        let done = SyncWait::default();
        self.in_flight = Some(done.clone());
        self.writes += 1;
        Ok(WriteSlot {
            file: Arc::clone(&self.file),
            id: self.id,
            offset: self.end,
            seq: self.seq,
            n: n as u64,
            done,
            #[cfg(test)]
            fail: std::mem::take(&mut self.fail_next),
        })
    }

    /// Publish the write of `slot` with its result (the bytes written). On
    /// success the end offset, the sequence number and the write count move
    /// and the segment is unsynced. On an error they stay (the caller marks
    /// its log failed; a later write overwrites the partial bytes). Either
    /// way the write in flight ends and its waiters wake.
    pub fn end_write(&mut self, slot: WriteSlot, result: &io::Result<u64>) {
        if slot.id == self.id && slot.offset == self.end {
            if let Ok(len) = result {
                self.end += len;
                self.seq += slot.n;
                self.writes += 1;
                self.unsynced = true;
            }
            self.in_flight = None;
        }
        slot.done.done();
    }

    /// The end of the record write in flight, if one is.
    pub fn in_flight(&self) -> Option<SyncWait> {
        self.in_flight.clone()
    }

    /// Make the next record write (`begin_write`'s slot), `flush` or `sync`
    /// fail (test fault hook).
    #[cfg(test)]
    pub(crate) fn fail_next_io(&mut self) {
        self.fail_next = true;
    }

    #[cfg(test)]
    fn injected_fault(&mut self) -> io::Result<()> {
        if std::mem::take(&mut self.fail_next) {
            return Err(io::Error::other("injected write-log fault"));
        }
        Ok(())
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn injected_fault(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Number of records written so far (the sequence number of the next).
    pub fn records(&self) -> u64 {
        self.seq
    }

    /// Write the user-space buffer into the file and return a second handle
    /// of the file, so a caller can sync it without holding this writer
    /// (`sync_all` on the clone syncs the same file).
    pub fn file_for_sync(&mut self) -> io::Result<File> {
        self.injected_fault()?;
        self.write_buf()?;
        self.file.try_clone()
    }

    /// First half of a sync that runs with no lock held: write the
    /// user-space buffer into the file and return a second handle of it for
    /// `sync_all`, and a mark for [`Self::end_sync`]. `None`: nothing was
    /// written since the last sync. A record write in flight is not covered.
    pub fn begin_sync(&mut self) -> io::Result<Option<(File, SyncMark)>> {
        self.injected_fault()?;
        self.write_buf()?;
        if !self.unsynced {
            return Ok(None);
        }
        let file = self.file.try_clone()?;
        Ok(Some((file, SyncMark { id: self.id, writes: self.writes })))
    }

    /// Second half of a sync, after `sync_all` on the handle of `mark`'s
    /// `begin_sync` succeeded: count the disk sync, and mark the segment
    /// synced if no write started or finished since. Does nothing if this
    /// writer is another segment (a cut or rotate replaced it meanwhile).
    pub fn end_sync(&mut self, mark: SyncMark) {
        if mark.id != self.id {
            return;
        }
        self.disk_syncs += 1;
        if mark.writes == self.writes && self.in_flight.is_none() {
            self.unsynced = false;
        }
    }

    /// Id of this segment.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Number of real disk syncs so far.
    pub fn disk_syncs(&self) -> u64 {
        self.disk_syncs
    }

    /// Write EOF marker, sync, and close the file.
    pub fn close(mut self) -> io::Result<()> {
        // EofMarker: type=0xFF, payload_len=0
        self.buf.push(RecordType::EofMarker as u8);
        self.buf.extend_from_slice(&0u32.to_le_bytes());
        self.sync()
    }
}

impl Drop for SegmentWriter {
    /// Write what is still buffered (best effort, as a buffered stream does
    /// on drop).
    fn drop(&mut self) {
        if self.in_flight.is_none() {
            let _ = self.write_buf();
        }
    }
}

fn in_flight_error() -> io::Error {
    io::Error::other("a segment write is in flight or buffered")
}

#[cfg(test)]
mod sync_tests {
    use super::*;
    use slicefs_traits::key::ObjectKey;

    #[test]
    fn sync_skips_disk_flush_when_nothing_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = SegmentWriter::new(&dir.path().join("s.seg"), 1).unwrap();
        w.sync().unwrap();
        assert_eq!(w.disk_syncs(), 1, "header must be synced once");
        w.sync().unwrap();
        w.sync().unwrap();
        assert_eq!(w.disk_syncs(), 1, "no new bytes: no disk flush");
        let root: ObjectKey = ObjectKey::example(7);
        w.write_entry(&SegmentEntry::RootUpdate { root }).unwrap();
        w.sync().unwrap();
        assert_eq!(w.disk_syncs(), 2);
    }
}
