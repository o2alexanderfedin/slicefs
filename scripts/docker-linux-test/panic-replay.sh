#!/bin/bash
# POOL-03 live check (plan 21-03): a panic on a request thread aborts the
# daemon (SIGABRT, status 134), and the remount replays the write log: the
# file that was fsynced before the panic is there, the half-done entry is not.
# Needs a debug build: SLICEFS_TEST_PANIC_CREATE exists only there.
# Runs inside the Docker container. Usage: panic-replay.sh <slicefs-binary>
# verify-phases.sh runs it; it can also run alone. Exit status 1 when a check failed.
B=$1; PRE=(); R=/tmp/pr; rm -rf $R; mkdir -p $R; F=0
ok(){ echo "  $1: ok"; }; bad(){ echo "  $1: FAIL ${2:-}"; F=1; }
step(){ local n=$1 t=$2; shift 2
  if timeout -s KILL $t "$@" >$R/step.out 2>&1; then ok "$n"; else bad "$n" "($(head -c 200 $R/step.out))"; fi; }
mnt(){ # mnt <store> <mountpoint> [extra args...]
  local s=$1 m=$2; shift 2; mkdir -p $m
  "${PRE[@]}" $B mount --store $s "$@" $m </dev/null >>$R/mount.log 2>&1 & MP=$!
  for i in $(seq 1 20); do mountpoint -q $m && return 0; sleep 0.5; done
  echo "  NOT MOUNTED: $m"; tail -5 $R/mount.log; F=1; return 1; }
umnt(){ # umnt <mountpoint>: unmount and require the daemon to exit
  timeout -s KILL 15 $B unmount $1 >/dev/null 2>&1 || fusermount -u $1 2>/dev/null
  for i in $(seq 20); do kill -0 $MP 2>/dev/null || return 0; sleep 0.5; done
  bad "daemon-exit $1" "(after 10s; $(tail -2 $R/mount.log | tr '\n' ' '))"; kill -9 $MP; }

echo "== POOL-03: a request panic aborts the daemon; the remount replays the write log"
mkdir -p $R/src; echo "seeded" > $R/src/seed.txt
head -c 1000000 /dev/urandom > $R/pa.bin
for W in 4 1; do   # 1 worker: the request runs on the reader thread (inline)
  S=$R/s$W; M=$R/m$W; : > $R/mount.log
  $B seed $S $R/src >/dev/null 2>&1
  # Create 2 in the mount panics; no commit timer, so only the write log keeps a.bin.
  PRE=(env SLICEFS_TEST_PANIC_CREATE=2)
  mnt $S $M --workers $W --commit-interval 0; up=$?
  PRE=()
  [ $up -eq 0 ] || continue
  # Create 1, fsynced. No other create, mkdir, mknod or symlink before b.txt.
  dd if=$R/pa.bin of=$M/a.bin bs=64k conv=fsync status=none || echo "    dd of a.bin failed"
  timeout 10 touch $M/b.txt 2>/dev/null && echo "    touch b.txt succeeded"   # create 2: the panic; a failure is expected
  for i in $(seq 20); do kill -0 $MP 2>/dev/null || break; sleep 0.5; done
  if kill -0 $MP 2>/dev/null; then
    bad "panic-aborts-w$W" "(daemon still running 10s after the panic)"; kill -9 $MP; wait $MP 2>/dev/null
  else
    wait $MP; st=$?
    if [ $st -eq 134 ]; then ok "panic-aborts-w$W"; else bad "panic-aborts-w$W" "(exit status $st, not 134)"; fi
  fi
  if grep -q "panicked: abort" $R/mount.log; then ok "panic-line-w$W"
  else bad "panic-line-w$W" "($(grep -m2 -E 'panicked|WARNING' $R/mount.log | tr '\n' ' '))"; fi
  fusermount -uz $M 2>/dev/null || umount -l $M 2>/dev/null
  if mnt $S $M --workers $W; then
    step "fsynced-file-replayed-w$W" 30 cmp $R/pa.bin $M/a.bin
    step "half-done-entry-absent-w$W" 10 test ! -e $M/b.txt
    umnt $M
  fi
done
echo "RESULT panic-replay: $([ $F -eq 0 ] && echo PASS || echo FAIL)"; exit $F
