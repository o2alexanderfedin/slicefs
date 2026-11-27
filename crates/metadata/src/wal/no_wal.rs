//! No-op WAL strategy — for testing or purely in-memory usage.

use super::{Lsn, WalEntry, WalError, WalStrategy};

/// A `WalStrategy` that does nothing. All operations return `Ok(())`.
///
/// Suitable for unit tests or scenarios where crash safety is not required.
pub struct NoWal;

impl WalStrategy for NoWal {
    fn append(&self, _entry: &WalEntry) -> Result<Lsn, WalError> {
        Ok(Lsn(0))
    }

    fn write_out(&self, _upto: Lsn) -> Result<(), WalError> {
        Ok(())
    }

    fn flush_and_sync(&self) -> Result<(), WalError> {
        Ok(())
    }

    fn shutdown(&self) -> Result<(), WalError> {
        Ok(())
    }
}
