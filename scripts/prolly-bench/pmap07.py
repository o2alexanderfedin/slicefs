#!/usr/bin/env python3
"""The pre-registered PMAP-07 decision rule of Phase 26, as code. The rule file
.planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/pmap07-rule.md says
the same in words. The limits below are fixed before any measurement and must
not change after the first decision run.

usage: pmap07.py --flat-commit F... --prolly-commit F... --flat-boost CSV --prolly-boost CSV
       pmap07.py --summary F...
       pmap07.py --self-test

Commit logs hold lines (other lines are ignored):
  sample tree=<t> n=<n> seq=<i> wall_ms=<f> cpu_ms=<f> nodes_read=<u|NA> nodes_written=<u|NA> presync_ms=<f|NA>
Boost CSVs are the results.csv of scripts/prolly-bench/bench.sh.

Check A (pre-sync): median cpu_ms of prolly <= 0.20 x flat AND median
(nodes_read + nodes_written) of prolly <= 0.20 x flat.
Check B (total): median wall_ms of prolly < flat.
Check C (Boost, not drop): rows mode=whole run=median for the patterns scatter,
cluster, insdel and K = 1, 10, 100 (9 rows); each row is better iff prolly
cpu_user_s + cpu_sys_s < flat AND total_ms < flat, and bytes ok iff prolly
new_stored_bytes <= 2 x flat; C passes iff all 9 rows are better and bytes ok.
Verdict: pass iff A, B and C pass.

Output lines: `check A ...`, `check B ...`, `row ...` and `check C ...`, `info
...` (the recorded rows), and the last line `verdict pmap07=<pass|fail>`.
--summary prints, per tree label, the sample count and the medians of wall_ms
and cpu_ms (no verdict).

Exit codes: 0 with a verdict or a summary; 2 (message on stderr) on a missing
file, a missing column, a non-number, an NA where a check needs a number,
fewer than 22 samples of a tree for a verdict (two rounds; 11 for
--summary), a tree label that is on both sides or on
more than one side's files, or a missing gated row; 1 when the self-test fails.
"""

import csv
import math
import os
import statistics
import subprocess
import sys
import tempfile

# The rule. Fixed before the first measurement.
A_LIMIT = 0.20          # check A: prolly <= 0.20 x flat (CPU and nodes)
C_BYTES_LIMIT = 2.0     # check C: new stored bytes <= 2 x flat
MIN_SAMPLES = 11        # samples of one round of a tree; a summary needs them
VERDICT_SAMPLES = 2 * MIN_SAMPLES   # samples of a tree for a verdict: two rounds (the rule file)
N_FILES = 1_000_000     # files of a decision run
GATED_PATTERNS = ("scatter", "cluster", "insdel")
GATED_KS = (1, 10, 100)


class Refuse(Exception):
    """A required input is missing or malformed: exit 2."""


def num(text, what):
    try:
        v = float(text)
    except ValueError:
        raise Refuse("not a number: %s=%r" % (what, text))
    if math.isnan(v) or math.isinf(v):
        raise Refuse("not a finite number: %s=%r" % (what, text))
    return v


# -- input: commit logs ------------------------------------------------------

SAMPLE_FIELDS = ("tree", "n", "seq", "wall_ms", "cpu_ms", "nodes_read", "nodes_written", "presync_ms")


def read_samples(paths):
    """The sample lines of the files, as dicts of strings."""
    if not paths:
        raise Refuse("no commit log given")
    out = []
    for path in paths:
        if not os.path.isfile(path):
            raise Refuse("missing file: " + path)
        with open(path) as f:
            for line in f:
                words = line.split()
                if not words or words[0] != "sample":
                    continue
                kv = dict(w.split("=", 1) for w in words[1:] if "=" in w)
                for field in SAMPLE_FIELDS:
                    if field not in kv:
                        raise Refuse("missing column %s: %s" % (field, line.strip()))
                num(kv["wall_ms"], "wall_ms")
                num(kv["cpu_ms"], "cpu_ms")
                out.append(kv)
    return out


def by_tree(samples, n):
    """Group the samples with n files by tree label; each tree needs VERDICT_SAMPLES."""
    trees = {}
    for s in samples:
        if int(num(s["n"], "n")) == n:
            trees.setdefault(s["tree"], []).append(s)
    for label, rows in trees.items():
        if len(rows) < VERDICT_SAMPLES:
            raise Refuse("tree %s has %d samples with n=%d, need %d" % (label, len(rows), n, VERDICT_SAMPLES))
    if not trees:
        raise Refuse("no sample with n=%d" % n)
    return trees


def side(paths, n):
    """The one tree of a side: (label, samples)."""
    trees = by_tree(read_samples(paths), n)
    if len(trees) != 1:
        raise Refuse("expected one tree label, found: " + ", ".join(sorted(trees)))
    return next(iter(trees.items()))


def column(rows, name):
    return [num(r[name], name) for r in rows]


def node_sums(rows):
    out = []
    for r in rows:
        if r["nodes_read"] == "NA" or r["nodes_written"] == "NA":
            raise Refuse("NA nodes in tree %s where check A needs numbers" % r["tree"])
        out.append(num(r["nodes_read"], "nodes_read") + num(r["nodes_written"], "nodes_written"))
    return out


def ratio(num_, den):
    if den == 0:
        return 1.0 if num_ == 0 else math.inf
    return num_ / den


def r3(x):
    return "inf" if math.isinf(x) else "%.3f" % x


# -- the checks ---------------------------------------------------------------

def check_a(flat, prolly):
    """Pre-sync work: CPU and nodes of prolly at most A_LIMIT x flat."""
    cpu = ratio(statistics.median(column(prolly, "cpu_ms")), statistics.median(column(flat, "cpu_ms")))
    nodes = ratio(statistics.median(node_sums(prolly)), statistics.median(node_sums(flat)))
    ok = cpu <= A_LIMIT and nodes <= A_LIMIT
    print("check A cpu_ratio=%s nodes_ratio=%s pass=%d" % (r3(cpu), r3(nodes), ok))
    return ok


def check_b(flat, prolly):
    """Total wall time of prolly strictly lower than flat."""
    wall = ratio(statistics.median(column(prolly, "wall_ms")), statistics.median(column(flat, "wall_ms")))
    ok = statistics.median(column(prolly, "wall_ms")) < statistics.median(column(flat, "wall_ms"))
    print("check B wall_ratio=%s pass=%d" % (r3(wall), ok))
    return ok


def read_boost(path):
    """The mode=whole run=median rows of a results.csv: (pattern, k) -> row."""
    if not path or not os.path.isfile(path):
        raise Refuse("missing file: %s" % path)
    rows = {}
    with open(path, newline="") as f:
        reader = csv.DictReader(f)
        need = ("mode", "run", "pattern", "k", "total_ms", "cpu_user_s", "cpu_sys_s", "new_stored_bytes")
        for col in need:
            if col not in (reader.fieldnames or []):
                raise Refuse("missing column %s in %s" % (col, os.path.basename(path)))
        for row in reader:
            if row["mode"] != "whole" or row["run"] != "median":
                continue
            key = (row["pattern"], row["k"])
            if key in rows:
                raise Refuse("duplicate median row %s k=%s" % key)
            rows[key] = row
    return rows


def boost_values(row):
    return (num(row["cpu_user_s"], "cpu_user_s") + num(row["cpu_sys_s"], "cpu_sys_s"),
            num(row["total_ms"], "total_ms"),
            num(row["new_stored_bytes"], "new_stored_bytes"))


def check_c(flat_rows, prolly_rows):
    """Boost, not drop: the 9 gated rows better and the bytes at most 2 x flat."""
    all_ok = True
    for pattern in GATED_PATTERNS:
        for k in GATED_KS:
            key = (pattern, str(k))
            if key not in flat_rows or key not in prolly_rows:
                raise Refuse("missing gated row: pattern=%s k=%d" % (pattern, k))
            fc, fw, fb = boost_values(flat_rows[key])
            pc, pw, pb = boost_values(prolly_rows[key])
            better = pc < fc and pw < fw
            bytes_ok = pb <= C_BYTES_LIMIT * fb
            all_ok = all_ok and better and bytes_ok
            print("row pattern=%s k=%d cpu_ratio=%s wall_ratio=%s bytes_ratio=%s better=%d bytes_ok=%d" % (
                pattern, k, r3(ratio(pc, fc)), r3(ratio(pw, fw)), r3(ratio(pb, fb)), better, bytes_ok))
    print("check C rows=%d pass=%d" % (len(GATED_PATTERNS) * len(GATED_KS), all_ok))
    return all_ok


def info_rows(flat_rows, prolly_rows):
    """The recorded rows. Every row that no check gates, and every insdel row
    with its absolute CPU and wall time."""
    gated = {(p, str(k)) for p in GATED_PATTERNS for k in GATED_KS}
    for key in sorted(set(flat_rows) & set(prolly_rows)):
        try:
            fc, fw, fb = boost_values(flat_rows[key])
            pc, pw, pb = boost_values(prolly_rows[key])
        except Refuse:
            # A row that no check gates never stops the verdict (a column may be NA).
            print("info pattern=%s k=%s unreadable" % key)
            continue
        if key not in gated:
            print("info pattern=%s k=%s cpu_ratio=%s wall_ratio=%s bytes_ratio=%s" % (
                key[0], key[1], r3(ratio(pc, fc)), r3(ratio(pw, fw)), r3(ratio(pb, fb))))
        if key[0] == "insdel":
            print("info insdel k=%s cpu_s_flat=%.4f cpu_s_prolly=%.4f wall_ms_flat=%.3f wall_ms_prolly=%.3f" % (
                key[1], fc, pc, fw, pw))


def presync_info(flat, prolly):
    vals = [r["presync_ms"] for r in prolly if r["presync_ms"] != "NA"]
    if vals:
        print("info presync_ms_median_prolly=%.3f" % statistics.median([num(v, "presync_ms") for v in vals]))
    print("info flat_cpu_ms_median=%.3f prolly_cpu_ms_median=%.3f flat_wall_ms_median=%.3f prolly_wall_ms_median=%.3f" % (
        statistics.median(column(flat, "cpu_ms")), statistics.median(column(prolly, "cpu_ms")),
        statistics.median(column(flat, "wall_ms")), statistics.median(column(prolly, "wall_ms"))))


def decide(args):
    flat_label, flat = side(args["--flat-commit"], N_FILES)
    prolly_label, prolly = side(args["--prolly-commit"], N_FILES)
    if flat_label == prolly_label:
        raise Refuse("both sides have the tree label " + flat_label)
    flat_rows = read_boost(one(args, "--flat-boost"))
    prolly_rows = read_boost(one(args, "--prolly-boost"))
    a = check_a(flat, prolly)
    b = check_b(flat, prolly)
    c = check_c(flat_rows, prolly_rows)
    presync_info(flat, prolly)
    info_rows(flat_rows, prolly_rows)
    print("verdict pmap07=%s" % ("pass" if a and b and c else "fail"))


def summary(paths):
    trees = {}
    for s in read_samples(paths):
        trees.setdefault((s["tree"], s["n"]), []).append(s)
    if not trees:
        raise Refuse("no sample line")
    for (label, n), rows in sorted(trees.items()):
        if len(rows) < MIN_SAMPLES:
            raise Refuse("tree %s n=%s has %d samples, need %d" % (label, n, len(rows), MIN_SAMPLES))
        print("summary tree=%s n=%s samples=%d wall_ms_median=%.3f cpu_ms_median=%.3f" % (
            label, n, len(rows), statistics.median(column(rows, "wall_ms")), statistics.median(column(rows, "cpu_ms"))))


# -- command line ---------------------------------------------------------------

FLAGS = ("--flat-commit", "--prolly-commit", "--flat-boost", "--prolly-boost")


def one(args, flag):
    vals = args.get(flag) or []
    if len(vals) != 1:
        raise Refuse("%s needs exactly one file" % flag)
    return vals[0]


def parse(argv):
    args, cur = {}, None
    for a in argv:
        if a in FLAGS or a == "--summary":
            cur = a
            args[cur] = []
        elif a.startswith("--") or cur is None:
            raise Refuse("bad argument: " + a)
        else:
            args[cur].append(a)
    return args


USAGE = ("usage: pmap07.py --flat-commit F... --prolly-commit F... --flat-boost CSV --prolly-boost CSV\n"
         "       pmap07.py --summary F...\n       pmap07.py --self-test")


def main(argv):
    if argv == ["--self-test"]:
        return self_test()
    try:
        args = parse(argv)
        if "--summary" in args:
            if len(args) != 1:
                raise Refuse("--summary takes only files")
            summary(args["--summary"])
        elif all(f in args for f in FLAGS):
            decide(args)
        else:
            raise Refuse("missing option")
    except Refuse as e:
        print(str(e), file=sys.stderr)
        print(USAGE, file=sys.stderr)
        return 2
    return 0


# -- self-test ---------------------------------------------------------------------

def sample_lines(tree, n, count, wall, cpu, nodes_read, nodes_written):
    return "".join(
        "sample tree=%s n=%d seq=%d wall_ms=%s cpu_ms=%s nodes_read=%s nodes_written=%s presync_ms=NA\n" % (
            tree, n, i, wall, cpu, nodes_read, nodes_written) for i in range(count))


BOOST_HDR = "mode,run,pattern,k,seq,total_ms,cpu_user_s,cpu_sys_s,new_stored_bytes\n"


def boost_csv(cpu=1.0, wall=100.0, stored=1000, **change):
    """A results.csv with the 9 gated rows and two others; change[(p,k)] = (cpu, wall, stored)."""
    out = BOOST_HDR
    for p in GATED_PATTERNS:
        for k in GATED_KS + (1000,):
            c, w, b = change.get("%s%d" % (p, k), (cpu, wall, stored))
            out += "whole,median,%s,%d,0,%s,%s,0,%s\n" % (p, k, w, c, b)
    out += "whole,1,scatter,1,0,1,1,0,1\n"  # a run row: ignored
    return out


def self_test():
    failures = []

    def expect(name, cond):
        if not cond:
            failures.append(name)

    def run(files, flat_commit, prolly_commit, flat_boost, prolly_boost):
        d = tempfile.mkdtemp(prefix="pmap07-self-")
        paths = {}
        for name, text in files.items():
            paths[name] = os.path.join(d, name)
            with open(paths[name], "w") as f:
                f.write(text)
        cmd = [sys.executable, os.path.abspath(__file__),
               "--flat-commit", *[paths[x] for x in flat_commit],
               "--prolly-commit", *[paths[x] for x in prolly_commit],
               "--flat-boost", paths[flat_boost], "--prolly-boost", paths[prolly_boost]]
        r = subprocess.run(cmd, capture_output=True, text=True, stdin=subprocess.DEVNULL)
        return r.returncode, r.stdout, r.stderr

    n = N_FILES
    flat = sample_lines("flat", n, 22, "100.0", "50.0", 0, 1000)
    good = sample_lines("prolly", n, 22, "60.0", "10.0", 20, 80)       # cpu 0.2, nodes 0.1, wall 0.6

    def case(prolly, flat_b=None, prolly_b=None, flat_text=flat, tag="case"):
        files = {"flat.log": flat_text, "prolly.log": prolly,
                 "flat.csv": flat_b or boost_csv(), "prolly.csv": prolly_b or boost_csv(cpu=0.5, wall=50.0, stored=1500)}
        return run(files, ["flat.log"], ["prolly.log"], "flat.csv", "prolly.csv")

    rc, out, _ = case(good)
    expect("pass rc", rc == 0)
    expect("pass verdict", out.strip().splitlines()[-1] == "verdict pmap07=pass")
    expect("pass check A line", "check A cpu_ratio=0.200 nodes_ratio=0.100 pass=1" in out)
    expect("pass check B line", "check B wall_ratio=0.600 pass=1" in out)
    expect("pass check C line", "check C rows=9 pass=1" in out)
    expect("info rows", "info pattern=scatter k=1000 " in out)
    rc2, out2, _ = case(good)
    expect("idempotent", out == out2 and rc2 == 0)

    rc, out, _ = case(sample_lines("prolly", n, 22, "60.0", "10.5", 20, 80))
    expect("A fails on cpu 0.21", rc == 0 and "check A cpu_ratio=0.210" in out and "pass=0" in out.split("check B")[0]
           and out.strip().splitlines()[-1] == "verdict pmap07=fail")
    rc, out, _ = case(sample_lines("prolly", n, 22, "60.0", "10.0", 30, 75))   # nodes 105/1000 -> pass; use bigger
    expect("A passes at nodes 0.105", "check A cpu_ratio=0.200 nodes_ratio=0.105 pass=1" in out)
    rc, out, _ = case(sample_lines("prolly", n, 22, "60.0", "10.0", 100, 101))  # nodes 201/1000 = 0.201
    expect("A fails on nodes", "check A cpu_ratio=0.200 nodes_ratio=0.201 pass=0" in out
           and out.strip().splitlines()[-1] == "verdict pmap07=fail")
    rc, out, _ = case(sample_lines("prolly", n, 22, "100.0", "10.0", 20, 80))
    expect("B fails on equal wall", "check B wall_ratio=1.000 pass=0" in out
           and out.strip().splitlines()[-1] == "verdict pmap07=fail")
    pb = boost_csv(cpu=0.5, wall=50.0, stored=1500, scatter10=(1.5, 50.0, 1500))
    rc, out, _ = case(good, prolly_b=pb)
    expect("C fails on one cpu row", "row pattern=scatter k=10 cpu_ratio=1.500" in out and "better=0" in out
           and "check C rows=9 pass=0" in out)
    pb = boost_csv(cpu=0.5, wall=150.0, stored=1500)
    rc, out, _ = case(good, prolly_b=pb)
    expect("C fails on wall", "check C rows=9 pass=0" in out)
    pb = boost_csv(cpu=0.5, wall=50.0, stored=2100)
    rc, out, _ = case(good, prolly_b=pb)
    expect("C fails at bytes 2.1x", "bytes_ratio=2.100" in out and "bytes_ok=0" in out and "check C rows=9 pass=0" in out)
    pb = boost_csv(cpu=0.5, wall=50.0, stored=2000)
    rc, out, _ = case(good, prolly_b=pb)
    expect("C passes at bytes 2.0x", "check C rows=9 pass=1" in out)
    # K = 1,000 and the version step are recorded, not gated.
    pb = boost_csv(cpu=0.5, wall=50.0, stored=1500, insdel1000=(9.0, 900.0, 99999))
    rc, out, _ = case(good, prolly_b=pb)
    expect("K=1000 not gated", rc == 0 and "check C rows=9 pass=1" in out and "info pattern=insdel k=1000 " in out)

    bad_line = good.replace("cpu_ms=10.0", "cpu_ms=abc", 1)
    rc, _, err = case(bad_line)
    expect("exit 2 on malformed line", rc == 2 and err)
    rc, _, err = case(sample_lines("prolly", n, 10, "60.0", "10.0", 20, 80))
    expect("exit 2 on fewer than 11 samples", rc == 2 and err)
    rc, _, err = case(sample_lines("prolly", n, 11, "60.0", "10.0", 20, 80))
    expect("exit 2 on one round (11 of 22 samples)", rc == 2 and "need 22" in err)
    rc, _, err = case(sample_lines("prolly", n, 22, "60.0", "10.0", "NA", "NA"))
    expect("exit 2 on NA nodes", rc == 2 and err)
    rc, _, err = case(sample_lines("flat", n, 22, "60.0", "10.0", 20, 80))
    expect("exit 2 on same tree label", rc == 2 and err)
    rc, _, err = case(good, prolly_b=boost_csv().replace("whole,median,insdel,100,", "whole,median,insdel,101,"))
    expect("exit 2 on missing gated row", rc == 2 and err)
    pb = boost_csv(cpu=0.5, wall=50.0, stored=1500) + "whole,median,version,0,0,NA,NA,NA,NA\n"
    rc, out, _ = case(good, prolly_b=pb, flat_b=boost_csv() + "whole,median,version,0,0,NA,NA,NA,NA\n")
    expect("NA in an ungated row is recorded", rc == 0 and "info pattern=version k=0 unreadable" in out
           and out.strip().splitlines()[-1] == "verdict pmap07=pass")
    rc, _, err = case(good, prolly_b="mode,run\nwhole,median\n")
    expect("exit 2 on missing column", rc == 2 and err)
    r = subprocess.run([sys.executable, os.path.abspath(__file__), "--flat-commit", "/nonexistent/x", "--prolly-commit",
                        "/nonexistent/y", "--flat-boost", "/nonexistent/a", "--prolly-boost", "/nonexistent/b"],
                       capture_output=True, text=True, stdin=subprocess.DEVNULL)
    expect("exit 2 on missing file", r.returncode == 2)

    d = tempfile.mkdtemp(prefix="pmap07-self-")
    p = os.path.join(d, "s.log")
    with open(p, "w") as f:
        f.write(sample_lines("flat-early", 100000, 11, "5.0", "4.0", 0, 10))
    r = subprocess.run([sys.executable, os.path.abspath(__file__), "--summary", p], capture_output=True, text=True,
                       stdin=subprocess.DEVNULL)
    expect("summary", r.returncode == 0 and r.stdout.strip() ==
           "summary tree=flat-early n=100000 samples=11 wall_ms_median=5.000 cpu_ms_median=4.000")

    if failures:
        print("self-test FAILED: " + ", ".join(failures), file=sys.stderr)
        return 1
    print("self-test ok")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
