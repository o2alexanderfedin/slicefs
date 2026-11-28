#!/bin/bash
# Mount smoke test inside the container. Usage: mt-linux.sh <slicefs-binary>
# SLICEFS_WORKERS=N mounts with --workers N, and the Workers line of the mount
# must then be "Workers: N (set)"; for N >= 2 the pool line must show jobs.
B=$1; R=/tmp/run; rm -rf $R; mkdir -p $R/src $R/mnt; echo seed-file > $R/src/seed.txt
$B seed $R/store $R/src >/dev/null 2>&1
$B mount --store $R/store ${SLICEFS_WORKERS:+--workers $SLICEFS_WORKERS} $R/mnt </dev/null > $R/mount.log 2>&1 &
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
# A 60,000-byte value (above the inline limit, so spilled), 300 names on
# one file, then a remove: all through the kernel (size query, then read).
head -c 60000 /dev/urandom > $R/xv
step xattr-big 30 bash -c "setfattr -n user.big -v 0s\$(base64 -w0 $R/xv) $R/mnt/e.txt && getfattr --only-values -n user.big $R/mnt/e.txt | cmp - $R/xv"
step xattr-many 120 bash -c "for i in \$(seq 1 300); do setfattr -n user.n\$i -v v\$i $R/mnt/e.txt || exit 1; done; [ \$(getfattr -d $R/mnt/e.txt | grep -c '^user\.n') = 300 ]"
step xattr-remove 10 bash -c "setfattr -x user.big $R/mnt/e.txt && ! getfattr -n user.big $R/mnt/e.txt"
# 3,000 names in one directory: the kernel reads the listing in many
# readdir replies, each filled from the streamed directory scan; rmdir
# refuses the directory until it is empty.
step readdir-many 180 bash -c "mkdir $R/mnt/big && for i in \$(seq 1 3000); do : > $R/mnt/big/f\$i || exit 1; done; [ \$(ls -f $R/mnt/big | wc -l) = 3002 ] && [ \$(ls $R/mnt/big | sort -u | wc -l) = 3000 ]"
step rmdir-notempty 10 bash -c "! rmdir $R/mnt/big 2>/dev/null"
step rmdir 120 bash -c "rm -f $R/mnt/big/f* && rmdir $R/mnt/big && [ ! -e $R/mnt/big ]"
# Unlink each name while the directory is read (one open handle, no
# rewind): the readdir cursor resumes after the last name, so no name is
# left behind when the names before it are gone. Names of 204 bytes, so
# the 1,000 names take many readdir replies (a kernel reply can hold as
# many bytes as the getdents buffer).
step readdir-unlink 180 bash -c "mkdir $R/mnt/del && printf -v p %0200d 0 && for i in \$(seq 1000 1999); do : > $R/mnt/del/\$p\$i || exit 1; done; perl -e 'opendir(D, \$ARGV[0]) or die; while (defined(my \$e = readdir D)) { next if \$e eq q(.) || \$e eq q(..); unlink(\$ARGV[0] . q(/) . \$e) or die } closedir D' $R/mnt/del && [ \$(ls -A $R/mnt/del | wc -l) = 0 ] && rmdir $R/mnt/del"
step rm 10 rm $R/mnt/d/r1.bin
step unmount 15 $B unmount $R/mnt
for i in $(seq 20); do kill -0 $MP 2>/dev/null || break; sleep .5; done
if kill -0 $MP 2>/dev/null; then echo "  daemon-exit: FAIL (still running 10s after unmount)"; F=1; kill -9 $MP; else echo "  daemon-exit: ok"; fi
WL=$(grep -m1 '^Workers:' $R/mount.log)
echo "  workers-line: ${WL:-none}"
if [ -n "${SLICEFS_WORKERS:-}" ] && [ "$WL" != "Workers: $SLICEFS_WORKERS (set)" ]; then echo "  workers-line: FAIL"; F=1; fi
PL=$(grep -m1 -o 'pool: workers=.*' $R/mount.log)
echo "  pool-line: ${PL:-none}"
if [ "${SLICEFS_WORKERS:-0}" -ge 2 ] 2>/dev/null && ! grep -qE 'pool: workers=[0-9]+ jobs=[1-9]' $R/mount.log; then echo "  pool-line: FAIL"; F=1; fi
AL=$(grep -m1 '^Allocator:' $R/mount.log)
echo "  allocator-line: ${AL:-none}"
if [ "$AL" != "Allocator: mmap threshold 131072 bytes (fixed)" ]; then echo "  allocator-line: FAIL (glibc daemon start)"; F=1; fi
SL=$(grep -m1 -o 'steps: save_steps=.*' $R/mount.log)
echo "  steps-line: ${SL:-none}"
$B stats $R/store 2>/dev/null | grep -iE "dedup|CAS" | head -3
exit $F
