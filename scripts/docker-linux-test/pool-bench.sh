#!/bin/bash
# Pool bench (POOL-05, Phase 21): C clients on C different files at several
# --workers values. Inside the Docker container, by hand (run.sh pool-bench).
# Usage: pool-bench.sh <slicefs-binary>
#
# One run (N workers, round r, trace on|off, arena default|2) has 4 mounts of a
# fresh store, each with --workers N:
#   A: write phase, C parallel dd with conv=fsync.
#   B: read phase (cold cache), C parallel dd to /dev/null, cmp of every file
#      with its source; then the random-write phase: C processes each write
#      FILE_MIB x 1 MiB at random 1 MiB-aligned offsets of their own file
#      (fixed seed per client), fsync, close.
#   M: metadata phase, its own mount so a lock-stats build gives lock rows of
#      this phase only: C processes, each in its own directory, create 250
#      files of 4 KiB, then stat each, read each (content check), unlink each;
#      ops/s per step.
#   V: check of the random-write content (cold cache), C parallel readers.
# The daemon VmHWM is read from /proc before each unmount; the daemon must exit
# within 30 s of the unmount. Any content mismatch or failure stops the script
# at once (exit 1) and keeps the store under $POOL_BENCH_DIR.
#
# Environment:
#   POOL_BENCH_QUICK=1    C = 4, FILE_MIB = 8, worker counts 1 and 2, one round,
#                         no extra runs (a short check of the script itself).
#   POOL_BENCH_N=<n>      only n workers, POOL_BENCH_ROUNDS (default 1) runs with
#                         the trace lines off (the lock-stats runs use this).
#   POOL_BENCH_EXTRA=1    with POOL_BENCH_N: also one trace-on run and one
#                         MALLOC_ARENA_MAX=2 run at n (a check of those paths).
#   POOL_BENCH_DIR        work directory (default /tmp/pb).
# The full set (no POOL_BENCH_QUICK, no POOL_BENCH_N): rounds 1 to 3 with N in
# the orders 1 2 4 8 16 / 16 8 4 2 1 / 4 16 1 8 2, trace lines off (the
# default); after each round one trace-on run (SLICEFS_FUSE_TRACE=1) at 1 and
# at 4 workers; at the end one run at 16 with MALLOC_ARENA_MAX=2.
set -u
B=$1
R=${POOL_BENCH_DIR:-/tmp/pb}; MNT=/tmp/pb-mnt
C=16; FILE_MIB=64; FILES=250
[ "${POOL_BENCH_QUICK:-0}" = 1 ] && { C=4; FILE_MIB=8; }
STEP_T=900   # time limit of one client process of a phase (s)

echo "nproc $(nproc)"
echo "uname -r $(uname -r)"
echo "loadavg $(cat /proc/loadavg)"
echo "clients=$C file_mib=$FILE_MIB files_per_client=$FILES"

mkdir -p $R $MNT
rm -rf $R/src $R/store $R/seed $R/*.log
mkdir -p $R/src $R/seed; echo s > $R/seed/s.txt
for i in $(seq $C); do
  timeout -s KILL 300 head -c ${FILE_MIB}M /dev/urandom > $R/src/s$i || { echo "FAIL: source file $i"; exit 1; }
done

# Client programs (1 MiB buffers at most; no file is held whole in memory).
cat > $R/pb.py <<'PY'
import os, random, sys
MIB = 1 << 20

def small(c, j):
    return random.Random(c * 100003 + j).randbytes(4096)

def pread_full(fd, n, off):
    out = b""
    while len(out) < n:
        d = os.pread(fd, n - len(out), off + len(out))
        if not d:
            break
        out += d
    return out

cmd = sys.argv[1]
if cmd in ("create", "stat", "read", "unlink"):
    d, c, k = sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
    bad = 0
    if cmd == "create":
        os.mkdir(d)
    for j in range(k):
        p = f"{d}/s{j}"
        if cmd == "create":
            with open(p, "wb") as f:
                f.write(small(c, j))
        elif cmd == "stat":
            bad += os.stat(p).st_size != 4096
        elif cmd == "read":
            with open(p, "rb") as f:
                bad += f.read() != small(c, j)
        else:
            os.unlink(p)
    if cmd == "unlink":
        os.rmdir(d)
    if bad:
        print(f"meta-mismatch step={cmd} client={c} files={bad}")
        sys.exit(3)
elif cmd == "rw":  # rw <file> <client> <mib>: random 1 MiB writes, fsync, close
    f, c, n = sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
    rng = random.Random(c)
    fd = os.open(f, os.O_WRONLY)
    for _ in range(n):
        off = rng.randrange(n) * MIB
        data = rng.randbytes(MIB)
        done = 0
        while done < MIB:
            done += os.pwrite(fd, data[done:], off + done)
    os.fsync(fd)
    os.close(fd)
elif cmd == "rv":  # rv <file> <source> <client> <mib>: check the rw content
    f, s, c, n = sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5])
    rng = random.Random(c)
    last = {}
    for k in range(n):
        off = rng.randrange(n) * MIB
        rng.randbytes(MIB)
        last[off] = k
    fd = os.open(f, os.O_RDONLY)
    sd = os.open(s, os.O_RDONLY)
    bad = int(os.fstat(fd).st_size != n * MIB)
    rng = random.Random(c)
    for k in range(n):
        off = rng.randrange(n) * MIB
        data = rng.randbytes(MIB)
        if last[off] == k and pread_full(fd, MIB, off) != data:
            bad += 1
    for b in range(n):
        if b * MIB not in last and pread_full(fd, MIB, b * MIB) != pread_full(sd, MIB, b * MIB):
            bad += 1
    if bad:
        print(f"rand-mismatch client={c} blocks={bad}")
        sys.exit(3)
PY

N=0; TRACE=off; RUN=""; ENVV=(); MP=0; LOG=""; HWM=0; SECS=0; PBAD=0
MAXW=na; MAXQ=na

fail() { # fail <text>: print, keep the store, stop
  echo "FAIL $RUN: $1"
  [ -n "$LOG" ] && [ -f "$LOG" ] && tail -5 "$LOG"
  echo "store kept at $R/store"
  exit 1
}

mnt() { # mnt <tag>: mount the store with --workers N; MP is the daemon pid
  LOG=$R/mount-$1.log
  "${ENVV[@]}" $B mount --store $R/store --workers $N $MNT </dev/null >$LOG 2>&1 &
  MP=$!
  for i in $(seq 60); do mountpoint -q $MNT && break; kill -0 $MP 2>/dev/null || break; sleep 0.5; done
  mountpoint -q $MNT || fail "mount $1 did not come up"
  [ "$(cat /proc/$MP/comm 2>/dev/null)" = slicefs ] || fail "pid $MP is not the daemon ($(cat /proc/$MP/comm 2>/dev/null))"
  grep -q "^Workers: $N (set)$" $LOG || fail "mount $1: no line 'Workers: $N (set)'"
}

umnt() { # umnt <tag>: VmHWM (MiB) into HWM, unmount, the daemon must exit in 30 s
  HWM=$(awk '/^VmHWM:/{print int($2 / 1024)}' /proc/$MP/status 2>/dev/null)
  [ -n "$HWM" ] || fail "mount $1: no VmHWM (the daemon is gone before the unmount)"
  timeout -s KILL 60 $B unmount $MNT >/dev/null 2>&1 || fusermount -u $MNT 2>/dev/null
  for i in $(seq 60); do kill -0 $MP 2>/dev/null || break; sleep 0.5; done
  if kill -0 $MP 2>/dev/null; then kill -9 $MP; fail "mount $1: daemon still running 30 s after the unmount"; fi
  local pl w q
  pl=$(grep -m1 -o 'pool: workers=.*' $LOG)
  [ -n "$pl" ] && echo "detail $RUN mount=$1 $pl"
  grep -m1 -o 'steps: save_steps=.*' $LOG | sed "s/^/detail $RUN mount=$1 /"
  grep '^lock_row ' $LOG | sed "s/^/lock $RUN mount=$1 /"
  if [ -n "$pl" ]; then
    w=$(echo "$pl" | grep -o 'max_weight=[0-9]*' | cut -d= -f2)
    q=$(echo "$pl" | grep -o 'max_queue_wait_ms=[0-9]*' | cut -d= -f2)
    if [ "$MAXW" = na ] || [ "$w" -gt "$MAXW" ]; then MAXW=$w; fi
    if [ "$MAXQ" = na ] || [ "$q" -gt "$MAXQ" ]; then MAXQ=$q; fi
  fi
  # With the trace lines on: the requests of the metadata mount per kind
  # (do the stat and read steps reach the daemon, or the kernel caches?).
  if [ "$1" = m ] && [ "$TRACE" = on ]; then
    echo "detail $RUN mount=m requests: $(grep -o '\]\[FUSE\] [a-z_]*: ENTRY' $LOG | awk '{print $2}' | tr -d : | sort | uniq -c | awk '{printf "%s=%s ", $2, $1}')"
  fi
  rm -f $LOG; LOG=""
}

par() { # par <fn>: run <fn> i for every client i at once; wall time in SECS, failures in PBAD
  local fn=$1 pids=() p t0 t1 i
  PBAD=0
  t0=$(date +%s.%N)
  for i in $(seq $C); do $fn $i & pids+=($!); done
  for p in "${pids[@]}"; do wait $p || PBAD=$((PBAD + 1)); done
  t1=$(date +%s.%N)
  SECS=$(awk -v a=$t0 -v b=$t1 'BEGIN{printf "%.3f", b - a}')
}
rate() { awk -v x=$1 -v s=$SECS 'BEGIN{printf "%.1f", x / s}'; }

w_one()  { timeout -s KILL $STEP_T dd if=$R/src/s$1 of=$MNT/f$1 bs=1M conv=fsync status=none; }
r_one()  { timeout -s KILL $STEP_T dd if=$MNT/f$1 of=/dev/null bs=1M status=none; }
rw_one() { timeout -s KILL $STEP_T python3 $R/pb.py rw $MNT/f$1 $1 $FILE_MIB; }
rv_one() { timeout -s KILL $STEP_T python3 $R/pb.py rv $MNT/f$1 $R/src/s$1 $1 $FILE_MIB; }
mc_one() { timeout -s KILL $STEP_T python3 $R/pb.py create $MNT/d$1 $1 $FILES; }
ms_one() { timeout -s KILL $STEP_T python3 $R/pb.py stat $MNT/d$1 $1 $FILES; }
mr_one() { timeout -s KILL $STEP_T python3 $R/pb.py read $MNT/d$1 $1 $FILES; }
mu_one() { timeout -s KILL $STEP_T python3 $R/pb.py unlink $MNT/d$1 $1 $FILES; }

run_one() { # run_one <N> <round> <on|off> <default|2>
  N=$1; TRACE=$3; local r=$2 tr=$3 ar=$4 load mism=0 i wm rm mf ms mr mu rwm rvm ha hb hm hv
  RUN="n=$N round=$r trace=$tr arena=$ar"
  MAXW=na; MAXQ=na
  ENVV=(env -u SLICEFS_FUSE_TRACE -u MALLOC_ARENA_MAX)
  [ $tr = on ] && ENVV+=(SLICEFS_FUSE_TRACE=1)
  [ $ar = 2 ] && ENVV+=(MALLOC_ARENA_MAX=2)
  load=$(cut -d' ' -f1 /proc/loadavg)
  rm -rf $R/store
  $B seed $R/store $R/seed >/dev/null 2>&1 || fail "seed"

  mnt a
  par w_one; [ $PBAD = 0 ] || fail "write phase: $PBAD client(s) failed"
  wm=$(rate $((C * FILE_MIB)))
  umnt a; ha=$HWM

  mnt b
  par r_one; [ $PBAD = 0 ] || fail "read phase: $PBAD client(s) failed"
  rm=$(rate $((C * FILE_MIB)))
  for i in $(seq $C); do timeout -s KILL 300 cmp -s $R/src/s$i $MNT/f$i || mism=$((mism + 1)); done
  [ $mism = 0 ] || { echo "bench $RUN write_mbs=$wm read_mbs=$rm mismatches=$mism"; fail "content mismatch after the read phase"; }
  par rw_one; [ $PBAD = 0 ] || fail "random-write phase: $PBAD client(s) failed"
  rwm=$(rate $((C * FILE_MIB)))
  umnt b; hb=$HWM

  mnt m
  par mc_one; [ $PBAD = 0 ] || fail "metadata create: $PBAD client(s) failed"; mf=$(rate $((C * FILES)))
  par ms_one; [ $PBAD = 0 ] || fail "metadata stat: $PBAD client(s) failed"; ms=$(rate $((C * FILES)))
  par mr_one; [ $PBAD = 0 ] || fail "metadata read: $PBAD client(s) failed (a meta-mismatch line is a content mismatch)"; mr=$(rate $((C * FILES)))
  par mu_one; [ $PBAD = 0 ] || fail "metadata unlink: $PBAD client(s) failed"; mu=$(rate $((C * FILES)))
  umnt m; hm=$HWM

  mnt v
  par rv_one; [ $PBAD = 0 ] || fail "random-write content check: $PBAD client(s) failed (a rand-mismatch line is a content mismatch)"
  rvm=$(rate $((C * FILE_MIB)))
  umnt v; hv=$HWM

  echo "bench $RUN write_mbs=$wm read_mbs=$rm meta_fps=$mf rand_mbs=$rwm vmhwm_a_mib=$ha vmhwm_b_mib=$hb mismatches=$mism max_weight=$MAXW max_queue_wait_ms=$MAXQ load=$load meta_stat_ops=$ms meta_read_ops=$mr meta_unlink_ops=$mu randcheck_mbs=$rvm vmhwm_m_mib=$hm vmhwm_v_mib=$hv"
}

if [ -n "${POOL_BENCH_N:-}" ]; then
  for r in $(seq ${POOL_BENCH_ROUNDS:-1}); do run_one $POOL_BENCH_N $r off default; done
  if [ "${POOL_BENCH_EXTRA:-0}" = 1 ]; then run_one $POOL_BENCH_N 1 on default; run_one $POOL_BENCH_N 1 off 2; fi
elif [ "${POOL_BENCH_QUICK:-0}" = 1 ]; then
  for n in 1 2; do run_one $n 1 off default; done
else
  r=0
  for order in "1 2 4 8 16" "16 8 4 2 1" "4 16 1 8 2"; do
    r=$((r + 1))
    for n in $order; do run_one $n $r off default; done
    run_one 1 $r on default
    run_one 4 $r on default
  done
  run_one 16 1 off 2
fi
echo "loadavg-end $(cat /proc/loadavg)"
rm -rf $R/store $R/src
echo "pool-bench: done"
