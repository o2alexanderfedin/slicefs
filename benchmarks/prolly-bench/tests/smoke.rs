//! Smoke test of the bench on a small generated tree (never real data,
//! D-06): every mode that is left runs. The modes that read the flat maps
//! of a product store are retired (tag `v2.3.0` keeps them).

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
    let roots = t.join("roots.txt");

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
        "scatter cluster insdel",
        "--next-tree",
        p(&tree2),
    ]);
    // Setup, 2 patterns x 2 K x 1 commit, version, then insdel x 2 K x 1 commit.
    let commits: Vec<&str> = lines(&out, "commit").into_iter().filter(|l| l.contains(" mode=whole ")).collect();
    assert_eq!(commits.len(), 8, "{out}");
    assert!(commits.iter().map(|l| num(l, "edits_delete")).sum::<u64>() >= 1, "{out}");
    let version = lines(&out, "version");
    assert_eq!(version.len(), 1, "{out}");
    assert!(num(version[0], "files_changed") >= 1, "{out}");
    assert_eq!(num(version[0], "files_created"), 20, "{out}");

    for mode in ["flat", "prolly"] {
        let store = t.join(format!("scale-{mode}"));
        let out = ok(&["scale", "--mode", mode, "--n", "5000", "--store", p(&store)]);
        assert_eq!(lines(&out, "scale").len(), 2, "{out}");
    }
}

/// One `synth` run of the smoke: the lines of kind `build`, `commit`,
/// `tree` and `check` and the exit code.
fn synth_run(t: &Path, name: &str, layout: &str, extra: &[&str]) -> String {
    let store = t.join(name);
    let mut args = vec!["synth", "--layout", layout, "--store", p(&store)];
    args.extend(extra);
    ok(&args)
}

#[test]
fn synth_smoke() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let t = tmp.path();
    // Task 1: the `one` pattern in every layout, and a swept config.
    for (layout, trees) in [("a", 5), ("b", 1), ("c", 2)] {
        let out = synth_run(t, &format!("one-{layout}"), layout, &["--n", "5000", "--commits", "2", "--patterns", "one"]);
        assert_eq!(lines(&out, "build").len(), 1, "{out}");
        let commits = lines(&out, "commit");
        assert_eq!(commits.len(), 2, "{out}");
        assert!(commits.iter().all(|l| l.contains(" source=synth ") && l.contains(" pattern=one ") && l.contains(" k=1 ")), "{out}");
        assert!(commits.iter().all(|l| num(l, "edits") >= 3), "{out}");
        // IN-05: the default `--l` is the product value, 8192.
        assert!(commits.iter().all(|l| l.contains(" l=8192 node_max=16384 ")), "{out}");
        // base and final trees
        assert_eq!(lines(&out, "tree").len(), 2 * trees, "{out}");
        let check = lines(&out, "check");
        assert_eq!(check.len(), 1, "{out}");
        assert!(check[0].contains(&format!(" layout={layout} ")) && check[0].ends_with(" ok=1"), "{out}");
    }
    let out = synth_run(t, "one-wide", "a", &["--n", "5000", "--commits", "2", "--patterns", "one", "--l", "8192", "--node-max", "32768"]);
    assert!(lines(&out, "commit").iter().all(|l| l.contains(" l=8192 node_max=32768 ")), "{out}");
    assert!(lines(&out, "check")[0].ends_with(" ok=1"), "{out}");

    // Task 2: the whole mix, in layouts b and c.
    for layout in ["b", "c"] {
        let out = synth_run(t, &format!("mix-{layout}"), layout, &["--n", "20000", "--commits", "2", "--ks", "10 40"]);
        let commits = lines(&out, "commit");
        let of = |pattern: &str| -> Vec<&str> {
            commits.iter().copied().filter(|l| l.contains(&format!(" pattern={pattern} "))).collect()
        };
        assert_eq!(of("one").len(), 2, "{out}");
        assert_eq!(of("mixed").len(), 4, "{out}");
        let insdel = of("insdel");
        assert_eq!(insdel.len(), 4, "{out}");
        assert!(insdel.iter().all(|l| num(l, "edits") >= num(l, "k")), "{out}");
        assert_eq!(commits.len(), 10, "{out}");
        let check = lines(&out, "check");
        assert_eq!(check.len(), 1, "{out}");
        assert!(check[0].ends_with(" ok=1"), "{out}");
    }
}

#[test]
fn bad_arguments_exit_2() {
    assert_eq!(run(&["no-such-subcommand"]).0, 2);
    // Retired modes are unknown names.
    for mode in ["batches", "flat", "prolly", "maps", "layout"] {
        assert_eq!(run(&[mode, "--store", "no-such-store"]).0, 2, "{mode}");
    }
    // whole: the option checks come before any file access.
    assert_eq!(run(&["whole", "--store", "no-such-store", "--roots", "no-such-file", "--no-such-flag", "x"]).0, 2);
    assert_eq!(run(&["whole", "--store", "no-such-store"]).0, 2);
    assert_eq!(run(&["whole", "--store", "no-such-store", "--roots", "no-such-file", "--patterns", "x"]).0, 2);
    // synth: the option checks come before any file access too.
    let synth: [&[&str]; 5] = [
        &["--n", "5", "--layout", "a"],
        &["--n", "100", "--layout", "z"],
        &["--n", "100", "--layout", "a", "--patterns", "x"],
        &["--n", "100", "--layout", "a", "--ks", "26"],
        &["--n", "100", "--layout", "a", "--commits", "0"],
    ];
    for extra in synth {
        let mut args = vec!["synth", "--store", "no-such-store"];
        args.extend(extra);
        assert_eq!(run(&args).0, 2, "{args:?}");
    }
}
