//! Command-line interface definitions for the `slicefs` binary.
//!
//! Provides a clap-derived [`Cli`] struct with subcommands:
//! - `mount`    — mount a SliceFS volume at a mountpoint
//! - `unmount`  — unmount a SliceFS volume
//! - `seed`     — ingest files from a source directory into a store
//! - `gc`       — run offline garbage collection
//! - `snapshot` — create, list, and switch snapshots
//! - `stats`    — show store statistics (dedup ratio, block counts, etc.)
//! - `scrub`    — verify integrity of all stored blocks by re-hashing

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// SliceFS deduplicating filesystem
#[derive(Parser, Debug)]
#[command(name = "slicefs", version, about = "SliceFS deduplicating filesystem")]
pub struct Cli {
    /// Output structured JSON instead of human-readable text.
    #[arg(long, global = true)]
    pub json: bool,

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
        /// Mount a specific snapshot read-only (by version number or name).
        #[arg(long)]
        snapshot: Option<String>,
        /// Automatically create a snapshot on clean unmount.
        #[arg(long, default_value_t = false)]
        auto_snapshot: bool,
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

    /// Run offline garbage collection on a SliceFS block store.
    ///
    /// The store must NOT be mounted when this command is run.
    /// Use the background GC thread for in-process GC during active mounts.
    Gc {
        /// Path to the SliceFS block store directory.
        store: PathBuf,
    },

    /// Manage snapshots for a SliceFS block store.
    ///
    /// Snapshots capture an immutable point-in-time view of the filesystem.
    /// The store must NOT be mounted when running snapshot commands.
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },

    /// Show store statistics: dedup ratio, block counts, snapshot info, refcount distribution.
    ///
    /// Works on both mounted and unmounted stores (read-only scan).
    Stats {
        /// Path to the SliceFS block store directory.
        store: PathBuf,
    },

    /// Verify integrity of all stored blocks by re-hashing content.
    ///
    /// Exits 0 on a clean store, non-zero if any corruption is detected.
    Scrub {
        /// Path to the SliceFS block store directory.
        store: PathBuf,
    },
}

/// Snapshot subcommands.
#[derive(Subcommand, Debug)]
pub enum SnapshotAction {
    /// Create a new snapshot of the current committed state.
    Create {
        /// Path to the SliceFS block store directory.
        store: PathBuf,
        /// Optional human-readable name/tag for the snapshot.
        #[arg(long)]
        name: Option<String>,
    },
    /// List all snapshots in the store.
    List {
        /// Path to the SliceFS block store directory.
        store: PathBuf,
    },
    /// Switch the live filesystem root to a snapshot's root.
    ///
    /// Automatically saves the current state as an auto-snapshot before switching.
    Switch {
        /// Path to the SliceFS block store directory.
        store: PathBuf,
        /// Snapshot version number or name to switch to.
        version_or_name: String,
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

    // ── Snapshot CLI tests ───────────────────────────────────────────────────

    #[test]
    fn test_snapshot_create_no_name() {
        let cli = Cli::try_parse_from(["slicefs", "snapshot", "create", "/data"]).unwrap();
        match cli.command {
            Cmd::Snapshot { action: SnapshotAction::Create { store, name } } => {
                assert_eq!(store, PathBuf::from("/data"));
                assert!(name.is_none());
            }
            _ => panic!("expected Snapshot::Create"),
        }
    }

    #[test]
    fn test_snapshot_create_with_name() {
        let cli = Cli::try_parse_from([
            "slicefs", "snapshot", "create", "/data", "--name", "release-1.0",
        ])
        .unwrap();
        match cli.command {
            Cmd::Snapshot { action: SnapshotAction::Create { store, name } } => {
                assert_eq!(store, PathBuf::from("/data"));
                assert_eq!(name.as_deref(), Some("release-1.0"));
            }
            _ => panic!("expected Snapshot::Create with name"),
        }
    }

    #[test]
    fn test_snapshot_list() {
        let cli = Cli::try_parse_from(["slicefs", "snapshot", "list", "/data"]).unwrap();
        match cli.command {
            Cmd::Snapshot { action: SnapshotAction::List { store } } => {
                assert_eq!(store, PathBuf::from("/data"));
            }
            _ => panic!("expected Snapshot::List"),
        }
    }

    #[test]
    fn test_snapshot_switch_by_version() {
        let cli =
            Cli::try_parse_from(["slicefs", "snapshot", "switch", "/data", "3"]).unwrap();
        match cli.command {
            Cmd::Snapshot { action: SnapshotAction::Switch { store, version_or_name } } => {
                assert_eq!(store, PathBuf::from("/data"));
                assert_eq!(version_or_name, "3");
            }
            _ => panic!("expected Snapshot::Switch"),
        }
    }

    #[test]
    fn test_snapshot_switch_by_name() {
        let cli = Cli::try_parse_from([
            "slicefs", "snapshot", "switch", "/data", "release-1.0",
        ])
        .unwrap();
        match cli.command {
            Cmd::Snapshot { action: SnapshotAction::Switch { store, version_or_name } } => {
                assert_eq!(store, PathBuf::from("/data"));
                assert_eq!(version_or_name, "release-1.0");
            }
            _ => panic!("expected Snapshot::Switch by name"),
        }
    }

    #[test]
    fn test_mount_with_snapshot_flag() {
        let cli = Cli::try_parse_from([
            "slicefs", "mount", "/mnt", "--store", "/data", "--snapshot", "3",
        ])
        .unwrap();
        match cli.command {
            Cmd::Mount { snapshot, auto_snapshot, .. } => {
                assert_eq!(snapshot.as_deref(), Some("3"));
                assert!(!auto_snapshot);
            }
            _ => panic!("expected Mount with --snapshot"),
        }
    }

    #[test]
    fn test_mount_with_auto_snapshot_flag() {
        let cli = Cli::try_parse_from([
            "slicefs", "mount", "/mnt", "--store", "/data", "--auto-snapshot",
        ])
        .unwrap();
        match cli.command {
            Cmd::Mount { snapshot, auto_snapshot, .. } => {
                assert!(snapshot.is_none());
                assert!(auto_snapshot);
            }
            _ => panic!("expected Mount with --auto-snapshot"),
        }
    }

    // ── Compressor CLI tests ─────────────────────────────────────────────────

    #[test]
    fn test_mount_default_compressor_is_zstd() {
        let cli = Cli::try_parse_from(["slicefs", "mount", "/mnt", "--store", "/data"]).unwrap();
        match cli.command {
            Cmd::Mount { compressor, compressor_level, .. } => {
                assert_eq!(compressor, "zstd", "default compressor should be zstd");
                assert!(compressor_level.is_none(), "default level should be None");
            }
            _ => panic!("expected Mount"),
        }
    }

    #[test]
    fn test_mount_compressor_lz4() {
        let cli = Cli::try_parse_from([
            "slicefs", "mount", "/mnt", "--store", "/data", "--compressor", "lz4",
        ])
        .unwrap();
        match cli.command {
            Cmd::Mount { compressor, .. } => {
                assert_eq!(compressor, "lz4");
            }
            _ => panic!("expected Mount"),
        }
    }

    #[test]
    fn test_mount_compressor_none() {
        let cli = Cli::try_parse_from([
            "slicefs", "mount", "/mnt", "--store", "/data", "--compressor", "none",
        ])
        .unwrap();
        match cli.command {
            Cmd::Mount { compressor, .. } => {
                assert_eq!(compressor, "none");
            }
            _ => panic!("expected Mount"),
        }
    }

    #[test]
    fn test_mount_compressor_zstd_with_level() {
        let cli = Cli::try_parse_from([
            "slicefs", "mount", "/mnt", "--store", "/data",
            "--compressor", "zstd", "--compressor-level", "9",
        ])
        .unwrap();
        match cli.command {
            Cmd::Mount { compressor, compressor_level, .. } => {
                assert_eq!(compressor, "zstd");
                assert_eq!(compressor_level, Some(9));
            }
            _ => panic!("expected Mount"),
        }
    }

    // ── Stats / Scrub / JSON CLI tests ───────────────────────────────────────

    #[test]
    fn test_stats_subcommand() {
        let cli = Cli::try_parse_from(["slicefs", "stats", "/data"]).unwrap();
        match cli.command {
            Cmd::Stats { store } => {
                assert_eq!(store, PathBuf::from("/data"));
            }
            _ => panic!("expected Stats"),
        }
    }

    #[test]
    fn test_scrub_subcommand() {
        let cli = Cli::try_parse_from(["slicefs", "scrub", "/data"]).unwrap();
        match cli.command {
            Cmd::Scrub { store } => {
                assert_eq!(store, PathBuf::from("/data"));
            }
            _ => panic!("expected Scrub"),
        }
    }

    #[test]
    fn test_json_flag_global() {
        let cli = Cli::try_parse_from(["slicefs", "--json", "stats", "/data"]).unwrap();
        assert!(cli.json, "expected --json to be true");
        match cli.command {
            Cmd::Stats { store } => {
                assert_eq!(store, PathBuf::from("/data"));
            }
            _ => panic!("expected Stats"),
        }
    }

    #[test]
    fn test_json_flag_after_subcommand() {
        // Global flags work after the subcommand name too.
        let cli = Cli::try_parse_from(["slicefs", "stats", "--json", "/data"]).unwrap();
        assert!(cli.json, "expected --json to be true");
        match cli.command {
            Cmd::Stats { store } => {
                assert_eq!(store, PathBuf::from("/data"));
            }
            _ => panic!("expected Stats after --json"),
        }
    }

    #[test]
    fn test_no_json_flag_default() {
        let cli = Cli::try_parse_from(["slicefs", "stats", "/data"]).unwrap();
        assert!(!cli.json, "expected --json to default to false");
    }
}
