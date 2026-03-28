//! InodeMeta binary serialization for SliceFS.
//!
//! Provides 56-byte little-endian pack/unpack of `InodeMeta`, plus
//! Dictionary-backed `intern_inode` and `load_inode` for CAS storage.
//!
//! Note: `blockset::StorageAdd` and `StorageGet` are private traits in blockset.
//! The only publicly-accessible type that implements them is `blockset::Dictionary`
//! (which is `BTreeMap<[u32;7], [[u32;8];2]>`). Therefore, `intern_inode` and
//! `load_inode` work directly with `blockset::Dictionary`.

use slicefs_traits::metadata::{InodeMeta, MetaError};
use slicefs_traits::digest::{Digest224, Digest256, from_digest224};
use blockset::{State, Tree, GetBytes, GetData, Dictionary};

/// Serialize `InodeMeta` to a fixed 56-byte little-endian buffer.
///
/// Layout (all little-endian):
///   [0..8]   ino        u64  (8 bytes)
///   [8..12]  mode       u32  (4 bytes)
///   [12..16] uid        u32  (4 bytes)
///   [16..20] gid        u32  (4 bytes)
///   [20..24] nlinks     u32  (4 bytes)
///   [24..32] size       u64  (8 bytes)
///   [32..40] mtime_sec  i64  (8 bytes)
///   [40..44] mtime_nsec u32  (4 bytes)
///   [44..52] ctime_sec  i64  (8 bytes)
///   [52..56] ctime_nsec u32  (4 bytes)
///   Total: 56 bytes
pub fn serialize_inode(meta: &InodeMeta) -> [u8; 56] {
    let mut buf = [0u8; 56];
    buf[0..8].copy_from_slice(&meta.ino.to_le_bytes());
    buf[8..12].copy_from_slice(&meta.mode.to_le_bytes());
    buf[12..16].copy_from_slice(&meta.uid.to_le_bytes());
    buf[16..20].copy_from_slice(&meta.gid.to_le_bytes());
    buf[20..24].copy_from_slice(&meta.nlinks.to_le_bytes());
    buf[24..32].copy_from_slice(&meta.size.to_le_bytes());
    buf[32..40].copy_from_slice(&meta.mtime_sec.to_le_bytes());
    buf[40..44].copy_from_slice(&meta.mtime_nsec.to_le_bytes());
    buf[44..52].copy_from_slice(&meta.ctime_sec.to_le_bytes());
    buf[52..56].copy_from_slice(&meta.ctime_nsec.to_le_bytes());
    buf
}

/// Deserialize `InodeMeta` from a 56-byte little-endian buffer.
///
/// Returns `MetaError::Corrupted` if `buf.len() != 56`.
pub fn deserialize_inode(buf: &[u8]) -> Result<InodeMeta, MetaError> {
    if buf.len() != 56 {
        return Err(MetaError::Corrupted(format!(
            "expected 56 bytes, got {}",
            buf.len()
        )));
    }
    Ok(InodeMeta {
        ino: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
        mode: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
        uid: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
        gid: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
        nlinks: u32::from_le_bytes(buf[20..24].try_into().unwrap()),
        size: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
        mtime_sec: i64::from_le_bytes(buf[32..40].try_into().unwrap()),
        mtime_nsec: u32::from_le_bytes(buf[40..44].try_into().unwrap()),
        ctime_sec: i64::from_le_bytes(buf[44..52].try_into().unwrap()),
        ctime_nsec: u32::from_le_bytes(buf[52..56].try_into().unwrap()),
    })
}

/// Serialize an inode and store it in a blockset Dictionary.
///
/// Returns the `Digest224` key that can later be passed to `load_inode`.
///
/// Note: Uses `blockset::Dictionary` directly because `StorageAdd` is a private
/// blockset trait; `Dictionary` is the only publicly-accessible implementation.
pub fn intern_inode(dict: &mut Dictionary, meta: &InodeMeta) -> Digest224 {
    let bytes = serialize_inode(meta);
    State::push_all(dict, &bytes)
}

/// Retrieve and deserialize an inode from a blockset Dictionary.
///
/// Returns `MetaError::Corrupted` if the key is not found or the data is wrong length.
pub fn load_inode(dict: &Dictionary, key: &Digest224) -> Result<InodeMeta, MetaError> {
    let digest256: Digest256 = from_digest224(key);
    let get_data = GetData::new(dict, &digest256);
    let bytes: Vec<u8> = GetBytes::new(get_data).collect();
    deserialize_inode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use slicefs_traits::metadata::InodeMeta;
    use proptest::prelude::*;

    // Proptest strategy for arbitrary InodeMeta
    prop_compose! {
        fn arb_inode_meta()(
            ino in any::<u64>(),
            mode in any::<u32>(),
            uid in any::<u32>(),
            gid in any::<u32>(),
            nlinks in any::<u32>(),
            size in any::<u64>(),
            mtime_sec in any::<i64>(),
            mtime_nsec in any::<u32>(),
            ctime_sec in any::<i64>(),
            ctime_nsec in any::<u32>(),
        ) -> InodeMeta {
            InodeMeta { ino, mode, uid, gid, nlinks, size, mtime_sec, mtime_nsec, ctime_sec, ctime_nsec }
        }
    }

    proptest! {
        #[test]
        fn prop_round_trip(meta in arb_inode_meta()) {
            let bytes = serialize_inode(&meta);
            prop_assert_eq!(bytes.len(), 56);
            let recovered = deserialize_inode(&bytes).expect("deserialize failed");
            prop_assert_eq!(meta, recovered);
        }
    }

    fn sample_meta() -> InodeMeta {
        InodeMeta {
            ino: 42,
            mode: 0o755,
            uid: 1000,
            gid: 1000,
            nlinks: 2,
            size: 4096,
            mtime_sec: 1711497600,
            mtime_nsec: 123456789,
            ctime_sec: 1711497601,
            ctime_nsec: 987654321,
        }
    }

    #[test]
    fn test_serialize_size_is_56() {
        let meta = sample_meta();
        let bytes = serialize_inode(&meta);
        assert_eq!(bytes.len(), 56);
    }

    #[test]
    fn test_round_trip() {
        let original = sample_meta();
        let bytes = serialize_inode(&original);
        let recovered = deserialize_inode(&bytes).expect("deserialize failed");
        assert_eq!(original, recovered);
    }

    #[test]
    fn test_round_trip_max_values() {
        let meta = InodeMeta {
            ino: u64::MAX,
            mode: u32::MAX,
            uid: u32::MAX,
            gid: u32::MAX,
            nlinks: u32::MAX,
            size: u64::MAX,
            mtime_sec: i64::MIN,
            mtime_nsec: u32::MAX,
            ctime_sec: i64::MAX,
            ctime_nsec: u32::MAX,
        };
        let bytes = serialize_inode(&meta);
        assert_eq!(bytes.len(), 56);
        let recovered = deserialize_inode(&bytes).expect("deserialize failed");
        assert_eq!(meta, recovered);
    }

    #[test]
    fn test_round_trip_zero_ino() {
        let meta = InodeMeta {
            ino: 0,
            mode: 0,
            uid: 0,
            gid: 0,
            nlinks: 0,
            size: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
        };
        let bytes = serialize_inode(&meta);
        let recovered = deserialize_inode(&bytes).expect("deserialize failed");
        assert_eq!(meta, recovered);
    }

    #[test]
    fn test_deserialize_rejects_wrong_length() {
        let short = [0u8; 55];
        assert!(deserialize_inode(&short).is_err());

        let long = [0u8; 57];
        assert!(deserialize_inode(&long).is_err());

        let empty: &[u8] = &[];
        assert!(deserialize_inode(empty).is_err());
    }

    #[test]
    fn test_new_directory_nlinks() {
        let meta = InodeMeta::new_directory(1, 0, 0, 0o755);
        assert_eq!(meta.nlinks, 2);
        assert_eq!(meta.size, 0);
    }

    #[test]
    fn test_new_file_nlinks() {
        let meta = InodeMeta::new_file(2, 1000, 1000, 0o644);
        assert_eq!(meta.nlinks, 1);
        assert_eq!(meta.size, 0);
    }

    #[test]
    fn test_intern_and_load_roundtrip() {
        let mut dict = Dictionary::default();
        let original = sample_meta();
        let key = intern_inode(&mut dict, &original);
        let recovered = load_inode(&dict, &key).expect("load_inode failed");
        assert_eq!(original, recovered);
    }
}
