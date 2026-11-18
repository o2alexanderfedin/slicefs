#!/bin/bash
# SPIKE (wal-memory): kill -9 safety and 1 GiB cp time for the write log
# options. Runs inside the Docker container. Usage: wal-spike.sh <slicefs-binary>
# The option is the env var SLICEFS_WAL_SPIKE of the daemon (unset, a, b).
B=$1; R=/tmp/ws; rm -rf $R; mkdir -p $R
REPS=${SPIKE_REPS:-3}; CP_REPS=${SPIKE_CP_REPS:-$REPS}; SKIP_KILL=${SPIKE_SKIP_KILL:-0}
mnt(){ # mnt <mode> <store> <mountpoint> [extra args...]
  local mode=$1 s=$2 m=$3; shift 3; mkdir -p $m
  SLICEFS_WAL_SPIKE=$mode $B mount --store $s "$@" $m </dev/null >>$R/mount.log 2>&1 & MP=$!
  for i in $(seq 1 20); do mountpoint -q $m && return 0; sleep 0.5; done
  echo "  NOT MOUNTED: $m"; tail -5 $R/mount.log; return 1; }
umnt(){ timeout -s KILL 15 $B unmount $1 >/dev/null 2>&1 || fusermount -u $1 2>/dev/null
  for i in $(seq 60); do kill -0 $MP 2>/dev/null || return 0; sleep 0.5; done; kill -9 $MP; }
crash(){ kill -9 $MP; sleep 0.5; fusermount -uz $1 2>/dev/null || umount -l $1 2>/dev/null; }
has(){ [ -e "$1" ] && cmp -s "$2" "$1" && echo kept || echo lost; }

mkdir -p $R/src; echo seed > $R/src/seed.txt
head -c 3000 /dev/urandom > $R/small.bin
head -c 1048576 /dev/urandom > $R/one.bin
head -c 10485760 /dev/urandom > $R/ten.bin

echo "== kill -9 with no fsync (commit interval 0: only the write log can keep the data)"
for wal in $([ $SKIP_KILL = 1 ] || echo per-op flush-on-fsync); do
for mode in base a b; do
  for rep in $(seq $REPS); do
    S=$R/s-$wal-$mode-$rep; M=$R/m-$wal-$mode-$rep
    $B seed $S $R/src >/dev/null 2>&1
    mnt $mode $S $M --commit-interval 0 --wal-strategy $wal || continue
    cp $R/small.bin $M/small.bin                          # one 3000-byte write, close
    dd if=$R/one.bin of=$M/one.bin bs=64k status=none     # 16 writes of 64 KiB, close
    cp $R/one.bin $M/tmp.bin && mv $M/tmp.bin $M/renamed.bin
    dd if=$R/ten.bin of=$M/ten.bin bs=128k status=none    # 10 MiB, close
    mkdir $M/dir && echo last > $M/dir/last.txt           # small ops at the end
    sleep 1; crash $M
    : > $R/mount.log   # only the remount's lines below
    if mnt base $S $M; then
      echo "  wal=$wal option=$mode rep=$rep: small=$(has $M/small.bin $R/small.bin) one=$(has $M/one.bin $R/one.bin) renamed=$(has $M/renamed.bin $R/one.bin) ten=$(has $M/ten.bin $R/ten.bin) last=$([ "$(cat $M/dir/last.txt 2>/dev/null)" = last ] && echo kept || echo lost) | $(grep -h Replayed $R/mount.log | tail -1 || true)"
      umnt $M
    fi
    rm -rf $S $M
  done
done
done

echo "== cap cuts a file: flush-on-fsync, one 6 MiB file, no fsync, kill -9"
head -c 6291456 /dev/urandom > $R/six.bin
for mode in base b; do
  S=$R/s6-$mode; M=$R/m6-$mode
  $B seed $S $R/src >/dev/null 2>&1
  mnt $mode $S $M --commit-interval 0 --wal-strategy flush-on-fsync || continue
  dd if=$R/six.bin of=$M/six.bin bs=128k status=none
  sleep 1; crash $M; : > $R/mount.log
  if mnt base $S $M; then
    if [ -e $M/six.bin ]; then sz=$(stat -c %s $M/six.bin); pre=$(cmp -s -n $sz $R/six.bin $M/six.bin && echo yes || echo no)
      echo "  option=$mode: six.bin exists, size $sz of 6291456, prefix of the source: $pre, mode $(stat -c %a $M/six.bin)"
    else echo "  option=$mode: six.bin does not exist"; fi
    echo "    $(grep -h Replayed $R/mount.log | tail -1)"
    umnt $M
  fi
  rm -rf $S $M
done

echo "== 1 GiB cp time, per-op (release build), load: $(cat /proc/loadavg)"
head -c 1073741824 /dev/urandom > $R/big.bin
for rep in $(seq $CP_REPS); do
  for mode in base a; do
    S=$R/sL-$mode; M=$R/mL-$mode
    $B seed $S $R/src >/dev/null 2>&1
    mnt $mode $S $M || continue
    l0=$(cut -d' ' -f1 /proc/loadavg); t0=$(date +%s.%N)
    cp $R/big.bin $M/big.bin
    t1=$(date +%s.%N)
    hwm=$(awk '/^VmHWM:/{print int($2/1024)}' /proc/$MP/status)
    cpu=$(awk '{print ($14+$15)/100}' /proc/$MP/stat)
    echo "  cp option=$mode rep=$rep: $(awk "BEGIN{printf \"%.2f\", $t1 - $t0}") s, daemon cpu ${cpu} s, VmHWM ${hwm} MiB, load ${l0}"
    umnt $M; rm -rf $S $M
  done
done
echo "== done"
