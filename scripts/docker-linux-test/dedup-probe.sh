#!/bin/bash
# Write the same 3 MB file twice across separate mounts; show that the second copy
# adds ~no CAS bytes and that both files survive remount. Exits non-zero on mismatch.
B=$1; R=/tmp/probe; rm -rf $R; mkdir -p $R/src $R/mnt; echo s > $R/src/s.txt
$B seed $R/store $R/src >/dev/null 2>&1
head -c 3000000 /dev/urandom > $R/r.bin
mnt(){ $B mount --store $R/store $R/mnt </dev/null >> $R/mount.log 2>&1 & MP=$!; for i in $(seq 20); do mountpoint -q $R/mnt && break; sleep .5; done; }
umnt(){ $B unmount $R/mnt >/dev/null 2>&1; wait $MP 2>/dev/null; }
show(){ echo "== $1"; $B stats $R/store 2>&1 | sed -n 3,6p; du -sb $R/store/* | sed "s#$R/store/##"; }
show seeded
mnt; cp $R/r.bin $R/mnt/a.bin; umnt; show "after a.bin"
mnt; cp $R/r.bin $R/mnt/b.bin; umnt; show "after b.bin (same content)"
mnt; cmp $R/r.bin $R/mnt/a.bin && cmp $R/r.bin $R/mnt/b.bin && echo "content ok" || { echo "CONTENT MISMATCH"; umnt; exit 1; }; umnt
