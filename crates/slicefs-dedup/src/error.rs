use slicefs_traits::CasError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DedupIndexError {
    #[error("redb error: {0}")]
    Redb(#[from] redb::Error),
    #[error("redb database error: {0}")]
    Database(#[from] redb::DatabaseError),
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("bloom snapshot corrupt: {0}")]
    BloomCorrupt(&'static str),
    #[error("manifest corrupt: {0}")]
    ManifestCorrupt(&'static str),
    #[error("bloom capacity exceeded: load_factor={load_factor:.3}")]
    BloomCapacityExceeded { load_factor: f64 },
    #[error("recovery failed: {0}")]
    Recovery(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("engine panic: {0}")]
    EnginePanic(String),
}

impl From<DedupIndexError> for CasError {
    fn from(e: DedupIndexError) -> Self {
        match e {
            DedupIndexError::Io(io) => CasError::Io(io),
            other => CasError::Index(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_passthrough_to_cas_io() {
        let e = DedupIndexError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"));
        match CasError::from(e) {
            CasError::Io(_) => {}
            other => panic!("expected CasError::Io, got {other:?}"),
        }
    }

    #[test]
    fn other_variants_collapse_to_index() {
        let e = DedupIndexError::BloomCorrupt("xxh3 mismatch");
        let mapped = CasError::from(e);
        match mapped {
            CasError::Index(msg) => assert!(msg.contains("xxh3")),
            other => panic!("expected CasError::Index, got {other:?}"),
        }
    }
}
