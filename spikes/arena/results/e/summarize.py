#!/usr/bin/env python3
"""Spike E: median [min..max] per variant of the e1 JSONL files.

usage: python3 -I results/e/summarize.py results/e/e1_*_r*.jsonl
"""
import json
import statistics
import sys
from collections import defaultdict

ORDER = ["alloc", "pwbuf", "mmap", "pwbufV3", "mmapV3"]
COLS = [
    ("put_cpu_us", "cpu/put us", 1),
    ("put_user_us", "user us", 1),
    ("put_sys_us", "sys us", 1),
    ("put_p50_us", "put p50 us", 1),
    ("put_wall_us", "put mean us", 1),
    ("put_minflt", "minflt/put", 2),
    ("put_majflt", "majflt/put", 2),
    ("mib_per_s", "MiB/s", 1),
    ("sync_p50_ms", "sync p50 ms", 1),
    ("msync_p50_ms", "msync p50 ms", 2),
    ("fullsync_p50_ms", "FULLFSYNC p50 ms", 1),
    ("sync_cpu_us", "cpu/sync us", 0),
    ("new_arena_ms", "new arena ms", 1),
    ("unmap_ms", "unmap ms", 2),
]


def fmt(v, d):
    return "-" if v is None else f"{v:.{d}f}"


def main(paths):
    groups = defaultdict(lambda: defaultdict(list))
    loads = defaultdict(list)
    for p in paths:
        for line in open(p):
            r = json.loads(line)
            key = r["cfg"]
            groups[key][r["variant"]].append(r)
            loads[key].append(r["load1"])
    for key in sorted(groups):
        g = groups[key]
        n = max(len(v) for v in g.values())
        sync = "sync=none" not in key
        cols = [c for c in COLS if sync or not c[0].startswith(("sync", "msync", "fullsync"))]
        print(f"\n### {key}  (runs per variant: {n}; load1 median {statistics.median(loads[key]):.1f}, "
              f"min {min(loads[key]):.1f}, max {max(loads[key]):.1f})\n")
        print("| variant | " + " | ".join(c[1] for c in cols) + " |")
        print("|---" * (len(cols) + 1) + "|")
        for v in ORDER:
            if v not in g:
                continue
            cells = []
            for k, _, d in cols:
                xs = [r[k] for r in g[v] if r.get(k) is not None]
                if not xs:
                    cells.append("-")
                    continue
                cells.append(f"{fmt(statistics.median(xs), d)} [{fmt(min(xs), d)}..{fmt(max(xs), d)}]")
            print(f"| {v} | " + " | ".join(cells) + " |")


if __name__ == "__main__":
    main(sys.argv[1:])
