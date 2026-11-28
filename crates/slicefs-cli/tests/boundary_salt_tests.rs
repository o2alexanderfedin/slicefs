//! The boundary salt of a store (Phase 25, D-10, D-11): a new store records
//! it in its header when `seed --boundary-salt` makes it, and an existing
//! store refuses any other salt with no change.

use std::path::{Path, PathBuf};

use chunk_store::{ChunkStore, DEFAULT_HASHER, ObjectId, Options};
use metadata::segment::load_store_from_segments;
use metadata::store_io::get_value;
use slicefs_cli::content::{self, store_io, store_io_read_only};
use slicefs_cli::rehash::{rehash_report_with, RehashArgs};
use slicefs_traits::key::KEY_BYTES;
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

/// Offset of the manifest tree root in the root record (pinned by the
/// `root_record_layout` unit test of `metadata`): root dir ino, next ino,
/// then the inode and directory maps.
const MANIFEST_DATA: usize = 8 + 8 + 2 * KEY_BYTES;

/// A source directory with enough files that the manifest tree has several
/// leaves (one 41-byte entry per file, a leaf is about 8 KiB).
fn big_source(dir: &Path) -> PathBuf {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..1_500u32 {
        std::fs::write(src.join(format!("f{i:04}")), format!("file {i}")).unwrap();
    }
    src
}

/// The (key, value) entries of a tree, in key order.
type Entries = Vec<(Vec<u8>, Vec<u8>)>;

/// The manifest tree of the current root of `store`: its root and its
/// entries in key order.
fn manifest_tree(store: &Path) -> (ObjectId, Entries) {
    let (root, _) = load_store_from_segments(&store.join("segments")).unwrap();
    let cs = content::open_read_only(store).unwrap();
    let rec = get_value(&cs, &root.unwrap()).unwrap();
    let tree = ObjectId(rec[MANIFEST_DATA..MANIFEST_DATA + KEY_BYTES].try_into().unwrap());
    let entries = prolly::iter(&cs, &tree).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert!(prolly::height(&prolly::Ctx::uncached(&cs), &tree).unwrap() >= 2, "a tree of several nodes");
    (tree, entries)
}

/// The root of `entries` built with `salt`, with the write hash `hash`, in
/// a scratch store.
fn built_root(entries: &[(Vec<u8>, Vec<u8>)], salt: Option<[u8; 32]>, hash: &str) -> ObjectId {
    let dir = TempDir::new().unwrap();
    let hasher = chunk_store::hash::hasher_by_name(hash).unwrap();
    let cs = ChunkStore::open(dir.path(), Options { hasher, ..Options::default() }).unwrap();
    let cfg = prolly::Config::DEFAULT.with_salt(salt);
    prolly::build(&cs, cfg, entries.iter().map(|(k, v)| (k, v))).unwrap().0
}

fn salt_a() -> [u8; 32] {
    header::parse_salt_hex(HEX_A).unwrap()
}

/// A salted store writes salted trees: the manifest tree is the one that
/// `prolly::build` makes for its entries with the salt, and not the one
/// with no salt.
#[test]
fn a_salted_store_writes_salted_trees() {
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("store");
    seed::run_seed_with_salt(&store, &big_source(dir.path()), None, None, Some(value(HEX_A))).unwrap();
    let (root, entries) = manifest_tree(&store);
    assert!(entries.len() >= 1_500, "{}", entries.len());
    assert_eq!(root, built_root(&entries, Some(salt_a()), "blake3-256"));
    assert_ne!(root, built_root(&entries, None, "blake3-256"));
}

/// A store with no salt writes the unsalted trees.
#[test]
fn an_unsalted_store_writes_unsalted_trees() {
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("store");
    seed::run_seed_with_salt(&store, &big_source(dir.path()), None, None, None).unwrap();
    let (root, entries) = manifest_tree(&store);
    assert_eq!(root, built_root(&entries, None, "blake3-256"));
    assert_ne!(root, built_root(&entries, Some(salt_a()), "blake3-256"));
}

/// Both ways to open the node store of a store give it the salt of the
/// header (every writer of a tree opens it this way).
#[test]
fn the_node_store_has_the_salt_of_the_header() {
    let dir = TempDir::new().unwrap();
    let (salted, plain) = (dir.path().join("salted"), dir.path().join("plain"));
    seed::run_seed_with_salt(&salted, &source(dir.path()), None, None, Some(value(HEX_A))).unwrap();
    seed::run_seed_with_salt(&plain, &source(dir.path()), None, None, None).unwrap();
    assert_eq!(store_io(&salted).unwrap().tree_config().salt(), Some(&salt_a()));
    assert_eq!(store_io_read_only(&salted).unwrap().tree_config().salt(), Some(&salt_a()));
    assert_eq!(store_io(&plain).unwrap().tree_config().salt(), None);
    assert_eq!(store_io_read_only(&plain).unwrap().tree_config().salt(), None);
}

/// A rehash of a salted store rebuilds its manifest tree with the salt:
/// the new tree is the salted build of its own entries.
#[test]
fn a_rehash_keeps_salted_trees() {
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("store");
    seed::run_seed_with_salt(&store, &big_source(dir.path()), None, None, Some(value(HEX_A))).unwrap();
    let (before, _) = manifest_tree(&store);
    rehash_report_with(&RehashArgs::new(&store, "sha256", false), &mut |_| {}).unwrap();
    let (root, entries) = manifest_tree(&store);
    assert_ne!(root, before);
    assert_eq!(root.tag(), 0x12, "a tree of the target hash");
    assert_eq!(root, built_root(&entries, Some(salt_a()), "sha256"));
    assert_ne!(root, built_root(&entries, None, "sha256"));
}
