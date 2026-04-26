use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DedupRoot {
    base: PathBuf,
}

impl DedupRoot {
    pub fn new(base: impl AsRef<Path>) -> Self {
        Self { base: base.as_ref().to_path_buf() }
    }

    pub fn base(&self) -> &Path { &self.base }
    pub fn redb(&self)         -> PathBuf { self.base.join("index.redb") }
    pub fn redb_lock(&self)    -> PathBuf { self.base.join("index.redb.lock") }
    pub fn manifest(&self)     -> PathBuf { self.base.join("manifest.json") }
    pub fn manifest_tmp(&self) -> PathBuf { self.base.join("manifest.json.tmp") }
    pub fn bloom(&self)        -> PathBuf { self.base.join("bloom.snap") }
    pub fn bloom_tmp(&self)    -> PathBuf { self.base.join("bloom.snap.tmp") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_under_base() {
        let r = DedupRoot::new("/store/cas/.dedup-index");
        assert_eq!(r.redb(), Path::new("/store/cas/.dedup-index/index.redb"));
        assert_eq!(r.bloom(), Path::new("/store/cas/.dedup-index/bloom.snap"));
        assert_eq!(r.manifest_tmp(), Path::new("/store/cas/.dedup-index/manifest.json.tmp"));
    }
}
