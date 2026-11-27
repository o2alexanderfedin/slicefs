//! InodeAlloc: the inode number allocator.
//!
//! Hands out monotonically increasing inode numbers. The counter is not a
//! map: the root record stores `next_ino`, and a load restores it with
//! `set_next_ino`.

/// The inode number counter.
///
/// Inode 1 is reserved for the root directory and is never allocated by
/// `allocate_ino`. Allocation starts at 2 and increases monotonically.
#[derive(Clone)]
pub struct InodeAlloc {
    next_ino: u64,
}

impl InodeAlloc {
    /// Create an allocator. `next_ino` starts at 2 (inode 1 = root).
    pub fn new() -> Self {
        Self { next_ino: 2 }
    }

    /// Allocate and return the next available inode number.
    pub fn allocate_ino(&mut self) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
    }

    /// Current `next_ino` value (stored in the root record).
    pub fn next_ino(&self) -> u64 {
        self.next_ino
    }

    /// Directly set `next_ino` to the given value.
    ///
    /// Used at load to restore the exact counter from the root record, which
    /// may be higher than the highest number in use if inodes were deleted.
    pub fn set_next_ino(&mut self, value: u64) {
        self.next_ino = value;
    }
}

impl Default for InodeAlloc {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_starts_at_two() {
        let mut m = InodeAlloc::new();
        assert_eq!(m.allocate_ino(), 2);
        assert_eq!(m.allocate_ino(), 3);
    }

    #[test]
    fn test_set_next_ino_restores_the_counter() {
        let mut m = InodeAlloc::new();
        m.set_next_ino(40);
        assert_eq!(m.next_ino(), 40);
        assert_eq!(m.allocate_ino(), 40);
        assert_eq!(m.next_ino(), 41);
    }
}
