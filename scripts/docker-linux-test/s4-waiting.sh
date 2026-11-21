#!/bin/bash
# Spike S4: kernel queue depth of the FUSE connection while one file gets random
# writes, readahead, and both. The depth is the `waiting` file of the connection
# in fusectl (/sys/fs/fuse/connections/<id>/waiting), sampled about every 2 ms.
# Limits: `waiting` counts the whole connection, not one inode, so only one file
# is active; SliceFS has one dispatch thread, so only the kernel-side queue is
# visible. A sanity step first stops the daemon and checks that the counter moves.
# Runs inside the Docker container (run.sh s4). By hand only, not in CI.
# Usage: s4-waiting.sh <slicefs-binary>
B=$1
R=/tmp/s4; rm -rf $R; mkdir -p $R/empty; F=0
FUSECTL=/sys/fs/fuse/connections
DUR=60              # seconds per pass
FILE_MIB=256
ok(){ echo "  $1: ok"; }; bad(){ echo "  $1: FAIL ${2:-}"; F=1; }
step(){ local n=$1 t=$2; shift 2
  if timeout -s KILL $t "$@" >$R/step.out 2>&1; then ok "$n"; else bad "$n" "($(head -c 200 $R/step.out))"; fi; }
mnt(){ # mnt <store> <mountpoint>: per-op write log, GC off
  local s=$1 m=$2; shift 2; mkdir -p $m
  $B mount --store $s --wal-strategy per-op --gc-interval 0 "$@" $m </dev/null >>$R/mount.log 2>&1 & MP=$!
  for i in $(seq 1 20); do
    kill -0 $MP 2>/dev/null || break
    mountpoint -q $m && return 0; sleep 0.5; done
  echo "  NOT MOUNTED: $m"; tail -5 $R/mount.log; F=1
  kill -9 $MP 2>/dev/null; wait $MP 2>/dev/null; fusermount -uz $m 2>/dev/null; return 1; }
umnt(){ # umnt <mountpoint>: unmount and require the daemon to exit
  timeout -s KILL 15 $B unmount $1 >/dev/null 2>&1 || fusermount -u $1 2>/dev/null
  for i in $(seq 20); do kill -0 $MP 2>/dev/null || return 0; sleep 0.5; done
  bad "daemon-exit $1" "(wchan=$(cat /proc/$MP/wchan 2>/dev/null); after 10s; $(tail -2 $R/mount.log | tr '\n' ' '))"
  for i in $(seq 120); do kill -0 $MP 2>/dev/null || { echo "    daemon exited $((10 + i/2))s after unmount"; return; }; sleep 0.5; done
  kill -9 $MP; }

# Sampler: reads the waiting file about every 2 ms until the stop file appears
# or the deadline (seconds) passes. Prints one S4 line.
cat > $R/sample.py <<'PY'
import collections, sys, time
path, stop, deadline, name = sys.argv[1], sys.argv[2], float(sys.argv[3]), sys.argv[4]
import os
vals = []
end = time.monotonic() + deadline
while time.monotonic() < end and not os.path.exists(stop):
    with open(path) as f:
        vals.append(int(f.read().strip() or 0))
    time.sleep(0.002)
vals_sorted = sorted(vals)
def pct(q): return vals_sorted[min(len(vals_sorted) - 1, int(len(vals_sorted) * q))] if vals_sorted else 0
hist = collections.Counter(vals)
h = ",".join(f"{k}:{hist[k]}" for k in sorted(hist))
print(f"S4 pass={name} samples={len(vals)} waiting_max={max(vals) if vals else 0} waiting_p50={pct(0.5)} waiting_p99={pct(0.99)} hist={h}")
PY

# Writer: seeded random pwrite of 1 MiB and 4 KiB (one 1 MiB then 64 of 4 KiB per
# round) into the one file for DUR seconds; fsync every 64 MiB written.
cat > $R/wr.py <<'PY'
import os, random, sys, time
KB, MB = 4096, 1 << 20
fd = os.open(sys.argv[1], os.O_RDWR)
size = os.fstat(fd).st_size
rnd = random.Random(98)
end = time.monotonic() + float(sys.argv[2])
since = 0
while time.monotonic() < end:
    for n in [MB] + [KB] * 64:
        off = rnd.randrange((size - n) // KB + 1) * KB
        data = rnd.randbytes(n); done = 0
        while done < n:
            done += os.pwrite(fd, data[done:], off + done)
        since += n
    if since >= 64 * MB:
        os.fsync(fd); since = 0
os.fsync(fd); os.close(fd)
PY

# Reader: dd of the whole file in a loop for DUR seconds. The page cache of the
# file is dropped (fadvise DONTNEED, no privilege needed) before each dd so that
# the reads reach the daemon and the kernel readahead keeps working.
cat > $R/rd.sh <<'SH'
f=$1; dur=$2; end=$((SECONDS + dur))
while [ $SECONDS -lt $end ]; do
  python3 -c "import os,sys; fd=os.open(sys.argv[1],os.O_RDONLY); os.posix_fadvise(fd,0,0,os.POSIX_FADV_DONTNEED)" $f
  timeout -s KILL 60 dd if=$f of=/dev/null bs=1M 2>/dev/null
done
SH

SHA=$(git -c safe.directory=/src -C /src rev-parse --short HEAD 2>/dev/null || echo unknown)
echo "# S4 queue depth, commit $SHA, kernel $(uname -r), file ${FILE_MIB} MiB, ${DUR} s per pass"
echo "# note: waiting counts the whole FUSE connection; one active file only; one daemon dispatch thread"

# fusectl: mount it if needed; fail clearly if the directory stays empty.
mountpoint -q $FUSECTL 2>/dev/null || mount -t fusectl none $FUSECTL 2>$R/fusectl.err
T0=$SECONDS
ST=$R/store; M=$R/m
timeout -s KILL 60 $B seed $ST $R/empty >$R/step.out 2>&1 || { bad s4-seed "($(head -c 200 $R/step.out))"; echo "RESULT s4: FAIL"; exit 1; }
mnt $ST $M || { echo "RESULT s4: FAIL"; exit 1; }
if [ -z "$(ls $FUSECTL 2>/dev/null)" ]; then
  echo "S4 fusectl unavailable ($(head -c 200 $R/fusectl.err))"
  umnt $M; echo "RESULT s4: FAIL"; exit 2
fi
step s4-create-file 600 sh -c "head -c $((FILE_MIB * 1048576)) /dev/urandom > $M/big.bin"
sync
DEV=$(stat -c %d $M)
if [ -d "$FUSECTL/$DEV" ]; then CONN=$FUSECTL/$DEV
elif [ "$(ls $FUSECTL | wc -l)" -eq 1 ]; then CONN=$FUSECTL/$(ls $FUSECTL)
else echo "S4 connection not found (dev=$DEV, entries: $(ls $FUSECTL | tr '\n' ' '))"; umnt $M; echo "RESULT s4: FAIL"; exit 1; fi
echo "S4 connection=$(basename $CONN)"
W=$CONN/waiting

# Sanity: the daemon is stopped, a stat of a new name must queue a request.
kill -STOP $MP
timeout -s KILL 10 stat $M/no-such-name-s4 >/dev/null 2>&1 & SP=$!
SMAX=0
for i in $(seq 50); do v=$(cat $W); [ "$v" -gt "$SMAX" ] && SMAX=$v; sleep 0.02; done
kill -CONT $MP
for i in $(seq 100); do kill -0 $SP 2>/dev/null || break; sleep 0.1; done
kill -9 $SP 2>/dev/null; wait $SP 2>/dev/null
echo "S4 sanity waiting_while_stopped=$SMAX"
if [ "$SMAX" -lt 1 ]; then echo "S4 sanity FAIL"; umnt $M; echo "RESULT s4: FAIL"; exit 1; fi

pass(){ # pass <name> <writer:0|1> <reader:0|1>
  local name=$1 w=$2 r=$3 wp= rp=
  rm -f $R/stop
  python3 $R/sample.py $W $R/stop 180 $name >$R/sample.out 2>&1 & local sp=$!
  [ "$w" = 1 ] && { timeout -s KILL 120 python3 $R/wr.py $M/big.bin $DUR >$R/wr.out 2>&1 & wp=$!; }
  [ "$r" = 1 ] && { timeout -s KILL 150 bash $R/rd.sh $M/big.bin $DUR >$R/rd.out 2>&1 & rp=$!; }
  [ -n "$wp" ] && { wait $wp || bad "s4-writer-$name" "($(head -c 200 $R/wr.out))"; }
  [ -n "$rp" ] && { wait $rp || bad "s4-reader-$name"; }
  touch $R/stop; wait $sp
  cat $R/sample.out
}
pass write 1 0
pass readahead 0 1
pass both 1 1
umnt $M
echo "    run time: $((SECONDS - T0)) s"
echo "RESULT s4: $([ $F -eq 0 ] && echo PASS || echo FAIL)"; exit $F
