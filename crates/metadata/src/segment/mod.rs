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
//! EofMarker: no payload (payload_len = 0), terminates iteration

pub mod writer;
pub mod reader;

pub use writer::SegmentWriter;
pub use reader::SegmentReader;

use slicefs_traits::digest::{Branches, Digest224};

/// Magic bytes identifying a SliceFS segment file: "SLSG"
pub const SEGMENT_MAGIC: [u8; 4] = [0x53, 0x4C, 0x53, 0x47];

/// Segment file format version.
pub const SEGMENT_VERSION: u32 = 1;

/// Record type discriminants.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordType {
    DictEntry  = 0x01,
    RootUpdate = 0x02,
    EofMarker  = 0xFF,
}

impl RecordType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(RecordType::DictEntry),
            0x02 => Some(RecordType::RootUpdate),
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
    DictEntry  { key: Digest224, branches: Branches },
    RootUpdate { root: Digest224 },
}

impl SegmentEntry {
    pub fn record_type(&self) -> RecordType {
        match self {
            SegmentEntry::DictEntry  { .. } => RecordType::DictEntry,
            SegmentEntry::RootUpdate { .. } => RecordType::RootUpdate,
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
}
