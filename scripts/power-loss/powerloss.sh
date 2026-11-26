#!/bin/bash
# The power-loss loop of `slicefs rehash` (see README.md). Runs in the
# container of run.sh. Usage: powerloss.sh <slicefs-binary>
# PL_TO is the target hash (default sha256). PL_DEADLINE is the time limit of
# the loop in seconds (default 900). PL_ONLY=N runs crash point N only.
set -u
B=$1
TO=${PL_TO:-sha256}
DEADLINE=$((SECONDS + ${PL_DEADLINE:-900}))
W=/tmp/pl
LZ_BIN=/opt/lazyfs/lazyfs/build/lazyfs
SHIM=/opt/plshim.so
rm -rf $W; mkdir -p $W/src1/d $W/src2 $W/root $W/mnt $W/smnt $W/pts
die() { echo "POWERLOSS setup failed: $*"; exit 2; }

# --- The store: 2 roots (current and one snapshot), a value shared by two
# files and by both roots, about 0.6 MB. It is made on a plain directory and
# copied into the LazyFS root for each point, so its start is durable.
head -c 200000 /dev/urandom > $W/src1/a.bin
cp $W/src1/a.bin $W/src1/b.bin
echo hello > $W/src1/c.txt
head -c 300000 /dev/urandom > $W/src1/d/e.bin
head -c 100000 /dev/urandom > $W/src2/f.bin
echo changed > $W/src2/c.txt
cp -a $W/src1 $W/exp-snap
cp -a $W/src1 $W/exp-cur && cp -a $W/src2/. $W/exp-cur/
timeout 60 $B seed $W/base $W/src1 >/dev/null 2>&1 || die "seed 1"
timeout 60 $B snapshot create $W/base --name s1 >/dev/null 2>&1 || die "snapshot create"

# --- LazyFS: unsynced data stays in its cache until fsync; clear-cache drops it.
cat > $W/lazyfs.toml <<EOF
[faults]
fifo_path="$W/faults.fifo"
[cache]
apply_eviction=false
[cache.simple]
custom_size="256mb"
blocks_per_page=1
[filesystem]
log_all_operations=false
logfile=""
EOF
LZ=
lz_up() {
  : > $W/lazyfs.out
  # A wrapper shell, so that the abort of LazyFS at its exit prints no job line.
  bash -c '"$@" </dev/null > '$W'/lazyfs.out 2>&1' lazyfs $LZ_BIN $W/mnt --config-path $W/lazyfs.toml \
    -o allow_other -o modules=subdir -o subdir=$W/root -f 2>/dev/null &
  LZ=$!
  for _ in $(seq 40); do mountpoint -q $W/mnt && [ -p $W/faults.fifo ] && return 0; sleep .25; done
  return 1
}
lz_down() {
  mountpoint -q $W/mnt && { timeout 10 fusermount3 -u $W/mnt 2>/dev/null || timeout 10 fusermount3 -uz $W/mnt; }
  for _ in $(seq 40); do kill -0 $LZ 2>/dev/null || break; sleep .25; done
  kill -0 $LZ 2>/dev/null && { kill -9 $(cat /proc/$LZ/task/*/children) 2>/dev/null; sleep .5; }
  wait $LZ 2>/dev/null
  ! mountpoint -q $W/mnt
}
# The power loss: drop every unsynced byte that LazyFS holds.
lz_clear() {
  local n; n=$(grep -c "cache is cleared" $W/lazyfs.out)
  timeout 5 bash -c "echo lazyfs::clear-cache > $W/faults.fifo" || return 1
  for _ in $(seq 40); do [ "$(grep -c "cache is cleared" $W/lazyfs.out)" -gt "$n" ] && return 0; sleep .25; done
  return 1
}

# --- slicefs mounts, on a copy of the store (a mount writes the store; the
# copy keeps the crashed state for the rerun).
SM=
sm_up() { # <store> [snapshot]
  timeout 60 $B mount --store "$1" ${2:+--snapshot $2} $W/smnt </dev/null > $W/smount.log 2>&1 &
  SM=$!
  for _ in $(seq 40); do mountpoint -q $W/smnt && return 0; sleep .25; done
  return 1
}
sm_down() {
  timeout 15 $B unmount $W/smnt >/dev/null 2>&1 || timeout 10 fusermount3 -uz $W/smnt
  for _ in $(seq 40); do kill -0 $SM 2>/dev/null || break; sleep .25; done
  kill -0 $SM 2>/dev/null && kill -9 $SM
  wait $SM 2>/dev/null
  return 0
}
# Files of the current root and of snapshot 1 byte-equal the sources.
files_ok() { # <store>
  local c=$W/chk ok=0
  rm -rf $c; cp -a "$1" $c || return 1
  if sm_up $c; then timeout 60 diff -r $W/exp-cur $W/smnt >/dev/null || ok=1; else ok=1; fi; sm_down
  if [ $ok = 0 ]; then
    if sm_up $c 1; then timeout 60 diff -r $W/exp-snap $W/smnt >/dev/null || ok=1; else ok=1; fi; sm_down
  fi
  rm -rf $c
  return $ok
}
scrub_ok() { timeout 60 $B scrub "$1" > $W/scrub.out 2>&1 && grep -q "Status *: clean" $W/scrub.out; }
snap_root() { timeout 30 $B snapshot list "$1" 2>/dev/null | awk '$1 == "1" && $NF ~ /[.][.][.]$/ { print $NF }'; }
# Live objects with another hash than the target (dry run, writes nothing).
others() { timeout 60 $B rehash --to $TO --dry-run "$1" 2>/dev/null | sed -n 's/^Objects of another hash: \([0-9]*\),.*/\1/p'; }
kept() { sed -n 's/^GC (full): kept \([0-9]*\) objects.*/\1/p' "$1"; }

# --- The reference runs: the old roots (the base) and a clean run under the
# shim with no kill, which counts the sync-type calls and gives the new roots.
w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w   # FUSE gotcha of Docker, see scripts/docker-linux-test/run.sh
# The current root: the snapshot tree with the changes of src2, written
# through a mount (a seed into a store that has a root adds nothing).
sm_up $W/base || die "mount of the base"
cp -a $W/src2/. $W/smnt/ || die "write of src2"
sm_down
files_ok $W/base || die "the base store does not give the source files"
OLD_SNAP=$(snap_root $W/base); OLD_OTHERS=$(others $W/base)
[ -n "$OLD_SNAP" ] && [ "${OLD_OTHERS:-0}" -gt 0 ] || die "no old root ($OLD_SNAP, $OLD_OTHERS)"
cp -a $W/base $W/root/store
lz_up || die "LazyFS mount"
PL_LOG=$W/count.log LD_PRELOAD=$SHIM timeout 120 $B rehash --to $TO $W/mnt/store > $W/clean.out 2>&1 || die "clean rehash: $(tail -3 $W/clean.out)"
NEW_SNAP=$(snap_root $W/mnt/store); NEW_KEPT=$(kept $W/clean.out)
[ "$(others $W/mnt/store)" = 0 ] && [ -n "$NEW_SNAP" ] && [ "$NEW_SNAP" != "$OLD_SNAP" ] || die "clean run gave no new root"
lz_down || die "LazyFS unmount"
rm -rf $W/root/store
TOTAL=$(grep -c '^[0-9]' $W/count.log)
POINTS=$((TOTAL + 1))
echo "POWERLOSS setup: old snapshot root $OLD_SNAP ($OLD_OTHERS live objects of the old hash), new snapshot root $NEW_SNAP, clean run kept $NEW_KEPT objects"
echo "POWERLOSS setup: $TOTAL sync-type calls in a clean run, so $POINTS crash points (before each call, and after the last one)"

# --- The loop.
OK=0; FAIL=0; NOLD=0; NNEW=0; RUN=0
for n in $(seq 1 $POINTS); do
  [ -n "${PL_ONLY:-}" ] && [ "$n" != "$PL_ONLY" ] && continue
  if [ $SECONDS -gt $DEADLINE ]; then echo "POWERLOSS deadline reached before point $n"; FAIL=$((FAIL + 1)); break; fi
  RUN=$((RUN + 1))
  if [ $n -le $TOTAL ]; then
    at=$(awk -v n=$n '$1 == n { sub("/tmp/pl/mnt/store/?", "", $3); print "before " $2 " " ($3 == "" ? "." : $3) }' $W/count.log)
  else
    at="after the end"
  fi
  why=; root=?
  rm -rf $W/root/store; cp -a $W/base $W/root/store
  lz_up || why="LazyFS mount"
  if [ -z "$why" ]; then
    rm -f $W/pts/$n.log
    { PL_KILL_AT=$n PL_LOG=$W/pts/$n.log LD_PRELOAD=$SHIM timeout 120 $B rehash --to $TO $W/mnt/store > $W/pts/$n.out 2>&1; } 2>/dev/null
    rc=$?
    step=$(grep -o '^rehash: [a-z]*' $W/pts/$n.out | tail -1 | cut -d' ' -f2)
    if [ $n -le $TOTAL ]; then
      { [ $rc = 137 ] && grep -qx "KILL $n" $W/pts/$n.log; } || why="the kill did not fire (rc=$rc)"
    else
      [ $rc = 0 ] || why="the run with no kill failed (rc=$rc)"
    fi
  fi
  [ -z "$why" ] && { lz_clear || why="clear-cache"; }
  [ -z "$why" ] && { lz_down || why="LazyFS unmount"; }
  rm -rf $W/crashed; cp -a $W/root/store $W/crashed   # the state after the power loss, kept on a failure
  [ -z "$why" ] && { lz_up || why="LazyFS remount"; }
  S=$W/mnt/store
  # After the power loss: the store opens with the old roots or the new roots.
  if [ -z "$why" ]; then
    scrub_ok $S || why="scrub after the crash: $(grep -m1 -i 'status\|error' $W/scrub.out)"
  fi
  if [ -z "$why" ]; then
    sr=$(snap_root $S); ot=$(others $S)
    if [ "$sr" = "$OLD_SNAP" ] && [ "$ot" = "$OLD_OTHERS" ]; then root=old
    elif [ "$sr" = "$NEW_SNAP" ] && [ "$ot" = 0 ]; then root=new
    else why="roots are neither old nor new (snapshot root ${sr:-none}, live objects of another hash ${ot:-none})"; fi
  fi
  [ -z "$why" ] && { files_ok $S || why="files after the crash differ"; }
  # A rerun finishes the job.
  if [ -z "$why" ]; then
    timeout 120 $B rehash --to $TO $S > $W/pts/$n.rerun 2>&1 || why="rerun failed: $(tail -1 $W/pts/$n.rerun)"
  fi
  if [ -z "$why" ]; then
    scrub_ok $S || why="scrub after the rerun"
  fi
  if [ -z "$why" ]; then
    sr=$(snap_root $S); ot=$(others $S); k=$(kept $W/pts/$n.rerun)
    [ "$sr" = "$NEW_SNAP" ] && [ "$ot" = 0 ] || why="after the rerun: snapshot root $sr, live objects of another hash $ot"
    [ -z "$why" ] && [ "$k" != "$NEW_KEPT" ] && why="after the rerun the store keeps $k objects, a clean run $NEW_KEPT"
    [ -z "$why" ] && ! grep -q "$TO" $S/FORMAT && why="the header does not record $TO"
    [ -z "$why" ] && [ -e $S/rehash-tmp ] && why="rehash-tmp is left"
    [ -z "$why" ] && ls $S/segments | grep -q '\.tmp$' && why="a segment temp file is left"
  fi
  [ -z "$why" ] && { files_ok $S || why="files after the rerun differ"; }
  lz_down || { [ -z "$why" ] && why="LazyFS unmount at the end"; }
  case $root in old) NOLD=$((NOLD + 1)) ;; new) NNEW=$((NNEW + 1)) ;; esac
  if [ -z "$why" ]; then
    OK=$((OK + 1)); echo "point $n/$POINTS $at (last step: ${step:-none}): root=$root, scrub ok, files equal, rerun ok, final ok"
  else
    FAIL=$((FAIL + 1)); echo "point $n/$POINTS $at (last step: ${step:-none}): root=$root FAIL: $why"
    mkdir -p $W/failed; cp -a $W/crashed $W/failed/store-$n 2>/dev/null; cp $W/pts/$n.* $W/failed/ 2>/dev/null
  fi
done
echo "POWERLOSS points=$RUN ok=$OK fail=$FAIL old_root=$NOLD new_root=$NNEW"
[ $FAIL = 0 ] && [ $RUN -gt 0 ]
