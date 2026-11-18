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

#[test]
fn bad_arguments_exit_with_code_2() {
    let out = bench(&["open", "--dir"]);
    assert_eq!(out.status.code(), Some(2));
    let out = bench(&["open", "--dir", "x", "--no-such-flag", "1"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
}
