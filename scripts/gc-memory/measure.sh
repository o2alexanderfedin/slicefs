#!/bin/bash
# The container side of run.sh: build, then for each N make a store and
# measure each command. Peak RSS is from /usr/bin/time (max resident set).
# anon is the highest RssAnon of a sample every 50 ms: RSS without the
# mapped index file pages, so it is the memory that the process allocates.
# For the real rehash, the anon peak after the "rehash: gc" progress line
# is the full GC of step 6.
set -uo pipefail
TO=${GCMEM_STEP_TIMEOUT:-900}; SNAPS=${GCMEM_SNAPS:-1}
command -v /usr/bin/time >/dev/null || { apt-get update -qq && apt-get install -y -qq time >/dev/null; } || exit 1
# GCMEM_HEAPTRACK=1: also run gc, gc --full and rehash under heaptrack and
# print the allocation sites at the heap peak (release with line tables).
if [ -n "${GCMEM_HEAPTRACK:-}" ]; then
  apt-get install -y -qq heaptrack >/dev/null || exit 1
  export CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
fi
timeout 1800 cargo build -q --release --locked -p slicefs-cli || exit 1
B=/target/release/slicefs; W=/work; mkdir -p $W
mib() { awk -v k="$1" 'BEGIN { printf "%.1f", k / 1024 }'; }

# meas LABEL CMD...: run CMD under time and timeout; set PEAK (KiB),
# ANON (KiB), ANON_GC (KiB, after "rehash: gc"), WALL (s).
meas() {
  local label=$1; shift
  /usr/bin/time -f "%M %e" -o $W/time.txt timeout "$TO" "$@" >$W/out.txt 2>$W/err.txt &
  local tp=$! tc="" p="" a g=0 m=0
  while kill -0 $tp 2>/dev/null; do
    # time -> timeout -> the command.
    [ -n "$tc" ] || tc=$(awk '{print $1}' /proc/$tp/task/$tp/children 2>/dev/null)
    [ -z "$tc" ] || [ -n "$p" ] || p=$(awk '{print $1}' /proc/$tc/task/$tc/children 2>/dev/null)
    if [ -n "$p" ]; then
      a=$(awk '/^RssAnon/ {print $2}' /proc/$p/status 2>/dev/null)
      if [ -n "$a" ]; then
        [ "$a" -le "$m" ] || m=$a
        if grep -q "rehash: gc" $W/err.txt 2>/dev/null; then [ "$a" -le "$g" ] || g=$a; fi
      fi
    fi
    sleep 0.05
  done
  wait $tp; local rc=$?
  read -r PEAK WALL < <(tail -1 $W/time.txt)
  ANON=$m; ANON_GC=$g
  echo "# $label: rc=$rc peak=$(mib "$PEAK") MiB anon=$(mib "$ANON") MiB anon_gc=$(mib "$ANON_GC") MiB wall=${WALL}s"
  [ $rc -eq 0 ] || { echo "# $label failed:"; tail -5 $W/err.txt $W/out.txt; }
  return $rc
}

nproc; grep MemTotal /proc/meminfo; df -h $W | tail -1
ROWS=()
for N in "$@"; do
  echo "## N=$N snapshots=$SNAPS"
  rm -rf $W/src $W/store
  t0=$SECONDS
  # Unique content per file, 1000 files per directory.
  python3 -I -c '
import os, sys
n, root = int(sys.argv[1]), sys.argv[2]
for i in range(n):
    d = os.path.join(root, "d%05d" % (i // 1000))
    if i % 1000 == 0: os.makedirs(d)
    with open(os.path.join(d, "f%d" % i), "w") as f: f.write("file %d\n" % i)
' "$N" $W/src || exit 1
  echo "# source: $((SECONDS - t0)) s"
  meas seed $B seed $W/store $W/src || exit 1; SEED=$PEAK; SEED_W=$WALL
  rm -rf $W/src
  for s in $(seq 1 "$SNAPS"); do $B snapshot create $W/store --name "s$s" >/dev/null || exit 1; done
  SIZE=$(du -sk $W/store | awk '{print $1}')
  meas gc $B gc $W/store || exit 1; GC=$PEAK; GC_A=$ANON
  meas gc-full $B gc --full $W/store || exit 1; GCF=$PEAK; GCF_A=$ANON; GCF_W=$WALL
  meas dry-run $B rehash --to sha256 --dry-run $W/store || exit 1; DRY=$PEAK; DRY_A=$ANON; DRY_W=$WALL
  tail -4 $W/out.txt
  if [ -n "${GCMEM_HEAPTRACK:-}" ]; then
    # The real rehash last: it changes the store (the measured rehash
    # below then runs on a store that has the target hash already).
    for c in "gc" "gc --full" "rehash --to sha256 --dry-run" "rehash --to sha256"; do
      echo "### heaptrack: $c"
      rm -f $W/ht.*
      # shellcheck disable=SC2086 # $c is the subcommand and its flags
      timeout "$TO" heaptrack -o $W/ht $B $c $W/store >/dev/null 2>&1 || exit 1
      f=$(ls $W/ht.* | head -1)
      # The sites (crate code lines only), then the totals.
      heaptrack_print -f "$f" -p 1 -a 0 -T 0 -l 0 -n 15 -s 4 2>&1 \
        | grep -E "peak memory consumed|consumed over|^ +at crates/" | awk '/consumed/ {n = 0} n++ < 4' | head -150
      heaptrack_print -f "$f" -p 0 -a 0 -T 0 -l 0 2>&1 | grep -E "peak heap|peak RSS|total runtime"
    done
  fi
  meas rehash $B rehash --to sha256 $W/store || exit 1; RH=$PEAK; RH_A=$ANON; RH_G=$ANON_GC; RH_W=$WALL
  tail -12 $W/out.txt
  ROWS+=("$N|$SNAPS|$(mib "$SIZE")|$(mib "$SEED") / ${SEED_W}|$(mib "$GC") ($(mib "$GC_A"))|$(mib "$GCF") ($(mib "$GCF_A")) / ${GCF_W}|$(mib "$DRY") ($(mib "$DRY_A")) / ${DRY_W}|$(mib "$RH") ($(mib "$RH_A")) / ${RH_W}|$(mib "$RH_G")")
  rm -rf $W/store
done
echo
echo "| N files | snapshots | store MiB | seed peak MiB / wall s | gc peak (anon) MiB | gc --full peak (anon) MiB / wall s | dry-run peak (anon) MiB / wall s | rehash peak (anon) MiB / wall s | rehash step-6 GC anon MiB |"
echo "|---|---|---|---|---|---|---|---|---|"
for r in "${ROWS[@]}"; do echo "| ${r//|/ | } |"; done
