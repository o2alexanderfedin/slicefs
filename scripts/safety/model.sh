#!/bin/bash
# The loom models of the lock protocol (Phase 21.2, TSAFE-02): the command of
# job `model` in .github/workflows/safety.yml, and the same run for a developer.
# It builds with `--cfg loom` on the pinned stable toolchain (loom's
# `generator` dependency does not build on the pinned nightly), in a separate
# target directory (the cfg changes every crate), and runs only the `loom_`
# tests of each crate: other tests cannot run under `--cfg loom`.
# Models named `*_all_orders` search every thread order; the others search
# every order with at most LOOM_MAX_PREEMPTIONS preemptions.
# Usage: scripts/safety/model.sh
# Env:   LOOM_MAX_PREEMPTIONS   the preemption bound (default 3)
#        MODEL_CRATES           the crates with models (default "chunk-store slicefs-cli")
#        CARGO_TARGET_DIR       the target root (default <repo>/target); the models use <root>/loom
#        RUST_STABLE            the toolchain (default: the pin in safety.yml; RUST_STABLE=stable locally)
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
source "$HERE/toolchains.sh"
need_toolchain "$RUST_STABLE"

export RUSTFLAGS="--cfg loom"
export LOOM_MAX_PREEMPTIONS="${LOOM_MAX_PREEMPTIONS:-3}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}/loom"
cd "$REPO"
echo "MODEL bound=$LOOM_MAX_PREEMPTIONS"

LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT
for crate in ${MODEL_CRATES:-chunk-store slicefs-cli}; do
  rc=0
  cargo +"$RUST_STABLE" test --locked --release -p "$crate" --lib -- loom_ </dev/null 2>&1 | tee "$LOG" || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "MODEL FAILED crate=$crate: cargo exit $rc"; exit 1
  fi
  if grep -q '^running 0 tests' "$LOG"; then
    echo "MODEL FAILED crate=$crate: 0 loom_ tests ran"; exit 1
  fi
  passed=$(grep -cE '^test .*loom_.* \.\.\. ok$' "$LOG" || true)
  if [ "$passed" -eq 0 ]; then
    echo "MODEL FAILED crate=$crate: no passed loom_ test"; exit 1
  fi
  echo "MODEL crate=$crate passed=$passed"
done
echo "MODEL ok"
