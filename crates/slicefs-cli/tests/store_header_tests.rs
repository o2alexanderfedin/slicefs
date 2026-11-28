//! The open rules of the store header at the command level
//! (docs/design/store-format.md): every command checks the header before it
//! writes. An unknown incompat flag refuses every command; an unknown
//! ro_compat flag refuses the commands that write and lets the read commands
//! run; a store with data and no header is refused; a new store gets its
//! header first. On each refusal the store tree does not change.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

use slicefs_cli::header::{self, FORMAT_FILE};
use slicefs_cli::mount::{start, StartArgs};
use slicefs_cli::{content, gc, rehash, scrub, seed, snapshot, stats};
use tempfile::TempDir;

const INCOMPAT: &str = "incompat test-new-feature r-test\n";
const RO: &str = "ro_compat test-ro-feature r-test\n";

/// Names, sizes and blake3 hashes (read in 64 KiB pieces) of all files and
/// directories under `root`, by relative name.
fn tree(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut todo = vec![root.to_path_buf()];
    let mut buf = vec![0u8; 64 << 10];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            let m = std::fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                out.insert(format!("{rel}/"), String::new());
                todo.push(p);
                continue;
            }
            let mut f = std::fs::File::open(&p).unwrap();
            let mut h = blake3::Hasher::new();
            loop {
                let n = f.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            out.insert(rel, format!("{} {}", m.len(), h.finalize().to_hex()));
        }
    }
    out
}

/// A seeded store of one small file at `<dir>/store`, and its source at
/// `<dir>/src`.
fn seeded() -> TempDir {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"hello").unwrap();
    seed::run_seed(&dir.path().join("store"), &src, None).unwrap();
    dir
}

fn append_line(store: &Path, line: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(store.join(FORMAT_FILE)).unwrap();
    f.write_all(line.as_bytes()).unwrap();
}

type Outcome = Result<(), String>;

/// Each command that writes the store, as (name, result).
fn write_commands(dir: &Path) -> Vec<(&'static str, Outcome)> {
    let (store, src) = (&dir.join("store"), &dir.join("src"));
    let s = |r: Result<(), Box<dyn std::error::Error>>| r.map_err(|e| e.to_string());
    let io = |r: std::io::Result<content::Content>| r.map(drop).map_err(|e| e.to_string());
    vec![
        ("gc", gc::gc_report(store, false).map(drop).map_err(|e| e.to_string())),
        ("snapshot create", s(snapshot::run_snapshot_create(store, None))),
        ("snapshot switch", s(snapshot::run_snapshot_switch(store, "1"))),
        ("seed", s(seed::run_seed(store, src, None))),
        ("rehash", s(rehash::run_rehash(store, "blake3-256", false))),
        ("content::open", io(content::open(store))),
    ]
}

/// Each command that only reads the store, as (name, result).
fn read_commands(dir: &Path) -> Vec<(&'static str, Outcome)> {
    let store = &dir.join("store");
    let s = |r: Result<(), Box<dyn std::error::Error>>| r.map_err(|e| e.to_string());
    vec![
        ("stats", s(stats::run_stats(store, false))),
        ("scrub", s(scrub::run_scrub(store, false))),
        ("snapshot list", s(snapshot::run_snapshot_list(store))),
        ("content::open_read_only", content::open_read_only(store).map(drop).map_err(|e| e.to_string())),
    ]
}

fn mount(store: &Path, args: &StartArgs) -> Outcome {
    start(store, args, None).map(|s| s.stop()).map_err(|e| e.to_string())
}

/// Assert that each result is an error whose text has every part of `parts`.
fn assert_refused(results: &[(&str, Outcome)], parts: &[&str]) {
    for (name, r) in results {
        let err = r.as_ref().err().unwrap_or_else(|| panic!("{name} is refused"));
        assert!(parts.iter().all(|p| err.contains(p)), "{name}: {err}");
    }
}

/// [Incompat]: an unknown incompat flag refuses every command; the message
/// names the flag and its release.
#[test]
fn incompat_flag_is_refused_at_every_open_point() {
    let dir = seeded();
    let store = dir.path().join("store");
    append_line(&store, INCOMPAT);
    let before = tree(&store);
    let mut all = write_commands(dir.path());
    all.extend(read_commands(dir.path()));
    all.push(("mount", mount(&store, &StartArgs::default())));
    assert_refused(&all, &["test-new-feature", "r-test"]);
    assert_eq!(tree(&store), before, "the store tree does not change");
}

/// [Ro-compat]: an unknown ro_compat flag refuses the commands that write,
/// also gc of a mounted store (no `gc.request` is written).
#[test]
fn ro_compat_flag_refuses_write_commands() {
    let dir = seeded();
    let store = dir.path().join("store");
    append_line(&store, RO);
    let before = tree(&store);
    assert_refused(&write_commands(dir.path()), &["test-ro-feature", "r-test"]);
    assert_eq!(tree(&store), before, "the store tree does not change");

    std::fs::write(store.join("mount.lock"), b"").unwrap();
    let locked = tree(&store);
    assert_refused(&[("gc of a mounted store", gc::gc_report(&store, false).map(drop).map_err(|e| e.to_string()))], &["test-ro-feature"]);
    assert_eq!(tree(&store), locked, "no gc.request is written");
}

/// [Ro-compat]: the read commands run on a store with an unknown ro_compat
/// flag and change nothing.
#[test]
fn ro_compat_flag_allows_read_commands() {
    let dir = seeded();
    let store = dir.path().join("store");
    append_line(&store, RO);
    let before = tree(&store);
    for (name, r) in read_commands(dir.path()) {
        assert!(r.is_ok(), "{name}: {r:?}");
    }
    assert_eq!(tree(&store), before, "the store tree does not change");
}

/// [Ro options]: a ro_compat mount refuses `--hash` and `--auto-snapshot`.
#[test]
fn ro_compat_mount_refuses_hash_and_auto_snapshot() {
    let dir = seeded();
    let store = dir.path().join("store");
    append_line(&store, RO);
    let before = tree(&store);
    let hash = StartArgs { hash: Some("blake3-256".to_string()), ..Default::default() };
    assert_refused(&[("mount --hash", mount(&store, &hash))], &["--hash", "test-ro-feature"]);
    let auto = StartArgs { auto_snapshot: true, ..Default::default() };
    assert_refused(&[("mount --auto-snapshot", mount(&store, &auto))], &["--auto-snapshot", "test-ro-feature"]);
    assert_eq!(tree(&store), before, "the store tree does not change");
}

/// [New store]: a store with data and no header is refused by every
/// command; no header is written.
#[test]
fn data_and_no_header_is_refused_at_every_open_point() {
    let dir = seeded();
    let store = dir.path().join("store");
    std::fs::remove_file(store.join(FORMAT_FILE)).unwrap();
    let before = tree(&store);
    let mut all = write_commands(dir.path());
    all.extend(read_commands(dir.path()));
    all.push(("mount", mount(&store, &StartArgs::default())));
    assert_refused(&all, &["FORMAT is missing"]);
    assert_eq!(tree(&store), before, "no header is written");
}

/// [Number]: a format-6 store is refused with the older-SliceFS message.
#[test]
fn a_format_6_store_is_refused() {
    let dir = seeded();
    let store = dir.path().join("store");
    let text = std::fs::read_to_string(store.join(FORMAT_FILE)).unwrap();
    let (_, rest) = text.split_once('\n').unwrap();
    std::fs::write(store.join(FORMAT_FILE), format!("slicefs-store 6 (metadata and file content in packs, tagged object IDs)\n{rest}")).unwrap();
    let results = [
        ("mount", mount(&store, &StartArgs::default())),
        ("stats", stats::run_stats(&store, false).map_err(|e| e.to_string())),
    ];
    assert_refused(&results, &["older SliceFS (store format 6, no feature flags)"]);
}

/// [Number]: a format-7 store (made by release v2.3.0) is refused by every
/// command, with the message that names the upgrade page; no file changes
/// and no lock file is left.
#[test]
fn a_format_7_store_is_refused() {
    let dir = seeded();
    let store = dir.path().join("store");
    let text = std::fs::read_to_string(store.join(FORMAT_FILE)).unwrap();
    let (_, rest) = text.split_once('\n').unwrap();
    std::fs::write(store.join(FORMAT_FILE), format!("slicefs-store 7 (store header with feature flags)\n{rest}")).unwrap();
    let before = tree(&store);
    let mut all = write_commands(dir.path());
    all.extend(read_commands(dir.path()));
    all.push(("mount", mount(&store, &StartArgs::default())));
    assert_refused(&all, &["store format 7, flat metadata maps", "docs/user/upgrade.md"]);
    assert_eq!(tree(&store), before, "the store tree does not change");
}

/// [IN-02]: a format 8 store whose root record fields are flat maps (a
/// store of an interim build) is refused by the writable mount, the
/// read-only mount and `start` with M19, which names the store; no file of
/// the store changes and no lock file is left.
#[test]
fn an_interim_flat_map_store_is_refused_with_m19() {
    use metadata::store_io::put_value;
    use metadata::wal::WalConfig;
    use slicefs_cli::mount::{load_store, load_store_read_only};

    let dir = seeded();
    let store = dir.path().join("store");
    {
        // A 197-byte root record whose five map fields are plain values,
        // logged as the last root.
        let (meta, io, _locks) = load_store(&store, WalConfig::PerOp, None).unwrap();
        let cs = std::sync::Arc::clone(io.lock().unwrap().store());
        let mut root = Vec::new();
        root.extend_from_slice(&1u64.to_le_bytes());
        root.extend_from_slice(&3u64.to_le_bytes());
        for i in 0..5 {
            root.extend_from_slice(&put_value(&cs, format!("flat map {i}").as_bytes()).unwrap().0);
        }
        root.extend_from_slice(&5u64.to_le_bytes());
        root.extend_from_slice(&2u64.to_le_bytes());
        assert_eq!(root.len(), 197);
        let key = put_value(&cs, &root).unwrap();
        cs.sync().unwrap();
        meta.commit_root(key).unwrap();
        meta.shutdown_wal().unwrap();
    }
    assert!(!store.join("mount.lock").exists());
    let before = tree(&store);
    let m19 = header::msg_interim_flat_maps(&store);
    assert!(m19.contains(&store.display().to_string()), "{m19}");

    let flag = header::Flag {
        class: header::Class::RoCompat,
        name: "test-ro-feature".to_string(),
        release: "r-test".to_string(),
    };
    let results = [
        ("load_store", load_store(&store, WalConfig::PerOp, None).map(drop).map_err(|e| e.to_string())),
        ("load_store_read_only", load_store_read_only(&store, &flag).map(drop).map_err(|e| e.to_string())),
        ("mount", mount(&store, &StartArgs::default())),
    ];
    for (name, r) in &results {
        assert_eq!(r.as_ref().err(), Some(&m19), "{name}");
    }
    assert_eq!(tree(&store), before, "the store tree does not change");
}

/// [Durable first]: seed into a new directory writes the header of a new
/// store (format 8, the default hash, no flag line).
#[test]
fn seed_writes_the_header_first() {
    let dir = seeded();
    let text = std::fs::read_to_string(dir.path().join("store").join(FORMAT_FILE)).unwrap();
    assert_eq!(text, header::marker(chunk_store::DEFAULT_HASHER));
}

/// [Empty store reads]: stats of an empty store with no header writes
/// nothing.
#[test]
fn an_empty_store_read_writes_nothing() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("segments")).unwrap();
    stats::run_stats(dir.path(), false).unwrap();
    assert!(!dir.path().join(FORMAT_FILE).exists(), "no header is written");
}

/// A command that writes an existing store (gc, snapshot create and
/// switch, rehash) refuses a path with no store and makes nothing there.
#[test]
fn a_write_command_on_no_store_makes_nothing() {
    let dir = TempDir::new().unwrap();
    let (missing, empty) = (dir.path().join("missing"), dir.path().join("empty"));
    std::fs::create_dir_all(&empty).unwrap();
    for store in [&missing, &empty] {
        let s = |r: Result<(), Box<dyn std::error::Error>>| r.map_err(|e| e.to_string());
        let results = [
            ("gc", gc::gc_report(store, false).map(drop).map_err(|e| e.to_string())),
            ("snapshot create", s(snapshot::run_snapshot_create(store, None))),
            ("snapshot switch", s(snapshot::run_snapshot_switch(store, "1"))),
            ("rehash", s(rehash::run_rehash(store, "blake3-256", false))),
        ];
        assert_refused(&results, &["failed to load segments: store not found at"]);
    }
    assert!(!missing.exists(), "no directory is made");
    assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0, "nothing is made");
}
