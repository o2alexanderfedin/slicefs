//! Build script for slicefs-cli.
//!
//! On macOS, emits a linker flag so the `slicefs` binary finds
//! `libfuse-t.dylib` at runtime without a manual `install_name_tool` step.
//! On other platforms (Linux, etc.) nothing is emitted — the system FUSE
//! library is in standard linker search paths.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "macos" {
        // rpath so the binary resolves libfuse-t.dylib from the FUSE-T
        // default install location (/usr/local/lib) at runtime.
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/local/lib");
    }
}
