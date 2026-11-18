#!/bin/bash
# Random writes on a big file: daemon peak memory (VmHWM) on Linux libfuse (#98).
# For each size: copy a urandom file in, then one python3 process writes 512 MiB
# of seeded random 1 MiB and 4 KiB blocks into it (and into a reference copy),
# then cmp, then cmp again after a remount.
# Runs inside the Docker container (run.sh bigwrite). By hand only, not in CI.
# Usage: bigwrite.sh <slicefs-binary> [size-MiB ...]   (default: 1024 4096)
B=$1; shift; SIZES=("$@"); [ ${#SIZES[@]} -gt 0 ] || SIZES=(1024 4096)
# Cap rule (plan 14-07): P1 = VmHWM after cmp of a measure-only 1024 MiB run;
# P1 <= 170 MiB gives 256 MiB. Measured 2026-11-18, release build, DIRTY_LIMIT
# 32 MiB: P1 117 MiB (after cp 30, after random write 82). The same cap holds
# for 1 GiB and 4 GiB (#98). Empty: measure only. Set: fail a size above the
# cap or with no overlay switch.
CAP_MIB=256
VOL_MIB=512         # random-write volume per size
SEED=98
SWITCH_TEXT="switches to overlay"
PRE=(); R=/tmp/bw; rm -rf $R; mkdir -p $R/empty; F=0
ok(){ echo "  $1: ok"; }; bad(){ echo "  $1: FAIL ${2:-}"; F=1; }
step(){ local n=$1 t=$2; shift 2
  if timeout -s KILL $t "$@" >$R/step.out 2>&1; then ok "$n"; else bad "$n" "($(head -c 200 $R/step.out))"; fi; }
mnt(){ # mnt <store> <mountpoint> [extra args...]: per-op write log, GC only on request
  local s=$1 m=$2; shift 2; mkdir -p $m
  "${PRE[@]}" $B mount --store $s --wal-strategy per-op --gc-interval 0 "$@" $m </dev/null >>$R/mount.log 2>&1 & MP=$!
  for i in $(seq 1 20); do mountpoint -q $m && return 0; sleep 0.5; done
  echo "  NOT MOUNTED: $m"; tail -5 $R/mount.log; F=1; return 1; }
umnt(){ # umnt <mountpoint>: unmount and require the daemon to exit
  timeout -s KILL 15 $B unmount $1 >/dev/null 2>&1 || fusermount -u $1 2>/dev/null
  for i in $(seq 20); do kill -0 $MP 2>/dev/null || return 0; sleep 0.5; done
  bad "daemon-exit $1" "(wchan=$(cat /proc/$MP/wchan 2>/dev/null); after 10s; $(tail -2 $R/mount.log | tr '\n' ' '))"
  for i in $(seq 120); do kill -0 $MP 2>/dev/null || { echo "    daemon exited $((10 + i/2))s after unmount"; return; }; sleep 0.5; done
  kill -9 $MP; }
cas(){ $B stats $1 2>/dev/null | awk -F': ' '/CAS bytes/{print $2}' | grep -oE '[0-9]+ bytes' | head -1 | cut -d' ' -f1; }
hwm(){ awk '/^VmHWM:/{print int($2/1024)}' /proc/$MP/status 2>/dev/null; }   # daemon peak RSS, MiB
nsw(){ grep -c "$SWITCH_TEXT" $R/mount.log; }

# Random writer: the same seeded writes go to the mounted file and to the
# reference file. One handle per file for the whole pass, opened O_RDWR with
# no truncate. Each round is one 1 MiB write and 256 writes of 4 KiB, at random
# 4 KiB-aligned offsets inside the file. fsync of the target every 64 MiB.
cat > $R/rw.py <<'PY'
import os, random, sys, time
KB, MB = 4096, 1 << 20
tgt = os.open(sys.argv[1], os.O_RDWR)
ref = os.open(sys.argv[2], os.O_RDWR)
rnd = random.Random(int(sys.argv[3]))
vol = int(sys.argv[4]) * MB
size = os.fstat(ref).st_size
def put(fd, data, off):
    done = 0
    while done < len(data):
        done += os.pwrite(fd, data[done:], off + done)
written = writes = since_sync = 0
max_w = max_s = 0.0
T0 = time.perf_counter()
slow = []  # PROBE: (dur, idx, wall, off, n, written_before)
fsyncs = []
def wall():
    t = time.time(); s = int(t)
    return "%02d:%02d:%02d.%03d" % ((s // 3600) % 24, (s // 60) % 60, s % 60, int((t - s) * 1000))
def one(n):
    global written, writes, since_sync, max_w
    off = rnd.randrange((size - n) // KB + 1) * KB
    data = rnd.randbytes(n)
    w0 = wall()
    t = time.perf_counter(); put(tgt, data, off); d = time.perf_counter() - t; max_w = max(max_w, d)
    if d > 0.05:
        slow.append((d, writes, w0, off, n, written))
    t = time.perf_counter(); put(ref, data, off); dr = time.perf_counter() - t
    if dr > 0.05:
        print(f"PROBE ref write slow {dr*1000:.0f} ms at {w0}", file=sys.stderr)
    written += n; writes += 1; since_sync += n
def sync():
    global since_sync, max_s
    w0 = wall()
    t = time.perf_counter(); os.fsync(tgt); d = time.perf_counter() - t; max_s = max(max_s, d)
    fsyncs.append((d, w0, written))
    since_sync = 0
while written < vol:
    one(MB)
    for _ in range(256):
        one(KB)
    if since_sync >= 64 * MB:
        sync()
sync()
os.close(tgt); os.close(ref)
with open(os.environ.get("RW_DETAIL", "/tmp/bw/rw-detail.txt"), "w") as f:
    f.write(f"total {time.perf_counter() - T0:.1f} s; writes > 50 ms: {len(slow)}, sum {sum(x[0] for x in slow):.1f} s\n")
    for d, i, w0, off, n, wb in sorted(slow, reverse=True)[:40]:
        f.write(f"  slow write {d*1000:.0f} ms idx={i} start={w0} off={off} len={n} written_before={wb >> 20} MiB\n")
    for d, i, w0, off, n, wb in sorted(slow, key=lambda x: x[1]):
        f.write(f"  chrono write {d*1000:.0f} ms idx={i} start={w0} len={n} written_before={wb >> 20} MiB\n")
    for d, w0, wb in fsyncs:
        f.write(f"  fsync {d*1000:.0f} ms start={w0} written={wb >> 20} MiB\n")
print(f"writes {writes}, max write {max_w * 1000:.0f} ms, max fsync {max_s * 1000:.0f} ms")
PY

T0=$SECONDS
for S in "${SIZES[@]}"; do
  echo "== bigwrite $S MiB: cp, $VOL_MIB MiB of random 1 MiB and 4 KiB writes, cmp, remount, cmp"
  ST=$R/s$S; M=$R/m$S
  if ! $B seed $ST $R/empty >$R/step.out 2>&1; then bad "bw-seed-$S" "($(head -c 200 $R/step.out))"; continue; fi
  head -c $((S * 1048576)) /dev/urandom > $R/ref.bin
  if mnt $ST $M; then   # default commit interval
    N0=$(nsw)
    t=$SECONDS; step bw-cp-$S 900 cp $R/ref.bin $M/big.bin; T1=$((SECONDS - t)); H1=$(hwm)
    ( while :; do echo "$(date -u +%H:%M:%S.%3N) $(awk '/^(Dirty|Writeback):/{printf "%s=%dMiB ", $1, $2/1024}' /proc/meminfo) $(tr '\n' ' ' </proc/pressure/io 2>/dev/null | cut -c1-60)"; sleep 0.5; done ) > $R/mem.log 2>&1 & SAMP=$!
    t=$SECONDS; step bw-random-write-$S 1800 python3 $R/rw.py $M/big.bin $R/ref.bin $SEED $VOL_MIB
    T2=$((SECONDS - t)); H2=$(hwm); RW=$(tail -1 $R/step.out)
    kill $SAMP 2>/dev/null
    echo "---- PROBE rw-detail $S"; cat $R/rw-detail.txt; grep PROBE $R/step.out
    echo "---- PROBE daemon $S"; grep -E 'PROBE|commit-timer|switches to overlay' $R/mount.log
    echo "---- PROBE meminfo $S"; cat $R/mem.log
    echo "---- PROBE end $S"
    t=$SECONDS; step bw-cmp-$S 600 cmp $R/ref.bin $M/big.bin; T3=$((SECONDS - t)); H3=$(hwm)
    N=$(( $(nsw) - N0 ))
    if [ -n "$CAP_MIB" ]; then
      if [ -n "$H3" ] && [ "$H3" -le "$CAP_MIB" ]; then ok bw-rss-bounded-$S
      else bad bw-rss-bounded-$S "(VmHWM $H3 MiB > cap $CAP_MIB MiB)"; fi
      if [ "$N" -ge 1 ]; then ok bw-overlay-used-$S; else bad bw-overlay-used-$S "(no '$SWITCH_TEXT' line)"; fi
    fi
    umnt $M
    echo "    bigwrite $S MiB: VmHWM after cp $H1, after random write $H2, after cmp $H3 MiB ($([ -n "$CAP_MIB" ] && echo "cap $CAP_MIB MiB" || echo "measure only")); $RW; overlay switches $N; CAS bytes $(cas $ST); times cp/rw/cmp $T1/$T2/$T3 s"
    if mnt $ST $M; then
      step bw-cmp-after-remount-$S 600 cmp $R/ref.bin $M/big.bin
      umnt $M
    fi
  fi
  rm -rf $ST $M $R/ref.bin
done
echo "    run time: $((SECONDS - T0)) s"

echo "RESULT bigwrite: $([ $F -eq 0 ] && echo PASS || echo FAIL)"; exit $F
