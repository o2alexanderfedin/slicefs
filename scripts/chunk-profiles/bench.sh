#!/bin/bash
# Benchmark of the chunk profiles (#67, PROF-03): for each run, input and
# profile, ingest the input into a new store in process (no mount), open it
# cold in a new process, and probe reads and small edits. By hand only, never
# in cargo test. Runs on macOS and Linux. See README.md next to this script.
#
# usage: bench.sh <data-dir> [runs]
#        SMOKE=1 bench.sh                  (no data dir: the smoke data go in $WORK/smoke-data)
# env:   WORK          work dir for the store, logs and results (default: a new temp dir);
#                      the store goes in a new WORK/store.XXXXXX dir that the script makes
#        BIN           the chunk_profile_bench binary (default: build it in release)
#        PROFILES      profiles MIN/AVG/MAX in KiB (default: 4/8/32 8/16/64 8/32/128 16/64/256 32/128/512)
#        INPUTS        inputs, in order (default: trees tar; also rand, sparse)
#        BOOST_VERS    Boost versions of the trees input, in order; the tar is the last one
#                      (default: 1_84_0 1_85_0)
#        RAND_BYTES    size of the rand file (default 1073741824)
#        SPARSE_BYTES  size of the sparse file (default 1073741824)
#        SEQ_BYTES     bytes of sequential 4 KiB reads from the start (default 67108864)
#        READS         random 4 KiB reads (default 10000)
#        EDIT_BYTES    edit base: the first EDIT_BYTES of the file (default 67108864)
#        EDIT_SIZE     bytes of each edit (default 4096)
#        RUNS          runs when [runs] is not given (default 3)
#        SMOKE=1       small synthetic inputs in $WORK/smoke-data, short probe
# inputs: trees  = every DATA/x/boost_<v>/ of BOOST_VERS, into one store (see fetch-boost.sh);
#         tar    = DATA/boost_<last v>.tar;
#         rand   = DATA/in-rand-RAND_BYTES/random.bin, made once;
#         sparse = DATA/in-sparse-SPARSE_BYTES/sparse.bin, made once.
# stdout: CSV only; progress goes to stderr. Rows also go to $WORK/results.csv:
#         one row per run, input and profile, then one row with run=median per
#         input and profile (median_low per column). Edit rows go to
#         $WORK/edits.csv, machine info to $WORK/machine.txt (no path, host or user).
set -euo pipefail
usage() { echo "usage: bench.sh <data-dir> [runs]   or   SMOKE=1 bench.sh   (no data dir)" >&2; exit 2; }
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
OS=$(uname -s)
SMOKE=${SMOKE:-0}
D=${1:-}; N=${2:-${RUNS:-3}}
[[ "$N" =~ ^[1-9][0-9]*$ ]] || usage
# The smoke data never go in a real data dir: gen writes over the files of
# the versions in BOOST_VERS, and those may be real Boost data.
if [ "$SMOKE" = 1 ]; then [ -z "$D" ] || usage; else [ -n "$D" ] || usage; fi
abs() { case $1 in /*) echo "$1" ;; *) echo "$PWD/$1" ;; esac; }

if [ -z "${WORK:-}" ]; then WORK=$(mktemp -d "${TMPDIR:-/tmp}/chunk-profiles.XXXXXX"); echo "work dir: $WORK" >&2; fi
mkdir -p "$WORK"; WORK=$(cd "$WORK" && pwd)
[ -n "$WORK" ] && [ "$WORK" != / ] || { echo "refusing WORK=$WORK" >&2; exit 2; }
# The store lives in a dir that this script makes (mktemp), so the rm -rf of
# each step never removes a path that the script did not make.
STORE_DIR=$(mktemp -d "${WORK:?}/store.XXXXXX")
trap 'rm -rf "$STORE_DIR"' EXIT
STORE=$STORE_DIR/store

if [ -z "${BIN:-}" ]; then
  echo "building chunk_profile_bench (release)" >&2
  (cd "$REPO" && cargo build --release -p chunk-store --example chunk_profile_bench >&2)
  BIN=${CARGO_TARGET_DIR:-$REPO/target}/release/examples/chunk_profile_bench
fi
BIN=$(abs "$BIN")
[ -x "$BIN" ] || { echo "BIN is not executable: $BIN" >&2; exit 2; }

PROFILES=${PROFILES:-4/8/32 8/16/64 8/32/128 16/64/256 32/128/512}
INPUTS=${INPUTS:-trees tar}
RAND_BYTES=${RAND_BYTES:-1073741824}
SPARSE_BYTES=${SPARSE_BYTES:-1073741824}
if [ "$SMOKE" = 1 ]; then
  D=$WORK/smoke-data
  BOOST_VERS=${BOOST_VERS:-0_0_1 0_0_2}
  EDIT_BYTES=${EDIT_BYTES:-4194304}; SEQ_BYTES=${SEQ_BYTES:-4194304}; READS=${READS:-500}
fi
BOOST_VERS=${BOOST_VERS:-1_84_0 1_85_0}
SEQ_BYTES=${SEQ_BYTES:-67108864}; READS=${READS:-10000}
EDIT_BYTES=${EDIT_BYTES:-67108864}; EDIT_SIZE=${EDIT_SIZE:-4096}
for v in "$RAND_BYTES" "$SPARSE_BYTES" "$SEQ_BYTES" "$READS" "$EDIT_BYTES" "$EDIT_SIZE"; do
  [[ "$v" =~ ^[1-9][0-9]*$ ]] || { echo "not a positive number: $v" >&2; exit 2; }
done
mkdir -p "$D"; D=$(cd "$D" && pwd)
LAST_V=${BOOST_VERS##* }

if [ "$SMOKE" = 1 ]; then
  set -- $BOOST_VERS
  [ $# -eq 2 ] || { echo "SMOKE=1 needs two BOOST_VERS" >&2; exit 2; }
  echo "making smoke data" >&2
  "$BIN" gen tree --dir "$D/x/boost_$1" --files 200 --seed 1 >&2
  "$BIN" gen tree --dir "$D/x/boost_$2" --files 200 --seed 1 --change-every 10 >&2
  "$BIN" gen file --path "$D/boost_$2.tar" --bytes 8388608 --seed 2 >&2
fi

nodata() { echo "missing $1; get it with scripts/macos-mount-test/fetch-boost.sh DATA_DIR $BOOST_VERS" >&2; exit 2; }
# src_for <input>: print the ingest inputs of an input, one per line; make rand and sparse once.
src_for() {
  local f v
  case $1 in
    trees)
      for v in $BOOST_VERS; do [ -d "$D/x/boost_$v" ] || nodata "$D/x/boost_$v"; echo "$D/x/boost_$v"; done ;;
    tar)
      [ -f "$D/boost_$LAST_V.tar" ] || nodata "$D/boost_$LAST_V.tar"
      echo "$D/boost_$LAST_V.tar" ;;
    rand)
      f=$D/in-rand-$RAND_BYTES/random.bin
      [ -f "$f" ] || { echo "making random.bin ($RAND_BYTES bytes)" >&2; "$BIN" gen file --path "$f.tmp" --bytes "$RAND_BYTES" --seed 3 >&2; mv "$f.tmp" "$f"; }
      echo "$f" ;;
    sparse)
      f=$D/in-sparse-$SPARSE_BYTES/sparse.bin
      [ -f "$f" ] || { echo "making sparse.bin ($SPARSE_BYTES bytes)" >&2; "$BIN" gen file --path "$f.tmp" --bytes "$SPARSE_BYTES" --seed 4 --sparse >&2; mv "$f.tmp" "$f"; }
      echo "$f" ;;
    *) echo "unknown input: $1" >&2; exit 2 ;;
  esac
}
# Check every input before the first run.
for IN in $INPUTS; do src_for "$IN" > /dev/null; done

# kv <file> <word> <key>: the value of key=... on the first line that starts with word.
kv() { awk -v w="$2" -v k="$3" '$1 == w { for (i = 2; i <= NF; i++) { n = index($i, "="); if (substr($i, 1, n - 1) == k) { print substr($i, n + 1); exit } } }' "$1"; }
loadavg() {
  if [ "$OS" = Darwin ]; then sysctl -n vm.loadavg | tr -d '{}' | awk '{print $1, $2, $3}'
  else cut -d' ' -f1-3 /proc/loadavg; fi
}
utc() { date -u +%Y-%m-%dT%H:%M:%SZ; }
START_UTC=$(utc); LOAD_BEFORE=$(loadavg)

# Peak RSS of the cold open: /usr/bin/time -l (macOS, bytes) or -v (GNU, kbytes), else NA.
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

R=$WORK/results.csv; E=$WORK/edits.csv
HDR=profile,input,run,in_bytes,in_files,ingest_secs,ingest_mb_s,cpu_user_s,cpu_sys_s,objects,raw_bytes,stored_bytes,apparent_bytes,alloc_bytes,index_runs,index_run_entries,index_memtable_entries,index_filter_bytes,index_fence_bytes,open_ms,open_peak_rss_bytes,seq_p50_us,seq_p99_us,rand_p50_us,rand_p99_us,edit_objects,edit_stored_bytes,edit_rechunked_bytes
echo "$HDR"; echo "$HDR" > "$R"
echo profile,input,run,kind,where,objects,stored_bytes,rechunked_bytes > "$E"
row() { echo "$1"; echo "$1" >> "$R"; }
fail() { echo "$1 failed: profile=$P input=$IN run=$i" >&2; tail -n 5 "$2" >&2; exit 1; }

# The page cache stays warm (not dropped): read each input once before the runs.
for IN in $INPUTS; do
  echo "== $IN: warming the page cache" >&2
  src_for "$IN" | while IFS= read -r s; do find "$s" -type f -exec cat {} + > /dev/null; done
done

TIMEFORMAT='%3U %3S'
for i in $(seq 1 "$N"); do
  for IN in $INPUTS; do
    SRC=(); while IFS= read -r s; do SRC+=("$s"); done < <(src_for "$IN")
    for P in $PROFILES; do
      echo "run $i, $IN, $P ..." >&2
      rm -rf "$STORE"
      ing=$WORK/ingest.txt; opn=$WORK/open.txt; prb=$WORK/probe.txt; tf=$WORK/time.txt; log=$WORK/step.log
      { time "$BIN" ingest --profile "$P" --store "$STORE" "${SRC[@]}" > "$ing" 2> "$log"; } 2> "$WORK/cpu.txt" || fail ingest "$log"
      read -r cpu_u cpu_s < "$WORK/cpu.txt"
      if [ -n "$TIME_OPT" ]; then
        /usr/bin/time "$TIME_OPT" -o "$tf" "$BIN" open --profile "$P" --store "$STORE" > "$opn" 2> "$log" || fail open "$log"
        rss=$(peak_rss "$tf"); [ -n "$rss" ] || rss=NA
      else
        "$BIN" open --profile "$P" --store "$STORE" > "$opn" 2> "$log" || fail open "$log"
        rss=NA
      fi
      in_bytes=$(kv "$ing" ingest in_bytes); secs=$(kv "$ing" ingest secs)
      mbs=$(awk -v b="$in_bytes" -v s="$secs" 'BEGIN { if (s < 0.001) s = 0.001; printf "%.1f", b / s / 1e6 }')
      probe="NA,NA,NA,NA,NA,NA,NA"
      case $IN in
        tar|rand|sparse)
          "$BIN" probe --profile "$P" --store "$STORE" --key "$(kv "$ing" ingest key)" --seq-bytes "$SEQ_BYTES" \
            --reads "$READS" --edit-bytes "$EDIT_BYTES" --edit-size "$EDIT_SIZE" > "$prb" 2> "$log" || fail probe "$log"
          probe=$(for k in seq_p50_us seq_p99_us rand_p50_us rand_p99_us edit_objects edit_stored_bytes edit_rechunked_bytes; do kv "$prb" probe "$k"; done | paste -sd, -)
          awk -v p="$P" -v in_="$IN" -v r="$i" '$1 == "edit" { line = p "," in_ "," r; for (f = 2; f <= NF; f++) line = line "," substr($f, index($f, "=") + 1); print line }' "$prb" >> "$E" ;;
      esac
      ingest=$(for k in in_bytes in_files secs; do kv "$ing" ingest "$k"; done | paste -sd, -)
      stats=$(for k in objects raw_bytes stored_bytes; do kv "$ing" ingest "$k"; done | paste -sd, -)
      opened=$(for k in apparent_bytes alloc_bytes index_runs index_run_entries index_memtable_entries index_filter_bytes index_fence_bytes open_ms; do kv "$opn" open "$k"; done | paste -sd, -)
      row "$P,$IN,$i,$ingest,$mbs,$cpu_u,$cpu_s,$stats,$opened,$rss,$probe"
      rm -rf "$STORE"
    done
  done
done

# median_low per numeric column over the run rows of each input and profile
# (always a value of a real run); a column that is NA in every run stays NA.
for IN in $INPUTS; do
  for P in $PROFILES; do
    row "$(python3 -c '
import csv, statistics, sys
f, p, i = sys.argv[1:4]
rows = [r for r in csv.reader(open(f)) if r[0] == p and r[1] == i and r[2] not in ("run", "median")]
out = [p, i, "median"]
for c in range(3, 28):
    vals = [(float(r[c]), r[c]) for r in rows if r[c] != "NA"]
    out.append(statistics.median_low(vals)[1] if vals else "NA")
print(",".join(out))
' "$R" "$P" "$IN")"
  done
done

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
} > "$WORK/machine.txt"
echo "results: $R, edits: $E, machine: $WORK/machine.txt" >&2
