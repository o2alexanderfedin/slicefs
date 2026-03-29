//! Command-line interface definitions for the `slicefs` binary.
//!
//! Provides a clap-derived [`Cli`] struct with three subcommands:
//! - `mount`   — mount a SliceFS volume at a mountpoint
//! - `unmount` — unmount a SliceFS volume
//! - `seed`    — ingest files from a source directory into a store

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// SliceFS deduplicating filesystem
#[derive(Parser, Debug)]
#[command(name = "slicefs", version, about = "SliceFS deduplicating filesystem")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
}

/// Subcommands available via the `slicefs` binary.
#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Mount a SliceFS store at the given mountpoint.
    Mount {
        /// Directory to mount the filesystem at.
        mountpoint: PathBuf,
        /// Path to the SliceFS block store directory.
        #[arg(long)]
        store: PathBuf,
        /// Do not update access times (recommended for dedup workloads).
        #[arg(long, default_value_t = true)]
        noatime: bool,
        /// In-memory read cache size in bytes (0 = disabled).
        #[arg(long, default_value_t = 0)]
        cache_size: usize,
        /// Allow other users to access the mount (passes allow_other to FUSE).
        #[arg(long, default_value_t = false)]
        allow_other: bool,
        /// WAL durability strategy: per-op, periodic, flush-on-fsync, or no-wal.
        ///
        /// - per-op (default): sync to disk after every mutation (strongest durability)
        /// - flush-on-fsync: buffer mutations; flush on explicit fsync
        /// - periodic: buffer mutations; flush on background timer or shutdown
        /// - no-wal: no write-ahead log (testing only; data loss on crash)
        #[arg(long, value_name = "STRATEGY")]
        wal_strategy: Option<String>,
    },

    /// Unmount a SliceFS filesystem mounted at the given path.
    Unmount {
        /// Mountpoint to unmount.
        mountpoint: PathBuf,
    },

    /// Seed a source directory tree into a SliceFS block store.
    Seed {
        /// Path to the SliceFS block store directory.
        store: PathBuf,
        /// Source directory to ingest.
        source_dir: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mount_basic() {
        let cli = Cli::try_parse_from(["slicefs", "mount", "/mnt", "--store", "/data"]).unwrap();
        match cli.command {
            Cmd::Mount { mountpoint, store, .. } => {
                assert_eq!(mountpoint, PathBuf::from("/mnt"));
                assert_eq!(store, PathBuf::from("/data"));
            }
            _ => panic!("expected Mount"),
        }
    }

    #[test]
    fn test_unmount() {
        let cli = Cli::try_parse_from(["slicefs", "unmount", "/mnt"]).unwrap();
        match cli.command {
            Cmd::Unmount { mountpoint } => {
                assert_eq!(mountpoint, PathBuf::from("/mnt"));
            }
            _ => panic!("expected Unmount"),
        }
    }

    #[test]
    fn test_seed() {
        let cli = Cli::try_parse_from(["slicefs", "seed", "/data", "/src"]).unwrap();
        match cli.command {
            Cmd::Seed { store, source_dir } => {
                assert_eq!(store, PathBuf::from("/data"));
                assert_eq!(source_dir, PathBuf::from("/src"));
            }
            _ => panic!("expected Seed"),
        }
    }

    #[test]
    fn test_mount_extra_options() {
        let cli = Cli::try_parse_from([
            "slicefs",
            "mount",
            "/mnt",
            "--store",
            "/data",
            "--noatime",
            "--cache-size",
            "1048576",
        ])
        .unwrap();
        match cli.command {
            Cmd::Mount { mountpoint, store, noatime, cache_size, .. } => {
                assert_eq!(mountpoint, PathBuf::from("/mnt"));
                assert_eq!(store, PathBuf::from("/data"));
                assert!(noatime);
                assert_eq!(cache_size, 1048576);
            }
            _ => panic!("expected Mount"),
        }
    }
}
