#!/bin/bash
# Spike S6: many small fsynced files, per WAL strategy, on a Linux FUSE mount.
# For each strategy (per-op, flush-on-fsync, periodic) and each of two passes
# (interleaved: pass 1 of all three, then pass 2 of all three): fresh store,
# mount with that strategy, then one python3 process creates N files, and for
# each file does write(4096), a timed fsync, close. One result line per
# strategy and pass.
# There is no WAL flusher yet: PerOp is compared with the buffered strategies.
# Runs inside the Docker container (run.sh s6). By hand only, not in CI.
# Usage: s6-smallfsync.sh <slicefs-binary> [files]   (default: 2000)
B=$1; N=${2:-2000}
STRATS=(per-op flush-on-fsync periodic)
R=/tmp/s6; rm -rf $R; mkdir -p $R/empty; F=0
ok(){ echo "  $1: ok"; }; bad(){ echo "  $1: FAIL ${2:-}"; F=1; }
step(){ local n=$1 t=$2; shift 2
  if timeout -s KILL $t "$@" >$R/step.out 2>&1; then ok "$n"; else bad "$n" "($(head -c 200 $R/step.out))"; fi; }
mnt(){ # mnt <store> <mountpoint> <strategy>: GC off
  local s=$1 m=$2 w=$3; mkdir -p $m
  $B mount --store $s --wal-strategy $w --gc-interval 0 $m </dev/null >>$R/mount.log 2>&1 & MP=$!
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

cat > $R/s6.py <<'PY'
import os, sys, time
mp, n, strat, p = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
data = os.urandom(4096)
fs, ops = [], []
t_all = time.perf_counter()
for i in range(n):
    t0 = time.perf_counter()
    fd = os.open(f"{mp}/f{i:06d}", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
    os.write(fd, data)
    t1 = time.perf_counter()
    os.fsync(fd)
    t2 = time.perf_counter()
    os.close(fd)
    t3 = time.perf_counter()
    fs.append((t2 - t1) * 1000); ops.append((t3 - t0) * 1000)
wall = time.perf_counter() - t_all
def pct(v, q):
    v = sorted(v); return v[min(len(v) - 1, int(len(v) * q))]
print(f"S6 strategy={strat} pass={p} files={n} "
      f"fsyncs_per_s_fsync_only={n / sum(fs) * 1000:.1f} files_per_s_wall={n / wall:.1f} "
      f"fsync_p50_ms={pct(fs, 0.50):.2f} fsync_p99_ms={pct(fs, 0.99):.2f} fsync_max_ms={max(fs):.2f} "
      f"op_p99_ms={pct(ops, 0.99):.2f} wall_s={wall:.1f}")
PY

SHA=$(git -c safe.directory=/src -C /src rev-parse --short HEAD 2>/dev/null || echo unknown)
echo "# S6 small files, commit $SHA, kernel $(uname -r), store_fs=$(stat -f -c %T $R)"
echo "# files per run $N, 4096 bytes each, write then timed fsync then close; GC off"
echo "# note: no WAL flusher yet: PerOp is compared with the buffered strategies"
T0=$SECONDS
for P in 1 2; do
  for S in "${STRATS[@]}"; do
    ST=$R/store-$S-$P; M=$R/m-$S-$P
    if ! timeout -s KILL 60 $B seed $ST $R/empty >$R/step.out 2>&1; then bad "s6-seed-$S-$P" "($(head -c 200 $R/step.out))"; continue; fi
    if mnt $ST $M $S; then
      step s6-run-$S-$P 900 python3 $R/s6.py $M $N $S $P
      grep '^S6 strategy=' $R/step.out || cat $R/step.out
      umnt $M
    fi
    rm -rf $ST $M
  done
done
echo "    run time: $((SECONDS - T0)) s"
echo "RESULT s6: $([ $F -eq 0 ] && echo PASS || echo FAIL)"; exit $F
