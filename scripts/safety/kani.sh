#!/bin/bash
# Kani proofs of slicefs-cli (Phase 21.2, TSAFE-04, D-05, D-22, D-23).
# Installs the pinned KANI_VERSION (the env block of safety.yml) when it is
# missing, runs every #[cfg(kani)] harness of the slicefs-cli lib and prints
# one line per harness. It fails unless every harness is SUCCESSFUL and the
# verified count equals the count of proof attributes in the source (no
# harness dropped silently).
# Usage: scripts/safety/kani.sh
#   KANI_HARNESS=<name> scripts/safety/kani.sh   one harness, by hand
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/safety/toolchains.sh

if ! cargo kani --version </dev/null 2>/dev/null | grep -qF "Verifier $KANI_VERSION "; then
  cargo install --locked kani-verifier --version "$KANI_VERSION" </dev/null
  cargo kani setup </dev/null
fi
echo "KANI version=$KANI_VERSION (Kani uses its own bundled nightly, not NIGHTLY=$NIGHTLY)"

args=(--manifest-path crates/slicefs-cli/Cargo.toml --lib --output-format terse)
[ -n "${KANI_HARNESS:-}" ] && args+=(--harness "$KANI_HARNESS")
log="$(mktemp "${TMPDIR:-/tmp}/kani.XXXXXX")"
rc=0
timeout 1800 cargo kani "${args[@]}" </dev/null 2>&1 | tee "$log" || rc=$?

# One line per harness: the terse output prints the name, the result, then the time.
awk '
  /^Checking harness / { name = $3; sub(/\.\.\.$/, "", name) }
  /^VERIFICATION:- / { result = $2 }
  /^Verification Time: / { secs = $3; sub(/s$/, "", secs)
    printf "KANI harness=%s result=%s secs=%.2f\n", name, result, secs }
' "$log" </dev/null

summary="$(grep -E '^Complete - [0-9]+ successfully verified harnesses, [0-9]+ failures, [0-9]+ total\.' "$log" </dev/null || true)"
rm -f "$log"
[ -n "$summary" ] || { echo "KANI fail: no Complete line (cargo kani exit $rc)"; exit 1; }
ok="$(echo "$summary" | sed -E 's/^Complete - ([0-9]+) .*/\1/')"
failed="$(echo "$summary" | sed -E 's/.* ([0-9]+) failures.*/\1/')"
if [ -n "${KANI_HARNESS:-}" ]; then
  want=1
else
  want="$(grep -rhoF '#[kani::proof]' crates/slicefs-cli/src </dev/null | wc -l | tr -d ' ')"
fi
if [ "$rc" -ne 0 ] || [ "$failed" -ne 0 ] || [ "$ok" -ne "$want" ]; then
  echo "KANI fail: verified=$ok failures=$failed expected=$want (cargo kani exit $rc)"
  exit 1
fi
echo "KANI ok harnesses=$ok"
