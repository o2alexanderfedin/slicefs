#!/bin/bash
# The Miri probe (Phase 21.2, D-12): runs each lib test of one crate under Miri
# in its own process and prints its class. A tool for a developer, not a CI
# job: run it again when tests change, to find the tests that need a
# `#[cfg_attr(miri, ignore = "<reason>")]`. One process per test, because an
# unsupported operation stops the whole test binary and hides the later tests.
# Usage: scripts/safety/miri-probe.sh <crate> [target]
# Env:   MIRI_PROBE_SECS     the time limit of one test (default 120)
#        MIRI_PROBE_JOBS     tests at the same time (default 4)
#        MIRI_PROBE_TESTS    a file with the test names to run (default: all lib tests)
#        MIRI_PROBE_LOGDIR   keep the log of each test in this directory (default: removed)
#        NIGHTLY             the toolchain (default: the pin in safety.yml)
# Output: one line per test, `PROBE <crate> <name> <class> <secs> <detail>`, with
#         class pass, unsupported, ub, fail or timeout; then
#         `PROBE <crate> total=<n> pass=<p> unsupported=<u> ub=<b> fail=<f> timeout=<t>`.
# A Miri test run counts as UB when its log has "Undefined Behavior" or "Data race".
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
[ $# -ge 1 ] || { echo "usage: $0 <crate> [target]" >&2; exit 2; }
source "$HERE/toolchains.sh"
need_toolchain "$NIGHTLY" miri rust-src
cd "$REPO"

CRATE="$1"
TARGET_ARGS=()
[ $# -ge 2 ] && TARGET_ARGS=(--target "$2")
# The host clock, temp dirs and files; does not weaken the UB or data-race checks.
export MIRIFLAGS="-Zmiri-disable-isolation"
cargo +"$NIGHTLY" miri setup ${TARGET_ARGS[@]+"${TARGET_ARGS[@]}"} </dev/null >&2

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
LOGS="${MIRI_PROBE_LOGDIR:-$WORK/logs}"
mkdir -p "$LOGS"

if [ -n "${MIRI_PROBE_TESTS:-}" ]; then
  cp "$MIRI_PROBE_TESTS" "$WORK/tests.txt"
else
  cargo +"$NIGHTLY" miri test --locked -p "$CRATE" ${TARGET_ARGS[@]+"${TARGET_ARGS[@]}"} --lib -- --list --format terse </dev/null 2>"$WORK/list.err" \
    | sed -n -E 's/^(.*): test$/\1/p' > "$WORK/tests.txt" || { cat "$WORK/list.err" >&2; exit 1; }
fi
[ -s "$WORK/tests.txt" ] || { echo "PROBE $CRATE: no tests listed" >&2; exit 1; }

probe_one() {
  local name="$1" log rc=0 t0 secs class detail targs=()
  [ -n "$TARGET_SPEC" ] && targs=(--target "$TARGET_SPEC")
  log="$LOGS/$(printf '%s' "$name" | tr -c 'A-Za-z0-9_.-' '_').log"
  t0=$(date +%s)
  timeout "${MIRI_PROBE_SECS:-120}" cargo +"$NIGHTLY" miri test --locked -p "$CRATE" ${targs[@]+"${targs[@]}"} --lib -- --exact "$name" </dev/null >"$log" 2>&1 || rc=$?
  secs=$(( $(date +%s) - t0 ))
  detail=""
  if grep -qE 'Undefined Behavior|Data race' "$log"; then
    class=ub; detail=$(grep -m 1 -E 'Undefined Behavior|Data race' "$log")
  elif [ "$rc" -eq 124 ]; then
    class=timeout
  elif grep -q 'unsupported operation:' "$log"; then
    class=unsupported; detail=$(grep -m 1 'unsupported operation:' "$log" | sed -E 's/.*unsupported operation: *//')
  elif [ "$rc" -eq 0 ] && grep -q '^test result: ok\. 1 passed' "$log"; then
    class=pass
  else
    class=fail
    detail=$(grep -m 1 -A 1 'panicked at' "$log" | tr '\n' ' ' || true)
    [ -n "$detail" ] || detail=$(grep -m 1 '^error' "$log" || true)
    [ -n "$detail" ] || detail="exit $rc"
  fi
  echo "PROBE $CRATE $name $class $secs $detail"
}
export -f probe_one
export CRATE LOGS NIGHTLY
# An array does not pass to the bash of xargs; probe_one reads the target from here.
export TARGET_SPEC="${2:-}"

xargs -P "${MIRI_PROBE_JOBS:-4}" -I{} bash -c 'probe_one "$1"' _ {} \
  < "$WORK/tests.txt" | tee "$WORK/out.txt"

count() { awk -v c="$1" '$4 == c' "$WORK/out.txt" | wc -l | tr -d ' '; }
total=$(grep -c '^PROBE ' "$WORK/out.txt" || true)
echo "PROBE $CRATE total=$total pass=$(count pass) unsupported=$(count unsupported) ub=$(count ub) fail=$(count fail) timeout=$(count timeout)"
