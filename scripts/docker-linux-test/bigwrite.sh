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
  echo "  NOT MOUNTED: $m"; tail -5 $R/mount.log; F=1
  kill -9 $MP 2>/dev/null; wait $MP 2>/dev/null; fusermount -uz $m 2>/dev/null; return 1; }
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
# Each write and fsync of the target is logged (#108): start time (epoch s),
# latency (ms), kind (W/F), size, offset -> $R/writes-<size>.log.
# BW_QUIET_REF=1: the reference file is synced before the pass and gets the
# same writes in a second pass after it, so that no dirty pages of the
# reference are on the disk while the target is timed (its fsyncs and the
# daemon's pack syncs then wait only for SliceFS's own data).
cat > $R/rw.py <<'PY'
import os, random, sys, time
KB, MB = 4096, 1 << 20
tgt = os.open(sys.argv[1], os.O_RDWR)
log = open(sys.argv[5], "w", buffering=1 << 20)
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
quiet = len(sys.argv) > 6 and sys.argv[6] == "1"
def one(n):
    global written, writes, since_sync, max_w
    off = rnd.randrange((size - n) // KB + 1) * KB
    data = rnd.randbytes(n)
    w = time.time(); t = time.perf_counter(); put(tgt, data, off); d = time.perf_counter() - t
    max_w = max(max_w, d); log.write(f"{w:.3f} {d * 1000:.1f} W {n} {off}\n")
    if not quiet:
        put(ref, data, off)
    written += n; writes += 1; since_sync += n
def sync():
    global since_sync, max_s
    w = time.time(); t = time.perf_counter(); os.fsync(tgt); d = time.perf_counter() - t
    max_s = max(max_s, d); log.write(f"{w:.3f} {d * 1000:.1f} F 0 0\n")
    since_sync = 0
while written < vol:
    one(MB)
    for _ in range(256):
        one(KB)
    if since_sync >= 64 * MB:
        sync()
sync()
if quiet:  # the same writes into the reference, after the timed pass
    rnd = random.Random(int(sys.argv[3])); done = 0
    while done < vol:
        for n in [MB] + [KB] * 256:
            off = rnd.randrange((size - n) // KB + 1) * KB
            put(ref, rnd.randbytes(n), off); done += n
os.close(tgt); os.close(ref); log.close()
print(f"writes {writes}, max write {max_w * 1000:.0f} ms, max fsync {max_s * 1000:.0f} ms")
PY

# Match the slowest writes of rw.py to the daemon probe lines ([t=epoch]...)
# in mount.log that fall inside each write: rebase steps, timed commits,
# pack syncs and the daemon's own time for the write (#108).
cat > $R/match.py <<'PY'
import re, sys
writes = [l.split() for l in open(sys.argv[1])]
writes = [(float(a), float(b), k, int(n), int(o)) for a, b, k, n, o in writes]
ev = []
for line in open(sys.argv[2], errors="replace"):
    m = re.match(r"(?:\[[^\]]*\])?\[t=([0-9.]+)\]\[([a-z-]+)\] ?(.*)", line.strip())
    if m:
        ev.append((float(m.group(1)), m.group(2), m.group(3)))
ev.sort()
t0, t1 = writes[0][0], writes[-1][0] + writes[-1][1] / 1000
inrw = [e for e in ev if t0 <= e[0] <= t1]
def num(e, k):
    m = re.search(k + r"=([0-9.]+)", e[2]); return float(m.group(1)) if m else 0.0
reb = [e for e in inrw if e[1] == "rebase"]
syn = [e for e in inrw if e[1] == "pack"]
com = [e for e in inrw if e[1] == "commit-timer"]
w = [x for x in writes if x[2] == "W"]
print(f"    rw: {len(w)} writes, sum {sum(x[1] for x in w) / 1000:.1f} s; wall {t1 - t0:.1f} s")
print(f"    rebase steps: {len(reb)}, splice sum {sum(num(e, 'splice_ms') for e in reb) / 1000:.1f} s, "
      f"install sum {sum(num(e, 'install_ms') for e in reb) / 1000:.1f} s, rechunked {sum(num(e, 'rechunked') for e in reb) / 2**20:.0f} MiB, "
      f"reused_entries {sum(num(e, 'reused_entries') for e in reb):.0f}")
if reb:
    print(f"    step splice_ms max {max(num(e, 'splice_ms') for e in reb):.0f}, install_ms max {max(num(e, 'install_ms') for e in reb):.0f}")
    # Step time without the pack syncs inside it (a sync line comes before
    # the rebase line of its step).
    net, acc, k = [], 0.0, 0
    for e in inrw:
        if e[1] == "pack":
            acc += num(e, "ms")
        elif e[1] == "rebase":
            net.append(num(e, "splice_ms") - acc); acc = 0.0
    net.sort()
    print(f"    step minus syncs: median {net[len(net) // 2]:.0f} ms, p99 {net[int(len(net) * 0.99)]:.0f} ms, max {net[-1]:.0f} ms; "
          f"nodes_loaded mean {sum(num(e, 'nodes_loaded') for e in reb) / len(reb):.0f}")
print(f"    pack syncs: {len(syn)}, sum {sum(num(e, 'ms') for e in syn) / 1000:.1f} s, max {max([num(e, 'ms') for e in syn] or [0]):.0f} ms")
for e in com:
    print(f"    commit at {e[0]:.3f}: {e[2]}")
print("    slowest writes (start, ms, kind, size) and the daemon lines inside them:")
for x in sorted(writes, key=lambda x: -x[1])[:int(sys.argv[3])]:
    a, b = x[0] - 0.005, x[0] + x[1] / 1000 + 0.005
    print(f"    - {x[0]:.3f} {x[1]:.0f} ms {x[2]} {x[3]} @ {x[4]}")
    inside = [e for e in ev if a <= e[0] <= b]
    for e in inside[:12]:
        print(f"        {e[0]:.3f} [{e[1]}] {e[2][:200]}")
    if len(inside) > 12:
        print(f"        ... {len(inside) - 12} more")
PY

T0=$SECONDS
for S in "${SIZES[@]}"; do
  echo "== bigwrite $S MiB: cp, $VOL_MIB MiB of random 1 MiB and 4 KiB writes, cmp, remount, cmp"
  ST=$R/s$S; M=$R/m$S
  if ! $B seed $ST $R/empty >$R/step.out 2>&1; then bad "bw-seed-$S" "($(head -c 200 $R/step.out))"; continue; fi
  echo "    store_fs=$(stat -f -c %T $ST)"
  head -c $((S * 1048576)) /dev/urandom > $R/ref.bin
  [ -n "${BW_QUIET_REF:-}" ] && sync
  if mnt $ST $M; then   # default commit interval
    N0=$(nsw)
    t=$SECONDS; step bw-cp-$S 900 cp $R/ref.bin $M/big.bin; T1=$((SECONDS - t)); H1=$(hwm)
    t=$SECONDS; step bw-random-write-$S 1800 python3 $R/rw.py $M/big.bin $R/ref.bin $SEED $VOL_MIB $R/writes-$S.log "${BW_QUIET_REF:-}"
    T2=$((SECONDS - t)); H2=$(hwm); RW=$(tail -1 $R/step.out)
    t=$SECONDS; step bw-cmp-$S 600 cmp $R/ref.bin $M/big.bin; T3=$((SECONDS - t)); H3=$(hwm)
    N=$(( $(nsw) - N0 ))
    if [ -n "$CAP_MIB" ]; then
      if [ -n "$H3" ] && [ "$H3" -le "$CAP_MIB" ]; then ok bw-rss-bounded-$S
      else bad bw-rss-bounded-$S "(VmHWM $H3 MiB > cap $CAP_MIB MiB)"; fi
      if [ "$N" -ge 1 ]; then ok bw-overlay-used-$S; else bad bw-overlay-used-$S "(no '$SWITCH_TEXT' line)"; fi
    fi
    umnt $M
    python3 $R/match.py $R/writes-$S.log $R/mount.log 8 || echo "    (no write match)"
    # Keep the logs on the target volume when it is writable (for a closer look).
    if [ -n "${BW_KEEP:-}" ] && mkdir -p "$BW_KEEP/$S" 2>/dev/null; then
      cp $R/writes-$S.log $R/mount.log "$BW_KEEP/$S/" 2>/dev/null; fi
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
