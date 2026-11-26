//! The bench links the `slicefs-cli` library, and so the FUSE library. On
//! macOS this gives the binary the rpath that `crates/slicefs-cli/build.rs`
//! gives `slicefs` (a link argument of a dependency does not reach this
//! binary), so it runs outside `cargo` too.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "macos" {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/local/lib");
    }
}
