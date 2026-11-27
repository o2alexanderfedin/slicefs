#!/bin/bash
# The layout and constants matrix of Phase 25 (D-02, D-08): Boost rows (the real
# commits of two Boost versions, replayed into trees of layouts a, b and c) and
# synthetic rows (1M and 10M inodes with the D-02 commit mix), each for three
# layouts and three configs of L / NODE_MAX. One process per row, each with a
# time limit; resumable. In process, no mount. By hand only, never in cargo
# test. Runs on macOS and Linux (bash 3.2 or newer). See README.md next to
# this script.
#
# usage: layout.sh <data-dir>
#        LAYOUT_SMOKE=1 layout.sh                (no data dir: generated trees, small sizes)
# env:   WORK         work dir for the stores, rows and results (default: a new temp dir)
#        BIN          the prolly-bench binary (default: build it in release)
#        CONFIGS      "L:NODE_MAX" pairs (default: 4096:16384 8192:16384 8192:32768; the first is Dolt)
#        LAYOUTS      layouts (default: a b c)
#        SYNTH_NS     synthetic inode counts (default: 1000000 10000000)
#        BOOST_VERS   the seed tree, then the next tree (default: 1_84_0 1_85_0)
#        KS           changed files per Boost commit (default: 1 100 1000)
#        SYNTH_KS     K of the synthetic mixed and insdel patterns (default: 100 1000)
#        COMMITS      commits per pattern and K (default 10)
#        SEED         seed of the edit script (default 25)
#        ROW_TIMEOUT  seconds per row process (default 5400)
#        RESUME=1     keep the work dir and skip the rows and steps that ended with rc=0
#        LAYOUT_SMOKE=1  two small generated trees, KS="1 10 40", SYNTH_KS="10 40",
#                     SYNTH_NS="5000 20000", COMMITS=2
# inputs: DATA/x/boost_<v>/ for both versions of BOOST_VERS (see fetch-boost.sh).
# Files in $WORK:
#         layout.csv   one row per commit of every row process
#         shape.csv    the tree lines (base and final) of every row process
#         build.csv    the base build of every row process
#         checks.txt   the check lines of every row process
#         machine.txt  machine and run parameters (no path, host or user)
#         rows/        the output of each row process, ending with a line rc=<code>
#         steps/       the output of the Boost seed, whole and maps steps
# Exit 0 when every row ended with rc=0 and every check is ok=1; else 1.
# The rules that read the CSVs are in decide.py.
set -euo pipefail
usage() { echo "usage: layout.sh <data-dir>   or   LAYOUT_SMOKE=1 layout.sh   (no data dir)" >&2; exit 2; }
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
OS=$(uname -s)
LAYOUT_SMOKE=${LAYOUT_SMOKE:-0}
RESUME=${RESUME:-0}
D=${1:-}
if [ "$LAYOUT_SMOKE" = 1 ]; then [ -z "$D" ] || usage; else [ -n "$D" ] || usage; fi
command -v python3 > /dev/null || { echo "python3 is needed (the CSV rows)" >&2; exit 2; }
abs() { case $1 in /*) echo "$1" ;; *) echo "$PWD/$1" ;; esac; }

if [ "$LAYOUT_SMOKE" = 1 ]; then
  BOOST_VERS=${BOOST_VERS:-0_0_1 0_0_2}
  KS=${KS:-1 10 40}; SYNTH_KS=${SYNTH_KS:-10 40}; SYNTH_NS=${SYNTH_NS:-5000 20000}; COMMITS=${COMMITS:-2}
fi
BOOST_VERS=${BOOST_VERS:-1_84_0 1_85_0}
KS=${KS:-1 100 1000}; SYNTH_KS=${SYNTH_KS:-100 1000}; SYNTH_NS=${SYNTH_NS:-1000000 10000000}
COMMITS=${COMMITS:-10}; SEED=${SEED:-25}; ROW_TIMEOUT=${ROW_TIMEOUT:-5400}
CONFIGS=${CONFIGS:-4096:16384 8192:16384 8192:32768}; LAYOUTS=${LAYOUTS:-a b c}
set -- $BOOST_VERS
[ $# -eq 2 ] || { echo "BOOST_VERS must be two versions: the seed tree, then the next tree" >&2; exit 2; }
V1=$1; V2=$2
for v in $KS $SYNTH_KS $SYNTH_NS $COMMITS $ROW_TIMEOUT; do
  [[ "$v" =~ ^[1-9][0-9]*$ ]] || { echo "not a positive number: $v" >&2; exit 2; }
done
[[ "$SEED" =~ ^[0-9]+$ ]] || { echo "SEED is not a number: $SEED" >&2; exit 2; }
for c in $CONFIGS; do
  [[ "$c" =~ ^[1-9][0-9]*:[1-9][0-9]*$ ]] || { echo "CONFIGS wants L:NODE_MAX pairs, got $c" >&2; exit 2; }
done
for l in $LAYOUTS; do
  case $l in a | b | c) ;; *) echo "LAYOUTS wants a, b or c, got $l" >&2; exit 2 ;; esac
done

nodata() { echo "missing $1; get it with scripts/macos-mount-test/fetch-boost.sh DATA_DIR $BOOST_VERS" >&2; exit 2; }
# The data dir is only read, never written; a mistyped data dir is refused, not made.
if [ "$LAYOUT_SMOKE" != 1 ]; then
  [ -d "$D" ] || nodata "$D"
  D=$(cd "$D" && pwd)
  for v in "$V1" "$V2"; do [ -d "$D/x/boost_$v" ] || nodata "$D/x/boost_$v"; done
fi

if [ -z "${WORK:-}" ]; then WORK=$(mktemp -d "${TMPDIR:-/tmp}/prolly-layout.XXXXXX"); echo "work dir: $WORK" >&2; fi
mkdir -p "$WORK"; WORK=$(cd "$WORK" && pwd)
[ -n "$WORK" ] && [ "$WORK" != / ] || { echo "refusing WORK=$WORK" >&2; exit 2; }

if [ -z "${BIN:-}" ]; then
  # cargo runs in REPO, so a relative CARGO_TARGET_DIR means REPO/that dir.
  case ${CARGO_TARGET_DIR:-} in
    '' | /*) ;;
    *) CARGO_TARGET_DIR=$REPO/$CARGO_TARGET_DIR; export CARGO_TARGET_DIR ;;
  esac
  echo "building prolly-bench (release)" >&2
  (cd "$REPO" && cargo build --release --locked -p prolly-bench >&2)
  BIN=${CARGO_TARGET_DIR:-$REPO/target}/release/prolly-bench
fi
BIN=$(abs "$BIN")
[ -x "$BIN" ] || { echo "BIN is not executable: $BIN" >&2; exit 2; }

# A time limit for every process: GNU timeout, or gtimeout, else none (said once).
if command -v timeout > /dev/null; then TMO=timeout
elif command -v gtimeout > /dev/null; then TMO=gtimeout
else TMO=; echo "no timeout command: the rows run without a time limit" >&2; fi

# Without RESUME the work dirs of an earlier run are cleared (the named volume of
# the Docker case persists between runs). Only dirs that this script makes.
ROWS=$WORK/rows; STEPS=$WORK/steps; B=$WORK/boost; S=$WORK/stores
if [ "$RESUME" != 1 ]; then rm -rf "${ROWS:?}" "${STEPS:?}" "${B:?}" "${S:?}"; fi
mkdir -p "$ROWS" "$STEPS" "$B" "$S"
trap 'trap "" INT TERM HUP; exit 130' INT TERM HUP

loadavg() {
  if [ "$OS" = Darwin ]; then sysctl -n vm.loadavg | tr -d '{}' | awk '{print $1, $2, $3}'
  else cut -d' ' -f1-3 /proc/loadavg; fi
}
utc() { date -u +%Y-%m-%dT%H:%M:%SZ; }
START_UTC=$(utc); LOAD_BEFORE=$(loadavg)

# done_ok <file>: true when the file ends with the line rc=0.
done_ok() { [ -f "$1" ] && [ "$(tail -n 1 "$1")" = rc=0 ]; }
# run_to <file> <cmd...>: run with the time limit, stdout to <file> and a last line
# rc=<code>; stderr to <file>.err. Returns 0 always (the code is in the file).
run_to() {
  local f=$1 rc=0; shift
  $TMO ${TMO:+"$ROW_TIMEOUT"} "$@" > "$f" 2> "$f.err" < /dev/null || rc=$?
  echo "rc=$rc" >> "$f"
  [ "$rc" -eq 0 ] || { echo "failed (rc=$rc): $(basename "$f")" >&2; tail -n 3 "$f.err" >&2; }
  return 0
}

FAILED=0
# row <name> <cmd...>: one row process on a fresh store dir $S/row.
row() {
  local name=$1; shift
  if [ "$RESUME" = 1 ] && done_ok "$ROWS/$name.txt"; then echo "skip $name" >&2; return 0; fi
  echo "row $name" >&2
  rm -rf "$S/row"
  run_to "$ROWS/$name.txt" "$@" --store "$S/row"
  rm -rf "$S/row"
  done_ok "$ROWS/$name.txt" || FAILED=$((FAILED + 1))
}

# ── Boost rows ──
if [ "$LAYOUT_SMOKE" = 1 ]; then
  D=$S/smoke-data
  if [ ! -d "$D/x/boost_$V2" ]; then
    echo "making smoke data" >&2
    "$BIN" gen tree --dir "$D/x/boost_$V1" --files 2000 --seed 1 --max-bytes 4096 >&2
    "$BIN" gen tree --dir "$D/x/boost_$V2" --files 2100 --seed 1 --max-bytes 4096 --change-every 10 >&2
  fi
fi
T1=$D/x/boost_$V1; T2=$D/x/boost_$V2
echo "warming the page cache" >&2
find "$T1" "$T2" -type f -exec cat {} + > /dev/null

# The seed and whole steps change one store together: a failed whole means a new seed.
if ! { done_ok "$STEPS/seed.txt" && done_ok "$STEPS/whole.txt" && [ "$RESUME" = 1 ]; }; then
  rm -rf "$B/store" "$B/roots.txt" "$B/maps" "$STEPS/maps.txt"
  run_to "$STEPS/seed.txt" "$BIN" seed --tree "$T1" --store "$B/store"
  done_ok "$STEPS/seed.txt" || { echo "the seed step failed" >&2; exit 1; }
  run_to "$STEPS/whole.txt" "$BIN" whole --store "$B/store" --roots "$B/roots.txt" --seed "$SEED" --ks "$KS" \
    --commits "$COMMITS" --patterns "scatter insdel" --next-tree "$T2"
  done_ok "$STEPS/whole.txt" || { echo "the whole step failed" >&2; exit 1; }
fi
if ! { done_ok "$STEPS/maps.txt" && [ -f "$B/maps/plan.txt" ]; }; then
  rm -rf "$B/maps"
  run_to "$STEPS/maps.txt" "$BIN" maps --store "$B/store" --roots "$B/roots.txt" --out "$B/maps"
  done_ok "$STEPS/maps.txt" || { echo "the maps step failed" >&2; exit 1; }
fi
for layout in $LAYOUTS; do
  for c in $CONFIGS; do
    row "boost-0-$layout-${c%:*}-${c#*:}" "$BIN" layout --maps "$B/maps" --layout "$layout" --l "${c%:*}" --node-max "${c#*:}"
  done
done

# ── synthetic rows ──
for n in $SYNTH_NS; do
  for layout in $LAYOUTS; do
    for c in $CONFIGS; do
      row "synth-$n-$layout-${c%:*}-${c#*:}" "$BIN" synth --n "$n" --layout "$layout" --l "${c%:*}" --node-max "${c#*:}" \
        --ks "$SYNTH_KS" --commits "$COMMITS" --seed "$SEED"
    done
  done
done

# ── CSV files ──
CSV_RC=0
python3 - "$WORK" <<'PY' || CSV_RC=$?
import csv, glob, os, sys

work = sys.argv[1]
rows = sorted(glob.glob(os.path.join(work, "rows", "*.txt")))


def fields(line):
    kv = {}
    for w in line.split()[1:]:
        if "=" in w:
            k, v = w.split("=", 1)
            kv[k] = v
    return kv


LAYOUT = "source,n,layout,l,node_max,pattern,k,seq,edits,ms,cpu_ms,maxrss_bytes,new_objects,new_stored_bytes,nodes_read,nodes_written,bytes_written,forced_cuts".split(",")
SHAPE = "source,n,layout,l,node_max,phase,map,entries,depth,levels,leaf_bytes,internal_bytes,internal_entries,forced_cuts".split(",")
BUILD = "source,n,layout,l,node_max,ms,cpu_ms,maxrss_bytes,nodes_written,bytes_written,forced_cuts".split(",")
lay, shp, bld, chk = [], [], [], []
for path in rows:
    with open(path) as f:
        lines = f.read().splitlines()
    if not lines or lines[-1] != "rc=0":
        continue
    for line in lines:
        kind = line.split(" ", 1)[0]
        if "mode=layout" not in line:
            continue
        kv = fields(line)
        if kind in ("commit", "build"):
            kv["cpu_ms"] = "%.3f" % (1000 * (float(kv["cpu_user_s"]) + float(kv["cpu_sys_s"])))
        if kind == "commit":
            lay.append([kv[c] for c in LAYOUT])
        elif kind == "tree":
            shp.append([kv[c] for c in SHAPE])
        elif kind == "build":
            bld.append([kv[c] for c in BUILD])
        elif kind == "check":
            chk.append(line)
for name, head, data in (("layout.csv", LAYOUT, lay), ("shape.csv", SHAPE, shp), ("build.csv", BUILD, bld)):
    with open(os.path.join(work, name), "w", newline="") as f:
        w = csv.writer(f, lineterminator="\n")
        w.writerow(head)
        w.writerows(data)
with open(os.path.join(work, "checks.txt"), "w") as f:
    f.write("".join(line + "\n" for line in chk))
print("layout.csv %d rows, shape.csv %d rows, build.csv %d rows, checks.txt %d lines" % (len(lay), len(shp), len(bld), len(chk)), file=sys.stderr)
bad = [line for line in chk if not line.endswith(" ok=1")]
if bad:
    print("checks that are not ok=1: %d" % len(bad), file=sys.stderr)
    sys.exit(1)
PY

# ── machine.txt ──
if [ "$OS" = Darwin ]; then
  cpu=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown)
  cpus=$(sysctl -n hw.ncpu); mem=$(sysctl -n hw.memsize)
  dev=$(df "$WORK" | awk 'NR == 2 {print $1}')
  fs=$(mount | awk -v d="$dev" '$1 == d { s = $0; sub(/.*\(/, "", s); sub(/[,)].*/, "", s); print s; exit }')
else
  cpu=$(awk -F': ' '/^model name/ {print $2; exit}' /proc/cpuinfo)
  [ -n "$cpu" ] || cpu=$(lscpu 2>/dev/null | awk -F': *' '/^Model name/ {print $2; exit}')
  cpus=$(nproc); mem=$(awk '/^MemTotal/ {printf "%.0f\n", $2 * 1024}' /proc/meminfo)
  fs=$(df -T "$WORK" | awk 'NR == 2 {print $2}')
fi
if [ "$LAYOUT_SMOKE" = 1 ]; then BENCH_DATA=smoke; else BENCH_DATA=boost; fi
{
  echo "os=$(uname -sr)"
  echo "arch=$(uname -m)"
  echo "cpu=${cpu:-unknown}"
  echo "cpus=$cpus"
  echo "mem_bytes=$mem"
  echo "fs=${fs:-unknown}"
  echo "rustc=$(rustc -V 2>/dev/null || echo unknown)"
  echo "commit=$(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  echo "page_cache=warm (not dropped)"
  echo "load_before=$LOAD_BEFORE"
  echo "load_after=$(loadavg)"
  echo "start_utc=$START_UTC"
  echo "end_utc=$(utc)"
  echo "bench_data=$BENCH_DATA"
  echo "configs=$CONFIGS"
  echo "layouts=$LAYOUTS"
  echo "synth_ns=$SYNTH_NS"
  echo "boost_vers=$BOOST_VERS"
  echo "ks=$KS"
  echo "synth_ks=$SYNTH_KS"
  echo "commits=$COMMITS"
  echo "seed=$SEED"
  echo "row_timeout_s=$ROW_TIMEOUT"
} > "$WORK/machine.txt"

# The Boost store and the smoke data are removed when the run is whole, unless RESUME=1.
if [ "$FAILED" -eq 0 ] && [ "$CSV_RC" -eq 0 ] && [ "$RESUME" != 1 ]; then rm -rf "${B:?}" "${S:?}"; fi
echo "results: layout.csv, shape.csv, build.csv, checks.txt, machine.txt and rows/ in $WORK; failed rows: $FAILED" >&2
[ "$FAILED" -eq 0 ] && [ "$CSV_RC" -eq 0 ] || exit 1
trap '' INT TERM HUP
