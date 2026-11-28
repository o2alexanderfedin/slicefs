//! Extended attributes (xattrs): one entry per name in the `xattr_data`
//! prolly tree (store format 8).
//!
//! - **Key:** the inode number (8 bytes, big endian), then the name bytes
//!   (UTF-8, 1 to [`XATTR_NAME_MAX`] bytes). The names of one inode are the
//!   key range `ino BE .. (ino + 1) BE` ([`xattr_range`]), in name byte order.
//! - **Value:** a 1-byte tag, then
//!   - tag 0 ([`TAG_INLINE`]): the value bytes, when the leaf entry fits
//!     `prolly::MAX_ENTRY` ([`fits_inline`]);
//!   - tag 1 ([`TAG_SPILLED`]): the 33-byte `ObjectKey` of the value, stored
//!     like file content (FastCDC chunks and a manifest,
//!     `chunk_store::file::FileWriter`).
//!
//!   The choice depends only on the name and value lengths, so equal xattrs
//!   give equal trees.
//!
//! No xattr set object exists. A get, set or remove touches one entry
//! (O(tree height) nodes); a spilled value is read by range
//! (`FileReader::read(offset, size)`: only the manifest nodes and chunks of
//! the range), and its length comes from the manifest root. Memory per call
//! is one request, not the size or count of the xattrs of a file.

use std::ops::Range;

use chunk_store::file::{FileReader, FileWriter};
use chunk_store::ChunkStore;
use slicefs_traits::key::{ObjectKey, KEY_BYTES};
use slicefs_traits::metadata::MetaError;

use crate::pmap::{shard_of, TreeKey, TreeValue};
use crate::store_io::{node_err, to_id, to_key};

/// Longest xattr name in bytes (Linux `XATTR_NAME_MAX`).
pub const XATTR_NAME_MAX: usize = 255;

/// Tag of a value stored in the leaf entry.
pub const TAG_INLINE: u8 = 0;
/// Tag of a value stored like file content (its key follows).
pub const TAG_SPILLED: u8 = 1;

/// Leaf entry bytes of an xattr other than the name and the value: the
/// prolly leaf entry header (4), the inode number (8) and the tag (1).
pub const XATTR_ENTRY_OVERHEAD: usize = 4 + 8 + 1;

/// An empty name is invalid; a name above [`XATTR_NAME_MAX`] bytes is too long.
pub fn check_name(name: &str) -> Result<(), MetaError> {
    if name.is_empty() {
        return Err(MetaError::InvalidName("empty xattr name".into()));
    }
    if name.len() > XATTR_NAME_MAX {
        return Err(MetaError::NameTooLong(name.len()));
    }
    Ok(())
}

/// True when a value of `value_len` bytes under a name of `name_len` bytes
/// is stored inline (its leaf entry fits `prolly::MAX_ENTRY`).
pub fn fits_inline(name_len: usize, value_len: usize) -> bool {
    XATTR_ENTRY_OVERHEAD
        .saturating_add(name_len)
        .saturating_add(value_len)
        <= prolly::MAX_ENTRY
}

/// The key of one xattr: inode number and name bytes.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct XattrKey {
    pub ino: u64,
    pub name: Box<[u8]>,
}

impl XattrKey {
    pub fn new(ino: u64, name: &str) -> Self {
        XattrKey { ino, name: name.as_bytes().into() }
    }

    /// The key of the encoded tree key `k` (at least 9 bytes, else corrupt).
    pub fn from_encoded(k: &[u8]) -> Result<Self, MetaError> {
        if k.len() < 9 {
            return Err(MetaError::Corrupted(format!("xattr_data tree: key of {} bytes", k.len())));
        }
        Ok(XattrKey { ino: u64::from_be_bytes(k[..8].try_into().unwrap()), name: k[8..].into() })
    }
}

/// The names of one inode share its 8-byte prefix, so they are in one
/// overlay shard (a range scan of one inode reads one shard).
impl TreeKey for XattrKey {
    fn encode(&self) -> Vec<u8> {
        let mut k = Vec::with_capacity(8 + self.name.len());
        k.extend_from_slice(&self.ino.to_be_bytes());
        k.extend_from_slice(&self.name);
        k
    }

    fn shard(&self) -> usize {
        shard_of(self.ino)
    }
}

/// The tree key range of the xattrs of `ino`: `ino BE .. (ino + 1) BE`
/// (no end for `u64::MAX`).
pub fn xattr_range(ino: u64) -> (Vec<u8>, Option<Vec<u8>>) {
    (ino.to_be_bytes().to_vec(), ino.checked_add(1).map(|n| n.to_be_bytes().to_vec()))
}

/// The name of the encoded tree key `k`: at least 9 bytes, valid UTF-8
/// after the 8-byte inode number, else corrupt.
pub fn name_of(k: &[u8]) -> Result<&str, MetaError> {
    if k.len() < 9 {
        return Err(MetaError::Corrupted(format!("xattr_data tree: key of {} bytes", k.len())));
    }
    std::str::from_utf8(&k[8..]).map_err(|_| MetaError::Corrupted("xattr_data tree: name not UTF-8".into()))
}

/// The value of one xattr entry.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum XattrValue {
    /// The value bytes, in the leaf entry (tag 0).
    Inline(Vec<u8>),
    /// The key of the value, stored like file content (tag 1).
    Spilled(ObjectKey),
}

impl TreeValue for XattrValue {
    fn encode(&self) -> Vec<u8> {
        match self {
            XattrValue::Inline(v) => {
                let mut b = Vec::with_capacity(1 + v.len());
                b.push(TAG_INLINE);
                b.extend_from_slice(v);
                b
            }
            XattrValue::Spilled(k) => {
                let mut b = Vec::with_capacity(1 + KEY_BYTES);
                b.push(TAG_SPILLED);
                b.extend_from_slice(&k.0);
                b
            }
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        match bytes.split_first() {
            None => Err("empty xattr value".into()),
            Some((&TAG_INLINE, v)) => Ok(XattrValue::Inline(v.to_vec())),
            Some((&TAG_SPILLED, k)) => k
                .try_into()
                .map(|k| XattrValue::Spilled(ObjectKey(k)))
                .map_err(|_| format!("spilled xattr value of {} key bytes, not {KEY_BYTES}", k.len())),
            Some((t, _)) => Err(format!("xattr value tag {t}")),
        }
    }
}

/// Store `value` like file content (chunks and a manifest) and give its
/// key. Run it with no store lock held.
pub fn spill(cs: &ChunkStore, value: &[u8]) -> Result<ObjectKey, MetaError> {
    let io = |e: chunk_store::Error| MetaError::Io(std::io::Error::other(e.to_string()));
    let mut w = FileWriter::new();
    w.write(cs, value).map_err(io)?;
    w.finish(cs).map(to_key).map_err(io)
}

/// The length of `v`: an inline value's length, or for a spilled value the
/// length in its manifest root (no value byte is read).
pub fn value_len(cs: &ChunkStore, v: &XattrValue) -> Result<u64, MetaError> {
    match v {
        XattrValue::Inline(b) => Ok(b.len() as u64),
        XattrValue::Spilled(k) => FileReader::open(cs, to_id(k)).map(|r| r.len()).map_err(|e| node_err("xattr value", k, e)),
    }
}

/// Up to `size` bytes of `v` from `offset` (fewer at the end, none past
/// it). A spilled value reads only the manifest nodes and chunks of the
/// range; never the whole-value readers (`store_io::get_value`).
pub fn read_range(cs: &ChunkStore, v: &XattrValue, offset: u64, size: usize) -> Result<Vec<u8>, MetaError> {
    match v {
        XattrValue::Inline(b) => Ok(b[clamp(b.len(), offset, size)].to_vec()),
        XattrValue::Spilled(k) => {
            let err = |e| node_err("xattr value", k, e);
            let mut r = FileReader::open(cs, to_id(k)).map_err(err)?;
            if offset >= r.len() {
                return Ok(Vec::new());
            }
            r.read(cs, offset, size).map_err(err)
        }
    }
}

/// The byte range `offset .. offset + size` cut to `0 .. len`.
fn clamp(len: usize, offset: u64, size: usize) -> Range<usize> {
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(len);
    start..start.saturating_add(size).min(len)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn cs() -> ChunkStore {
        ChunkStore::open_with(Arc::new(chunk_store::mem::MemIo::new()), chunk_store::Options::default()).unwrap()
    }

    #[test]
    fn a_key_is_the_ino_then_the_name() {
        let k = XattrKey::new(5, "user.a");
        let mut want = 5u64.to_be_bytes().to_vec();
        want.extend_from_slice(b"user.a");
        assert_eq!(TreeKey::encode(&k), want);
        assert_eq!(XattrKey::from_encoded(&want).unwrap(), k);
        assert_eq!(name_of(&want).unwrap(), "user.a");
        assert_eq!(k.shard(), shard_of(5));
        assert_eq!(XattrKey::new(5, "other").shard(), k.shard(), "one inode, one shard");
        assert!(name_of(&5u64.to_be_bytes()).is_err(), "no name");
        let mut bad = 5u64.to_be_bytes().to_vec();
        bad.push(0xff);
        assert!(name_of(&bad).is_err(), "not UTF-8");
        assert!(XattrKey::from_encoded(&[1, 2, 3]).is_err());
    }

    #[test]
    fn the_range_of_an_inode() {
        assert_eq!(xattr_range(5), (5u64.to_be_bytes().to_vec(), Some(6u64.to_be_bytes().to_vec())));
        assert_eq!(xattr_range(u64::MAX), (u64::MAX.to_be_bytes().to_vec(), None));
    }

    #[test]
    fn values_round_trip_and_bad_bytes_are_refused() {
        let largest = prolly::MAX_ENTRY - XATTR_ENTRY_OVERHEAD - 1;
        for v in [vec![], vec![7u8], vec![9u8; largest]] {
            let x = XattrValue::Inline(v.clone());
            let b = x.encode();
            assert_eq!(b[0], TAG_INLINE);
            assert_eq!(XattrValue::decode(&b).unwrap(), x);
        }
        let s = XattrValue::Spilled(ObjectKey::example(3));
        let b = s.encode();
        assert_eq!((b[0], b.len()), (TAG_SPILLED, 1 + KEY_BYTES));
        assert_eq!(XattrValue::decode(&b).unwrap(), s);
        assert!(XattrValue::decode(&[]).is_err(), "empty");
        assert!(XattrValue::decode(&[2, 1, 2]).is_err(), "tag 2");
        assert!(XattrValue::decode(&[TAG_SPILLED; 33]).is_err(), "32 key bytes");
        assert!(XattrValue::decode(&[TAG_SPILLED; 35]).is_err(), "34 key bytes");
    }

    #[test]
    fn the_inline_rule() {
        for n in [1, 6, 255] {
            let v = prolly::MAX_ENTRY - 13 - n;
            assert!(fits_inline(n, v), "13 + {n} + {v} == MAX_ENTRY");
            assert!(!fits_inline(n, v + 1), "one byte above");
        }
        assert!(!fits_inline(usize::MAX, 1), "no overflow");
    }

    #[test]
    fn name_checks() {
        assert!(matches!(check_name(""), Err(MetaError::InvalidName(_))));
        assert!(matches!(check_name(&"a".repeat(256)), Err(MetaError::NameTooLong(256))));
        check_name(&"a".repeat(255)).unwrap();
        check_name("user.x").unwrap();
    }

    #[test]
    fn ranged_reads_of_inline_and_spilled_values() {
        let cs = cs();
        let bytes: Vec<u8> = (0..100_000u32).map(|i| (i * 7 + i / 251) as u8).collect();
        let spilled = XattrValue::Spilled(spill(&cs, &bytes).unwrap());
        let inline = XattrValue::Inline(bytes[..50].to_vec());
        for (v, b) in [(&spilled, &bytes[..]), (&inline, &bytes[..50])] {
            let n = b.len() as u64;
            assert_eq!(value_len(&cs, v).unwrap(), n);
            assert_eq!(read_range(&cs, v, 0, 10).unwrap(), &b[..10]);
            let mid = b.len() / 2;
            assert_eq!(read_range(&cs, v, mid as u64, 7).unwrap(), &b[mid..mid + 7]);
            assert_eq!(read_range(&cs, v, n - 3, 100).unwrap(), &b[b.len() - 3..], "short at the end");
            assert!(read_range(&cs, v, n, 5).unwrap().is_empty(), "at the end");
            assert!(read_range(&cs, v, n + 5, 5).unwrap().is_empty(), "past the end");
            assert!(read_range(&cs, v, u64::MAX, usize::MAX).unwrap().is_empty());
            assert_eq!(read_range(&cs, v, 0, usize::MAX).unwrap(), b, "whole");
        }
        assert_eq!(spill(&cs, &bytes).unwrap(), match spilled { XattrValue::Spilled(k) => k, _ => unreachable!() }, "same bytes, same key");
    }
}
