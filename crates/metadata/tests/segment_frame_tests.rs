//! Segment record frame v2 (#20): each record has a sequence number and a
//! CRC32C. The reader stops at the first record that is cut, changed or out
//! of sequence (a torn tail after a crash).

use std::path::Path;

use metadata::segment::{SegmentEntry, SegmentReader, SegmentWriter};
use tempfile::TempDir;

fn root(i: u32) -> SegmentEntry {
    SegmentEntry::RootUpdate { root: slicefs_traits::key::ObjectKey::example(i) }
}

/// Write `n` records; return the file length after each record.
fn write(path: &Path, n: u32) -> Vec<u64> {
    let mut w = SegmentWriter::new(path, 7).unwrap();
    let mut ends = Vec::new();
    for i in 0..n {
        w.write_entry(&root(i)).unwrap();
        w.sync().unwrap();
        ends.push(std::fs::metadata(path).unwrap().len());
    }
    ends
}

fn read(path: &Path) -> Vec<SegmentEntry> {
    SegmentReader::open(path).unwrap().collect()
}

#[test]
fn records_read_back_in_order() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("s.seg");
    write(&p, 3);
    assert_eq!(read(&p), vec![root(0), root(1), root(2)]);
}

#[test]
fn a_record_cut_in_the_middle_is_ignored() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("s.seg");
    let ends = write(&p, 3);
    let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
    f.set_len(ends[2] - 3).unwrap();
    assert_eq!(read(&p), vec![root(0), root(1)]);
}

#[test]
fn a_changed_byte_stops_the_read_at_that_record() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("s.seg");
    let ends = write(&p, 3);
    let mut bytes = std::fs::read(&p).unwrap();
    // A byte in the payload of the second record (not the frame length).
    let at = (ends[0] + (ends[1] - ends[0]) / 2) as usize;
    bytes[at] ^= 0x40;
    std::fs::write(&p, &bytes).unwrap();
    assert_eq!(read(&p), vec![root(0)]);
}

#[test]
fn a_repeated_record_is_out_of_sequence() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("s.seg");
    let ends = write(&p, 2);
    // Append a copy of the first record: its CRC is good, its sequence is not.
    let bytes = std::fs::read(&p).unwrap();
    let first = bytes[16..ends[0] as usize].to_vec();
    let mut all = bytes.clone();
    all.extend_from_slice(&first);
    std::fs::write(&p, &all).unwrap();
    assert_eq!(read(&p), vec![root(0), root(1)]);
}
