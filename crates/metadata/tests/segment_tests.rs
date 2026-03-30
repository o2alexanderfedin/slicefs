/// Tests for the segment file format: writer, reader, crash-tolerance.
use metadata::segment::{
    SegmentEntry, SegmentReader, SegmentWriter, SEGMENT_MAGIC, SEGMENT_VERSION,
};
use slicefs_traits::digest::Digest224;
use tempfile::TempDir;

fn make_key(v: u32) -> Digest224 {
    [v, v + 1, v + 2, v + 3, v + 4, v + 5, v + 6]
}

/// Write 3 RootUpdate records and read them back — all 3 must match.
#[test]
fn test_round_trip_root_update_entries() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("seg0.seg");

    let entries = vec![
        SegmentEntry::RootUpdate { root: make_key(1) },
        SegmentEntry::RootUpdate { root: make_key(2) },
        SegmentEntry::RootUpdate { root: make_key(3) },
    ];

    {
        let mut writer = SegmentWriter::new(&path, 0).unwrap();
        for e in &entries {
            writer.write_entry(e).unwrap();
        }
        writer.close().unwrap();
    }

    let reader = SegmentReader::open(&path).unwrap();
    let read_back: Vec<SegmentEntry> = reader.collect();

    assert_eq!(read_back.len(), 3);
    for (expected, actual) in entries.iter().zip(read_back.iter()) {
        match (expected, actual) {
            (
                SegmentEntry::RootUpdate { root: r1 },
                SegmentEntry::RootUpdate { root: r2 },
            ) => {
                assert_eq!(r1, r2);
            }
            _ => panic!("entry type mismatch"),
        }
    }
}

/// Write a RootUpdate record and read it back — digest must match.
#[test]
fn test_round_trip_root_update() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("seg1.seg");
    let root = make_key(99);

    {
        let mut writer = SegmentWriter::new(&path, 1).unwrap();
        writer.write_entry(&SegmentEntry::RootUpdate { root }).unwrap();
        writer.close().unwrap();
    }

    let reader = SegmentReader::open(&path).unwrap();
    let entries: Vec<SegmentEntry> = reader.collect();

    assert_eq!(entries.len(), 1);
    match &entries[0] {
        SegmentEntry::RootUpdate { root: r } => assert_eq!(r, &root),
        _ => panic!("expected RootUpdate"),
    }
}

/// Truncate the segment file mid-record — reader returns only complete records (no error).
#[test]
fn test_truncated_record_is_skipped() {
    use std::io::Write;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("seg2.seg");

    {
        let mut writer = SegmentWriter::new(&path, 2).unwrap();
        writer.write_entry(&SegmentEntry::RootUpdate { root: make_key(1) }).unwrap();
        // Intentionally do NOT call close() — just drop; no EOF marker
    }

    // Append partial record header to simulate truncation
    {
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        // Write partial record: type=RootUpdate(0x02), partial len bytes
        file.write_all(&[0x02, 0x1C, 0x00, 0x00]).unwrap(); // type + partial len
    }

    let reader = SegmentReader::open(&path).unwrap();
    let entries: Vec<SegmentEntry> = reader.collect();

    // Only the complete first entry should be returned
    assert_eq!(entries.len(), 1);
}

/// Write a record with unknown type byte 0xFE — reader skips it and reads subsequent valid records.
#[test]
fn test_unknown_record_type_skipped() {
    use std::io::Write;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("seg3.seg");

    // Write header manually
    {
        let mut file = std::fs::File::create(&path).unwrap();
        // 16-byte segment header: magic(4) + version(4 LE) + segment_id(8 LE)
        file.write_all(&SEGMENT_MAGIC).unwrap();
        file.write_all(&SEGMENT_VERSION.to_le_bytes()).unwrap();
        file.write_all(&42u64.to_le_bytes()).unwrap();

        // Unknown record type 0xFE with a 4-byte payload
        let payload: [u8; 4] = [0xAA, 0xBB, 0xCC, 0xDD];
        file.write_all(&[0xFE]).unwrap(); // type
        file.write_all(&(payload.len() as u32).to_le_bytes()).unwrap(); // payload_len
        file.write_all(&payload).unwrap();

        // Valid RootUpdate record after it: type(1) + payload_len(4) + root(28)
        let root = make_key(7);
        file.write_all(&[0x02]).unwrap(); // RootUpdate
        file.write_all(&28u32.to_le_bytes()).unwrap();
        for v in &root {
            file.write_all(&v.to_le_bytes()).unwrap();
        }
    }

    let reader = SegmentReader::open(&path).unwrap();
    let entries: Vec<SegmentEntry> = reader.collect();

    assert_eq!(entries.len(), 1, "should skip unknown type and read valid entry");
    match &entries[0] {
        SegmentEntry::RootUpdate { root } => assert_eq!(root, &make_key(7)),
        _ => panic!("expected RootUpdate"),
    }
}

/// Legacy DictEntry records (type 0x01) are silently skipped on read.
#[test]
fn test_legacy_dict_entry_skipped() {
    use std::io::Write;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("seg_legacy.seg");

    {
        let mut file = std::fs::File::create(&path).unwrap();
        // 16-byte segment header
        file.write_all(&SEGMENT_MAGIC).unwrap();
        file.write_all(&SEGMENT_VERSION.to_le_bytes()).unwrap();
        file.write_all(&1u64.to_le_bytes()).unwrap();

        // Legacy DictEntry: type=0x01, payload_len=92, payload=(28+64 zeros)
        let payload = vec![0u8; 92];
        file.write_all(&[0x01]).unwrap();
        file.write_all(&92u32.to_le_bytes()).unwrap();
        file.write_all(&payload).unwrap();

        // Valid RootUpdate after the legacy record
        let root = make_key(42);
        file.write_all(&[0x02]).unwrap();
        file.write_all(&28u32.to_le_bytes()).unwrap();
        for v in &root {
            file.write_all(&v.to_le_bytes()).unwrap();
        }
    }

    let reader = SegmentReader::open(&path).unwrap();
    let entries: Vec<SegmentEntry> = reader.collect();

    // The legacy DictEntry must be skipped; only the RootUpdate is returned.
    assert_eq!(entries.len(), 1, "legacy DictEntry should be skipped");
    match &entries[0] {
        SegmentEntry::RootUpdate { root } => assert_eq!(root, &make_key(42)),
        _ => panic!("expected RootUpdate after skipping legacy DictEntry"),
    }
}

/// Empty segment (header only) returns zero entries.
#[test]
fn test_empty_segment_returns_zero_entries() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("seg4.seg");

    {
        let writer = SegmentWriter::new(&path, 4).unwrap();
        writer.close().unwrap();
    }

    let reader = SegmentReader::open(&path).unwrap();
    let entries: Vec<SegmentEntry> = reader.collect();
    assert_eq!(entries.len(), 0);
}

/// Segment header has correct magic bytes [0x53, 0x4C, 0x53, 0x47] and version 1.
#[test]
fn test_segment_header_magic_and_version() {
    use std::io::Read;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("seg5.seg");

    {
        let writer = SegmentWriter::new(&path, 5).unwrap();
        writer.close().unwrap();
    }

    let mut file = std::fs::File::open(&path).unwrap();
    let mut magic = [0u8; 4];
    let mut version_bytes = [0u8; 4];
    file.read_exact(&mut magic).unwrap();
    file.read_exact(&mut version_bytes).unwrap();

    assert_eq!(&magic, &[0x53, 0x4C, 0x53, 0x47], "magic bytes must be SLSG");
    assert_eq!(
        u32::from_le_bytes(version_bytes),
        1,
        "version must be 1"
    );
    assert_eq!(&magic, &SEGMENT_MAGIC);
    assert_eq!(u32::from_le_bytes(version_bytes), SEGMENT_VERSION);
}
