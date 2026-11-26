#!/bin/bash
# The Miri gate (Phase 21.2, TSAFE-01, D-12 to D-14): the command of job `miri`
# in .github/workflows/safety.yml, and the same run for a developer (Linux or
# macOS). It runs the lib tests of a fixed, named list of test modules per
# crate under Miri on the pinned nightly: the modules that reach `unsafe` or
# the lock shim (D-13). The full `--lib` run took 1449 s on Linux, over the
# 1200 s rule of D-13 (runs/miri-linux-21.2-05.log); MIRI_MODULES=all runs it.
# Every lib test Miri cannot run has `#[cfg_attr(miri, ignore = "<reason>")]`,
# so the full run passes too; find such tests with scripts/safety/miri-probe.sh.
# A new test in a listed module that Miri cannot run fails this gate until it
# gets an attribute: an unsupported operation stops the binary.
# Usage: scripts/safety/miri.sh
# Env:   MIRI_CRATES    the crates (default "chunk-store metadata")
#        MIRI_MODULES   libtest filters for every crate (default: the list per
#                       crate below); "all" runs every lib test
#        NIGHTLY        the toolchain (default: the pin in safety.yml)
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
source "$HERE/toolchains.sh"
need_toolchain "$NIGHTLY" miri rust-src
# need_toolchain skips a toolchain that is there already, also without miri.
rustup component add --toolchain "$NIGHTLY" miri rust-src </dev/null
cd "$REPO"

# The test modules of each crate that reach `unsafe` or the lock shim (D-13).
modules_of() {
  case "$1" in
    chunk-store) echo "pack_io::tests:: sync::tests:: lock_tail_tests:: run::tests:: index::tests::" ;;
    metadata) echo "wal:: inode::tests:: xattr::tests::" ;;
    *) echo "miri.sh: no module list for crate $1 (set MIRI_MODULES)" >&2; exit 1 ;;
  esac
}

# D-13: the tests read the host clock and make temp dirs and files, which
# Miri refuses under isolation. This does not weaken the UB or data-race checks.
export MIRIFLAGS="-Zmiri-disable-isolation"
cargo +"$NIGHTLY" miri setup </dev/null

LOG=$(mktemp); ERR=$(mktemp)
trap 'rm -f "$LOG" "$ERR"' EXIT
for crate in ${MIRI_CRATES:-chunk-store metadata}; do
  mods="${MIRI_MODULES:-$(modules_of "$crate")}"
  [ "$mods" = all ] && mods=""
  echo "MIRI crate=$crate modules=${mods:-all}"
  rc=0; t0=$(date +%s)
  # stdout (the test lines) live; stderr (the Miri reports) after the run, so
  # the Miri text never splits a `test <name> ... ok` line.
  # shellcheck disable=SC2086 # the filters are words
  cargo +"$NIGHTLY" miri test --locked -p "$crate" --lib -- $mods </dev/null 2>"$ERR" | tee "$LOG" || rc=$?
  cat "$ERR" >&2
  secs=$(( $(date +%s) - t0 ))
  if grep -q 'unsupported operation' "$LOG" "$ERR"; then
    echo "MIRI FAILED crate=$crate: an unsupported operation (give the test a cfg_attr(miri, ignore = \"<reason>\"))"; exit 1
  fi
  if [ "$rc" -ne 0 ]; then
    echo "MIRI FAILED crate=$crate: cargo exit $rc"; exit 1
  fi
  if grep -q '^running 0 tests' "$LOG"; then
    echo "MIRI FAILED crate=$crate: 0 tests ran"; exit 1
  fi
  # A renamed module would drop out of the list with no other sign.
  for m in $mods; do
    if ! grep -qE "^test ${m}.* \.\.\. ok$" "$LOG"; then
      echo "MIRI FAILED crate=$crate: no passed test in module $m"; exit 1
    fi
  done
  # The only chunk-store unsafe Miri can run (D-14): the RunMap view, Send/Sync and munmap.
  if [ "$crate" = chunk-store ] && ! grep -q '^test pack_io::tests::a_run_map_reads_back_moves_across_threads_and_unmaps \.\.\. ok$' "$LOG"; then
    echo "MIRI FAILED crate=$crate: the RunMap test did not pass"; exit 1
  fi
  passed=$(sed -n -E 's/^test result: ok\. ([0-9]+) passed.*/\1/p' "$LOG" | head -n 1)
  ignored=$(sed -n -E 's/^test result: ok\..* ([0-9]+) ignored.*/\1/p' "$LOG" | head -n 1)
  echo "MIRI crate=$crate passed=$passed ignored=$ignored secs=$secs"
done
echo "MIRI ok"
