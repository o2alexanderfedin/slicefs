fn main() {
    if cfg!(target_os = "macos") {
        cc::Build::new()
            .file("shim.c")
            .flag("-Wno-unused-parameter")
            .compile("dedup_fi_shim_c");
    }
    println!("cargo:rerun-if-changed=shim.c");
    println!("cargo:rerun-if-changed=build.rs");
}
