#!/bin/bash
# Spike S1 client: do FUSE requests overlap, and are out-of-order replies accepted?
# Mounts the S1 probe (crates/slicefs-cli/examples/s1_probe.rs), then in 5 rounds
# reads a slow file (the probe delays that READ 200 ms) while another process
# stats 50 new names. Prints one `round=` line per round and the probe's
# `S1 RESULT` line. Works on macOS (FUSE-T FSKit) and on Linux (Docker control).
# Every wait has a time limit and ends when the probe ends.
#
# usage: s1-overlap.sh <probe-binary>
# env:   WORK   work dir (default: a new temp dir)
set -u
PROBE=${1:?usage: s1-overlap.sh <probe-binary>}
PROBE=$(cd "$(dirname "$PROBE")" && pwd -P)/$(basename "$PROBE")
[ -n "${WORK:-}" ] && mkdir -p "$WORK"
W=$(cd "${WORK:-$(mktemp -d)}" && pwd -P)
M="$W/mnt"
mkdir -p "$M"
OS=$(uname -s)
echo "run dir: $W"

MP=
cleanup() {
  if [ -n "$MP" ] && kill -0 "$MP" 2>/dev/null; then
    do_unmount >/dev/null 2>&1
    sleep 1
    kill -9 "$MP" 2>/dev/null
  fi
}
trap cleanup EXIT

# mount(8) shows the path without the /private prefix on macOS.
mounted() { mount | grep -qF -- " ${M#/private} "; }
# waitmnt: up to 60 s; stops early if the probe ends.
waitmnt() {
  for _ in $(seq 1 60); do
    mounted && return 0
    kill -0 "$MP" 2>/dev/null || { echo "  probe ended before mount"; break; }
    sleep 1
  done
  echo "  MOUNT FAILED"; tail -20 "$W/probe.err"; return 1
}
# waitexit: up to 60 s for the probe to exit after the unmount.
waitexit() {
  for _ in $(seq 1 120); do kill -0 "$MP" 2>/dev/null || return 0; sleep 0.5; done
  echo "  PROBE DID NOT EXIT in 60 s"; kill -9 "$MP" 2>/dev/null; return 1
}
# runlim <secs> <cmd...>: run with a kill-based time limit (macOS has no timeout).
runlim() {
  local lim=$1 s; s=$(date +%s); shift
  "$@" & local p=$!
  while kill -0 $p 2>/dev/null; do
    if [ $(( $(date +%s) - s )) -ge "$lim" ]; then
      kill -9 $p 2>/dev/null; wait $p 2>/dev/null; echo "  HANG (>${lim}s): $*"; return 124
    fi
    sleep 0.2
  done
  wait $p
}
do_unmount() {
  if [ "$OS" = Darwin ]; then
    runlim 30 umount "$M" || runlim 30 diskutil unmount force "$M"
  else
    runlim 30 fusermount -u "$M" || runlim 30 umount -l "$M"
  fi
}

# One round: slow read in the background; the 50 stats of new names start only
# after the probe logged the slow READ (`slow_read` on its ARR line), so the
# READ is in flight when the stats run. `during` counts stats that ended before
# dd ended. `first_stat_after_arr_ms` is the gap from the READ arrival log to
# the first stat; it is a client-side bound (polling and python start-up).
ROUND_PY='
import os, sys, subprocess, threading, time
m, i, errp = sys.argv[1], sys.argv[2], sys.argv[3]
def seen():
    try:
        with open(errp) as f:
            return sum(1 for l in f if "slow_read" in l)
    except OSError:
        return 0
base = seen()
t0 = time.monotonic()
p = subprocess.Popen(["dd", "if=%s/slow%s" % (m, i), "of=/dev/null", "bs=4096", "count=1"],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
done = {}
def waiter():
    done["rc"] = p.wait()
    done["t"] = time.monotonic()
th = threading.Thread(target=waiter, daemon=True)
th.start()
while seen() <= base:
    if time.monotonic() - t0 > 5:
        p.kill()
        print("  probe never logged the slow READ in 5 s")
        sys.exit(5)
    time.sleep(0.005)
t_seen = time.monotonic()
ends, lat = [], []
first = None
for j in range(50):
    s = time.monotonic()
    if first is None:
        first = (s - t_seen) * 1000.0
    try:
        os.stat("%s/f%s-%d" % (m, i, j))
    except OSError as e:
        print("  stat error: %s" % e)
    e = time.monotonic()
    ends.append(e)
    lat.append((e - s) * 1000.0)
th.join(10)
if "t" not in done:
    p.kill()
    print("  dd did not finish in 10 s")
    sys.exit(3)
dd_ms = (done["t"] - t0) * 1000.0
during = sum(1 for e in ends if e < done["t"])
print("round=%s dd_ms=%.0f fast_stats_during_slow=%d max_stat_ms=%.0f first_stat_after_arr_ms=%.0f"
      % (i, dd_ms, during, max(lat), first))
sys.exit(0 if done["rc"] == 0 else 4)
'

"$PROBE" "$M" >"$W/probe.out" 2>"$W/probe.err" </dev/null &
MP=$!
FAILS=0
if waitmnt; then
  for i in 0 1 2 3 4; do
    # The python round has its own 10 s dd limit; this limit covers the stats.
    runlim 60 python3 -c "$ROUND_PY" "$M" "$i" "$W/probe.err" || FAILS=$((FAILS+1))
  done
  do_unmount || FAILS=$((FAILS+1))
else
  FAILS=$((FAILS+1))
fi
waitexit || FAILS=$((FAILS+1))
MP=

echo "uname: $(uname -sr)"
[ "$OS" = Darwin ] && echo "sw_vers: $(sw_vers -productVersion)"
RES=$(grep '^S1 RESULT' "$W/probe.out" | head -1)
if [ -z "$RES" ]; then
  echo "NO S1 RESULT LINE"; tail -20 "$W/probe.err"; exit 1
fi
echo "$RES"
[ $FAILS -eq 0 ] || { echo "client failures: $FAILS"; exit 1; }
