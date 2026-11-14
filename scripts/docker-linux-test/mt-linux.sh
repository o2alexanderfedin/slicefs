#!/bin/bash
# Mount smoke test inside the container. Usage: mt-linux.sh <slicefs-binary>
B=$1; R=/tmp/run; rm -rf $R; mkdir -p $R/src $R/mnt; echo seed-file > $R/src/seed.txt
$B seed $R/store $R/src >/dev/null 2>&1
$B mount --store $R/store $R/mnt </dev/null > $R/mount.log 2>&1 &
MP=$!
for i in $(seq 1 20); do mountpoint -q $R/mnt && break; sleep 1; done
mountpoint -q $R/mnt || { echo "NOT MOUNTED"; cat $R/mount.log | tail; exit 1; }
F=0
step(){ local n=$1 t=$2; shift 2
  if timeout -s KILL $t "$@" >/dev/null 2>&1; then echo "  $n: ok"; else echo "  $n: FAIL"; F=1; fi; }
head -c 3000000 /dev/urandom > $R/r.bin
step read 10 cat $R/mnt/seed.txt
step echo-write 10 bash -c "echo hi > $R/mnt/e.txt"
step cp-3MB 120 cp $R/r.bin $R/mnt/r1.bin
step cp-again 120 cp $R/r.bin $R/mnt/r2.bin
step readback 60 cmp $R/r.bin $R/mnt/r2.bin
step mkdir-mv 10 bash -c "mkdir $R/mnt/d && mv $R/mnt/r1.bin $R/mnt/d/"
step chmod 10 bash -c "chmod 600 $R/mnt/e.txt && [ \$(stat -c %a $R/mnt/e.txt) = 600 ]"
step truncate 10 bash -c "truncate -s 1 $R/mnt/e.txt && [ \$(stat -c %s $R/mnt/e.txt) = 1 ]"
step xattr 10 bash -c "setfattr -n user.k -v v $R/mnt/e.txt && [ \"\$(getfattr --only-values -n user.k $R/mnt/e.txt)\" = v ]"
step rm 10 rm $R/mnt/d/r1.bin
step unmount 15 $B unmount $R/mnt
for i in $(seq 20); do kill -0 $MP 2>/dev/null || break; sleep .5; done
if kill -0 $MP 2>/dev/null; then echo "  daemon-exit: FAIL (still running 10s after unmount)"; F=1; kill -9 $MP; else echo "  daemon-exit: ok"; fi
$B stats $R/store 2>/dev/null | grep -iE "dedup|CAS" | head -3
exit $F
