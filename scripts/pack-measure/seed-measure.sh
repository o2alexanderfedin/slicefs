#!/bin/bash
# Measure `slicefs seed`: wall time, MB/s, peak RSS, and the store's apparent
# bytes, allocated bytes and file count (split by top dir), per input and run.
#
# usage: seed-measure.sh <slicefs-binary> <data-dir> [runs]
# env:   WORK        work dir for stores, logs and results (default: a new temp dir)
#        LABEL       build name in the CSV (default: basename of the binary)
#        INPUTS      inputs to measure, in order (default: tree tar rand)
#        BOOST_VER   Boost version of the tree and tar inputs (default 1_85_0)
#        RAND_BYTES  size of the random file (default 1073741824)
#        RUNS        runs per input when [runs] is not given (default 3)
#        BOOST_DIR   data dir when <data-dir> is not given
# inputs: tree = DATA/x/boost_V/; tar = DATA/boost_V.tar (see fetch-boost.sh);
#         rand = one random file, made once in DATA/in-rand-RAND_BYTES/.
# stdout: CSV only; progress goes to stderr. Rows also go to $WORK/results-$LABEL.csv.
#         RUNS rows per input, then one row with run=median (median_low per column).
set -euo pipefail
usage() { echo "usage: seed-measure.sh <slicefs-binary> <data-dir> [runs]" >&2; exit 2; }
[ "$(uname -s)" = Darwin ] || { echo "seed-measure.sh needs macOS (stat -f, /usr/bin/time -l)" >&2; exit 2; }
B=${1:-}; D=${2:-${BOOST_DIR:-}}; N=${3:-${RUNS:-3}}
[ -n "$B" ] && [ -x "$B" ] || usage
[ -n "$D" ] && [ -d "$D" ] || usage
[[ "$N" =~ ^[1-9][0-9]*$ ]] || usage
if [ -z "${WORK:-}" ]; then WORK=$(mktemp -d); echo "work dir: $WORK" >&2; fi
mkdir -p "$WORK"
LABEL=${LABEL:-$(basename "$B")}
INPUTS=${INPUTS:-tree tar rand}
BOOST_VER=${BOOST_VER:-1_85_0}
RAND_BYTES=${RAND_BYTES:-1073741824}
R=$WORK/results-$LABEL.csv
HDR=build,input,run,in_bytes,in_files,secs,mb_s,apparent_bytes,alloc_bytes,files,vt0_files,packs_files,segments_files,other_files,peak_rss_bytes
echo "$HDR"; echo "$HDR" > "$R"
row() { echo "$1"; echo "$1" >> "$R"; }

# Sum of file sizes under a path (bytes).
fbytes() { find "$1" -type f -exec stat -f %z {} + 2>/dev/null | awk '{s+=$1} END{print s+0}'; }
# Count of regular files under a path (0 when the path is absent).
nfiles() { if [ -d "$1" ]; then find "$1" -type f | wc -l | tr -d ' '; else echo 0; fi; }

nodata() { echo "missing $1; get it with scripts/macos-mount-test/fetch-boost.sh DATA_DIR $BOOST_VER" >&2; exit 2; }
# median <input>: one row with run=median; each numeric column is the median_low
# over the run rows of this build and input (always a value from a real run).
median() {
  python3 -c '
import csv, statistics, sys
f, b, i = sys.argv[1:4]
rows = [r for r in csv.reader(open(f)) if r[0] == b and r[1] == i and r[2] != "run" and r[2] != "median"]
print(",".join([b, i, "median"] + [statistics.median_low([(float(r[c]), r[c]) for r in rows])[1] for c in range(3, 15)]))
' "$R" "$LABEL" "$1"
}

# src_for <input>: print the source dir for an input; make it when needed.
src_for() {
  local v=$BOOST_VER
  case $1 in
    tree)
      [ -d "$D/x/boost_$v" ] || nodata "$D/x/boost_$v"
      echo "$D/x/boost_$v" ;;
    tar)
      local s=$D/in-tar-$v
      [ -f "$D/boost_$v.tar" ] || nodata "$D/boost_$v.tar"
      mkdir -p "$s"
      # A hard link (or a copy), not a symlink: seed skips symlinks.
      ln -f "$D/boost_$v.tar" "$s/boost_$v.tar" 2>/dev/null || cp "$D/boost_$v.tar" "$s/boost_$v.tar"
      echo "$s" ;;
    rand)
      local s=$D/in-rand-$RAND_BYTES f
      f=$s/random.bin
      mkdir -p "$s"
      if [ ! -f "$f" ]; then
        echo "making $f ($RAND_BYTES bytes)" >&2
        head -c "$RAND_BYTES" /dev/urandom > "$f.tmp"; mv "$f.tmp" "$f"
      fi
      [ "$(stat -f %z "$f")" -eq "$RAND_BYTES" ] || { echo "$f is not $RAND_BYTES bytes" >&2; exit 1; }
      echo "$s" ;;
    *) echo "unknown input: $1" >&2; exit 2 ;;
  esac
}

for IN in $INPUTS; do
  src=$(src_for "$IN")
  ib=$(fbytes "$src"); inf=$(nfiles "$src")
  echo "== $LABEL $IN: $inf files, $ib bytes; warming page cache" >&2
  find "$src" -type f -exec cat {} + > /dev/null
  d=${WORK:?}/$LABEL/$IN; mkdir -p "$d"
  for i in $(seq 1 "$N"); do
    store=${WORK:?}/$LABEL/$IN/store-$i; tf=$d/time-$i.txt; log=$d/seed-$i.log
    rm -rf "${WORK:?}/$LABEL/$IN/store-$i"
    echo "  run $i ..." >&2
    rc=0; /usr/bin/time -l -o "$tf" "$B" seed "$store" "$src" > "$log" 2>&1 || rc=$?
    if [ $rc -ne 0 ]; then
      echo "seed failed: build=$LABEL input=$IN run=$i rc=$rc" >&2; tail -5 "$log" >&2; exit 1
    fi
    secs=$(awk '/ real/ {print $1; exit}' "$tf")
    rss=$(awk '/maximum resident set size/ {print $1; exit}' "$tf")
    mbs=$(python3 -c "import sys; print(f'{int(sys.argv[1])/1e6/max(float(sys.argv[2]),0.01):.1f}')" "$ib" "$secs")
    ab=$(fbytes "$store"); al=$(( $(du -sk "$store" | cut -f1) * 1024 ))
    nf=$(nfiles "$store"); nv=$(nfiles "$store/vt0"); np=$(nfiles "$store/packs"); ns=$(nfiles "$store/segments")
    row "$LABEL,$IN,$i,$ib,$inf,$secs,$mbs,$ab,$al,$nf,$nv,$np,$ns,$((nf - nv - np - ns)),$rss"
    echo "  run $i: ${secs}s ${mbs} MB/s, store $ab bytes, $nf files" >&2
    rm -rf "${WORK:?}/$LABEL/$IN/store-$i"
  done
  m=$(median "$IN"); row "$m"
done
