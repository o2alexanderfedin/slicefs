#!/bin/bash
# The ThreadSanitizer gate (Phase 21.2, TSAFE-01, TSAFE-03, D-15 to D-17): the
# command of job `tsan` in .github/workflows/safety.yml, and the same run for a
# developer (Linux or macOS). It builds with -Zsanitizer=thread and -Zbuild-std
# for the host triple on the pinned nightly (with rust-src) and runs a fixed,
# named set: the multi-worker and concurrency test targets of slicefs-cli, the
# two race tests of plans 21.2-02 and 21.2-04, a few chunk-store and metadata
# lib test modules, and a short in-process stress run with a fixed seed.
# Each entry prints `TSAN target=<t> result=<ok|fail> reports=<n> secs=<s>`.
# Any ThreadSanitizer report, any test failure, or an entry with no passed
# test fails the gate. A suppression needs a proven false positive and a `#`
# reason line right above it in scripts/safety/tsan.supp (D-17).
# On the ubuntu runner, run `sudo sysctl vm.mmap_rnd_bits=28` first (the job
# does): with 32 bits TSan aborts with "unexpected memory mapping".
# Usage: scripts/safety/tsan.sh
# Env:   TSAN_TESTS   slicefs-cli test targets (default: the list below)
#        TSAN_LIB     crate:filter pairs of lib tests (default: the list below)
#        TSAN_LIB_SKIP lib tests to skip, one per line (default: the list below)
#        TSAN_STRESS  1 (default) runs the stress entry, 0 skips it
#        NIGHTLY      the toolchain (default: the pin in safety.yml)
#        CARGO_TARGET_DIR  the build goes to its subdirectory tsan
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
source "$HERE/toolchains.sh"
need_toolchain "$NIGHTLY" rust-src
# need_toolchain skips a toolchain that is there already, also without rust-src.
rustup component add --toolchain "$NIGHTLY" rust-src </dev/null
cd "$REPO"

# The named set (D-16), one name per line.
DEFAULT_TESTS="
pool_tests
reply_after_locks
multi_handle_posix
lock_free_io
flusher_tests
dirty_budget_tests
race_closed_size_tests
rename_ancestor_tests
"
# Candidates (crate:filter), kept or removed by the rule of plan 21.2-07 Task 3.
DEFAULT_LIB="
chunk-store:race_tests::
chunk-store:lock_tail_tests::
metadata:wal::per_op::
"
# Lib tests that the TSAN_LIB runs skip (libtest --skip, a substring), each
# with its reason on the line above it.
DEFAULT_LIB_SKIP="
# a real-time limit of 5 s per step: the 26 MiB of puts take longer in a debug
# TSan build (it times out at 9.9 s on the Mac); it passes under TSan in release
# in 0.53 s with no report (plan 21.2-07).
lock_tail_tests::put_and_read_pass_a_held_keep_cap_wait
"
TSAN_TESTS="${TSAN_TESTS:-$DEFAULT_TESTS}"
TSAN_LIB_SKIP="${TSAN_LIB_SKIP-$DEFAULT_LIB_SKIP}"
skip_args=()
while IFS= read -r s; do
  case "$s" in ''|'#'*) continue ;; esac
  skip_args+=(--skip "$s")
done <<<"$TSAN_LIB_SKIP"
TSAN_LIB="${TSAN_LIB-$DEFAULT_LIB}"
TSAN_STRESS="${TSAN_STRESS:-1}"

# D-17: every suppression has a reason line right above it.
SUPP="$HERE/tsan.supp"
if [ -f "$SUPP" ]; then
  if ! awk '/^[[:space:]]*$/ { prev = ""; next }
            /^#/ { prev = "#"; next }
            { if (prev != "#") { print "tsan.supp line " NR ": no # reason line above: " $0; bad = 1 } prev = "x" }
            END { exit bad }' "$SUPP" </dev/null; then
    echo "TSAN FAILED: a tsan.supp entry has no reason"; exit 1
  fi
fi

HOST=$(rustc +"$NIGHTLY" -vV </dev/null | sed -n 's/^host: //p')
[ -n "$HOST" ] || { echo "TSAN FAILED: no host triple from rustc -vV"; exit 1; }
# With an explicit --target, RUSTFLAGS do not reach build scripts and proc macros.
export RUSTFLAGS="-Zsanitizer=thread"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}/tsan"
TSAN_OPTIONS="halt_on_error=0 second_deadlock_stack=1"
[ -f "$SUPP" ] && TSAN_OPTIONS="$TSAN_OPTIONS suppressions=$SUPP"
export TSAN_OPTIONS
echo "TSAN host=$HOST options=$TSAN_OPTIONS"
TSAN=(cargo +"$NIGHTLY" test --locked -Zbuild-std --target "$HOST")

LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT
FAILED=0

# One entry: $1 the target name, the rest the cargo test arguments.
# stdout and stderr go to one log, so a report is counted with its test.
run_entry() {
  local name="$1"; shift
  local rc=0 t0 secs reports passed result=ok
  t0=$(date +%s)
  timeout 1200 "${TSAN[@]}" "$@" </dev/null 2>&1 | tee "$LOG" || rc=$?
  secs=$(( $(date +%s) - t0 ))
  reports=$(grep -c 'WARNING: ThreadSanitizer:' "$LOG" || true)
  passed=$(sed -n -E 's/^test result: ok\. ([0-9]+) passed.*/\1/p' "$LOG" | awk '{ s += $1 } END { print s + 0 }')
  if [ "$rc" -ne 0 ] || [ "$reports" -ne 0 ] || [ "$passed" -eq 0 ]; then
    result=fail; FAILED=1
  fi
  echo "TSAN target=$name result=$result reports=$reports secs=$secs passed=$passed exit=$rc"
}

# Build every test binary first, so the build-std time is not part of the
# first entry's time.
t0=$(date +%s); rc=0
build_args=()
for t in $TSAN_TESTS; do build_args+=(--test "$t"); done
if [ ${#build_args[@]} -gt 0 ]; then
  timeout 2400 "${TSAN[@]}" -p slicefs-cli "${build_args[@]}" --no-run </dev/null || rc=$?
fi
for c in $(for e in $TSAN_LIB; do echo "${e%%:*}"; done | sort -u); do
  [ "$rc" -eq 0 ] && { timeout 2400 "${TSAN[@]}" -p "$c" --lib --no-run </dev/null || rc=$?; }
done
if [ "$rc" -eq 0 ] && [ "$TSAN_STRESS" = 1 ]; then
  timeout 2400 "${TSAN[@]}" --release -p slicefs-cli --test stress_harness --no-run </dev/null || rc=$?
fi
echo "TSAN build secs=$(( $(date +%s) - t0 )) exit=$rc"
[ "$rc" -eq 0 ] || { echo "TSAN FAILED: the build of the test targets"; exit 1; }

for t in $TSAN_TESTS; do
  run_entry "$t" -p slicefs-cli --test "$t"
done
for e in $TSAN_LIB; do
  crate="${e%%:*}"; filter="${e#*:}"
  run_entry "$crate:$filter" -p "$crate" --lib -- "$filter" ${skip_args[@]+"${skip_args[@]}"}
done
if [ "$TSAN_STRESS" = 1 ]; then
  # A short stress run of the real write path with a fixed seed (D-16).
  STRESS_SECS=10 STRESS_THREADS=4 STRESS_FILE_MIB=8 STRESS_SERIALIZE=0 STRESS_MODE=both STRESS_SEED=2102 \
    run_entry long_run --release -p slicefs-cli --test stress_harness -- --ignored --exact long_run --nocapture
fi

if [ "$FAILED" -ne 0 ]; then
  echo "TSAN FAILED: see the TSAN target lines above"; exit 1
fi
echo "TSAN ok"
