#!/bin/bash
# The deadlock-detect stress gate (Phase 21.2, TSAFE-01, D-30): the command of
# job `deadlock` in .github/workflows/safety.yml, and the same run for a
# developer. It builds tests/stress_harness.rs in release with
# --features deadlock-detect and runs `long_run` (8 workers, no serialize, both
# modes, a fixed seed), then `serialize_off_target`. With the feature, the
# harness asks parking_lot for a lock cycle every second and aborts on one.
# What the detector cannot see: std locks (the commit gate, Mutex<StoreIo>, the
# pool queue) and condvar waits. A stall there is caught by the harness
# watchdog (abort after secs + 60 + verify time) and by `timeout` around each
# run; the loom models (scripts/safety/model.sh) cover the gate and the pool.
# No mount-level run: a mounted daemon has no checker thread (D-30).
# The `max_inflight` value printed is the smallest of the two modes; each mode
# must reach more than 1 (the operations overlapped).
# Usage: scripts/safety/deadlock.sh
# Env:   STRESS_SECS STRESS_THREADS STRESS_SERIALIZE STRESS_MODE STRESS_FILE_MIB
#        STRESS_SEED override the defaults below
#        RUST_STABLE  the toolchain (default: the pin in safety.yml; locally
#                     RUST_STABLE=stable)
#        CARGO_TARGET_DIR  the build goes to its subdirectory deadlock
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
source "$HERE/toolchains.sh"
need_toolchain "$RUST_STABLE"
cd "$REPO"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}/deadlock"

DEFAULTS="STRESS_SECS=20 STRESS_THREADS=8 STRESS_SERIALIZE=0 STRESS_MODE=both STRESS_FILE_MIB=16 STRESS_SEED=2102"
for kv in $DEFAULTS; do
  k="${kv%%=*}"
  [ -n "${!k:-}" ] || export "$kv"
  export "$k"
done
echo "DEADLOCK STRESS_SECS=$STRESS_SECS STRESS_THREADS=$STRESS_THREADS STRESS_SERIALIZE=$STRESS_SERIALIZE STRESS_MODE=$STRESS_MODE STRESS_FILE_MIB=$STRESS_FILE_MIB STRESS_SEED=$STRESS_SEED"

TEST=(cargo +"$RUST_STABLE" test --locked --release -p slicefs-cli --features deadlock-detect --test stress_harness)
t0=$(date +%s); rc=0
timeout 1800 "${TEST[@]}" --no-run </dev/null || rc=$?
echo "DEADLOCK build secs=$(( $(date +%s) - t0 )) exit=$rc"
[ "$rc" -eq 0 ] || { echo "DEADLOCK fail build exit $rc"; exit 1; }

LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT

# Why a run failed, from its exit code and log.
reason() {
  if grep -q 'STRESS deadlock detected' "$LOG"; then echo "lock cycle (parking_lot detector)"
  elif grep -q 'STRESS watchdog' "$LOG"; then echo "stall (harness watchdog)"
  elif [ "$1" -eq 124 ]; then echo "stall (timeout)"
  else echo "test failure exit $1"
  fi
}

# One run: $1 the test name, the rest the libtest arguments.
run_one() {
  local name="$1"; shift
  local rc=0 t0 passed
  t0=$(date +%s)
  timeout 900 "${TEST[@]}" -- "$@" </dev/null 2>&1 | tee "$LOG" || rc=$?
  SECS=$(( $(date +%s) - t0 ))
  if [ "$rc" -ne 0 ]; then echo "DEADLOCK fail $name: $(reason "$rc")"; exit 1; fi
  passed=$(sed -n -E 's/^test result: ok\. ([0-9]+) passed.*/\1/p' "$LOG" | head -n 1)
  if [ "${passed:-0}" -eq 0 ]; then echo "DEADLOCK fail $name: no test passed"; exit 1; fi
}

run_one long_run --ignored --exact long_run --nocapture
inflight=$(sed -n -E 's/^max_inflight=([0-9]+)$/\1/p' "$LOG" | sort -n | head -n 1)
if [ -z "$inflight" ] || [ "$inflight" -le 1 ]; then
  echo "DEADLOCK fail long_run: the operations did not overlap (max_inflight=${inflight:-none})"; exit 1
fi
echo "DEADLOCK long_run=ok max_inflight=$inflight secs=$SECS"

run_one serialize_off_target --exact serialize_off_target --nocapture
echo "DEADLOCK serialize_off_target=ok secs=$SECS"
echo "DEADLOCK ok"
