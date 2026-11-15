#!/bin/bash
# Copy a file into a SliceFS mount and back out; print MB/s both ways.
# Usage: read-bench.sh <slicefs-binary> [MB]   (inside the Docker container)
B=$1; MB=${2:-100}; R=/tmp/rb; rm -rf $R; mkdir -p $R/src $R/m
echo s > $R/src/s.txt; $B seed $R/store $R/src >/dev/null
head -c ${MB}M /dev/urandom > $R/in.bin
$B mount --store $R/store $R/m </dev/null >/dev/null 2>&1 & MP=$!
for i in $(seq 20); do mountpoint -q $R/m && break; sleep .5; done
t0=$(date +%s.%N); cp $R/in.bin $R/m/f.bin; t1=$(date +%s.%N)
# Remount so the read goes through SliceFS, not the kernel page cache.
$B unmount $R/m >/dev/null 2>&1; for i in $(seq 40); do kill -0 $MP 2>/dev/null || break; sleep .5; done
$B mount --store $R/store $R/m </dev/null >/dev/null 2>&1 & MP=$!
for i in $(seq 20); do mountpoint -q $R/m && break; sleep .5; done
t2=$(date +%s.%N); timeout -s KILL 900 cp $R/m/f.bin $R/out.bin; rc=$?; t3=$(date +%s.%N)
cmp -s $R/in.bin $R/out.bin && ok=ok || ok=MISMATCH
python3 -c "import sys; mb,a,b,c,d=float(sys.argv[1]),*map(float,sys.argv[2:6]); print(f'write {mb/(b-a):.1f} MB/s ({b-a:.1f}s)  read {mb/(d-c):.1f} MB/s ({d-c:.1f}s)')" $MB $t0 $t1 $t2 $t3
echo "read rc=$rc content=$ok"
$B unmount $R/m >/dev/null 2>&1
