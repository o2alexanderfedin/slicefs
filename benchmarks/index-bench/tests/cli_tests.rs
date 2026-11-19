//! End-to-end tests of the index-bench binary on small stores. They check the
//! tool, not timings: no test asserts a duration.

use std::path::Path;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_index-bench");

fn bench(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("start index-bench")
}

/// Run a step that must succeed; its stdout.
fn ok(args: &[&str]) -> String {
    let out = bench(args);
    assert!(
        out.status.success(),
        "index-bench {args:?} failed: {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The value of `key` in `key=value` output (the first one).
fn value<'a>(out: &'a str, key: &str) -> Option<&'a str> {
    out.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
}

fn p(d: &Path) -> &str {
    d.to_str().unwrap()
}

fn create(d: &Path, objects: &str, extra: &[&str]) -> String {
    let mut args = vec!["create", "--dir", p(d), "--objects", objects];
    args.extend_from_slice(extra);
    ok(&args)
}

#[test]
fn run_creates_and_opens_a_small_store() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("s");
    let out = ok(&["run", "--dir", p(&d), "--objects", "20000", "--reps", "1"]);
    assert!(out.lines().any(|l| l == "step=create"), "{out}");
    assert!(out.lines().filter(|l| *l == "step=open").count() >= 2, "{out}");
    assert!(out.lines().any(|l| l == "objects=20000"), "{out}");
    assert!(out.lines().any(|l| l.starts_with("| OS | Objects |")), "{out}");
    assert!(out.lines().any(|l| l == "step=lookup"), "{out}");
    assert!(out.lines().any(|l| l == "## Lookups"), "{out}");
    assert!(out.lines().any(|l| l.starts_with("| OS | Objects | contains hit ns |")), "{out}");
    assert_eq!(out.lines().filter(|l| *l == "step=recover").count(), 4, "{out}");
    assert!(out.lines().any(|l| l == "# open check (not in the summary)"), "{out}");
    assert!(out.lines().any(|l| l == "## Recovery"), "{out}");
    assert!(out.lines().any(|l| l.starts_with("| OS | Objects | Scope | Mode |")), "{out}");
    assert!(!d.join("bench-saved").exists());
    assert!(!out.contains(p(&d)), "run must not print the store path:\n{out}");
    assert!(d.join("index-bench.txt").is_file());
}

#[test]
fn open_reports_rss_per_object() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("s");
    create(&d, "20000", &[]);
    let out = ok(&["open", "--dir", p(&d)]);
    assert_eq!(value(&out, "step"), Some("open"), "{out}");
    assert_eq!(value(&out, "objects"), Some("20000"), "{out}");
    let peak: f64 = value(&out, "peak_bytes_per_object").unwrap().parse().expect("a number");
    assert!(peak >= 0.0, "{out}");
}

/// Name and size of every file in the store's packs directory.
fn pack_files(d: &Path) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = std::fs::read_dir(d.join("packs"))
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.file_name().to_string_lossy().into_owned(), e.metadata().unwrap().len())
        })
        .collect();
    v.sort();
    v
}

fn positive(out: &str, key: &str) {
    let v: f64 = value(out, key).unwrap_or_else(|| panic!("no {key}:\n{out}")).parse().expect("a number");
    assert!(v > 0.0, "{key} is not positive:\n{out}");
}

#[test]
fn lookup_finds_hits_not_misses_and_does_not_change_the_store() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("s");
    create(&d, "5000", &[]);
    let before = pack_files(&d);
    let out = ok(&["lookup", "--dir", p(&d), "--hits", "2000", "--misses", "2000"]);
    assert_eq!(value(&out, "step"), Some("lookup"), "{out}");
    assert_eq!(value(&out, "objects"), Some("5000"), "{out}");
    assert_eq!(value(&out, "hits"), Some("2000"), "{out}");
    assert_eq!(value(&out, "misses"), Some("2000"), "{out}");
    for key in ["contains_hit_per_s", "contains_miss_per_s", "put_hit_per_s", "contains_hit_ns", "put_hit_ns"] {
        positive(&out, key);
    }
    assert_eq!(pack_files(&d), before, "lookup changed the pack files");
    let open = ok(&["open", "--dir", p(&d)]);
    assert_eq!(value(&open, "objects"), Some("5000"), "{open}");
}

#[test]
fn lookup_refuses_a_dir_without_the_marker() {
    let t = tempfile::tempdir().unwrap();
    let out = bench(&["lookup", "--dir", p(t.path())]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not an index-bench store"));
}

/// The store is as before a recover: the same pack and idx files with the
/// same sizes, one idx per pack, no saved-files directory, all objects.
fn assert_unchanged(d: &Path, before: &[(String, u64)], objects: &str) {
    let now = pack_files(d);
    let packs = now.iter().filter(|(n, _)| n.ends_with(".pack")).count();
    let idxs = now.iter().filter(|(n, _)| n.ends_with(".idx")).count();
    assert_eq!(idxs, packs, "every pack needs its idx: {now:?}");
    assert_eq!(now, before, "the pack directory changed");
    assert!(!d.join("bench-saved").exists(), "bench-saved left behind");
    let open = ok(&["open", "--dir", p(d)]);
    assert_eq!(value(&open, "objects"), Some(objects), "{open}");
}

fn recover(d: &Path, packs: &str, read_only: bool) -> String {
    let mut args = vec!["recover", "--dir", p(d), "--packs", packs];
    if read_only {
        args.push("--read-only");
    }
    ok(&args)
}

#[test]
fn recover_on_a_multi_pack_store_restores_every_idx() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("s");
    let created = create(&d, "5000", &["--pack-limit", "65536"]);
    let packs: u64 = value(&created, "packs").unwrap().parse().unwrap();
    assert!(packs > 2, "the test needs several packs:\n{created}");
    let before = pack_files(&d);
    let others = (packs - 1).to_string();

    let out = recover(&d, "1", false);
    assert_eq!(value(&out, "step"), Some("recover"), "{out}");
    assert_eq!(value(&out, "scope"), Some("one"), "{out}");
    assert_eq!(value(&out, "mode"), Some("rw"), "{out}");
    assert_eq!(value(&out, "packs_dropped"), Some("1"), "{out}");
    assert_eq!(value(&out, "sealed_packs"), Some("1"), "{out}");
    assert_eq!(value(&out, "active_packs"), Some("0"), "{out}");
    assert_eq!(value(&out, "objects"), Some("5000"), "{out}");
    assert_eq!(value(&out, "idx_identical"), Some("yes"), "{out}");
    positive(&out, "scanned_bytes");
    positive(&out, "scanned_objects");
    assert_unchanged(&d, &before, "5000");

    let out = recover(&d, "1", true);
    assert_eq!(value(&out, "mode"), Some("ro"), "{out}");
    assert_eq!(value(&out, "sealed_packs"), Some("1"), "{out}");
    assert_eq!(value(&out, "idx_identical"), Some("n/a"), "{out}");
    assert_eq!(value(&out, "objects"), Some("5000"), "{out}");
    assert_unchanged(&d, &before, "5000");

    let out = recover(&d, "all", false);
    assert_eq!(value(&out, "scope"), Some("all"), "{out}");
    assert_eq!(value(&out, "active_packs"), Some("1"), "{out}");
    assert_eq!(value(&out, "sealed_packs"), Some(others.as_str()), "{out}");
    assert_eq!(value(&out, "scanned_objects"), Some("5000"), "{out}");
    assert_eq!(value(&out, "idx_identical"), Some("yes"), "{out}");
    assert_eq!(value(&out, "objects"), Some("5000"), "{out}");
    assert_unchanged(&d, &before, "5000");

    let out = recover(&d, "all", true);
    assert_eq!(value(&out, "mode"), Some("ro"), "{out}");
    assert_eq!(value(&out, "active_packs"), Some("1"), "{out}");
    assert_eq!(value(&out, "sealed_packs"), Some(others.as_str()), "{out}");
    assert_eq!(value(&out, "objects"), Some("5000"), "{out}");
    assert_unchanged(&d, &before, "5000");
}

#[test]
fn recover_on_a_one_pack_store_takes_the_active_pack_path() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("s");
    create(&d, "1000", &[]);
    let before = pack_files(&d);
    for read_only in [false, true] {
        let out = recover(&d, "1", read_only);
        assert_eq!(value(&out, "active_packs"), Some("1"), "{out}");
        assert_eq!(value(&out, "sealed_packs"), Some("0"), "{out}");
        assert_eq!(value(&out, "scanned_objects"), Some("1000"), "{out}");
        assert_eq!(value(&out, "objects"), Some("1000"), "{out}");
        assert_unchanged(&d, &before, "1000");
    }
}

#[test]
fn recover_refuses_a_dir_without_the_marker() {
    let t = tempfile::tempdir().unwrap();
    let out = bench(&["recover", "--dir", p(t.path()), "--packs", "all"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not an index-bench store"));
}

#[test]
fn recover_refuses_a_store_with_saved_files_left() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("s");
    create(&d, "1000", &[]);
    let before = pack_files(&d);
    std::fs::create_dir(d.join("bench-saved")).unwrap();
    std::fs::write(d.join("bench-saved").join("0000000000000001.idx"), b"x").unwrap();
    let out = bench(&["recover", "--dir", p(&d), "--packs", "1"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("bench-saved"));
    assert_eq!(pack_files(&d), before, "a refused recover must not move files");
}

#[test]
fn create_refuses_a_non_empty_dir() {
    let t = tempfile::tempdir().unwrap();
    std::fs::write(t.path().join("other-file"), b"x").unwrap();
    let out = bench(&["create", "--dir", p(t.path()), "--objects", "10"]);
    assert!(!out.status.success());
    assert!(!out.stderr.is_empty());
    assert!(!t.path().join("packs").exists(), "create must not write into a non-empty dir");
}

#[test]
fn open_refuses_a_dir_without_the_marker() {
    let t = tempfile::tempdir().unwrap();
    let out = bench(&["open", "--dir", p(t.path())]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not an index-bench store"));
}

/// Sum of the sizes of every file under `d`, recursively, except (at the top)
/// the marker file and the saved-files directory.
fn store_bytes(d: &Path, top: bool) -> u64 {
    let mut sum = 0;
    for e in std::fs::read_dir(d).unwrap() {
        let e = e.unwrap();
        let name = e.file_name();
        if top && (name == "index-bench.txt" || name == "bench-saved") {
            continue;
        }
        let m = e.metadata().unwrap();
        sum += if m.is_dir() { store_bytes(&e.path(), false) } else { m.len() };
    }
    sum
}

fn num(out: &str, key: &str) -> u64 {
    let v = value(out, key).unwrap_or_else(|| panic!("no {key}:\n{out}"));
    v.parse().unwrap_or_else(|_| panic!("{key} is not a whole number:\n{out}"))
}

fn float(out: &str, key: &str) -> f64 {
    let v = value(out, key).unwrap_or_else(|| panic!("no {key}:\n{out}"));
    v.parse().unwrap_or_else(|_| panic!("{key} is not a number:\n{out}"))
}

#[test]
fn create_counts_the_whole_store() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("s");
    let out = create(&d, "5000", &["--memtable-limit", "500"]);
    let disk = num(&out, "disk_bytes");
    assert_eq!(disk, store_bytes(&d, true), "{out}");
    assert!(disk > store_bytes(&d.join("packs"), false), "disk_bytes counts only the packs:\n{out}");
    assert!(num(&out, "index_bytes") > 0, "{out}");
    assert_eq!(num(&out, "index_bytes"), store_bytes(&d.join("index"), false), "{out}");
}

#[test]
fn open_prints_the_index_keys() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("s");
    create(&d, "5000", &["--memtable-limit", "500"]);
    let out = ok(&["open", "--dir", p(&d)]);
    assert!(num(&out, "index_runs") >= 1, "{out}");
    for k in ["index_filter_bytes", "index_fence_bytes", "index_memtable_entries", "hwm_pack"] {
        num(&out, k);
    }
    // The RSS split: anonymous and file-backed pages, before and after.
    for k in ["rss_anon_before_bytes", "rss_anon_after_bytes", "rss_file_before_bytes", "rss_file_after_bytes"] {
        num(&out, k);
    }
    float(&out, "steady_anon_bytes_per_object");
    float(&out, "steady_file_bytes_per_object");
    // The old keys come first, in their old order.
    let keys: Vec<&str> = out.lines().filter_map(|l| l.split_once('=').map(|(k, _)| k)).collect();
    let steady = keys.iter().position(|k| *k == "steady_bytes_per_object").unwrap();
    let runs = keys.iter().position(|k| *k == "index_runs").unwrap();
    assert!(steady < runs, "{out}");
}

#[test]
fn recover_counts_only_the_scanned_packs() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("s");
    create(&d, "5000", &["--memtable-limit", "500", "--pack-limit", "65536"]);
    let before = pack_files(&d);
    let out = recover(&d, "all", false);
    assert!(num(&out, "covered_packs") >= 1, "{out}");
    assert!(num(&out, "covered_packs") <= num(&out, "packs_dropped"), "{out}");
    assert!(num(&out, "scanned_objects") < 5000, "{out}");
    assert_eq!(value(&out, "objects"), Some("5000"), "{out}");
    assert_unchanged(&d, &before, "5000");
}

#[test]
fn gc_reports_the_lock_hold() {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("s");
    create(&d, "5000", &["--memtable-limit", "500", "--pack-limit", "65536"]);
    let out = ok(&["gc", "--dir", p(&d)]);
    assert_eq!(value(&out, "step"), Some("gc"), "{out}");
    assert_eq!(value(&out, "live_every"), Some("4"), "{out}");
    assert_eq!(value(&out, "dead_run"), Some("1"), "{out}");
    assert_eq!(num(&out, "dead_objects"), 1250, "{out}");
    // The packs of this store are small, so the GC compacts them and
    // removes every dead object.
    let removed = num(&out, "removed_objects");
    assert_eq!(removed, 1250, "{out}");
    assert_eq!(removed + num(&out, "kept_objects"), 5000, "{out}");
    assert_eq!(num(&out, "objects_after"), 5000 - removed, "{out}");
    let wall = float(&out, "wall_s");
    for k in ["lock_hold_max_ms", "lock_hold_total_ms"] {
        let v = float(&out, k);
        assert!((0.0..=wall * 1000.0 + 1.0).contains(&v), "{k} is above the wall time:\n{out}");
    }
    assert!(float(&out, "lock_hold_max_ms") <= float(&out, "lock_hold_total_ms"), "{out}");
    for k in ["packs_punched", "packs_compacted", "packs_deleted", "max_rss_bytes"] {
        num(&out, k);
    }
    // The store no longer has N objects: every step refuses it now.
    for args in [
        vec!["open", "--dir", p(&d)],
        vec!["lookup", "--dir", p(&d)],
        vec!["recover", "--dir", p(&d), "--packs", "all"],
        vec!["gc", "--dir", p(&d)],
        vec!["run", "--dir", p(&d), "--objects", "5000"],
    ] {
        let r = bench(&args);
        assert!(!r.status.success(), "{args:?} accepted a store after gc");
        let err = String::from_utf8_lossy(&r.stderr);
        assert!(err.contains("gc step"), "{args:?}: {err}");
    }
}

#[test]
fn bad_arguments_exit_with_code_2() {
    let out = bench(&["open", "--dir"]);
    assert_eq!(out.status.code(), Some(2));
    let out = bench(&["open", "--dir", "x", "--no-such-flag", "1"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
}
