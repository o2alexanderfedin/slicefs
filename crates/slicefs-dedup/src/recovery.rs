//! Rebuild the redb index from CAS shards (ARCHITECTURE §9.2; I7).
//!
//! Operator-facing recovery path: when the redb file is lost or judged
//! untrusted, the canonical state lives in the CAS shard tree. Walking
//! `cas_root/{00..ff}/{52 hex}` yields the authoritative key set; we
//! bulk-load it into a fresh redb at `index.redb.tmp`, fsync, then
//! `rename(2)` over `index.redb` and fsync the parent directory.
//!
//! Idempotency (I7): the procedure is restartable. A pre-existing
//! `index.redb.tmp` from a previous crashed run is removed up-front.
//! Re-running after success rebuilds an equivalent index — same key
//! set in, same key set out.

use crate::config::DedupIndexConfig;
use crate::error::DedupIndexError;
use crate::paths::DedupRoot;
use crate::platform::fsync_parent_dir;
use crate::redb_dedup_index::DEDUP_TABLE;
use redb::{Database, Durability};

/// Walk every CAS shard, parse 28-byte content addresses from filenames,
/// bulk-load them into a fresh redb at `<dedup_root>/index.redb.tmp`,
/// then atomically rename onto `index.redb` and fsync the parent dir.
///
/// Filenames inside `cas_root/<shard>/` must be 54 lowercase hex chars
/// (the remaining 27 bytes after the 1-byte shard prefix). Anything
/// else — `.tmp` in-flight CAS writes, junk files, wrong length —
/// is silently skipped: CAS is the source of truth for what counts as
/// a block, not us.
pub fn rebuild_from_cas(config: DedupIndexConfig) -> Result<(), DedupIndexError> {
    let root = DedupRoot::new(&config.dedup_root);
    std::fs::create_dir_all(root.base())?;
    let tmp = root.base().join("index.redb.tmp");
    if tmp.exists() {
        std::fs::remove_file(&tmp)?;
    }

    let db = Database::builder()
        .set_cache_size(config.redb_cache_bytes)
        .create(&tmp)?;
    let mut txn = db.begin_write()?;
    let _ = txn.set_durability(Durability::Immediate);
    {
        let mut t = txn.open_table(DEDUP_TABLE)?;
        for shard in 0u8..=255 {
            let shard_dir = config.cas_root.join(format!("{:02x}", shard));
            if !shard_dir.exists() {
                continue;
            }
            for entry in std::fs::read_dir(&shard_dir)? {
                let entry = entry?;
                let name = entry.file_name();
                let s = match name.to_str() {
                    Some(s) => s,
                    None => continue,
                };
                if s.ends_with(".tmp") {
                    continue;
                }
                // 28 bytes total = 56 hex chars; first 2 are the shard
                // prefix encoded in the directory name, leaving 54 in
                // the file name.
                if s.len() != 54 {
                    continue;
                }
                let mut h = [0u8; 28];
                h[0] = shard;
                if !decode_hex_into(s, &mut h[1..]) {
                    continue;
                }
                t.insert(&h, ())?;
            }
        }
    }
    txn.commit()?;
    drop(db);

    std::fs::rename(&tmp, root.redb())?;
    fsync_parent_dir(root.base())?;
    Ok(())
}

/// Decode a lowercase hex string into a fixed-size byte slice. Returns
/// `false` if the string length is wrong or any character is not a
/// valid hex digit. No allocations.
fn decode_hex_into(s: &str, out: &mut [u8]) -> bool {
    if s.len() != out.len() * 2 {
        return false;
    }
    let bytes = s.as_bytes();
    for (i, byte) in out.iter_mut().enumerate() {
        let h = match (bytes[2 * i] as char).to_digit(16) {
            Some(v) => v,
            None => return false,
        };
        let l = match (bytes[2 * i + 1] as char).to_digit(16) {
            Some(v) => v,
            None => return false,
        };
        *byte = ((h << 4) | l) as u8;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_hex_into_round_trips() {
        let mut out = [0u8; 4];
        assert!(decode_hex_into("00ff10ab", &mut out));
        assert_eq!(out, [0x00, 0xff, 0x10, 0xab]);
    }

    #[test]
    fn decode_hex_into_rejects_bad_length() {
        let mut out = [0u8; 4];
        assert!(!decode_hex_into("00ff10a", &mut out));
        assert!(!decode_hex_into("00ff10abcd", &mut out));
    }

    #[test]
    fn decode_hex_into_rejects_non_hex() {
        let mut out = [0u8; 2];
        assert!(!decode_hex_into("00zz", &mut out));
        assert!(!decode_hex_into("g0ab", &mut out));
    }
}
