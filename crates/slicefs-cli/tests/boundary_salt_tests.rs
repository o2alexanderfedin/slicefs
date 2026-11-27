//! The boundary salt of a store (Phase 25, D-10, D-11): a new store records
//! it in its header when `seed --boundary-salt` makes it, and an existing
//! store refuses any other salt with no change.

use std::path::{Path, PathBuf};

use chunk_store::DEFAULT_HASHER;
use slicefs_cli::content::SaltOpt;
use slicefs_cli::header::{self, BOUNDARY_SALT_FLAG, BOUNDARY_SALT_RELEASE, FORMAT_FILE};
use slicefs_cli::seed;
use tempfile::TempDir;

const HEX_A: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const HEX_B: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";

fn value(hex: &str) -> SaltOpt {
    slicefs_cli::content::parse_boundary_salt(hex).unwrap()
}

/// A source directory with one small file.
fn source(dir: &Path) -> PathBuf {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("f"), b"ten bytes!").unwrap();
    src
}

fn format(store: &Path) -> String {
    std::fs::read_to_string(store.join(FORMAT_FILE)).unwrap()
}

fn parsed(store: &Path) -> header::Header {
    header::parse(store, &format(store)).unwrap()
}

fn salt_lines(text: &str) -> Vec<&str> {
    text.lines().filter(|l| l.starts_with("boundary-salt ")).collect()
}

fn flag_line() -> String {
    format!("ro_compat {BOUNDARY_SALT_FLAG} {BOUNDARY_SALT_RELEASE}")
}

#[test]
fn seed_with_a_random_salt_writes_the_line_and_flag() {
    let dir = TempDir::new().unwrap();
    let src = source(dir.path());
    let mut salts = Vec::new();
    for name in ["one", "two"] {
        let store = dir.path().join(name);
        seed::run_seed_with_salt(&store, &src, None, None, Some(SaltOpt::Random)).unwrap();
        let text = format(&store);
        let lines = salt_lines(&text);
        assert_eq!(lines.len(), 1, "{text}");
        let hex = lines[0].strip_prefix("boundary-salt ").unwrap();
        assert!(hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "{hex}");
        assert_eq!(text.lines().filter(|l| *l == flag_line()).count(), 1, "{text}");
        let salt = parsed(&store).boundary_salt().expect("the parser returns the salt");
        assert_eq!(header::salt_hex(&salt), hex);
        salts.push(salt);
    }
    assert_ne!(salts[0], salts[1], "two random salts differ");
}

#[test]
fn seed_with_a_hex_salt_round_trips() {
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("store");
    seed::run_seed_with_salt(&store, &source(dir.path()), None, None, Some(value(HEX_A))).unwrap();
    let text = format(&store);
    let h = parsed(&store);
    assert_eq!(header::salt_hex(&h.boundary_salt().unwrap()), HEX_A);
    assert!(text.lines().any(|l| l == format!("boundary-salt {HEX_A}")), "{text}");
    assert_eq!(h.to_text(), text);
    assert_eq!(text, header::marker_with_salt(DEFAULT_HASHER, chunk_store::ChunkProfile::DEFAULT, h.boundary_salt()));
}

#[test]
fn a_new_store_without_the_option_has_no_salt_line() {
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("store");
    seed::run_seed_with_salt(&store, &source(dir.path()), None, None, None).unwrap();
    assert_eq!(format(&store), header::marker(DEFAULT_HASHER));
    assert_eq!(parsed(&store).boundary_salt(), None);
}

#[test]
fn an_existing_store_refuses_another_salt() {
    let dir = TempDir::new().unwrap();
    let src = source(dir.path());
    let store = dir.path().join("store");
    seed::run_seed_with_salt(&store, &src, None, None, Some(value(HEX_A))).unwrap();
    let before = std::fs::read(store.join(FORMAT_FILE)).unwrap();

    // The same salt again: success, and the header is byte for byte the same.
    seed::run_seed_with_salt(&store, &src, None, None, Some(value(HEX_A))).unwrap();
    assert_eq!(std::fs::read(store.join(FORMAT_FILE)).unwrap(), before);

    // Another value, and `random`: M18, and no change.
    for (want, text) in [(value(HEX_B), HEX_B), (SaltOpt::Random, "random")] {
        let err = seed::run_seed_with_salt(&store, &src, None, None, Some(want)).expect_err(text).to_string();
        assert_eq!(err, header::msg_salt_differs(&store, true, text));
        assert!(err.contains("has a boundary salt") && err.contains("fixed when the store is made"), "{err}");
        assert_eq!(std::fs::read(store.join(FORMAT_FILE)).unwrap(), before, "{text}");
    }

    // No option: success, and the salt is kept.
    seed::run_seed_with_salt(&store, &src, None, None, None).unwrap();
    assert_eq!(std::fs::read(store.join(FORMAT_FILE)).unwrap(), before);
    assert_eq!(header::salt_hex(&parsed(&store).boundary_salt().unwrap()), HEX_A);
}

#[test]
fn an_unsalted_store_refuses_a_salt() {
    let dir = TempDir::new().unwrap();
    let src = source(dir.path());
    let store = dir.path().join("store");
    seed::run_seed_with_salt(&store, &src, None, None, None).unwrap();
    let before = std::fs::read(store.join(FORMAT_FILE)).unwrap();
    for (want, text) in [(value(HEX_A), HEX_A), (SaltOpt::Random, "random")] {
        let err = seed::run_seed_with_salt(&store, &src, None, None, Some(want)).expect_err(text).to_string();
        assert_eq!(err, header::msg_salt_differs(&store, false, text));
        assert!(err.contains("has no boundary salt"), "{err}");
        assert_eq!(std::fs::read(store.join(FORMAT_FILE)).unwrap(), before, "{text}");
    }
}

#[test]
fn two_parallel_salted_seeds_make_one_salt() {
    let dir = TempDir::new().unwrap();
    let src = source(dir.path());
    let store = dir.path().join("store");
    let barrier = std::sync::Barrier::new(2);
    let results: Vec<Result<(), String>> = std::thread::scope(|s| {
        let runs: Vec<_> = (0..2)
            .map(|_| {
                s.spawn(|| {
                    barrier.wait();
                    seed::run_seed_with_salt(&store, &src, None, None, Some(SaltOpt::Random)).map_err(|e| e.to_string())
                })
            })
            .collect();
        runs.into_iter().map(|r| r.join().unwrap()).collect()
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1, "{results:?}");
    let err = results.iter().find_map(|r| r.as_ref().err()).unwrap();
    assert!(err.contains("is in use") || err.contains("is another salt"), "{err}");
    let text = format(&store);
    assert_eq!(salt_lines(&text).len(), 1, "{text}");
    assert_eq!(text.lines().filter(|l| *l == flag_line()).count(), 1, "{text}");
}

#[test]
fn stats_lists_the_boundary_salt_flag() {
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("store");
    seed::run_seed_with_salt(&store, &source(dir.path()), None, None, Some(value(HEX_A))).unwrap();
    let stats = slicefs_cli::stats::store_stats(&store).unwrap();
    assert_eq!(stats.access, "read-write");
    let flags = &stats.flags.ro_compat;
    assert_eq!(flags.len(), 1, "{flags:?}");
    assert_eq!((flags[0].name.as_str(), flags[0].release.as_str(), flags[0].known), (BOUNDARY_SALT_FLAG, BOUNDARY_SALT_RELEASE, true));
    let v = serde_json::to_value(&stats).unwrap();
    assert_eq!(v["flags"]["ro_compat"][0]["name"], BOUNDARY_SALT_FLAG);
}
