#!/bin/bash
# Pool bench on macOS FSKit (Phase 21, [Bench]): C = 4 clients on 4 different
# files of 64 MiB, at --workers 1 and 2, 3 runs each, in the order 1 2 2 1 1 2.
# CI only (pool-bench-macos.yml); do not run it on a desktop machine.
# usage: pool-bench.sh <slicefs-binary>
# env:   WORK  work dir (default: a new temp dir)
# Per run: a fresh store; mount A under /usr/bin/time -l, C parallel writers
# (1 MiB chunks, fsync per file), unmount; mount B, C parallel readers, cmp of
# every file; unmount. One line per run:
#   macbench n=<N> run=<i> write_mbs=<x> read_mbs=<x> mismatches=<k> maxrss_mib=<v>
# where maxrss_mib is the larger maximum resident set size of the two mounts.
# bash 3.2 and BSD tools: no timeout, no mountpoint, no date +%N.
B=$1; R=${WORK:-$(mktemp -d)}/pb; rm -rf $R
C=4; FILE_MIB=64; T=600
mkdir -p $R/src $R/seed $R/mnt; echo s > $R/seed/s.txt
echo "run dir: $R"
echo "sw_vers: $(sw_vers -productVersion)  cpus: $(sysctl -n hw.ncpu)  load: $(sysctl -n vm.loadavg)"
for i in $(seq $C); do head -c $((FILE_MIB * 1048576)) /dev/urandom > $R/src/s$i; done
FAILS=0
now() { python3 -c 'import time; print(f"{time.time():.3f}")'; }

cat > $R/cp.py <<'PY'
import os, sys
src, dst = sys.argv[1], sys.argv[2]
with open(src, "rb") as a, open(dst, "wb") as b:
    while True:
        d = a.read(1 << 20)
        if not d:
            break
        b.write(d)
    b.flush()
    os.fsync(b.fileno())
PY

mount_fs() { # mount_fs <N> <tag>: mount under /usr/bin/time -l (it prints the maximum RSS
  # of the daemon in bytes at exit); MP is the pid of time, the parent of the daemon
  /usr/bin/time -l $B mount --store $R/store --backend fskit --workers $1 $R/mnt </dev/null >$R/mount-$2.log 2>$R/time-$2.err &
  MP=$!
  for i in $(seq 1 60); do mount | grep -q " $R/mnt " && { echo "  workers: $(grep '^Workers:' $R/mount-$2.log | tail -1)"; return 0; }; kill -0 $MP 2>/dev/null || break; sleep 1; done
  echo "MOUNT FAILED"; tail -5 $R/mount-$2.log $R/time-$2.err; exit 1
}
unmount_fs() { # unmount_fs <tag>: unmount; time -l (and so the daemon) must exit
  local s=$(date +%s)
  ( $B unmount $R/mnt >/dev/null 2>&1 ) & local p=$!
  while kill -0 $p 2>/dev/null; do
    if [ $(( $(date +%s) - s )) -ge 60 ]; then kill -9 $p 2>/dev/null; echo "  unmount: HANG (>60s)"; FAILS=$((FAILS+1)); break; fi
    sleep 1
  done
  for i in $(seq 1 60); do kill -0 $MP 2>/dev/null || break; sleep 0.5; done
  if kill -0 $MP 2>/dev/null; then echo "  daemon-exit: FAIL"; FAILS=$((FAILS+1)); kill -9 $MP; pkill -9 -f "mount --store $R/store" 2>/dev/null; fi
  cat $R/mount-$1.log $R/time-$1.err | grep -o 'pool: workers=.*' | head -1 | sed 's/^/  /'
  RSS=$(awk '/maximum resident set size/{printf "%d", $1 / 1048576}' $R/time-$1.err)
}
par() { # par <fn>: run <fn> i for every client at once, each with a time limit of T s
  local fn=$1 pids="" p i s=$(date +%s)
  PBAD=0
  for i in $(seq $C); do $fn $i & pids="$pids $!"; done
  for p in $pids; do
    while kill -0 $p 2>/dev/null; do
      if [ $(( $(date +%s) - s )) -ge $T ]; then kill -9 $p 2>/dev/null; echo "  client: HANG (>${T}s)"; break; fi
      sleep 0.2
    done
    wait $p || PBAD=$((PBAD+1))
  done
}
w_one() { python3 $R/cp.py $R/src/s$1 $R/mnt/f$1; }
r_one() { cat $R/mnt/f$1 > /dev/null; }
c_one() { cmp -s $R/src/s$1 $R/mnt/f$1; }
mbs() { python3 -c "import sys; print(f'{float(sys.argv[1]) / (float(sys.argv[3]) - float(sys.argv[2])):.1f}')" $((C * FILE_MIB)) $1 $2; }

I1=0; I2=0
for n in 1 2 2 1 1 2; do
  if [ $n = 1 ]; then I1=$((I1+1)); i=$I1; else I2=$((I2+1)); i=$I2; fi
  echo "== n=$n run=$i"
  rm -rf $R/store; $B seed $R/store $R/seed >/dev/null 2>&1 || { echo "seed failed"; exit 1; }
  mount_fs $n a
  t0=$(now); par w_one; t1=$(now); wbad=$PBAD
  unmount_fs a; ra=$RSS
  mount_fs $n b
  t2=$(now); par r_one; t3=$(now); rbad=$PBAD
  par c_one; m=$PBAD
  unmount_fs b; rb=$RSS
  [ $wbad = 0 ] && [ $rbad = 0 ] || { echo "  clients failed: write=$wbad read=$rbad"; FAILS=$((FAILS+1)); }
  [ $m = 0 ] || FAILS=$((FAILS+1))
  rss=$ra; [ "${rb:-0}" -gt "${ra:-0}" ] 2>/dev/null && rss=$rb
  echo "macbench n=$n run=$i write_mbs=$(mbs $t0 $t1) read_mbs=$(mbs $t2 $t3) mismatches=$m maxrss_mib=$rss"
done
rm -rf $R/store $R/src
echo "pool-bench-macos: FAILS=$FAILS"
exit $([ $FAILS = 0 ] && echo 0 || echo 1)
