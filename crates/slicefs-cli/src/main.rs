//! SliceFS binary entry point.
//!
//! Parses the CLI arguments and dispatches to the appropriate subcommand.
//! Each arm is stubbed with `todo!` — Plans 02 and 03 fill in the logic.

mod cli;
mod filesystem;
mod seed;
mod store_io;

use clap::Parser;
use cli::{Cli, Cmd};

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Cmd::Mount { .. } => {
            todo!("mount")
        }
        Cmd::Unmount { .. } => {
            todo!("unmount")
        }
        Cmd::Seed { store, source_dir } => {
            if let Err(e) = seed::run_seed(&store, &source_dir) {
                eprintln!("slicefs seed error: {e}");
                std::process::exit(1);
            }
        }
    }
}
