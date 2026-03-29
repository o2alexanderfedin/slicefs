//! Segment file format for SliceFS metadata persistence.
//!
//! A segment file is an append-only log of records. Records are framed as:
//!   record_type: u8
//!   payload_len: u32 LE
//!   payload:     [u8; payload_len]
//!
//! The file begins with a 16-byte header:
//!   magic:      [u8; 4]  = [0x53, 0x4C, 0x53, 0x47] ("SLSG")
//!   version:    u32 LE   = 1
//!   segment_id: u64 LE
//!
//! DictEntry payload: 92 bytes — Digest224 as 7×u32 LE (28 bytes) + Branches as 2×8×u32 LE (64 bytes)
//! RootUpdate payload: 28 bytes — Digest224 as 7×u32 LE
//! SnapshotRecord payload: 48+ bytes — version(8) + root(28) + created_at(8) + name_len(4) + name_bytes(variable)
//! EofMarker: no payload (payload_len = 0), terminates iteration

pub mod writer;
pub mod reader;
pub mod compaction;

pub use writer::SegmentWriter;
pub use reader::SegmentReader;

use std::path::Path;
use slicefs_traits::digest::{Branches, Digest224};
use blockset::Dictionary;
use thiserror::Error;

use crate::snapshot::SnapshotEntry;

/// Errors returned by segment-level operations.
#[derive(Debug, Error)]
pub enum SegmentError {
    #[error("segment I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("missing root.bin in legacy store at {0}")]
    MissingRootBin(String),
    #[error("invalid root.bin: expected 28 bytes, got {0}")]
    InvalidRootBin(usize),
    #[error("migration failed: {0}")]
    Migration(String),
}

/// Load all segment files from `segments_dir`, replay them into a `Dictionary`,
/// and return the last `RootUpdate` digest seen along with all `SnapshotRecord` entries.
///
/// Segment files are read in ascending order by segment_id (encoded in the filename
/// as `segment-{id:06}.seg`). DictEntry records are inserted into the Dictionary;
/// RootUpdate records update the `last_root` tracker;
/// SnapshotRecord entries are collected into the returned Vec.
///
/// Returns `(dictionary, Option<last_root_digest>, Vec<SnapshotEntry>)`.
pub fn load_store_from_segments(
    segments_dir: &Path,
) -> Result<(Dictionary, Option<Digest224>, Vec<SnapshotEntry>), SegmentError> {
    let mut dict = Dictionary::new();
    let mut last_root: Option<Digest224> = None;
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

    // Sort lexicographically — segment-000001.seg < segment-000002.seg etc.
    seg_paths.sort();

    for path in &seg_paths {
        let reader = SegmentReader::open(path)?;
        for entry in reader {
            match entry {
                SegmentEntry::DictEntry { key, branches } => {
                    dict.insert(key, branches);
                }
                SegmentEntry::RootUpdate { root } => {
                    last_root = Some(root);
                }
                ref snap @ SegmentEntry::SnapshotRecord { .. } => {
                    if let Some(snap_entry) = snap.as_snapshot_entry() {
                        snapshots.push(snap_entry);
                    }
                }
            }
        }
    }

    Ok((dict, last_root, snapshots))
}

/// Migrate a legacy store (dictionary.bin + root.bin) to segment format.
///
/// Reads `<store_path>/root.bin` and `<store_path>/dictionary.bin`, writes all
/// entries into a single segment file at `<store_path>/segments/segment-000001.seg`,
/// then removes `dictionary.bin` and `root.bin`.
pub fn migrate_legacy_store(store_path: &Path) -> Result<(), SegmentError> {
    use crate::store::deserialize_dictionary;

    let root_bytes = std::fs::read(store_path.join("root.bin"))
        .map_err(|_| SegmentError::MissingRootBin(store_path.display().to_string()))?;
    if root_bytes.len() != 28 {
        return Err(SegmentError::InvalidRootBin(root_bytes.len()));
    }
    let mut root: Digest224 = [0u32; 7];
    for (i, word) in root.iter_mut().enumerate() {
        *word = u32::from_le_bytes(root_bytes[i * 4..i * 4 + 4].try_into().unwrap());
    }

    let dict_bytes = std::fs::read(store_path.join("dictionary.bin"))
        .map_err(|e| SegmentError::Migration(format!("failed to read dictionary.bin: {}", e)))?;
    let dict = deserialize_dictionary(&dict_bytes)
        .map_err(|e| SegmentError::Migration(format!("failed to deserialize dictionary.bin: {}", e)))?;

    // Create segments directory
    let segs_dir = store_path.join("segments");
    std::fs::create_dir_all(&segs_dir)?;

    // Write all dict entries + root update into segment-000001.seg
    let seg_path = segs_dir.join("segment-000001.seg");
    let mut writer = SegmentWriter::new(&seg_path, 1)?;
    for (key, branches) in &dict {
        writer.write_entry(&SegmentEntry::DictEntry { key: *key, branches: *branches })?;
    }
    writer.write_entry(&SegmentEntry::RootUpdate { root })?;
    writer.close()?;

    // Remove legacy files
    let _ = std::fs::remove_file(store_path.join("dictionary.bin"));
    let _ = std::fs::remove_file(store_path.join("root.bin"));

    Ok(())
}

/// Magic bytes identifying a SliceFS segment file: "SLSG"
pub const SEGMENT_MAGIC: [u8; 4] = [0x53, 0x4C, 0x53, 0x47];

/// Segment file format version.
pub const SEGMENT_VERSION: u32 = 1;

/// Record type discriminants.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordType {
    DictEntry      = 0x01,
    RootUpdate     = 0x02,
    SnapshotRecord = 0x03,
    EofMarker      = 0xFF,
}

impl RecordType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(RecordType::DictEntry),
            0x02 => Some(RecordType::RootUpdate),
            0x03 => Some(RecordType::SnapshotRecord),
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
    DictEntry      { key: Digest224, branches: Branches },
    RootUpdate     { root: Digest224 },
    SnapshotRecord { version: u64, root: Digest224, created_at: u64, name: Option<String> },
}

impl SegmentEntry {
    pub fn record_type(&self) -> RecordType {
        match self {
            SegmentEntry::DictEntry      { .. } => RecordType::DictEntry,
            SegmentEntry::RootUpdate     { .. } => RecordType::RootUpdate,
            SegmentEntry::SnapshotRecord { .. } => RecordType::SnapshotRecord,
        }
    }

    /// Serialize payload bytes (does not include type byte or payload_len).
    pub fn payload_bytes(&self) -> Vec<u8> {
        match self {
            SegmentEntry::DictEntry { key, branches } => {
                let mut buf = Vec::with_capacity(92);
                for v in key {
                    buf.extend_from_slice(&v.to_le_bytes());
                }
                for digest in branches {
                    for v in digest {
                        buf.extend_from_slice(&v.to_le_bytes());
                    }
                }
                buf
            }
            SegmentEntry::RootUpdate { root } => {
                let mut buf = Vec::with_capacity(28);
                for v in root {
                    buf.extend_from_slice(&v.to_le_bytes());
                }
                buf
            }
            SegmentEntry::SnapshotRecord { version, root, created_at, name } => {
                let name_bytes = name.as_deref().unwrap_or("").as_bytes();
                let name_len = name_bytes.len() as u32;
                let mut buf = Vec::with_capacity(48 + name_bytes.len());
                // version: u64 LE (8 bytes)
                buf.extend_from_slice(&version.to_le_bytes());
                // root: 7×u32 LE (28 bytes)
                for v in root {
                    buf.extend_from_slice(&v.to_le_bytes());
                }
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

    /// Parse a DictEntry from a 92-byte payload.
    pub fn parse_dict_entry(payload: &[u8]) -> Option<Self> {
        if payload.len() < 92 { return None; }
        let key: Digest224 = std::array::from_fn(|i| {
            u32::from_le_bytes(payload[i*4..i*4+4].try_into().unwrap())
        });
        let branches: Branches = std::array::from_fn(|d| {
            std::array::from_fn(|i| {
                let off = 28 + d * 32 + i * 4;
                u32::from_le_bytes(payload[off..off+4].try_into().unwrap())
            })
        });
        Some(SegmentEntry::DictEntry { key, branches })
    }

    /// Parse a RootUpdate from a 28-byte payload.
    pub fn parse_root_update(payload: &[u8]) -> Option<Self> {
        if payload.len() < 28 { return None; }
        let root: Digest224 = std::array::from_fn(|i| {
            u32::from_le_bytes(payload[i*4..i*4+4].try_into().unwrap())
        });
        Some(SegmentEntry::RootUpdate { root })
    }

    /// Parse a SnapshotRecord from a payload.
    ///
    /// Layout: version(8) + root(28) + created_at(8) + name_len(4) + name_bytes(name_len)
    /// Minimum 48 bytes (name_len = 0).
    pub fn parse_snapshot_record(payload: &[u8]) -> Option<Self> {
        if payload.len() < 48 { return None; }
        let version = u64::from_le_bytes(payload[0..8].try_into().unwrap());
        let root: Digest224 = std::array::from_fn(|i| {
            u32::from_le_bytes(payload[8 + i*4..8 + i*4 + 4].try_into().unwrap())
        });
        let created_at = u64::from_le_bytes(payload[36..44].try_into().unwrap());
        let name_len = u32::from_le_bytes(payload[44..48].try_into().unwrap()) as usize;
        if payload.len() < 48 + name_len { return None; }
        let name = if name_len == 0 {
            None
        } else {
            let name_bytes = &payload[48..48 + name_len];
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
