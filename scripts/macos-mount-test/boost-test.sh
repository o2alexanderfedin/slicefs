#!/bin/bash
# Boost round trip through a SliceFS mount on macOS (FUSE-T): copy to the mount,
# copy back, compare; remount and compare again; delete all; GC on the mounted store.
#
# usage: boost-test.sh <backend: fskit> <versions...>   e.g. boost-test.sh fskit 1_85_0
# env:   SLICEFS    slicefs binary (default: target/release/slicefs)
#        BOOST_DIR  dir with boost_<v>.tar and x/boost_<v>/ (see fetch-boost.sh)
#        WORK       work dir (default: a new temp dir)
#        STEP_TIMEOUT (s, default 3600), SKIP_TAR=1, SKIP_TREE=1
BK=$1; shift; VERS=("$@")
B=${SLICEFS:-target/release/slicefs}; DL=${BOOST_DIR:?set BOOST_DIR}
R=${WORK:-$(mktemp -d)}/bt-$BK; rm -rf $R
T=${STEP_TIMEOUT:-3600}
mkdir -p $R/seed $R/mnt $R/out; echo s > $R/seed/s.txt; echo "version,step,mb,files,secs" > $R/perf.csv
$B seed $R/store $R/seed >/dev/null 2>&1
echo "run dir: $R"
# CPU sampler: a timestamped top snapshot every 30 s in top.log, to see what
# else uses the CPU when a step is slow.
( while :; do echo "== $(date +%T)"; sysctl -n vm.loadavg; ps -Ao pid,pcpu,rss,state,comm -r | head -13; sleep 30; done ) > $R/top.log 2>&1 &
SAMPLER=$!
trap 'kill $SAMPLER 2>/dev/null' EXIT
FAILS=0

mount_fs() {
  $B mount --store $R/store --backend $BK $R/mnt </dev/null >>$R/mount.log 2>>$R/mount.err &
  MP=$!
  for i in $(seq 1 60); do mount | grep -q " $R/mnt " && return 0; sleep 1; done
  echo "MOUNT FAILED"; tail -5 $R/mount.log $R/mount.err; exit 1
}
unmount_fs() {
  step unmount 60 $B unmount $R/mnt
  for i in $(seq 1 40); do kill -0 $MP 2>/dev/null || break; sleep 0.5; done
  if kill -0 $MP 2>/dev/null; then echo "  daemon-exit: FAIL"; FAILS=$((FAILS+1)); kill -9 $MP; else echo "  daemon-exit: ok"; fi
}
# step <name> <timeout> <cmd...>: run with watchdog, print status + duration
step() {
  local n=$1 t=$2; shift 2; local s=$(date +%s)
  echo "  [$(date +%T)] $n ..."
  ( "$@" ) >$R/step.out 2>&1 & local p=$!
  while kill -0 $p 2>/dev/null; do
    if [ $(( $(date +%s) - s )) -ge $t ]; then kill -9 $p 2>/dev/null; echo "  $n: HANG (>${t}s)"; FAILS=$((FAILS+1)); return 1; fi
    sleep 1
  done
  wait $p; local rc=$? d=$(( $(date +%s) - s )); LAST_D=$d
  if [ $rc -eq 0 ]; then echo "  $n: ok (${d}s)"; else echo "  $n: FAIL rc=$rc (${d}s)"; head -5 $R/step.out | sed 's/^/      /'; FAILS=$((FAILS+1)); fi
  return $rc
}
rate() { python3 -c "import sys; mb,f,d=float(sys.argv[1]),int(sys.argv[2]),max(int(sys.argv[3]),1); print(f'      -> {mb/d:.1f} MB/s' + (f', {f/d:.0f} files/s' if f>1 else ''))" $1 $2 $LAST_D; echo "$CUR,$3,$1,$2,$LAST_D" >> $R/perf.csv; }
# "Logical bytes    : 908.24 KiB (930038 bytes)", or "... : 2 B" when small.
bytes() { $B stats $R/store 2>/dev/null | awk -v k="$1" '$0 ~ "^"k { if (match($0, /\([0-9]+ bytes\)/)) print substr($0, RSTART+1, RLENGTH-8); else { sub(/.*: /, ""); print $1 } }'; }
# Sum of file sizes under a path (bytes).
fbytes() { find "$1" -type f -exec stat -f %z {} + 2>/dev/null | awk '{s+=$1} END{print s+0}'; }
# Dedup across versions: after each version, the data added and how much the
# store grew (CAS bytes: packs and objects, apparent size).
echo "version,data_added,store_growth,data_total,store_total" > $R/dedup.csv
DATA_TOTAL=0; CAS_PREV=$(bytes "CAS bytes"); CAS_PREV=${CAS_PREV:-0}
# dedup <bytes added> <label>
dedup() {
  local cas=$(bytes "CAS bytes"); cas=${cas:-0}
  DATA_TOTAL=$((DATA_TOTAL + $1))
  python3 -c "import sys; v,a,g,dt,st=sys.argv[1],*map(int,sys.argv[2:]); m=lambda x: x/1048576; print(f'  dedup {v}: data +{m(a):.1f} MiB, store +{m(g):.1f} MiB ({a/max(g,1):.1f}x); total data {m(dt):.1f} MiB, store {m(st):.1f} MiB ({dt/max(st,1):.2f}x)')" \
    "$2" "$1" $((cas - CAS_PREV)) $DATA_TOTAL $cas
  echo "$2,$1,$((cas - CAS_PREV)),$DATA_TOTAL,$cas" >> $R/dedup.csv
  CAS_PREV=$cas
}
stats() { echo "  stats: $($B stats $R/store 2>/dev/null | awk -F': ' '/Logical|CAS bytes/{printf "%s=%s  ", $1, $2}' | tr -s ' ')"; }
objects() { find $R/store/vt0 -type f 2>/dev/null | wc -l | tr -d ' '; }
mb() { du -sm "$1" | cut -f1; }

mount_fs
mount | grep " $R/mnt " | sed 's/^/  /'
for v in "${VERS[@]}"; do
  echo "== boost_$v"
  if [ -z "$SKIP_TAR" ]; then
    tarf=$DL/boost_$v.tar
    echo "  tar: $(mb $tarf) MB"
    CUR=$v; TMB=$(mb $tarf)
    step tar-to $T cp $tarf $R/mnt/ && rate $TMB 1 tar-to
    stats
    step tar-from $T cp $R/mnt/boost_$v.tar $R/out/ && rate $TMB 1 tar-from
    step tar-cmp $T cmp $tarf $R/out/boost_$v.tar
    rm -f $R/out/boost_$v.tar
    dedup $(stat -f %z $tarf) "$v tar"
  fi
  if [ -z "$SKIP_TREE" ]; then
    src=$DL/x/boost_$v
    CUR=$v; XMB=$(mb $src); XN=$(find $src -type f | wc -l | tr -d ' ')
    echo "  tree: $XMB MB, $XN files"
    mkdir -p $R/mnt/trees 2>/dev/null
    step tree-to $T cp -Rp $src $R/mnt/trees/ && rate $XMB $XN tree-to
    stats
    step tree-from $T cp -Rp $R/mnt/trees/boost_$v $R/out/ && rate $XMB $XN tree-from
    step tree-diff $T diff -r --no-dereference $src $R/out/boost_$v
    step tree-modes $T bash -c "diff <(cd $src && find . -exec stat -f '%Sp %z %N' {} + | sort) <(cd $R/out/boost_$v && find . -exec stat -f '%Sp %z %N' {} + | sort)"
    rm -rf $R/out/boost_$v
    dedup $(fbytes $src) "$v tree"
  fi
done
echo "== dedup summary (data added -> store growth)"
column -t -s, $R/dedup.csv | sed 's/^/  /'
unmount_fs

echo "== remount + re-verify"
mount_fs
for v in "${VERS[@]}"; do
  [ -z "$SKIP_TAR" ] && step "tar-cmp-after-remount $v" $T cmp $DL/boost_$v.tar $R/mnt/boost_$v.tar
  [ -z "$SKIP_TREE" ] && step "tree-diff-after-remount $v" $T diff -r --no-dereference $DL/x/boost_$v $R/mnt/trees/boost_$v
done
stats
step "rm-all" $T rm -rf $R/mnt/trees $R/mnt/boost_*.tar
# FSKit volumes get a .fseventsd directory from macOS.
step "empty-after-rm" 30 bash -c "[ \$(ls -A $R/mnt | grep -v -e '^s.txt\$' -e '^.fseventsd\$' | wc -l) -eq 0 ]"
before=$(objects)
step "gc-mounted" $T $B gc $R/store && sed 's/^/      /' $R/step.out
after=$(objects); echo "  objects: $before -> $after"
# Files left on the mount: s.txt, and on FSKit the .fseventsd files of macOS.
left=$(find $R/mnt -type f -exec stat -f %z {} + 2>/dev/null | awk '{s+=$1} END{print s+0}')
unmount_fs
step "gc-offline-check" $T $B gc $R/store && head -1 $R/step.out | sed 's/^/      /'
# After the GCs, the store holds only the data of the files left: the logical
# bytes equal their sizes (+ up to 1 MiB: macOS can write .fseventsd at the
# unmount), and the stored objects are at most 2x that + 1 MiB (block and
# format cost). A leak of the deleted Boost data (GBs) fails here.
logical=$(bytes "Logical bytes"); cas=$(bytes "CAS bytes")
echo "  left: $left bytes in files; store: logical $logical, CAS $cas bytes"
if [ -n "$logical" ] && [ "$logical" -ge "$left" ] && [ "$logical" -le $(( left + 1048576 )) ] \
   && [ "$cas" -le $(( 2 * logical + 1048576 )) ]; then echo "  gc-freed: ok"
else echo "  gc-freed: FAIL"; FAILS=$((FAILS+1)); fi
echo "csv: $R/perf.csv"
echo "RESULT $BK: $FAILS failure(s)"
[ $FAILS -eq 0 ]
