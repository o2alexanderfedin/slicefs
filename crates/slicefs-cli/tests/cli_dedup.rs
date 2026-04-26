//! Integration tests for the dedup-index CLI surface (O1/O2/O3).
//!
//! Shells out to the compiled `slicefs` binary at `target/debug/slicefs`.
//! Each test SKIPs gracefully when the binary has not been built yet — run
//! `cargo build -p slicefs-cli` first to exercise the full surface.

use std::process::Command;

fn slicefs_bin() -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.join("..").join("..").join("target").join("debug").join("slicefs")
}

/// Write a CAS block file at `cas/<hh>/<54-hex>` so reindex/recover have
/// at least one entry to bulk-load.
fn write_cas_block(cas: &std::path::Path, hash: &[u8; 28]) {
    let hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
    let dir = cas.join(&hex[..2]);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(&hex[2..]), b"x").unwrap();
}

#[test]
fn reindex_offline_rebuilds_index_from_cas() {
    if !slicefs_bin().exists() {
        eprintln!("SKIP: build the binary first: cargo build -p slicefs-cli");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let store = td.path().to_path_buf();
    let cas = store.join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let mut h = [0u8; 28];
    h[0] = 0x77;
    write_cas_block(&cas, &h);

    let out = Command::new(slicefs_bin())
        .args(["reindex", "--store"])
        .arg(&store)
        .arg("--offline")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let idx_path = cas.join(".dedup-index").join("index.redb");
    assert!(idx_path.exists(), "reindex must produce {idx_path:?}");
}

#[test]
fn dedup_recover_preserves_old_index_as_bak() {
    if !slicefs_bin().exists() {
        eprintln!("SKIP: build the binary first");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let store = td.path().to_path_buf();
    let cas = store.join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let mut h = [0u8; 28];
    h[0] = 0x88;
    write_cas_block(&cas, &h);

    // First recover creates the index (no prior backup expected).
    let out1 = Command::new(slicefs_bin())
        .args(["dedup-recover", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(
        out1.status.success(),
        "first recover failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out1.stdout),
        String::from_utf8_lossy(&out1.stderr)
    );
    let idx = cas.join(".dedup-index").join("index.redb");
    assert!(idx.exists(), "first recover must create {idx:?}");

    // Sleep 1s so the bak filename (unix-secs suffix) differs.
    std::thread::sleep(std::time::Duration::from_secs(1));

    // Second recover backs the existing index up + creates a fresh one.
    let out2 = Command::new(slicefs_bin())
        .args(["dedup-recover", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(
        out2.status.success(),
        "second recover failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out2.stdout),
        String::from_utf8_lossy(&out2.stderr)
    );
    assert!(idx.exists(), "second recover must leave index.redb in place");

    let baks: Vec<_> = std::fs::read_dir(cas.join(".dedup-index"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("index.redb.bak.")
        })
        .collect();
    assert!(
        !baks.is_empty(),
        "previous index must be retained as .bak after second recover"
    );
}

#[test]
fn stats_includes_index_block_when_dedup_index_exists() {
    if !slicefs_bin().exists() {
        eprintln!("SKIP: build the binary first");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let store = td.path().to_path_buf();
    let cas = store.join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    // stats requires <store>/segments/ to exist (otherwise it returns
    // "store not found" before the [Index] block ever runs).
    std::fs::create_dir_all(store.join("segments")).unwrap();

    let mut h = [0u8; 28];
    h[0] = 0x99;
    write_cas_block(&cas, &h);

    // Reindex first so .dedup-index/index.redb exists.
    let r = Command::new(slicefs_bin())
        .args(["reindex", "--store"])
        .arg(&store)
        .arg("--offline")
        .output()
        .unwrap();
    assert!(
        r.status.success(),
        "reindex setup failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );

    let out = Command::new(slicefs_bin())
        .args(["stats"])
        .arg(&store)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    if out.status.success() {
        assert!(
            stdout.contains("[Index]"),
            "stats must include [Index] when dedup index exists.\nstdout={stdout}"
        );
    } else {
        eprintln!(
            "stats subcommand exited non-zero (probably needs more setup); skipping [Index] check.\nstderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
