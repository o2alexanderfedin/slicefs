//! Segment file format for SliceFS metadata persistence.
//!
//! A segment file is an append-only log of records. Records are framed as:
//!   record_type: u8
//!   payload_len: u32 LE
//!   seq:         u64 LE (records of this segment, from 0)
//!   payload:     [u8; payload_len]
//!   crc32c:      u32 LE over all bytes above
//!
//! The file begins with a 16-byte header:
//!   magic:      [u8; 4]  = [0x53, 0x4C, 0x53, 0x47] ("SLSG")
//!   version:    u32 LE   = 3
//!   segment_id: u64 LE
//!
//! RootUpdate payload: `KEY_BYTES` (33) bytes — ObjectKey (tag + digest)
//! SnapshotRecord payload: `SNAPSHOT_MIN` (53)+ bytes — version(8) + root(KEY_BYTES) + created_at(8) + name_len(4) + name_bytes(variable)
//! EofMarker: no payload (payload_len = 0), terminates iteration
//!
//! The reader stops at the first cut, changed or out-of-sequence record.

pub mod writer;
pub mod reader;
pub mod compaction;

pub use writer::SegmentWriter;
pub use reader::SegmentReader;

use std::path::Path;
use slicefs_traits::key::{ObjectKey, KEY_BYTES};

/// Offsets in a SnapshotRecord payload.
const SNAPSHOT_ROOT: usize = 8;
const SNAPSHOT_CREATED_AT: usize = SNAPSHOT_ROOT + KEY_BYTES;
pub(crate) const SNAPSHOT_NAME_LEN: usize = SNAPSHOT_CREATED_AT + 8;
/// Size of a SnapshotRecord payload without a name.
pub(crate) const SNAPSHOT_MIN: usize = SNAPSHOT_NAME_LEN + 4;
use thiserror::Error;

use crate::snapshot::SnapshotEntry;

/// Errors returned by segment-level operations.
#[derive(Debug, Error)]
pub enum SegmentError {
    #[error("segment I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Load all segment files from `segments_dir`, replay them, and return the last
/// `RootUpdate` digest seen along with all `SnapshotRecord` entries.
///
/// Segment files are read in ascending order by segment_id (encoded in the filename
/// as `segment-{id:06}.seg`). Legacy DictEntry records (type 0x01) are silently
/// skipped: the metadata nodes in the chunk store are the data.
/// RootUpdate records update the `last_root` tracker;
/// SnapshotRecord entries are collected into the returned Vec.
///
/// Returns `(Option<last_root_digest>, Vec<SnapshotEntry>)`.
pub fn load_store_from_segments(
    segments_dir: &Path,
) -> Result<(Option<ObjectKey>, Vec<SnapshotEntry>), SegmentError> {
    let mut last_root: Option<ObjectKey> = None;
    let mut snapshots: Vec<SnapshotEntry> = Vec::new();

    // Collect segment files and sort by name (which encodes segment_id numerically)
    let mut seg_paths: Vec<std::path::PathBuf> = std::fs::read_dir(segments_dir)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("segment-") && name.ends_with(".seg") {
                Some(entry.path())
            } else {
                None
            }
        })
        .collect();

    // By segment id, not by name: past id 999999 the name has more digits
    // ("segment-1000000.seg" < "segment-999999.seg" as text).
    seg_paths.sort_by_key(|p| segment_order(p));

    for path in &seg_paths {
        // Skip empty segment files (created by WAL init but never written to,
        // e.g. when the mount is killed before any commits).
        let file_len = std::fs::metadata(path)
            .map(|m| m.len())
            .unwrap_or(0);
        if file_len < 16 {
            // Too small to contain even the 16-byte header — skip silently.
            continue;
        }

        let reader = SegmentReader::open(path)?;
        for entry in reader {
            match entry {
                SegmentEntry::RootUpdate { root } => {
                    last_root = Some(root);
                }
                ref snap @ SegmentEntry::SnapshotRecord { .. } => {
                    if let Some(snap_entry) = snap.as_snapshot_entry() {
                        snapshots.push(snap_entry);
                    }
                }
                SegmentEntry::Op { .. } => {}
            }
        }
    }

    Ok((last_root, snapshots))
}

/// Like [`load_store_from_segments`], and also return the operation records
/// after the last `RootUpdate`, in order (the write log, #20), all in RAM.
/// For tests and tools; a mount streams them with [`scan_store_with_log`].
pub fn load_store_with_log(
    segments_dir: &Path,
) -> Result<(Option<ObjectKey>, Vec<SnapshotEntry>, Vec<Vec<u8>>), SegmentError> {
    let (root, snapshots, log) = scan_store_with_log(segments_dir)?;
    let ops = log.records().collect::<Result<Vec<_>, _>>()?;
    Ok((root, snapshots, ops))
}

/// Like [`load_store_from_segments`], and also find the write log (#20): the
/// operation records after the last `RootUpdate`. Pass 1 of 2: it keeps only
/// where the log is, not its records, so memory does not grow with the log
/// (#106). [`WriteLog::records`] is pass 2.
pub fn scan_store_with_log(
    segments_dir: &Path,
) -> Result<(Option<ObjectKey>, Vec<SnapshotEntry>, WriteLog), SegmentError> {
    let mut last_root = None;
    let mut snapshots = Vec::new();
    let mut spans: Vec<LogSpan> = Vec::new();
    let mut ops = 0usize;
    for path in segment_paths(segments_dir)? {
        let mut reader = SegmentReader::open(&path)?;
        let (pos, seq) = reader.cursor();
        spans.push(LogSpan { path, pos, seq, end_seq: seq });
        while let Some(entry) = reader.next() {
            match entry {
                SegmentEntry::RootUpdate { root } => {
                    last_root = Some(root);
                    // The log starts after this record, in this segment.
                    let (pos, seq) = reader.cursor();
                    let span = spans.pop().expect("span of this segment");
                    spans.clear();
                    spans.push(LogSpan { path: span.path, pos, seq, end_seq: seq });
                    ops = 0;
                }
                ref snap @ SegmentEntry::SnapshotRecord { .. } => {
                    if let Some(s) = snap.as_snapshot_entry() {
                        snapshots.push(s);
                    }
                }
                SegmentEntry::Op { .. } => ops += 1,
            }
        }
        // Pass 2 reads this segment up to here, and no further.
        let end = reader.cursor().1;
        spans.last_mut().expect("span of this segment").end_seq = end;
    }
    spans.retain(|s| s.end_seq > s.seq);
    Ok((last_root, snapshots, WriteLog { spans, ops }))
}

/// Where the write log is, from pass 1 ([`scan_store_with_log`]).
#[derive(Debug)]
pub struct WriteLog {
    spans: Vec<LogSpan>,
    ops: usize,
}

/// The part of one segment file that is in the write log: the records with
/// sequence numbers `seq..end_seq`, the first at byte `pos`.
#[derive(Debug)]
struct LogSpan {
    path: std::path::PathBuf,
    pos: u64,
    seq: u64,
    end_seq: u64,
}

impl WriteLog {
    /// Number of operation records that pass 1 found.
    pub fn len(&self) -> usize {
        self.ops
    }

    /// True if there is nothing to replay.
    pub fn is_empty(&self) -> bool {
        self.ops == 0
    }

    /// Pass 2: the operation records, in order, read from the files one at a
    /// time. It reads exactly the records that pass 1 saw: none after them
    /// (records added since), and an error if one of them is not there any
    /// more (the file changed between the passes).
    pub fn records(&self) -> WriteLogRecords<'_> {
        WriteLogRecords { spans: &self.spans, next: 0, reader: None, end_seq: 0, ops: 0, want: self.ops, done: false }
    }
}

/// Iterator of [`WriteLog::records`].
pub struct WriteLogRecords<'a> {
    spans: &'a [LogSpan],
    /// Index of the next span to open.
    next: usize,
    reader: Option<SegmentReader>,
    /// End (exclusive sequence number) of the span of `reader`.
    end_seq: u64,
    /// Operation records returned so far, and the number pass 1 found.
    ops: usize,
    want: usize,
    done: bool,
}

impl WriteLogRecords<'_> {
    fn changed(&mut self, what: &str) -> Option<Result<Vec<u8>, SegmentError>> {
        self.done = true;
        Some(Err(SegmentError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("the write log changed while it was read: {what}"),
        ))))
    }
}

impl Iterator for WriteLogRecords<'_> {
    type Item = Result<Vec<u8>, SegmentError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let reader = match &mut self.reader {
                Some(r) if r.cursor().1 < self.end_seq => r,
                _ => {
                    self.reader = None;
                    let Some(span) = self.spans.get(self.next) else {
                        if self.ops != self.want {
                            return self.changed("it has other records");
                        }
                        self.done = true;
                        return None;
                    };
                    self.next += 1;
                    match SegmentReader::open_at(&span.path, span.pos, span.seq) {
                        Ok(r) => {
                            self.end_seq = span.end_seq;
                            self.reader = Some(r);
                            continue;
                        }
                        Err(e) => {
                            self.done = true;
                            return Some(Err(e.into()));
                        }
                    }
                }
            };
            match reader.next() {
                Some(SegmentEntry::Op { bytes }) => {
                    self.ops += 1;
                    return Some(Ok(bytes));
                }
                Some(SegmentEntry::SnapshotRecord { .. }) => {}
                Some(SegmentEntry::RootUpdate { .. }) => return self.changed("a root after the last root"),
                // A record that pass 1 also read as the end of the segment
                // (an EOF marker, a bad root): the span ends here.
                None if reader.cursor().1 == self.end_seq => {}
                None => return self.changed("a record is cut or changed"),
            }
        }
    }
}

/// Segment files of `segments_dir` with at least a header, in id order.
fn segment_paths(segments_dir: &Path) -> Result<Vec<std::path::PathBuf>, SegmentError> {
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(segments_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with("segment-") && n.ends_with(".seg")
        })
        .map(|e| e.path())
        .filter(|p| std::fs::metadata(p).map(|m| m.len() >= 16).unwrap_or(false))
        .collect();
    paths.sort_by_key(|p| segment_order(p));
    Ok(paths)
}

/// Sort key of a segment file: its numeric id (then the name, for a name
/// that has no number).
fn segment_order(p: &std::path::Path) -> (u64, String) {
    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let id = name.strip_prefix("segment-").and_then(|n| n.strip_suffix(".seg")).and_then(|n| n.parse().ok());
    (id.unwrap_or(u64::MAX), name)
}

#[cfg(test)]
mod order_tests {
    #[test]
    fn segments_sort_by_id_past_six_digits() {
        let mut v: Vec<std::path::PathBuf> =
            ["segment-1000000.seg", "segment-999999.seg", "segment-000002.seg"].iter().map(|n| n.into()).collect();
        v.sort_by_key(|p| super::segment_order(p));
        let names: Vec<_> = v.iter().map(|p| p.to_string_lossy().to_string()).collect();
        assert_eq!(names, ["segment-000002.seg", "segment-999999.seg", "segment-1000000.seg"]);
    }
}

/// Magic bytes identifying a SliceFS segment file: "SLSG"
pub const SEGMENT_MAGIC: [u8; 4] = [0x53, 0x4C, 0x53, 0x47];

/// Segment file format version. Version 2 adds a sequence number and a
/// CRC32C to each record (#20); version 3 has 33-byte tagged keys. Older
/// files are not read.
pub const SEGMENT_VERSION: u32 = 3;

/// Bytes of a record frame before the payload: type, payload length, seq.
pub(crate) const FRAME_HEAD: usize = 1 + 4 + 8;

/// Largest payload the reader accepts (a bad length must not allocate GiBs).
pub(crate) const MAX_PAYLOAD: usize = 64 << 20;

/// Frame one record: type(u8) + payload_len(u32 LE) + seq(u64 LE) + payload +
/// crc32c(u32 LE) over all bytes before the CRC.
pub fn frame_record(record_type: u8, seq: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEAD + payload.len() + 4);
    out.push(record_type);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(payload);
    let crc = crc32c::crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Record type discriminants.
///
/// 0x01 (formerly DictEntry) is no longer used but reserved for legacy skip.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordType {
    RootUpdate     = 0x02,
    SnapshotRecord = 0x03,
    Op             = 0x10,
    EofMarker      = 0xFF,
}

impl RecordType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => None, // legacy DictEntry — caller should skip payload_len bytes
            0x02 => Some(RecordType::RootUpdate),
            0x03 => Some(RecordType::SnapshotRecord),
            0x10 => Some(RecordType::Op),
            0xFF => Some(RecordType::EofMarker),
            _ => None,
        }
    }
}

/// 16-byte segment file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    pub magic:      [u8; 4],
    pub version:    u32,
    pub segment_id: u64,
}

impl SegmentHeader {
    pub fn new(segment_id: u64) -> Self {
        SegmentHeader {
            magic: SEGMENT_MAGIC,
            version: SEGMENT_VERSION,
            segment_id,
        }
    }

    /// Serialize to 16 bytes.
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&self.magic);
        buf[4..8].copy_from_slice(&self.version.to_le_bytes());
        buf[8..16].copy_from_slice(&self.segment_id.to_le_bytes());
        buf
    }

    /// Parse from 16 bytes. Returns None if magic or version is wrong.
    pub fn from_bytes(buf: &[u8; 16]) -> Option<Self> {
        let magic: [u8; 4] = buf[0..4].try_into().ok()?;
        if magic != SEGMENT_MAGIC {
            return None;
        }
        let version = u32::from_le_bytes(buf[4..8].try_into().ok()?);
        if version != SEGMENT_VERSION {
            return None;
        }
        let segment_id = u64::from_le_bytes(buf[8..16].try_into().ok()?);
        Some(SegmentHeader { magic, version, segment_id })
    }
}

/// A logical entry in a segment file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentEntry {
    RootUpdate     { root: ObjectKey },
    SnapshotRecord { version: u64, root: ObjectKey, created_at: u64, name: Option<String> },
    /// A file system request since the last root (opaque bytes, #20).
    Op { bytes: Vec<u8> },
}

impl SegmentEntry {
    pub fn record_type(&self) -> RecordType {
        match self {
            SegmentEntry::RootUpdate     { .. } => RecordType::RootUpdate,
            SegmentEntry::SnapshotRecord { .. } => RecordType::SnapshotRecord,
            SegmentEntry::Op { .. } => RecordType::Op,
        }
    }

    /// Serialize payload bytes (does not include type byte or payload_len).
    pub fn payload_bytes(&self) -> Vec<u8> {
        match self {
            SegmentEntry::Op { bytes } => bytes.clone(),
            SegmentEntry::RootUpdate { root } => {
                root.0.to_vec()
            }
            SegmentEntry::SnapshotRecord { version, root, created_at, name } => {
                let name_bytes = name.as_deref().unwrap_or("").as_bytes();
                let name_len = name_bytes.len() as u32;
                let mut buf = Vec::with_capacity(SNAPSHOT_MIN + name_bytes.len());
                // version: u64 LE (8 bytes)
                buf.extend_from_slice(&version.to_le_bytes());
                // root: KEY_BYTES (tag + digest)
                buf.extend_from_slice(&root.0);
                // created_at: u64 LE (8 bytes)
                buf.extend_from_slice(&created_at.to_le_bytes());
                // name_len: u32 LE (4 bytes)
                buf.extend_from_slice(&name_len.to_le_bytes());
                // name bytes (0 or more)
                buf.extend_from_slice(name_bytes);
                buf
            }
        }
    }

    /// Parse a RootUpdate from a `KEY_BYTES`-byte payload.
    pub fn parse_root_update(payload: &[u8]) -> Option<Self> {
        if payload.len() < KEY_BYTES { return None; }
        let root = ObjectKey::read(payload, 0);
        Some(SegmentEntry::RootUpdate { root })
    }

    /// Parse a SnapshotRecord from a payload.
    ///
    /// Layout: version(8) + root(KEY_BYTES) + created_at(8) + name_len(4) + name_bytes(name_len)
    /// Minimum `SNAPSHOT_MIN` bytes (name_len = 0).
    pub fn parse_snapshot_record(payload: &[u8]) -> Option<Self> {
        if payload.len() < SNAPSHOT_MIN { return None; }
        let version = u64::from_le_bytes(payload[0..8].try_into().unwrap());
        let root = ObjectKey::read(payload, SNAPSHOT_ROOT);
        let created_at = u64::from_le_bytes(payload[SNAPSHOT_CREATED_AT..SNAPSHOT_NAME_LEN].try_into().unwrap());
        let name_len = u32::from_le_bytes(payload[SNAPSHOT_NAME_LEN..SNAPSHOT_MIN].try_into().unwrap()) as usize;
        if payload.len() < SNAPSHOT_MIN + name_len { return None; }
        let name = if name_len == 0 {
            None
        } else {
            let name_bytes = &payload[SNAPSHOT_MIN..SNAPSHOT_MIN + name_len];
            Some(String::from_utf8(name_bytes.to_vec()).ok()?)
        };
        Some(SegmentEntry::SnapshotRecord { version, root, created_at, name })
    }

    /// Convert this entry to a `SnapshotEntry` if it is a `SnapshotRecord`.
    pub fn as_snapshot_entry(&self) -> Option<SnapshotEntry> {
        match self {
            SegmentEntry::SnapshotRecord { version, root, created_at, name } => {
                Some(SnapshotEntry {
                    version: *version,
                    name: name.clone(),
                    root: *root,
                    created_at: *created_at,
                })
            }
            _ => None,
        }
    }
}
