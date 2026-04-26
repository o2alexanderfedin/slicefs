use crate::error::DedupIndexError;
use crate::paths::DedupRoot;
use crate::platform::{durable_sync, fsync_parent_dir};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

const MANIFEST_MAGIC: &str = "SLDX-MANIFEST-01";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub magic: String,
    pub schema_version: u32,
    pub redb_format_version: u32,
    pub created_at_unix_micros: u64,
    pub last_clean_shutdown_unix_micros: u64,
    pub last_shutdown_was_clean: bool,
    pub bloom_capacity: u64,
    pub bloom_fpr: f64,
    pub entries_high_water_mark: u64,
    pub page_size_bytes: u32,
    pub cas_root_relpath: String,
    /// CRC32C over canonical-JSON bytes with this field replaced by 0.
    pub manifest_crc32c: u32,
}

impl Manifest {
    pub fn new(bloom_capacity: u64, bloom_fpr: f64, page_size: u32) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;
        Self {
            magic: MANIFEST_MAGIC.into(),
            schema_version: SCHEMA_VERSION,
            redb_format_version: 4,
            created_at_unix_micros: now,
            last_clean_shutdown_unix_micros: 0,
            last_shutdown_was_clean: false,
            bloom_capacity,
            bloom_fpr,
            entries_high_water_mark: 0,
            page_size_bytes: page_size,
            cas_root_relpath: "../".into(),
            manifest_crc32c: 0,
        }
    }

    fn canonical_bytes_no_crc(&self) -> Result<Vec<u8>, DedupIndexError> {
        let mut copy = self.clone();
        copy.manifest_crc32c = 0;
        let json = serde_json::to_vec(&copy).map_err(|e| {
            DedupIndexError::ManifestCorrupt(Box::leak(format!("encode: {e}").into_boxed_str()))
        })?;
        Ok(json)
    }

    pub fn fill_crc(&mut self) -> Result<(), DedupIndexError> {
        let bytes = self.canonical_bytes_no_crc()?;
        self.manifest_crc32c = crc32c::crc32c(&bytes);
        Ok(())
    }

    pub fn verify_crc(&self) -> Result<(), DedupIndexError> {
        let bytes = self.canonical_bytes_no_crc()?;
        let expected = crc32c::crc32c(&bytes);
        if expected != self.manifest_crc32c {
            return Err(DedupIndexError::ManifestCorrupt("crc32c mismatch"));
        }
        if self.magic != MANIFEST_MAGIC {
            return Err(DedupIndexError::ManifestCorrupt("magic mismatch"));
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(DedupIndexError::ManifestCorrupt("schema version mismatch"));
        }
        Ok(())
    }

    /// Atomic write: tmp + rename + fsync(parent).
    /// Tmp is opened with O_DSYNC for I11.
    pub fn write_atomic(&mut self, root: &DedupRoot) -> Result<(), DedupIndexError> {
        self.fill_crc()?;
        let tmp = root.manifest_tmp();
        let final_path = root.manifest();

        std::fs::create_dir_all(root.base())?;

        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_DSYNC);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(self).map_err(|e| {
            DedupIndexError::ManifestCorrupt(Box::leak(format!("encode: {e}").into_boxed_str()))
        })?)?;
        durable_sync(&f)?;
        drop(f);

        std::fs::rename(&tmp, &final_path)?;
        fsync_parent_dir(root.base())?;
        Ok(())
    }

    pub fn read(root: &DedupRoot) -> Result<Self, DedupIndexError> {
        let bytes = std::fs::read(root.manifest())?;
        let m: Manifest = serde_json::from_slice(&bytes).map_err(|e| {
            DedupIndexError::ManifestCorrupt(Box::leak(format!("decode: {e}").into_boxed_str()))
        })?;
        m.verify_crc()?;
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> (tempfile::TempDir, DedupRoot) {
        let td = tempfile::tempdir().unwrap();
        let r = DedupRoot::new(td.path().join("d"));
        (td, r)
    }

    #[test]
    fn write_then_read_roundtrip() {
        let (_g, r) = root();
        let mut m = Manifest::new(1_000_000, 0.01, 4096);
        m.last_shutdown_was_clean = true;
        m.write_atomic(&r).unwrap();
        let m2 = Manifest::read(&r).unwrap();
        assert!(m2.last_shutdown_was_clean);
        assert_eq!(m2.bloom_capacity, 1_000_000);
    }

    #[test]
    fn corrupt_crc_is_detected() {
        let (_g, r) = root();
        let mut m = Manifest::new(1, 0.01, 4096);
        m.write_atomic(&r).unwrap();
        // Flip a byte in the on-disk file but keep the CRC field stale.
        let path = r.manifest();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        let err = Manifest::read(&r).unwrap_err();
        assert!(matches!(err, DedupIndexError::ManifestCorrupt(_)));
    }

    #[test]
    fn missing_file_is_io_error() {
        let (_g, r) = root();
        let err = Manifest::read(&r).unwrap_err();
        assert!(matches!(err, DedupIndexError::Io(_)));
    }
}
