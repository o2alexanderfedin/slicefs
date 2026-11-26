#!/usr/bin/env python3
"""Markdown tables of the prolly benchmark (plan 24-05), from the CSV files.

usage: report-tables.py --dir DIR --stem P [--check REPORT]

Reads DIR/P-results.csv, P-scale.csv, P-disk.csv, P-shape.txt and
P-machine.txt (the outputs of scripts/prolly-bench/bench.sh) and prints the
report tables. Every number is a value of a median row (run=median), or a
number computed from median values by the formula in the table heading.
With bench_data=smoke in P-machine.txt, a smoke line is printed above each
table.

--check REPORT: print REPORT-OK when every table line (a line that starts
with "|") and every smoke line that this script prints is a line of REPORT;
else print the missing lines and exit 1.

Python 3 standard library only.
"""
import argparse
import csv
import os
import statistics
import sys

SMOKE_LINE = "> Smoke data (generated trees): a check of the tools, not the Boost result."
MODES = ("whole", "flat", "prolly")


def fail(msg):
    print("report-tables: " + msg, file=sys.stderr)
    sys.exit(2)


def read_csv(path):
    if not os.path.isfile(path):
        fail("missing " + path)
    with open(path, newline="") as f:
        return list(csv.DictReader(f))


def read_lines(path):
    if not os.path.isfile(path):
        fail("missing " + path)
    with open(path) as f:
        return [l.rstrip("\n") for l in f]


def kv(line):
    """`run=1 tree a=1 b=2` -> ("tree", {"run": "1", "a": "1", "b": "2"})."""
    kind, d = None, {}
    for w in line.split():
        if "=" in w:
            k, v = w.split("=", 1)
            d[k] = v
        elif kind is None:
            kind = w
    return kind, d


def num(v):
    if v is None or v == "" or v == "NA":
        return None
    return float(v)


# Formats (fixed): ms with one decimal; KB = bytes / 1e3 and MiB = bytes /
# 1048576 with one decimal; counts with thousands separators; NA stays NA.
def f_ms(v):
    x = num(v)
    return "NA" if x is None else f"{x:,.1f}"


def f_cpu(user, sys_):
    u, s = num(user), num(sys_)
    return "NA" if u is None or s is None else f"{(u + s) * 1000:,.1f}"


def f_kb(v):
    x = num(v)
    return "NA" if x is None else f"{x / 1e3:,.1f}"


def f_mib(v):
    x = num(v)
    return "NA" if x is None else f"{x / 1048576:,.1f}"


def f_count(v):
    x = num(v)
    return "NA" if x is None else f"{int(x):,}"


def row(cells):
    return "| " + " | ".join(cells) + " |"


def sep(n):
    return "|" + "---|" * n


class Tables:
    def __init__(self, d, stem):
        p = lambda s: os.path.join(d, f"{stem}-{s}")
        all_rows = read_csv(p("results.csv"))
        self.results = [r for r in all_rows if r["run"] == "median"]
        self.run_rows = [r for r in all_rows if r["run"] != "median"]
        self.scale = [r for r in read_csv(p("scale.csv")) if r["run"] == "median"]
        self.disk = [r for r in read_csv(p("disk.csv")) if r["run"] == "median"]
        self.shape = [kv(l) for l in read_lines(p("shape.txt")) if l.strip()]
        self.machine = dict(l.split("=", 1) for l in read_lines(p("machine.txt")) if "=" in l)
        if not self.results:
            fail("no median rows in " + p("results.csv"))
        self.smoke = self.machine.get("bench_data") == "smoke"
        self.by = {(r["mode"], r["pattern"], r["k"]): r for r in self.results}

    def groups(self):
        """(pattern, k) in the order setup, bulk, scatter by K, cluster by K,
        insdel by K, version (insdel runs after the version step, but its rows
        stand next to the other small commits)."""
        keys = {(r["pattern"], r["k"]) for r in self.results}
        order = {"setup": 0, "bulk": 1, "scatter": 2, "cluster": 3, "insdel": 4, "version": 5}
        return sorted(keys, key=lambda g: (order.get(g[0], 6), g[0], int(g[1])))

    def get(self, mode, g):
        return self.by.get((mode, g[0], g[1]))

    def entries(self, g):
        for m in MODES:
            r = self.get(m, g)
            if r is not None and r["entries_changed"] != "NA":
                return f_count(r["entries_changed"])
        return "NA"

    def commit_time(self):
        out = ["### Commit time and CPU", "",
               row(["pattern", "K", "entries changed",
                    "whole ms", "whole begin ms", "whole CPU ms",
                    "flat ms", "flat CPU ms", "prolly ms", "prolly CPU ms"]),
               sep(10)]
        for g in self.groups():
            c = [g[0], g[1], self.entries(g)]
            for m, cols in (("whole", 3), ("flat", 2), ("prolly", 2)):
                r = self.get(m, g)
                if r is None:
                    c += ["-"] * cols
                    continue
                c.append(f_ms(r["total_ms"]))
                if m == "whole":
                    c.append(f_ms(r["begin_ms"]))
                c.append(f_cpu(r["cpu_user_s"], r["cpu_sys_s"]))
            out.append(row(c))
        return out

    def bytes_per_commit(self):
        out = ["### Bytes per commit", "",
               row(["pattern", "K",
                    "whole serialized KB", "whole new objects", "whole new stored KB",
                    "flat serialized KB", "flat new objects", "flat new stored KB",
                    "prolly serialized KB", "prolly new objects", "prolly new stored KB",
                    "prolly nodes read", "prolly nodes written"]),
               sep(13)]
        for g in self.groups():
            c = [g[0], g[1]]
            for m in MODES:
                r = self.get(m, g)
                if r is None:
                    c += ["-"] * 3
                    continue
                c += [f_kb(r["serialized_bytes"]), f_count(r["new_objects"]), f_kb(r["new_stored_bytes"])]
            r = self.get("prolly", g)
            c += ["-", "-"] if r is None else [f_count(r["nodes_read"]), f_count(r["nodes_written"])]
            out.append(row(c))
        return out

    def estimate(self):
        out = ["### Estimated whole commit with a prolly inode_data (estimate, not measured)", "",
               row(["pattern", "K", "whole ms", "flat ms", "prolly ms",
                    "estimate ms (whole - flat + prolly)", "estimate / whole",
                    "whole CPU ms", "estimate CPU ms (whole - flat + prolly)", "estimate CPU / whole CPU"]),
               sep(10)]
        cpu = lambda r: (num(r["cpu_user_s"]) + num(r["cpu_sys_s"])) * 1000
        for g in self.groups():
            w, f, p = (self.get(m, g) for m in MODES)
            if w is None or f is None or p is None:
                continue
            wm, fm, pm = num(w["total_ms"]), num(f["total_ms"]), num(p["total_ms"])
            est = wm - fm + pm
            wc, ec = cpu(w), cpu(w) - cpu(f) + cpu(p)
            pct = lambda a, b: "NA" if b <= 0 else f"{100 * a / b:.1f} %"
            out.append(row([g[0], g[1], f"{wm:,.1f}", f"{fm:,.1f}", f"{pm:,.1f}", f"{est:,.1f}",
                            pct(est, wm), f"{wc:,.1f}", f"{ec:,.1f}", pct(ec, wc)]))
        return out

    def peak_memory(self):
        loads = [num(d.get("maxrss_after_load")) for k, d in self.shape if k == "info"]
        loads = [x for x in loads if x is not None]
        load = statistics.median_low(loads) if loads else None
        out = ["### Peak memory", "",
               row(["mode", "peak RSS of the process (MiB)", "ru_maxrss after the store load (MiB)",
                    "ru_maxrss after the last commit (MiB)"]),
               sep(4)]
        for m in MODES:
            rs = [r for r in self.results if r["mode"] == m]
            if not rs:
                continue
            peaks = [num(r["peak_rss_bytes"]) for r in rs if num(r["peak_rss_bytes"]) is not None]
            peak = "NA" if not peaks else f_mib(statistics.median_low(peaks))
            # ru_maxrss after the last commit: the row with the highest seq of
            # each run, median_low over the runs.
            ends = {}
            for r in self.run_rows:
                if r["mode"] == m and (r["run"] not in ends or int(r["seq"]) > int(ends[r["run"]]["seq"])):
                    ends[r["run"]] = r
            last_rss = statistics.median_low([num(r["maxrss_bytes"]) for r in ends.values()]) if ends else None
            after_load = (f_mib(load) if load is not None else "NA") if m == "whole" else "-"
            out.append(row([m, peak, after_load, "NA" if last_rss is None else f_mib(last_rss)]))
        return out

    def on_disk(self):
        out = ["### On-disk size", "",
               row(["mode", "start apparent MiB", "end apparent MiB", "growth apparent MiB",
                    "start allocated MiB", "end allocated MiB", "growth allocated MiB", "files at end"]),
               sep(8)]
        d = {(r["mode"], r["phase"]): r for r in self.disk}
        for m in MODES:
            s, e = d.get((m, "start")), d.get((m, "end"))
            if s is None or e is None:
                continue
            g = lambda c: f_mib(num(e[c]) - num(s[c]))
            out.append(row([m, f_mib(s["apparent_bytes"]), f_mib(e["apparent_bytes"]), g("apparent_bytes"),
                            f_mib(s["alloc_bytes"]), f_mib(e["alloc_bytes"]), g("alloc_bytes"),
                            f_count(e["files"])]))
        return out

    def scale_rows(self):
        out = ["### Scale", "",
               row(["n", "op", "flat ms", "flat CPU ms", "flat serialized KB", "flat peak RSS MiB",
                    "prolly ms", "prolly CPU ms", "prolly serialized KB", "prolly new objects",
                    "prolly peak RSS MiB"]),
               sep(11)]
        by = {(r["mode"], r["n"], r["op"]): r for r in self.scale}
        keys = sorted({(r["n"], r["op"]) for r in self.scale},
                      key=lambda k: (int(k[0]), 0 if k[1] == "build" else 1, k[1]))
        for n, op in keys:
            c = [f_count(n), op]
            f, p = by.get(("flat", n, op)), by.get(("prolly", n, op))
            c += ["-"] * 4 if f is None else [f_ms(f["ms"]), f_cpu(f["cpu_user_s"], f["cpu_sys_s"]),
                                               f_kb(f["serialized_bytes"]), f_mib(f["peak_rss_bytes"])]
            c += ["-"] * 5 if p is None else [f_ms(p["ms"]), f_cpu(p["cpu_user_s"], p["cpu_sys_s"]),
                                               f_kb(p["serialized_bytes"]), f_count(p["new_objects"]),
                                               f_mib(p["peak_rss_bytes"])]
            out.append(row(c))
        return out

    def tree_shape(self):
        def med(kind, field):
            vals = [num(d.get(field)) for k, d in self.shape if k == kind]
            vals = [x for x in vals if x is not None]
            return statistics.median_low(vals) if vals else None
        out = ["### Tree shape", "", row(["field", "value"]), sep(2)]
        info = [("inode_data entries after the seed", "info", "inode_data_entries", f_count)]
        tree = [("depth", "depth", f_count), ("nodes", "nodes", f_count), ("leaves", "leaves", f_count),
                ("leaf bytes min", "leaf_bytes_min", f_count), ("leaf bytes mean", "leaf_bytes_mean", f_count),
                ("leaf bytes max", "leaf_bytes_max", f_count), ("internal nodes", "internal_nodes", f_count),
                ("internal bytes mean", "internal_bytes_mean", f_count)]
        for label, kind, field, fmt in info:
            v = med(kind, field)
            out.append(row([label, "NA" if v is None else fmt(v)]))
        for label, field, fmt in tree:
            v = med("tree", field)
            out.append(row(["final tree: " + label, "NA" if v is None else fmt(v)]))
        h = med("hash", "boundary_hash_ns_per_key")
        out.append(row(["boundary hash ns per key (median of runs)", "NA" if h is None else f"{h:,.1f}"]))
        return out

    def all(self):
        parts = [self.commit_time(), self.bytes_per_commit(), self.estimate(), self.peak_memory(),
                 self.on_disk(), self.scale_rows(), self.tree_shape()]
        lines = []
        for t in parts:
            if self.smoke:
                lines += [t[0], "", SMOKE_LINE] + t[1:]
            else:
                lines += t
            lines.append("")
        return lines


def main():
    ap = argparse.ArgumentParser(description="markdown tables of the prolly benchmark")
    ap.add_argument("--dir", required=True)
    ap.add_argument("--stem", required=True)
    ap.add_argument("--check", metavar="REPORT")
    a = ap.parse_args()
    lines = Tables(a.dir, a.stem).all()
    if a.check is None:
        print("\n".join(lines).rstrip("\n"))
        return 0
    if not os.path.isfile(a.check):
        fail("missing " + a.check)
    with open(a.check) as f:
        have = {l.rstrip() for l in f}
    need = [l for l in lines if l.startswith("|") or l == SMOKE_LINE]
    missing = [l for l in need if l.rstrip() not in have]
    if missing:
        print("missing lines in " + a.check + ":")
        for l in missing:
            print(l)
        return 1
    print("REPORT-OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
