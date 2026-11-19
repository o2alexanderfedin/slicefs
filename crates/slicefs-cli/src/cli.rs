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
        /// - per-op (default): each completed request is written to the log file
        ///   at once, so it survives a crash of the daemon; fsync, fsyncdir or a
        ///   commit makes it survive a power loss (POSIX rules)
        /// - flush-on-fsync: keep up to 4 MiB of records in RAM; write them to
        ///   the log at fsync, at a commit, or when they pass 4 MiB
        /// - periodic: as flush-on-fsync, and also sync the log every 5 s
        /// - no-wal: no write-ahead log (testing only; data loss on crash)
        ///
        /// After a crash with no fsync, flush-on-fsync and periodic can lose up
        /// to the last 4 MiB of records, and a file can come back shorter: a
        /// correct first part of the data that was written.
        #[arg(long, value_name = "STRATEGY")]
        wal_strategy: Option<String>,
        /// Mount a specific snapshot read-only (by version number or name).
        #[arg(long)]
        snapshot: Option<String>,
        /// Automatically create a snapshot on clean unmount.
        #[arg(long, default_value_t = false)]
        auto_snapshot: bool,
        /// FUSE-T backend (macOS): fskit, the default and only backend.
        /// Needs the FUSE-T extension enabled in System Settings.
        #[arg(long, value_name = "BACKEND")]
        backend: Option<String>,
        /// Accepted for compatibility; currently has no effect (no backend is blocked).
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Commit metadata every SECS seconds while mounted, when something
        /// changed (0 = only at unmount). A crash loses at most about this much.
        #[arg(long, value_name = "SECS", default_value_t = crate::commit_timer::DEFAULT_COMMIT_INTERVAL_SECS)]
        commit_interval: u64,
        /// GC the stored objects while mounted: at most every SECS seconds,
        /// only when something was committed since the last GC, and with a
        /// gap of at least 10 times the last GC's duration
        /// (0 = only when `slicefs gc` asks for one).
        #[arg(long, value_name = "SECS", default_value_t = crate::online_gc::DEFAULT_GC_INTERVAL_SECS)]
        gc_interval: u64,
        /// Compression of new file data in this mount: `zstd`, `zstd:LEVEL`
        /// (1-22) or `none`. Data already stored keeps its form, so this can
        /// change at each mount.
        #[arg(long, value_name = "SPEC", default_value = "zstd:3", value_parser = crate::content::parse_compression)]
        compression: i32,
        /// Hash of the IDs of new objects (`blake3-256`). Default: the hash
        /// that the store records (blake3-256 for a new store). A new hash is
        /// recorded; objects of the old hash stay readable.
        #[arg(long, value_name = "NAME", value_parser = crate::content::parse_hash)]
        hash: Option<String>,
    },

    /// Unmount a SliceFS filesystem mounted at the given path.
    Unmount {
        /// Mountpoint to unmount.
        mountpoint: PathBuf,
        /// Path to the SliceFS store (optional; enables mount.lock cleanup).
        #[arg(long)]
        store: Option<PathBuf>,
    },

    /// Seed a source directory tree into a SliceFS block store.
    Seed {
        /// Path to the SliceFS block store directory.
        store: PathBuf,
        /// Source directory to ingest.
        source_dir: PathBuf,
        /// Hash of the IDs of new objects (`blake3-256`). Default: the hash
        /// that the store records (blake3-256 for a new store). A new hash is
        /// recorded; objects of the old hash stay readable.
        #[arg(long, value_name = "NAME", value_parser = crate::content::parse_hash)]
        hash: Option<String>,
    },

    /// Run offline garbage collection on a SliceFS block store.
    ///
    /// The store must NOT be mounted when this command is run.
    /// Use the background GC thread for in-process GC during active mounts.
    Gc {
        /// Path to the SliceFS block store directory.
        store: PathBuf,
        /// Full GC: an exact mark (sorted temp files in the store, bounded
        /// RAM) and no holes, so every dead object goes. Slower than the
        /// normal GC, which can keep a few dead objects.
        #[arg(long)]
        full: bool,
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
            Cmd::Unmount { mountpoint, store } => {
                assert_eq!(mountpoint, PathBuf::from("/mnt"));
                assert!(store.is_none());
            }
            _ => panic!("expected Unmount"),
        }
    }

    #[test]
    fn test_unmount_with_store() {
        let cli =
            Cli::try_parse_from(["slicefs", "unmount", "/mnt", "--store", "/data"]).unwrap();
        match cli.command {
            Cmd::Unmount { mountpoint, store } => {
                assert_eq!(mountpoint, PathBuf::from("/mnt"));
                assert_eq!(store.as_deref(), Some(PathBuf::from("/data").as_path()));
            }
            _ => panic!("expected Unmount with --store"),
        }
    }

    #[test]
    fn test_seed() {
        let cli = Cli::try_parse_from(["slicefs", "seed", "/data", "/src"]).unwrap();
        match cli.command {
            Cmd::Seed { store, source_dir, hash } => {
                assert_eq!(store, PathBuf::from("/data"));
                assert_eq!(source_dir, PathBuf::from("/src"));
                assert!(hash.is_none(), "no --hash: keep the store's recorded hash");
            }
            _ => panic!("expected Seed"),
        }
    }

    /// `--hash` takes a known hash name; an unknown one is refused with the
    /// list of the known names.
    #[test]
    fn test_hash_flag() {
        let cli = Cli::try_parse_from(["slicefs", "mount", "/mnt", "--store", "/data", "--hash", "blake3-256"]).unwrap();
        match cli.command {
            Cmd::Mount { hash, .. } => assert_eq!(hash.as_deref(), Some("blake3-256")),
            _ => panic!("expected Mount"),
        }
        let cli = Cli::try_parse_from(["slicefs", "seed", "/data", "/src", "--hash", "blake3-256"]).unwrap();
        match cli.command {
            Cmd::Seed { hash, .. } => assert_eq!(hash.as_deref(), Some("blake3-256")),
            _ => panic!("expected Seed"),
        }
        for args in [
            &["slicefs", "mount", "/mnt", "--store", "/data", "--hash", "md5"][..],
            &["slicefs", "seed", "/data", "/src", "--hash", "md5"][..],
        ] {
            let err = Cli::try_parse_from(args).err().expect("an unknown hash is refused").to_string();
            assert!(err.contains("unknown hash \"md5\"") && err.contains("blake3-256"), "{err}");
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
