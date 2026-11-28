//! Directory entries: one entry per name in the `dir_data` prolly tree
//! (store format 8).
//!
//! - **Key:** the directory inode number (8 bytes, big endian), then the
//!   name bytes (UTF-8, 1 to 255 bytes, so a key is at most 263 bytes). The
//!   names of one directory are the key range `dir BE .. (dir + 1) BE`
//!   ([`dir_range`]), in name byte order.
//! - **Value:** 9 bytes: the child inode number (8 bytes, little endian),
//!   then the file type of the child ([`kind_of`]: the Linux `DT_*` number,
//!   0 if the child had no inode when it was linked).
//!
//! Every directory has the entries `.` (itself) and `..` (its parent), so
//! an empty directory has two entries and `is_dir` is one read of `.`. A
//! hard link is one more entry: another name with the same child.

use slicefs_traits::metadata::MetaError;

use crate::pmap::{shard_of, TreeKey, TreeValue};

/// Type bits of a mode (`S_IFMT`).
const S_IFMT: u32 = 0o170_000;

/// The type byte of a directory (`DT_DIR`).
pub(crate) const KIND_DIR: u8 = 4;

/// The key of one directory entry: directory inode number and name bytes.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct DirKey {
    pub dir: u64,
    pub name: Box<[u8]>,
}

impl DirKey {
    /// The key of the encoded tree key `k` (at least 9 bytes, else corrupt).
    pub(crate) fn from_encoded(k: &[u8]) -> Result<Self, MetaError> {
        if k.len() < 9 {
            return Err(MetaError::Corrupted(format!("dir_data tree: key of {} bytes", k.len())));
        }
        Ok(DirKey { dir: u64::from_be_bytes(k[..8].try_into().unwrap()), name: k[8..].into() })
    }
}

/// The names of one directory share its 8-byte prefix, so they are in one
/// overlay shard (a listing of one directory reads one shard).
impl TreeKey for DirKey {
    fn encode(&self) -> Vec<u8> {
        let mut k = Vec::with_capacity(8 + self.name.len());
        k.extend_from_slice(&self.dir.to_be_bytes());
        k.extend_from_slice(&self.name);
        k
    }

    fn shard(&self) -> usize {
        shard_of(self.dir)
    }
}

/// The key of the entry `name` of directory `dir`.
pub(crate) fn entry_key(dir: u64, name: &str) -> DirKey {
    DirKey { dir, name: name.as_bytes().into() }
}

/// The tree key range of the entries of `dir`: `dir BE .. (dir + 1) BE`
/// (no end for `u64::MAX`).
pub(crate) fn dir_range(dir: u64) -> (Vec<u8>, Option<Vec<u8>>) {
    (dir.to_be_bytes().to_vec(), dir.checked_add(1).map(|n| n.to_be_bytes().to_vec()))
}

/// The name of the encoded tree key `k`: at least 9 bytes, valid UTF-8
/// after the 8-byte directory number, else corrupt.
pub(crate) fn name_of(k: &[u8]) -> Result<&str, MetaError> {
    if k.len() < 9 {
        return Err(MetaError::Corrupted(format!("dir_data tree: key of {} bytes", k.len())));
    }
    std::str::from_utf8(&k[8..]).map_err(|_| MetaError::Corrupted("dir_data tree: name not UTF-8".into()))
}

/// The type byte of a child with mode `mode`: `(mode & S_IFMT) >> 12`, the
/// Linux `DT_*` number (4 a directory, 8 a regular file, 10 a symlink).
pub(crate) fn kind_of(mode: u32) -> u8 {
    ((mode & S_IFMT) >> 12) as u8
}

/// The value of one directory entry: the child and its type byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DirValue {
    pub ino: u64,
    pub kind: u8,
}

impl TreeValue for DirValue {
    fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(9);
        b.extend_from_slice(&self.ino.to_le_bytes());
        b.push(self.kind);
        b
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != 9 {
            return Err(format!("entry value of {} bytes, not 9", bytes.len()));
        }
        Ok(DirValue { ino: u64::from_le_bytes(bytes[..8].try_into().unwrap()), kind: bytes[8] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_sort_by_directory_then_name_bytes() {
        let k = |d, n| entry_key(d, n).encode();
        assert!(k(5, "a") < k(5, "b"));
        assert!(k(5, "zz") < k(6, "!"), "the directory number first");
        assert!(k(5, "+x") < k(5, "."), "a name may sort before the dot entry");
        let (s, e) = dir_range(5);
        assert!(k(5, ".") >= s && k(5, ".") < e.unwrap());
        assert_eq!(dir_range(u64::MAX).1, None);
        assert_eq!(DirKey::from_encoded(&k(7, "name")).unwrap(), entry_key(7, "name"));
        assert!(DirKey::from_encoded(&[0u8; 8]).is_err(), "no name");
        assert_eq!(name_of(&k(7, "name")).unwrap(), "name");
        assert!(name_of(&[0, 0, 0, 0, 0, 0, 0, 7, 0xff]).is_err(), "not UTF-8");
    }

    #[test]
    fn a_value_is_nine_bytes() {
        let v = DirValue { ino: 0x0102_0304_0506_0708, kind: 8 };
        let b = v.encode();
        assert_eq!(b, [8, 7, 6, 5, 4, 3, 2, 1, 8]);
        assert_eq!(DirValue::decode(&b).unwrap(), v);
        for n in [0usize, 8, 10, 33] {
            assert!(DirValue::decode(&vec![0u8; n]).is_err(), "{n} bytes");
        }
    }

    #[test]
    fn the_type_byte_is_the_dt_number() {
        assert_eq!(kind_of(0o040_755), KIND_DIR);
        assert_eq!(kind_of(0o100_644), 8);
        assert_eq!(kind_of(0o120_777), 10);
        assert_eq!(kind_of(0o010_644), 1);
        assert_eq!(kind_of(0o140_755), 12);
        assert_eq!(kind_of(0o644), 0);
    }
}
