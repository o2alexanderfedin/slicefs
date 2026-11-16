//! Crash-tolerant segment file reader.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use super::{RecordType, SegmentEntry, SegmentHeader};

/// Reads entries from a segment file.
///
/// Crash-tolerant: a torn tail (a cut, changed or out-of-sequence record) ends
/// the iteration. Unknown record types are skipped.
pub struct SegmentReader {
    inner: BufReader<File>,
    /// Sequence number that the next record must have.
    next_seq: u64,
}

impl SegmentReader {
    /// Open a segment file and validate its header.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);

        let mut header_buf = [0u8; 16];
        reader.read_exact(&mut header_buf)?;

        SegmentHeader::from_bytes(&header_buf)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid segment header"))?;

        Ok(SegmentReader { inner: reader, next_seq: 0 })
    }
}

/// Read exactly `n` bytes; return None if fewer bytes are available (truncated).
fn read_exact_or_none(reader: &mut BufReader<File>, n: usize) -> Option<Vec<u8>> {
    if n == 0 {
        return Some(vec![]);
    }
    let mut buf = vec![0u8; n];
    match reader.read_exact(&mut buf) {
        Ok(()) => Some(buf),
        Err(_) => None, // truncated or EOF
    }
}

impl Iterator for SegmentReader {
    type Item = SegmentEntry;

    /// The next good record. Stops at the end of the file, at an EOF marker,
    /// and at the first record that is cut, has a bad CRC, or is out of
    /// sequence: after a crash only the tail can be torn, and nothing after a
    /// bad record can be trusted.
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let head = read_exact_or_none(&mut self.inner, super::FRAME_HEAD)?;
            let record_type = head[0];
            let payload_len = u32::from_le_bytes(head[1..5].try_into().unwrap()) as usize;
            let seq = u64::from_le_bytes(head[5..13].try_into().unwrap());
            if payload_len > super::MAX_PAYLOAD || seq != self.next_seq {
                return None;
            }
            let payload = read_exact_or_none(&mut self.inner, payload_len)?;
            let crc = read_exact_or_none(&mut self.inner, 4)?;
            let want = crc32c::crc32c_append(crc32c::crc32c(&head), &payload);
            if u32::from_le_bytes(crc.try_into().unwrap()) != want {
                return None;
            }
            self.next_seq += 1;
            match RecordType::from_u8(record_type) {
                Some(RecordType::EofMarker) => return None,
                Some(RecordType::RootUpdate) => return SegmentEntry::parse_root_update(&payload),
                Some(RecordType::SnapshotRecord) => return SegmentEntry::parse_snapshot_record(&payload),
                None => continue, // a type this version does not know
            }
        }
    }
}
