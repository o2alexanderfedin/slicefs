#!/bin/bash
# The clippy gate (Phase 21.2, TSAFE-01): the command of job `clippy` in
# .github/workflows/safety.yml (rows linux and macos), and the same run for a
# developer. Two checks:
#   1. clippy on the pinned stable toolchain, every workspace crate and every
#      target, with warnings as errors (vendor/fuser is outside the workspace,
#      so its findings stay warnings);
#   2. every lint allow/expect attribute in crates/ and benchmarks/ has a
#      `reason = "..."` on the same line.
# Usage: scripts/safety/clippy.sh
# Env:   RUST_STABLE   the toolchain (default: the pin in safety.yml; RUST_STABLE=stable locally)
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
source "$HERE/toolchains.sh"
need_toolchain "$RUST_STABLE" clippy
# need_toolchain skips a toolchain that is there already, also without clippy.
rustup component add --toolchain "$RUST_STABLE" clippy </dev/null
cd "$REPO"

cargo +"$RUST_STABLE" clippy --locked --workspace --all-targets -- -D warnings </dev/null

# A lint allow without a written reason (D-09). grep exits 1 when it finds
# nothing, which is the good case here.
missing=$(grep -rnE '(allow|expect)\((clippy::|dead_code|unused|deprecated|unreachable)' crates benchmarks --include='*.rs' </dev/null \
  | grep -v 'reason =' || true)
if [ -n "$missing" ]; then
  echo "CLIPPY FAILED: lint allow without a reason (add reason = \"...\" on the same line):"
  echo "$missing"
  exit 1
fi
echo "CLIPPY os=$(uname -s) toolchain=$RUST_STABLE result=ok"
