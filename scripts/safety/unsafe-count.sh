#!/bin/bash
# The unsafe count check (Phase 21.2, TSAFE-06): every `unsafe` token in the
# code must be counted in docs/design/unsafe-review.md. Run by clippy.sh, so
# the Safety job `clippy` checks it on Linux and macOS.
#   - code side: per .rs file under crates, benchmarks and vendor/fuser (no
#     target directories), the number of `unsafe` word tokens
#     (`grep -ow unsafe`, comments included); files with 0 are left out;
#   - doc side: the lines `- count: <path> <n>` of the review doc.
# Each difference, and each file that is on one side only, prints
# `UNSAFE mismatch <path> code=<n> doc=<m>` (`-` for a missing side) and the
# script exits 1. A new `unsafe` needs a review entry and a new count line.
# Usage: scripts/safety/unsafe-count.sh   (plain bash, no cargo)
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
DOC=docs/design/unsafe-review.md
cd "$REPO"
export LC_ALL=C

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Code side: "<path> <n>" per file with at least one token, sorted by path.
find crates benchmarks vendor/fuser -name target -prune -o -type f -name '*.rs' -print </dev/null \
  | sort | while IFS= read -r f; do
    n=$( (grep -ow unsafe "$f" </dev/null || true) | wc -l | tr -d ' ')
    [ "$n" -gt 0 ] && echo "$f $n"
    true
  done | sort -k1,1 > "$tmp/code"

# Doc side: the "- count: <path> <n>" lines.
[ -f "$DOC" ] || { echo "UNSAFE FAILED: $DOC is missing"; exit 1; }
(grep -E '^- count: ' "$DOC" </dev/null || true) | sed -E 's/^- count: +//; s/ +$//' | sort -k1,1 > "$tmp/doc"

# Compare by path: join both lists, "-" for a missing side.
bad=$(join -a 1 -a 2 -e - -o 0,1.2,2.2 "$tmp/code" "$tmp/doc" </dev/null \
  | awk '$2 != $3 { printf "UNSAFE mismatch %s code=%s doc=%s\n", $1, $2, $3 }')
if [ -n "$bad" ]; then
  echo "$bad"
  echo "UNSAFE FAILED: update the review entries and the count lines in $DOC"
  exit 1
fi
files=$(wc -l < "$tmp/code" | tr -d ' ')
tokens=$(awk '{ t += $2 } END { print t + 0 }' "$tmp/code")
echo "UNSAFE files=$files tokens=$tokens result=ok"
