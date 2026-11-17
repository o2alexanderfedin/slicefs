//! Snapshot table types for SliceFS metadata.
//!
//! A `SnapshotEntry` represents an immutable root pointer captured at a point in time.
//! Snapshots are persisted as `SegmentEntry::SnapshotRecord` entries in WAL segments,
//! giving them crash-safe durability for free.

/// A snapshot: an immutable pointer to a committed filesystem root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    /// Auto-incremented version number (starts at 1).
    pub version: u64,
    /// Optional human-readable name/tag.
    pub name: Option<String>,
    /// Root `Digest256` captured at snapshot time.
    pub root: slicefs_traits::digest::Digest256,
    /// Creation time as Unix seconds (UTC).
    pub created_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::{SegmentEntry, SNAPSHOT_MIN, SNAPSHOT_NAME_LEN};
    use slicefs_traits::digest::Digest256;

    fn make_root(v: u32) -> Digest256 {
        [v; 8]
    }

    #[test]
    fn test_snapshot_entry_fields() {
        let root = make_root(42);
        let snap = SnapshotEntry {
            version: 1,
            name: Some("release-1.0".to_string()),
            root,
            created_at: 1_700_000_000,
        };
        assert_eq!(snap.version, 1);
        assert_eq!(snap.name.as_deref(), Some("release-1.0"));
        assert_eq!(snap.root, root);
        assert_eq!(snap.created_at, 1_700_000_000);
    }

    #[test]
    fn test_snapshot_entry_no_name() {
        let root = make_root(7);
        let snap = SnapshotEntry {
            version: 3,
            name: None,
            root,
            created_at: 0,
        };
        assert!(snap.name.is_none());
    }

    // ── SnapshotRecord round-trip tests ─────────────────────────────────────

    #[test]
    fn test_snapshot_record_round_trip_no_name() {
        let root = make_root(99);
        let entry = SegmentEntry::SnapshotRecord {
            version: 1,
            root,
            created_at: 12345,
            name: None,
        };
        let payload = entry.payload_bytes();
        // version(8) + root(32) + created_at(8) + name_len(4) = 52 bytes
        assert_eq!(payload.len(), SNAPSHOT_MIN, "no-name payload must be SNAPSHOT_MIN bytes");

        let parsed = SegmentEntry::parse_snapshot_record(&payload)
            .expect("parse_snapshot_record must succeed");
        assert_eq!(parsed, entry, "round-trip must be identity");
    }

    #[test]
    fn test_snapshot_record_round_trip_with_name() {
        let root = make_root(5);
        let entry = SegmentEntry::SnapshotRecord {
            version: 7,
            root,
            created_at: 9999,
            name: Some("test".to_string()),
        };
        let payload = entry.payload_bytes();
        // SNAPSHOT_MIN + len("test")
        assert_eq!(payload.len(), SNAPSHOT_MIN + 4, "named payload must be SNAPSHOT_MIN + name_len bytes");

        let parsed = SegmentEntry::parse_snapshot_record(&payload)
            .expect("parse_snapshot_record with name must succeed");
        assert_eq!(parsed, entry, "round-trip with name must be identity");
    }

    #[test]
    fn test_snapshot_record_name_len_zero_for_none() {
        let root = make_root(1);
        let entry = SegmentEntry::SnapshotRecord {
            version: 1,
            root,
            created_at: 0,
            name: None,
        };
        let payload = entry.payload_bytes();
        let name_len = u32::from_le_bytes(payload[SNAPSHOT_NAME_LEN..SNAPSHOT_MIN].try_into().unwrap());
        assert_eq!(name_len, 0, "name_len must be 0 for name=None");
    }

    #[test]
    fn test_snapshot_record_parse_returns_none_on_short_payload() {
        let short = [0u8; 10];
        assert!(
            SegmentEntry::parse_snapshot_record(&short).is_none(),
            "must return None for payload shorter than SNAPSHOT_MIN bytes"
        );
    }
}
