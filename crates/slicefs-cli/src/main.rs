//! SliceFS binary entry point.
//!
//! Parses the CLI arguments and dispatches to the appropriate subcommand.

mod cli;
mod filesystem;
mod gc;
mod mount;
mod seed;
mod snapshot;
mod store_io;
mod unmount;

use clap::Parser;
use cli::{Cli, Cmd};

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Cmd::Mount { mountpoint, store, noatime, cache_size, wal_strategy, compressor, compressor_level, snapshot, auto_snapshot, .. } => {
            if let Err(e) = mount::run_mount(
                &store,
                &mountpoint,
                noatime,
                cache_size,
                wal_strategy.as_deref(),
                &compressor,
                compressor_level,
                snapshot.as_deref(),
                auto_snapshot,
            ) {
                eprintln!("slicefs mount error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Unmount { mountpoint } => {
            if let Err(e) = unmount::run_unmount(&mountpoint) {
                eprintln!("slicefs unmount error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Seed { store, source_dir } => {
            if let Err(e) = seed::run_seed(&store, &source_dir) {
                eprintln!("slicefs seed error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Gc { store } => {
            if let Err(e) = gc::run_gc(&store) {
                eprintln!("slicefs gc error: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Snapshot { action } => {
            if let Err(e) = snapshot::run_snapshot(action) {
                eprintln!("slicefs snapshot error: {e}");
                std::process::exit(1);
            }
        }
    }
}
