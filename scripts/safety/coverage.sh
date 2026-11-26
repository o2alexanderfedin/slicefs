#!/bin/bash
# Line coverage gate (Phase 21.2, TSAFE-05, D-05, D-31, D-32): the command of
# job `coverage` in .github/workflows/safety.yml, and the same run for a developer.
# It runs scripts/docker-linux-test/run.sh coverage (Linux Docker with /dev/fuse:
# the instrumented workspace tests, then the instrumented daemon through the FUSE
# smoke) and gates the tests-only workspace number on the floor.
# The floor is the first line of scripts/safety/coverage-floor.txt that is not a
# comment or empty (the one place of the floor). With no floor the job prints the
# numbers and COVERAGE floor=unset and fails (exit 3).
# Usage: scripts/safety/coverage.sh
# Env:   COVERAGE_FLOOR     a value set in the environment wins over the file
#        SLICEFS_TARGET_VOL the cargo target volume of run.sh (default slicefs-target)
#        CARGO_BUILD_JOBS   cargo -j inside the container (default: all CPUs)
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
FLOOR_FILE="$HERE/coverage-floor.txt"
if [ -z "${COVERAGE_FLOOR:-}" ] && [ -f "$FLOOR_FILE" ]; then
  COVERAGE_FLOOR=$(grep -vE '^[[:space:]]*(#|$)' "$FLOOR_FILE" </dev/null | head -n 1 | tr -d '[:space:]' || true)
fi
if [ -n "${COVERAGE_FLOOR:-}" ]; then
  [[ "$COVERAGE_FLOOR" =~ ^[0-9]+(\.[0-9]+)?$ ]] \
    || { echo "coverage.sh: the floor '$COVERAGE_FLOOR' is not a number" >&2; exit 2; }
  export COVERAGE_FLOOR
fi
echo "coverage.sh: floor $([ -n "${COVERAGE_FLOOR:-}" ] && echo "$COVERAGE_FLOOR" || echo none)"
exec "$REPO/scripts/docker-linux-test/run.sh" coverage
