#!/bin/bash
# Benchmark of the prolly prototype against the metadata commit path (#19,
# PROLLY-02). Seed the first tree once; then, for each run, copy the seeded
# store, run the real commit path (whole), extract the inode_data change of
# each commit (batches), and replay the changes through the current map
# serializer (flat) and the prototype (prolly). Each step is its own process.
# In process, no mount. By hand only, never in cargo test. Runs on macOS and
# Linux. See README.md next to this script.
#
# usage: bench.sh <data-dir> [runs]
#        SMOKE=1 bench.sh                  (no data dir: generated trees in a stores dir under WORK)
# env:   WORK        work dir for the stores and results (default: a new temp dir);
#                    stores and smoke data go in a new WORK/stores.XXXXXX dir, removed at exit
#        BIN         the prolly-bench binary (default: build it in release)
#        RUNS        runs when [runs] is not given (default 3)
#        SMOKE=1     two small generated trees, short edit script
#        BOOST_VERS  the seed tree, then the next tree (default: 1_84_0 1_85_0)
#        KS          changed files per small commit (default: 1 10 100 1000)
#        COMMITS     small commits per pattern and K (default 10)
#        PATTERNS    edit patterns (default: scatter cluster)
#        SEED        seed of the edit script (default 24)
#        SCALE_NS    entry counts of the synthetic scale rows (default: 100000 1000000 4000000;
#                    empty skips them)
# inputs: DATA/x/boost_<v>/ for both versions of BOOST_VERS (see fetch-boost.sh).
# stdout: CSV only (results.csv); progress goes to stderr. Files in $WORK:
#         results.csv  one row per commit and mode, then one run=median row per
#                      (mode, pattern, k) (median_low per column)
#         machine.txt  machine and run parameters (no path, host or user)
set -euo pipefail
usage() { echo "usage: bench.sh <data-dir> [runs]   or   SMOKE=1 bench.sh   (no data dir)" >&2; exit 2; }
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
OS=$(uname -s)
SMOKE=${SMOKE:-0}
D=${1:-}; N=${2:-${RUNS:-3}}
[[ "$N" =~ ^[1-9][0-9]*$ ]] || usage
# The smoke data never go in a real data dir.
if [ "$SMOKE" = 1 ]; then [ -z "$D" ] || usage; else [ -n "$D" ] || usage; fi
command -v python3 > /dev/null || { echo "python3 is needed (the median rows)" >&2; exit 2; }
abs() { case $1 in /*) echo "$1" ;; *) echo "$PWD/$1" ;; esac; }

if [ "$SMOKE" = 1 ]; then
  BOOST_VERS="0_0_1 0_0_2"
  KS=${KS:-1 10}; COMMITS=${COMMITS:-2}; SCALE_NS=${SCALE_NS-10000 50000}
fi
BOOST_VERS=${BOOST_VERS:-1_84_0 1_85_0}
KS=${KS:-1 10 100 1000}; COMMITS=${COMMITS:-10}; PATTERNS=${PATTERNS:-scatter cluster}
SEED=${SEED:-24}; SCALE_NS=${SCALE_NS-100000 1000000 4000000}
read -r -a VERS <<< "$BOOST_VERS"
[ ${#VERS[@]} -eq 2 ] || { echo "BOOST_VERS must be two versions: the seed tree, then the next tree" >&2; exit 2; }
V1=${VERS[0]}; V2=${VERS[1]}
for v in $KS $COMMITS $SCALE_NS; do
  [[ "$v" =~ ^[1-9][0-9]*$ ]] || { echo "not a positive number: $v" >&2; exit 2; }
done
[[ "$SEED" =~ ^[0-9]+$ ]] || { echo "SEED is not a number: $SEED" >&2; exit 2; }

nodata() { echo "missing $1; get it with scripts/macos-mount-test/fetch-boost.sh DATA_DIR $BOOST_VERS" >&2; exit 2; }
# The data dir is only read, never written; a mistyped data dir is refused, not made.
if [ "$SMOKE" != 1 ]; then
  [ -d "$D" ] || nodata "$D"
  D=$(cd "$D" && pwd)
  for v in "$V1" "$V2"; do [ -d "$D/x/boost_$v" ] || nodata "$D/x/boost_$v"; done
fi

if [ -z "${WORK:-}" ]; then WORK=$(mktemp -d "${TMPDIR:-/tmp}/prolly-bench.XXXXXX"); echo "work dir: $WORK" >&2; fi
mkdir -p "$WORK"; WORK=$(cd "$WORK" && pwd)
[ -n "$WORK" ] && [ "$WORK" != / ] || { echo "refusing WORK=$WORK" >&2; exit 2; }

if [ -z "${BIN:-}" ]; then
  echo "building prolly-bench (release)" >&2
  (cd "$REPO" && cargo build --release -p prolly-bench >&2)
  BIN=${CARGO_TARGET_DIR:-$REPO/target}/release/prolly-bench
fi
BIN=$(abs "$BIN")
[ -x "$BIN" ] || { echo "BIN is not executable: $BIN" >&2; exit 2; }

# Every store (and the smoke data) lives in a dir that this script makes
# (mktemp), so the rm -rf never removes a path that the script did not make.
# A signal exits through the EXIT trap too.
S=$(mktemp -d "${WORK:?}/stores.XXXXXX")
trap 'rm -rf "$S"' EXIT
trap 'exit 130' INT TERM HUP

if [ "$SMOKE" = 1 ]; then
  D=$S/smoke-data
  echo "making smoke data" >&2
  "$BIN" gen tree --dir "$D/x/boost_$V1" --files 2000 --seed 1 --max-bytes 4096 >&2
  "$BIN" gen tree --dir "$D/x/boost_$V2" --files 2100 --seed 1 --max-bytes 4096 --change-every 10 >&2
fi
T1=$D/x/boost_$V1; T2=$D/x/boost_$V2

loadavg() {
  if [ "$OS" = Darwin ]; then sysctl -n vm.loadavg | tr -d '{}' | awk '{print $1, $2, $3}'
  else cut -d' ' -f1-3 /proc/loadavg; fi
}
utc() { date -u +%Y-%m-%dT%H:%M:%SZ; }
START_UTC=$(utc); LOAD_BEFORE=$(loadavg)

# Peak RSS of each step: /usr/bin/time -l (macOS, bytes) or -v (GNU, kbytes), else NA.
TIME_OPT=
if [ -x /usr/bin/time ]; then
  if [ "$OS" = Darwin ]; then TIME_OPT=-l
  elif /usr/bin/time -v true > /dev/null 2>&1; then TIME_OPT=-v; fi
fi
peak_rss() {
  case $TIME_OPT in
    -l) awk '/maximum resident set size/ {print $1; exit}' "$1" ;;
    -v) awk -F: '/Maximum resident set size/ {gsub(/ /, "", $2); printf "%.0f\n", $2 * 1024; exit}' "$1" ;;
    *) echo NA ;;
  esac
}
# step <name> <out> <cmd...>: run one step as its own process, stdout to <out>;
# set RSS to its peak RSS. A failed step stops the script with its exit code.
step() {
  local name=$1 out=$2 rc=0; shift 2
  echo "run $r: $name" >&2
  if [ -n "$TIME_OPT" ]; then
    /usr/bin/time "$TIME_OPT" -o "$S/time.txt" "$@" > "$out" 2> "$S/step.log" || rc=$?
  else
    "$@" > "$out" 2> "$S/step.log" || rc=$?
  fi
  if [ "$rc" -ne 0 ]; then echo "step $name failed (exit $rc, run $r)" >&2; tail -n 5 "$S/step.log" >&2; exit "$rc"; fi
  RSS=NA
  if [ -n "$TIME_OPT" ]; then RSS=$(peak_rss "$S/time.txt"); [ -n "$RSS" ] || RSS=NA; fi
}
# csv <file> <word> <run> <rss> <keys> [batch file]: one CSV row per line of
# <file> that starts with <word>; the values of the comma-separated <keys> in
# order, NA when a line lacks a key. The pseudo keys run and peak_rss_bytes take
# <run> and <rss>. With a batch file, a missing entries_changed is puts + dels
# of the batch line with the same seq (NA when there is none).
csv() {
  awk -v w="$2" -v run="$3" -v rss="$4" -v keys="$5" -v bf="${6:-}" '
    function parse(s, A,   n, F, i, e) {
      n = split(s, F, " ")
      for (i = 2; i <= n; i++) { e = index(F[i], "="); if (e > 0) A[substr(F[i], 1, e - 1)] = substr(F[i], e + 1) }
      return F[1]
    }
    BEGIN {
      nk = split(keys, K, ",")
      if (bf != "") while ((getline l < bf) > 0) {
        split("", B)
        if (parse(l, B) == "batch") ec[B["seq"]] = B["puts"] + B["dels"]
      }
    }
    $1 == w {
      split("", V); parse($0, V); V["run"] = run; V["peak_rss_bytes"] = rss
      if (bf != "" && !("entries_changed" in V)) V["entries_changed"] = (V["seq"] in ec) ? ec[V["seq"]] : "NA"
      line = ""
      for (c = 1; c <= nk; c++) line = line (c > 1 ? "," : "") ((K[c] in V && V[K[c]] != "") ? V[K[c]] : "NA")
      print line
    }' "$1"
}
# median <csv> <key cols> <seq col>: one run=median row per group of the key
# columns (0-based, first-seen order) over the run rows: median_low per other
# column (always a value of a real run); the seq column (-1: none) is "all"; a
# column that is NA in every row of the group stays NA. Column 1 is run.
median() {
  python3 -c '
import csv, statistics, sys
f, keys, seq = sys.argv[1], [int(c) for c in sys.argv[2].split(",")], int(sys.argv[3])
rows = list(csv.reader(open(f)))
groups = {}
for r in rows[1:]:
    if r[1] != "median":
        groups.setdefault(tuple(r[c] for c in keys), []).append(r)
for g in groups.values():
    out = []
    for c in range(len(rows[0])):
        if c == 1: out.append("median")
        elif c in keys: out.append(g[0][c])
        elif c == seq: out.append("all")
        else:
            vals = [(float(r[c]), r[c]) for r in g if r[c] != "NA"]
            out.append(statistics.median_low(vals)[1] if vals else "NA")
    print(",".join(out))
' "$@" < /dev/null
}

R=$WORK/results.csv
HDR=mode,run,pattern,k,seq,entries_changed,serialized_bytes,begin_ms,finish_ms,total_ms,cpu_user_s,cpu_sys_s,maxrss_bytes,new_objects,new_raw_bytes,new_stored_bytes,new_record_bytes,nodes_read,nodes_written,peak_rss_bytes
echo "$HDR"; echo "$HDR" > "$R"
# rows <file>: print and append to results.csv.
rows() { tee -a "$R" < "$1"; }

# The page cache stays warm (not dropped): read both trees once before the runs.
echo "warming the page cache" >&2
find "$T1" "$T2" -type f -exec cat {} + > /dev/null

r=0
step seed "$S/seed.txt" "$BIN" seed --tree "$T1" --store "$S/seeded"

for r in $(seq 1 "$N"); do
  rm -rf "$S/run" "$S/b" "$S/flat" "$S/prolly" "$S/roots.txt"
  # A clone where the file system can (APFS, reflink), else a plain copy.
  if [ "$OS" = Darwin ]; then cp -cR "$S/seeded" "$S/run" 2> /dev/null || { rm -rf "$S/run"; cp -R "$S/seeded" "$S/run"; }
  else cp -a --reflink=auto "$S/seeded" "$S/run" 2> /dev/null || { rm -rf "$S/run"; cp -R "$S/seeded" "$S/run"; }; fi
  step whole "$S/whole.txt" "$BIN" whole --store "$S/run" --roots "$S/roots.txt" --seed "$SEED" --ks "$KS" \
    --commits "$COMMITS" --patterns "$PATTERNS" --next-tree "$T2"
  RSS_WHOLE=$RSS
  step batches "$S/batches.txt" "$BIN" batches --store "$S/run" --roots "$S/roots.txt" --out "$S/b"
  csv "$S/whole.txt" commit "$r" "$RSS_WHOLE" "$HDR" "$S/batches.txt" > "$S/rows.csv"; rows "$S/rows.csv"
  # flat first on odd runs, prolly first on even runs
  if [ $((r % 2)) -eq 1 ]; then ORDER="flat prolly"; else ORDER="prolly flat"; fi
  for M in $ORDER; do
    step "$M" "$S/$M.txt" "$BIN" "$M" --batches "$S/b" --store "$S/$M"
    csv "$S/$M.txt" commit "$r" "$RSS" "$HDR" > "$S/rows.csv"; rows "$S/rows.csv"
  done
  rm -rf "$S/run" "$S/b" "$S/flat" "$S/prolly" "$S/roots.txt"
done

median "$R" 0,2,3 4 > "$S/med.csv" || { echo "median failed: results.csv" >&2; exit 1; }
[ -s "$S/med.csv" ] || { echo "empty median rows: results.csv" >&2; exit 1; }
rows "$S/med.csv"

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
if [ "$SMOKE" = 1 ]; then BENCH_DATA=smoke; else BENCH_DATA=boost; fi
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
  echo "runs=$N"
  echo "ks=$KS"
  echo "commits=$COMMITS"
  echo "patterns=$PATTERNS"
  echo "seed=$SEED"
  echo "scale_ns=$SCALE_NS"
  echo "boost_vers=$BOOST_VERS"
} > "$WORK/machine.txt"
echo "results: $R, machine: $WORK/machine.txt" >&2
