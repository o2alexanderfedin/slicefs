//! `DictMetadataStore` — MetadataStore backed by a blockset `Dictionary`.
//!
//! All state is held in-memory as `Mutex`-guarded maps; `commit()` serializes
//! everything into the Dictionary and returns the root `Digest224`.
//!
//! Locking order (always acquire in this order to prevent deadlocks):
//!   1. `inode_map`
//!   2. `dict`
//!   3. `inode_data`
//!   4. `dir_data`
//!   5. `manifest_data`

use std::collections::BTreeMap;
use std::sync::Mutex;

use slicefs_traits::digest::Digest224;
use slicefs_traits::metadata::{DirEntry, InodeId, InodeMeta, MetaError, MetadataStore};
use blockset::{Dictionary, State, Tree};

use crate::inode::{intern_inode, load_inode};
use crate::inode_map::{InodeMap, intern_inode_map};
use crate::directory::{create_dir_entries, add_dir_entry, remove_dir_entry,
                       lookup_dir_entry, list_dir_entries};
use crate::manifest::{intern_manifest, load_manifest};
use crate::xattr::{intern_xattrs, load_xattrs, set_xattr_entry, get_xattr_entry,
                   list_xattr_names, remove_xattr_entry};

// S_IFDIR bit mask (POSIX directory type)
const S_IFDIR: u32 = 0o0040_000;

/// Concrete `MetadataStore` backed by a blockset `Dictionary`.
///
/// All operations are lock-safe and `Send + Sync`.
///
/// Locking order (always acquire in this order to prevent deadlocks):
///   1. `inode_map`
///   2. `dict`
///   3. `inode_data`
///   4. `dir_data`
///   5. `manifest_data`
///   6. `xattr_data`
pub struct DictMetadataStore {
    /// The CAS block store — shared across all operations.
    dict: Mutex<Dictionary>,
    /// Inode-number allocator + mapping table.
    inode_map: Mutex<InodeMap>,
    /// Maps inode number → current inode data `Digest224`.
    inode_data: Mutex<BTreeMap<u64, Digest224>>,
    /// Maps directory inode number → current entry list `Digest224`.
    dir_data: Mutex<BTreeMap<u64, Digest224>>,
    /// Maps file inode number → current manifest `Digest224`.
    manifest_data: Mutex<BTreeMap<u64, Digest224>>,
    /// Maps inode number → xattr set `Digest224` (only for inodes with xattrs).
    xattr_data: Mutex<BTreeMap<u64, Digest224>>,
}

impl DictMetadataStore {
    /// Create a new store with a root directory at inode 1.
    pub fn new() -> Self {
        let mut dict = Dictionary::default();
        let mut inode_map = InodeMap::new();

        // Build root inode (ino=1, mode=directory 0o40755, uid=0, gid=0, nlinks=2)
        let root_meta = InodeMeta::new_directory(1, 0, 0, S_IFDIR | 0o755);
        let root_inode_digest = intern_inode(&mut dict, &root_meta);

        // Create root dir entries: . → 1, .. → 1
        let root_dir_digest = create_dir_entries(&mut dict, 1, 1);

        inode_map.insert(1, root_inode_digest);

        let mut inode_data = BTreeMap::new();
        inode_data.insert(1, root_inode_digest);

        let mut dir_data = BTreeMap::new();
        dir_data.insert(1, root_dir_digest);

        DictMetadataStore {
            dict: Mutex::new(dict),
            inode_map: Mutex::new(inode_map),
            inode_data: Mutex::new(inode_data),
            dir_data: Mutex::new(dir_data),
            manifest_data: Mutex::new(BTreeMap::new()),
            xattr_data: Mutex::new(BTreeMap::new()),
        }
    }
}

impl Default for DictMetadataStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataStore for DictMetadataStore {
    fn create_inode(&self, meta: &InodeMeta) -> Result<InodeId, MetaError> {
        let mut inode_map = self.inode_map.lock().unwrap();
        let ino = inode_map.allocate_ino();
        let mut full_meta = meta.clone();
        full_meta.ino = ino;

        let mut dict = self.dict.lock().unwrap();
        let digest = intern_inode(&mut *dict, &full_meta);
        drop(dict);

        inode_map.insert(ino, digest);
        self.inode_data.lock().unwrap().insert(ino, digest);
        Ok(ino)
    }

    fn get_inode(&self, ino: InodeId) -> Result<InodeMeta, MetaError> {
        let inode_data = self.inode_data.lock().unwrap();
        let digest = inode_data.get(&ino).copied().ok_or(MetaError::NotFound(ino))?;
        drop(inode_data);

        let dict = self.dict.lock().unwrap();
        load_inode(&*dict, &digest)
    }

    fn update_inode(&self, meta: &InodeMeta) -> Result<(), MetaError> {
        let ino = meta.ino;
        {
            let inode_data = self.inode_data.lock().unwrap();
            if !inode_data.contains_key(&ino) {
                return Err(MetaError::NotFound(ino));
            }
        }
        let mut dict = self.dict.lock().unwrap();
        let new_digest = intern_inode(&mut *dict, meta);
        drop(dict);

        self.inode_data.lock().unwrap().insert(ino, new_digest);
        self.inode_map.lock().unwrap().insert(ino, new_digest);
        Ok(())
    }

    fn delete_inode(&self, ino: InodeId) -> Result<(), MetaError> {
        let removed = self.inode_data.lock().unwrap().remove(&ino);
        if removed.is_none() {
            return Err(MetaError::NotFound(ino));
        }
        self.inode_map.lock().unwrap().remove(ino);
        Ok(())
    }

    fn create_directory(
        &self,
        parent_ino: InodeId,
        name: &str,
        meta: &InodeMeta,
    ) -> Result<InodeId, MetaError> {
        // Validate parent exists and is a directory
        let parent_meta = self.get_inode(parent_ino)?;
        if parent_meta.mode & S_IFDIR == 0 {
            return Err(MetaError::NotADirectory(parent_ino));
        }
        if !self.dir_data.lock().unwrap().contains_key(&parent_ino) {
            return Err(MetaError::NotADirectory(parent_ino));
        }

        // Allocate new inode number
        let mut inode_map = self.inode_map.lock().unwrap();
        let ino = inode_map.allocate_ino();
        let mut dir_meta = meta.clone();
        dir_meta.ino = ino;
        // Ensure mode has directory type bit
        if dir_meta.mode & S_IFDIR == 0 {
            dir_meta.mode |= S_IFDIR;
        }

        let mut dict = self.dict.lock().unwrap();

        // Create new directory's inode
        let dir_inode_digest = intern_inode(&mut *dict, &dir_meta);

        // Create . and .. entries for new directory
        let dir_entry_digest = create_dir_entries(&mut *dict, ino, parent_ino);

        // Add name entry in parent directory
        let parent_dir_digest = *self.dir_data.lock().unwrap().get(&parent_ino).unwrap();
        let new_parent_dir_digest = add_dir_entry(&mut *dict, &parent_dir_digest, name, ino)?;

        drop(dict);

        // Update maps
        inode_map.insert(ino, dir_inode_digest);
        drop(inode_map);

        self.inode_data.lock().unwrap().insert(ino, dir_inode_digest);
        self.dir_data.lock().unwrap().insert(ino, dir_entry_digest);
        self.dir_data.lock().unwrap().insert(parent_ino, new_parent_dir_digest);

        // Increment parent nlinks (for the .. backlink from new subdir)
        let mut parent_meta = self.get_inode(parent_ino)?;
        parent_meta.nlinks += 1;
        self.update_inode(&parent_meta)?;

        Ok(ino)
    }

    fn list_directory(&self, ino: InodeId) -> Result<Vec<DirEntry>, MetaError> {
        let dir_data = self.dir_data.lock().unwrap();
        let dir_digest = dir_data.get(&ino).copied().ok_or(MetaError::NotADirectory(ino))?;
        drop(dir_data);

        let dict = self.dict.lock().unwrap();
        list_dir_entries(&*dict, &dir_digest)
    }

    fn lookup(&self, parent_ino: InodeId, name: &str) -> Result<InodeId, MetaError> {
        let dir_data = self.dir_data.lock().unwrap();
        let dir_digest = dir_data.get(&parent_ino).copied().ok_or(MetaError::NotADirectory(parent_ino))?;
        drop(dir_data);

        let mut dict = self.dict.lock().unwrap();
        lookup_dir_entry(&mut *dict, &dir_digest, name)
            .map_err(|_| MetaError::NotFound(0))
    }

    fn link(&self, parent_ino: InodeId, name: &str, ino: InodeId) -> Result<(), MetaError> {
        let dir_data = self.dir_data.lock().unwrap();
        let dir_digest = dir_data.get(&parent_ino).copied().ok_or(MetaError::NotADirectory(parent_ino))?;
        drop(dir_data);

        let mut dict = self.dict.lock().unwrap();
        let new_dir_digest = add_dir_entry(&mut *dict, &dir_digest, name, ino)?;
        drop(dict);

        self.dir_data.lock().unwrap().insert(parent_ino, new_dir_digest);
        Ok(())
    }

    fn unlink(&self, parent_ino: InodeId, name: &str) -> Result<(), MetaError> {
        let dir_data = self.dir_data.lock().unwrap();
        let dir_digest = dir_data.get(&parent_ino).copied().ok_or(MetaError::NotADirectory(parent_ino))?;
        drop(dir_data);

        let mut dict = self.dict.lock().unwrap();
        let new_dir_digest = remove_dir_entry(&mut *dict, &dir_digest, name)?;
        drop(dict);

        self.dir_data.lock().unwrap().insert(parent_ino, new_dir_digest);
        Ok(())
    }

    fn set_manifest(&self, ino: InodeId, blocks: &[Digest224]) -> Result<(), MetaError> {
        let mut dict = self.dict.lock().unwrap();
        let digest = intern_manifest(&mut *dict, blocks);
        drop(dict);
        self.manifest_data.lock().unwrap().insert(ino, digest);
        Ok(())
    }

    fn get_manifest(&self, ino: InodeId) -> Result<Vec<Digest224>, MetaError> {
        let manifest_data = self.manifest_data.lock().unwrap();
        let digest = manifest_data.get(&ino).copied().ok_or(MetaError::NotFound(ino))?;
        drop(manifest_data);

        let dict = self.dict.lock().unwrap();
        load_manifest(&*dict, &digest)
    }

    fn set_xattr(&self, ino: InodeId, name: &str, value: &[u8]) -> Result<(), MetaError> {
        // Load existing xattrs for this inode (or empty vec if none yet)
        let mut xattrs = {
            let xattr_data = self.xattr_data.lock().unwrap();
            if let Some(digest) = xattr_data.get(&ino).copied() {
                drop(xattr_data);
                let dict = self.dict.lock().unwrap();
                load_xattrs(&*dict, &digest)?
            } else {
                vec![]
            }
        };

        set_xattr_entry(&mut xattrs, name, value);

        let mut dict = self.dict.lock().unwrap();
        let new_digest = intern_xattrs(&mut *dict, &xattrs);
        drop(dict);

        self.xattr_data.lock().unwrap().insert(ino, new_digest);
        Ok(())
    }

    fn get_xattr(&self, ino: InodeId, name: &str) -> Result<Vec<u8>, MetaError> {
        let xattr_data = self.xattr_data.lock().unwrap();
        let digest = match xattr_data.get(&ino).copied() {
            Some(d) => d,
            None => return Err(MetaError::NotFound(ino)),
        };
        drop(xattr_data);

        let dict = self.dict.lock().unwrap();
        let xattrs = load_xattrs(&*dict, &digest)?;
        drop(dict);

        get_xattr_entry(&xattrs, name).ok_or(MetaError::NotFound(ino))
    }

    fn list_xattrs(&self, ino: InodeId) -> Result<Vec<String>, MetaError> {
        let xattr_data = self.xattr_data.lock().unwrap();
        let digest = match xattr_data.get(&ino).copied() {
            Some(d) => d,
            None => return Ok(vec![]),
        };
        drop(xattr_data);

        let dict = self.dict.lock().unwrap();
        let xattrs = load_xattrs(&*dict, &digest)?;
        drop(dict);

        Ok(list_xattr_names(&xattrs))
    }

    fn remove_xattr(&self, ino: InodeId, name: &str) -> Result<(), MetaError> {
        let xattr_data = self.xattr_data.lock().unwrap();
        let digest = match xattr_data.get(&ino).copied() {
            Some(d) => d,
            None => return Err(MetaError::NotFound(ino)),
        };
        drop(xattr_data);

        let dict = self.dict.lock().unwrap();
        let mut xattrs = load_xattrs(&*dict, &digest)?;
        drop(dict);

        if !remove_xattr_entry(&mut xattrs, name) {
            return Err(MetaError::NotFound(ino));
        }

        let mut dict = self.dict.lock().unwrap();
        let new_digest = intern_xattrs(&mut *dict, &xattrs);
        drop(dict);

        self.xattr_data.lock().unwrap().insert(ino, new_digest);
        Ok(())
    }

    fn root_ino(&self) -> InodeId {
        1
    }

    fn commit(&self) -> Result<Digest224, MetaError> {
        // Serialize full in-memory state into a root record (156 bytes):
        //   [inode_map_digest:   28 bytes (Digest224)]
        //   [root_dir_ino:        8 bytes (u64 LE)]
        //   [next_ino:            8 bytes (u64 LE)]
        //   [inode_data_digest:  28 bytes (Digest224)] -- serialized BTreeMap<u64, Digest224>
        //   [dir_data_digest:    28 bytes (Digest224)] -- serialized BTreeMap<u64, Digest224>
        //   [manifest_data_digest: 28 bytes (Digest224)]
        //   [xattr_data_digest:  28 bytes (Digest224)]
        let inode_map = self.inode_map.lock().unwrap();
        let mut dict = self.dict.lock().unwrap();

        let inode_map_digest = intern_inode_map(&mut *dict, &inode_map);
        let next_ino = inode_map.next_ino();
        drop(inode_map);

        let inode_data_digest = {
            let map = self.inode_data.lock().unwrap();
            intern_u64_digest_map(&mut *dict, &map)
        };
        let dir_data_digest = {
            let map = self.dir_data.lock().unwrap();
            intern_u64_digest_map(&mut *dict, &map)
        };
        let manifest_data_digest = {
            let map = self.manifest_data.lock().unwrap();
            intern_u64_digest_map(&mut *dict, &map)
        };
        let xattr_data_digest = {
            let map = self.xattr_data.lock().unwrap();
            intern_u64_digest_map(&mut *dict, &map)
        };

        let mut root_bytes = Vec::with_capacity(156);
        // inode_map_digest: 28 bytes
        for word in &inode_map_digest {
            root_bytes.extend_from_slice(&word.to_le_bytes());
        }
        // root_dir_ino: 8 bytes (always 1)
        root_bytes.extend_from_slice(&1u64.to_le_bytes());
        // next_ino: 8 bytes
        root_bytes.extend_from_slice(&next_ino.to_le_bytes());
        // inode_data_digest: 28 bytes
        for word in &inode_data_digest {
            root_bytes.extend_from_slice(&word.to_le_bytes());
        }
        // dir_data_digest: 28 bytes
        for word in &dir_data_digest {
            root_bytes.extend_from_slice(&word.to_le_bytes());
        }
        // manifest_data_digest: 28 bytes
        for word in &manifest_data_digest {
            root_bytes.extend_from_slice(&word.to_le_bytes());
        }
        // xattr_data_digest: 28 bytes
        for word in &xattr_data_digest {
            root_bytes.extend_from_slice(&word.to_le_bytes());
        }

        assert_eq!(root_bytes.len(), 156, "root record must be 156 bytes");
        let root_digest = State::push_all(&mut *dict, &root_bytes);
        Ok(root_digest)
    }
}

// ─── helpers for BTreeMap<u64, Digest224> serialization ─────────────────────

/// Serialize and store a `BTreeMap<u64, Digest224>` in the Dictionary.
///
/// Format: `[count: u64 LE][for each entry: u64 LE ino + 28 bytes Digest224]`
/// Each entry is 36 bytes; total = 8 + count × 36.
fn intern_u64_digest_map(dict: &mut Dictionary, map: &BTreeMap<u64, Digest224>) -> Digest224 {
    let mut bytes = Vec::with_capacity(8 + map.len() * 36);
    bytes.extend_from_slice(&(map.len() as u64).to_le_bytes());
    for (ino, digest) in map {
        bytes.extend_from_slice(&ino.to_le_bytes());
        for word in digest {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
    }
    State::push_all(dict, &bytes)
}

/// Retrieve and deserialize a `BTreeMap<u64, Digest224>` from the Dictionary.
fn load_u64_digest_map(
    dict: &Dictionary,
    key: &Digest224,
) -> Result<BTreeMap<u64, Digest224>, MetaError> {
    use slicefs_traits::digest::from_digest224;
    use blockset::{GetBytes, GetData};
    let digest256 = from_digest224(key);
    let get_data = GetData::new(dict, &digest256);
    let bytes: Vec<u8> = GetBytes::new(get_data).collect();

    if bytes.len() < 8 {
        return Err(MetaError::Corrupted(format!(
            "u64-digest map: expected at least 8 bytes, got {}",
            bytes.len()
        )));
    }
    let count = u64::from_le_bytes(bytes[0..8].try_into().unwrap()) as usize;
    let expected = 8 + count * 36;
    if bytes.len() != expected {
        return Err(MetaError::Corrupted(format!(
            "u64-digest map: expected {} bytes for {} entries, got {}",
            expected, count, bytes.len()
        )));
    }

    let mut map = BTreeMap::new();
    for i in 0..count {
        let off = 8 + i * 36;
        let ino = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        let mut digest: Digest224 = [0u32; 7];
        for (j, word) in digest.iter_mut().enumerate() {
            let o = off + 8 + j * 4;
            *word = u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        }
        map.insert(ino, digest);
    }
    Ok(map)
}

impl DictMetadataStore {
    /// Reconstruct a `DictMetadataStore` from a Dictionary and a root digest
    /// previously returned by `commit()`.
    ///
    /// Restores all in-memory maps (inode_data, dir_data, manifest_data, xattr_data)
    /// and inode numbering state so that subsequent operations continue seamlessly.
    pub fn load_from_root(dict: Dictionary, root: &Digest224) -> Result<Self, MetaError> {
        use slicefs_traits::digest::from_digest224;
        use blockset::{GetBytes, GetData};
        use crate::inode_map::load_inode_map;

        // Read root record bytes
        let digest256 = from_digest224(root);
        let get_data = GetData::new(&dict, &digest256);
        let bytes: Vec<u8> = GetBytes::new(get_data).collect();

        if bytes.len() != 156 {
            return Err(MetaError::Corrupted(format!(
                "root record: expected 156 bytes, got {}",
                bytes.len()
            )));
        }

        // Parse the 7 fixed-width fields
        let mut off = 0;

        let inode_map_digest = parse_digest224(&bytes[off..off + 28]);
        off += 28;

        let root_dir_ino = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        off += 8;

        let next_ino = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        off += 8;

        let inode_data_digest = parse_digest224(&bytes[off..off + 28]);
        off += 28;

        let dir_data_digest = parse_digest224(&bytes[off..off + 28]);
        off += 28;

        let manifest_data_digest = parse_digest224(&bytes[off..off + 28]);
        off += 28;

        let xattr_data_digest = parse_digest224(&bytes[off..off + 28]);

        // Load and patch inode_map so next_ino is exactly restored.
        // The deserialized map computes max(keys)+1 which may be lower if
        // inodes were deleted.  The root record stores the authoritative value.
        let mut inode_map = load_inode_map(&dict, &inode_map_digest)?;
        inode_map.set_next_ino(next_ino);

        let inode_data = load_u64_digest_map(&dict, &inode_data_digest)?;
        let dir_data = load_u64_digest_map(&dict, &dir_data_digest)?;
        let manifest_data = load_u64_digest_map(&dict, &manifest_data_digest)?;
        let xattr_data = load_u64_digest_map(&dict, &xattr_data_digest)?;

        // Sanity: root directory inode must exist
        if !inode_data.contains_key(&root_dir_ino) {
            return Err(MetaError::Corrupted(format!(
                "root dir ino {} not found in inode_data after reload",
                root_dir_ino
            )));
        }

        Ok(DictMetadataStore {
            dict: Mutex::new(dict),
            inode_map: Mutex::new(inode_map),
            inode_data: Mutex::new(inode_data),
            dir_data: Mutex::new(dir_data),
            manifest_data: Mutex::new(manifest_data),
            xattr_data: Mutex::new(xattr_data),
        })
    }
}

// ─── Dictionary serialization ────────────────────────────────────────────────
//
// blockset::serialize / blockset::deserialize are broken for small payloads
// (< 248 bytes) stored via State::push_all / State::end().  Specifically,
// deserialize recomputes `to_digest224(&compress(left, right))` which panics
// when the combined data fits inline (no SHA-224 hash is generated).
//
// We provide our own, correct serialization:
// Format per entry (92 bytes):
//   key:    7 × u32 LE (28 bytes)   = Digest224
//   left:   8 × u32 LE (32 bytes)   = Digest256 left branch
//   right:  8 × u32 LE (32 bytes)   = Digest256 right branch
// Total: 28 + 32 + 32 = 92 bytes per entry.

const DICT_ENTRY_SIZE: usize = 92; // 28 (key) + 32 (left) + 32 (right)

/// Serialize a Dictionary to a byte vector.
///
/// Entries are sorted by key (BTreeMap guarantees this) so serialization is
/// deterministic.
pub fn serialize_dictionary(dict: &Dictionary) -> Vec<u8> {
    let mut buf = Vec::with_capacity(dict.len() * DICT_ENTRY_SIZE);
    for (key, branches) in dict {
        for word in key {
            buf.extend_from_slice(&word.to_le_bytes());
        }
        for digest in branches {
            for word in digest {
                buf.extend_from_slice(&word.to_le_bytes());
            }
        }
    }
    buf
}

/// Deserialize a Dictionary from bytes produced by `serialize_dictionary`.
pub fn deserialize_dictionary(bytes: &[u8]) -> Result<Dictionary, MetaError> {
    if bytes.len() % DICT_ENTRY_SIZE != 0 {
        return Err(MetaError::Corrupted(format!(
            "dictionary: expected multiple-of-{} bytes, got {}",
            DICT_ENTRY_SIZE, bytes.len()
        )));
    }
    let mut dict = Dictionary::new();
    let count = bytes.len() / DICT_ENTRY_SIZE;
    for i in 0..count {
        let off = i * DICT_ENTRY_SIZE;
        let mut key: Digest224 = [0u32; 7];
        for (j, word) in key.iter_mut().enumerate() {
            let o = off + j * 4;
            *word = u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        }
        let mut branches: [[u32; 8]; 2] = [[0u32; 8]; 2];
        for (b, branch) in branches.iter_mut().enumerate() {
            for (j, word) in branch.iter_mut().enumerate() {
                let o = off + 28 + b * 32 + j * 4;
                *word = u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
            }
        }
        dict.insert(key, branches);
    }
    Ok(dict)
}

/// Parse a `Digest224` from a 28-byte slice.
fn parse_digest224(bytes: &[u8]) -> Digest224 {
    assert_eq!(bytes.len(), 28);
    let mut d = [0u32; 7];
    for (i, word) in d.iter_mut().enumerate() {
        let off = i * 4;
        *word = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
    }
    d
}


// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};

    fn new_dir_meta() -> InodeMeta {
        InodeMeta::new_directory(0, 1000, 1000, S_IFDIR | 0o755)
    }

    fn new_file_meta() -> InodeMeta {
        InodeMeta::new_file(0, 1000, 1000, 0o644)
    }

    #[test]
    fn test_new_has_root_dir() {
        let store = DictMetadataStore::new();
        assert_eq!(store.root_ino(), 1);
        let entries = store.list_directory(1).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"."), ". not found in root: {:?}", names);
        assert!(names.contains(&".."), ".. not found in root: {:?}", names);
    }

    #[test]
    fn test_inode_crud() {
        let store = DictMetadataStore::new();

        // Create
        let meta = new_file_meta();
        let ino = store.create_inode(&meta).unwrap();
        assert!(ino >= 2, "allocated ino should be >= 2, got {}", ino);

        // Get
        let retrieved = store.get_inode(ino).unwrap();
        assert_eq!(retrieved.ino, ino);
        assert_eq!(retrieved.mode, meta.mode);
        assert_eq!(retrieved.uid, meta.uid);

        // Update
        let mut updated = retrieved.clone();
        updated.size = 4096;
        store.update_inode(&updated).unwrap();
        let after_update = store.get_inode(ino).unwrap();
        assert_eq!(after_update.size, 4096);

        // Delete
        store.delete_inode(ino).unwrap();
        let result = store.get_inode(ino);
        assert!(matches!(result, Err(MetaError::NotFound(_))));
    }

    #[test]
    fn test_create_directory_shows_in_parent() {
        let store = DictMetadataStore::new();
        let dir_meta = new_dir_meta();
        let ino = store.create_directory(1, "subdir", &dir_meta).unwrap();
        assert!(ino >= 2);

        // Parent should contain the new dir
        let parent_entries = store.list_directory(1).unwrap();
        let found = parent_entries.iter().any(|e| e.name == "subdir" && e.ino == ino);
        assert!(found, "subdir not found in parent: {:?}", parent_entries);

        // New dir should have . and ..
        let sub_entries = store.list_directory(ino).unwrap();
        let names: Vec<&str> = sub_entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"."), ". missing in new dir");
        assert!(names.contains(&".."), ".. missing in new dir");
    }

    #[test]
    fn test_lookup_and_link() {
        let store = DictMetadataStore::new();

        // Create a file inode
        let file_ino = store.create_inode(&new_file_meta()).unwrap();

        // Link it into root
        store.link(1, "myfile", file_ino).unwrap();

        // Lookup should find it
        let found_ino = store.lookup(1, "myfile").unwrap();
        assert_eq!(found_ino, file_ino);
    }

    #[test]
    fn test_unlink_removes_entry() {
        let store = DictMetadataStore::new();

        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.link(1, "toremove", file_ino).unwrap();
        assert!(store.lookup(1, "toremove").is_ok());

        store.unlink(1, "toremove").unwrap();
        let result = store.lookup(1, "toremove");
        assert!(result.is_err(), "entry should be gone after unlink");
    }

    #[test]
    fn test_manifest_round_trip() {
        let store = DictMetadataStore::new();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();

        let blocks: Vec<Digest224> = (0..5).map(|i| [i as u32; 7]).collect();
        store.set_manifest(file_ino, &blocks).unwrap();
        let recovered = store.get_manifest(file_ino).unwrap();
        assert_eq!(recovered, blocks);
    }

    #[test]
    fn test_root_ino_is_one() {
        let store = DictMetadataStore::new();
        assert_eq!(store.root_ino(), 1);
    }

    #[test]
    fn test_commit_returns_nonzero_digest() {
        let store = DictMetadataStore::new();
        let digest = store.commit().unwrap();
        assert_ne!(digest, [0u32; 7], "commit should return non-zero digest");
    }

    #[test]
    fn test_delete_nonexistent_returns_not_found() {
        let store = DictMetadataStore::new();
        let result = store.delete_inode(9999);
        assert!(matches!(result, Err(MetaError::NotFound(9999))));
    }

    #[test]
    fn test_create_dir_under_nonexistent_parent_returns_not_found() {
        let store = DictMetadataStore::new();
        let result = store.create_directory(9999, "sub", &new_dir_meta());
        // Should be NotFound since ino 9999 does not exist
        assert!(result.is_err());
    }

    #[test]
    fn test_create_dir_under_file_returns_not_a_directory() {
        let store = DictMetadataStore::new();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        let result = store.create_directory(file_ino, "sub", &new_dir_meta());
        assert!(
            matches!(result, Err(MetaError::NotADirectory(_))),
            "expected NotADirectory, got {:?}",
            result.err()
        );
    }

    #[test]
    fn test_xattr_set_get() {
        let store = DictMetadataStore::new();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.set_xattr(file_ino, "user.test", b"myvalue").unwrap();
        let val = store.get_xattr(file_ino, "user.test").unwrap();
        assert_eq!(val, b"myvalue");
    }

    #[test]
    fn test_xattr_list() {
        let store = DictMetadataStore::new();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.set_xattr(file_ino, "user.a", b"1").unwrap();
        store.set_xattr(file_ino, "user.b", b"2").unwrap();
        store.set_xattr(file_ino, "security.x", b"3").unwrap();
        let mut names = store.list_xattrs(file_ino).unwrap();
        names.sort();
        assert!(names.contains(&"user.a".to_string()));
        assert!(names.contains(&"user.b".to_string()));
        assert!(names.contains(&"security.x".to_string()));
        assert_eq!(names.len(), 3);
    }

    #[test]
    fn test_xattr_remove() {
        let store = DictMetadataStore::new();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.set_xattr(file_ino, "user.k", b"v").unwrap();
        store.remove_xattr(file_ino, "user.k").unwrap();
        let result = store.get_xattr(file_ino, "user.k");
        assert!(result.is_err(), "get after remove should error");
    }

    #[test]
    fn test_xattr_overwrite() {
        let store = DictMetadataStore::new();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.set_xattr(file_ino, "user.x", b"first").unwrap();
        store.set_xattr(file_ino, "user.x", b"second").unwrap();
        let val = store.get_xattr(file_ino, "user.x").unwrap();
        assert_eq!(val, b"second");
    }

    #[test]
    fn test_xattr_on_directory() {
        let store = DictMetadataStore::new();
        // Set xattr on the root directory inode (ino=1)
        store.set_xattr(1, "user.dir_attr", b"dir_val").unwrap();
        let val = store.get_xattr(1, "user.dir_attr").unwrap();
        assert_eq!(val, b"dir_val");
    }

    #[test]
    fn test_xattr_large_value() {
        let store = DictMetadataStore::new();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        // Value > 31 bytes exercises CAS tree storage
        let large_value: Vec<u8> = (0u8..=127u8).collect();
        store.set_xattr(file_ino, "user.big", &large_value).unwrap();
        let recovered = store.get_xattr(file_ino, "user.big").unwrap();
        assert_eq!(recovered, large_value);
    }

    #[test]
    fn test_xattr_list_empty() {
        let store = DictMetadataStore::new();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        let names = store.list_xattrs(file_ino).unwrap();
        assert!(names.is_empty());
    }

    #[test]
    fn test_xattr_get_not_found() {
        let store = DictMetadataStore::new();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        let result = store.get_xattr(file_ino, "user.missing");
        assert!(matches!(result, Err(MetaError::NotFound(_))));
    }

    #[test]
    fn test_xattr_remove_not_found() {
        let store = DictMetadataStore::new();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        let result = store.remove_xattr(file_ino, "user.nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_update_nonexistent_returns_not_found() {
        let store = DictMetadataStore::new();
        let mut meta = new_file_meta();
        meta.ino = 9999;
        let result = store.update_inode(&meta);
        assert!(matches!(result, Err(MetaError::NotFound(9999))));
    }

    #[test]
    fn test_inode_numbers_are_monotonic() {
        let store = DictMetadataStore::new();
        let ino1 = store.create_inode(&new_file_meta()).unwrap();
        let ino2 = store.create_inode(&new_file_meta()).unwrap();
        let ino3 = store.create_inode(&new_file_meta()).unwrap();
        assert!(ino1 < ino2);
        assert!(ino2 < ino3);
    }

    #[test]
    fn test_link_duplicate_name_rejected() {
        let store = DictMetadataStore::new();
        let ino = store.create_inode(&new_file_meta()).unwrap();
        store.link(1, "dup", ino).unwrap();
        let result = store.link(1, "dup", ino);
        assert!(matches!(result, Err(MetaError::AlreadyExists(_))));
    }

    #[test]
    fn test_list_directory_non_dir_returns_error() {
        let store = DictMetadataStore::new();
        // Inode 9999 doesn't exist as a directory
        let result = store.list_directory(9999);
        assert!(result.is_err());
    }

    // ── persistence round-trip tests (Task 2) ─────────────────────────────────

    #[test]
    fn test_commit_and_reload() {
        let store = DictMetadataStore::new();

        // Add a file
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.link(1, "myfile", file_ino).unwrap();
        let blocks: Vec<Digest224> = vec![[1u32; 7], [2u32; 7]];
        store.set_manifest(file_ino, &blocks).unwrap();

        // Add a subdirectory
        let dir_ino = store.create_directory(1, "subdir", &new_dir_meta()).unwrap();

        // Add xattrs
        store.set_xattr(file_ino, "user.meta", b"mymetavalue").unwrap();
        store.set_xattr(dir_ino, "user.tag", b"important").unwrap();

        // Commit
        let root_digest = store.commit().unwrap();
        // Take the Dictionary out via serialize/deserialize round-trip
        let dict = {
            let d = store.dict.lock().unwrap();
            d.clone()
        };

        // Reload from the same Dictionary + root digest
        let reloaded = DictMetadataStore::load_from_root(dict, &root_digest).unwrap();

        // Verify file inode
        let file_meta = reloaded.get_inode(file_ino).unwrap();
        assert_eq!(file_meta.ino, file_ino);
        assert_eq!(file_meta.mode, 0o644);

        // Verify manifest
        let recovered_blocks = reloaded.get_manifest(file_ino).unwrap();
        assert_eq!(recovered_blocks, blocks);

        // Verify directory
        let entries = reloaded.list_directory(1).unwrap();
        assert!(entries.iter().any(|e| e.name == "myfile" && e.ino == file_ino));
        assert!(entries.iter().any(|e| e.name == "subdir" && e.ino == dir_ino));

        // Verify xattrs
        let val = reloaded.get_xattr(file_ino, "user.meta").unwrap();
        assert_eq!(val, b"mymetavalue");
        let val2 = reloaded.get_xattr(dir_ino, "user.tag").unwrap();
        assert_eq!(val2, b"important");
    }

    #[test]
    fn test_inode_stability_across_reload() {
        let store = DictMetadataStore::new();

        // Allocate 5 file inodes (inos 2-6)
        let inos: Vec<u64> = (0..5)
            .map(|_| store.create_inode(&new_file_meta()).unwrap())
            .collect();

        let root = store.commit().unwrap();
        let dict = store.dict.lock().unwrap().clone();

        let reloaded = DictMetadataStore::load_from_root(dict, &root).unwrap();

        // All 5 inodes still exist
        for &ino in &inos {
            assert!(reloaded.get_inode(ino).is_ok(), "ino {} missing after reload", ino);
        }

        // Next allocated inode continues from where it left off (no reuse)
        let next_ino = reloaded.create_inode(&new_file_meta()).unwrap();
        for &old_ino in &inos {
            assert_ne!(next_ino, old_ino, "inode {} was reused!", next_ino);
        }
        // next_ino must be > max of previous inos
        let max_ino = *inos.iter().max().unwrap();
        assert!(next_ino > max_ino, "next_ino {} not > max_ino {}", next_ino, max_ino);
    }

    #[test]
    fn test_directory_stable_across_reload() {
        let store = DictMetadataStore::new();
        let sub1 = store.create_directory(1, "alpha", &new_dir_meta()).unwrap();
        let sub2 = store.create_directory(1, "beta", &new_dir_meta()).unwrap();
        let _sub3 = store.create_directory(sub1, "gamma", &new_dir_meta()).unwrap();

        let root = store.commit().unwrap();
        let dict = store.dict.lock().unwrap().clone();
        let reloaded = DictMetadataStore::load_from_root(dict, &root).unwrap();

        let entries = reloaded.list_directory(1).unwrap();
        assert!(entries.iter().any(|e| e.name == "alpha" && e.ino == sub1));
        assert!(entries.iter().any(|e| e.name == "beta" && e.ino == sub2));
    }

    #[test]
    fn test_manifest_stable_across_reload() {
        let store = DictMetadataStore::new();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        let blocks: Vec<Digest224> = (0..10).map(|i| [i as u32; 7]).collect();
        store.set_manifest(file_ino, &blocks).unwrap();

        let root = store.commit().unwrap();
        let dict = store.dict.lock().unwrap().clone();
        let reloaded = DictMetadataStore::load_from_root(dict, &root).unwrap();

        let recovered = reloaded.get_manifest(file_ino).unwrap();
        assert_eq!(recovered, blocks);
    }

    #[test]
    fn test_xattr_stable_across_reload() {
        let store = DictMetadataStore::new();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.set_xattr(file_ino, "user.k1", b"val1").unwrap();
        store.set_xattr(file_ino, "user.k2", b"val2").unwrap();

        let root = store.commit().unwrap();
        let dict = store.dict.lock().unwrap().clone();
        let reloaded = DictMetadataStore::load_from_root(dict, &root).unwrap();

        assert_eq!(reloaded.get_xattr(file_ino, "user.k1").unwrap(), b"val1");
        assert_eq!(reloaded.get_xattr(file_ino, "user.k2").unwrap(), b"val2");
    }

    #[test]
    fn test_dictionary_persistence_round_trip() {
        // This test proves POSIX-10: inode numbers are stable across a full
        // serialize/deserialize cycle of the Dictionary.
        //
        // Note: blockset::serialize / blockset::deserialize have a known bug for
        // small payloads (< 248 bytes) stored via State::push_all: the round-trip
        // panics because deserialize expects only SHA-224 hashes but end() can
        // produce inline encodings.  We use serialize_dictionary / deserialize_dictionary
        // from this crate which implement a correct, format-agnostic encoding.

        let store = DictMetadataStore::new();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.link(1, "file.txt", file_ino).unwrap();
        let blocks: Vec<Digest224> = vec![[0xABu32; 7]];
        store.set_manifest(file_ino, &blocks).unwrap();
        store.set_xattr(file_ino, "user.tag", b"important").unwrap();

        let root = store.commit().unwrap();

        // Serialize Dictionary to bytes using our correct serializer
        let bytes = {
            let dict = store.dict.lock().unwrap();
            serialize_dictionary(&*dict)
        };

        // Deserialize into a fresh Dictionary
        let new_dict = deserialize_dictionary(&bytes).unwrap();

        // Load metadata from the fresh Dictionary
        let reloaded = DictMetadataStore::load_from_root(new_dict, &root).unwrap();

        // Verify all data intact
        let meta = reloaded.get_inode(file_ino).unwrap();
        assert_eq!(meta.ino, file_ino);

        let found_ino = reloaded.lookup(1, "file.txt").unwrap();
        assert_eq!(found_ino, file_ino);

        let recovered = reloaded.get_manifest(file_ino).unwrap();
        assert_eq!(recovered, blocks);

        let tag = reloaded.get_xattr(file_ino, "user.tag").unwrap();
        assert_eq!(tag, b"important");
    }

    #[test]
    fn test_empty_store_commit_reload() {
        let store = DictMetadataStore::new();
        let root = store.commit().unwrap();
        let dict = store.dict.lock().unwrap().clone();

        let reloaded = DictMetadataStore::load_from_root(dict, &root).unwrap();

        // Root dir still exists with . and ..
        assert_eq!(reloaded.root_ino(), 1);
        let entries = reloaded.list_directory(1).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"."), ". missing after reload");
        assert!(names.contains(&".."), ".. missing after reload");
    }
}

