#!/bin/bash
# Line coverage on Linux (Phase 21.2, TSAFE-05, D-31, D-32). Runs inside the
# Docker image of this directory, from the tree root, with /dev/fuse
# (run.sh coverage, or scripts/safety/coverage.sh on the host).
# Steps:
#   1. cargo llvm-cov show-env: the instrumented build goes to
#      $CARGO_TARGET_DIR/cov, apart from the plain builds of the other modes.
#   2. cargo llvm-cov clean --workspace, then cargo test --workspace (instrumented).
#   3. Report of the tests: COVERAGE tests workspace=<x> <crate>=<y> ... fuse_callbacks=<z>
#   4. Gate on the tests number: COVERAGE floor=<f> result=<ok|below>, or
#      COVERAGE floor=unset (no floor: fail closed, exit 3).
#   5. The instrumented daemon through the FUSE smoke (mt-linux.sh); its profiles
#      go to a side directory, so the tests number never includes them.
#   6. Report of tests plus smoke: COVERAGE merged workspace=<x> fuse_callbacks=<y>
#      (information only). A smoke failure prints COVERAGE fuse=fail and does
#      not change the exit status: the gate is the tests number.
# Env: COVERAGE_FLOOR        the floor in percent of lines (unset: exit 3)
#      COVERAGE_REPORT_ONLY  1: steps 3 and 4 only, on the profiles of the last run
#                            (run from the same tree path: the profile names hold it)
#      CARGO_BUILD_JOBS      passed to cargo as -j when set
#      CARGO_TARGET_DIR      the target root (default ./target); coverage uses <root>/cov
# Exit: 0 ok, 1 below the floor, 3 no floor, other: the build or a test failed.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
IGNORE='/(tests|benches|benchmarks|vendor|examples)/'
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}/cov"
JOBS=()
[ -n "${CARGO_BUILD_JOBS:-}" ] && JOBS=(-j "$CARGO_BUILD_JOBS")
echo "coverage: $(cargo llvm-cov --version) $(rustc -V)"

source <(cargo llvm-cov show-env --sh 2>/dev/null)
[ -n "${CARGO_LLVM_COV:-}" ] && [ -n "${LLVM_PROFILE_FILE:-}" ] \
  || { echo "coverage: cargo llvm-cov show-env set no environment"; exit 2; }
SIDE="$CARGO_TARGET_DIR/fuse-profiles"

# One line from a JSON summary: workspace, then each crate, then fuse_callbacks.rs.
# Per crate (by name): covered lines over all lines of its files, not a mean of percents.
summary() {
  jq -r '.data[0].totals.lines as $t | "workspace \($t.covered) \($t.count)",
    (.data[0].files[] | .summary.lines as $l |
      ((.filename | capture("/crates/(?<c>[^/]+)/").c // empty) | "crate \(.) \($l.covered) \($l.count)"),
      (if (.filename | endswith("/fuse_callbacks.rs")) then "fuse \($l.covered) \($l.count)" else empty end))' "$1" \
  | LC_ALL=C sort | awk -v kind="$2" -v crates="$3" '
      function pct(c, n) { return n ? sprintf("%.2f", 100 * c / n) : "0.00" }
      $1 == "workspace" { w = pct($2, $3) }
      $1 == "crate" { cc[$2] += $3; cn[$2] += $4; if (!($2 in seen)) { seen[$2] = 1; order[++k] = $2 } }
      $1 == "fuse" { fc += $2; fn += $3 }
      END {
        line = "COVERAGE " kind " workspace=" w
        if (crates) for (i = 1; i <= k; i++) line = line " " order[i] "=" pct(cc[order[i]], cn[order[i]])
        print line " fuse_callbacks=" pct(fc, fn)
      }'
}

if [ "${COVERAGE_REPORT_ONLY:-0}" != 1 ]; then
  cargo llvm-cov clean --workspace
  rm -rf "$SIDE"
  rc=0
  cargo test --workspace ${JOBS[@]+"${JOBS[@]}"} </dev/null || rc=$?
  [ "$rc" -eq 0 ] || { echo "coverage: cargo test --workspace failed (exit $rc)"; exit "$rc"; }
fi

cargo llvm-cov report --json --summary-only --ignore-filename-regex "$IGNORE" --output-path /tmp/cov-tests.json
summary /tmp/cov-tests.json tests 1

gate=0
if [ -z "${COVERAGE_FLOOR:-}" ]; then
  echo "COVERAGE floor=unset"
  gate=3
else
  rc=0
  cargo llvm-cov report --summary-only --ignore-filename-regex "$IGNORE" \
    --fail-under-lines "$COVERAGE_FLOOR" > /tmp/cov-table.txt || rc=$?
  tail -n 1 /tmp/cov-table.txt
  if [ "$rc" -eq 0 ]; then echo "COVERAGE floor=$COVERAGE_FLOOR result=ok"
  else echo "COVERAGE floor=$COVERAGE_FLOOR result=below"; gate=1; fi
fi

if [ "${COVERAGE_REPORT_ONLY:-0}" != 1 ]; then
  cargo build -p slicefs-cli ${JOBS[@]+"${JOBS[@]}"} </dev/null
  B="$CARGO_TARGET_DIR/debug/slicefs"
  [ -x "$B" ] || { echo "coverage: no instrumented binary at $B"; exit 2; }
  # The container must be 45 s old before the first mount (see run.sh).
  w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w
  mkdir -p "$SIDE"
  rc=0
  LLVM_PROFILE_FILE="$SIDE/$(basename "$LLVM_PROFILE_FILE")" bash "$HERE/mt-linux.sh" "$B" || rc=$?
  [ "$rc" -eq 0 ] || echo "COVERAGE fuse=fail"
  echo "coverage: smoke profiles=$(find "$SIDE" -name '*.profraw' | wc -l)"
  # Copy the smoke profiles in for the merged report, then take them out again.
  copied=()
  for f in "$SIDE"/*.profraw; do
    [ -e "$f" ] || continue
    cp "$f" "$CARGO_TARGET_DIR/"; copied+=("$CARGO_TARGET_DIR/$(basename "$f")")
  done
  cargo llvm-cov report --json --summary-only --ignore-filename-regex "$IGNORE" --output-path /tmp/cov-merged.json
  summary /tmp/cov-merged.json merged 0
  [ ${#copied[@]} -eq 0 ] || rm -f "${copied[@]}"
fi
exit "$gate"
