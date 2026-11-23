//! SliceFS binary entry point.
//!
//! Parses the CLI arguments and dispatches to the appropriate subcommand.

use slicefs_cli::{cli, gc, mount, scrub, seed, snapshot, stats, unmount};
use clap::Parser;
use cli::{Cli, Cmd};

fn main() {
    let cli = Cli::parse();
    let json = cli.json;

    match cli.command {
        Cmd::Mount { mountpoint, store, noatime, allow_other, cache_size, wal_strategy, snapshot, auto_snapshot, backend, force, commit_interval, gc_interval, compression, hash, dirty_budget, dirty_limit } => {
            let dirty = slicefs_cli::filesystem::DirtyConfig { budget: dirty_budget, limit: dirty_limit, ..Default::default() };
            if let Err(e) = mount::run_mount(
                &store,
                &mountpoint,
                noatime,
                allow_other,
                cache_size,
                wal_strategy.as_deref(),
                snapshot.as_deref(),
                auto_snapshot,
                backend.as_deref(),
                force,
                commit_interval,
                gc_interval,
                compression,
                hash.as_deref(),
                dirty,
            ) {
                if json {
                    eprintln!("{{\"error\": \"{e}\"}}");
                } else {
                    eprintln!("slicefs mount error: {e}");
                }
                std::process::exit(1);
            }
        }
        Cmd::Unmount { mountpoint, store } => {
            if let Err(e) = unmount::run_unmount(&mountpoint, store.as_deref()) {
                if json {
                    eprintln!("{{\"error\": \"{e}\"}}");
                } else {
                    eprintln!("slicefs unmount error: {e}");
                }
                std::process::exit(1);
            }
        }
        Cmd::Seed { store, source_dir, hash } => {
            if let Err(e) = seed::run_seed(&store, &source_dir, hash.as_deref()) {
                if json {
                    eprintln!("{{\"error\": \"{e}\"}}");
                } else {
                    eprintln!("slicefs seed error: {e}");
                }
                std::process::exit(1);
            }
        }
        Cmd::Gc { store, full } => {
            if let Err(e) = gc::run_gc_mode(&store, full) {
                if json {
                    eprintln!("{{\"error\": \"{e}\"}}");
                } else {
                    eprintln!("slicefs gc error: {e}");
                }
                std::process::exit(1);
            }
        }
        Cmd::Snapshot { action } => {
            if let Err(e) = snapshot::run_snapshot(action) {
                if json {
                    eprintln!("{{\"error\": \"{e}\"}}");
                } else {
                    eprintln!("slicefs snapshot error: {e}");
                }
                std::process::exit(1);
            }
        }
        Cmd::Stats { store } => {
            if let Err(e) = stats::run_stats(&store, json) {
                if json {
                    eprintln!("{{\"error\": \"{e}\"}}");
                } else {
                    eprintln!("slicefs stats error: {e}");
                }
                std::process::exit(1);
            }
        }
        Cmd::Scrub { store } => {
            if let Err(e) = scrub::run_scrub(&store, json) {
                if json {
                    eprintln!("{{\"error\": \"{e}\"}}");
                } else {
                    eprintln!("slicefs scrub error: {e}");
                }
                std::process::exit(1);
            }
        }
    }
}
