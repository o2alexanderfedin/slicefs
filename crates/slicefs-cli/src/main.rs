//! SliceFS binary entry point.
//!
//! Parses the CLI arguments and dispatches to the appropriate subcommand.

mod cli;
mod filesystem;
mod mount;
mod seed;
mod store_io;
mod unmount;

use clap::Parser;
use cli::{Cli, Cmd};

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Cmd::Mount { mountpoint, store, noatime, cache_size, .. } => {
            if let Err(e) = mount::run_mount(&store, &mountpoint, noatime, cache_size) {
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
    }
}
