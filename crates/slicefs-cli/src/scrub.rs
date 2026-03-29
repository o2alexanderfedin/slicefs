//! `scrub` subcommand — verify integrity of all stored blocks.
//!
//! ## Usage
//!
//! ```text
//! slicefs scrub <store>
//! slicefs scrub <store> --json
//! ```

use std::path::Path;

pub fn run_scrub(_store_path: &Path, _json: bool) -> Result<(), Box<dyn std::error::Error>> {
    todo!("scrub implementation in progress")
}
