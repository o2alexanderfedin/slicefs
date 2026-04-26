//! SliceFS binary entry point.
//!
//! Parses the CLI arguments and dispatches to the appropriate subcommand.

mod backend;
mod cli;
mod dedup_recover;
mod filesystem;
mod fuse_callbacks;
mod handlers;
mod gc;
mod mount;
mod reindex;
mod scrub;
mod seed;
mod snapshot;
mod stats;
mod store_io;
mod unmount;
mod util;

use clap::Parser;
use cli::{Cli, Cmd};

fn main() {
    let cli = Cli::parse();
    let json = cli.json;

    match cli.command {
        Cmd::Mount { mountpoint, store, noatime, allow_other, cache_size, wal_strategy, snapshot, auto_snapshot, backend, force } => {
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
        Cmd::Seed { store, source_dir } => {
            if let Err(e) = seed::run_seed(&store, &source_dir) {
                if json {
                    eprintln!("{{\"error\": \"{e}\"}}");
                } else {
                    eprintln!("slicefs seed error: {e}");
                }
                std::process::exit(1);
            }
        }
        Cmd::Gc { store } => {
            if let Err(e) = gc::run_gc(&store) {
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
        Cmd::Reindex { store, offline } => {
            if let Err(e) = reindex::run_reindex(&store, offline) {
                if json {
                    eprintln!("{{\"error\": \"{e}\"}}");
                } else {
                    eprintln!("slicefs reindex error: {e}");
                }
                // Online reindex (offline=false) is a usage error → exit 2.
                let code = if !offline { 2 } else { 1 };
                std::process::exit(code);
            }
        }
        Cmd::DedupRecover { store } => {
            if let Err(e) = dedup_recover::run_dedup_recover(&store) {
                if json {
                    eprintln!("{{\"error\": \"{e}\"}}");
                } else {
                    eprintln!("slicefs dedup-recover error: {e}");
                }
                std::process::exit(1);
            }
        }
    }
}
