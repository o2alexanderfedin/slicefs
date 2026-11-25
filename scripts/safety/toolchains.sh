#!/bin/bash
# The tool pins of the Safety jobs (Phase 21.2), sourced by every
# scripts/safety/*.sh script. The only place of the pins is the `env:` block
# of .github/workflows/safety.yml; a value set in the environment wins, so a
# developer runs the same scripts locally (for example RUST_STABLE=stable).
# Usage: source scripts/safety/toolchains.sh
# Sets RUST_STABLE, NIGHTLY, KANI_VERSION; defines need_toolchain <name> <components...>.
set -euo pipefail
_SAFETY_WF="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/.github/workflows/safety.yml"

# The value of `  <name>: <value>` in the env block of safety.yml.
_safety_pin() {
  sed -n -E "s/^  $1: *\"?([^\" ]+)\"? *$/\1/p" "$_SAFETY_WF" </dev/null | head -n 1
}

RUST_STABLE="${RUST_STABLE:-$(_safety_pin RUST_STABLE)}"
NIGHTLY="${NIGHTLY:-$(_safety_pin NIGHTLY)}"
KANI_VERSION="${KANI_VERSION:-$(_safety_pin KANI_VERSION)}"
for _v in RUST_STABLE NIGHTLY KANI_VERSION; do
  [ -n "${!_v}" ] || { echo "toolchains.sh: no $_v in the environment or in $_SAFETY_WF" >&2; exit 1; }
done
export RUST_STABLE NIGHTLY KANI_VERSION

# Install toolchain $1 with the components $2... unless it is there already.
need_toolchain() {
  local tc="$1"; shift
  rustup run "$tc" rustc -V </dev/null >/dev/null 2>&1 && return 0
  local args=()
  for c in "$@"; do args+=(-c "$c"); done
  rustup toolchain install "$tc" --profile minimal --no-self-update ${args[@]+"${args[@]}"} </dev/null
}

echo "TOOLCHAINS stable=$RUST_STABLE nightly=$NIGHTLY kani=$KANI_VERSION"
