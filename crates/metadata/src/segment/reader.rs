//! Crash-tolerant segment file reader.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use super::{RecordType, SegmentEntry, SegmentHeader};

/// Reads entries from a segment file.
///
/// Crash-tolerant: truncated records at the end are silently skipped.
/// Unknown record types (including legacy 0x01 DictEntry records) are skipped
/// by consuming `payload_len` bytes forward.
/// Iteration stops at EOF marker, any truncated read, or physical end of file.
pub struct SegmentReader {
    inner: BufReader<File>,
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

        Ok(SegmentReader { inner: reader })
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

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Read record type byte
            let mut type_buf = [0u8; 1];
            match self.inner.read_exact(&mut type_buf) {
                Ok(()) => {}
                Err(_) => return None, // EOF or truncated
            }

            // Read payload_len (4 bytes LE)
            let payload_len_buf = read_exact_or_none(&mut self.inner, 4)?;
            let payload_len = u32::from_le_bytes(payload_len_buf.try_into().unwrap()) as usize;

            // 0x01 is the legacy DictEntry record type — skip its payload and continue
            if type_buf[0] == 0x01 {
                let _ = read_exact_or_none(&mut self.inner, payload_len)?;
                continue;
            }

            match RecordType::from_u8(type_buf[0]) {
                Some(RecordType::EofMarker) => {
                    // Read (and discard) any payload, then stop
                    let _ = read_exact_or_none(&mut self.inner, payload_len);
                    return None;
                }
                Some(RecordType::RootUpdate) => {
                    let payload = read_exact_or_none(&mut self.inner, payload_len)?;
                    if let Some(entry) = SegmentEntry::parse_root_update(&payload) {
                        return Some(entry);
                    }
                    return None;
                }
                Some(RecordType::SnapshotRecord) => {
                    let payload = read_exact_or_none(&mut self.inner, payload_len)?;
                    if let Some(entry) = SegmentEntry::parse_snapshot_record(&payload) {
                        return Some(entry);
                    }
                    return None;
                }
                None => {
                    // Unknown type — skip payload_len bytes and continue
                    let _ = read_exact_or_none(&mut self.inner, payload_len)?;
                    // Continue to next record
                }
            }
        }
    }
}
