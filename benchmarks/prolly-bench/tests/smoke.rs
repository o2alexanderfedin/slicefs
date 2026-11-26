//! Smoke test of the bench on a small generated tree (never real data,
//! D-06): every mode runs, and the replay matches the real final map.

use std::path::Path;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_prolly-bench");

/// Runs the bench; returns the exit code and stdout.
fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(BIN).args(args).stdin(Stdio::null()).output().expect("run prolly-bench");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        eprintln!("prolly-bench {args:?} stderr:\n{}", String::from_utf8_lossy(&out.stderr));
    }
    (out.status.code().unwrap_or(-1), stdout)
}

/// Runs the bench and asserts exit 0.
fn ok(args: &[&str]) -> String {
    let (code, out) = run(args);
    assert_eq!(code, 0, "prolly-bench {args:?}:\n{out}");
    out
}

fn p(path: &Path) -> &str {
    path.to_str().expect("utf-8 temp path")
}

/// The lines whose first word is `kind`.
fn lines<'a>(out: &'a str, kind: &str) -> Vec<&'a str> {
    out.lines().filter(|l| l.split_whitespace().next() == Some(kind)).collect()
}

/// The number value of `name=` in `line`.
fn num(line: &str, name: &str) -> u64 {
    let pre = format!("{name}=");
    line.split_whitespace()
        .find_map(|w| w.strip_prefix(pre.as_str()))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no number {name}= in: {line}"))
}

#[test]
fn smoke_end_to_end() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let t = tmp.path();
    let (tree, tree2, store) = (t.join("tree"), t.join("tree2"), t.join("store"));
    let (roots, batches, pstore) = (t.join("roots.txt"), t.join("batches"), t.join("prolly"));

    ok(&["gen", "tree", "--dir", p(&tree), "--files", "200", "--seed", "1"]);
    // The next version: the same 200 files, every 10th changed, 20 new.
    ok(&["gen", "tree", "--dir", p(&tree2), "--files", "220", "--seed", "1", "--change-every", "10"]);
    let out = ok(&["seed", "--tree", p(&tree), "--store", p(&store)]);
    assert_eq!(lines(&out, "seed").len(), 1, "{out}");

    let out = ok(&[
        "whole",
        "--store",
        p(&store),
        "--roots",
        p(&roots),
        "--ks",
        "1 4",
        "--commits",
        "1",
        "--patterns",
        "scatter cluster",
        "--next-tree",
        p(&tree2),
    ]);
    // Setup, 2 patterns x 2 K x 1 commit, version.
    let commits: Vec<&str> = lines(&out, "commit").into_iter().filter(|l| l.contains(" mode=whole ")).collect();
    assert_eq!(commits.len(), 6, "{out}");
    assert!(commits.iter().map(|l| num(l, "edits_delete")).sum::<u64>() >= 1, "{out}");
    let version = lines(&out, "version");
    assert_eq!(version.len(), 1, "{out}");
    assert!(num(version[0], "files_changed") >= 1, "{out}");
    assert_eq!(num(version[0], "files_created"), 20, "{out}");

    let out = ok(&["batches", "--store", p(&store), "--roots", p(&roots), "--out", p(&batches)]);
    let b1 = lines(&out, "batch").into_iter().find(|l| l.contains(" seq=1 ")).expect("a batch seq=1 line");
    assert!(num(b1, "puts") >= 1, "{b1}");

    let out = ok(&["prolly", "--batches", p(&batches), "--store", p(&pstore)]);
    let check = lines(&out, "check");
    assert_eq!(check.len(), 1, "{out}");
    assert!(check[0].contains("mode=prolly") && check[0].ends_with(" ok=1"), "{out}");
}

#[test]
fn bad_arguments_exit_2() {
    assert_eq!(run(&["no-such-subcommand"]).0, 2);
    assert_eq!(run(&["prolly", "--no-such-flag", "x"]).0, 2);
}
