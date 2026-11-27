#!/usr/bin/env python3
"""The pre-registered decision rules of the Q-2 (layout) and constants test of
Phase 25, as code. The thresholds below are fixed before the real run of plan
25-07 and must not change after it starts.

usage: decide.py [--expect-ns N1,N2] [--expect-ks K1,K2] DIR
       decide.py --self-test

DIR holds layout.csv, shape.csv and checks.txt (written by layout.sh).
--expect-ns names the two synthetic sizes (the smaller is the "1M" row, the
larger the "10M" row; default 1000000,10000000). --expect-ks names the two
K values of the medium and the large rows (default 100,1000). The K=1 row is
always k=1. (Smoke runs use other sizes and K values; the rules do not change.)

Rows. "Small" rows: for each source (Boost; synthetic 1M; synthetic 10M) the
K=1 row (Boost `scatter` k=1, synthetic `one`) and the K=100 row (Boost
`scatter` k=100, synthetic `mixed` k=100): 6 rows. "Large" rows: per source the
K=1,000 row (Boost `scatter` k=1000, synthetic `mixed` k=1000) and the
insert/delete row (`insdel` k=1000): 6 rows. Per row and layout the value is
the median over its commits; nodes = nodes_read + nodes_written; CPU =
cpu_user_s + cpu_sys_s (the column cpu_ms).

Layout (decided at the Dolt config 4096/16384): an arm X in {b, c} qualifies
against a iff the mean over the small rows of nodes(X)/nodes(a) is at most
0.80, the mean over the small rows of CPU(X)/CPU(a) is at most 1.25 (an
allowance for the noise of a shared host; a real CPU regression of the
combined key is larger), the largest bytes_written(X)/bytes_written(a) over the
large rows is at most 1.10, and the 1B node count of X is not larger than that
of a. If both qualify, take the lower node ratio; within 0.02 of each other,
take c. If none qualifies, take a (simpler; a tie goes to a). The
deterministic columns (nodes, bytes) decide; CPU only guards.

Constants (for the chosen layout): N1B(C) = the 1B node count; W(C) = the mean
of the median bytes_written over the rows synthetic 10M `one`, synthetic 10M
`mixed` k=100 and Boost `scatter` k=1; T(C) = the same mean of the median CPU;
F(C) = forced cuts / nodes of the 10M base build. A config is eligible iff
F(C) <= 0.005. A config C other than Dolt D (4096/16384) replaces D iff it is
eligible, N1B(C) <= 0.65 x N1B(D), W(C) <= 2.5 x W(D) and T(C) <= 2.0 x T(D).
If more than one qualifies, take the smaller NODE_MAX, then the smaller N1B.
Else keep D.

1B extrapolation (D-05), per tree, from the `tree` row of the synthetic 10M
base build: entries per leaf e = entries / leaves; fanout f = leaves / (nodes
of level 1); scale the entries by 1,000,000,000 / n; leaves' = ceil(entries' /
e); each level above = ceil(previous / f) until 1; nodes = the sum; depth = the
number of levels. N1B of a layout = the sum over its trees.

Degenerate cases (guards, not part of the rules): a tree of one leaf has no
level 1, so its fanout is taken as e; a fanout of 1 or less would never reach
the root, so it is raised to 2; a ratio with a zero denominator is 1 when the
numerator is also zero and infinite otherwise; the median of an even number of
commits is the mean of the two middle values.

Boost refcounts. The product never writes refcounts today, so the refcount map
of every Boost store is empty. The Boost rows therefore say nothing about
refcounts in a layout; the rules keep the Boost rows as written, and the
signal about layout b against c (refcounts inside or apart) comes only from
the synthetic rows. The output carries a `note` line that says so.

Output lines: `row ...` (the medians of each row used), `rule ...` (each
threshold with the measured value and pass or fail), `extrapolation ...` (per
tree), `nodes_1b ...` (per layout and config), `note ...`, and the last line
`verdict layout=<a|b|c> l=<L> node_max=<M>`.

Exit codes: 0 with a verdict; 2 when a required row is missing
(`missing row: ...`), a required check is absent or is not ok=1
(`check failed: ...`), or the arguments are bad.
"""

import csv
import math
import os
import statistics
import subprocess
import sys
import tempfile

# The Dolt config, the layout of the decision, and the swept configs.
LAYOUT_CFG = (4096, 16384)
CONFIGS = [(4096, 16384), (8192, 16384), (8192, 32768)]
LAYOUTS = ["a", "b", "c"]

# Layout rule.
NODES_RATIO_MAX = 0.80
CPU_RATIO_MAX = 1.25
BYTES_RATIO_MAX = 1.10
ARM_TIE = 0.02

# Constants rule.
CONST_NODES_1B_RATIO_MAX = 0.65
CONST_BYTES_RATIO_MAX = 2.5
CONST_CPU_RATIO_MAX = 2.0
FORCED_SHARE_MAX = 0.005

# The extrapolation target.
TARGET_INODES = 1_000_000_000

NOTE = ("boost refcounts are empty (the product writes none today): the layout b-versus-c "
        "and refcount signal comes only from the synthetic rows")


class Refuse(Exception):
    """A required input is missing or a check failed: exit 2."""


def cfg_fields(cfg):
    return "l=%d node_max=%d" % cfg


# ── input ────────────────────────────────────────────────────────────────────

class Data:
    def __init__(self, directory):
        self.commits = {}   # (source, n, layout, cfg, pattern, k) -> [row]
        self.base = {}      # (source, n, layout, cfg) -> [shape row]
        self.checks = {}    # (source, n, layout, cfg) -> [ok]
        for row in self.read_csv(directory, "layout.csv"):
            key = self.key(row) + (row["pattern"], int(row["k"]))
            self.commits.setdefault(key, []).append(row)
        for row in self.read_csv(directory, "shape.csv"):
            if row["phase"] == "base":
                self.base.setdefault(self.key(row), []).append(row)
        path = os.path.join(directory, "checks.txt")
        if not os.path.exists(path):
            raise Refuse("missing file: checks.txt")
        with open(path) as f:
            for line in f:
                words = line.split()
                if not words or words[0] != "check":
                    continue
                kv = dict(w.split("=", 1) for w in words[1:] if "=" in w)
                key = self.key(kv)
                self.checks.setdefault(key, []).append(kv.get("ok") == "1")
                if kv.get("ok") != "1":
                    raise Refuse("check failed: " + line.strip())

    @staticmethod
    def read_csv(directory, name):
        path = os.path.join(directory, name)
        if not os.path.exists(path):
            raise Refuse("missing file: " + name)
        with open(path, newline="") as f:
            return list(csv.DictReader(f))

    @staticmethod
    def key(row):
        # The Boost n is the inode count of the seed tree; it is not a row key.
        n = 0 if row["source"] == "boost" else int(row["n"])
        return (row["source"], n, row["layout"], (int(row["l"]), int(row["node_max"])))

    def need_checks(self, source, n, layout, cfg):
        if not any(self.checks.get((source, 0 if source == "boost" else n, layout, cfg), [])):
            raise Refuse("check failed: no check line ok=1 for source=%s n=%s layout=%s %s"
                         % (source, n, layout, cfg_fields(cfg)))

    def row(self, source, n, layout, cfg, pattern, k):
        """The medians of one row: (nodes, cpu_ms, bytes_written, commits)."""
        key = (source, 0 if source == "boost" else n, layout, cfg, pattern, k)
        rows = self.commits.get(key)
        if not rows:
            raise Refuse("missing row: source=%s n=%s layout=%s %s pattern=%s k=%d"
                         % (source, n, layout, cfg_fields(cfg), pattern, k))
        nodes = statistics.median(int(r["nodes_read"]) + int(r["nodes_written"]) for r in rows)
        cpu = statistics.median(float(r["cpu_ms"]) for r in rows)
        nbytes = statistics.median(int(r["bytes_written"]) for r in rows)
        return nodes, cpu, nbytes, len(rows)


# ── rows ─────────────────────────────────────────────────────────────────────

def row_defs(ns, ks):
    """(small, large): lists of (source, n, pattern, k)."""
    k0, k1 = ks
    small, large = [], []
    for source, n, one, mixed in [("boost", 0, "scatter", "scatter"),
                                  ("synth", ns[0], "one", "mixed"),
                                  ("synth", ns[1], "one", "mixed")]:
        small += [(source, n, one, 1), (source, n, mixed, k0)]
        large += [(source, n, mixed, k1), (source, n, "insdel", k1)]
    return small, large


def ratio(x, y):
    if y == 0:
        return 1.0 if x == 0 else math.inf
    return x / y


def mean(xs):
    return sum(xs) / len(xs)


def pass_word(ok):
    return "pass" if ok else "fail"


# ── 1B extrapolation ─────────────────────────────────────────────────────────

def extrapolate(entries, levels, n):
    """The levels of one tree scaled to TARGET_INODES: (entries', [levels])."""
    leaves = levels[0]
    e = entries / leaves
    f = leaves / levels[1] if len(levels) > 1 else e
    f = max(f, 2.0)
    scaled = entries * TARGET_INODES / n
    cur = math.ceil(scaled / e)
    out = [cur]
    while cur > 1:
        cur = math.ceil(cur / f)
        out.append(cur)
    return scaled, out


def nodes_1b(data, source_n, layout, cfg, lines, quiet=False):
    """The 1B node count of a layout and config, from the 10M base build."""
    key = ("synth", source_n, layout, cfg)
    trees = data.base.get(key)
    if not trees:
        raise Refuse("missing row: source=synth n=%d layout=%s %s phase=base tree" % (source_n, layout, cfg_fields(cfg)))
    total = 0
    for t in trees:
        levels = [int(x) for x in t["levels"].split("/")]
        scaled, out = extrapolate(int(t["entries"]), levels, source_n)
        total += sum(out)
        if not quiet:
            lines.append("extrapolation layout=%s %s map=%s entries_1b=%d depth=%d levels=%s nodes=%d"
                         % (layout, cfg_fields(cfg), t["map"], round(scaled), len(out),
                            "/".join(str(x) for x in out), sum(out)))
    if not quiet:
        lines.append("nodes_1b layout=%s %s nodes=%d" % (layout, cfg_fields(cfg), total))
    return total


def forced_share(data, source_n, layout, cfg):
    trees = data.base.get(("synth", source_n, layout, cfg), [])
    nodes = sum(sum(int(x) for x in t["levels"].split("/")) for t in trees)
    forced = sum(int(t["forced_cuts"]) for t in trees)
    return forced / nodes if nodes else 0.0


# ── the rules ────────────────────────────────────────────────────────────────

def decide(directory, ns, ks):
    """Runs the rules; returns the output lines, the last being the verdict."""
    data = Data(directory)
    lines = []
    small, large = row_defs(ns, ks)
    big = ns[1]
    dolt = LAYOUT_CFG

    # The rows of the layout decision: every layout at the Dolt config.
    value = {}
    for layout in LAYOUTS:
        for source, n, pattern, k in small + large:
            value[(layout, source, n, pattern, k)] = data.row(source, n, layout, dolt, pattern, k)
            nodes, cpu, nbytes, commits = value[(layout, source, n, pattern, k)]
            lines.append("row source=%s n=%d layout=%s %s pattern=%s k=%d commits=%d nodes=%g cpu_ms=%.3f bytes_written=%g"
                         % (source, n, layout, cfg_fields(dolt), pattern, k, commits, nodes, cpu, nbytes))
        for source, n in [("boost", 0), ("synth", ns[0]), ("synth", ns[1])]:
            data.need_checks(source, n, layout, dolt)
    n1b_dolt = {layout: nodes_1b(data, big, layout, dolt, lines) for layout in LAYOUTS}

    qualified = []
    for x in ("b", "c"):
        nodes_ratio = mean([ratio(value[(x, s, n, p, k)][0], value[("a", s, n, p, k)][0]) for s, n, p, k in small])
        cpu_ratio = mean([ratio(value[(x, s, n, p, k)][1], value[("a", s, n, p, k)][1]) for s, n, p, k in small])
        bytes_ratio = max(ratio(value[(x, s, n, p, k)][2], value[("a", s, n, p, k)][2]) for s, n, p, k in large)
        n1b_ok = n1b_dolt[x] <= n1b_dolt["a"]
        checks = [
            ("nodes_ratio", nodes_ratio, NODES_RATIO_MAX, nodes_ratio <= NODES_RATIO_MAX),
            ("cpu_ratio", cpu_ratio, CPU_RATIO_MAX, cpu_ratio <= CPU_RATIO_MAX),
            ("bytes_ratio", bytes_ratio, BYTES_RATIO_MAX, bytes_ratio <= BYTES_RATIO_MAX),
            ("nodes_1b", n1b_dolt[x], n1b_dolt["a"], n1b_ok),
        ]
        for name, got, limit, ok in checks:
            lines.append("rule layout=%s %s measured=%.4g max=%.4g %s" % (x, name, got, limit, pass_word(ok)))
        if all(c[3] for c in checks):
            qualified.append((nodes_ratio, x))
    if not qualified:
        layout = "a"
    elif len(qualified) == 1:
        layout = qualified[0][1]
    else:
        qualified.sort()
        layout = "c" if abs(qualified[0][0] - qualified[1][0]) <= ARM_TIE else qualified[0][1]
    lines.append("rule layout_choice=%s qualified=%s" % (layout, ",".join(x for _, x in qualified) or "none"))

    # Constants for the chosen layout.
    k0 = ks[0]
    const_rows = [("synth", big, "one", 1), ("synth", big, "mixed", k0), ("boost", 0, "scatter", 1)]
    stats = {}
    for cfg in CONFIGS:
        w, t = [], []
        for source, n, pattern, k in const_rows:
            nodes, cpu, nbytes, commits = data.row(source, n, layout, cfg, pattern, k)
            w.append(nbytes)
            t.append(cpu)
            if cfg != dolt:
                lines.append("row source=%s n=%d layout=%s %s pattern=%s k=%d commits=%d nodes=%g cpu_ms=%.3f bytes_written=%g"
                             % (source, n, layout, cfg_fields(cfg), pattern, k, commits, nodes, cpu, nbytes))
        for source, n in [("boost", 0), ("synth", ns[0]), ("synth", ns[1])]:
            data.need_checks(source, n, layout, cfg)
        n1b = n1b_dolt[layout] if cfg == dolt else nodes_1b(data, big, layout, cfg, lines)
        stats[cfg] = {"n1b": n1b, "w": mean(w), "t": mean(t), "f": forced_share(data, big, layout, cfg)}
    d = stats[dolt]
    winners = []
    for cfg in CONFIGS:
        s = stats[cfg]
        eligible = s["f"] <= FORCED_SHARE_MAX
        lines.append("rule config %s forced_share=%.5f max=%.5f %s" % (cfg_fields(cfg), s["f"], FORCED_SHARE_MAX, pass_word(eligible)))
        if cfg == dolt:
            continue
        r_nodes, r_bytes, r_cpu = ratio(s["n1b"], d["n1b"]), ratio(s["w"], d["w"]), ratio(s["t"], d["t"])
        checks = [("n1b_ratio", r_nodes, CONST_NODES_1B_RATIO_MAX), ("bytes_ratio", r_bytes, CONST_BYTES_RATIO_MAX),
                  ("cpu_ratio", r_cpu, CONST_CPU_RATIO_MAX)]
        ok = eligible
        for name, got, limit in checks:
            lines.append("rule config %s %s measured=%.4g max=%.4g %s" % (cfg_fields(cfg), name, got, limit, pass_word(got <= limit)))
            ok = ok and got <= limit
        if ok:
            winners.append((cfg[1], s["n1b"], cfg))
    chosen = min(winners)[2] if winners else dolt
    lines.append("note " + NOTE)
    lines.append("verdict layout=%s %s" % (layout, cfg_fields(chosen)))
    return lines


# ── self-test ────────────────────────────────────────────────────────────────

SELF_NS = (1000, 10000)
SELF_KS = (10, 40)
LAYOUT_HDR = ("source,n,layout,l,node_max,pattern,k,seq,edits,ms,cpu_ms,maxrss_bytes,new_objects,new_stored_bytes,"
              "nodes_read,nodes_written,bytes_written,forced_cuts")
SHAPE_HDR = "source,n,layout,l,node_max,phase,map,entries,depth,levels,leaf_bytes,internal_bytes,internal_entries,forced_cuts"


def fixture(directory, mult=None, drop_row=None):
    """Writes a full matrix. mult[(layout, cfg)] = dict of factors: nodes, cpu,
    bytes (the commit rows), leaves (the 10M base shape), forced (the forced
    cut share of the base shape). Missing entries are 1 and 0."""
    mult = mult or {}
    sources = [("boost", 777), ("synth", SELF_NS[0]), ("synth", SELF_NS[1])]
    small, large = row_defs(SELF_NS, SELF_KS)
    wanted = sorted(set((s, n, p, k) for s, n, p, k in small + large)
                    | set([("synth", SELF_NS[1], "one", 1), ("synth", SELF_NS[1], "mixed", SELF_KS[0]),
                           ("boost", 0, "scatter", 1)]))
    layout_rows, shape_rows, checks = [], [], []
    for layout in LAYOUTS:
        for cfg in CONFIGS:
            f = {"nodes": 1.0, "cpu": 1.0, "bytes": 1.0, "leaves": 1.0, "forced": 0.0}
            f.update(mult.get((layout, cfg), {}))
            for source, n_shown in sources:
                n = 0 if source == "boost" else n_shown
                for src, nn, pattern, k in wanted:
                    if (src, nn) != (source, n):
                        continue
                    if drop_row == (source, n, layout, cfg, pattern, k):
                        continue
                    for seq in range(3):
                        nodes = 100 * f["nodes"] + seq
                        layout_rows.append(",".join(str(x) for x in [
                            source, n_shown, layout, cfg[0], cfg[1], pattern, k, seq + 1, k, 1.5, "%.3f" % (50 * f["cpu"] + seq),
                            1000, 5, 1000, int(nodes) // 2, int(nodes) - int(nodes) // 2, int(1000 * f["bytes"]) + seq, 0]))
                checks.append("check mode=layout source=%s n=%d layout=%s l=%d node_max=%d ok=1" % (source, n_shown, layout, cfg[0], cfg[1]))
            # The 10M base build: one tree of 10,000 entries per layout, a few leaves.
            leaves = max(2, int(100 * f["leaves"]))
            lv = [leaves]
            while lv[-1] > 1:
                lv.append(math.ceil(lv[-1] / 10))
            total = sum(lv)
            shape_rows.append(",".join(str(x) for x in [
                "synth", SELF_NS[1], layout, cfg[0], cfg[1], "base", "ns", 10000, len(lv), "/".join(str(x) for x in lv),
                1, 1, 1, math.ceil(f["forced"] * total)]))
    with open(os.path.join(directory, "layout.csv"), "w") as fh:
        fh.write(LAYOUT_HDR + "\n" + "\n".join(layout_rows) + "\n")
    with open(os.path.join(directory, "shape.csv"), "w") as fh:
        fh.write(SHAPE_HDR + "\n" + "\n".join(shape_rows) + "\n")
    with open(os.path.join(directory, "checks.txt"), "w") as fh:
        fh.write("\n".join(checks) + "\n")


def verdict_of(mult=None, drop_row=None):
    with tempfile.TemporaryDirectory() as d:
        fixture(d, mult, drop_row)
        return decide(d, SELF_NS, SELF_KS)[-1]


def self_test():
    dolt, wide, widest = CONFIGS
    # 1. b with 30% fewer nodes and equal CPU and bytes: layout b.
    v = verdict_of({("b", dolt): {"nodes": 0.7}})
    assert v == "verdict layout=b l=4096 node_max=16384", v
    # 2. b with 10% fewer nodes: layout a.
    v = verdict_of({("b", dolt): {"nodes": 0.9}})
    assert v == "verdict layout=a l=4096 node_max=16384", v
    # 3. b and c both qualify within 0.02: c; far apart: the lower one.
    v = verdict_of({("b", dolt): {"nodes": 0.70}, ("c", dolt): {"nodes": 0.69}})
    assert v == "verdict layout=c l=4096 node_max=16384", v
    v = verdict_of({("b", dolt): {"nodes": 0.60}, ("c", dolt): {"nodes": 0.70}})
    assert v == "verdict layout=b l=4096 node_max=16384", v
    # A CPU regression above 1.25 disqualifies b.
    v = verdict_of({("b", dolt): {"nodes": 0.7, "cpu": 1.5}})
    assert v == "verdict layout=a l=4096 node_max=16384", v
    # 4. A removed row gives exit 2 (through the command line).
    with tempfile.TemporaryDirectory() as d:
        fixture(d, drop_row=("synth", SELF_NS[0], "b", dolt, "mixed", SELF_KS[0]))
        proc = subprocess.run([sys.executable, "-I", os.path.abspath(__file__), "--expect-ns", "%d,%d" % SELF_NS,
                               "--expect-ks", "%d,%d" % SELF_KS, d], capture_output=True, text=True, stdin=subprocess.DEVNULL)
        assert proc.returncode == 2, proc.returncode
        assert "missing row:" in proc.stderr, proc.stderr
    # A check that is not ok=1 gives exit 2 too.
    with tempfile.TemporaryDirectory() as d:
        fixture(d)
        with open(os.path.join(d, "checks.txt"), "a") as fh:
            fh.write("check mode=layout source=synth n=1000 layout=a l=4096 node_max=16384 ok=0\n")
        try:
            decide(d, SELF_NS, SELF_KS)
            raise AssertionError("a failed check was accepted")
        except Refuse as e:
            assert str(e).startswith("check failed:"), e
    # 5. A config with half the 1B nodes, 1.9x bytes and 1.4x CPU replaces Dolt.
    good = {"leaves": 0.5, "bytes": 1.9, "cpu": 1.4}
    v = verdict_of({("a", wide): good})
    assert v == "verdict layout=a l=8192 node_max=16384", v
    # 6. The same with 2.6x bytes keeps Dolt.
    v = verdict_of({("a", wide): dict(good, bytes=2.6)})
    assert v == "verdict layout=a l=4096 node_max=16384", v
    # 7. A forced-cut share of 0.01 makes the config ineligible.
    v = verdict_of({("a", wide): dict(good, forced=0.01)})
    assert v == "verdict layout=a l=4096 node_max=16384", v
    # Two configs qualify: the smaller NODE_MAX wins.
    v = verdict_of({("a", wide): good, ("a", widest): dict(good, leaves=0.4)})
    assert v == "verdict layout=a l=8192 node_max=16384", v
    # The extrapolation of a known tree: 10,000 entries, 100 leaves, 10, 1.
    scaled, out = extrapolate(10000, [100, 10, 1], 10000)
    assert round(scaled) == 1_000_000_000 and out == [10_000_000, 1_000_000, 100_000, 10_000, 1_000, 100, 10, 1], out
    assert sum(out) == 11_111_111
    # Degenerate trees end: one leaf, and a fanout of 1.
    assert extrapolate(50, [1], 1000)[1][-1] == 1
    assert extrapolate(100, [4, 4, 1], 1000)[1][-1] == 1
    print("self-test ok")


# ── main ─────────────────────────────────────────────────────────────────────

def pair(text, name):
    try:
        a, b = (int(x) for x in text.split(","))
        assert 0 < a < b
        return (a, b)
    except (ValueError, AssertionError):
        print("decide.py: %s wants two increasing numbers like 1000000,10000000, got %r" % (name, text), file=sys.stderr)
        sys.exit(2)


def main(argv):
    ns, ks, directory = (1_000_000, 10_000_000), (100, 1000), None
    args = list(argv)
    if args == ["--self-test"]:
        self_test()
        return 0
    while args:
        a = args.pop(0)
        if a == "--expect-ns" and args:
            ns = pair(args.pop(0), "--expect-ns")
        elif a == "--expect-ks" and args:
            ks = pair(args.pop(0), "--expect-ks")
        elif not a.startswith("--") and directory is None:
            directory = a
        else:
            print("usage: decide.py [--expect-ns N1,N2] [--expect-ks K1,K2] DIR | --self-test", file=sys.stderr)
            return 2
    if directory is None:
        print("usage: decide.py [--expect-ns N1,N2] [--expect-ks K1,K2] DIR | --self-test", file=sys.stderr)
        return 2
    try:
        lines = decide(directory, ns, ks)
    except Refuse as e:
        print(str(e), file=sys.stderr)
        return 2
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
