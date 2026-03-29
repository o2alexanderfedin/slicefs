//! No-op WAL strategy — for testing or purely in-memory usage.

use super::{WalEntry, WalError, WalStrategy};

/// A `WalStrategy` that does nothing. All operations return `Ok(())`.
///
/// Suitable for unit tests or scenarios where crash safety is not required.
pub struct NoWal;

impl WalStrategy for NoWal {
    fn log_mutation(&self, _entry: &WalEntry) -> Result<(), WalError> {
        Ok(())
    }

    fn flush_and_sync(&self) -> Result<(), WalError> {
        Ok(())
    }

    fn shutdown(&self) -> Result<(), WalError> {
        Ok(())
    }
}
