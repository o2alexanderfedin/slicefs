// Wired up by F1/J1/J2; until then keep items dead-code-friendly.
#![allow(dead_code)]

use crate::error::DedupIndexError;
use crate::paths::DedupRoot;
use crate::platform::{durable_sync, fsync_parent_dir};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};

pub const BLOOM_MAGIC_START: [u8; 8] = *b"SLDXBL01";
pub const BLOOM_MAGIC_END:   [u8; 8] = *b"BL01ENDX";
pub const BLOOM_VERSION: u32 = 1;
pub const HEADER_LEN:    usize = 64;
pub const FOOTER_LEN:    usize = 16;

#[derive(Debug, Clone)]
pub struct BloomSnapshotMeta {
    pub bloom_capacity: u64,
    pub bloom_fpr_bits: f64,
    pub entries_at_snapshot: u64,
    pub redb_hwm_at_snapshot: u64,
}

fn now_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64
}

pub fn write_atomic(
    root: &DedupRoot,
    meta: &BloomSnapshotMeta,
    payload: &[u8],
) -> Result<(), DedupIndexError> {
    let tmp = root.bloom_tmp();
    std::fs::create_dir_all(root.base())?;

    let mut header = [0u8; HEADER_LEN];
    header[..8].copy_from_slice(&BLOOM_MAGIC_START);
    header[8..12].copy_from_slice(&BLOOM_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&0u32.to_le_bytes()); // flags
    header[16..24].copy_from_slice(&now_micros().to_le_bytes());
    header[24..32].copy_from_slice(&meta.bloom_capacity.to_le_bytes());
    header[32..40].copy_from_slice(&meta.bloom_fpr_bits.to_le_bytes());
    header[40..48].copy_from_slice(&meta.entries_at_snapshot.to_le_bytes());
    header[48..56].copy_from_slice(&meta.redb_hwm_at_snapshot.to_le_bytes());

    let payload_xxh3 = xxhash_rust::xxh3::xxh3_128(payload);
    let xxh3_lo = payload_xxh3 as u32;
    let xxh3_hi = (payload_xxh3 >> 32) as u64;
    header[56..60].copy_from_slice(&xxh3_lo.to_le_bytes());

    let header_crc = crc32c::crc32c(&header[..60]);
    header[60..64].copy_from_slice(&header_crc.to_le_bytes());

    let mut footer = [0u8; FOOTER_LEN];
    footer[..8].copy_from_slice(&xxh3_hi.to_le_bytes());
    footer[8..16].copy_from_slice(&BLOOM_MAGIC_END);

    let mut f = OpenOptions::new().create(true).truncate(true).write(true).open(&tmp)?;
    f.write_all(&header)?;
    f.write_all(payload)?;
    f.write_all(&footer)?;
    durable_sync(&f)?;
    drop(f);

    std::fs::rename(&tmp, root.bloom())?;
    fsync_parent_dir(root.base())?;
    Ok(())
}

pub fn load(root: &DedupRoot) -> Result<(BloomSnapshotMeta, Vec<u8>), DedupIndexError> {
    let mut f = File::open(root.bloom())?;
    let mut header = [0u8; HEADER_LEN];
    f.read_exact(&mut header)?;

    if header[..8] != BLOOM_MAGIC_START {
        return Err(DedupIndexError::BloomCorrupt("magic_start mismatch"));
    }
    let header_crc_stored = u32::from_le_bytes(header[60..64].try_into().unwrap());
    let header_crc_calc   = crc32c::crc32c(&header[..60]);
    if header_crc_stored != header_crc_calc {
        return Err(DedupIndexError::BloomCorrupt("header crc32c mismatch"));
    }

    let total = std::fs::metadata(root.bloom())?.len() as usize;
    if total < HEADER_LEN + FOOTER_LEN {
        return Err(DedupIndexError::BloomCorrupt("truncated"));
    }
    let payload_len = total - HEADER_LEN - FOOTER_LEN;
    let mut payload = vec![0u8; payload_len];
    f.read_exact(&mut payload)?;

    let mut footer = [0u8; FOOTER_LEN];
    f.read_exact(&mut footer)?;
    if footer[8..16] != BLOOM_MAGIC_END {
        return Err(DedupIndexError::BloomCorrupt("magic_end mismatch"));
    }

    let xxh3_lo = u32::from_le_bytes(header[56..60].try_into().unwrap());
    let xxh3_hi = u64::from_le_bytes(footer[..8].try_into().unwrap());
    let calc = xxhash_rust::xxh3::xxh3_128(&payload);
    let calc_lo = calc as u32;
    let calc_hi = (calc >> 32) as u64;
    if xxh3_lo != calc_lo || xxh3_hi != calc_hi {
        return Err(DedupIndexError::BloomCorrupt("payload xxh3-128 mismatch"));
    }

    let meta = BloomSnapshotMeta {
        bloom_capacity:       u64::from_le_bytes(header[24..32].try_into().unwrap()),
        bloom_fpr_bits:       f64::from_le_bytes(header[32..40].try_into().unwrap()),
        entries_at_snapshot:  u64::from_le_bytes(header[40..48].try_into().unwrap()),
        redb_hwm_at_snapshot: u64::from_le_bytes(header[48..56].try_into().unwrap()),
    };
    Ok((meta, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r() -> (tempfile::TempDir, DedupRoot) {
        let td = tempfile::tempdir().unwrap();
        let r = DedupRoot::new(td.path().join("d"));
        std::fs::create_dir_all(r.base()).unwrap();
        (td, r)
    }

    fn meta() -> BloomSnapshotMeta {
        BloomSnapshotMeta {
            bloom_capacity: 1_000_000,
            bloom_fpr_bits: 0.01,
            entries_at_snapshot: 42,
            redb_hwm_at_snapshot: 99,
        }
    }

    #[test]
    fn roundtrip_small_payload() {
        let (_g, r) = r();
        let payload = b"hello world".repeat(1000);
        write_atomic(&r, &meta(), &payload).unwrap();
        let (got_meta, got_payload) = load(&r).unwrap();
        assert_eq!(got_payload, payload);
        assert_eq!(got_meta.entries_at_snapshot, 42);
    }

    #[test]
    fn flipped_payload_byte_is_caught_by_xxh3() {
        let (_g, r) = r();
        let payload = vec![0xAB; 8192];
        write_atomic(&r, &meta(), &payload).unwrap();

        let mut bytes = std::fs::read(r.bloom()).unwrap();
        // Flip a byte deep in the payload (offset 4096).
        bytes[HEADER_LEN + 4096] ^= 0x80;
        std::fs::write(r.bloom(), &bytes).unwrap();

        let err = load(&r).unwrap_err();
        match err {
            DedupIndexError::BloomCorrupt(s) => assert!(s.contains("xxh3")),
            other => panic!("expected BloomCorrupt(xxh3), got {other:?}"),
        }
    }

    #[test]
    fn flipped_header_byte_is_caught_by_crc32c() {
        let (_g, r) = r();
        write_atomic(&r, &meta(), &vec![0u8; 1024]).unwrap();
        let mut bytes = std::fs::read(r.bloom()).unwrap();
        bytes[20] ^= 0xFF; // flip a byte inside the header
        std::fs::write(r.bloom(), &bytes).unwrap();
        let err = load(&r).unwrap_err();
        match err {
            DedupIndexError::BloomCorrupt(s) => assert!(s.contains("crc32c") || s.contains("magic")),
            other => panic!("expected BloomCorrupt, got {other:?}"),
        }
    }

    #[test]
    fn missing_end_magic_is_caught() {
        let (_g, r) = r();
        write_atomic(&r, &meta(), &vec![0xAB; 256]).unwrap();
        let mut bytes = std::fs::read(r.bloom()).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0xFF;
        std::fs::write(r.bloom(), &bytes).unwrap();
        let err = load(&r).unwrap_err();
        assert!(matches!(err, DedupIndexError::BloomCorrupt(_)));
    }
}
